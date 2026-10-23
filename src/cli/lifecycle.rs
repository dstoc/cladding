use super::DEFAULT_COMPONENT_IMAGE;
use super::check::check_project_readiness;
use super::context::Context;
use super::control;
use super::selection::{SelectedRun, same_expected_roles, verified_control_socket_path};
use anyhow::Context as _;
use cladding::assets::{materialize_config, write_embedded_tools, write_proxy_startup_script};
use cladding::config::{
    DEFAULT_PROXY_IMAGE, ExecutionConfig, ImageBuildConfig, write_default_cladding_config,
};
use cladding::error::{Error, Result};
use cladding::fs_utils::{is_broken_symlink, path_is_symlink};
use cladding::identity::invoking_uid;
use cladding::podman::{
    RuntimeResourceAttempts, initialize_baffle_ca, list_container_snapshot, list_discovered_runs,
    podman_build_image, podman_build_proxy_image, podman_required, runtime_cleanup_owned_report,
    runtime_create, runtime_inventory_from_snapshot,
};
use cladding::runtime::RuntimeSpec;
use std::collections::HashMap;
use std::fs;

pub(super) fn cmd_build(context: &Context) -> Result<()> {
    let config = context.load_config()?;
    cladding::config::validate_baffle_session_files(&context.project_root.join("config"), &config)?;

    let host_uid = invoking_uid();
    let host_gid = unsafe { libc::getgid() };
    let build_plan = plan_image_builds(&config)?;

    let tools_dir = context.project_root.join("tools");
    if is_broken_symlink(&tools_dir)? {
        eprintln!("missing: tools (broken symlink at {})", tools_dir.display());
        eprintln!("hint: create or relink {}", tools_dir.display());
        return Err(Error::message("missing tools"));
    }

    let tools_bin_dir = tools_dir.join("bin");
    cladding::dependencies::install_tools(&context.project_root)?;
    write_embedded_tools(&tools_bin_dir)?;
    write_proxy_startup_script(
        &context
            .project_root
            .join("runtime/scripts/proxy_startup.sh"),
    )?;

    let default_context = &context.workspace_root;
    for target in build_plan {
        match target.build {
            BuildDefinition::Embedded => podman_build_image(
                &target.image,
                None,
                default_context,
                &Default::default(),
                Some((host_uid, host_gid)),
            )?,
            BuildDefinition::Custom(build) => podman_build_image(
                &target.image,
                Some(&build.containerfile),
                &build.context,
                &build.args,
                None,
            )?,
            BuildDefinition::EmbeddedProxy => {
                podman_build_proxy_image(&target.image, default_context)?
            }
        }
    }

    let spec = context.runtime_spec(&config)?;
    prepare_persistent_baffle_ca(&context.project_root, || {
        initialize_baffle_ca(&spec, false, false).map_err(anyhow::Error::new)
    })?;

    Ok(())
}

fn prepare_persistent_baffle_ca(
    project_root: &std::path::Path,
    initialize_ca: impl FnMut() -> anyhow::Result<()>,
) -> Result<()> {
    prepare_baffle_runtime(project_root)?;
    cladding::credentials::ensure_baffle_ca(project_root, initialize_ca)
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum BuildDefinition {
    Embedded,
    EmbeddedProxy,
    Custom(ImageBuildConfig),
}

#[derive(Debug, Clone)]
struct PlannedImageBuild {
    image: String,
    build: BuildDefinition,
}

fn plan_image_builds(config: &ExecutionConfig) -> Result<Vec<PlannedImageBuild>> {
    let mut builds_by_image = HashMap::<String, BuildDefinition>::new();
    let mut plan = Vec::new();

    add_image_build(
        &mut plan,
        &mut builds_by_image,
        "agent",
        config.agent_image(),
        config.agent.build.as_ref(),
    )?;
    if let Some(component) = config
        .nw_sandbox
        .as_ref()
        .filter(|component| component.enabled)
    {
        add_image_build(
            &mut plan,
            &mut builds_by_image,
            "nw_sandbox",
            &component.image,
            component.build.as_ref(),
        )?;
    }
    if let Some(component) = config
        .fs_sandbox
        .as_ref()
        .filter(|component| component.enabled)
    {
        add_image_build(
            &mut plan,
            &mut builds_by_image,
            "fs_sandbox",
            &component.image,
            component.build.as_ref(),
        )?;
    }
    let proxy = config.proxy.as_ref();
    let proxy_image = proxy
        .map(|proxy| proxy.image.as_str())
        .unwrap_or(DEFAULT_PROXY_IMAGE);
    add_proxy_image_build(
        &mut plan,
        &mut builds_by_image,
        proxy_image,
        proxy.and_then(|proxy| proxy.build.as_ref()),
    )?;

    Ok(plan)
}

fn add_image_build(
    plan: &mut Vec<PlannedImageBuild>,
    builds_by_image: &mut HashMap<String, BuildDefinition>,
    component: &str,
    image: &str,
    custom_build: Option<&ImageBuildConfig>,
) -> Result<()> {
    let build = match custom_build {
        Some(build) => BuildDefinition::Custom(build.clone()),
        None if image == DEFAULT_COMPONENT_IMAGE => BuildDefinition::Embedded,
        None => return Ok(()),
    };

    add_planned_image_build(plan, builds_by_image, component, image, build)
}

fn add_proxy_image_build(
    plan: &mut Vec<PlannedImageBuild>,
    builds_by_image: &mut HashMap<String, BuildDefinition>,
    image: &str,
    custom_build: Option<&ImageBuildConfig>,
) -> Result<()> {
    let build = match custom_build {
        Some(build) => BuildDefinition::Custom(build.clone()),
        None if image == DEFAULT_PROXY_IMAGE => BuildDefinition::EmbeddedProxy,
        None => return Ok(()),
    };
    add_planned_image_build(plan, builds_by_image, "proxy", image, build)
}

fn add_planned_image_build(
    plan: &mut Vec<PlannedImageBuild>,
    builds_by_image: &mut HashMap<String, BuildDefinition>,
    component: &str,
    image: &str,
    build: BuildDefinition,
) -> Result<()> {
    if let Some(existing) = builds_by_image.get(image) {
        if existing != &build {
            eprintln!(
                "error: components define conflicting builds for image '{image}' (including {component})"
            );
            return Err(Error::message("conflicting image build definitions"));
        }
        return Ok(());
    }

    builds_by_image.insert(image.to_string(), build.clone());
    plan.push(PlannedImageBuild {
        image: image.to_string(),
        build,
    });
    Ok(())
}

pub(super) fn cmd_init(context: &Context, name_override: Option<&str>) -> Result<()> {
    let project_root = &context.project_root;
    let config_dir = project_root.join("config");
    let home_dir = project_root.join("home");
    let tools_dir = project_root.join("tools");
    let runtime_dir = project_root.join("runtime");
    let empty_mask_dir = runtime_dir.join("empty-mask");
    let cladding_config = project_root.join("cladding.json");
    let cladding_gitignore = project_root.join(".gitignore");
    let cladding_config_preexisting = cladding_config.exists();

    if project_root.exists() && !project_root.is_dir() {
        eprintln!(
            "error: .cladding path exists but is not a directory: {}",
            project_root.display()
        );
        return Err(Error::message("invalid .cladding path"));
    }

    let project_root_created = !project_root.exists();
    fs::create_dir_all(project_root)
        .with_context(|| format!("failed to create {}", project_root.display()))?;

    if project_root_created {
        fs::write(&cladding_gitignore, "*\n")
            .with_context(|| format!("failed to write {}", cladding_gitignore.display()))?;
    }

    if config_dir.exists() || path_is_symlink(&config_dir) {
        println!("config already exists: {}", config_dir.display());
    } else {
        fs::create_dir_all(&config_dir)
            .with_context(|| format!("failed to create {}", config_dir.display()))?;
        println!("initialized: {}", config_dir.display());
    }

    materialize_config(&config_dir)?;

    if home_dir.exists() || path_is_symlink(&home_dir) {
        println!("home already exists: {}", home_dir.display());
    } else {
        fs::create_dir_all(&home_dir)
            .with_context(|| format!("failed to create {}", home_dir.display()))?;
        println!("initialized: {}", home_dir.display());
    }

    if tools_dir.exists() || path_is_symlink(&tools_dir) {
        println!("tools already exists: {}", tools_dir.display());
    } else {
        fs::create_dir_all(&tools_dir)
            .with_context(|| format!("failed to create {}", tools_dir.display()))?;
        println!("initialized: {}", tools_dir.display());
    }

    if runtime_dir.exists() || path_is_symlink(&runtime_dir) {
        println!("runtime already exists: {}", runtime_dir.display());
    } else {
        fs::create_dir_all(&runtime_dir)
            .with_context(|| format!("failed to create {}", runtime_dir.display()))?;
        println!("initialized: {}", runtime_dir.display());
    }

    if empty_mask_dir.exists() || path_is_symlink(&empty_mask_dir) {
        println!("empty-mask already exists: {}", empty_mask_dir.display());
    } else {
        fs::create_dir_all(&empty_mask_dir)
            .with_context(|| format!("failed to create {}", empty_mask_dir.display()))?;
        println!("initialized: {}", empty_mask_dir.display());
    }

    prepare_baffle_runtime(project_root)?;

    if cladding_config_preexisting {
        println!(
            "cladding config already exists: {}",
            cladding_config.display()
        );
    } else {
        let generated = write_default_cladding_config(
            name_override,
            DEFAULT_COMPONENT_IMAGE,
            DEFAULT_COMPONENT_IMAGE,
        )?;
        fs::write(&cladding_config, generated)
            .with_context(|| format!("failed to write {}", cladding_config.display()))?;
        println!("generated: {}", cladding_config.display());
    }

    Ok(())
}

pub(super) fn start_run_runtime(
    context: &Context,
    config: &ExecutionConfig,
    spec: &RuntimeSpec,
    attempts: &mut RuntimeResourceAttempts,
    verbose: bool,
) -> (Result<()>, bool) {
    let mut runtime_create_attempted = false;
    let result = (|| {
        let preparation = cladding::runtime_metrics::phase("preparation/preflight");
        let quiet_helpers = !verbose;
        let containers = list_container_snapshot(verbose, quiet_helpers)?;
        let inventory = runtime_inventory_from_snapshot(spec, &containers);
        if !inventory.is_empty() {
            eprintln!(
                "error: one-off instance '{}' has incomplete or stopped runtime resources",
                config.name
            );
            for resource in inventory.resources {
                eprintln!("  {} {} ({})", resource.kind, resource.name, resource.state);
            }
            return Err(Error::message("incomplete or stopped runtime"));
        }
        check_project_readiness(context, config, spec, verbose, quiet_helpers)?;
        drop(preparation);

        let startup = cladding::runtime_metrics::phase("runtime startup");
        spec.validate_custom_mount_sources()?;
        runtime_create_attempted = true;
        let result = runtime_create(spec, attempts, verbose, quiet_helpers);
        drop(startup);
        if let Err(startup_error) = result {
            return Err(startup_error);
        }
        Ok(())
    })();
    (result, runtime_create_attempted)
}

pub(super) fn prepare_run_runtime_root(runtime_root: &std::path::Path) -> Result<()> {
    prepare_runtime_files(runtime_root)?;
    fs::create_dir_all(runtime_root.join("runtime/empty-mask"))
        .with_context(|| "failed to create one-off runtime empty-mask directory")?;
    Ok(())
}

fn prepare_runtime_files(runtime_root: &std::path::Path) -> Result<()> {
    write_proxy_startup_script(&runtime_root.join("runtime/scripts/proxy_startup.sh"))?;
    Ok(())
}

fn prepare_baffle_runtime(project_root: &std::path::Path) -> Result<()> {
    cladding::credentials::ensure_baffle_credentials(project_root)?;
    prepare_runtime_files(project_root)?;
    Ok(())
}

pub(super) fn cleanup_run_runtime(
    spec: &RuntimeSpec,
    attempts: &RuntimeResourceAttempts,
    verbose: bool,
) -> (Result<()>, bool) {
    let report = runtime_cleanup_owned_report(spec, attempts, verbose, !verbose);
    (report.result, report.runtime_root_safe_to_remove)
}

pub(super) fn cmd_ps() -> Result<()> {
    podman_required("podman (required for cladding ps)")?;
    let runs = list_discovered_runs(false, false)?;
    if runs.is_empty() {
        println!("no cladding runs");
        return Ok(());
    }

    println!("UUID  STATE  PROJECT  COMPONENTS");
    for run in runs {
        let observed = run
            .components
            .iter()
            .map(|component| format!("{}:{}", component.role, component.state))
            .collect::<Vec<_>>()
            .join(",");
        let state = if run.malformed.is_some() {
            "unknown (malformed)".to_string()
        } else if let Ok(socket) = verified_control_socket_path(&run) {
            match control::status(&socket, &run.uuid) {
                Ok(status) => {
                    let missing = run
                        .expected_roles
                        .as_deref()
                        .map(|expected| {
                            expected
                                .split(',')
                                .filter(|role| {
                                    !run.components
                                        .iter()
                                        .any(|component| component.role == *role)
                                })
                                .collect::<Vec<_>>()
                        })
                        .unwrap_or_default();
                    if !same_expected_roles(run.expected_roles.as_deref(), &status.expected_roles) {
                        "unknown (supervisor metadata mismatch)".to_string()
                    } else if !missing.is_empty() {
                        if status.state == "ready" {
                            format!("unknown (missing {})", missing.join(","))
                        } else {
                            format!("{} (missing {})", status.state, missing.join(","))
                        }
                    } else if status.state == "ready"
                        && run
                            .components
                            .iter()
                            .any(|component| component.state != "running")
                    {
                        "unknown (component stopped)".to_string()
                    } else {
                        status.state
                    }
                }
                Err(_) if run_lock_is_released(&run) => "orphaned".to_string(),
                Err(_) => "unknown/unreachable".to_string(),
            }
        } else if run.control_socket.is_some() {
            "unknown (invalid supervisor endpoint)".to_string()
        } else if run_lock_is_released(&run) {
            "orphaned".to_string()
        } else {
            "unknown/unreachable".to_string()
        };
        println!(
            "{}  {}  {}  {}",
            run.uuid,
            state,
            run.project_root.as_deref().unwrap_or("unknown"),
            if observed.is_empty() {
                "none"
            } else {
                &observed
            }
        );
    }

    Ok(())
}

fn run_lock_is_released(run: &cladding::podman::DiscoveredRun) -> bool {
    let Some(root) = run.project_root.as_deref() else {
        return false;
    };
    let root = std::path::Path::new(root);
    if !root.is_absolute()
        || root.file_name().is_none_or(|name| name != ".cladding")
        || std::fs::canonicalize(root).ok().as_deref() != Some(root)
    {
        return false;
    }
    let runtime_root = root.join("runtime/runs").join(&run.name);
    match control::acquire_recovery_lock(&runtime_root) {
        Ok(lock) => {
            drop(lock);
            true
        }
        Err(_) => false,
    }
}

pub(super) fn cmd_stop_selected(selected: &SelectedRun) -> Result<()> {
    if selected.control.is_none() {
        return Err(Error::message(
            "cannot stop the selected run because its supervisor is unreachable",
        ));
    }
    let socket = selected
        .run
        .control_socket
        .as_deref()
        .ok_or_else(|| Error::message("selected run has no supervisor control endpoint"))?;
    control::stop(
        std::path::Path::new(socket),
        selected.uuid(),
        std::time::Duration::from_secs(120),
    )?;
    println!("stopped run {}", selected.uuid());
    Ok(())
}

pub(super) fn cmd_destroy_selected(selected: &SelectedRun) -> Result<()> {
    if selected
        .control
        .as_ref()
        .is_some_and(|control| control.state != "stopped")
    {
        return Err(Error::message(format!(
            "refusing to destroy run {} while its supervisor is {}",
            selected.uuid(),
            selected.control.as_ref().expect("checked state").state
        )));
    }
    let project_root = selected
        .run
        .project_root
        .as_deref()
        .ok_or_else(|| Error::message("selected run has no project identity"))?;
    let project_root = std::path::Path::new(project_root);
    if !project_root.is_absolute()
        || project_root
            .file_name()
            .is_none_or(|name| name != ".cladding")
    {
        return Err(Error::message(
            "refusing recovery because the project identity is invalid",
        ));
    }
    let canonical_root = std::fs::canonicalize(project_root).map_err(|error| {
        Error::message(format!("failed to verify run project identity: {error}"))
    })?;
    if canonical_root != project_root {
        return Err(Error::message(
            "refusing recovery because the project identity uses a symlink",
        ));
    }
    let runtime_root = project_root.join("runtime/runs").join(&selected.run.name);
    let _lifetime_lock = control::acquire_recovery_lock(&runtime_root)?;

    let fresh_runs = list_discovered_runs(false, false)?;
    let fresh_run = fresh_runs
        .into_iter()
        .find(|run| run.uuid == selected.run.uuid)
        .ok_or_else(|| Error::message("selected run resources are no longer present"))?;
    verify_run_inventory_unchanged(&selected.run, &fresh_run)?;
    for component in fresh_run
        .components
        .iter()
        .chain(&fresh_run.helper_containers)
    {
        let id = component
            .id
            .as_deref()
            .ok_or_else(|| Error::message("run-owned container has no immutable ID"))?;
        let status = std::process::Command::new("podman")
            .args(["container", "rm", "--force", id])
            .status()
            .map_err(|error| {
                Error::message(format!("failed to remove run container {id}: {error}"))
            })?;
        cladding::podman::ensure_success(status, "podman container rm")?;
    }
    for volume in &fresh_run.volumes {
        remove_run_owned_volume(
            volume,
            &selected.run.name,
            project_root,
            &runtime_root,
            selected
                .run
                .control_socket
                .as_deref()
                .ok_or_else(|| Error::message("selected run has no supervisor endpoint"))?,
            selected
                .run
                .expected_roles
                .as_deref()
                .ok_or_else(|| Error::message("selected run has no expected component metadata"))?,
        )?;
    }
    if let Some(socket) = selected.run.control_socket.as_deref() {
        control::remove_stale_socket(std::path::Path::new(socket), &runtime_root, selected.uuid())?;
    }
    std::fs::remove_dir_all(&runtime_root).map_err(|error| {
        Error::message(format!(
            "run containers were removed but private run cleanup failed at {}: {error}",
            runtime_root.display()
        ))
    })?;
    println!("destroyed orphaned run {}", selected.uuid());
    Ok(())
}

fn remove_run_owned_volume(
    volume_name: &str,
    run_name: &str,
    project_root: &std::path::Path,
    runtime_root: &std::path::Path,
    control_socket: &str,
    expected_roles: &str,
) -> Result<()> {
    let output = std::process::Command::new("podman")
        .args(["volume", "inspect", volume_name])
        .output()
        .map_err(|error| {
            Error::message(format!(
                "failed to inspect run volume {volume_name}: {error}"
            ))
        })?;
    if !output.status.success() {
        return Err(Error::message(format!(
            "failed to inspect run volume {volume_name}"
        )));
    }
    let document: serde_json::Value = serde_json::from_slice(&output.stdout).map_err(|_| {
        Error::message(format!(
            "podman returned invalid inspection for volume {volume_name}"
        ))
    })?;
    let item = document
        .as_array()
        .and_then(|items| items.first())
        .or_else(|| document.as_object().map(|_| &document))
        .ok_or_else(|| {
            Error::message(format!(
                "podman returned no inspection for volume {volume_name}"
            ))
        })?;
    let labels = item
        .get("Labels")
        .and_then(serde_json::Value::as_object)
        .ok_or_else(|| {
            Error::message(format!("run volume {volume_name} has no ownership labels"))
        })?;
    let label = |key: &str| labels.get(key).and_then(serde_json::Value::as_str);
    let resource = label("cladding_resource").unwrap_or_default();
    let is_owned_socket = resource == "inter-container-socket"
        && label("cladding_socket_volume") == Some(volume_name);
    let runtime_root_text = runtime_root.to_string_lossy();
    let project_root_text = project_root.to_string_lossy();
    let is_owned_mount = resource == "managed-mount"
        && label("cladding_mount_volume") == Some(volume_name)
        && label("cladding_runtime_root") == Some(runtime_root_text.as_ref());
    if label("cladding") != Some(run_name)
        || label("project_root") != Some(project_root_text.as_ref())
        || label("cladding_run_metadata_version") != Some("1")
        || label("cladding_run_control_socket") != Some(control_socket)
        || label("cladding_run_expected_roles") != Some(expected_roles)
        || !(is_owned_socket || is_owned_mount)
    {
        return Err(Error::message(format!(
            "refusing to remove volume {volume_name} because its ownership labels do not match this run"
        )));
    }
    let status = std::process::Command::new("podman")
        .args(["volume", "rm", volume_name])
        .status()
        .map_err(|error| {
            Error::message(format!(
                "failed to remove run volume {volume_name}: {error}"
            ))
        })?;
    cladding::podman::ensure_success(status, "podman volume rm")
}

fn verify_run_inventory_unchanged(
    selected: &cladding::podman::DiscoveredRun,
    current: &cladding::podman::DiscoveredRun,
) -> Result<()> {
    if current.malformed.is_some() {
        return Err(Error::message(
            "refusing recovery because the current run inventory is malformed",
        ));
    }
    let selected_components_match = selected.components.len() == current.components.len()
        && selected.components.iter().all(|selected_component| {
            current
                .components
                .iter()
                .find(|component| component.role == selected_component.role)
                .and_then(|component| component.id.as_deref())
                == selected_component.id.as_deref()
        });
    let selected_helpers_match = selected.helper_containers.len()
        == current.helper_containers.len()
        && selected.helper_containers.iter().all(|selected_helper| {
            current
                .helper_containers
                .iter()
                .find(|helper| {
                    helper.role == selected_helper.role && helper.name == selected_helper.name
                })
                .and_then(|helper| helper.id.as_deref())
                == selected_helper.id.as_deref()
        });
    let mut selected_volumes = selected.volumes.clone();
    let mut current_volumes = current.volumes.clone();
    selected_volumes.sort();
    current_volumes.sort();
    if current.uuid != selected.uuid
        || current.project_root != selected.project_root
        || current.metadata_version != selected.metadata_version
        || current.control_socket != selected.control_socket
        || current.expected_roles != selected.expected_roles
        || !selected_components_match
        || !selected_helpers_match
        || selected_volumes != current_volumes
    {
        return Err(Error::message(
            "refusing recovery because the run inventory changed after selection",
        ));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use cladding::config::{ExecutionComponentConfig, ExecutionProxyConfig, ResolvedMountConfig};
    use rcgen::{BasicConstraints, DnType, IsCa, KeyPair, KeyUsagePurpose};
    use std::collections::BTreeMap;
    use std::path::PathBuf;
    use time::{Duration, OffsetDateTime};

    fn config() -> ExecutionConfig {
        ExecutionConfig {
            name: "demo".to_string(),
            use_runsc: false,
            fake_credential_env: std::collections::BTreeMap::new(),
            agent: ExecutionComponentConfig {
                enabled: true,
                init: true,
                image: DEFAULT_COMPONENT_IMAGE.to_string(),
                build: None,
                security_opts: Vec::new(),
            },
            nw_sandbox: Some(ExecutionComponentConfig {
                enabled: true,
                init: true,
                image: DEFAULT_COMPONENT_IMAGE.to_string(),
                build: None,
                security_opts: Vec::new(),
            }),
            fs_sandbox: None,
            proxy: None,
            mounts: Vec::<ResolvedMountConfig>::new(),
        }
    }

    fn discovered_run_for_recovery() -> cladding::podman::DiscoveredRun {
        let uuid = "aabbccdd-1111-4222-8333-123456789abc";
        cladding::podman::DiscoveredRun {
            uuid: uuid.to_string(),
            name: format!("cladding-run-{uuid}"),
            project_root: Some("/tmp/project/.cladding".to_string()),
            metadata_version: Some("1".to_string()),
            control_socket: Some(format!(
                "/tmp/project/.cladding/runtime/runs/cladding-run-{uuid}/control.sock"
            )),
            expected_roles: Some("agent,proxy".to_string()),
            components: ["agent", "proxy"]
                .into_iter()
                .map(|role| cladding::podman::RunComponent {
                    role: role.to_string(),
                    id: Some(format!("immutable-{role}-id")),
                    name: format!("cladding-run-{uuid}-{role}-instance"),
                    state: "exited".to_string(),
                    cwd_mappings: Some("[]".to_string()),
                })
                .collect(),
            helper_containers: Vec::new(),
            volumes: vec!["run-managed-volume".to_string()],
            malformed: None,
        }
    }

    #[test]
    fn recovery_rejects_container_replacement_after_selection() {
        let selected = discovered_run_for_recovery();
        let mut current = selected.clone();
        current.components[0].id = Some("replacement-agent-id".to_string());

        let error = verify_run_inventory_unchanged(&selected, &current).unwrap_err();

        assert!(error.to_string().contains("changed after selection"));
    }

    #[test]
    fn recovery_rejects_resource_metadata_and_volume_changes() {
        let selected = discovered_run_for_recovery();
        let mut replaced_identity = selected.clone();
        replaced_identity.project_root = Some("/tmp/other/.cladding".to_string());
        assert!(verify_run_inventory_unchanged(&selected, &replaced_identity).is_err());

        let mut changed_volumes = selected.clone();
        changed_volumes.volumes.push("foreign-volume".to_string());
        assert!(verify_run_inventory_unchanged(&selected, &changed_volumes).is_err());
    }

    fn build(context: &str, feature: &str) -> ImageBuildConfig {
        ImageBuildConfig {
            containerfile: PathBuf::from("/tmp/Containerfile"),
            context: PathBuf::from(context),
            args: BTreeMap::from([("FEATURE".to_string(), feature.to_string())]),
        }
    }

    #[test]
    fn build_ca_setup_initializes_once_and_reuses_existing_material() {
        let root = std::env::temp_dir().join(format!("cladding-build-ca-{}", std::process::id()));
        let _ = fs::remove_dir_all(&root);
        fs::create_dir_all(&root).unwrap();
        let baffle_dir = root.join("credentials/baffle");
        let mut initializer_calls = 0;

        prepare_persistent_baffle_ca(&root, || {
            initializer_calls += 1;
            write_test_ca(&baffle_dir)
        })
        .unwrap();
        assert_eq!(initializer_calls, 1);
        let certificate = fs::read(baffle_dir.join("ca.crt")).unwrap();
        let private_key = fs::read(baffle_dir.join("ca-key.pem")).unwrap();

        prepare_persistent_baffle_ca(&root, || {
            initializer_calls += 1;
            anyhow::bail!("build must reuse an existing CA")
        })
        .unwrap();

        assert_eq!(initializer_calls, 1);
        assert_eq!(fs::read(baffle_dir.join("ca.crt")).unwrap(), certificate);
        assert_eq!(
            fs::read(baffle_dir.join("ca-key.pem")).unwrap(),
            private_key
        );
        fs::remove_dir_all(root).unwrap();
    }

    fn write_test_ca(baffle_dir: &std::path::Path) -> anyhow::Result<()> {
        let mut params = rcgen::CertificateParams::default();
        params
            .distinguished_name
            .push(DnType::CommonName, "Baffle Interception CA");
        params.is_ca = IsCa::Ca(BasicConstraints::Unconstrained);
        params.key_usages = vec![KeyUsagePurpose::KeyCertSign, KeyUsagePurpose::CrlSign];
        params.not_before = OffsetDateTime::now_utc() - Duration::days(1);
        params.not_after = OffsetDateTime::now_utc() + Duration::days(3650);
        let key_pair = KeyPair::generate()?;
        let certificate = params.self_signed(&key_pair)?;
        fs::write(baffle_dir.join("ca.crt"), certificate.pem())?;
        fs::write(baffle_dir.join("ca-key.pem"), key_pair.serialize_pem())?;
        Ok(())
    }

    #[test]
    fn build_plan_reuses_embedded_default_image_across_components() {
        let plan = plan_image_builds(&config()).unwrap();
        assert_eq!(plan.len(), 2);
        assert_eq!(plan[0].image, DEFAULT_COMPONENT_IMAGE);
        assert_eq!(plan[0].build, BuildDefinition::Embedded);
        assert_eq!(plan[1].image, DEFAULT_PROXY_IMAGE);
        assert_eq!(plan[1].build, BuildDefinition::EmbeddedProxy);
    }

    #[test]
    fn build_plan_deduplicates_shared_custom_image_definitions() {
        let mut config = config();
        config.agent.image = "localhost/shared:latest".to_string();
        config.agent.build = Some(build("/tmp/context", "enabled"));
        config.nw_sandbox.as_mut().unwrap().image = "localhost/shared:latest".to_string();
        config.nw_sandbox.as_mut().unwrap().build = Some(build("/tmp/context", "enabled"));

        let plan = plan_image_builds(&config).unwrap();
        assert_eq!(plan.len(), 2);
        assert_eq!(plan[0].image, "localhost/shared:latest");
        assert_eq!(
            plan[0].build,
            BuildDefinition::Custom(build("/tmp/context", "enabled"))
        );
        assert_eq!(plan[1].image, DEFAULT_PROXY_IMAGE);
        assert_eq!(plan[1].build, BuildDefinition::EmbeddedProxy);
    }

    #[test]
    fn build_plan_rejects_conflicting_definitions_for_a_shared_image_tag() {
        let mut config = config();
        config.agent.image = "localhost/shared:latest".to_string();
        config.agent.build = Some(build("/tmp/agent", "agent"));
        config.nw_sandbox.as_mut().unwrap().image = "localhost/shared:latest".to_string();
        config.nw_sandbox.as_mut().unwrap().build = Some(build("/tmp/sandbox", "sandbox"));

        assert!(plan_image_builds(&config).is_err());
    }

    #[test]
    fn build_plan_skips_disabled_sandbox_builds() {
        let mut config = config();
        config.agent.image = "agent:prebuilt".to_string();
        config.nw_sandbox.as_mut().unwrap().enabled = false;
        config.nw_sandbox.as_mut().unwrap().image = "localhost/sandbox:latest".to_string();
        config.nw_sandbox.as_mut().unwrap().build = Some(build("/tmp/sandbox", "enabled"));

        let plan = plan_image_builds(&config).unwrap();
        assert_eq!(plan.len(), 1);
        assert_eq!(plan[0].image, DEFAULT_PROXY_IMAGE);
        assert_eq!(plan[0].build, BuildDefinition::EmbeddedProxy);
    }

    #[test]
    fn build_plan_includes_custom_proxy_build() {
        let mut config = config();
        config.agent.image = "agent:prebuilt".to_string();
        config.nw_sandbox = None;
        config.proxy = Some(ExecutionProxyConfig {
            image: "localhost/proxy:latest".to_string(),
            build: Some(build("/tmp/proxy", "enabled")),
            agent_session_config: "agent.toml".to_string(),
            nw_sandbox_session_config: "nw-sandbox.toml".to_string(),
        });

        let plan = plan_image_builds(&config).unwrap();

        assert_eq!(plan.len(), 1);
        assert_eq!(plan[0].image, "localhost/proxy:latest");
        assert!(matches!(plan[0].build, BuildDefinition::Custom(_)));
    }

    #[test]
    fn build_plan_keeps_a_prebuilt_proxy_image_override() {
        let mut config = config();
        config.agent.image = "agent:prebuilt".to_string();
        config.nw_sandbox = None;
        config.proxy = Some(ExecutionProxyConfig {
            image: "localhost/custom-proxy:latest".to_string(),
            build: None,
            agent_session_config: "agent.toml".to_string(),
            nw_sandbox_session_config: "nw-sandbox.toml".to_string(),
        });

        assert!(plan_image_builds(&config).unwrap().is_empty());
    }

    #[test]
    fn concurrent_runs_share_project_mounts_and_isolate_runtime_files() {
        let root =
            std::env::temp_dir().join(format!("cladding-run-runtime-roots-{}", std::process::id()));
        let _ = fs::remove_dir_all(&root);
        let project_root = root.join("project/.cladding");
        let first_runtime = root.join("run-one");
        let second_runtime = root.join("run-two");
        fs::create_dir_all(project_root.join("config")).unwrap();
        fs::create_dir_all(project_root.join("credentials/baffle")).unwrap();
        fs::create_dir_all(project_root.join("home")).unwrap();
        fs::create_dir_all(project_root.join("tools")).unwrap();

        std::thread::scope(|scope| {
            let first = scope.spawn(|| prepare_run_runtime_root(&first_runtime));
            let second = scope.spawn(|| prepare_run_runtime_root(&second_runtime));
            first.join().unwrap().unwrap();
            second.join().unwrap().unwrap();
        });

        let make_spec = |name: &str, runtime_root: &std::path::Path| {
            let mut config = config();
            config.name = name.to_string();
            RuntimeSpec::build_with_runtime_root(&project_root, runtime_root, &config)
        };
        let first_spec = make_spec("cladding-run-one", &first_runtime);
        let second_spec = make_spec("cladding-run-two", &second_runtime);

        assert_eq!(first_spec.project_root, project_root);
        assert_eq!(second_spec.project_root, project_root);
        assert_eq!(first_spec.runtime_root, first_runtime);
        assert_eq!(second_spec.runtime_root, second_runtime);
        assert_ne!(first_spec.agent.name, second_spec.agent.name);
        for (spec, runtime_root) in [
            (&first_spec, &first_runtime),
            (&second_spec, &second_runtime),
        ] {
            let proxy = &spec.proxy.container;
            assert!(proxy.mounts.iter().any(|mount| {
                mount.mount_path == "/opt/config"
                    && matches!(
                        &mount.source,
                        cladding::runtime::RuntimeMountSource::HostPath { path }
                            if path == &project_root.join("config")
                    )
            }));
            assert!(proxy.mounts.iter().any(|mount| {
                mount.mount_path == "/opt/credentials/baffle"
                    && matches!(
                        &mount.source,
                        cladding::runtime::RuntimeMountSource::HostPath { path }
                            if path == &project_root.join("credentials/baffle")
                    )
            }));
            assert!(proxy.mounts.iter().any(|mount| {
                mount.mount_path == "/opt/scripts/proxy_startup.sh"
                    && matches!(
                        &mount.source,
                        cladding::runtime::RuntimeMountSource::HostPath { path }
                            if path == &runtime_root.join("runtime/scripts/proxy_startup.sh")
                    )
            }));
            assert!(spec.agent.container.mounts.iter().any(|mount| {
                mount.mount_path == "/home/user"
                    && matches!(
                        &mount.source,
                        cladding::runtime::RuntimeMountSource::HostPath { path }
                            if path == &project_root.join("home")
                    )
            }));
            assert!(!runtime_root.join("config").exists());
            assert!(!runtime_root.join("tools").exists());
            assert!(!runtime_root.join("home").exists());
            assert!(!runtime_root.join("credentials").exists());
            assert!(runtime_root.join("runtime/empty-mask").is_dir());
            assert!(
                runtime_root
                    .join("runtime/scripts/proxy_startup.sh")
                    .is_file()
            );
        }

        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn baffle_runtime_preparation_does_not_generate_ca_or_change_secrets() {
        let root = std::env::temp_dir().join(format!(
            "cladding-baffle-runtime-preparation-{}",
            std::process::id()
        ));
        let _ = fs::remove_dir_all(&root);
        fs::create_dir_all(&root).unwrap();

        prepare_baffle_runtime(&root).unwrap();
        let credential_dir = root.join("credentials/baffle");
        let secret_path = credential_dir.join("secrets/api-token");
        fs::write(&secret_path, "persistent-secret").unwrap();

        prepare_baffle_runtime(&root).unwrap();

        assert!(!credential_dir.join("ca.crt").exists());
        assert!(!credential_dir.join("ca-key.pem").exists());
        assert_eq!(
            fs::read_to_string(secret_path).unwrap(),
            "persistent-secret"
        );
        assert!(root.join("runtime/scripts/proxy_startup.sh").is_file());
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn init_creates_and_reuses_persistent_baffle_credential_storage() {
        let root =
            std::env::temp_dir().join(format!("cladding-init-credentials-{}", std::process::id()));
        let _ = fs::remove_dir_all(&root);
        let project_root = root.join(".cladding");
        let context = super::super::context::Context::default_for_project(project_root.clone());

        cmd_init(&context, Some("demo")).unwrap();
        let cert_path = project_root.join("credentials/baffle/ca.crt");
        cmd_init(&context, Some("demo")).unwrap();

        assert!(!cert_path.exists());
        assert!(!project_root.join("credentials/baffle/ca-key.pem").exists());
        assert!(project_root.join("credentials/baffle").is_dir());
        assert!(project_root.join("credentials/baffle/secrets").is_dir());
        assert!(
            project_root
                .join("runtime/scripts/proxy_startup.sh")
                .is_file()
        );
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn build_rejects_malformed_sessions_before_build_side_effects() {
        let root = std::env::temp_dir().join(format!(
            "cladding-build-invalid-session-{}",
            std::process::id()
        ));
        let _ = fs::remove_dir_all(&root);
        let project_root = root.join(".cladding");
        let context = super::super::context::Context::default_for_project(project_root.clone());

        cmd_init(&context, Some("demo")).unwrap();
        fs::write(
            project_root.join("config/proxy/sessions/agent.toml"),
            "version = [\n",
        )
        .unwrap();

        let error = cmd_build(&context).expect_err("build must reject malformed session data");
        assert!(error.to_string().contains("agent Baffle session config"));
        assert!(!project_root.join("tools/bin").exists());
        fs::remove_dir_all(root).unwrap();
    }
}
