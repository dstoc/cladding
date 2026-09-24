use crate::assets::containerfile as embedded_containerfile;
use crate::error::{Error, Result};
use anyhow::Context as _;
use std::collections::BTreeMap;
use std::fs::{self, OpenOptions};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicUsize, Ordering};

use super::command::ensure_success;

static IGNORE_FILE_COUNTER: AtomicUsize = AtomicUsize::new(0);

pub fn podman_build_image(
    image: &str,
    containerfile: Option<&Path>,
    context: &Path,
    build_args: &BTreeMap<String, String>,
    secret_dirs: &[PathBuf],
    host_ids: Option<(u32, u32)>,
) -> Result<()> {
    if let Some(containerfile) = containerfile {
        if !containerfile.is_file() {
            eprintln!(
                "error: Containerfile does not exist: {}",
                containerfile.display()
            );
            return Err(Error::message("missing Containerfile"));
        }
        let containerfile_path = fs::canonicalize(containerfile).with_context(|| {
            format!(
                "failed to resolve Containerfile {}",
                containerfile.display()
            )
        })?;
        for secret_dir in secret_dirs {
            let secret_path = absolute_path(secret_dir)?;
            let secret_path = fs::canonicalize(secret_dir).unwrap_or(secret_path);
            if containerfile_path.starts_with(secret_path) {
                eprintln!("error: Containerfile is inside a configured secret directory");
                return Err(Error::message("Containerfile overlaps secret directory"));
            }
        }
    }
    let ignore_file = BuildIgnoreFile::create(context, secret_dirs)?;
    let mut cmd = build_command(
        image,
        containerfile,
        context,
        build_args,
        host_ids,
        &ignore_file.path,
    );

    let mut child = cmd.spawn().with_context(|| "failed to run podman build")?;
    if containerfile.is_none()
        && let Some(mut stdin) = child.stdin.take()
    {
        stdin
            .write_all(embedded_containerfile().as_bytes())
            .and_then(|_| stdin.flush())
            .with_context(|| "failed to write Containerfile to podman")?;
    }

    let status = child
        .wait()
        .with_context(|| "failed to wait on podman build")?;

    ensure_success(status, "podman build")
}

fn build_command(
    image: &str,
    containerfile: Option<&Path>,
    context: &Path,
    build_args: &BTreeMap<String, String>,
    host_ids: Option<(u32, u32)>,
    ignore_file: &Path,
) -> Command {
    let mut cmd = Command::new("podman");
    cmd.arg("build").arg("--ignorefile").arg(ignore_file);
    if let Some((host_uid, host_gid)) = host_ids {
        cmd.arg("--build-arg")
            .arg(format!("UID={host_uid}"))
            .arg("--build-arg")
            .arg(format!("GID={host_gid}"));
    }
    for (name, value) in build_args {
        cmd.arg("--build-arg").arg(format!("{name}={value}"));
    }
    cmd.arg("-t").arg(image).arg("-f");
    match containerfile {
        Some(containerfile) => {
            cmd.arg(containerfile);
        }
        None => {
            cmd.arg("-").stdin(Stdio::piped());
        }
    }
    cmd.arg(context);
    cmd
}

struct BuildIgnoreFile {
    path: PathBuf,
}

impl BuildIgnoreFile {
    fn create(context: &Path, secret_dirs: &[PathBuf]) -> Result<Self> {
        let context = fs::canonicalize(context)
            .with_context(|| format!("failed to resolve build context {}", context.display()))?;
        if !context.is_dir() {
            eprintln!(
                "error: build context is not a directory: {}",
                context.display()
            );
            return Err(Error::message("invalid build context"));
        }

        let mut contents = String::new();
        let container_ignore = context.join(".containerignore");
        let docker_ignore = context.join(".dockerignore");
        let existing_ignore = if container_ignore.exists() {
            Some(container_ignore)
        } else if docker_ignore.exists() {
            Some(docker_ignore)
        } else {
            None
        };
        if let Some(path) = existing_ignore {
            contents = fs::read_to_string(&path)
                .with_context(|| format!("failed to read {}", path.display()))?;
            if !contents.is_empty() && !contents.ends_with('\n') {
                contents.push('\n');
            }
        }

        let mut exclusions = Vec::new();
        for secret_dir in secret_dirs {
            exclusions.push(absolute_path(secret_dir)?);
            if let Ok(canonical) = fs::canonicalize(secret_dir) {
                exclusions.push(canonical);
            }
        }
        exclusions.sort();
        exclusions.dedup();

        for secret_dir in exclusions {
            if context == secret_dir || context.starts_with(&secret_dir) {
                eprintln!(
                    "error: build context {} overlaps a configured secret directory {}",
                    context.display(),
                    secret_dir.display()
                );
                return Err(Error::message("build context overlaps secret directory"));
            }
            let Ok(relative) = secret_dir.strip_prefix(&context) else {
                continue;
            };
            let relative = path_to_ignore_pattern(relative)?;
            if relative.is_empty() {
                continue;
            }
            contents.push_str(&relative);
            contents.push('\n');
            contents.push_str(&relative);
            contents.push_str("/**\n");
            contents.push_str("**/");
            contents.push_str(&relative);
            contents.push('\n');
            contents.push_str("**/");
            contents.push_str(&relative);
            contents.push_str("/**\n");
        }

        let id = IGNORE_FILE_COUNTER.fetch_add(1, Ordering::Relaxed);
        let path =
            std::env::temp_dir().join(format!("cladding-build-ignore-{}-{id}", std::process::id()));
        let mut options = OpenOptions::new();
        options.write(true).create_new(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt as _;
            options.mode(0o600);
        }
        let mut file = options
            .open(&path)
            .with_context(|| format!("failed to create build ignore file {}", path.display()))?;
        file.write_all(contents.as_bytes())
            .with_context(|| format!("failed to write build ignore file {}", path.display()))?;
        file.flush()
            .with_context(|| format!("failed to write build ignore file {}", path.display()))?;

        Ok(Self { path })
    }
}

impl Drop for BuildIgnoreFile {
    fn drop(&mut self) {
        let _ = fs::remove_file(&self.path);
    }
}

fn absolute_path(path: &Path) -> Result<PathBuf> {
    if path.is_absolute() {
        Ok(normalize_path(path))
    } else {
        let cwd =
            std::env::current_dir().with_context(|| "failed to determine current directory")?;
        Ok(normalize_path(&cwd.join(path)))
    }
}

fn normalize_path(path: &Path) -> PathBuf {
    let mut normalized = PathBuf::new();
    for component in path.components() {
        match component {
            std::path::Component::CurDir => {}
            std::path::Component::ParentDir => {
                normalized.pop();
            }
            other => normalized.push(other.as_os_str()),
        }
    }
    normalized
}

fn path_to_ignore_pattern(path: &Path) -> Result<String> {
    let mut components = Vec::new();
    for component in path.components() {
        let std::path::Component::Normal(component) = component else {
            eprintln!("error: invalid secret directory path in build context");
            return Err(Error::message("invalid secret directory path"));
        };
        let Some(component) = component.to_str() else {
            eprintln!("error: secret directory path is not valid UTF-8");
            return Err(Error::message("invalid secret directory path"));
        };
        if component.chars().any(|character| {
            matches!(character, '*' | '?' | '[' | ']' | '!' | '#' | '\\') || character.is_control()
        }) {
            eprintln!(
                "error: secret directory path cannot be represented safely in a Podman ignore file"
            );
            return Err(Error::message("invalid secret directory path"));
        }
        components.push(component);
    }
    Ok(components.join("/"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::BTreeMap;
    use std::env;
    use std::sync::atomic::{AtomicUsize, Ordering};

    static TEMP_DIR_COUNTER: AtomicUsize = AtomicUsize::new(0);

    #[test]
    fn build_command_passes_custom_file_context_args_and_default_uid_gid() {
        let args = BTreeMap::from([("FEATURE".to_string(), "enabled".to_string())]);
        let command = build_command(
            "localhost/custom:latest",
            Some(Path::new("/tmp/config/agent.Containerfile")),
            Path::new("/tmp/config/context"),
            &args,
            Some((1001, 1002)),
            Path::new("/tmp/ignore"),
        );

        let args = command
            .get_args()
            .map(|arg| arg.to_string_lossy().into_owned())
            .collect::<Vec<_>>();
        assert_eq!(
            args,
            [
                "build",
                "--ignorefile",
                "/tmp/ignore",
                "--build-arg",
                "UID=1001",
                "--build-arg",
                "GID=1002",
                "--build-arg",
                "FEATURE=enabled",
                "-t",
                "localhost/custom:latest",
                "-f",
                "/tmp/config/agent.Containerfile",
                "/tmp/config/context",
            ]
        );
        assert!(!args.windows(2).any(|pair| pair == ["-f", "-"]));
    }

    #[test]
    fn build_command_embeds_default_containerfile_and_preserves_uid_gid() {
        let command = build_command(
            "localhost/cladding-default:latest",
            None,
            Path::new("/tmp/project"),
            &BTreeMap::new(),
            Some((1000, 1000)),
            Path::new("/tmp/ignore"),
        );
        let args = command
            .get_args()
            .map(|arg| arg.to_string_lossy().into_owned())
            .collect::<Vec<_>>();
        assert!(
            args.windows(2)
                .any(|pair| pair == ["--build-arg", "UID=1000"])
        );
        assert!(
            args.windows(2)
                .any(|pair| pair == ["--build-arg", "GID=1000"])
        );
        assert_eq!(args.last().map(String::as_str), Some("/tmp/project"));
        assert!(args.windows(2).any(|pair| pair == ["-f", "-"]));
    }

    #[test]
    fn build_ignore_file_preserves_existing_rules_and_excludes_secret_directories() {
        let context = create_temp_dir("ignore-file");
        let cladding = context.join(".cladding");
        let credentials = cladding.join("credentials");
        fs::create_dir_all(&credentials).unwrap();
        fs::write(context.join(".dockerignore"), "target/\n").unwrap();

        let ignore = BuildIgnoreFile::create(&context, std::slice::from_ref(&credentials)).unwrap();
        let contents = fs::read_to_string(&ignore.path).unwrap();
        assert!(contents.starts_with("target/\n"));
        assert!(contents.contains(".cladding/credentials\n"));
        assert!(contents.contains(".cladding/credentials/**\n"));
        drop(ignore);
        fs::remove_dir_all(context).unwrap();
    }

    #[test]
    fn build_ignore_file_rejects_secret_contexts() {
        let context = create_temp_dir("secret-context");
        let secret_dir = context.join("credentials");
        fs::create_dir_all(&secret_dir).unwrap();

        assert!(BuildIgnoreFile::create(&secret_dir, std::slice::from_ref(&secret_dir)).is_err());
        fs::remove_dir_all(context).unwrap();
    }

    fn create_temp_dir(name: &str) -> PathBuf {
        let unique = TEMP_DIR_COUNTER.fetch_add(1, Ordering::Relaxed);
        let path = env::temp_dir().join(format!(
            "cladding-build-{name}-{}-{unique}",
            std::process::id()
        ));
        if path.exists() {
            fs::remove_dir_all(&path).unwrap();
        }
        fs::create_dir_all(&path).unwrap();
        path
    }
}
