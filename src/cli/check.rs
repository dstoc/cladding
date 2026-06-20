use super::DEFAULT_CLADDING_BUILD_IMAGE;
use super::context::Context;
use cladding::assets::{scripts_files, tool_files};
use cladding::config::{ExecutionConfig, load_cladding_config_v2};
use cladding::error::{Error, Result};
use cladding::fs_utils::{is_executable, path_is_symlink};
use cladding::podman::runsc_available;
use cladding::runtime::RuntimeSpec;
use std::collections::HashSet;
use std::fs;
use std::path::Path;
use std::process::Command;

pub(super) fn cmd_check(context: &Context) -> Result<()> {
    let legacy_config_entries_present = check_legacy_config_entries(context);
    let config = load_cladding_config_v2(&context.project_root)?;

    warn_obsolete_generated_paths(context);
    check_required_binaries(context, &config)?;
    check_runsc_runtime(&config, false)?;
    check_required_config_files(context, &config)?;
    check_required_images(&config, false)?;
    let spec = RuntimeSpec::build(&context.project_root, &config);
    check_required_host_paths(&spec)?;
    if legacy_config_entries_present {
        return Err(Error::message("legacy config entries"));
    }
    println!("check: ok");
    Ok(())
}

pub(super) fn check_required_binaries(context: &Context, config: &ExecutionConfig) -> Result<()> {
    let mut missing = false;
    let bin_dir = context.project_root.join("tools/bin");

    let mut required = vec!["mcp-run", "run-remote"];
    if config.nw_sandbox_enabled() {
        required.push("run-in-nw-sandbox");
    }
    if config.fs_sandbox_enabled() {
        required.push("run-in-fs-sandbox");
    }

    for name in required {
        let path = bin_dir.join(name);
        if !is_executable(&path) {
            eprintln!("missing: tools/bin/{name} ({})", path.display());
            eprintln!("hint: run cladding build");
            missing = true;
            continue;
        }

        let Some((_, embedded)) = tool_files()
            .into_iter()
            .find(|(embedded_name, _)| *embedded_name == name)
        else {
            continue;
        };

        match fs::read(&path) {
            Ok(existing) if existing == embedded => {}
            Ok(_) => {
                eprintln!("outdated: tools/bin/{name} ({})", path.display());
                eprintln!("hint: run cladding build");
                missing = true;
            }
            Err(err) => {
                eprintln!("error: failed to read tools/bin/{name} ({err})");
                eprintln!("hint: run cladding build");
                missing = true;
            }
        }
    }

    if missing {
        return Err(Error::message("missing tools binaries"));
    }

    Ok(())
}

pub(super) fn check_required_config_files(
    context: &Context,
    config: &ExecutionConfig,
) -> Result<()> {
    let dst = context.project_root.join("config");
    let mut missing = false;

    for name in required_config_entries(config) {
        let path = dst.join(name);
        if !path.exists() {
            eprintln!("missing: config/{name} ({})", path.display());
            missing = true;
        }
    }

    if missing {
        eprintln!(
            "hint: run cladding init, or add missing config paths under {}",
            dst.display()
        );
        return Err(Error::message("missing config files"));
    }

    Ok(())
}

fn check_legacy_config_entries(context: &Context) -> bool {
    let dst = context.project_root.join("config");
    let mut legacy = false;

    for (legacy_name, replacement) in legacy_config_entries() {
        let path = dst.join(*legacy_name);
        if path.exists() {
            eprintln!(
                "error: legacy config/{legacy_name} exists ({})",
                path.display()
            );
            eprintln!("hint: replace config/{legacy_name} with config/{replacement}");
            legacy = true;
        }
    }

    legacy
}

fn required_config_entries(config: &ExecutionConfig) -> Vec<&'static str> {
    let mut entries = vec!["agent/domains.lst", "agent/host_ports.lst"];
    if config.nw_sandbox_enabled() {
        entries.push("nw_sandbox");
        entries.push("nw_sandbox/domains.lst");
    }
    if config.fs_sandbox_enabled() {
        entries.push("fs_sandbox");
        entries.push("fs_sandbox/main.rego");
    }
    entries
}

fn legacy_config_entries() -> &'static [(&'static str, &'static str)] {
    &[
        ("sandbox_commands", "nw_sandbox"),
        ("sandbox_domains.lst", "nw_sandbox/domains.lst"),
        ("cli_domains.lst", "agent/domains.lst"),
        ("cli_host_ports.lst", "agent/host_ports.lst"),
        ("agent_domains.lst", "agent/domains.lst"),
        ("agent_host_ports.lst", "agent/host_ports.lst"),
        ("nw_sandbox_domains.lst", "nw_sandbox/domains.lst"),
    ]
}

pub(super) fn warn_obsolete_generated_paths(context: &Context) {
    for rel_path in ["scripts", "config/proxy/squid.conf", "config/squid.conf"] {
        let path = context.project_root.join(rel_path);
        if path.exists() || path_is_symlink(&path) {
            eprintln!(
                "warning: {rel_path} is no longer used and can be removed ({})",
                path.display()
            );
        }
    }
}

pub(super) fn report_runtime_script_mismatch(context: &Context, level: &str) -> Result<bool> {
    let dst = context.project_root.join("runtime/scripts");
    let mut mismatched = false;

    for (rel_path, contents) in scripts_files() {
        let target = dst.join(&rel_path);
        match fs::read(&target) {
            Ok(existing) => {
                if existing != contents {
                    eprintln!(
                        "{level}: runtime/scripts/{} differs from embedded version",
                        rel_path.display()
                    );
                    mismatched = true;
                }
            }
            Err(_) => {
                eprintln!("{level}: runtime/scripts/{} is missing", rel_path.display());
                mismatched = true;
            }
        }
    }

    if mismatched {
        eprintln!("hint: run cladding up to regenerate runtime scripts");
    }

    Ok(mismatched)
}

pub(super) fn check_required_host_paths(spec: &RuntimeSpec) -> Result<()> {
    let mut missing = false;
    let mut seen = HashSet::new();
    for path in spec.required_host_paths() {
        if !seen.insert(path.clone()) {
            continue;
        }
        let host_path = Path::new(&path);
        if !host_path.exists() {
            eprintln!("missing: hostPath {}", host_path.display());
            eprintln!("hint: create or relink {}", host_path.display());
            missing = true;
        }
    }

    if missing {
        return Err(Error::message("missing host paths"));
    }

    Ok(())
}

pub(super) fn check_required_images(config: &ExecutionConfig, verbose: bool) -> Result<()> {
    let mut missing = false;
    let mut images = vec![("agent", config.agent_image())];
    if config.nw_sandbox_enabled() {
        images.push(("nw_sandbox", config.nw_sandbox_image()));
    }
    if config.fs_sandbox_enabled() {
        images.push(("fs_sandbox", config.fs_sandbox_image()));
    }

    let mut seen = HashSet::new();
    for (label, image) in images {
        if !seen.insert(image.to_string()) {
            continue;
        }
        let mut cmd = Command::new("podman");
        cmd.args(["image", "exists", image]);
        cladding::podman::trace_command(&cmd, verbose);
        let status = cmd.status();

        match status {
            Ok(status) if status.success() => {}
            Ok(_) => {
                eprintln!("missing: image {image}");
                if image_is_buildable_by_cladding(image) {
                    eprintln!("hint: run cladding build");
                } else {
                    eprintln!(
                        "hint: pull/tag image '{image}', or set cladding.json {label}.image to a supported build target and run cladding build"
                    );
                }
                missing = true;
            }
            Err(err) => {
                eprintln!("error: failed to check image {image}: {err}");
                return Err(Error::message("failed to check image"));
            }
        }
    }

    if missing {
        return Err(Error::message("missing required images"));
    }

    Ok(())
}

pub(super) fn check_runsc_runtime(config: &ExecutionConfig, verbose: bool) -> Result<()> {
    if !config.use_runsc {
        return Ok(());
    }

    match runsc_available(verbose) {
        Ok(true) => Ok(()),
        Ok(false) => {
            eprintln!(
                "missing: runsc (not found on PATH and Podman does not report a runtime named 'runsc')"
            );
            eprintln!("hint: install runsc or configure Podman to expose a runtime named 'runsc'");
            Err(Error::message("missing runsc runtime"))
        }
        Err(err) => {
            eprintln!("error: failed to check runsc availability: {err}");
            eprintln!(
                "hint: install runsc or verify 'podman info' can inspect configured runtimes"
            );
            Err(Error::message("failed to check runsc availability"))
        }
    }
}

fn image_is_buildable_by_cladding(image: &str) -> bool {
    image == DEFAULT_CLADDING_BUILD_IMAGE
}

#[cfg(test)]
mod tests {
    use super::*;
    use cladding::assets::write_embedded_tools;
    use cladding::config::{ExecutionComponentConfig, ResolvedMountConfig};
    use std::env;
    use std::path::PathBuf;
    use std::sync::atomic::{AtomicU64, Ordering};

    static TEMP_DIR_COUNTER: AtomicU64 = AtomicU64::new(0);

    #[test]
    fn required_config_entries_use_normalized_layout() {
        let config = execution_config(false, false, Vec::new());
        assert_eq!(
            required_config_entries(&config),
            vec!["agent/domains.lst", "agent/host_ports.lst"]
        );
    }

    #[test]
    fn required_config_entries_include_enabled_fs_sandbox() {
        let config = execution_config(false, true, Vec::new());
        assert_eq!(
            required_config_entries(&config),
            vec![
                "agent/domains.lst",
                "agent/host_ports.lst",
                "fs_sandbox",
                "fs_sandbox/main.rego",
            ]
        );
    }

    #[test]
    fn legacy_config_entries_cover_pre_rename_and_flat_layouts() {
        assert_eq!(
            legacy_config_entries(),
            &[
                ("sandbox_commands", "nw_sandbox"),
                ("sandbox_domains.lst", "nw_sandbox/domains.lst"),
                ("cli_domains.lst", "agent/domains.lst"),
                ("cli_host_ports.lst", "agent/host_ports.lst"),
                ("agent_domains.lst", "agent/domains.lst"),
                ("agent_host_ports.lst", "agent/host_ports.lst"),
                ("nw_sandbox_domains.lst", "nw_sandbox/domains.lst"),
            ]
        );
    }

    #[test]
    fn check_legacy_config_entries_detects_old_layout_paths() {
        let temp = create_temp_dir("legacy-config-paths");
        let config_dir = temp.join("config");
        fs::create_dir_all(&config_dir).expect("create config dir");
        fs::write(config_dir.join("cli_domains.lst"), "legacy").expect("write legacy config");

        let context = Context { project_root: temp };

        assert!(check_legacy_config_entries(&context));
    }

    #[test]
    fn report_runtime_script_mismatch_detects_drift() {
        let temp = create_temp_dir("runtime-script-mismatch");
        let scripts_dir = temp.join("runtime/scripts");
        fs::create_dir_all(&scripts_dir).expect("create scripts dir");
        for (rel_path, contents) in scripts_files() {
            let path = scripts_dir.join(rel_path);
            if let Some(parent) = path.parent() {
                fs::create_dir_all(parent).expect("create script parent");
            }
            fs::write(path, contents).expect("write script");
        }

        let context = Context { project_root: temp };
        assert!(!report_runtime_script_mismatch(&context, "error").expect("check scripts"));

        fs::write(
            context
                .project_root
                .join("runtime/scripts/proxy_startup.sh"),
            b"#!/bin/sh\nexit 0\n",
        )
        .expect("modify script");

        assert!(report_runtime_script_mismatch(&context, "error").expect("check scripts"));
    }

    #[test]
    fn check_required_binaries_detects_outdated_tools() {
        let temp = create_temp_dir("outdated-tools");
        let bin_dir = temp.join("tools/bin");
        fs::create_dir_all(&bin_dir).expect("create bin dir");
        write_embedded_tools(&bin_dir).expect("write tools");

        let context = Context {
            project_root: temp.clone(),
        };
        let config = execution_config(true, true, Vec::new());

        assert!(check_required_binaries(&context, &config).is_ok());

        fs::write(bin_dir.join("mcp-run"), b"stale").expect("stale tool");
        assert!(check_required_binaries(&context, &config).is_err());
    }

    fn execution_config(
        nw_enabled: bool,
        fs_enabled: bool,
        mounts: Vec<ResolvedMountConfig>,
    ) -> ExecutionConfig {
        ExecutionConfig {
            name: "demo".to_string(),
            use_runsc: false,
            agent: ExecutionComponentConfig {
                enabled: true,
                image: "agent:image".to_string(),
            },
            nw_sandbox: nw_enabled.then(|| ExecutionComponentConfig {
                enabled: true,
                image: "sandbox:image".to_string(),
            }),
            fs_sandbox: fs_enabled.then(|| ExecutionComponentConfig {
                enabled: true,
                image: "fs:image".to_string(),
            }),
            mounts,
        }
    }

    fn create_temp_dir(name: &str) -> PathBuf {
        let unique = TEMP_DIR_COUNTER.fetch_add(1, Ordering::Relaxed);
        let path =
            env::temp_dir().join(format!("cladding-{name}-{}-{}", std::process::id(), unique));
        if path.exists() {
            fs::remove_dir_all(&path).expect("cleanup stale temp dir");
        }
        fs::create_dir_all(&path).expect("create temp dir");
        path
    }
}
