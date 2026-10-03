use crate::error::{Error, Result};
use anyhow::{Context as _, bail};
use rcgen::KeyPair;
use std::fs::{self, File, OpenOptions};
use std::io::Write as _;
use std::path::{Path, PathBuf};
use time::OffsetDateTime;
use x509_parser::pem::parse_x509_pem;

const CREDENTIALS_DIR: &str = "credentials";
const BAFFLE_DIR: &str = "baffle";
const SECRETS_DIR: &str = "secrets";
const CERTIFICATE_FILE: &str = "ca.crt";
const PRIVATE_KEY_FILE: &str = "ca-key.pem";
const CA_INIT_PENDING_FILE: &str = ".ca-init-pending";

/// Ensure the project has a private Baffle credentials directory.
///
/// Existing CA material is validated and never replaced. Only a newly created
/// credentials directory is allowed to defer CA creation until runtime startup.
pub fn ensure_baffle_credentials(project_root: &Path) -> Result<()> {
    ensure_baffle_credentials_inner(project_root, None, false).map_err(Error::from)
}

/// Ensure valid CA material exists, using the supplied Baffle initializer only
/// for the pending first bootstrap when neither configured CA file exists.
pub fn ensure_baffle_ca(
    project_root: &Path,
    mut initialize_ca: impl FnMut() -> anyhow::Result<()>,
) -> Result<()> {
    ensure_baffle_credentials_inner(project_root, Some(&mut initialize_ca), false)
        .map_err(Error::from)
}

/// Validate the persistent Baffle CA without bootstrapping it.
pub fn validate_baffle_ca(project_root: &Path) -> Result<()> {
    ensure_baffle_credentials_inner(project_root, None, true).map_err(Error::from)
}

fn ensure_baffle_credentials_inner(
    project_root: &Path,
    initialize_ca: Option<&mut dyn FnMut() -> anyhow::Result<()>>,
    require_ca: bool,
) -> anyhow::Result<()> {
    let credentials_root = project_root.join(CREDENTIALS_DIR);
    if require_ca && let Err(error) = fs::symlink_metadata(&credentials_root) {
        if error.kind() == std::io::ErrorKind::NotFound {
            bail!("Baffle CA is not initialized; run `cladding build` to initialize it");
        }
        return Err(error).with_context(|| {
            format!(
                "failed to inspect Baffle credentials directory {}",
                credentials_root.display()
            )
        });
    }
    ensure_private_directory(&credentials_root)?;
    let _lock = acquire_credentials_lock(&credentials_root)?;
    remove_stale_staging_directories(&credentials_root)?;

    let baffle_dir = credentials_root.join(BAFFLE_DIR);
    match fs::symlink_metadata(&baffle_dir) {
        Ok(metadata) => {
            if metadata.file_type().is_symlink() || !metadata.is_dir() {
                bail!(
                    "Baffle credentials path is not a directory: {}",
                    baffle_dir.display()
                );
            }
            require_current_owner(&baffle_dir)?;
            set_mode(&baffle_dir, 0o700)?;
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            if require_ca {
                bail!("Baffle CA is not initialized; run `cladding build` to initialize it");
            }
            create_credentials_atomically(&credentials_root, &baffle_dir)?;
        }
        Err(error) => {
            return Err(error).with_context(|| {
                format!(
                    "failed to inspect Baffle credentials path {}",
                    baffle_dir.display()
                )
            });
        }
    }

    let ca_exists = path_exists_without_following(&baffle_dir.join(CERTIFICATE_FILE))?
        || path_exists_without_following(&baffle_dir.join(PRIVATE_KEY_FILE))?;
    let ca_init_pending =
        !ca_exists && path_exists_without_following(&baffle_dir.join(CA_INIT_PENDING_FILE))?;
    if ca_exists {
        validate_existing_ca(&baffle_dir)?;
        remove_ca_init_pending_marker(&baffle_dir)?;
    } else if ca_init_pending {
        validate_ca_init_pending_marker(&baffle_dir)?;
        if initialize_ca.is_none() && require_ca {
            bail!("Baffle CA is not initialized; run `cladding build` to initialize it");
        }
    } else {
        validate_existing_ca(&baffle_dir)?;
    }

    let secrets_dir = ensure_secrets_directory(&baffle_dir)?;
    secure_secret_files(&secrets_dir)?;

    if ca_init_pending && let Some(initialize_ca) = initialize_ca {
        initialize_ca()?;
        validate_existing_ca(&baffle_dir)?;
        remove_ca_init_pending_marker(&baffle_dir)?;
    }

    Ok(())
}

fn ensure_private_directory(path: &Path) -> anyhow::Result<()> {
    match fs::symlink_metadata(path) {
        Ok(metadata) => {
            if metadata.file_type().is_symlink() || !metadata.is_dir() {
                bail!("credentials path is not a directory: {}", path.display());
            }
            require_current_owner(path)?;
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            match create_directory(path, 0o700) {
                Ok(()) => {}
                Err(error)
                    if error
                        .downcast_ref::<std::io::Error>()
                        .is_some_and(|error| error.kind() == std::io::ErrorKind::AlreadyExists) => {
                }
                Err(error) => return Err(error),
            }
        }
        Err(error) => {
            return Err(error).with_context(|| {
                format!("failed to inspect credentials directory {}", path.display())
            });
        }
    }
    set_mode(path, 0o700)
}

fn ensure_secrets_directory(baffle_dir: &Path) -> anyhow::Result<PathBuf> {
    let path = baffle_dir.join(SECRETS_DIR);
    match fs::symlink_metadata(&path) {
        Ok(metadata) => {
            if metadata.file_type().is_symlink() || !metadata.is_dir() {
                bail!("Baffle secrets path is not a directory: {}", path.display());
            }
            require_current_owner(&path)?;
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            create_directory(&path, 0o700)?;
        }
        Err(error) => {
            return Err(error).with_context(|| {
                format!(
                    "failed to inspect Baffle secrets directory {}",
                    path.display()
                )
            });
        }
    }
    set_mode(&path, 0o700)?;
    Ok(path)
}

fn secure_secret_files(secrets_dir: &Path) -> anyhow::Result<()> {
    for entry in fs::read_dir(secrets_dir).with_context(|| {
        format!(
            "failed to read Baffle secrets directory {}",
            secrets_dir.display()
        )
    })? {
        let entry = entry.with_context(|| {
            format!(
                "failed to inspect an entry in Baffle secrets directory {}",
                secrets_dir.display()
            )
        })?;
        let path = entry.path();
        let metadata = fs::symlink_metadata(&path)
            .with_context(|| format!("failed to inspect Baffle secret file {}", path.display()))?;
        if metadata.file_type().is_symlink() || !metadata.is_file() {
            bail!(
                "Baffle secret entry must be a regular file: {}",
                path.display()
            );
        }
        require_current_owner(&path)?;
        set_mode(&path, 0o600)?;
    }
    Ok(())
}

fn validate_existing_ca(baffle_dir: &Path) -> anyhow::Result<()> {
    let certificate_path = baffle_dir.join(CERTIFICATE_FILE);
    let private_key_path = baffle_dir.join(PRIVATE_KEY_FILE);
    let certificate_exists = path_exists_without_following(&certificate_path)?;
    let private_key_exists = path_exists_without_following(&private_key_path)?;
    if !certificate_exists || !private_key_exists {
        bail!(
            "incomplete Baffle CA in {}: both {CERTIFICATE_FILE} and {PRIVATE_KEY_FILE} must exist; Cladding will not replace partial material",
            baffle_dir.display()
        );
    }

    require_regular_file(&certificate_path)?;
    require_regular_file(&private_key_path)?;
    require_current_owner(&certificate_path)?;
    require_current_owner(&private_key_path)?;
    set_mode(&certificate_path, 0o644)?;
    set_mode(&private_key_path, 0o600)?;

    let certificate_pem = fs::read(&certificate_path).with_context(|| {
        format!(
            "failed to read Baffle CA certificate {}",
            certificate_path.display()
        )
    })?;
    let private_key_pem = fs::read_to_string(&private_key_path).with_context(|| {
        format!(
            "failed to read Baffle CA private key {}",
            private_key_path.display()
        )
    })?;
    validate_ca_material(&certificate_pem, &private_key_pem).map_err(|error| {
        anyhow::anyhow!(
            "invalid Baffle CA material in {}: {error:#}",
            baffle_dir.display()
        )
    })
}

fn validate_ca_init_pending_marker(baffle_dir: &Path) -> anyhow::Result<()> {
    let path = baffle_dir.join(CA_INIT_PENDING_FILE);
    require_regular_file(&path)?;
    require_current_owner(&path)?;
    set_mode(&path, 0o600)
}

fn remove_ca_init_pending_marker(baffle_dir: &Path) -> anyhow::Result<()> {
    let path = baffle_dir.join(CA_INIT_PENDING_FILE);
    match fs::remove_file(&path) {
        Ok(()) => sync_directory(baffle_dir),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(error).with_context(|| {
            format!(
                "failed to remove Baffle CA initialization marker {}",
                path.display()
            )
        }),
    }
}

fn path_exists_without_following(path: &Path) -> anyhow::Result<bool> {
    match fs::symlink_metadata(path) {
        Ok(_) => Ok(true),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(false),
        Err(error) => Err(error).with_context(|| format!("failed to inspect {}", path.display())),
    }
}

fn require_regular_file(path: &Path) -> anyhow::Result<()> {
    let metadata = fs::symlink_metadata(path)
        .with_context(|| format!("failed to inspect Baffle CA file {}", path.display()))?;
    if metadata.file_type().is_symlink() || !metadata.is_file() {
        bail!(
            "Baffle CA material must be a regular file: {}",
            path.display()
        );
    }
    Ok(())
}

fn create_credentials_atomically(credentials_root: &Path, baffle_dir: &Path) -> anyhow::Result<()> {
    let staging_dir = credentials_root.join(format!(
        ".baffle-init-{}-{}",
        std::process::id(),
        OffsetDateTime::now_utc().unix_timestamp_nanos()
    ));
    create_directory(&staging_dir, 0o700)?;
    let cleanup = StagingDirectory(staging_dir.clone());
    write_new_file(&staging_dir.join(CA_INIT_PENDING_FILE), b"", 0o600)?;
    let secrets_dir = staging_dir.join(SECRETS_DIR);
    create_directory(&secrets_dir, 0o700)?;
    sync_directory(&secrets_dir)?;
    sync_directory(&staging_dir)?;

    fs::rename(&staging_dir, baffle_dir).with_context(|| {
        format!(
            "failed to publish Baffle CA at {} (another initialization may have created it); inspect the directory and retry",
            baffle_dir.display()
        )
    })?;
    drop(cleanup);
    sync_directory(credentials_root)?;
    Ok(())
}

fn acquire_credentials_lock(credentials_root: &Path) -> anyhow::Result<File> {
    let path = credentials_root.join(".baffle.lock");
    match fs::symlink_metadata(&path) {
        Ok(metadata) if metadata.file_type().is_symlink() || !metadata.is_file() => {
            bail!(
                "Baffle credentials lock is not a regular file: {}",
                path.display()
            );
        }
        Ok(_) => {}
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
        Err(error) => {
            return Err(error).with_context(|| {
                format!(
                    "failed to inspect Baffle credentials lock {}",
                    path.display()
                )
            });
        }
    }

    let mut options = OpenOptions::new();
    options.read(true).write(true).create(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt as _;
        options.mode(0o600);
    }
    let lock = options
        .open(&path)
        .with_context(|| format!("failed to open Baffle credentials lock {}", path.display()))?;
    set_mode(&path, 0o600)?;
    #[cfg(unix)]
    // SAFETY: `lock` owns a live file descriptor and `flock` does not retain pointers.
    if unsafe { libc::flock(std::os::fd::AsRawFd::as_raw_fd(&lock), libc::LOCK_EX) } == -1 {
        return Err(std::io::Error::last_os_error()).with_context(|| {
            format!(
                "failed to lock Baffle credentials at {}",
                credentials_root.display()
            )
        });
    }
    Ok(lock)
}

fn remove_stale_staging_directories(credentials_root: &Path) -> anyhow::Result<()> {
    for entry in fs::read_dir(credentials_root).with_context(|| {
        format!(
            "failed to read credentials directory {}",
            credentials_root.display()
        )
    })? {
        let entry = entry.with_context(|| {
            format!(
                "failed to inspect credentials directory {}",
                credentials_root.display()
            )
        })?;
        if !entry
            .file_name()
            .to_string_lossy()
            .starts_with(".baffle-init-")
        {
            continue;
        }
        let metadata = fs::symlink_metadata(entry.path()).with_context(|| {
            format!(
                "failed to inspect stale Baffle staging path {}",
                entry.path().display()
            )
        })?;
        if metadata.is_dir() && !metadata.file_type().is_symlink() {
            fs::remove_dir_all(entry.path()).with_context(|| {
                format!(
                    "failed to remove stale Baffle staging directory {}",
                    entry.path().display()
                )
            })?;
        } else {
            fs::remove_file(entry.path()).with_context(|| {
                format!(
                    "failed to remove stale Baffle staging path {}",
                    entry.path().display()
                )
            })?;
        }
    }
    Ok(())
}

#[cfg(unix)]
fn require_current_owner(path: &Path) -> anyhow::Result<()> {
    use std::os::unix::fs::MetadataExt as _;
    let owner = fs::symlink_metadata(path)
        .with_context(|| format!("failed to inspect owner of {}", path.display()))?
        .uid();
    require_owner(path, owner, unsafe { libc::geteuid() })
}

#[cfg(not(unix))]
fn require_current_owner(_path: &Path) -> anyhow::Result<()> {
    Ok(())
}

#[cfg(unix)]
fn require_owner(path: &Path, owner: u32, expected: u32) -> anyhow::Result<()> {
    if owner != expected {
        bail!(
            "Baffle credentials must be owned by the Cladding user ({expected}), but {} is owned by {owner}",
            path.display()
        );
    }
    Ok(())
}

struct StagingDirectory(PathBuf);

impl Drop for StagingDirectory {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

fn validate_ca_material(certificate_pem: &[u8], private_key_pem: &str) -> anyhow::Result<()> {
    let (remaining, pem) =
        parse_x509_pem(certificate_pem).context("CA certificate is not valid PEM")?;
    if !remaining.iter().all(u8::is_ascii_whitespace) || pem.label != "CERTIFICATE" {
        bail!("CA certificate must contain one CERTIFICATE PEM block");
    }
    let (remaining_der, certificate) = x509_parser::parse_x509_certificate(&pem.contents)
        .context("CA certificate is not valid X.509")?;
    if !remaining_der.is_empty() {
        bail!("CA certificate contains trailing X.509 data");
    }

    let constraints = certificate
        .basic_constraints()
        .context("CA basic constraints extension is invalid")?;
    if !constraints.is_some_and(|extension| extension.value.ca) {
        bail!("CA certificate is missing CA basic constraints");
    }
    let key_usage = certificate
        .key_usage()
        .context("CA key usage extension is invalid")?;
    if !key_usage.is_some_and(|extension| extension.value.key_cert_sign()) {
        bail!("CA certificate is missing certificate-signing key usage");
    }
    if certificate.subject() != certificate.issuer() {
        bail!("CA certificate is not self-issued");
    }
    certificate
        .verify_signature(None)
        .context("CA certificate self-signature is invalid")?;

    let now = OffsetDateTime::now_utc().unix_timestamp();
    let validity = certificate.validity();
    if now < validity.not_before.timestamp() {
        bail!("CA certificate is not yet valid");
    }
    if now >= validity.not_after.timestamp() {
        bail!("CA certificate has expired");
    }

    let key_pair = KeyPair::from_pem(private_key_pem)
        .context("CA private key is invalid PEM or unsupported")?;
    if certificate.public_key().subject_public_key.data.as_ref() != key_pair.public_key_raw() {
        bail!("CA certificate and private key do not match");
    }

    Ok(())
}

fn create_directory(path: &Path, mode: u32) -> anyhow::Result<()> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::DirBuilderExt as _;
        let mut builder = fs::DirBuilder::new();
        builder.mode(mode).create(path)?;
    }
    #[cfg(not(unix))]
    fs::create_dir(path)?;
    set_mode(path, mode)
}

fn write_new_file(path: &Path, contents: &[u8], mode: u32) -> anyhow::Result<()> {
    let mut options = OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt as _;
        options.mode(mode);
    }
    let mut file = options
        .open(path)
        .with_context(|| format!("failed to create {}", path.display()))?;
    file.write_all(contents)
        .with_context(|| format!("failed to write {}", path.display()))?;
    file.sync_all()
        .with_context(|| format!("failed to sync {}", path.display()))?;
    set_mode(path, mode)
}

#[cfg(unix)]
fn set_mode(path: &Path, mode: u32) -> anyhow::Result<()> {
    use std::os::unix::fs::PermissionsExt as _;
    fs::set_permissions(path, fs::Permissions::from_mode(mode))
        .with_context(|| format!("failed to set mode {mode:o} on {}", path.display()))
}

#[cfg(not(unix))]
fn set_mode(_path: &Path, _mode: u32) -> anyhow::Result<()> {
    Ok(())
}

#[cfg(unix)]
fn sync_directory(path: &Path) -> anyhow::Result<()> {
    File::open(path)
        .with_context(|| format!("failed to open directory {} for sync", path.display()))?
        .sync_all()
        .with_context(|| format!("failed to sync directory {}", path.display()))
}

#[cfg(not(unix))]
fn sync_directory(_path: &Path) -> anyhow::Result<()> {
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use rcgen::{
        BasicConstraints, CertificateParams, DistinguishedName, DnType, IsCa, KeyUsagePurpose,
    };
    use std::sync::atomic::{AtomicU64, Ordering};
    use time::Duration;

    static TEMP_COUNTER: AtomicU64 = AtomicU64::new(0);

    fn temp_project() -> PathBuf {
        let path = std::env::temp_dir().join(format!(
            "cladding-credentials-test-{}-{}",
            std::process::id(),
            TEMP_COUNTER.fetch_add(1, Ordering::Relaxed)
        ));
        fs::create_dir(&path).unwrap();
        path
    }

    fn write_existing_ca(baffle_dir: &Path, not_before: OffsetDateTime, not_after: OffsetDateTime) {
        fs::create_dir_all(baffle_dir.join(SECRETS_DIR)).unwrap();
        let (certificate, key) =
            generate_test_ca_material_with_validity(not_before, not_after).unwrap();
        fs::write(baffle_dir.join(CERTIFICATE_FILE), certificate).unwrap();
        fs::write(baffle_dir.join(PRIVATE_KEY_FILE), key).unwrap();
    }

    fn write_generated_ca(baffle_dir: &Path) {
        let (certificate, key) = generate_test_ca_material().unwrap();
        fs::write(baffle_dir.join(CERTIFICATE_FILE), certificate).unwrap();
        fs::write(baffle_dir.join(PRIVATE_KEY_FILE), key).unwrap();
    }

    fn generate_test_ca_material() -> anyhow::Result<(String, String)> {
        generate_test_ca_material_with_validity(
            OffsetDateTime::now_utc() - Duration::days(1),
            OffsetDateTime::now_utc() + Duration::days(3650),
        )
    }

    fn generate_test_ca_material_with_validity(
        not_before: OffsetDateTime,
        not_after: OffsetDateTime,
    ) -> anyhow::Result<(String, String)> {
        let mut params = CertificateParams::default();
        params.distinguished_name = DistinguishedName::new();
        params
            .distinguished_name
            .push(DnType::CommonName, "Baffle Interception CA");
        params.is_ca = IsCa::Ca(BasicConstraints::Unconstrained);
        params.key_usages = vec![KeyUsagePurpose::KeyCertSign, KeyUsagePurpose::CrlSign];
        params.not_before = not_before;
        params.not_after = not_after;

        let key_pair = KeyPair::generate()?;
        let certificate = params.self_signed(&key_pair)?;
        Ok((certificate.pem(), key_pair.serialize_pem()))
    }

    #[cfg(unix)]
    fn mode(path: &Path) -> u32 {
        use std::os::unix::fs::PermissionsExt as _;
        fs::metadata(path).unwrap().permissions().mode() & 0o777
    }

    #[test]
    fn creates_private_baffle_credentials_without_generating_a_ca() {
        let root = temp_project();
        ensure_baffle_credentials(&root).unwrap();

        let baffle_dir = root.join("credentials/baffle");
        let cert_path = baffle_dir.join(CERTIFICATE_FILE);
        let key_path = baffle_dir.join(PRIVATE_KEY_FILE);
        assert!(!cert_path.exists());
        assert!(!key_path.exists());
        assert!(
            fs::read_dir(baffle_dir.join(SECRETS_DIR))
                .unwrap()
                .next()
                .is_none()
        );

        #[cfg(unix)]
        {
            assert_eq!(mode(&root.join("credentials")), 0o700);
            assert_eq!(mode(&baffle_dir), 0o700);
            assert_eq!(mode(&baffle_dir.join(SECRETS_DIR)), 0o700);
            use std::os::unix::fs::MetadataExt as _;
            let owner = unsafe { libc::geteuid() };
            assert_eq!(
                fs::metadata(baffle_dir.join(SECRETS_DIR)).unwrap().uid(),
                owner
            );
        }
        assert_eq!(
            fs::read_dir(root.join(CREDENTIALS_DIR))
                .unwrap()
                .map(|entry| entry.unwrap().file_name())
                .collect::<std::collections::BTreeSet<_>>(),
            [".baffle.lock", BAFFLE_DIR]
                .into_iter()
                .map(std::ffi::OsString::from)
                .collect()
        );

        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn baffle_initializer_runs_once_and_existing_ca_is_reused() {
        let root = temp_project();
        let baffle_dir = root.join("credentials/baffle");
        let mut init_count = 0;
        ensure_baffle_ca(&root, || {
            init_count += 1;
            write_generated_ca(&baffle_dir);
            Ok(())
        })
        .unwrap();

        let cert_path = baffle_dir.join(CERTIFICATE_FILE);
        let key_path = baffle_dir.join(PRIVATE_KEY_FILE);
        let cert_before = fs::read(&cert_path).unwrap();
        let key_before = fs::read(&key_path).unwrap();
        ensure_baffle_ca(&root, || {
            init_count += 1;
            Err(anyhow::anyhow!(
                "initializer must not run for existing CA material"
            ))
        })
        .unwrap();

        assert_eq!(init_count, 1);
        assert_eq!(fs::read(cert_path).unwrap(), cert_before);
        assert_eq!(fs::read(key_path).unwrap(), key_before);
        assert!(!baffle_dir.join(CA_INIT_PENDING_FILE).exists());
        #[cfg(unix)]
        {
            assert_eq!(mode(&baffle_dir.join(CERTIFICATE_FILE)), 0o644);
            assert_eq!(mode(&baffle_dir.join(PRIVATE_KEY_FILE)), 0o600);
            use std::os::unix::fs::MetadataExt as _;
            let owner = unsafe { libc::geteuid() };
            assert_eq!(
                fs::metadata(baffle_dir.join(CERTIFICATE_FILE))
                    .unwrap()
                    .uid(),
                owner
            );
            assert_eq!(
                fs::metadata(baffle_dir.join(PRIVATE_KEY_FILE))
                    .unwrap()
                    .uid(),
                owner
            );
        }
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn validation_rejects_pending_ca_without_bootstrapping_it() {
        let root = temp_project();
        ensure_baffle_credentials(&root).unwrap();
        let baffle_dir = root.join("credentials/baffle");
        let pending = baffle_dir.join(CA_INIT_PENDING_FILE);

        let error = validate_baffle_ca(&root).unwrap_err().to_string();

        assert!(error.contains("run `cladding build`"), "{error}");
        assert!(pending.is_file());
        assert!(!baffle_dir.join(CERTIFICATE_FILE).exists());
        assert!(!baffle_dir.join(PRIVATE_KEY_FILE).exists());
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn validation_rejects_corrupt_ca_without_replacing_it() {
        let root = temp_project();
        let baffle_dir = root.join("credentials/baffle");
        ensure_baffle_ca(&root, || {
            write_generated_ca(&baffle_dir);
            Ok(())
        })
        .unwrap();
        let cert_path = baffle_dir.join(CERTIFICATE_FILE);
        fs::write(&cert_path, b"corrupt certificate").unwrap();
        let corrupt_certificate = fs::read(&cert_path).unwrap();
        let private_key = fs::read(baffle_dir.join(PRIVATE_KEY_FILE)).unwrap();

        let error = validate_baffle_ca(&root).unwrap_err().to_string();

        assert!(error.contains("invalid Baffle CA material"), "{error}");
        assert_eq!(fs::read(cert_path).unwrap(), corrupt_certificate);
        assert_eq!(
            fs::read(baffle_dir.join(PRIVATE_KEY_FILE)).unwrap(),
            private_key
        );
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn missing_ca_after_success_is_reported_without_reinitializing() {
        let root = temp_project();
        let baffle_dir = root.join("credentials/baffle");
        ensure_baffle_ca(&root, || {
            write_generated_ca(&baffle_dir);
            Ok(())
        })
        .unwrap();
        fs::remove_file(baffle_dir.join(CERTIFICATE_FILE)).unwrap();
        fs::remove_file(baffle_dir.join(PRIVATE_KEY_FILE)).unwrap();

        let validation_error = validate_baffle_ca(&root).unwrap_err().to_string();
        assert!(
            validation_error.contains("incomplete Baffle CA"),
            "{validation_error}"
        );

        let mut init_count = 0;
        let error = ensure_baffle_ca(&root, || {
            init_count += 1;
            Ok(())
        })
        .unwrap_err()
        .to_string();

        assert_eq!(init_count, 0);
        assert!(error.contains("incomplete Baffle CA"), "{error}");
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn baffle_initializer_errors_are_reported_without_ca_files() {
        let root = temp_project();
        let error = ensure_baffle_ca(&root, || {
            Err(anyhow::anyhow!("proxy image could not initialize CA"))
        })
        .unwrap_err()
        .to_string();

        assert!(
            error.contains("proxy image could not initialize CA"),
            "{error}"
        );
        let baffle_dir = root.join("credentials/baffle");
        assert!(!baffle_dir.join(CERTIFICATE_FILE).exists());
        assert!(!baffle_dir.join(PRIVATE_KEY_FILE).exists());
        assert!(baffle_dir.join(CA_INIT_PENDING_FILE).is_file());
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn failed_baffle_initialization_can_retry_without_existing_ca_files() {
        let root = temp_project();
        let first_error = ensure_baffle_ca(&root, || {
            Err(anyhow::anyhow!("temporary proxy startup failure"))
        })
        .unwrap_err()
        .to_string();
        assert!(first_error.contains("temporary proxy startup failure"));

        let baffle_dir = root.join("credentials/baffle");
        ensure_baffle_ca(&root, || {
            write_generated_ca(&baffle_dir);
            Ok(())
        })
        .unwrap();

        assert!(baffle_dir.join(CERTIFICATE_FILE).is_file());
        assert!(baffle_dir.join(PRIVATE_KEY_FILE).is_file());
        assert!(!baffle_dir.join(CA_INIT_PENDING_FILE).exists());
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn incomplete_ca_is_reported_without_replacing_existing_material() {
        let root = temp_project();
        let baffle_dir = root.join("credentials/baffle");
        fs::create_dir_all(&baffle_dir).unwrap();
        let cert_path = baffle_dir.join(CERTIFICATE_FILE);
        fs::write(&cert_path, b"keep this file").unwrap();

        let error = ensure_baffle_credentials(&root).unwrap_err().to_string();
        assert!(error.contains("incomplete Baffle CA"), "{error}");
        assert_eq!(fs::read(cert_path).unwrap(), b"keep this file");

        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn expired_ca_is_reported_without_rotating_it() {
        let root = temp_project();
        let baffle_dir = root.join("credentials/baffle");
        let now = OffsetDateTime::now_utc();
        write_existing_ca(
            &baffle_dir,
            now - Duration::days(20),
            now - Duration::days(1),
        );
        let cert_path = baffle_dir.join(CERTIFICATE_FILE);
        let cert_before = fs::read(&cert_path).unwrap();

        let error = ensure_baffle_credentials(&root).unwrap_err().to_string();
        assert!(error.contains("expired"), "{error}");
        assert_eq!(fs::read(cert_path).unwrap(), cert_before);

        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn malformed_ca_is_reported_without_replacing_either_file() {
        let root = temp_project();
        let baffle_dir = root.join("credentials/baffle");
        fs::create_dir_all(baffle_dir.join(SECRETS_DIR)).unwrap();
        let certificate = b"not a certificate";
        let (_, key) = generate_test_ca_material().unwrap();
        let cert_path = baffle_dir.join(CERTIFICATE_FILE);
        let key_path = baffle_dir.join(PRIVATE_KEY_FILE);
        fs::write(&cert_path, certificate).unwrap();
        fs::write(&key_path, &key).unwrap();

        let error = ensure_baffle_credentials(&root).unwrap_err().to_string();
        assert!(error.contains("not valid PEM"), "{error}");
        assert_eq!(fs::read(cert_path).unwrap(), certificate);
        assert_eq!(fs::read(key_path).unwrap(), key.as_bytes());

        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn mismatched_ca_key_is_reported_without_replacing_either_file() {
        let root = temp_project();
        let baffle_dir = root.join("credentials/baffle");
        fs::create_dir_all(baffle_dir.join(SECRETS_DIR)).unwrap();
        let (certificate, _) = generate_test_ca_material().unwrap();
        let (_, other_key) = generate_test_ca_material().unwrap();
        let cert_path = baffle_dir.join(CERTIFICATE_FILE);
        let key_path = baffle_dir.join(PRIVATE_KEY_FILE);
        fs::write(&cert_path, &certificate).unwrap();
        fs::write(&key_path, &other_key).unwrap();

        let error = ensure_baffle_credentials(&root).unwrap_err().to_string();
        assert!(error.contains("do not match"), "{error}");
        assert_eq!(fs::read(cert_path).unwrap(), certificate.as_bytes());
        assert_eq!(fs::read(key_path).unwrap(), other_key.as_bytes());

        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn secret_files_are_kept_unchanged_and_made_private() {
        let root = temp_project();
        ensure_baffle_credentials(&root).unwrap();
        let secret_path = root.join("credentials/baffle/secrets/api-token");
        fs::write(&secret_path, b"user-provisioned secret value").unwrap();
        #[cfg(unix)]
        set_mode(&secret_path, 0o644).unwrap();

        ensure_baffle_credentials(&root).unwrap();

        assert_eq!(
            fs::read(secret_path).unwrap(),
            b"user-provisioned secret value"
        );
        #[cfg(unix)]
        assert_eq!(
            mode(&root.join("credentials/baffle/secrets/api-token")),
            0o600
        );
        fs::remove_dir_all(root).unwrap();
    }

    #[cfg(unix)]
    #[test]
    fn wrong_credential_owner_is_reported() {
        let root = temp_project();
        let expected = unsafe { libc::geteuid() };
        let actual = expected.wrapping_add(1);
        let error = require_owner(&root, actual, expected)
            .unwrap_err()
            .to_string();
        assert!(error.contains("owned by"), "{error}");
        fs::remove_dir_all(root).unwrap();
    }

    #[cfg(unix)]
    #[test]
    fn symlinked_secret_files_are_rejected() {
        use std::os::unix::fs::symlink;

        let root = temp_project();
        ensure_baffle_credentials(&root).unwrap();
        let outside = root.join("outside-secret");
        fs::write(&outside, b"secret").unwrap();
        symlink(&outside, root.join("credentials/baffle/secrets/api-token")).unwrap();

        let error = ensure_baffle_credentials(&root).unwrap_err().to_string();
        assert!(error.contains("regular file"), "{error}");
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn concurrent_initialization_publishes_one_valid_ca() {
        let root = temp_project();
        let first_root = root.clone();
        let second_root = root.clone();
        let init_count = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let first_count = init_count.clone();
        let second_count = init_count.clone();
        let first = std::thread::spawn(move || {
            ensure_baffle_ca(&first_root, || {
                first_count.fetch_add(1, Ordering::Relaxed);
                write_generated_ca(&first_root.join("credentials/baffle"));
                Ok(())
            })
        });
        let second = std::thread::spawn(move || {
            ensure_baffle_ca(&second_root, || {
                second_count.fetch_add(1, Ordering::Relaxed);
                write_generated_ca(&second_root.join("credentials/baffle"));
                Ok(())
            })
        });
        first.join().unwrap().unwrap();
        second.join().unwrap().unwrap();
        assert_eq!(init_count.load(Ordering::Relaxed), 1);

        let baffle_dir = root.join("credentials/baffle");
        let cert = fs::read(baffle_dir.join(CERTIFICATE_FILE)).unwrap();
        let key = fs::read_to_string(baffle_dir.join(PRIVATE_KEY_FILE)).unwrap();
        validate_ca_material(&cert, &key).unwrap();
        fs::remove_dir_all(root).unwrap();
    }
}
