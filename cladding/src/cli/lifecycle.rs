use super::check::{
    check_required_binaries, check_required_config_files, check_required_host_paths,
    check_required_images, check_runsc_runtime, report_runtime_script_mismatch,
    warn_obsolete_generated_paths,
};
use super::context::{Context, project_runtime_status};
use super::{DEFAULT_CLADDING_BUILD_IMAGE, DEFAULT_CLI_BUILD_IMAGE, DEFAULT_SANDBOX_BUILD_IMAGE};
use anyhow::Context as _;
use cladding::assets::{materialize_config, materialize_runtime_scripts, write_embedded_tools};
use cladding::config::{load_cladding_config_v2, write_default_cladding_config};
use cladding::error::{Error, Result};
use cladding::fs_utils::{is_broken_symlink, path_is_symlink};
use cladding::podman::{
    list_running_projects, podman_build_image, podman_required, runtime_cleanup, runtime_create,
};
use cladding::runtime::RuntimeSpec;
use std::collections::HashSet;
use std::fs;

pub(super) fn cmd_build(context: &Context) -> Result<()> {
    let config = load_cladding_config_v2(&context.project_root)?;

    let host_uid = unsafe { libc::getuid() };
    let host_gid = unsafe { libc::getgid() };

    let tools_dir = context.project_root.join("tools");
    if is_broken_symlink(&tools_dir)? {
        eprintln!("missing: tools (broken symlink at {})", tools_dir.display());
        eprintln!("hint: create or relink {}", tools_dir.display());
        return Err(Error::message("missing tools"));
    }

    let tools_bin_dir = tools_dir.join("bin");
    fs::create_dir_all(&tools_bin_dir).with_context(|| "failed to create tools directory")?;

    write_embedded_tools(&tools_bin_dir)?;

    let mut built_images = HashSet::new();
    build_default_image(
        "agent",
        config.agent_image(),
        host_uid,
        host_gid,
        &mut built_images,
    )?;
    if config.nw_sandbox_enabled() {
        build_default_image(
            "nw sandbox",
            config.nw_sandbox_image(),
            host_uid,
            host_gid,
            &mut built_images,
        )?;
    }
    if config.fs_sandbox_enabled() {
        build_default_image(
            "fs sandbox",
            config.fs_sandbox_image(),
            host_uid,
            host_gid,
            &mut built_images,
        )?;
    }

    Ok(())
}

fn build_default_image(
    label: &str,
    image: &str,
    host_uid: u32,
    host_gid: u32,
    built_images: &mut HashSet<String>,
) -> Result<()> {
    if !built_images.insert(image.to_string()) {
        println!("skip: {label} image already built ({image})");
        return Ok(());
    }

    if image != DEFAULT_CLADDING_BUILD_IMAGE {
        println!(
            "skip: not building {label} image (config image is {}, build target is {})",
            image, DEFAULT_CLADDING_BUILD_IMAGE
        );
        return Ok(());
    }

    podman_build_image(image, host_uid, host_gid)
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
    let config = load_cladding_config_v2(&context.project_root)?;
    materialize_runtime_scripts(&context.project_root)?;
    let status = project_runtime_status(context, &config, verbose)?;

    if status.already_running {
        println!(
            "already running: {} ({})",
            config.name, status.current_project_root
        );
        return Ok(());
    }

    check_required_binaries(context, &config)?;
    check_runsc_runtime(&config, verbose)?;
    check_required_images(&config, verbose)?;
    check_required_config_files(context, &config)?;
    warn_obsolete_generated_paths(context);
    let _ = report_runtime_script_mismatch(context, "warning")?;
    let spec = RuntimeSpec::build(&context.project_root, &config);
    fs::create_dir_all(context.project_root.join("runtime/empty-mask"))
        .with_context(|| "failed to create runtime empty-mask directory")?;
    check_required_host_paths(&spec)?;
    runtime_create(&spec, verbose)
}

pub(super) fn cmd_down(context: &Context, verbose: bool) -> Result<()> {
    let config = load_cladding_config_v2(&context.project_root)?;
    let spec = RuntimeSpec::build(&context.project_root, &config);
    let mut cleanup_error = None;
    record_cleanup_result(&mut cleanup_error, runtime_cleanup(&spec, verbose));

    match cleanup_error {
        Some(err) => Err(err),
        None => Ok(()),
    }
}

pub(super) fn cmd_destroy(context: &Context) -> Result<()> {
    let config = load_cladding_config_v2(&context.project_root)?;
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
