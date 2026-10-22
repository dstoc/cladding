use super::{
    ExecutionConfig, validate_baffle_session_config_for_socket, validate_session_config_path,
};
use crate::identity::invoking_uid;
use anyhow::Context as _;
use std::fs;
use std::path::Path;

/// Validate the Baffle file-only configuration paths and every session-tree entry.
pub fn validate_baffle_config_layout(config_dir: &Path) -> anyhow::Result<()> {
    for path in [
        config_dir.to_path_buf(),
        config_dir.join("proxy"),
        config_dir.join("proxy/sessions"),
    ] {
        validate_baffle_config_path(&path, true)?;
    }

    validate_baffle_config_path(&config_dir.join("proxy/daemon.toml"), false)?;
    validate_baffle_config_tree(&config_dir.join("proxy/sessions"))
}

/// Validate the selected session files used by the enabled Cladding components.
///
/// Baffle owns policy schema and rule validation. Cladding checks its supported
/// document version and the data socket assigned to each component.
pub fn validate_baffle_session_files(
    config_dir: &Path,
    config: &ExecutionConfig,
) -> anyhow::Result<()> {
    validate_baffle_config_layout(config_dir)?;
    validate_selected_session(
        config_dir,
        config.agent_session_config(),
        "agent",
        "agent/proxy.sock",
    )?;
    if config.nw_sandbox_enabled() {
        validate_selected_session(
            config_dir,
            config.nw_sandbox_session_config(),
            "network sandbox",
            "nw-sandbox/proxy.sock",
        )?;
    }
    Ok(())
}

fn validate_selected_session(
    config_dir: &Path,
    name: &str,
    component: &str,
    expected_socket_name: &str,
) -> anyhow::Result<()> {
    validate_session_config_path(name).map_err(|reason| anyhow::anyhow!("{reason}"))?;

    let sessions_dir = config_dir.join("proxy/sessions");
    let mut selected = sessions_dir.clone();
    let components = name.split('/').collect::<Vec<_>>();
    for (index, component_name) in components.iter().enumerate() {
        selected.push(component_name);
        let is_file = index + 1 == components.len();
        validate_baffle_config_path(&selected, !is_file)?;
    }

    let canonical_sessions_dir = fs::canonicalize(&sessions_dir)
        .with_context(|| format!("cannot resolve {}", sessions_dir.display()))?;
    let canonical_selected = fs::canonicalize(&selected)
        .with_context(|| format!("cannot resolve {}", selected.display()))?;
    if !canonical_selected.starts_with(&canonical_sessions_dir) {
        anyhow::bail!("selected session config resolves outside the sessions directory");
    }

    let contents = fs::read_to_string(&canonical_selected)
        .with_context(|| format!("cannot read selected session config {}", selected.display()))?;
    validate_baffle_session_config_for_socket(&contents, expected_socket_name)
        .map_err(|error| anyhow::anyhow!("{component} Baffle session config '{name}': {error}"))
}

fn validate_baffle_config_tree(directory: &Path) -> anyhow::Result<()> {
    for entry in fs::read_dir(directory)
        .with_context(|| format!("failed to read {}", directory.display()))?
    {
        let entry = entry.with_context(|| format!("failed to read {}", directory.display()))?;
        let path = entry.path();
        let metadata = fs::symlink_metadata(&path)
            .with_context(|| format!("failed to inspect {}", path.display()))?;
        if metadata.file_type().is_symlink() {
            anyhow::bail!(
                "Baffle session paths cannot contain symbolic links: {}",
                path.display()
            );
        }
        if metadata.is_dir() {
            validate_baffle_config_path(&path, true)?;
            validate_baffle_config_tree(&path)?;
        } else if metadata.is_file() {
            validate_baffle_config_path(&path, false)?;
        } else {
            anyhow::bail!(
                "Baffle session paths require regular files and directories: {}",
                path.display()
            );
        }
    }
    Ok(())
}

fn validate_baffle_config_path(path: &Path, directory: bool) -> anyhow::Result<()> {
    let metadata = fs::symlink_metadata(path)
        .with_context(|| format!("failed to inspect {}", path.display()))?;
    if metadata.file_type().is_symlink() {
        anyhow::bail!(
            "Baffle session paths cannot contain symbolic links: {}",
            path.display()
        );
    }
    if (directory && !metadata.is_dir()) || (!directory && !metadata.is_file()) {
        anyhow::bail!(
            "Baffle file-only configuration requires a {} at {}",
            if directory {
                "directory"
            } else {
                "regular file"
            },
            path.display()
        );
    }

    validate_baffle_session_path_metadata(&metadata, directory, path)
}

fn validate_baffle_session_path_metadata(
    metadata: &fs::Metadata,
    is_directory: bool,
    path: &Path,
) -> anyhow::Result<()> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::{MetadataExt as _, PermissionsExt as _};

        validate_baffle_session_owner(metadata.uid(), invoking_uid())?;
        let mode = metadata.permissions().mode();
        if mode & 0o022 != 0 {
            anyhow::bail!(
                "Baffle session paths must not be group- or world-writable: {}",
                path.display()
            );
        }
        let has_read_access = if is_directory {
            mode & 0o500 == 0o500
        } else {
            mode & 0o444 != 0
        };
        if !has_read_access {
            anyhow::bail!(
                "Baffle session path does not have the required read access: {}",
                path.display()
            );
        }
    }
    #[cfg(not(unix))]
    let _ = (metadata, is_directory, path);

    Ok(())
}

#[cfg(unix)]
fn validate_baffle_session_owner(uid: u32, trusted_uid: u32) -> anyhow::Result<()> {
    if uid != 0 && uid != trusted_uid {
        anyhow::bail!("Baffle session paths must be owned by root or the current user");
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::{
        DEFAULT_PROXY_IMAGE, ExecutionComponentConfig, ExecutionProxyConfig, ResolvedMountConfig,
    };
    use std::path::PathBuf;
    use std::sync::atomic::{AtomicU64, Ordering};

    static TEMP_DIR_COUNTER: AtomicU64 = AtomicU64::new(0);

    fn create_config_tree(name: &str) -> PathBuf {
        let unique = TEMP_DIR_COUNTER.fetch_add(1, Ordering::Relaxed);
        let config_dir = std::env::temp_dir().join(format!(
            "cladding-session-files-{name}-{}-{unique}",
            std::process::id()
        ));
        if config_dir.exists() {
            fs::remove_dir_all(&config_dir).expect("remove stale test directory");
        }
        let sessions = config_dir.join("proxy/sessions");
        fs::create_dir_all(&sessions).expect("create sessions directory");
        fs::write(config_dir.join("proxy/daemon.toml"), "[daemon]\n").expect("write daemon config");
        fs::write(
            sessions.join("agent.toml"),
            "version = 2\nsocket_name = 'agent/proxy.sock'\n",
        )
        .expect("write agent session");
        config_dir
    }

    fn execution_config() -> ExecutionConfig {
        ExecutionConfig {
            name: "test".to_string(),
            use_runsc: false,
            fake_credential_env: std::collections::BTreeMap::new(),
            agent: ExecutionComponentConfig {
                enabled: true,
                init: false,
                image: "test".to_string(),
                build: None,
                security_opts: Vec::new(),
            },
            nw_sandbox: None,
            fs_sandbox: None,
            proxy: Some(ExecutionProxyConfig {
                image: DEFAULT_PROXY_IMAGE.to_string(),
                build: None,
                agent_session_config: "agent.toml".to_string(),
                nw_sandbox_session_config: "nw-sandbox.toml".to_string(),
            }),
            mounts: Vec::<ResolvedMountConfig>::new(),
        }
    }

    #[test]
    fn accepts_a_valid_readable_session_tree_and_selection() {
        let config_dir = create_config_tree("valid");
        validate_baffle_session_files(&config_dir, &execution_config()).unwrap();
        fs::remove_dir_all(config_dir).unwrap();
    }

    #[cfg(unix)]
    #[test]
    fn trusts_root_and_the_real_invoking_uid_but_rejects_other_owners() {
        assert!(validate_baffle_session_owner(0, 1000).is_ok());
        assert!(validate_baffle_session_owner(1000, 1000).is_ok());
        assert!(validate_baffle_session_owner(2000, 1000).is_err());
    }

    #[cfg(unix)]
    #[test]
    fn rejects_symlinks_in_the_session_tree() {
        use std::os::unix::fs::symlink;

        let config_dir = create_config_tree("symlink");
        let outside = config_dir.join("outside.toml");
        fs::write(&outside, "version = 2\n").unwrap();
        symlink(&outside, config_dir.join("proxy/sessions/linked.toml")).unwrap();

        assert!(validate_baffle_config_layout(&config_dir).is_err());
        fs::remove_dir_all(config_dir).unwrap();
    }

    #[cfg(unix)]
    #[test]
    fn rejects_group_or_world_writable_session_paths() {
        use std::os::unix::fs::PermissionsExt as _;

        let config_dir = create_config_tree("unsafe-mode");
        let session = config_dir.join("proxy/sessions/agent.toml");
        fs::set_permissions(&session, fs::Permissions::from_mode(0o666)).unwrap();

        assert!(validate_baffle_session_files(&config_dir, &execution_config()).is_err());
        fs::remove_dir_all(config_dir).unwrap();
    }

    #[test]
    fn rejects_malformed_documents_unsupported_versions_and_wrong_socket_names() {
        let config_dir = create_config_tree("malformed");
        let session = config_dir.join("proxy/sessions/agent.toml");
        for contents in [
            "version = [\n",
            "version = 1\nsocket_name = 'agent/proxy.sock'\n",
            "version = 2\nsocket_name = 'nw-sandbox/proxy.sock'\n",
            "version = 2\nsocket_name = 3\n",
            "- 'not a document table'\n",
        ] {
            fs::write(&session, contents).unwrap();
            assert!(
                validate_baffle_session_files(&config_dir, &execution_config()).is_err(),
                "accepted malformed session: {contents}"
            );
        }
        fs::remove_dir_all(config_dir).unwrap();
    }
}
