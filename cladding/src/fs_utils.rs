use crate::error::{Error, Result};
use anyhow::Context as _;
use std::fs;
use std::path::{Path, PathBuf};

#[cfg(unix)]
use std::os::unix::fs::PermissionsExt;

pub fn is_broken_symlink(path: &Path) -> Result<bool> {
    let meta = fs::symlink_metadata(path)
        .with_context(|| format!("failed to stat {}", path.display()))?;
    if meta.file_type().is_symlink() {
        return Ok(fs::metadata(path).is_err());
    }
    Ok(false)
}

pub fn is_executable(path: &Path) -> bool {
    if let Ok(meta) = fs::metadata(path) {
        #[cfg(unix)]
        {
            return meta.permissions().mode() & 0o111 != 0;
        }
        #[cfg(not(unix))]
        {
            return meta.is_file();
        }
    }
    false
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
    #[cfg(unix)]
    {
        let perm = fs::Permissions::from_mode(mode);
        fs::set_permissions(path, perm)
            .with_context(|| format!("failed to set permissions on {}", path.display()))?;
    }
    Ok(())
}
