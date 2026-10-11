use anyhow::Context as _;
use std::fs::{self, File, OpenOptions};
use std::io::{Read as _, Write as _};
use std::path::Path;

const RUN_IDENTITY_FILE: &str = ".cladding-run-identity";

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct RunIdentity {
    pub owner: String,
    pub id: String,
}

pub fn write_run_identity(root: &Path, owner: &str, id: &str) -> anyhow::Result<()> {
    let marker = root.join(RUN_IDENTITY_FILE);
    #[cfg(unix)]
    let mut file = {
        use std::os::unix::fs::OpenOptionsExt as _;
        OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(&marker)
    }
    .with_context(|| format!("failed to create run identity marker {}", marker.display()))?;
    #[cfg(not(unix))]
    let mut file = OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&marker)
        .with_context(|| format!("failed to create run identity marker {}", marker.display()))?;

    writeln!(file, "{owner}").context("failed to write run owner to identity marker")?;
    writeln!(file, "{id}").context("failed to write run identity to identity marker")?;
    file.sync_all()
        .context("failed to sync run identity marker")?;
    File::open(root)
        .and_then(|directory| directory.sync_all())
        .with_context(|| format!("failed to sync private runtime root {}", root.display()))?;
    Ok(())
}

pub(crate) fn read_run_identity(root: &Path) -> anyhow::Result<Option<RunIdentity>> {
    let root_metadata = match fs::symlink_metadata(root) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(error) => {
            return Err(error).with_context(|| {
                format!("failed to inspect private runtime root {}", root.display())
            });
        }
    };
    if root_metadata.file_type().is_symlink() || !root_metadata.is_dir() {
        anyhow::bail!(
            "private runtime root {} is not a regular directory",
            root.display()
        );
    }

    let marker = root.join(RUN_IDENTITY_FILE);
    let marker_metadata = match fs::symlink_metadata(&marker) {
        Ok(metadata) => metadata,
        Err(error) => {
            return Err(error).with_context(|| {
                format!("failed to inspect run identity marker {}", marker.display())
            });
        }
    };
    if marker_metadata.file_type().is_symlink() || !marker_metadata.is_file() {
        anyhow::bail!(
            "run identity marker {} is not a regular file",
            marker.display()
        );
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::{MetadataExt as _, OpenOptionsExt as _};
        let expected_uid = unsafe { libc::geteuid() };
        if root_metadata.uid() != expected_uid
            || root_metadata.mode() & 0o077 != 0
            || marker_metadata.uid() != expected_uid
            || marker_metadata.mode() & 0o077 != 0
        {
            anyhow::bail!(
                "run identity marker {} is not private to this user",
                marker.display()
            );
        }

        // Reopen without following a replacement symlink after the metadata checks.
        let mut file = OpenOptions::new()
            .read(true)
            .custom_flags(libc::O_NOFOLLOW)
            .open(&marker)
            .with_context(|| format!("failed to open run identity marker {}", marker.display()))?;
        parse_identity(&mut file, &marker).map(Some)
    }
    #[cfg(not(unix))]
    {
        let mut file = File::open(&marker)
            .with_context(|| format!("failed to open run identity marker {}", marker.display()))?;
        parse_identity(&mut file, &marker).map(Some)
    }
}

fn parse_identity(file: &mut File, marker: &Path) -> anyhow::Result<RunIdentity> {
    let mut contents = String::new();
    file.read_to_string(&mut contents)
        .with_context(|| format!("failed to read run identity marker {}", marker.display()))?;
    let mut lines = contents.lines();
    let owner = lines.next().unwrap_or_default();
    let id = lines.next().unwrap_or_default();
    if owner.is_empty() || id.is_empty() || lines.next().is_some() {
        anyhow::bail!(
            "run identity marker {} has invalid contents",
            marker.display()
        );
    }
    Ok(RunIdentity {
        owner: owner.to_string(),
        id: id.to_string(),
    })
}
