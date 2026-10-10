use super::check::check_project_readiness;
use super::context::{Context, project_runtime_status};
use super::{DEFAULT_CLADDING_BUILD_IMAGE, DEFAULT_CLI_BUILD_IMAGE, DEFAULT_SANDBOX_BUILD_IMAGE};
use anyhow::Context as _;
use cladding::assets::{materialize_config, write_embedded_tools, write_proxy_startup_script};
use cladding::config::{
    DEFAULT_PROXY_IMAGE, ExecutionConfig, ImageBuildConfig, write_default_cladding_config,
};
use cladding::error::{Error, Result};
use cladding::fs_utils::{is_broken_symlink, path_is_symlink};
use cladding::podman::{
    initialize_baffle_ca, list_running_projects, podman_build_image, podman_build_proxy_image,
    podman_required, runtime_cleanup, runtime_cleanup_owned, runtime_create, runtime_inventory,
};
#[cfg(test)]
use cladding::runtime::RuntimeSpec;
use std::collections::HashMap;
use std::fs;

pub(super) fn cmd_build(context: &Context) -> Result<()> {
    let config = context.load_config()?;

    let host_uid = unsafe { libc::getuid() };
    let host_gid = unsafe { libc::getgid() };
    let build_plan = plan_image_builds(&config)?;

    let tools_dir = context.project_root.join("tools");
    if is_broken_symlink(&tools_dir)? {
        eprintln!("missing: tools (broken symlink at {})", tools_dir.display());
        eprintln!("hint: create or relink {}", tools_dir.display());
        return Err(Error::message("missing tools"));
    }

    let tools_bin_dir = tools_dir.join("bin");
    fs::create_dir_all(&tools_bin_dir).with_context(|| "failed to create tools directory")?;

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
        None if image == DEFAULT_CLADDING_BUILD_IMAGE => BuildDefinition::Embedded,
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
            DEFAULT_SANDBOX_BUILD_IMAGE,
            DEFAULT_CLI_BUILD_IMAGE,
        )?;
        fs::write(&cladding_config, generated)
            .with_context(|| format!("failed to write {}", cladding_config.display()))?;
        println!("generated: {}", cladding_config.display());
    }

    Ok(())
}

pub(super) fn cmd_up(context: &Context, verbose: bool) -> Result<()> {
    let mut runtime_create_attempted = false;
    cmd_up_inner(
        context,
        verbose,
        false,
        false,
        &mut runtime_create_attempted,
    )
}

pub(super) fn cmd_up_ephemeral(context: &Context, verbose: bool) -> (Result<()>, bool) {
    let mut runtime_create_attempted = false;
    let result = cmd_up_inner(
        context,
        verbose,
        true,
        !verbose,
        &mut runtime_create_attempted,
    );
    (result, runtime_create_attempted)
}

fn cmd_up_inner(
    context: &Context,
    verbose: bool,
    fail_if_already_running: bool,
    quiet_helpers: bool,
    runtime_create_attempted: &mut bool,
) -> Result<()> {
    let config = context.load_config()?;
    let status = project_runtime_status(context, &config, verbose, quiet_helpers)?;
    let spec = context.runtime_spec(&config)?;
    let inventory = runtime_inventory(&spec, verbose, quiet_helpers)?;

    if status.already_running && inventory.is_fully_running() {
        if fail_if_already_running {
            eprintln!(
                "error: one-off instance '{}' collides with existing runtime resources",
                config.name
            );
            return Err(Error::message("one-off runtime name collision"));
        }
        println!(
            "already running: {} ({})",
            config.name, status.current_project_root
        );
        return Ok(());
    }

    if !inventory.is_empty() {
        eprintln!(
            "error: cladding project '{}' has an incomplete or stopped runtime",
            config.name
        );
        eprintln!("existing runtime resources:");
        for resource in inventory.resources {
            eprintln!("  {} {} ({})", resource.kind, resource.name, resource.state);
        }
        eprintln!("hint: run 'cladding down' before running 'cladding up' again");
        return Err(Error::message("incomplete or stopped runtime"));
    }

    check_project_readiness(context, &config, &spec, verbose, quiet_helpers)?;
    if !fail_if_already_running {
        prepare_runtime_files(&context.runtime_root)?;
        fs::create_dir_all(context.runtime_root.join("runtime/empty-mask"))
            .with_context(|| "failed to create runtime empty-mask directory")?;
    }
    *runtime_create_attempted = true;
    if let Err(startup_error) = runtime_create(&spec, verbose, quiet_helpers) {
        if !fail_if_already_running
            && let Err(cleanup_error) = runtime_cleanup_owned(&spec, verbose, quiet_helpers)
        {
            eprintln!("error: cleanup after failed 'cladding up' failed: {cleanup_error}");
        }
        return Err(startup_error);
    }

    Ok(())
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

pub(super) fn cmd_down(context: &Context, verbose: bool) -> Result<()> {
    let config = context.load_config()?;
    let spec = context.runtime_spec(&config)?;
    let mut cleanup_error = None;
    record_cleanup_result(&mut cleanup_error, runtime_cleanup(&spec, verbose, false));

    match cleanup_error {
        Some(err) => Err(err),
        None => Ok(()),
    }
}

pub(super) fn cmd_down_ephemeral(context: &Context, verbose: bool) -> Result<()> {
    let config = context.load_config()?;
    let spec = context.runtime_spec(&config)?;
    runtime_cleanup_owned(&spec, verbose, !verbose)
}

pub(super) fn cmd_destroy(context: &Context) -> Result<()> {
    let config = context.load_config()?;
    let spec = context.runtime_spec(&config)?;
    let mut cleanup_error = None;
    record_cleanup_result(&mut cleanup_error, runtime_cleanup(&spec, false, false));

    match cleanup_error {
        Some(err) => Err(err),
        None => Ok(()),
    }
}

pub(super) fn cmd_ps(_context: &Context) -> Result<()> {
    podman_required("podman (required for cladding ps)")?;
    let projects = list_running_projects(false, false)?;
    if projects.is_empty() {
        println!("no running cladding projects");
        return Ok(());
    }

    println!("running cladding projects:");
    for project in projects {
        println!(
            "{}  {}  (containers: {})",
            project.name, project.project_root, project.container_count
        );
    }

    Ok(())
}

fn record_cleanup_result(target: &mut Option<Error>, result: Result<()>) {
    if let Err(err) = result
        && target.is_none()
    {
        *target = Some(err);
    }
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
            agent: ExecutionComponentConfig {
                enabled: true,
                image: DEFAULT_CLADDING_BUILD_IMAGE.to_string(),
                build: None,
                security_opts: Vec::new(),
            },
            nw_sandbox: Some(ExecutionComponentConfig {
                enabled: true,
                image: DEFAULT_CLADDING_BUILD_IMAGE.to_string(),
                build: None,
                security_opts: Vec::new(),
            }),
            fs_sandbox: None,
            proxy: None,
            mounts: Vec::<ResolvedMountConfig>::new(),
        }
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
        assert_eq!(plan[0].image, DEFAULT_CLADDING_BUILD_IMAGE);
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
        let workspace_root = root.join("project");
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
            RuntimeSpec::build_with_roots(&project_root, &workspace_root, runtime_root, &config)
        };
        let first_spec = make_spec("cladding-run-one", &first_runtime);
        let second_spec = make_spec("cladding-run-two", &second_runtime);

        assert_eq!(first_spec.project_root, project_root);
        assert_eq!(second_spec.project_root, project_root);
        assert_eq!(first_spec.runtime_root, first_runtime);
        assert_eq!(second_spec.runtime_root, second_runtime);
        assert_ne!(first_spec.agent.name, second_spec.agent.name);
        assert_ne!(
            first_spec.generated_runtime_socket_dirs(),
            second_spec.generated_runtime_socket_dirs()
        );

        for (spec, runtime_root) in [
            (&first_spec, &first_runtime),
            (&second_spec, &second_runtime),
        ] {
            let proxy = &spec.proxy.containers[0];
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
            assert!(spec.agent.containers[0].mounts.iter().any(|mount| {
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
}
