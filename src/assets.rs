use crate::error::Result;
use crate::fs_utils::set_permissions;
use anyhow::Context as _;
use include_dir::{Dir, include_dir};
use std::fs;
use std::io::Write as _;
use std::path::Path;

const CONTAINERFILE_CLADDING: &str = include_str!("../Containerfile.cladding");

static CONFIG_DIR: Dir<'_> = include_dir!("$CARGO_MANIFEST_DIR/config-template");

static MCP_RUN_BIN: &[u8] = include_bytes!(concat!(env!("OUT_DIR"), "/mcp-run"));
static RUN_REMOTE_BIN: &[u8] = include_bytes!(concat!(env!("OUT_DIR"), "/run-remote"));

const RUN_IN_NW_SANDBOX: &[u8] = b"#!/bin/sh\nset -eu\n: \"${RUN_NW_SANDBOX_SOCKET:?RUN_NW_SANDBOX_SOCKET must be set}\"\nRUN_REMOTE_SOCKET=\"$RUN_NW_SANDBOX_SOCKET\" exec \"$(dirname \"$0\")/run-remote\" \"$@\"\n";
const RUN_IN_FS_SANDBOX: &[u8] = b"#!/bin/sh\nset -eu\n: \"${RUN_FS_SANDBOX_SOCKET:?RUN_FS_SANDBOX_SOCKET must be set}\"\nRUN_REMOTE_SOCKET=\"$RUN_FS_SANDBOX_SOCKET\" exec \"$(dirname \"$0\")/run-remote\" \"$@\"\n";
pub fn config_top_level_entries() -> Vec<String> {
    let mut names = std::collections::BTreeSet::new();
    for entry in CONFIG_DIR.dirs() {
        if let Some(component) = entry.path().components().next()
            && let std::path::Component::Normal(name) = component
            && let Some(name) = name.to_str()
        {
            names.insert(name.to_string());
        }
    }
    for entry in CONFIG_DIR.files() {
        if let Some(component) = entry.path().components().next()
            && let std::path::Component::Normal(name) = component
            && let Some(name) = name.to_str()
        {
            names.insert(name.to_string());
        }
    }
    names.into_iter().collect()
}

pub fn materialize_config(base_dir: &Path) -> Result<()> {
    materialize_config_dir(base_dir, &CONFIG_DIR)?;
    validate_baffle_config_layout(base_dir)?;
    Ok(())
}

pub fn tool_files() -> Vec<(&'static str, &'static [u8])> {
    vec![
        ("mcp-run", MCP_RUN_BIN),
        ("run-remote", RUN_REMOTE_BIN),
        ("run-in-nw-sandbox", RUN_IN_NW_SANDBOX),
        ("run-in-fs-sandbox", RUN_IN_FS_SANDBOX),
    ]
}

pub fn write_embedded_tools(bin_dir: &Path) -> Result<()> {
    for (name, contents) in tool_files() {
        let path = bin_dir.join(name);
        fs::write(&path, contents)
            .with_context(|| format!("failed to write {}", path.display()))?;
        set_permissions(&path, 0o755)?;
    }

    Ok(())
}

fn materialize_config_dir(base_dir: &Path, dir: &Dir<'_>) -> anyhow::Result<()> {
    if !base_dir.exists() && !crate::fs_utils::path_is_symlink(base_dir) {
        fs::create_dir_all(base_dir)
            .with_context(|| format!("failed to create {}", base_dir.display()))?;
    }
    ensure_config_directory(base_dir)?;

    for subdir in dir.dirs() {
        ensure_config_directory_tree(base_dir, subdir.path())?;
        materialize_config_dir(base_dir, subdir)?;
    }

    for entry in dir.files() {
        let rel_path = entry.path();
        if let Some(parent) = rel_path.parent() {
            ensure_config_directory_tree(base_dir, parent)?;
        }
        let target = base_dir.join(rel_path);
        match fs::symlink_metadata(&target) {
            Ok(metadata) if metadata.file_type().is_symlink() => {
                anyhow::bail!(
                    "refusing to materialize configuration through symbolic link {}",
                    target.display()
                );
            }
            Ok(metadata) if metadata.is_file() => {
                set_permissions(&target, 0o644)?;
                continue;
            }
            Ok(_) => {
                anyhow::bail!(
                    "configuration path is not a regular file: {}",
                    target.display()
                );
            }
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => {}
            Err(err) => {
                return Err(err)
                    .with_context(|| format!("failed to inspect {}", target.display()))?;
            }
        }

        let mut output = fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&target)
            .with_context(|| format!("failed to create {}", target.display()))?;
        output
            .write_all(entry.contents())
            .with_context(|| format!("failed to write {}", target.display()))?;
        set_permissions(&target, 0o644)?;
    }

    Ok(())
}

fn ensure_config_directory_tree(base_dir: &Path, relative: &Path) -> anyhow::Result<()> {
    let mut current = base_dir.to_path_buf();
    for component in relative.components() {
        let std::path::Component::Normal(name) = component else {
            anyhow::bail!(
                "invalid embedded configuration directory {}",
                relative.display()
            );
        };
        current.push(name);
        ensure_config_directory(&current)?;
    }
    Ok(())
}

fn ensure_config_directory(path: &Path) -> anyhow::Result<()> {
    match fs::symlink_metadata(path) {
        Ok(metadata) if metadata.file_type().is_symlink() => {
            anyhow::bail!(
                "refusing to materialize configuration through symbolic link {}",
                path.display()
            );
        }
        Ok(metadata) if metadata.is_dir() => {}
        Ok(_) => anyhow::bail!("configuration path is not a directory: {}", path.display()),
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => {
            fs::create_dir(path).with_context(|| format!("failed to create {}", path.display()))?;
        }
        Err(err) => {
            return Err(err).with_context(|| format!("failed to inspect {}", path.display()))?;
        }
    }
    set_permissions(path, 0o755)?;
    Ok(())
}

fn validate_baffle_config_layout(config_dir: &Path) -> anyhow::Result<()> {
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
                "Baffle file-only configuration cannot contain symbolic links: {}",
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
                "Baffle file-only configuration requires regular files and directories: {}",
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
            "Baffle file-only configuration cannot contain symbolic links: {}",
            path.display()
        );
    }
    if directory && !metadata.is_dir() || !directory && !metadata.is_file() {
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

    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt as _;
        use std::os::unix::fs::PermissionsExt as _;
        let mode = metadata.permissions().mode() & 0o777;
        if mode & 0o022 != 0 {
            anyhow::bail!(
                "Baffle file-only configuration cannot be group- or world-writable: {}",
                path.display()
            );
        }
        let owner = unsafe { libc::geteuid() };
        if metadata.uid() != owner {
            anyhow::bail!(
                "Baffle file-only configuration must be owned by the user running cladding: {}",
                path.display()
            );
        }
    }
    Ok(())
}

pub fn containerfile() -> &'static str {
    CONTAINERFILE_CLADDING
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;
    use std::sync::atomic::{AtomicU64, Ordering};

    static TEMP_DIR_COUNTER: AtomicU64 = AtomicU64::new(0);

    #[test]
    fn materialized_baffle_configuration_parses_with_required_defaults() {
        let temp = create_temp_dir("baffle-config");
        materialize_config(&temp).expect("materialize configuration");

        let daemon: toml::Value = toml::from_str(
            &fs::read_to_string(temp.join("proxy/daemon.toml")).expect("read daemon config"),
        )
        .expect("parse daemon TOML");
        assert_eq!(daemon["daemon"]["create_mode"].as_str(), Some("file_only"));
        assert_eq!(
            daemon["daemon"]["session_config_dir"].as_str(),
            Some("/opt/config/proxy/sessions")
        );
        assert_eq!(
            daemon["daemon"]["control_socket"].as_str(),
            Some("/run/baffle/control.sock")
        );
        assert_eq!(
            daemon["daemon"]["socket_dir"].as_str(),
            Some("/run/cladding/proxy")
        );
        assert_eq!(
            daemon["ca"]["certificate"].as_str(),
            Some("/opt/credentials/baffle/ca.crt")
        );
        assert_eq!(
            daemon["ca"]["private_key"].as_str(),
            Some("/opt/credentials/baffle/ca-key.pem")
        );
        assert_eq!(
            daemon["secrets"]["directory"].as_str(),
            Some("/opt/credentials/baffle/secrets")
        );
        assert_eq!(
            daemon["secrets"]["allowed"].as_array().map(Vec::len),
            Some(0)
        );

        assert_session(&temp.join("proxy/sessions/agent.toml"), "agent/proxy.sock");
        assert_session(
            &temp.join("proxy/sessions/nw-sandbox.toml"),
            "nw-sandbox/proxy.sock",
        );
        fs::remove_dir_all(temp).expect("remove temporary configuration");
    }

    #[test]
    fn init_preserves_user_edited_baffle_toml() {
        let temp = create_temp_dir("baffle-edits");
        materialize_config(&temp).expect("materialize configuration");
        let daemon_path = temp.join("proxy/daemon.toml");
        let edited = "# user configuration\n[secrets]\nallowed = [\"model-api\"]\n";
        fs::write(&daemon_path, edited).expect("edit daemon config");

        materialize_config(&temp).expect("materialize configuration again");

        assert_eq!(fs::read_to_string(daemon_path).unwrap(), edited);
        fs::remove_dir_all(temp).expect("remove temporary configuration");
    }

    #[cfg(unix)]
    #[test]
    fn init_rejects_baffle_configuration_symlinks() {
        use std::os::unix::fs::symlink;

        let temp = create_temp_dir("baffle-symlink");
        materialize_config(&temp).expect("materialize configuration");
        let daemon_path = temp.join("proxy/daemon.toml");
        let outside = temp.join("outside.toml");
        fs::write(&outside, "untouched").expect("write outside file");
        fs::remove_file(&daemon_path).expect("remove daemon config");
        symlink(&outside, &daemon_path).expect("create daemon symlink");

        assert!(materialize_config(&temp).is_err());
        assert_eq!(fs::read_to_string(outside).unwrap(), "untouched");
        fs::remove_dir_all(temp).expect("remove temporary configuration");
    }

    #[cfg(unix)]
    #[test]
    fn init_rejects_symlinks_in_the_session_config_directory() {
        use std::os::unix::fs::symlink;

        let temp = create_temp_dir("baffle-session-symlink");
        materialize_config(&temp).expect("materialize configuration");
        let outside = temp.join("outside.toml");
        fs::write(&outside, "untouched").expect("write outside file");
        symlink(&outside, temp.join("proxy/sessions/custom.toml")).expect("create session symlink");

        assert!(materialize_config(&temp).is_err());
        assert_eq!(fs::read_to_string(outside).unwrap(), "untouched");
        fs::remove_dir_all(temp).expect("remove temporary configuration");
    }

    #[cfg(unix)]
    #[test]
    fn baffle_configuration_uses_readable_non_writable_modes() {
        use std::os::unix::fs::PermissionsExt;

        let temp = create_temp_dir("baffle-modes");
        materialize_config(&temp).expect("materialize configuration");
        let daemon_dir = fs::metadata(temp.join("proxy")).unwrap();
        let sessions_dir = fs::metadata(temp.join("proxy/sessions")).unwrap();
        let daemon_file = fs::metadata(temp.join("proxy/daemon.toml")).unwrap();
        let session_file = fs::metadata(temp.join("proxy/sessions/agent.toml")).unwrap();
        assert_eq!(daemon_dir.permissions().mode() & 0o777, 0o755);
        assert_eq!(sessions_dir.permissions().mode() & 0o777, 0o755);
        assert_eq!(daemon_file.permissions().mode() & 0o777, 0o644);
        assert_eq!(session_file.permissions().mode() & 0o777, 0o644);
        use std::os::unix::fs::MetadataExt as _;
        let current_uid = unsafe { libc::geteuid() };
        assert_eq!(daemon_dir.uid(), current_uid);
        assert_eq!(sessions_dir.uid(), current_uid);
        assert_eq!(daemon_file.uid(), current_uid);
        assert_eq!(session_file.uid(), current_uid);
        fs::remove_dir_all(temp).expect("remove temporary configuration");
    }

    fn assert_session(path: &Path, socket_name: &str) {
        let session: toml::Value =
            toml::from_str(&fs::read_to_string(path).expect("read session config"))
                .expect("parse session TOML");
        assert_eq!(session["version"].as_integer(), Some(1));
        assert_eq!(session["operation"].as_str(), Some("create"));
        assert_eq!(session["session"]["persistent"].as_bool(), Some(true));
        assert_eq!(
            session["session"]["socket_name"].as_str(),
            Some(socket_name)
        );
        let rules = session["rules"].as_array().expect("session rules");
        assert_eq!(rules.len(), 1);
        assert_eq!(rules[0]["host"].as_str(), Some("example.com"));
        assert_eq!(rules[0]["mode"].as_str(), Some("tunnel"));
        assert_eq!(rules[0]["ports"][0].as_integer(), Some(443));
    }

    fn create_temp_dir(name: &str) -> PathBuf {
        let unique = TEMP_DIR_COUNTER.fetch_add(1, Ordering::Relaxed);
        let path = std::env::temp_dir().join(format!(
            "cladding-assets-{name}-{}-{unique}",
            std::process::id()
        ));
        if path.exists() {
            fs::remove_dir_all(&path).expect("remove stale temporary directory");
        }
        fs::create_dir_all(&path).expect("create temporary directory");
        path
    }
}
