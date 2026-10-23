use anyhow::{Context as _, Result, bail, ensure};
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::env;
use std::fs::{self, File, OpenOptions};
use std::io;
use std::path::{Component, Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicUsize, Ordering};

const EMBEDDED_MANIFEST: &str = include_str!(concat!(env!("OUT_DIR"), "/dependency-manifest.json"));
const UPSTREAM_PINS: &str = include_str!("../scripts/dependency-pins.json");
const LINUX_ARCHITECTURE: &str = env!("CLADDING_LINUX_ARCHITECTURE");
const GANTRY_VERSION: &str = "v0.4.8";
const BAFFLE_VERSION: &str = "v2.2.0";
const GANTRY_MAX_GLIBC: (u16, u16) = (2, 35);
const DEVELOPMENT_MANIFEST: &str = "development-manifest.json";
static TEMPORARY_DIRECTORY_ID: AtomicUsize = AtomicUsize::new(0);

#[derive(Debug, Deserialize)]
struct Manifest {
    schema_version: u32,
    cladding_release: Option<CladdingRelease>,
    platforms: HashMap<String, Vec<Artifact>>,
}

#[derive(Debug, Deserialize)]
struct CladdingRelease {
    version: String,
    tag: String,
}

#[derive(Debug, Deserialize)]
struct Artifact {
    name: String,
    source_repository: String,
    release_tag: String,
    archive_filename: String,
    architecture: String,
    archive_sha256: String,
    archive_entries: Vec<ArchiveEntry>,
    executables: Vec<Executable>,
}

#[derive(Debug, Deserialize)]
struct ArchiveEntry {
    path: String,
    kind: String,
}

#[derive(Debug, Deserialize)]
struct Executable {
    archive_path: String,
    install_path: String,
    sha256: String,
}

#[derive(Debug, Deserialize, Serialize)]
struct DevelopmentManifest {
    schema_version: u32,
    architecture: String,
    executables: BTreeMap<String, String>,
}

struct StagingDirectory(PathBuf);

impl Drop for StagingDirectory {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

struct PendingInstall {
    name: String,
    staged_path: PathBuf,
    destination: PathBuf,
}

pub fn install_tools(project_root: &Path) -> Result<()> {
    let tools_dir = project_root.join("tools");
    let bin_dir = tools_dir.join("bin");
    ensure_tool_directories(&tools_dir, &bin_dir)?;

    match embedded_manifest()? {
        Some(manifest) => install_release_tools(&manifest, &bin_dir),
        None => {
            let overrides = development_overrides()?;
            let Some(overrides) = overrides else {
                bail!(
                    "this development build has no pinned release dependency manifest; set CLADDING_BRIDGE_BIN, CLADDING_GANTRY_DIR, and CLADDING_BAFFLE_BIN when running `cladding build`"
                );
            };
            install_development_tools(&overrides, &tools_dir, &bin_dir)
        }
    }
}

/// Return diagnostics for all fixed helper executables. This path is offline.
pub fn check_installed_tools(project_root: &Path) -> Vec<String> {
    let bin_dir = project_root.join("tools/bin");
    let release_manifest = match embedded_manifest() {
        Ok(manifest) => manifest,
        Err(error) => return vec![format!("invalid dependency manifest: {error:#}")],
    };

    let expected = match release_manifest {
        Some(manifest) => match expected_executables(&manifest) {
            Ok(expected) => expected,
            Err(error) => return vec![format!("invalid dependency manifest: {error:#}")],
        },
        None => match read_development_manifest(project_root) {
            Ok(Some(manifest)) => manifest.executables,
            Ok(None) => {
                return vec![
                    "dependency metadata is unavailable; run `cladding build` with a published release or explicit local tool overrides".to_string(),
                ];
            }
            Err(error) => return vec![format!("invalid development tool metadata: {error:#}")],
        },
    };

    let mut errors = Vec::new();
    for name in ["cladding-bridge", "gantry", "run-remote", "baffle"] {
        let path = bin_dir.join(name);
        let Some(expected_hash) = expected.get(name) else {
            errors.push(format!("missing dependency checksum for tools/bin/{name}"));
            continue;
        };
        match installed_checksum_status(&path, expected_hash) {
            Ok(None) => {}
            Ok(Some(reason)) => {
                errors.push(format!("{reason}: tools/bin/{name} ({})", path.display()))
            }
            Err(error) => errors.push(format!("corrupt: tools/bin/{name} ({error:#})")),
        }
    }
    errors
}

fn embedded_manifest() -> Result<Option<Manifest>> {
    let manifest: Manifest = serde_json::from_str(EMBEDDED_MANIFEST)
        .context("failed to parse embedded dependency manifest")?;
    ensure!(
        manifest.schema_version == 1,
        "unsupported dependency manifest schema"
    );
    let Some(release) = manifest.cladding_release.as_ref() else {
        return Ok(None);
    };
    let expected_version = env!("CARGO_PKG_VERSION");
    ensure!(
        release.version == expected_version && release.tag == format!("v{expected_version}"),
        "dependency manifest release {} does not match Cladding package version {expected_version}",
        release.tag
    );
    expected_executables(&manifest)?;
    for artifact in manifest
        .platforms
        .get(LINUX_ARCHITECTURE)
        .expect("validated platform records")
    {
        validate_upstream_archive_pin(artifact)?;
    }
    Ok(Some(manifest))
}

fn expected_executables(manifest: &Manifest) -> Result<BTreeMap<String, String>> {
    let release = manifest
        .cladding_release
        .as_ref()
        .context("release manifest has no Cladding release identity")?;
    let records = manifest
        .platforms
        .get(LINUX_ARCHITECTURE)
        .with_context(|| format!("manifest has no dependency records for {LINUX_ARCHITECTURE}"))?;
    ensure!(
        records.len() == 3,
        "manifest must contain exactly three archive records"
    );

    let mut groups = BTreeSet::new();
    let mut executables = BTreeMap::new();
    for artifact in records {
        ensure!(
            groups.insert(artifact.name.as_str()),
            "duplicate artifact group {}",
            artifact.name
        );
        ensure!(
            artifact.architecture == LINUX_ARCHITECTURE,
            "artifact {} targets {} instead of {LINUX_ARCHITECTURE}",
            artifact.name,
            artifact.architecture
        );
        validate_sha256(&artifact.archive_sha256).with_context(|| {
            format!("invalid archive checksum for {}", artifact.archive_filename)
        })?;
        ensure!(
            safe_relative_path(&artifact.archive_filename),
            "unsafe archive filename {}",
            artifact.archive_filename
        );
        ensure!(
            artifact.archive_filename.ends_with(".tar.gz"),
            "invalid archive filename {}",
            artifact.archive_filename
        );
        validate_archive_entries(artifact)
            .with_context(|| format!("invalid archive entry manifest for {}", artifact.name))?;

        let expected_source = match artifact.name.as_str() {
            "cladding-bridge" => "dstoc/cladding",
            "gantry" => "dstoc/gantry",
            "baffle" => "dstoc/baffle",
            other => bail!("unsupported dependency group {other}"),
        };
        ensure!(
            artifact.source_repository == expected_source,
            "unexpected source repository for {}",
            artifact.name
        );
        match artifact.name.as_str() {
            "cladding-bridge" => {
                ensure!(
                    artifact.release_tag == release.tag,
                    "bridge release {} does not match Cladding release {}",
                    artifact.release_tag,
                    release.tag
                );
                ensure!(
                    artifact.archive_filename
                        == format!(
                            "cladding-bridge-{}-{}.tar.gz",
                            release.tag, LINUX_ARCHITECTURE
                        ),
                    "unexpected bridge archive filename {}",
                    artifact.archive_filename
                );
            }
            "gantry" => {
                ensure!(
                    artifact.release_tag == GANTRY_VERSION,
                    "manifest changes the pinned Gantry version from {GANTRY_VERSION}"
                );
                let arch = LINUX_ARCHITECTURE
                    .strip_suffix("-unknown-linux-gnu")
                    .unwrap_or_default();
                ensure!(
                    artifact.archive_filename == format!("gantry-0.4.8-linux-{arch}-gnu.tar.gz"),
                    "unexpected Gantry archive filename {}",
                    artifact.archive_filename
                );
            }
            "baffle" => {
                ensure!(
                    artifact.release_tag == BAFFLE_VERSION,
                    "manifest changes the pinned Baffle version from {BAFFLE_VERSION}"
                );
                let arch = LINUX_ARCHITECTURE
                    .strip_suffix("-unknown-linux-gnu")
                    .unwrap_or_default();
                ensure!(
                    artifact.archive_filename
                        == format!("baffle-proxy-v2.2.0-{arch}-unknown-linux-gnu.tar.gz"),
                    "unexpected Baffle archive filename {}",
                    artifact.archive_filename
                );
            }
            _ => unreachable!(),
        }

        let mut group_executables = BTreeSet::new();
        for executable in &artifact.executables {
            let archive_path = normalize_archive_path(&executable.archive_path)?;
            ensure!(
                archive_path != ".",
                "unsafe executable archive path {}",
                executable.archive_path
            );
            ensure!(
                safe_tool_name(&executable.install_path),
                "unsafe executable install path {}",
                executable.install_path
            );
            validate_sha256(&executable.sha256).with_context(|| {
                format!(
                    "invalid executable checksum for {}",
                    executable.install_path
                )
            })?;
            ensure!(
                group_executables.insert(executable.install_path.as_str()),
                "duplicate executable {}",
                executable.install_path
            );
            ensure!(
                artifact.archive_entries.iter().any(|entry| {
                    entry.kind == "file"
                        && normalize_archive_path(&entry.path)
                            .is_ok_and(|path| path == archive_path)
                }),
                "archive manifest omits executable {}",
                executable.archive_path
            );
            ensure!(
                executables
                    .insert(executable.install_path.clone(), executable.sha256.clone())
                    .is_none(),
                "duplicate installed executable {}",
                executable.install_path
            );
        }
        match artifact.name.as_str() {
            "cladding-bridge" => ensure!(
                group_executables == BTreeSet::from(["cladding-bridge"]),
                "bridge group must contain cladding-bridge only"
            ),
            "gantry" => ensure!(
                group_executables == BTreeSet::from(["gantry", "run-remote"]),
                "Gantry group must contain gantry and run-remote together"
            ),
            "baffle" => ensure!(
                group_executables == BTreeSet::from(["baffle"]),
                "Baffle group must contain baffle only"
            ),
            _ => unreachable!(),
        }
    }
    ensure!(
        groups == BTreeSet::from(["baffle", "cladding-bridge", "gantry"]),
        "manifest is missing a required dependency archive"
    );
    ensure!(
        executables
            .keys()
            .map(String::as_str)
            .collect::<BTreeSet<_>>()
            == BTreeSet::from(["baffle", "cladding-bridge", "gantry", "run-remote"]),
        "manifest must pin all four executable checksums"
    );
    Ok(executables)
}

fn validate_upstream_archive_pin(artifact: &Artifact) -> Result<()> {
    if artifact.name == "cladding-bridge" {
        return Ok(());
    }
    let pins: serde_json::Value = serde_json::from_str(UPSTREAM_PINS)
        .context("failed to parse trusted upstream dependency pins")?;
    let arch = LINUX_ARCHITECTURE
        .strip_suffix("-unknown-linux-gnu")
        .unwrap_or_default();
    let pin = pins
        .get("dependencies")
        .and_then(|value| value.get(&artifact.name))
        .with_context(|| format!("trusted pin is missing {}", artifact.name))?;
    let expected_repository = pin
        .get("repository")
        .and_then(serde_json::Value::as_str)
        .unwrap_or_default();
    let expected_tag = pin
        .get("version")
        .and_then(serde_json::Value::as_str)
        .map(|version| format!("v{version}"))
        .unwrap_or_default();
    let expected_arch = pin
        .get("archives")
        .and_then(|value| value.get(arch))
        .with_context(|| format!("trusted pin is missing {} for {arch}", artifact.name))?;
    let expected_filename = expected_arch
        .get("filename")
        .and_then(serde_json::Value::as_str)
        .unwrap_or_default();
    let expected_sha256 = expected_arch
        .get("sha256")
        .and_then(serde_json::Value::as_str)
        .unwrap_or_default();
    ensure!(
        artifact.source_repository == expected_repository,
        "manifest changes the source repository for {}",
        artifact.name
    );
    ensure!(
        artifact.release_tag == expected_tag,
        "manifest changes the release tag for {}",
        artifact.name
    );
    ensure!(
        artifact.archive_filename == expected_filename,
        "manifest changes the archive filename for {}",
        artifact.name
    );
    ensure!(
        artifact.archive_sha256 == expected_sha256,
        "manifest changes the trusted archive checksum for {}",
        artifact.name
    );
    Ok(())
}

fn validate_archive_entries(artifact: &Artifact) -> Result<()> {
    ensure!(
        !artifact.archive_entries.is_empty(),
        "archive entry list is empty"
    );
    let mut paths = BTreeMap::new();
    for entry in &artifact.archive_entries {
        let path = normalize_archive_path(&entry.path)?;
        ensure!(
            path != "." || entry.kind == "directory",
            "unsafe archive entry {}",
            entry.path
        );
        ensure!(
            matches!(entry.kind.as_str(), "file" | "directory"),
            "unsupported archive entry type {}",
            entry.kind
        );
        ensure!(
            paths.insert(path.clone(), entry.kind.as_str()).is_none(),
            "duplicate archive entry {}",
            entry.path
        );
    }
    Ok(())
}

fn install_release_tools(manifest: &Manifest, bin_dir: &Path) -> Result<()> {
    install_release_tools_with(manifest, bin_dir, Path::new("curl"))
}

fn install_release_tools_with(manifest: &Manifest, bin_dir: &Path, curl: &Path) -> Result<()> {
    expected_executables(manifest)?;
    let records = manifest
        .platforms
        .get(LINUX_ARCHITECTURE)
        .expect("validated platform records");
    let staging = StagingDirectory(create_staging_directory(bin_dir)?);
    let mut pending = Vec::new();

    for artifact in records {
        let mut missing = Vec::new();
        for executable in &artifact.executables {
            let destination = bin_dir.join(&executable.install_path);
            if !matches_checksum(&destination, &executable.sha256)? {
                ensure_replaceable(&destination)?;
                missing.push(executable);
            }
        }
        if missing.is_empty() {
            continue;
        }

        let archive = staging.0.join(&artifact.archive_filename);
        let url = format!(
            "https://github.com/{}/releases/download/{}/{}",
            artifact.source_repository, artifact.release_tag, artifact.archive_filename
        );
        download_archive(curl, &url, &archive)?;
        ensure!(
            sha256_file(&archive)? == artifact.archive_sha256,
            "archive checksum mismatch for {} from {url}",
            artifact.archive_filename
        );
        validate_archive(&archive, artifact)?;

        for executable in missing {
            let staged_path = staging
                .0
                .join(format!("install-{}", executable.install_path));
            extract_archive_entry(&archive, &executable.archive_path, &staged_path)?;
            let bytes = read_regular_file(&staged_path)?;
            ensure!(
                sha256_file(&staged_path)? == executable.sha256,
                "executable checksum mismatch for {}",
                executable.install_path
            );
            validate_executable(&bytes, &executable.install_path, LINUX_ARCHITECTURE)?;
            set_executable_mode(&staged_path)?;
            pending.push(PendingInstall {
                name: executable.install_path.clone(),
                staged_path,
                destination: bin_dir.join(&executable.install_path),
            });
        }
    }

    commit_installs(pending)?;
    Ok(())
}

#[derive(Debug)]
struct DevelopmentOverrides {
    bridge: PathBuf,
    gantry: PathBuf,
    run_remote: PathBuf,
    baffle: PathBuf,
    gantry_dir: PathBuf,
}

fn development_overrides() -> Result<Option<DevelopmentOverrides>> {
    let bridge = env::var_os("CLADDING_BRIDGE_BIN");
    let gantry_dir = env::var_os("CLADDING_GANTRY_DIR");
    let baffle = env::var_os("CLADDING_BAFFLE_BIN");
    if bridge.is_none() && gantry_dir.is_none() && baffle.is_none() {
        return Ok(None);
    }
    let (Some(bridge), Some(gantry_dir), Some(baffle)) = (bridge, gantry_dir, baffle) else {
        bail!(
            "development tool overrides must be supplied together: CLADDING_BRIDGE_BIN, CLADDING_GANTRY_DIR (with gantry and run-remote), and CLADDING_BAFFLE_BIN"
        );
    };
    let gantry_dir = PathBuf::from(gantry_dir);
    let metadata = fs::symlink_metadata(&gantry_dir).with_context(|| {
        format!(
            "failed to inspect CLADDING_GANTRY_DIR {}",
            gantry_dir.display()
        )
    })?;
    ensure!(
        metadata.is_dir() && !metadata.file_type().is_symlink(),
        "CLADDING_GANTRY_DIR is not a real directory: {}",
        gantry_dir.display()
    );
    Ok(Some(DevelopmentOverrides {
        bridge: PathBuf::from(bridge),
        gantry: gantry_dir.join("gantry"),
        run_remote: gantry_dir.join("run-remote"),
        baffle: PathBuf::from(baffle),
        gantry_dir,
    }))
}

fn install_development_tools(
    overrides: &DevelopmentOverrides,
    tools_dir: &Path,
    bin_dir: &Path,
) -> Result<()> {
    let staging = StagingDirectory(create_staging_directory(bin_dir)?);
    let mut hashes = BTreeMap::new();
    let mut pending = Vec::new();
    for (name, source) in [
        ("cladding-bridge", overrides.bridge.as_path()),
        ("gantry", overrides.gantry.as_path()),
        ("run-remote", overrides.run_remote.as_path()),
        ("baffle", overrides.baffle.as_path()),
    ] {
        let bytes = read_regular_file(source)
            .with_context(|| format!("local override for {name} is invalid"))?;
        validate_executable(&bytes, name, LINUX_ARCHITECTURE)
            .with_context(|| format!("local override for {name} is incompatible"))?;
        let hash = sha256_file(source)?;
        hashes.insert(name.to_string(), hash.clone());
        let destination = bin_dir.join(name);
        if matches_checksum(&destination, &hash)? {
            continue;
        }
        ensure_replaceable(&destination)?;
        let staged_path = staging.0.join(format!("install-{name}"));
        fs::copy(source, &staged_path)
            .with_context(|| format!("failed to stage local override for {name}"))?;
        set_executable_mode(&staged_path)?;
        pending.push(PendingInstall {
            name: name.to_string(),
            staged_path,
            destination,
        });
    }
    commit_installs(pending)?;
    write_development_manifest(
        tools_dir,
        DevelopmentManifest {
            schema_version: 1,
            architecture: LINUX_ARCHITECTURE.to_string(),
            executables: hashes,
        },
    )?;

    for (source, relative) in [
        ("LICENSE", "share/doc/gantry/LICENSE"),
        ("README.md", "share/doc/gantry/README.md"),
        ("docs/reference.md", "share/doc/gantry/docs/reference.md"),
    ] {
        let path = overrides.gantry_dir.join(source);
        if let Ok(bytes) = read_regular_file(&path) {
            let destination = tools_dir.join(relative);
            if let Some(parent) = destination.parent() {
                fs::create_dir_all(parent)?;
            }
            fs::write(&destination, bytes)
                .with_context(|| format!("failed to install {}", path.display()))?;
            set_document_mode(&destination)?;
        }
    }
    Ok(())
}

fn validate_archive(archive: &Path, artifact: &Artifact) -> Result<()> {
    let listed = run_tar_list(archive, false)?;
    let detailed = run_tar_list(archive, true)?;
    ensure!(
        listed.len() == detailed.len(),
        "tar listing changed while validating {}",
        artifact.archive_filename
    );
    let expected = artifact
        .archive_entries
        .iter()
        .map(|entry| Ok((normalize_archive_path(&entry.path)?, entry.kind.as_str())))
        .collect::<Result<HashMap<_, _>>>()?;
    let mut found = BTreeSet::new();
    for (name, detail) in listed.iter().zip(detailed.iter()) {
        let normalized = normalize_archive_path(name)?;
        let kind = match detail.as_bytes().first() {
            Some(b'd') => "directory",
            Some(b'-') => "file",
            _ => bail!(
                "{} contains a symlink or unsupported entry {name:?}",
                artifact.archive_filename
            ),
        };
        ensure!(
            normalized != "." || kind == "directory",
            "unsafe entry {name:?} in {}",
            artifact.archive_filename
        );
        ensure!(
            expected.get(&normalized) == Some(&kind),
            "{} contains unexpected archive entry {name:?}",
            artifact.archive_filename
        );
        ensure!(
            found.insert(normalized),
            "{} contains duplicate archive entry {name:?}",
            artifact.archive_filename
        );
    }
    ensure!(
        found.len() == expected.len(),
        "{} is missing one or more expected archive entries",
        artifact.archive_filename
    );
    Ok(())
}

fn run_tar_list(archive: &Path, verbose: bool) -> Result<Vec<String>> {
    let mut command = Command::new("tar");
    if verbose {
        command.arg("-tvzf");
    } else {
        command.arg("-tzf");
    }
    command.arg(archive);
    let output = command
        .output()
        .context("failed to run tar for dependency archive")?;
    ensure!(
        output.status.success(),
        "tar could not list dependency archive {}",
        archive.display()
    );
    let text = String::from_utf8(output.stdout).context("tar returned non-UTF-8 archive paths")?;
    Ok(text.lines().map(str::to_string).collect())
}

fn extract_archive_entry(archive: &Path, entry: &str, destination: &Path) -> Result<()> {
    let normalized = normalize_archive_path(entry)?;
    ensure!(normalized != ".", "unsafe archive extraction path {entry}");
    let output_file = OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(destination)
        .with_context(|| format!("failed to stage archive entry {entry}"))?;
    let output = Command::new("tar")
        .args(["-xOzf"])
        .arg(archive)
        .arg("--")
        .arg(entry)
        .stdout(Stdio::from(output_file))
        .output()
        .with_context(|| format!("failed to extract {entry} from {}", archive.display()))?;
    ensure!(
        output.status.success(),
        "failed to extract {entry} from {}",
        archive.display()
    );
    Ok(())
}

fn download_archive(curl: &Path, url: &str, destination: &Path) -> Result<()> {
    let status = Command::new(curl)
        .args(["--fail", "--location", "--retry", "3", "--output"])
        .arg(destination)
        .arg(url)
        .status()
        .with_context(|| format!("failed to run curl for {url}"))?;
    ensure!(status.success(), "download failed for {url}");
    Ok(())
}

fn commit_installs(pending: Vec<PendingInstall>) -> Result<()> {
    for install in pending {
        fs::rename(&install.staged_path, &install.destination).with_context(|| {
            format!(
                "failed to atomically install {} at {}",
                install.name,
                install.destination.display()
            )
        })?;
    }
    Ok(())
}

fn create_staging_directory(bin_dir: &Path) -> Result<PathBuf> {
    for _ in 0..100 {
        let id = TEMPORARY_DIRECTORY_ID.fetch_add(1, Ordering::Relaxed);
        let path = bin_dir.join(format!(".dependency-stage-{}-{id}", std::process::id()));
        match fs::create_dir(&path) {
            Ok(()) => return Ok(path),
            Err(error) if error.kind() == io::ErrorKind::AlreadyExists => continue,
            Err(error) => {
                return Err(error).with_context(|| format!("failed to create {}", path.display()));
            }
        }
    }
    bail!(
        "could not allocate a temporary dependency staging directory in {}",
        bin_dir.display()
    )
}

fn ensure_tool_directories(tools_dir: &Path, bin_dir: &Path) -> Result<()> {
    ensure_directory(tools_dir)?;
    ensure_directory(bin_dir)
}

fn ensure_directory(path: &Path) -> Result<()> {
    match fs::symlink_metadata(path) {
        Ok(metadata) if metadata.file_type().is_symlink() => {
            let target = fs::metadata(path)
                .with_context(|| format!("failed to inspect tool directory {}", path.display()))?;
            ensure!(
                target.is_dir(),
                "tool directory symlink does not point to a directory: {}",
                path.display()
            );
            Ok(())
        }
        Ok(metadata) if !metadata.is_dir() => {
            bail!("tool path is not a directory: {}", path.display())
        }
        Ok(_) => Ok(()),
        Err(error) if error.kind() == io::ErrorKind::NotFound => fs::create_dir_all(path)
            .with_context(|| format!("failed to create tool directory {}", path.display())),
        Err(error) => Err(error)
            .with_context(|| format!("failed to inspect tool directory {}", path.display())),
    }
}

fn ensure_replaceable(path: &Path) -> Result<()> {
    match fs::symlink_metadata(path) {
        Ok(metadata) if metadata.is_file() || metadata.file_type().is_symlink() => Ok(()),
        Ok(_) => bail!("refusing to replace non-file tool path {}", path.display()),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(()),
        Err(error) => {
            Err(error).with_context(|| format!("failed to inspect tool path {}", path.display()))
        }
    }
}

fn matches_checksum(path: &Path, expected: &str) -> Result<bool> {
    match fs::symlink_metadata(path) {
        Ok(metadata) if metadata.file_type().is_symlink() || !metadata.is_file() => Ok(false),
        Ok(metadata) => {
            if !is_executable(&metadata) {
                return Ok(false);
            }
            Ok(sha256_file(path)? == expected)
        }
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(false),
        Err(error) => Err(error).with_context(|| format!("failed to inspect {}", path.display())),
    }
}

fn installed_checksum_status(path: &Path, expected: &str) -> Result<Option<&'static str>> {
    match fs::symlink_metadata(path) {
        Ok(metadata) if metadata.file_type().is_symlink() || !metadata.is_file() => {
            Ok(Some("corrupt"))
        }
        Ok(metadata) if !is_executable(&metadata) => Ok(Some("corrupt")),
        Ok(_) if sha256_file(path)? != expected => Ok(Some("outdated")),
        Ok(_) => Ok(None),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(Some("missing")),
        Err(error) => Err(error).with_context(|| format!("failed to inspect {}", path.display())),
    }
}

fn is_executable(metadata: &fs::Metadata) -> bool {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt as _;
        metadata.permissions().mode() & 0o111 != 0
    }
    #[cfg(not(unix))]
    {
        let _ = metadata;
        true
    }
}

fn read_regular_file(path: &Path) -> Result<Vec<u8>> {
    let metadata = fs::symlink_metadata(path)
        .with_context(|| format!("failed to inspect {}", path.display()))?;
    ensure!(
        metadata.is_file() && !metadata.file_type().is_symlink(),
        "{} is not a regular file",
        path.display()
    );
    fs::read(path).with_context(|| format!("failed to read {}", path.display()))
}

fn sha256_file(path: &Path) -> Result<String> {
    let metadata = fs::symlink_metadata(path)
        .with_context(|| format!("failed to inspect {}", path.display()))?;
    ensure!(
        metadata.is_file() && !metadata.file_type().is_symlink(),
        "{} is not a regular file",
        path.display()
    );
    for (program, args) in [("sha256sum", vec!["-"]), ("shasum", vec!["-a", "256", "-"])] {
        let file =
            File::open(path).with_context(|| format!("failed to open {}", path.display()))?;
        match Command::new(program)
            .args(args)
            .stdin(Stdio::from(file))
            .output()
        {
            Ok(output) if output.status.success() => {
                let text = String::from_utf8_lossy(&output.stdout);
                let hash = text
                    .split_whitespace()
                    .next()
                    .unwrap_or_default()
                    .to_ascii_lowercase();
                validate_sha256(&hash).context("SHA-256 utility returned an invalid digest")?;
                return Ok(hash);
            }
            Ok(_) => continue,
            Err(error) if error.kind() == io::ErrorKind::NotFound => continue,
            Err(error) => return Err(error).context(format!("failed to run {program}")),
        }
    }
    bail!("no SHA-256 utility is available (install sha256sum or shasum)")
}

fn validate_sha256(hash: &str) -> Result<()> {
    ensure!(
        hash.len() == 64 && hash.bytes().all(|byte| byte.is_ascii_hexdigit()),
        "invalid SHA-256 value"
    );
    Ok(())
}

fn safe_relative_path(value: &str) -> bool {
    let path = Path::new(value);
    !path.as_os_str().is_empty()
        && !path.is_absolute()
        && path
            .components()
            .all(|component| matches!(component, Component::Normal(_)))
        && value
            .split('/')
            .all(|part| !part.is_empty() && part != "." && part != "..")
        && !value.contains('\\')
        && !value.contains(':')
}

fn normalize_archive_path(value: &str) -> Result<String> {
    let value = if matches!(value, "." | "./") {
        "."
    } else {
        value.strip_prefix("./").unwrap_or(value)
    };
    let normalized = value.strip_suffix('/').unwrap_or(value);
    ensure!(
        normalized == "." || safe_relative_path(normalized),
        "unsafe archive path {value:?}"
    );
    Ok(normalized.to_string())
}

fn safe_tool_name(value: &str) -> bool {
    !value.is_empty()
        && !value.contains('/')
        && !value.contains('\\')
        && !matches!(value, "." | "..")
}

fn validate_executable(bytes: &[u8], name: &str, architecture: &str) -> Result<()> {
    let expected_arch = architecture
        .strip_suffix("-unknown-linux-gnu")
        .unwrap_or_default();
    validate_elf_architecture(bytes, expected_arch, name)?;
    match name {
        "gantry" | "run-remote" => validate_gnu_elf(bytes, name, expected_arch),
        "baffle" => validate_gnu_loader(bytes, name, expected_arch),
        _ => Ok(()),
    }
}

fn validate_elf_architecture(bytes: &[u8], expected_arch: &str, name: &str) -> Result<()> {
    ensure!(
        bytes.len() >= 64
            && &bytes[..4] == b"\x7fELF"
            && bytes[4] == 2
            && bytes[5] == 1
            && bytes[6] == 1
            && matches!(bytes[7], 0 | 3),
        "{name} is not a 64-bit little-endian Linux ELF executable"
    );
    let elf_type = u16::from_le_bytes([bytes[16], bytes[17]]);
    ensure!(
        matches!(elf_type, 2 | 3) && read_u32(bytes, 20) == Some(1),
        "{name} is not a valid executable Linux ELF file"
    );
    let machine = u16::from_le_bytes([bytes[18], bytes[19]]);
    let actual = match machine {
        62 => "x86_64",
        183 => "aarch64",
        _ => "unsupported",
    };
    ensure!(
        actual == expected_arch,
        "{name} contains a {actual} Linux executable, but the selected Podman guest requires {expected_arch}"
    );
    Ok(())
}

fn validate_gnu_loader(bytes: &[u8], name: &str, arch: &str) -> Result<()> {
    let loader = loader_for(arch)?;
    ensure!(
        elf_interpreter(bytes).is_some_and(|value| value.ends_with(loader)),
        "{name} does not use the {loader} GNU libc loader required by the selected Linux runtime"
    );
    Ok(())
}

fn validate_gnu_elf(bytes: &[u8], name: &str, arch: &str) -> Result<()> {
    validate_gnu_loader(bytes, name, arch)?;
    let max = bytes
        .split(|byte| *byte == 0)
        .filter_map(|part| {
            let value = std::str::from_utf8(part).ok()?.strip_prefix("GLIBC_")?;
            let (major, minor) = value.split_once('.')?;
            Some((major.parse::<u16>().ok()?, minor.parse::<u16>().ok()?))
        })
        .max();
    let Some(required) = max else {
        bail!("{name} has no GNU libc symbol-version requirements to validate");
    };
    ensure!(
        required <= GANTRY_MAX_GLIBC,
        "{name} requires GLIBC_{}.{} but pinned Gantry supports at most GLIBC_{}.{}",
        required.0,
        required.1,
        GANTRY_MAX_GLIBC.0,
        GANTRY_MAX_GLIBC.1
    );
    Ok(())
}

fn loader_for(arch: &str) -> Result<&'static str> {
    match arch {
        "x86_64" => Ok("ld-linux-x86-64.so.2"),
        "aarch64" => Ok("ld-linux-aarch64.so.1"),
        _ => bail!("unsupported Linux executable architecture {arch}"),
    }
}

fn elf_interpreter(bytes: &[u8]) -> Option<String> {
    let program_offset = read_u64(bytes, 32)? as usize;
    let entry_size = read_u16(bytes, 54)? as usize;
    let count = read_u16(bytes, 56)? as usize;
    for index in 0..count {
        let header = program_offset.checked_add(index.checked_mul(entry_size)?)?;
        if entry_size < 56 || header.checked_add(56)? > bytes.len() {
            return None;
        }
        if read_u32(bytes, header)? != 3 {
            continue;
        }
        let offset = read_u64(bytes, header + 8)? as usize;
        let size = read_u64(bytes, header + 32)? as usize;
        let interpreter = bytes
            .get(offset..offset.checked_add(size)?)?
            .split(|byte| *byte == 0)
            .next()?;
        return Some(String::from_utf8_lossy(interpreter).into_owned());
    }
    None
}

fn read_u16(bytes: &[u8], offset: usize) -> Option<u16> {
    let value = bytes.get(offset..offset.checked_add(2)?)?;
    Some(u16::from_le_bytes([value[0], value[1]]))
}

fn read_u32(bytes: &[u8], offset: usize) -> Option<u32> {
    let value = bytes.get(offset..offset.checked_add(4)?)?;
    Some(u32::from_le_bytes([value[0], value[1], value[2], value[3]]))
}

fn read_u64(bytes: &[u8], offset: usize) -> Option<u64> {
    let value = bytes.get(offset..offset.checked_add(8)?)?;
    Some(u64::from_le_bytes(value.try_into().ok()?))
}

fn set_executable_mode(path: &Path) -> Result<()> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt as _;
        fs::set_permissions(path, fs::Permissions::from_mode(0o755))
            .with_context(|| format!("failed to set executable mode on {}", path.display()))?;
    }
    Ok(())
}

fn set_document_mode(path: &Path) -> Result<()> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt as _;
        fs::set_permissions(path, fs::Permissions::from_mode(0o644))
            .with_context(|| format!("failed to set document mode on {}", path.display()))?;
    }
    Ok(())
}

fn read_development_manifest(project_root: &Path) -> Result<Option<DevelopmentManifest>> {
    let path = project_root.join("tools").join(DEVELOPMENT_MANIFEST);
    let bytes = match read_regular_file(&path) {
        Ok(bytes) => bytes,
        Err(error)
            if error
                .downcast_ref::<io::Error>()
                .is_some_and(|error| error.kind() == io::ErrorKind::NotFound) =>
        {
            return Ok(None);
        }
        Err(error) => return Err(error),
    };
    let manifest: DevelopmentManifest =
        serde_json::from_slice(&bytes).context("failed to parse development tool metadata")?;
    ensure!(
        manifest.schema_version == 1,
        "unsupported development tool metadata schema"
    );
    ensure!(
        manifest.architecture == LINUX_ARCHITECTURE,
        "development tool metadata targets {}, expected {LINUX_ARCHITECTURE}",
        manifest.architecture
    );
    ensure!(
        manifest
            .executables
            .keys()
            .map(String::as_str)
            .collect::<BTreeSet<_>>()
            == BTreeSet::from(["baffle", "cladding-bridge", "gantry", "run-remote"]),
        "development tool metadata must pin all four executables"
    );
    for hash in manifest.executables.values() {
        validate_sha256(hash)?;
    }
    Ok(Some(manifest))
}

fn write_development_manifest(tools_dir: &Path, manifest: DevelopmentManifest) -> Result<()> {
    let path = tools_dir.join(DEVELOPMENT_MANIFEST);
    ensure_replaceable(&path)?;
    let staging = StagingDirectory(create_staging_directory(tools_dir)?);
    let staged_path = staging.0.join(DEVELOPMENT_MANIFEST);
    fs::write(&staged_path, serde_json::to_vec_pretty(&manifest)?)
        .with_context(|| format!("failed to write {}", staged_path.display()))?;
    fs::rename(&staged_path, &path)
        .with_context(|| format!("failed to update {}", path.display()))?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::PermissionsExt as _;
    use std::sync::atomic::{AtomicU64, Ordering};

    static TEST_DIRECTORY_ID: AtomicU64 = AtomicU64::new(0);

    #[test]
    fn rejects_archive_path_traversal_and_non_tool_names() {
        assert!(!safe_relative_path("../baffle"));
        assert!(!safe_relative_path("/etc/passwd"));
        assert!(!safe_relative_path("tools\\baffle"));
        assert!(!safe_tool_name("../baffle"));
        assert!(safe_relative_path("package/bin/tool"));
        assert!(safe_tool_name("run-remote"));
        assert_eq!(
            normalize_archive_path("./package/bin/tool").unwrap(),
            "package/bin/tool"
        );
        assert_eq!(normalize_archive_path("./").unwrap(), ".");
        assert!(normalize_archive_path("./../baffle").is_err());
        assert!(normalize_archive_path("././baffle").is_err());
    }

    #[test]
    fn validates_linux_elf_architecture_and_gnu_loader() {
        let x86 = gnu_elf(62, "/lib64/ld-linux-x86-64.so.2", Some("GLIBC_2.35"));
        assert!(validate_executable(&x86, "gantry", "x86_64-unknown-linux-gnu").is_ok());
        assert!(validate_executable(&x86, "gantry", "aarch64-unknown-linux-gnu").is_err());
        let too_new = gnu_elf(62, "/lib64/ld-linux-x86-64.so.2", Some("GLIBC_2.36"));
        assert!(validate_executable(&too_new, "gantry", "x86_64-unknown-linux-gnu").is_err());
        let baffle = gnu_elf(183, "/lib/ld-linux-aarch64.so.1", None);
        assert!(validate_executable(&baffle, "baffle", "aarch64-unknown-linux-gnu").is_ok());

        let mut non_executable = basic_elf(machine_for_arch());
        non_executable[16..18].copy_from_slice(&1u16.to_le_bytes());
        assert!(
            validate_executable(&non_executable, "cladding-bridge", LINUX_ARCHITECTURE).is_err()
        );
    }

    #[test]
    fn uses_linux_guest_architecture_for_the_current_target() {
        let expected = match std::env::consts::ARCH {
            "x86_64" => "x86_64-unknown-linux-gnu",
            "aarch64" => "aarch64-unknown-linux-gnu",
            other => panic!("unsupported Cladding target architecture {other}"),
        };
        assert_eq!(LINUX_ARCHITECTURE, expected);
    }

    #[test]
    fn installer_reuses_matching_files_and_downloads_gantry_pair_once() {
        let temp = test_directory("install-reuse");
        let bin_dir = temp.join("tools/bin");
        fs::create_dir_all(&bin_dir).unwrap();
        let source = temp.join("archives");
        fs::create_dir_all(&source).unwrap();
        let manifest = fixture_manifest(&source);
        let log = temp.join("curl.log");
        let curl = fake_curl(&temp, &source, &log);

        install_release_tools_with(&manifest, &bin_dir, &curl).unwrap();
        assert_eq!(fs::read_to_string(&log).unwrap().lines().count(), 3);
        for name in ["cladding-bridge", "gantry", "run-remote", "baffle"] {
            assert!(
                matches_checksum(
                    &bin_dir.join(name),
                    &expected_executables(&manifest).unwrap()[name]
                )
                .unwrap()
            );
        }

        fs::remove_file(&log).unwrap();
        let no_network = temp.join("no-network-curl");
        fs::write(&no_network, "#!/bin/sh\nexit 91\n").unwrap();
        fs::set_permissions(&no_network, fs::Permissions::from_mode(0o755)).unwrap();
        install_release_tools_with(&manifest, &bin_dir, &no_network).unwrap();
        assert!(!log.exists(), "matching tools must not download archives");

        let gantry_before = fs::read(bin_dir.join("gantry")).unwrap();
        fs::write(bin_dir.join("run-remote"), b"outdated").unwrap();
        fs::set_permissions(
            bin_dir.join("run-remote"),
            fs::Permissions::from_mode(0o755),
        )
        .unwrap();
        install_release_tools_with(&manifest, &bin_dir, &curl).unwrap();
        let urls = fs::read_to_string(&log).unwrap();
        assert_eq!(
            urls.lines().count(),
            1,
            "Gantry tools share one archive download"
        );
        assert!(urls.contains("gantry-0.4.8-linux-"));
        assert_eq!(fs::read(bin_dir.join("gantry")).unwrap(), gantry_before);
        assert!(
            matches_checksum(
                &bin_dir.join("run-remote"),
                &expected_executables(&manifest).unwrap()["run-remote"]
            )
            .unwrap()
        );
        fs::remove_dir_all(temp).unwrap();
    }

    #[test]
    fn installer_extracts_baffle_from_upstream_dot_slash_archive_path() {
        let temp = test_directory("install-baffle-dot-slash");
        let bin_dir = temp.join("tools/bin");
        fs::create_dir_all(&bin_dir).unwrap();
        let source = temp.join("archives");
        fs::create_dir_all(&source).unwrap();
        let manifest = fixture_manifest(&source);
        let baffle = manifest
            .platforms
            .get(LINUX_ARCHITECTURE)
            .unwrap()
            .iter()
            .find(|record| record.name == "baffle")
            .unwrap();
        assert_eq!(baffle.executables[0].archive_path, "./baffle");

        install_release_tools_with(
            &manifest,
            &bin_dir,
            &fake_curl(&temp, &source, &temp.join("baffle-dot-slash.log")),
        )
        .unwrap();

        assert!(matches_checksum(&bin_dir.join("baffle"), &baffle.executables[0].sha256).unwrap());
        fs::remove_dir_all(temp).unwrap();
    }

    #[test]
    fn installer_rejects_duplicate_archive_names_after_prefix_normalization() {
        let temp = test_directory("install-duplicate-dot-slash");
        let bin_dir = temp.join("tools/bin");
        fs::create_dir_all(&bin_dir).unwrap();
        let source = temp.join("archives");
        fs::create_dir_all(&source).unwrap();
        let mut manifest = fixture_manifest(&source);
        let bridge = manifest
            .platforms
            .get_mut(LINUX_ARCHITECTURE)
            .unwrap()
            .iter_mut()
            .find(|record| record.name == "cladding-bridge")
            .unwrap();
        let archive = source.join(&bridge.archive_filename);
        let bridge_dir = temp.join("duplicate-bridge-a");
        let duplicate_dir = temp.join("duplicate-bridge-b");
        fs::create_dir_all(&bridge_dir).unwrap();
        fs::create_dir_all(&duplicate_dir).unwrap();
        let bytes = basic_elf(machine_for_arch());
        fs::write(bridge_dir.join("cladding-bridge"), &bytes).unwrap();
        fs::write(duplicate_dir.join("cladding-bridge"), bytes).unwrap();
        let status = Command::new("tar")
            .arg("-czf")
            .arg(&archive)
            .arg("-C")
            .arg(&bridge_dir)
            .arg("cladding-bridge")
            .arg("-C")
            .arg(&duplicate_dir)
            .arg("./cladding-bridge")
            .status()
            .expect("run tar for duplicate archive fixture");
        assert!(status.success(), "create duplicate archive fixture");
        bridge.archive_sha256 = sha256_file(&archive).unwrap();

        let error = install_release_tools_with(
            &manifest,
            &bin_dir,
            &fake_curl(&temp, &source, &temp.join("duplicate-dot-slash.log")),
        )
        .unwrap_err()
        .to_string();
        assert!(error.contains("duplicate archive entry"), "{error}");
        assert!(!bin_dir.join("cladding-bridge").exists());
        fs::remove_dir_all(temp).unwrap();
    }

    #[test]
    fn installer_validates_all_archives_before_replacing_any_tool() {
        let temp = test_directory("install-atomic");
        let bin_dir = temp.join("tools/bin");
        fs::create_dir_all(&bin_dir).unwrap();
        let source = temp.join("archives");
        fs::create_dir_all(&source).unwrap();
        let mut manifest = fixture_manifest(&source);
        install_release_tools_with(
            &manifest,
            &bin_dir,
            &fake_curl(&temp, &source, &temp.join("initial.log")),
        )
        .unwrap();

        let original_gantry = fs::read(bin_dir.join("gantry")).unwrap();
        let original_bridge = fs::read(bin_dir.join("cladding-bridge")).unwrap();
        fs::write(bin_dir.join("run-remote"), b"old but intact").unwrap();
        fs::set_permissions(
            bin_dir.join("run-remote"),
            fs::Permissions::from_mode(0o755),
        )
        .unwrap();
        let original_run_remote = fs::read(bin_dir.join("run-remote")).unwrap();
        let bridge = manifest
            .platforms
            .get_mut(LINUX_ARCHITECTURE)
            .unwrap()
            .iter_mut()
            .find(|record| record.name == "cladding-bridge")
            .unwrap();
        bridge.archive_sha256 = "0".repeat(64);
        bridge.executables[0].sha256 = "f".repeat(64);
        manifest
            .platforms
            .get_mut(LINUX_ARCHITECTURE)
            .unwrap()
            .sort_by_key(|record| match record.name.as_str() {
                "gantry" => 0,
                "cladding-bridge" => 1,
                _ => 2,
            });

        let result = install_release_tools_with(
            &manifest,
            &bin_dir,
            &fake_curl(&temp, &source, &temp.join("failed.log")),
        );
        assert!(result.is_err());
        assert_eq!(fs::read(bin_dir.join("gantry")).unwrap(), original_gantry);
        assert_eq!(
            fs::read(bin_dir.join("run-remote")).unwrap(),
            original_run_remote
        );
        assert_eq!(
            fs::read(bin_dir.join("cladding-bridge")).unwrap(),
            original_bridge
        );
        fs::remove_dir_all(temp).unwrap();
    }

    #[test]
    fn installer_rejects_unexpected_archive_entries_before_extraction() {
        let temp = test_directory("install-unexpected");
        let bin_dir = temp.join("tools/bin");
        fs::create_dir_all(&bin_dir).unwrap();
        let source = temp.join("archives");
        fs::create_dir_all(&source).unwrap();
        let mut manifest = fixture_manifest(&source);

        let record = manifest
            .platforms
            .get_mut(LINUX_ARCHITECTURE)
            .unwrap()
            .iter_mut()
            .find(|record| record.name == "cladding-bridge")
            .unwrap();
        let archive = source.join(&record.archive_filename);
        let bridge_dir = temp.join("malicious-bridge");
        fs::create_dir_all(&bridge_dir).unwrap();
        fs::write(
            bridge_dir.join("cladding-bridge"),
            basic_elf(machine_for_arch()),
        )
        .unwrap();
        fs::write(bridge_dir.join("unexpected"), b"unexpected").unwrap();
        create_tar(&bridge_dir, &archive, &["cladding-bridge", "unexpected"]);
        record.archive_sha256 = sha256_file(&archive).unwrap();

        let result = install_release_tools_with(
            &manifest,
            &bin_dir,
            &fake_curl(&temp, &source, &temp.join("unexpected.log")),
        );
        assert!(
            result
                .unwrap_err()
                .to_string()
                .contains("unexpected archive entry")
        );
        assert!(!bin_dir.join("cladding-bridge").exists());
        fs::remove_dir_all(temp).unwrap();
    }

    #[test]
    fn development_overrides_install_at_runtime_after_architecture_validation() {
        let temp = test_directory("development-overrides");
        let tools_dir = temp.join("tools");
        let bin_dir = tools_dir.join("bin");
        fs::create_dir_all(&bin_dir).unwrap();
        let local = temp.join("local");
        let gantry_dir = local.join("gantry");
        fs::create_dir_all(&gantry_dir).unwrap();
        let machine = machine_for_arch();
        let arch = LINUX_ARCHITECTURE
            .strip_suffix("-unknown-linux-gnu")
            .unwrap();
        let loader = loader_for(arch).unwrap();
        let files = [
            ("cladding-bridge", basic_elf(machine)),
            (
                "gantry",
                gnu_elf(machine, &format!("/lib/{loader}"), Some("GLIBC_2.35")),
            ),
            (
                "run-remote",
                gnu_elf(machine, &format!("/lib/{loader}"), Some("GLIBC_2.35")),
            ),
            ("baffle", gnu_elf(machine, &format!("/lib/{loader}"), None)),
        ];
        for (name, bytes) in &files {
            let directory = if *name == "gantry" || *name == "run-remote" {
                &gantry_dir
            } else {
                &local
            };
            fs::write(directory.join(name), bytes).unwrap();
        }
        let overrides = DevelopmentOverrides {
            bridge: local.join("cladding-bridge"),
            gantry: gantry_dir.join("gantry"),
            run_remote: gantry_dir.join("run-remote"),
            baffle: local.join("baffle"),
            gantry_dir: gantry_dir.clone(),
        };

        install_development_tools(&overrides, &tools_dir, &bin_dir).unwrap();
        for (name, bytes) in files {
            assert_eq!(fs::read(bin_dir.join(name)).unwrap(), bytes);
            assert!(matches_checksum(&bin_dir.join(name), &sha256_bytes(&bytes)).unwrap());
        }
        assert!(read_development_manifest(&temp).unwrap().is_some());
        fs::remove_dir_all(temp).unwrap();
    }

    fn gnu_elf(machine: u16, loader: &str, glibc: Option<&str>) -> Vec<u8> {
        let mut bytes = vec![0; 320];
        bytes[..4].copy_from_slice(b"\x7fELF");
        bytes[4] = 2;
        bytes[5] = 1;
        bytes[6] = 1;
        bytes[7] = 0;
        bytes[18..20].copy_from_slice(&machine.to_le_bytes());
        bytes[16..18].copy_from_slice(&3u16.to_le_bytes());
        bytes[20..24].copy_from_slice(&1u32.to_le_bytes());
        bytes[32..40].copy_from_slice(&64u64.to_le_bytes());
        bytes[54..56].copy_from_slice(&56u16.to_le_bytes());
        bytes[56..58].copy_from_slice(&1u16.to_le_bytes());
        bytes[64..68].copy_from_slice(&3u32.to_le_bytes());
        bytes[72..80].copy_from_slice(&160u64.to_le_bytes());
        bytes[96..104].copy_from_slice(&((loader.len() + 1) as u64).to_le_bytes());
        bytes[160..160 + loader.len()].copy_from_slice(loader.as_bytes());
        if let Some(glibc) = glibc {
            bytes[224..224 + glibc.len()].copy_from_slice(glibc.as_bytes());
        }
        bytes
    }

    fn fixture_manifest(source: &Path) -> Manifest {
        let arch = LINUX_ARCHITECTURE
            .strip_suffix("-unknown-linux-gnu")
            .unwrap();
        let bridge_bytes = basic_elf(machine_for_arch());
        let bridge_source = source.join("bridge-source");
        fs::create_dir_all(&bridge_source).unwrap();
        fs::write(bridge_source.join("cladding-bridge"), &bridge_bytes).unwrap();
        let bridge_archive_name = format!("cladding-bridge-v0.4.8-{LINUX_ARCHITECTURE}.tar.gz");
        let bridge_archive = source.join(&bridge_archive_name);

        let gantry_root = format!("gantry-0.4.8-linux-{arch}-gnu");
        let gantry_source = source.join(&gantry_root);
        fs::create_dir_all(&gantry_source).unwrap();
        let loader = loader_for(arch).unwrap();
        let gantry_bytes = gnu_elf(
            machine_for_arch(),
            &format!("/lib/{loader}"),
            Some("GLIBC_2.35"),
        );
        let remote_bytes = gnu_elf(
            machine_for_arch(),
            &format!("/lib/{loader}"),
            Some("GLIBC_2.35"),
        );
        fs::write(gantry_source.join("gantry"), &gantry_bytes).unwrap();
        fs::write(gantry_source.join("run-remote"), &remote_bytes).unwrap();
        let gantry_archive_name = format!("gantry-0.4.8-linux-{arch}-gnu.tar.gz");
        let gantry_archive = source.join(&gantry_archive_name);
        let gantry_executable_path = format!("{gantry_root}/gantry");
        let run_remote_path = format!("{gantry_root}/run-remote");

        let baffle_source = source.join("baffle-source");
        fs::create_dir_all(&baffle_source).unwrap();
        let baffle_bytes = gnu_elf(machine_for_arch(), &format!("/lib/{loader}"), None);
        fs::write(baffle_source.join("baffle"), &baffle_bytes).unwrap();
        let baffle_archive_name = format!("baffle-proxy-v2.2.0-{arch}-unknown-linux-gnu.tar.gz");
        let baffle_archive = source.join(&baffle_archive_name);

        let release_tag = "v0.4.8".to_string();
        let records = vec![
            artifact_record(
                "cladding-bridge",
                &release_tag,
                &bridge_archive,
                &bridge_source,
                &["cladding-bridge"],
                vec![("cladding-bridge", "cladding-bridge", &bridge_bytes)],
            ),
            artifact_record(
                "gantry",
                "v0.4.8",
                &gantry_archive,
                source,
                &[&gantry_root],
                vec![
                    (gantry_executable_path.as_str(), "gantry", &gantry_bytes),
                    (run_remote_path.as_str(), "run-remote", &remote_bytes),
                ],
            ),
            artifact_record(
                "baffle",
                "v2.2.0",
                &baffle_archive,
                &baffle_source,
                &["./baffle"],
                vec![("./baffle", "baffle", &baffle_bytes)],
            ),
        ];
        Manifest {
            schema_version: 1,
            cladding_release: Some(CladdingRelease {
                version: "0.4.8".into(),
                tag: release_tag,
            }),
            platforms: HashMap::from([(LINUX_ARCHITECTURE.into(), records)]),
        }
    }

    fn artifact_record(
        name: &str,
        release_tag: &str,
        archive_path: &Path,
        archive_source: &Path,
        archive_sources: &[&str],
        executable_sources: Vec<(&str, &str, &[u8])>,
    ) -> Artifact {
        create_tar(archive_source, archive_path, archive_sources);
        let listed = run_tar_list(archive_path, false).unwrap();
        let detailed = run_tar_list(archive_path, true).unwrap();
        let archive_entries = listed
            .iter()
            .zip(detailed.iter())
            .map(|(path, detail)| ArchiveEntry {
                path: normalize_archive_path(path).unwrap(),
                kind: if detail.starts_with('d') {
                    "directory"
                } else {
                    "file"
                }
                .to_string(),
            })
            .collect();
        let executables = executable_sources
            .into_iter()
            .map(|(archive_path, install_path, bytes)| Executable {
                archive_path: archive_path.to_string(),
                install_path: install_path.to_string(),
                sha256: sha256_bytes(bytes),
            })
            .collect();
        let source_repository = match name {
            "cladding-bridge" => "dstoc/cladding",
            "gantry" => "dstoc/gantry",
            "baffle" => "dstoc/baffle",
            _ => panic!("unknown fixture artifact {name}"),
        };
        Artifact {
            name: name.into(),
            source_repository: source_repository.into(),
            release_tag: release_tag.into(),
            archive_filename: archive_path
                .file_name()
                .unwrap()
                .to_string_lossy()
                .into_owned(),
            architecture: LINUX_ARCHITECTURE.into(),
            archive_sha256: sha256_file(archive_path).unwrap(),
            archive_entries,
            executables,
        }
    }

    fn create_tar(source: &Path, archive: &Path, entries: &[&str]) {
        let status = Command::new("tar")
            .arg("-czf")
            .arg(archive)
            .arg("-C")
            .arg(source)
            .args(entries)
            .status()
            .expect("run tar for fixture archive");
        assert!(status.success(), "create fixture archive");
    }

    fn fake_curl(directory: &Path, archives: &Path, log: &Path) -> PathBuf {
        let script = directory.join(format!(
            "curl-{}",
            TEST_DIRECTORY_ID.fetch_add(1, Ordering::Relaxed)
        ));
        let quote = |path: &Path| format!("'{}'", path.display());
        let contents = format!(
            "#!/bin/sh\nset -eu\noutput=''\nurl=''\nwhile [ \"$#\" -gt 0 ]; do\n  case \"$1\" in\n    --output) output=$2; shift 2 ;;\n    *) url=$1; shift ;;\n  esac\ndone\ncp {}\"/${{url##*/}}\" \"$output\"\nprintf '%s\\n' \"$url\" >> {}\n",
            quote(archives),
            quote(log)
        );
        fs::write(&script, contents).unwrap();
        fs::set_permissions(&script, fs::Permissions::from_mode(0o755)).unwrap();
        script
    }

    fn test_directory(name: &str) -> PathBuf {
        let path = std::env::temp_dir().join(format!(
            "cladding-dependencies-{name}-{}-{}",
            std::process::id(),
            TEST_DIRECTORY_ID.fetch_add(1, Ordering::Relaxed)
        ));
        if path.exists() {
            fs::remove_dir_all(&path).unwrap();
        }
        fs::create_dir_all(&path).unwrap();
        path
    }

    fn machine_for_arch() -> u16 {
        match LINUX_ARCHITECTURE {
            "x86_64-unknown-linux-gnu" => 62,
            "aarch64-unknown-linux-gnu" => 183,
            other => panic!("unsupported test architecture {other}"),
        }
    }

    fn basic_elf(machine: u16) -> Vec<u8> {
        let mut bytes = vec![0; 64];
        bytes[..4].copy_from_slice(b"\x7fELF");
        bytes[4] = 2;
        bytes[5] = 1;
        bytes[6] = 1;
        bytes[7] = 0;
        bytes[16..18].copy_from_slice(&3u16.to_le_bytes());
        bytes[18..20].copy_from_slice(&machine.to_le_bytes());
        bytes[20..24].copy_from_slice(&1u32.to_le_bytes());
        bytes
    }

    fn sha256_bytes(bytes: &[u8]) -> String {
        let path = test_directory("hash-bytes").join("input");
        fs::write(&path, bytes).unwrap();
        let hash = sha256_file(&path).unwrap();
        fs::remove_dir_all(path.parent().unwrap()).unwrap();
        hash
    }
}
