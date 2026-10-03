use super::DEFAULT_CLADDING_BUILD_IMAGE;
use super::context::Context;
use cladding::assets::tool_files;
use cladding::config::{DEFAULT_PROXY_IMAGE, ExecutionConfig};
use cladding::error::{Error, Result};
use cladding::fs_utils::is_executable;
use cladding::podman::runsc_available;
use cladding::runtime::RuntimeSpec;
use std::collections::HashSet;
use std::fs;
use std::path::Path;
use std::process::Command;

pub(super) fn cmd_check(context: &Context) -> Result<()> {
    let legacy_config_entries_present = check_legacy_config_entries(context);
    let config = context.load_config()?;
    let spec = context.runtime_spec(&config)?;
    check_project_readiness(context, &config, &spec, false, false)?;
    if legacy_config_entries_present {
        return Err(Error::message("legacy config entries"));
    }
    println!("check: ok");
    Ok(())
}

/// Check every project prerequisite needed before a runtime starts.
pub(super) fn check_project_readiness(
    context: &Context,
    config: &ExecutionConfig,
    spec: &RuntimeSpec,
    verbose: bool,
    quiet_helpers: bool,
) -> Result<()> {
    check_required_binaries(context, config)?;
    check_runsc_runtime(config, verbose, quiet_helpers)?;
    check_required_config_files(context, config)?;
    check_required_images(config, verbose, quiet_helpers)?;
    check_baffle_ca_readiness(&context.project_root)?;
    check_required_host_paths(spec)
}

fn check_baffle_ca_readiness(project_root: &Path) -> Result<()> {
    cladding::credentials::validate_baffle_ca(project_root)
}

pub(super) fn check_required_binaries(context: &Context, config: &ExecutionConfig) -> Result<()> {
    let mut missing = false;
    let bin_dir = context.project_root.join("tools/bin");

    let mut required = vec!["mcp-run", "run-remote", "baffle"];
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
    let mut invalid_sessions = false;

    for name in required_config_entries(config) {
        let path = dst.join(&name);
        if !path.exists() {
            eprintln!("missing: config/{name} ({})", path.display());
            missing = true;
        }
    }

    let mut session_configs = vec![("agent", config.agent_session_config(), "agent/proxy.sock")];
    if config.nw_sandbox_enabled() {
        session_configs.push((
            "network sandbox",
            config.nw_sandbox_session_config(),
            "nw-sandbox/proxy.sock",
        ));
    }
    for (component, name, expected_socket_name) in session_configs {
        if let Err(reason) = validate_selected_session_config(&dst, name, expected_socket_name) {
            eprintln!("invalid: {component} Baffle session config '{name}': {reason}");
            invalid_sessions = true;
        }
    }

    if missing {
        eprintln!(
            "hint: run cladding init, or add missing config paths under {}",
            dst.display()
        );
        return Err(Error::message("missing config files"));
    }
    if invalid_sessions {
        eprintln!(
            "hint: select a valid version 2 Baffle session TOML file under config/proxy/sessions"
        );
        return Err(Error::message("invalid Baffle session config"));
    }

    Ok(())
}

fn validate_selected_session_config(
    config_dir: &Path,
    name: &str,
    expected_socket_name: &str,
) -> anyhow::Result<()> {
    cladding::config::validate_session_config_path(name)
        .map_err(|reason| anyhow::anyhow!("{reason}"))?;

    let config_metadata = fs::symlink_metadata(config_dir)
        .map_err(|error| anyhow::anyhow!("cannot inspect {}: {error}", config_dir.display()))?;
    if config_metadata.file_type().is_symlink() || !config_metadata.is_dir() {
        anyhow::bail!(
            "config path is not a real directory: {}",
            config_dir.display()
        );
    }
    validate_baffle_session_path_metadata(&config_metadata, true)?;

    let sessions_dir = config_dir.join("proxy/sessions");
    let mut current = config_dir.to_path_buf();
    for component in ["proxy", "sessions"] {
        current.push(component);
        let metadata = fs::symlink_metadata(&current)
            .map_err(|error| anyhow::anyhow!("cannot inspect {}: {error}", current.display()))?;
        if metadata.file_type().is_symlink() || !metadata.is_dir() {
            anyhow::bail!(
                "session config directory is not a real directory: {}",
                current.display()
            );
        }
        validate_baffle_session_path_metadata(&metadata, true)?;
    }

    let canonical_sessions_dir = fs::canonicalize(&sessions_dir)
        .map_err(|error| anyhow::anyhow!("cannot resolve {}: {error}", sessions_dir.display()))?;
    let relative = Path::new(name);
    let mut selected = sessions_dir.clone();
    for (index, component) in relative.components().enumerate() {
        let std::path::Component::Normal(component) = component else {
            anyhow::bail!("session config path contains an unsafe component");
        };
        selected.push(component);
        let metadata = fs::symlink_metadata(&selected)
            .map_err(|error| anyhow::anyhow!("cannot inspect {}: {error}", selected.display()))?;
        if metadata.file_type().is_symlink() {
            anyhow::bail!("session config path contains a symlink");
        }
        if index + 1 == relative.components().count() {
            if !metadata.is_file() {
                anyhow::bail!("selected session config is not a regular file");
            }
            validate_baffle_session_path_metadata(&metadata, false)?;
        } else if !metadata.is_dir() {
            anyhow::bail!("session config parent is not a directory");
        } else {
            validate_baffle_session_path_metadata(&metadata, true)?;
        }
    }

    let canonical_selected = fs::canonicalize(&selected)
        .map_err(|error| anyhow::anyhow!("cannot resolve {}: {error}", selected.display()))?;
    if !canonical_selected.starts_with(&canonical_sessions_dir) {
        anyhow::bail!("selected session config resolves outside the sessions directory");
    }
    let contents = fs::read_to_string(&canonical_selected)
        .map_err(|error| anyhow::anyhow!("cannot read selected session config: {error}"))?;
    cladding::config::validate_baffle_session_config_for_socket(&contents, expected_socket_name)
}

fn validate_baffle_session_path_metadata(
    metadata: &fs::Metadata,
    is_directory: bool,
) -> anyhow::Result<()> {
    use std::os::unix::fs::{MetadataExt, PermissionsExt};

    let uid = metadata.uid();
    let trusted_uid = unsafe { libc::geteuid() };
    if uid != 0 && uid != trusted_uid {
        anyhow::bail!("Baffle session paths must be owned by root or the current user");
    }

    let mode = metadata.permissions().mode();
    if mode & 0o022 != 0 {
        anyhow::bail!("Baffle session paths must not be group- or world-writable");
    }
    let has_read_access = if is_directory {
        mode & 0o500 == 0o500
    } else {
        mode & 0o444 != 0
    };
    if !has_read_access {
        anyhow::bail!("Baffle session path does not have the required read access");
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

fn required_config_entries(config: &ExecutionConfig) -> Vec<String> {
    let mut entries = vec![
        "proxy/daemon.toml".to_string(),
        format!("proxy/sessions/{}", config.agent_session_config()),
    ];
    if config.nw_sandbox_enabled() {
        entries.push(format!(
            "proxy/sessions/{}",
            config.nw_sandbox_session_config()
        ));
    }
    if config.nw_sandbox_enabled() {
        entries.push("nw_sandbox".to_string());
    }
    if config.fs_sandbox_enabled() {
        entries.push("fs_sandbox".to_string());
        entries.push("fs_sandbox/main.rego".to_string());
    }
    entries
}

fn legacy_config_entries() -> &'static [(&'static str, &'static str)] {
    &[("sandbox_commands", "nw_sandbox")]
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

pub(super) fn check_required_images(
    config: &ExecutionConfig,
    verbose: bool,
    quiet_helpers: bool,
) -> Result<()> {
    let mut missing = false;
    let mut images = vec![
        ("agent", config.agent_image()),
        ("proxy", config.proxy_image()),
    ];
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
        let output = if quiet_helpers {
            Some(cmd.output())
        } else {
            None
        };
        let status = match output {
            Some(Ok(output)) => Some((output.status, Some(output))),
            Some(Err(err)) => {
                eprintln!("error: failed to check image {image}: {err}");
                return Err(Error::message("failed to check image"));
            }
            None => match cmd.status() {
                Ok(status) => Some((status, None)),
                Err(err) => {
                    eprintln!("error: failed to check image {image}: {err}");
                    return Err(Error::message("failed to check image"));
                }
            },
        };

        match status {
            Some((status, _)) if status.success() => {}
            Some((_, Some(output))) => {
                eprintln!("missing: image {image}");
                let stdout = String::from_utf8_lossy(&output.stdout);
                let stderr = String::from_utf8_lossy(&output.stderr);
                if !stdout.trim().is_empty() {
                    eprintln!("podman image exists stdout:\n{}", stdout.trim_end());
                }
                if !stderr.trim().is_empty() {
                    eprintln!("podman image exists stderr:\n{}", stderr.trim_end());
                }
                if image_is_buildable_by_cladding(config, image) {
                    eprintln!("hint: run cladding build");
                } else {
                    eprintln!(
                        "hint: pull/tag image '{image}', or set cladding.json {label}.image to a supported build target and run cladding build"
                    );
                }
                missing = true;
            }
            Some((_, None)) => {
                eprintln!("missing: image {image}");
                if image_is_buildable_by_cladding(config, image) {
                    eprintln!("hint: run cladding build");
                } else {
                    eprintln!(
                        "hint: pull/tag image '{image}', or set cladding.json {label}.image to a supported build target and run cladding build"
                    );
                }
                missing = true;
            }
            None => unreachable!("status is populated for each image check"),
        }
    }

    if missing {
        return Err(Error::message("missing required images"));
    }

    Ok(())
}

pub(super) fn check_runsc_runtime(
    config: &ExecutionConfig,
    verbose: bool,
    quiet_helpers: bool,
) -> Result<()> {
    if !config.use_runsc {
        return Ok(());
    }

    match runsc_available(verbose, quiet_helpers) {
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

fn image_is_buildable_by_cladding(config: &ExecutionConfig, image: &str) -> bool {
    image == DEFAULT_CLADDING_BUILD_IMAGE
        || image == DEFAULT_PROXY_IMAGE
        || (config.agent.image == image && config.agent.build.is_some())
        || config
            .nw_sandbox
            .as_ref()
            .is_some_and(|component| component.image == image && component.build.is_some())
        || config
            .fs_sandbox
            .as_ref()
            .is_some_and(|component| component.image == image && component.build.is_some())
        || config
            .proxy
            .as_ref()
            .is_some_and(|proxy| proxy.image == image && proxy.build.is_some())
}

#[cfg(test)]
mod tests {
    use super::*;
    use cladding::assets::write_embedded_tools;
    use cladding::config::{ExecutionComponentConfig, ResolvedMountConfig};
    use std::env;
    use std::fs;
    use std::path::PathBuf;
    use std::sync::atomic::{AtomicU64, Ordering};

    static TEMP_DIR_COUNTER: AtomicU64 = AtomicU64::new(0);

    #[test]
    fn readiness_requires_a_project_ca_without_initializing_it() {
        let root = create_temp_dir("ca-readiness");
        cladding::credentials::ensure_baffle_credentials(&root).unwrap();

        let error = check_baffle_ca_readiness(&root).unwrap_err().to_string();
        assert!(error.contains("run `cladding build`"), "{error}");
        assert!(!root.join("credentials/baffle/ca.crt").exists());
        assert!(!root.join("credentials/baffle/ca-key.pem").exists());
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn required_config_entries_use_normalized_layout() {
        let config = execution_config(false, false, Vec::new());
        assert_eq!(
            required_config_entries(&config),
            vec![
                "proxy/daemon.toml".to_string(),
                "proxy/sessions/agent.toml".to_string(),
            ]
        );
    }

    #[test]
    fn default_proxy_image_is_buildable_by_cladding() {
        let config = execution_config(false, false, Vec::new());
        assert!(image_is_buildable_by_cladding(&config, DEFAULT_PROXY_IMAGE));
    }

    #[test]
    fn required_config_entries_keep_network_sandbox_policy_files() {
        let config = execution_config(true, false, Vec::new());
        assert_eq!(
            required_config_entries(&config),
            vec![
                "proxy/daemon.toml".to_string(),
                "proxy/sessions/agent.toml".to_string(),
                "proxy/sessions/nw-sandbox.toml".to_string(),
                "nw_sandbox".to_string(),
            ]
        );
    }

    #[test]
    fn required_config_entries_include_enabled_fs_sandbox() {
        let config = execution_config(false, true, Vec::new());
        assert_eq!(
            required_config_entries(&config),
            vec![
                "proxy/daemon.toml".to_string(),
                "proxy/sessions/agent.toml".to_string(),
                "fs_sandbox".to_string(),
                "fs_sandbox/main.rego".to_string(),
            ]
        );
    }

    #[test]
    fn check_uses_independent_selected_session_files_and_skips_disabled_sandbox_file() {
        let root = create_temp_dir("selected-session-files");
        let sessions = root.join("config/proxy/sessions");
        fs::create_dir_all(&sessions).unwrap();
        fs::write(root.join("config/proxy/daemon.toml"), "[daemon]\n").unwrap();
        fs::write(
            sessions.join("agent-custom.toml"),
            "version = 2\nsocket_name = 'agent/proxy.sock'\n",
        )
        .unwrap();

        let mut config = execution_config(false, false, Vec::new());
        config.proxy = Some(cladding::config::ExecutionProxyConfig {
            image: DEFAULT_PROXY_IMAGE.to_string(),
            build: None,
            agent_session_config: "agent-custom.toml".to_string(),
            nw_sandbox_session_config: "missing-restricted.toml".to_string(),
        });

        let context = Context::default_for_project(root.clone());
        assert!(check_required_config_files(&context, &config).is_ok());
        assert!(
            !required_config_entries(&config)
                .iter()
                .any(|entry| entry.ends_with("missing-restricted.toml"))
        );
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn check_requires_and_validates_the_selected_enabled_sandbox_session() {
        let root = create_temp_dir("enabled-selected-session");
        let sessions = root.join("config/proxy/sessions");
        fs::create_dir_all(sessions.join("restricted")).unwrap();
        fs::create_dir_all(root.join("config/nw_sandbox")).unwrap();
        fs::write(root.join("config/proxy/daemon.toml"), "[daemon]\n").unwrap();
        fs::write(
            sessions.join("agent-custom.toml"),
            "version = 2\nsocket_name = 'agent/proxy.sock'\n",
        )
        .unwrap();
        fs::write(
            sessions.join("restricted/nw.toml"),
            "version = 2\nsocket_name = 'nw-sandbox/proxy.sock'\nunmatched = 'tunnel'\n",
        )
        .unwrap();

        let mut config = execution_config(true, false, Vec::new());
        config.proxy = Some(cladding::config::ExecutionProxyConfig {
            image: DEFAULT_PROXY_IMAGE.to_string(),
            build: None,
            agent_session_config: "agent-custom.toml".to_string(),
            nw_sandbox_session_config: "restricted/nw.toml".to_string(),
        });
        let context = Context::default_for_project(root.clone());

        assert!(check_required_config_files(&context, &config).is_ok());
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;

            let selected = sessions.join("restricted/nw.toml");
            fs::set_permissions(&selected, fs::Permissions::from_mode(0o666)).unwrap();
            assert!(check_required_config_files(&context, &config).is_err());
            fs::set_permissions(&selected, fs::Permissions::from_mode(0o644)).unwrap();
        }
        fs::write(sessions.join("restricted/nw.toml"), "version = 1\n").unwrap();
        assert!(check_required_config_files(&context, &config).is_err());
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn check_rejects_selected_sessions_with_the_other_components_socket() {
        let root = create_temp_dir("selected-session-socket-name");
        let sessions = root.join("config/proxy/sessions");
        fs::create_dir_all(sessions.join("restricted")).unwrap();
        fs::create_dir_all(root.join("config/nw_sandbox")).unwrap();
        fs::write(root.join("config/proxy/daemon.toml"), "[daemon]\n").unwrap();
        let agent_session = sessions.join("agent-custom.toml");
        let sandbox_session = sessions.join("restricted/nw.toml");
        fs::write(
            &agent_session,
            "version = 2\nsocket_name = 'agent/proxy.sock'\n",
        )
        .unwrap();
        fs::write(
            &sandbox_session,
            "version = 2\nsocket_name = 'nw-sandbox/proxy.sock'\n",
        )
        .unwrap();

        let mut config = execution_config(true, false, Vec::new());
        config.proxy = Some(cladding::config::ExecutionProxyConfig {
            image: DEFAULT_PROXY_IMAGE.to_string(),
            build: None,
            agent_session_config: "agent-custom.toml".to_string(),
            nw_sandbox_session_config: "restricted/nw.toml".to_string(),
        });
        let context = Context::default_for_project(root.clone());
        assert!(check_required_config_files(&context, &config).is_ok());

        fs::write(
            &agent_session,
            "version = 2\nsocket_name = 'nw-sandbox/proxy.sock'\n",
        )
        .unwrap();
        assert!(check_required_config_files(&context, &config).is_err());

        fs::write(
            &agent_session,
            "version = 2\nsocket_name = 'agent/proxy.sock'\n",
        )
        .unwrap();
        fs::write(
            &sandbox_session,
            "version = 2\nsocket_name = 'agent/proxy.sock'\n",
        )
        .unwrap();
        assert!(check_required_config_files(&context, &config).is_err());
        fs::remove_dir_all(root).unwrap();
    }

    #[cfg(unix)]
    #[test]
    fn check_rejects_session_symlinks() {
        use std::os::unix::fs::symlink;

        let root = create_temp_dir("selected-session-symlink");
        let sessions = root.join("config/proxy/sessions");
        fs::create_dir_all(&sessions).unwrap();
        fs::write(root.join("config/proxy/daemon.toml"), "[daemon]\n").unwrap();
        let outside = root.join("outside.toml");
        fs::write(&outside, "version = 2\n").unwrap();
        symlink(&outside, sessions.join("agent.toml")).unwrap();

        let config = execution_config(false, false, Vec::new());
        let context = Context::default_for_project(root.clone());
        assert!(check_required_config_files(&context, &config).is_err());
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn legacy_config_entries_preserve_command_policy_validation() {
        assert_eq!(
            legacy_config_entries(),
            &[("sandbox_commands", "nw_sandbox")]
        );
    }

    #[test]
    fn check_legacy_config_entries_ignores_squid_allow_lists() {
        let temp = create_temp_dir("squid-allow-lists");
        let config_dir = temp.join("config");
        fs::create_dir_all(&config_dir).expect("create config dir");
        fs::write(config_dir.join("cli_domains.lst"), "legacy").expect("write old domain list");
        fs::write(config_dir.join("agent_host_ports.lst"), "legacy")
            .expect("write old host-port list");

        let context = Context::default_for_project(temp);

        assert!(!check_legacy_config_entries(&context));
    }

    #[test]
    fn check_legacy_config_entries_still_detects_old_command_policy_path() {
        let temp = create_temp_dir("legacy-command-policy");
        let config_dir = temp.join("config");
        fs::create_dir_all(&config_dir).expect("create config dir");
        fs::create_dir_all(config_dir.join("sandbox_commands"))
            .expect("create legacy command policy dir");

        let context = Context::default_for_project(temp);

        assert!(check_legacy_config_entries(&context));
    }

    #[test]
    fn check_required_binaries_detects_outdated_tools() {
        let temp = create_temp_dir("outdated-tools");
        let bin_dir = temp.join("tools/bin");
        fs::create_dir_all(&bin_dir).expect("create bin dir");
        write_embedded_tools(&bin_dir).expect("write tools");

        let context = Context::default_for_project(temp.clone());
        let config = execution_config(true, true, Vec::new());

        assert!(check_required_binaries(&context, &config).is_ok());

        fs::write(bin_dir.join("baffle"), b"stale").expect("stale tool");
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
                build: None,
            },
            nw_sandbox: nw_enabled.then(|| ExecutionComponentConfig {
                enabled: true,
                image: "sandbox:image".to_string(),
                build: None,
            }),
            fs_sandbox: fs_enabled.then(|| ExecutionComponentConfig {
                enabled: true,
                image: "fs:image".to_string(),
                build: None,
            }),
            proxy: None,
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
