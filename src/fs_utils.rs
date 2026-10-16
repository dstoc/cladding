use crate::error::{Error, Result};
use anyhow::Context as _;
use std::ffi::OsStr;
use std::fs;
use std::path::{Path, PathBuf};

use std::os::unix::fs::PermissionsExt;

pub fn is_broken_symlink(path: &Path) -> Result<bool> {
    let meta =
        fs::symlink_metadata(path).with_context(|| format!("failed to stat {}", path.display()))?;
    if meta.file_type().is_symlink() {
        return Ok(fs::metadata(path).is_err());
    }
    Ok(false)
}

pub fn is_executable(path: &Path) -> bool {
    if let Ok(meta) = fs::metadata(path) {
        return meta.permissions().mode() & 0o111 != 0;
    }
    false
}

pub fn command_exists_in_path(command: &str, paths: Option<&OsStr>) -> bool {
    paths.is_some_and(|paths| {
        std::env::split_paths(paths).any(|path| is_executable(&path.join(command)))
    })
}

pub fn path_is_symlink(path: &Path) -> bool {
    fs::symlink_metadata(path)
        .map(|meta| meta.file_type().is_symlink())
        .unwrap_or(false)
}

pub fn canonicalize_path(path: &Path) -> Result<PathBuf> {
    fs::canonicalize(path)
        .with_context(|| format!("failed to resolve {}", path.display()))
        .map_err(Error::from)
}

pub fn set_permissions(path: &Path, mode: u32) -> Result<()> {
    let perm = fs::Permissions::from_mode(mode);
    fs::set_permissions(path, perm)
        .with_context(|| format!("failed to set permissions on {}", path.display()))?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::command_exists_in_path;
    use std::fs;

    #[test]
    fn command_exists_in_path_detects_executable_entries() {
        let temp =
            std::env::temp_dir().join(format!("cladding-command-exists-{}", std::process::id()));
        if temp.exists() {
            fs::remove_dir_all(&temp).expect("cleanup temp dir");
        }
        fs::create_dir_all(&temp).expect("create temp dir");

        let command_path = temp.join("runsc");
        fs::write(&command_path, b"#!/bin/sh\nexit 0\n").expect("write command");
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mut permissions = fs::metadata(&command_path).expect("metadata").permissions();
            permissions.set_mode(0o755);
            fs::set_permissions(&command_path, permissions).expect("chmod");
        }

        assert!(command_exists_in_path("runsc", Some(temp.as_os_str())));
        assert!(!command_exists_in_path("podman", Some(temp.as_os_str())));

        fs::remove_dir_all(temp).expect("cleanup temp dir");
    }
}
