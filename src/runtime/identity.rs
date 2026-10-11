use anyhow::Context as _;
use std::fs::{self, File, OpenOptions};
use std::io::{Read, Write as _};
use std::path::{Path, PathBuf};

const RUN_IDENTITY_FILE: &str = ".cladding-run-identity";
const RUN_IDENTITY_SIDECAR_SUFFIX: &str = ".cladding-run-identity";

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct RunIdentity {
    pub owner: String,
    pub id: String,
    root_file_id: Option<RootFileId>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct RootFileId {
    device: u64,
    inode: u64,
}

/// Holds the exclusive identity-file lock while a one-off run is active, or
/// while cleanup owns the run and its resources.
#[derive(Debug)]
pub struct RunIdentityGuard {
    _file: File,
}

pub fn create_run_identity_guard(
    root: &Path,
    owner: &str,
    id: &str,
) -> anyhow::Result<RunIdentityGuard> {
    let marker = root.join(RUN_IDENTITY_FILE);
    #[cfg(unix)]
    let mut file = {
        use std::os::unix::fs::OpenOptionsExt as _;
        OpenOptions::new()
            .read(true)
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(&marker)
    }
    .with_context(|| format!("failed to create run identity marker {}", marker.display()))?;
    #[cfg(not(unix))]
    let mut file = OpenOptions::new()
        .read(true)
        .write(true)
        .create_new(true)
        .open(&marker)
        .with_context(|| format!("failed to create run identity marker {}", marker.display()))?;

    file.try_lock()
        .with_context(|| format!("failed to lock run identity marker {}", marker.display()))?;
    let root_metadata = fs::symlink_metadata(root)
        .with_context(|| format!("failed to inspect private runtime root {}", root.display()))?;
    if root_metadata.file_type().is_symlink() || !root_metadata.is_dir() {
        anyhow::bail!(
            "private runtime root {} is not a regular directory",
            root.display()
        );
    }
    let root_file_id = root_file_id(&root_metadata);
    writeln!(file, "{owner}").context("failed to write run owner to identity marker")?;
    writeln!(file, "{id}").context("failed to write run identity to identity marker")?;
    match &root_file_id {
        Some(root_file_id) => {
            writeln!(file, "{}", root_file_id.device)
                .context("failed to write runtime root device to identity marker")?;
            writeln!(file, "{}", root_file_id.inode)
                .context("failed to write runtime root inode to identity marker")?;
        }
        None => {
            writeln!(file, "-")
                .context("failed to write runtime root device to identity marker")?;
            writeln!(file, "-").context("failed to write runtime root inode to identity marker")?;
        }
    }
    file.sync_all()
        .context("failed to sync run identity marker")?;
    File::open(root)
        .and_then(|directory| directory.sync_all())
        .with_context(|| format!("failed to sync private runtime root {}", root.display()))?;
    Ok(RunIdentityGuard { _file: file })
}

/// Compatibility helper for fixtures and callers that only need to write a
/// marker. A running `cladding run` must retain the guard returned by
/// `create_run_identity_guard` instead.
pub fn write_run_identity(root: &Path, owner: &str, id: &str) -> anyhow::Result<()> {
    drop(create_run_identity_guard(root, owner, id)?);
    Ok(())
}

pub(crate) fn try_lock_run_identity(
    root: &Path,
    owner: &str,
    id: &str,
) -> anyhow::Result<Option<RunIdentityGuard>> {
    let marker = identity_marker_to_lock(root)?;
    #[cfg(unix)]
    let file = {
        use std::os::unix::fs::OpenOptionsExt as _;
        OpenOptions::new()
            .read(true)
            .custom_flags(libc::O_NOFOLLOW)
            .open(&marker)
    }
    .with_context(|| format!("failed to open run identity marker {}", marker.display()))?;
    #[cfg(not(unix))]
    let file = File::open(&marker)
        .with_context(|| format!("failed to open run identity marker {}", marker.display()))?;

    validate_private_identity_file(&file, &marker)?;
    match file.try_lock() {
        Ok(()) => {}
        Err(std::fs::TryLockError::WouldBlock) => return Ok(None),
        Err(std::fs::TryLockError::Error(error)) => {
            return Err(error).with_context(|| {
                format!("failed to lock run identity marker {}", marker.display())
            });
        }
    }
    let identity = parse_identity(&mut &file, &marker)?;
    if identity.owner != owner || identity.id != id {
        anyhow::bail!(
            "run identity marker {} does not match the expected owner and identity",
            marker.display()
        );
    }
    Ok(Some(RunIdentityGuard { _file: file }))
}

pub(crate) fn read_run_identity(root: &Path) -> anyhow::Result<Option<RunIdentity>> {
    match fs::symlink_metadata(root) {
        Ok(metadata) => {
            if metadata.file_type().is_symlink() || !metadata.is_dir() {
                anyhow::bail!(
                    "private runtime root {} is not a regular directory",
                    root.display()
                );
            }
            validate_private_root(&metadata, root)?;
            let marker = root.join(RUN_IDENTITY_FILE);
            match read_identity_file(&marker) {
                Ok(identity) => {
                    verify_root_file_id(&identity, &metadata, root, true)?;
                    Ok(Some(identity))
                }
                Err(error)
                    if error
                        .downcast_ref::<std::io::Error>()
                        .is_some_and(|error| error.kind() == std::io::ErrorKind::NotFound) =>
                {
                    match read_sidecar_identity(root)? {
                        Some(identity) => {
                            verify_root_file_id(&identity, &metadata, root, false)?;
                            Ok(Some(identity))
                        }
                        None => Err(error),
                    }
                }
                Err(error) => Err(error),
            }
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => read_sidecar_identity(root),
        Err(error) => Err(error)
            .with_context(|| format!("failed to inspect private runtime root {}", root.display())),
    }
}

pub(crate) fn read_sidecar_identity(root: &Path) -> anyhow::Result<Option<RunIdentity>> {
    let sidecar = run_identity_sidecar_path(root)?;
    match read_identity_file(&sidecar) {
        Ok(identity) => Ok(Some(identity)),
        Err(error)
            if error
                .downcast_ref::<std::io::Error>()
                .is_some_and(|error| error.kind() == std::io::ErrorKind::NotFound) =>
        {
            Ok(None)
        }
        Err(error) => Err(error),
    }
}

pub(crate) fn run_identity_sidecar_path(root: &Path) -> anyhow::Result<PathBuf> {
    let owner = root
        .file_name()
        .and_then(|name| name.to_str())
        .filter(|owner| owner.starts_with("cladding-run-"))
        .ok_or_else(|| anyhow::anyhow!("private runtime root has no valid run owner name"))?;
    let parent = root
        .parent()
        .ok_or_else(|| anyhow::anyhow!("private runtime root has no parent directory"))?;
    Ok(parent.join(format!(".{owner}{RUN_IDENTITY_SIDECAR_SUFFIX}")))
}

/// Keep ownership evidence outside the directory before recursively removing
/// it. If removal stops partway through, the hard link remains available for a
/// later cleanup attempt and shares the same lock inode.
pub(crate) fn preserve_run_identity(root: &Path, owner: &str, id: &str) -> anyhow::Result<PathBuf> {
    let source = root.join(RUN_IDENTITY_FILE);
    let sidecar = run_identity_sidecar_path(root)?;
    verify_run_identity(root, owner, id)?;
    match fs::symlink_metadata(&sidecar) {
        Ok(metadata) => {
            if metadata.file_type().is_symlink() || !metadata.is_file() {
                anyhow::bail!(
                    "run identity sidecar {} is not a regular file",
                    sidecar.display()
                );
            }
            let identity = read_identity_file(&sidecar)?;
            if identity.owner != owner || identity.id != id {
                anyhow::bail!(
                    "run identity sidecar {} does not match the expected owner and identity",
                    sidecar.display()
                );
            }
            if source.exists() && !same_file(&source, &sidecar)? {
                anyhow::bail!(
                    "run identity sidecar {} does not match its marker",
                    sidecar.display()
                );
            }
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            fs::hard_link(&source, &sidecar).with_context(|| {
                format!("failed to preserve run identity at {}", sidecar.display())
            })?;
        }
        Err(error) => {
            return Err(error).with_context(|| {
                format!(
                    "failed to inspect run identity sidecar {}",
                    sidecar.display()
                )
            });
        }
    }
    let identity = read_identity_file(&sidecar)?;
    if identity.owner != owner || identity.id != id {
        anyhow::bail!(
            "run identity sidecar {} does not match the expected owner and identity",
            sidecar.display()
        );
    }
    File::open(sidecar.parent().expect("sidecar has a parent"))
        .and_then(|directory| directory.sync_all())
        .with_context(|| format!("failed to sync run identity sidecar {}", sidecar.display()))?;
    Ok(sidecar)
}

pub(crate) fn remove_run_identity_sidecar(
    root: &Path,
    owner: &str,
    id: &str,
) -> anyhow::Result<()> {
    let sidecar = run_identity_sidecar_path(root)?;
    match fs::symlink_metadata(root) {
        Ok(_) => verify_run_identity(root, owner, id)?,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
        Err(error) => {
            return Err(error).with_context(|| {
                format!("failed to inspect private runtime root {}", root.display())
            });
        }
    }
    let Some(identity) = read_sidecar_identity(root)? else {
        return Ok(());
    };
    if identity.owner != owner || identity.id != id {
        anyhow::bail!(
            "run identity sidecar {} does not match the expected owner and identity",
            sidecar.display()
        );
    }
    fs::remove_file(&sidecar).with_context(|| {
        format!(
            "failed to remove run identity sidecar {}",
            sidecar.display()
        )
    })?;
    File::open(sidecar.parent().expect("sidecar has a parent"))
        .and_then(|directory| directory.sync_all())
        .with_context(|| format!("failed to sync temporary base {}", sidecar.display()))?;
    Ok(())
}

fn verify_run_identity(root: &Path, owner: &str, id: &str) -> anyhow::Result<()> {
    match read_run_identity(root)? {
        Some(identity) if identity.owner == owner && identity.id == id => Ok(()),
        Some(_) => anyhow::bail!(
            "private runtime root {} does not match the expected run identity",
            root.display()
        ),
        None => anyhow::bail!(
            "private runtime root {} has no run identity marker",
            root.display()
        ),
    }
}

fn identity_marker_to_lock(root: &Path) -> anyhow::Result<PathBuf> {
    let marker = root.join(RUN_IDENTITY_FILE);
    match fs::symlink_metadata(&marker) {
        Ok(_) => Ok(marker),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            run_identity_sidecar_path(root)
        }
        Err(error) => Err(error)
            .with_context(|| format!("failed to inspect run identity marker {}", marker.display())),
    }
}

fn read_identity_file(marker: &Path) -> anyhow::Result<RunIdentity> {
    let mut file = {
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt as _;
            OpenOptions::new()
                .read(true)
                .custom_flags(libc::O_NOFOLLOW)
                .open(marker)
        }
        #[cfg(not(unix))]
        {
            File::open(marker)
        }
    }
    .with_context(|| format!("failed to open run identity marker {}", marker.display()))?;
    validate_private_identity_file(&file, marker)?;
    parse_identity(&mut file, marker)
}

fn validate_private_identity_file(file: &File, marker: &Path) -> anyhow::Result<()> {
    let metadata = fs::symlink_metadata(marker)
        .with_context(|| format!("failed to inspect run identity marker {}", marker.display()))?;
    if metadata.file_type().is_symlink() || !metadata.is_file() {
        anyhow::bail!(
            "run identity marker {} is not a regular file",
            marker.display()
        );
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt as _;
        let expected_uid = unsafe { libc::geteuid() };
        if metadata.uid() != expected_uid || metadata.mode() & 0o077 != 0 {
            anyhow::bail!(
                "run identity marker {} is not private to this user",
                marker.display()
            );
        }
        let opened = file.metadata()?;
        if opened.dev() != metadata.dev() || opened.ino() != metadata.ino() {
            anyhow::bail!(
                "run identity marker {} changed while being opened",
                marker.display()
            );
        }
    }
    Ok(())
}

fn validate_private_root(metadata: &fs::Metadata, root: &Path) -> anyhow::Result<()> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt as _;
        let expected_uid = unsafe { libc::geteuid() };
        if metadata.uid() != expected_uid || metadata.mode() & 0o077 != 0 {
            anyhow::bail!(
                "private runtime root {} is not private to this user",
                root.display()
            );
        }
    }
    Ok(())
}

fn same_file(left: &Path, right: &Path) -> anyhow::Result<bool> {
    let left =
        fs::metadata(left).with_context(|| format!("failed to inspect {}", left.display()))?;
    let right =
        fs::metadata(right).with_context(|| format!("failed to inspect {}", right.display()))?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt as _;
        Ok(left.dev() == right.dev() && left.ino() == right.ino())
    }
    #[cfg(not(unix))]
    {
        Ok(left.len() == right.len())
    }
}

fn parse_identity(file: &mut impl Read, marker: &Path) -> anyhow::Result<RunIdentity> {
    let mut contents = String::new();
    file.read_to_string(&mut contents)
        .with_context(|| format!("failed to read run identity marker {}", marker.display()))?;
    let mut lines = contents.lines();
    let owner = lines.next().unwrap_or_default();
    let id = lines.next().unwrap_or_default();
    let root_device = lines.next();
    let root_inode = lines.next();
    if owner.is_empty() || id.is_empty() || lines.next().is_some() {
        anyhow::bail!(
            "run identity marker {} has invalid contents",
            marker.display()
        );
    }
    let root_file_id = match (root_device, root_inode) {
        (None, None) => None,
        (Some("-"), Some("-")) => None,
        (Some(device), Some(inode)) => Some(RootFileId {
            device: device.parse().with_context(|| {
                format!(
                    "run identity marker {} has an invalid root device",
                    marker.display()
                )
            })?,
            inode: inode.parse().with_context(|| {
                format!(
                    "run identity marker {} has an invalid root inode",
                    marker.display()
                )
            })?,
        }),
        _ => {
            anyhow::bail!(
                "run identity marker {} has invalid contents",
                marker.display()
            );
        }
    };
    Ok(RunIdentity {
        owner: owner.to_string(),
        id: id.to_string(),
        root_file_id,
    })
}

fn verify_root_file_id(
    identity: &RunIdentity,
    metadata: &fs::Metadata,
    root: &Path,
    marker_in_root: bool,
) -> anyhow::Result<()> {
    match (&identity.root_file_id, root_file_id(metadata)) {
        (Some(expected), Some(actual)) if expected == &actual => Ok(()),
        (Some(_), Some(_)) => anyhow::bail!(
            "private runtime root {} does not match its durable run identity",
            root.display()
        ),
        (None, _) if marker_in_root => Ok(()),
        (None, _) => anyhow::bail!(
            "private runtime root {} has no verifiable directory identity in its retry marker",
            root.display()
        ),
        (Some(_), None) => anyhow::bail!(
            "private runtime root {} cannot be verified on this platform",
            root.display()
        ),
    }
}

fn root_file_id(metadata: &fs::Metadata) -> Option<RootFileId> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt as _;
        Some(RootFileId {
            device: metadata.dev(),
            inode: metadata.ino(),
        })
    }
    #[cfg(not(unix))]
    {
        let _ = metadata;
        None
    }
}
