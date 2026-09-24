use super::check::{
    check_required_binaries, check_required_config_files, check_required_host_paths,
    check_required_images, check_runsc_runtime, report_runtime_script_mismatch,
    warn_obsolete_generated_paths,
};
use super::context::{Context, project_runtime_status};
use super::{DEFAULT_CLADDING_BUILD_IMAGE, DEFAULT_CLI_BUILD_IMAGE, DEFAULT_SANDBOX_BUILD_IMAGE};
use anyhow::Context as _;
use cladding::assets::{materialize_config, materialize_runtime_scripts, write_embedded_tools};
use cladding::config::{ExecutionConfig, ImageBuildConfig, write_default_cladding_config};
use cladding::error::{Error, Result};
use cladding::fs_utils::{is_broken_symlink, path_is_symlink};
use cladding::podman::{
    list_running_projects, podman_build_image, podman_required, runtime_cleanup, runtime_create,
    runtime_inventory,
};
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

    let default_context = context
        .project_root
        .parent()
        .unwrap_or(&context.project_root);
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
        }
    }

    Ok(())
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum BuildDefinition {
    Embedded,
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
    if let Some(proxy) = &config.proxy {
        add_image_build(
            &mut plan,
            &mut builds_by_image,
            "proxy",
            &proxy.image,
            proxy.build.as_ref(),
        )?;
    }

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
    let config = context.load_config()?;
    materialize_runtime_scripts(&context.project_root)?;
    let status = project_runtime_status(context, &config, verbose)?;
    let spec = RuntimeSpec::build(&context.project_root, &config);
    let inventory = runtime_inventory(&spec, verbose)?;

    if status.already_running && inventory.is_fully_running() {
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

    check_required_binaries(context, &config)?;
    check_runsc_runtime(&config, verbose)?;
    check_required_images(&config, verbose)?;
    check_required_config_files(context, &config)?;
    warn_obsolete_generated_paths(context);
    let _ = report_runtime_script_mismatch(context, "warning")?;
    fs::create_dir_all(context.project_root.join("runtime/empty-mask"))
        .with_context(|| "failed to create runtime empty-mask directory")?;
    check_required_host_paths(&spec)?;
    runtime_create(&spec, verbose)
}

pub(super) fn cmd_down(context: &Context, verbose: bool) -> Result<()> {
    let config = context.load_config()?;
    let spec = RuntimeSpec::build(&context.project_root, &config);
    let mut cleanup_error = None;
    record_cleanup_result(&mut cleanup_error, runtime_cleanup(&spec, verbose));

    match cleanup_error {
        Some(err) => Err(err),
        None => Ok(()),
    }
}

pub(super) fn cmd_destroy(context: &Context) -> Result<()> {
    let config = context.load_config()?;
    let spec = RuntimeSpec::build(&context.project_root, &config);
    let mut cleanup_error = None;
    record_cleanup_result(&mut cleanup_error, runtime_cleanup(&spec, false));

    match cleanup_error {
        Some(err) => Err(err),
        None => Ok(()),
    }
}

pub(super) fn cmd_ps(_context: &Context) -> Result<()> {
    podman_required("podman (required for cladding ps)")?;
    let projects = list_running_projects(false)?;
    if projects.is_empty() {
        println!("no running cladding projects");
        return Ok(());
    }

    println!("running cladding projects:");
    for project in projects {
        println!(
            "{}  {}  (pods: {})",
            project.name, project.project_root, project.pod_count
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
    use std::collections::BTreeMap;
    use std::path::PathBuf;

    fn config() -> ExecutionConfig {
        ExecutionConfig {
            name: "demo".to_string(),
            use_runsc: false,
            agent: ExecutionComponentConfig {
                enabled: true,
                image: DEFAULT_CLADDING_BUILD_IMAGE.to_string(),
                build: None,
            },
            nw_sandbox: Some(ExecutionComponentConfig {
                enabled: true,
                image: DEFAULT_CLADDING_BUILD_IMAGE.to_string(),
                build: None,
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
    fn build_plan_reuses_embedded_default_image_across_components() {
        let plan = plan_image_builds(&config()).unwrap();
        assert_eq!(plan.len(), 1);
        assert_eq!(plan[0].image, DEFAULT_CLADDING_BUILD_IMAGE);
        assert_eq!(plan[0].build, BuildDefinition::Embedded);
    }

    #[test]
    fn build_plan_deduplicates_shared_custom_image_definitions() {
        let mut config = config();
        config.agent.image = "localhost/shared:latest".to_string();
        config.agent.build = Some(build("/tmp/context", "enabled"));
        config.nw_sandbox.as_mut().unwrap().image = "localhost/shared:latest".to_string();
        config.nw_sandbox.as_mut().unwrap().build = Some(build("/tmp/context", "enabled"));

        let plan = plan_image_builds(&config).unwrap();
        assert_eq!(plan.len(), 1);
        assert_eq!(plan[0].image, "localhost/shared:latest");
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

        assert!(plan_image_builds(&config).unwrap().is_empty());
    }

    #[test]
    fn build_plan_includes_custom_proxy_build() {
        let mut config = config();
        config.agent.image = "agent:prebuilt".to_string();
        config.nw_sandbox = None;
        config.proxy = Some(ExecutionProxyConfig {
            image: "localhost/proxy:latest".to_string(),
            build: Some(build("/tmp/proxy", "enabled")),
        });

        let plan = plan_image_builds(&config).unwrap();

        assert_eq!(plan.len(), 1);
        assert_eq!(plan[0].image, "localhost/proxy:latest");
        assert!(matches!(plan[0].build, BuildDefinition::Custom(_)));
    }
}
