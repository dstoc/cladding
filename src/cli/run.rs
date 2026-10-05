use super::context::Context;
use super::{exec, lifecycle};
use anyhow::Context as _;
use cladding::error::{Error, Result};
use signal_hook::consts::signal::{SIGINT, SIGTERM};
use signal_hook::iterator::{Handle as SignalHandle, Signals};
use std::collections::HashSet;
use std::ffi::OsString;
use std::fs::{self, File, OpenOptions};
use std::io::{Read as _, Write as _};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicI32, Ordering};
use std::thread::{self, JoinHandle};

pub(super) fn cmd_run(
    source_context: &Context,
    env_vars: &[String],
    secret_overrides: &[String],
    args: &[String],
    config_uses_stdin: bool,
    verbose: bool,
) -> Result<()> {
    if args.is_empty() {
        return Err(Error::message("missing run command"));
    }

    let secret_overrides = resolve_run_secret_overrides(
        secret_overrides,
        |name| std::env::var_os(name),
        read_run_secret_file,
    )?;

    let instance_name = format!("cladding-run-{}", generate_uuid_v4()?);
    if verbose {
        println!("starting one-off instance: {instance_name}");
    }

    let mut config = source_context.load_config().map_err(|err| {
        Error::message(format!(
            "one-off instance '{instance_name}' failed to load config: {err}"
        ))
    })?;
    config.name = instance_name.clone();

    cladding::podman::podman_required("podman (required for cladding run)").map_err(|err| {
        Error::message(format!(
            "one-off instance '{instance_name}' cannot start: {err}"
        ))
    })?;

    let mut signals = SignalWatcher::new().map_err(|err| {
        Error::message(format!(
            "one-off instance '{instance_name}' failed to install signal handlers: {err}"
        ))
    })?;

    let runtime_root = match create_private_runtime_root(
        &instance_name,
        &source_context.workspace_root,
    ) {
        Ok(root) => root,
        Err(err) => {
            let _ = signals.finish();
            return Err(Error::message(format!(
                "one-off instance '{instance_name}' failed to create its private runtime root: {err}"
            )));
        }
    };
    let secret_mounts = match materialize_run_secret_overrides(
        &runtime_root,
        &source_context.project_root,
        &secret_overrides,
    ) {
        Ok(mounts) => mounts,
        Err(setup_error) => {
            let root_cleanup = remove_private_runtime_root(&runtime_root).with_context(|| {
                format!(
                    "failed to remove private runtime root {}",
                    runtime_root.display()
                )
            });
            let signal = signals.finish();
            if let Err(cleanup_error) = root_cleanup {
                report_cleanup_failure(&instance_name, &runtime_root, &Error::from(cleanup_error));
            }
            if let Some(signal) = signal {
                eprintln!(
                    "error: one-off instance '{instance_name}' failed during setup: {setup_error}"
                );
                return Err(signal_status_error(signal));
            }
            return Err(Error::message(format!(
                "one-off instance '{instance_name}' failed during setup: {setup_error}"
            )));
        }
    };
    let context = source_context.with_resolved_config(runtime_root.clone(), config);
    let context = match secret_mounts {
        Some(secret_mounts) => context.with_run_secret_mounts(secret_mounts),
        None => context,
    };

    if let Err(setup_error) = lifecycle::prepare_run_runtime_root(&runtime_root) {
        let root_cleanup = remove_private_runtime_root(&runtime_root).with_context(|| {
            format!(
                "failed to remove private runtime root {}",
                runtime_root.display()
            )
        });
        let signal = signals.finish();
        if let Err(cleanup_error) = root_cleanup {
            report_cleanup_failure(&instance_name, &runtime_root, &Error::from(cleanup_error));
        }
        if let Some(signal) = signal {
            eprintln!(
                "error: one-off instance '{instance_name}' failed during setup: {setup_error}"
            );
            return Err(signal_status_error(signal));
        }
        return Err(Error::message(format!(
            "one-off instance '{instance_name}' failed during setup: {setup_error}"
        )));
    }

    let (startup_result, runtime_create_attempted) = lifecycle::cmd_up_ephemeral(&context, verbose);

    if let Err(startup_error) = startup_result {
        let cleanup_error = if runtime_create_attempted {
            cleanup_ephemeral_runtime(&context, &runtime_root, &instance_name, verbose)
        } else {
            remove_private_runtime_root(&runtime_root).map_err(Error::from)
        };
        let signal = signals.finish();
        if let Err(cleanup_error) = cleanup_error {
            report_cleanup_failure(&instance_name, &runtime_root, &cleanup_error);
        }
        if let Some(signal) = signal {
            eprintln!(
                "error: one-off instance '{instance_name}' stopped during startup: {startup_error}"
            );
            return Err(signal_status_error(signal));
        }
        return Err(Error::message(format!(
            "one-off instance '{instance_name}' failed during startup: {startup_error}"
        )));
    }

    let command_result = match signals.received() {
        Some(signal) => Err(signal_status_error(signal)),
        None => exec::cmd_run_command(&context, env_vars, args, !config_uses_stdin),
    };
    let cleanup_error = cleanup_ephemeral_runtime(&context, &runtime_root, &instance_name, verbose);
    let signal = signals.finish();

    finish_run(
        &instance_name,
        &runtime_root,
        command_result,
        cleanup_error,
        signal,
    )
}

enum RunSecretSource {
    Environment(String),
    File(PathBuf),
}

struct RunSecretSpec {
    name: String,
    source: RunSecretSource,
}

struct ResolvedRunSecret {
    name: String,
    contents: Vec<u8>,
}

fn resolve_run_secret_overrides(
    raw_overrides: &[String],
    read_environment: impl Fn(&str) -> Option<OsString>,
    read_file: impl Fn(&Path) -> std::io::Result<Vec<u8>>,
) -> Result<Vec<ResolvedRunSecret>> {
    let specs = parse_run_secret_overrides(raw_overrides)?;
    specs
        .into_iter()
        .map(|spec| {
            let contents = match spec.source {
                RunSecretSource::Environment(variable) => {
                    let value = read_environment(&variable).ok_or_else(|| {
                        Error::message(format!(
                            "host environment variable '{variable}' for Baffle secret '{}' is not set",
                            spec.name
                        ))
                    })?;
                    value.into_string().map_err(|_| {
                        Error::message(format!(
                            "host environment variable '{variable}' for Baffle secret '{}' is not valid UTF-8",
                            spec.name
                        ))
                    })?.into_bytes()
                }
                RunSecretSource::File(path) => read_file(&path).map_err(|error| {
                    Error::message(format!(
                        "failed to read host file for Baffle secret '{}': {error}",
                        spec.name
                    ))
                })?,
            };
            Ok(ResolvedRunSecret {
                name: spec.name,
                contents,
            })
        })
        .collect()
}

fn read_run_secret_file(path: &Path) -> std::io::Result<Vec<u8>> {
    let mut options = OpenOptions::new();
    options.read(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt as _;
        options.custom_flags(libc::O_NONBLOCK);
    }
    let mut file = options.open(path)?;
    if !file.metadata()?.is_file() {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            "secret source is not a regular file",
        ));
    }
    let mut contents = Vec::new();
    file.read_to_end(&mut contents)?;
    Ok(contents)
}

fn parse_run_secret_overrides(raw_overrides: &[String]) -> Result<Vec<RunSecretSpec>> {
    let mut names = HashSet::new();
    let mut specs = Vec::with_capacity(raw_overrides.len());

    for raw in raw_overrides {
        let Some((name, source)) = raw.split_once('=') else {
            return Err(Error::message(
                "Baffle secret override must use NAME=env:VARIABLE or NAME=file:PATH",
            ));
        };
        if !is_baffle_secret_identifier(name) {
            return Err(Error::message(
                "Baffle secret name must be a simple identifier containing only letters, digits, '.', '_' or '-' and starting with a letter or digit",
            ));
        }
        if !names.insert(name.to_string()) {
            return Err(Error::message(format!(
                "duplicate Baffle secret override for '{name}'"
            )));
        }

        let Some((kind, value)) = source.split_once(':') else {
            return Err(Error::message(
                "Baffle secret source must use env:VARIABLE or file:PATH; literal values are not accepted",
            ));
        };
        let parsed_source = match kind {
            "env" if is_environment_variable_name(value) => {
                RunSecretSource::Environment(value.to_string())
            }
            "file" if !value.is_empty() => RunSecretSource::File(PathBuf::from(value)),
            "env" => {
                return Err(Error::message(
                    "environment-backed Baffle secret source requires a valid variable name",
                ));
            }
            "file" => {
                return Err(Error::message(
                    "file-backed Baffle secret source requires a path",
                ));
            }
            _ => {
                return Err(Error::message(
                    "Baffle secret source must use env:VARIABLE or file:PATH; literal values are not accepted",
                ));
            }
        };
        specs.push(RunSecretSpec {
            name: name.to_string(),
            source: parsed_source,
        });
    }

    Ok(specs)
}

fn is_baffle_secret_identifier(name: &str) -> bool {
    let bytes = name.as_bytes();
    !bytes.is_empty()
        && bytes.len() <= 64
        && bytes[0].is_ascii_alphanumeric()
        && bytes
            .iter()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'.' | b'_' | b'-'))
        && !bytes
            .last()
            .is_some_and(|byte| matches!(byte, b'.' | b'_' | b'-'))
        && !name.contains("..")
}

fn is_environment_variable_name(name: &str) -> bool {
    let mut chars = name.chars();
    matches!(chars.next(), Some(ch) if ch.is_ascii_alphabetic() || ch == '_')
        && chars.all(|ch| ch.is_ascii_alphanumeric() || ch == '_')
}

fn materialize_run_secret_overrides(
    runtime_root: &Path,
    project_root: &Path,
    secrets: &[ResolvedRunSecret],
) -> anyhow::Result<Option<super::context::RunSecretMounts>> {
    if secrets.is_empty() {
        return Ok(None);
    }

    let secret_dir = runtime_root.join("runtime/secrets");
    let runtime_dir = secret_dir
        .parent()
        .expect("runtime secret directory has parent");
    #[cfg(unix)]
    {
        use std::os::unix::fs::{DirBuilderExt as _, PermissionsExt as _};
        for directory in [runtime_dir, secret_dir.as_path()] {
            let mut builder = fs::DirBuilder::new();
            builder.mode(0o700);
            builder.create(directory).with_context(|| {
                format!(
                    "failed to create private run secret directory {}",
                    directory.display()
                )
            })?;
            fs::set_permissions(directory, fs::Permissions::from_mode(0o700)).with_context(
                || {
                    format!(
                        "failed to secure private run secret directory {}",
                        directory.display()
                    )
                },
            )?;
        }
    }
    #[cfg(not(unix))]
    {
        fs::create_dir(runtime_dir).with_context(|| {
            format!(
                "failed to create private run runtime directory {}",
                runtime_dir.display()
            )
        })?;
        fs::create_dir(&secret_dir).with_context(|| {
            format!(
                "failed to create private run secret directory {}",
                secret_dir.display()
            )
        })?;
    }

    let mut overridden = HashSet::with_capacity(secrets.len());
    for secret in secrets {
        let path = secret_dir.join(&secret.name);
        write_private_run_secret(&path, &secret.contents)?;
        overridden.insert(secret.name.as_str());
    }

    let project_secrets_dir = project_root.join("credentials/baffle/secrets");
    let mut project_secrets = Vec::new();
    let mut project_secrets_exist = true;
    for directory in [
        project_root.join("credentials"),
        project_root.join("credentials/baffle"),
        project_secrets_dir.clone(),
    ] {
        match fs::symlink_metadata(&directory) {
            Ok(metadata) if !metadata.file_type().is_symlink() && metadata.is_dir() => {}
            Ok(_) => anyhow::bail!(
                "project Baffle secret path is not a regular directory: {}",
                directory.display()
            ),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                project_secrets_exist = false;
                break;
            }
            Err(error) => {
                return Err(error).with_context(|| {
                    format!(
                        "failed to inspect project Baffle secret directory {}",
                        directory.display()
                    )
                });
            }
        }
    }
    if project_secrets_exist {
        match fs::read_dir(&project_secrets_dir) {
            Ok(entries) => {
                for entry in entries {
                    let entry = entry.with_context(|| {
                        format!(
                            "failed to read project Baffle secret directory {}",
                            project_secrets_dir.display()
                        )
                    })?;
                    let Some(name) = entry.file_name().to_str().map(str::to_string) else {
                        continue;
                    };
                    if !is_baffle_secret_identifier(&name) || overridden.contains(name.as_str()) {
                        continue;
                    }
                    let source = entry.path();
                    let metadata = fs::symlink_metadata(&source).with_context(|| {
                        format!(
                            "failed to inspect project Baffle secret file {}",
                            source.display()
                        )
                    })?;
                    if metadata.file_type().is_symlink() || !metadata.is_file() {
                        anyhow::bail!(
                            "project Baffle secret entry is not a regular file: {}",
                            source.display()
                        );
                    }
                    write_private_run_secret(&secret_dir.join(&name), &[])?;
                    project_secrets.push(super::context::RunSecretMount { name, source });
                }
            }
            Err(error) => {
                return Err(error).with_context(|| {
                    format!(
                        "failed to read project Baffle secret directory {}",
                        project_secrets_dir.display()
                    )
                });
            }
        }
    }

    Ok(Some(super::context::RunSecretMounts {
        directory: secret_dir,
        project_secrets,
    }))
}

fn write_private_run_secret(path: &Path, contents: &[u8]) -> anyhow::Result<()> {
    let mut options = OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt as _;
        options.mode(0o600);
    }
    let mut file = options.open(path).with_context(|| {
        format!(
            "failed to create private run secret file {}",
            path.display()
        )
    })?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt as _;
        file.set_permissions(fs::Permissions::from_mode(0o600))
            .with_context(|| {
                format!(
                    "failed to secure private run secret file {}",
                    path.display()
                )
            })?;
    }
    file.write_all(contents)
        .with_context(|| format!("failed to write private run secret file {}", path.display()))?;
    file.sync_all()
        .with_context(|| format!("failed to sync private run secret file {}", path.display()))?;
    Ok(())
}

fn cleanup_ephemeral_runtime(
    context: &Context,
    runtime_root: &Path,
    instance_name: &str,
    verbose: bool,
) -> Result<()> {
    if verbose {
        eprintln!("cleaning up one-off instance: {instance_name}");
    }
    lifecycle::cmd_down_ephemeral(context, verbose)?;
    remove_private_runtime_root(runtime_root)?;
    Ok(())
}

fn remove_private_runtime_root(runtime_root: &Path) -> anyhow::Result<()> {
    fs::remove_dir_all(runtime_root).with_context(|| {
        format!(
            "failed to remove private runtime root {}",
            runtime_root.display()
        )
    })
}

fn report_cleanup_failure(instance_name: &str, runtime_root: &Path, err: &Error) {
    eprintln!("error: cleanup failed for one-off instance '{instance_name}': {err}");
    eprintln!(
        "manual cleanup may be required; private runtime root: {}",
        runtime_root.display()
    );
}

fn finish_run(
    instance_name: &str,
    runtime_root: &Path,
    command_result: Result<()>,
    cleanup_result: Result<()>,
    signal: Option<i32>,
) -> Result<()> {
    if let Err(err) = &cleanup_result {
        report_cleanup_failure(instance_name, runtime_root, err);
    }

    match command_result {
        Err(err) => Err(preserve_command_status(err, instance_name)),
        Ok(()) => match signal {
            Some(signal) => Err(signal_status_error(signal)),
            None => match cleanup_result {
                Ok(()) => Ok(()),
                Err(err) => Err(Error::message(format!(
                    "cleanup failed for one-off instance '{instance_name}': {err}"
                ))),
            },
        },
    }
}

fn preserve_command_status(err: Error, instance_name: &str) -> Error {
    match err {
        command_error @ Error::CommandFailed { .. }
        | command_error @ Error::CommandFailedWithOutput { .. } => command_error,
        other => Error::message(format!(
            "one-off instance '{instance_name}' failed: {other}"
        )),
    }
}

fn signal_status_error(signal: i32) -> Error {
    Error::CommandFailed {
        context: "cladding run",
        code: 128 + signal,
    }
}

fn generate_uuid_v4() -> Result<String> {
    let mut bytes = [0_u8; 16];
    File::open("/dev/urandom")
        .and_then(|mut file| file.read_exact(&mut bytes))
        .with_context(|| "failed to read random bytes for one-off instance name")?;

    bytes[6] = (bytes[6] & 0x0f) | 0x40;
    bytes[8] = (bytes[8] & 0x3f) | 0x80;

    Ok(format!(
        "{:02x}{:02x}{:02x}{:02x}-{:02x}{:02x}-{:02x}{:02x}-{:02x}{:02x}-{:02x}{:02x}{:02x}{:02x}{:02x}{:02x}",
        bytes[0],
        bytes[1],
        bytes[2],
        bytes[3],
        bytes[4],
        bytes[5],
        bytes[6],
        bytes[7],
        bytes[8],
        bytes[9],
        bytes[10],
        bytes[11],
        bytes[12],
        bytes[13],
        bytes[14],
        bytes[15]
    ))
}

fn create_private_runtime_root(instance_name: &str, workspace_root: &Path) -> Result<PathBuf> {
    let workspace_root = workspace_root
        .canonicalize()
        .unwrap_or_else(|_| workspace_root.to_path_buf());
    let mut bases = vec![std::env::temp_dir()];
    bases.extend([PathBuf::from("/var/tmp"), PathBuf::from("/dev/shm")]);

    for base in bases {
        if !base.is_dir() {
            continue;
        }
        let base = base.canonicalize().unwrap_or(base);
        let root = base.join(instance_name);
        if root.starts_with(&workspace_root) {
            continue;
        }

        #[cfg(unix)]
        {
            use std::os::unix::fs::{DirBuilderExt as _, PermissionsExt as _};
            let mut builder = fs::DirBuilder::new();
            builder.mode(0o700);
            builder.create(&root).with_context(|| {
                format!(
                    "failed to create private runtime root {} (name collisions are not reused)",
                    root.display()
                )
            })?;
            fs::set_permissions(&root, fs::Permissions::from_mode(0o700)).with_context(|| {
                format!("failed to secure private runtime root {}", root.display())
            })?;
        }
        #[cfg(not(unix))]
        {
            fs::create_dir(&root).with_context(|| {
                format!(
                    "failed to create private runtime root {} (name collisions are not reused)",
                    root.display()
                )
            })?;
        }
        return Ok(root);
    }

    Err(anyhow::anyhow!(
        "no temporary directory is available outside workspace {}",
        workspace_root.display()
    )
    .into())
}

struct SignalWatcher {
    handle: SignalHandle,
    received: Arc<AtomicI32>,
    thread: Option<JoinHandle<()>>,
}

impl SignalWatcher {
    fn new() -> Result<Self> {
        let mut signals = Signals::new([SIGINT, SIGTERM])
            .with_context(|| "failed to install one-off signal handlers")?;
        let handle = signals.handle();
        let received = Arc::new(AtomicI32::new(0));
        let thread_received = Arc::clone(&received);
        let thread = thread::spawn(move || {
            if let Some(signal) = signals.forever().next() {
                thread_received.store(signal, Ordering::SeqCst);
            }
        });
        Ok(Self {
            handle,
            received,
            thread: Some(thread),
        })
    }

    fn received(&self) -> Option<i32> {
        match self.received.load(Ordering::SeqCst) {
            0 => None,
            signal => Some(signal),
        }
    }

    fn finish(&mut self) -> Option<i32> {
        self.handle.close();
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
        self.received()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[cfg(unix)]
    use std::os::unix::fs::PermissionsExt;

    #[test]
    fn generated_instance_suffix_is_uuid_v4() {
        let uuid = generate_uuid_v4().unwrap();
        assert_eq!(uuid.len(), 36);
        assert_eq!(&uuid[8..9], "-");
        assert_eq!(&uuid[13..14], "-");
        assert_eq!(&uuid[18..19], "-");
        assert_eq!(&uuid[23..24], "-");
        assert_eq!(&uuid[14..15], "4");
        assert!(matches!(&uuid[19..20], "8" | "9" | "a" | "b"));
        assert!(
            uuid.chars()
                .all(|ch| ch.is_ascii_digit() || ('a'..='f').contains(&ch) || ch == '-')
        );
    }

    #[test]
    fn private_runtime_root_is_outside_the_source_workspace() {
        let name = format!("cladding-run-{}", generate_uuid_v4().unwrap());
        let workspace = std::env::temp_dir()
            .join(format!("cladding-run-workspace-{}", std::process::id()))
            .join(&name);
        fs::create_dir_all(&workspace).unwrap();

        let runtime_root = create_private_runtime_root(&name, &workspace).unwrap();

        assert!(!runtime_root.starts_with(&workspace));
        assert!(runtime_root.is_dir());
        let _ = fs::remove_dir_all(&runtime_root);
        let _ = fs::remove_dir_all(workspace.parent().unwrap());
    }

    #[test]
    fn command_failure_status_wins_over_cleanup_failure() {
        let error = finish_run(
            "cladding-run-test",
            Path::new("/tmp/cladding-run-test"),
            Err(Error::CommandFailed {
                context: "podman exec",
                code: 17,
            }),
            Err(Error::message("podman rm failed")),
            None,
        )
        .unwrap_err();

        assert_eq!(error.exit_code(), 17);
    }

    #[test]
    fn successful_command_returns_failure_when_cleanup_fails() {
        let error = finish_run(
            "cladding-run-test",
            Path::new("/tmp/cladding-run-test"),
            Ok(()),
            Err(Error::message("podman rm failed")),
            None,
        )
        .unwrap_err();

        assert_eq!(error.exit_code(), 1);
        assert!(error.to_string().contains("cladding-run-test"));
    }

    #[test]
    fn run_secret_overrides_resolve_environment_and_file_sources() {
        let root = std::env::temp_dir().join(format!(
            "cladding-run-secret-sources-{}-{}",
            std::process::id(),
            generate_uuid_v4().unwrap()
        ));
        fs::create_dir_all(&root).unwrap();
        let file_path = root.join("token-file");
        fs::write(&file_path, b"file-secret\n").unwrap();
        let raw = vec![
            "env-token=env:CLADDING_TEST_TOKEN".to_string(),
            format!("file-token=file:{}", file_path.display()),
        ];

        let resolved = resolve_run_secret_overrides(
            &raw,
            |name| (name == "CLADDING_TEST_TOKEN").then(|| OsString::from("env-secret")),
            read_run_secret_file,
        )
        .unwrap();

        assert_eq!(resolved.len(), 2);
        assert!(resolved[0].name == "env-token" && resolved[0].contents == b"env-secret");
        assert!(resolved[1].name == "file-token" && resolved[1].contents == b"file-secret\n");
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn run_secret_overrides_reject_duplicates_and_literal_values_without_echoing_them() {
        let duplicate = vec![
            "github-token=env:FIRST_TOKEN".to_string(),
            "github-token=env:SECOND_TOKEN".to_string(),
        ];
        assert!(
            parse_run_secret_overrides(&duplicate)
                .err()
                .unwrap()
                .to_string()
                .contains("duplicate Baffle secret override")
        );

        let literal = "github-token=cladding-test-literal-secret";
        let error = parse_run_secret_overrides(&[literal.to_string()])
            .err()
            .unwrap()
            .to_string();
        assert!(error.contains("literal values are not accepted"));
        assert!(!error.contains("cladding-test-literal-secret"));
    }

    #[test]
    fn run_secret_overrides_reject_path_names_and_missing_sources_safely() {
        for name in [
            "",
            "../token",
            "/tmp/token",
            "nested/token",
            "..",
            "secret.",
            "secret-",
            "secret_",
            "secret..value",
        ] {
            let raw = vec![format!("{name}=env:CLADDING_TEST_TOKEN")];
            assert!(parse_run_secret_overrides(&raw).is_err(), "name={name:?}");
        }
        let overlong_name = format!("{}=env:CLADDING_TEST_TOKEN", "a".repeat(65));
        assert!(parse_run_secret_overrides(&[overlong_name]).is_err());

        let error = resolve_run_secret_overrides(
            &["token=env:CLADDING_MISSING_TOKEN".to_string()],
            |_| None,
            read_run_secret_file,
        )
        .err()
        .unwrap()
        .to_string();
        assert!(error.contains("CLADDING_MISSING_TOKEN"));
        assert!(!error.contains("secret-content"));

        let missing_file = resolve_run_secret_overrides(
            &["token=file:/no/such/cladding-secret-file".to_string()],
            |_| None,
            read_run_secret_file,
        );
        assert!(missing_file.is_err());
        assert!(
            !missing_file
                .err()
                .unwrap()
                .to_string()
                .contains("secret-content")
        );

        #[cfg(unix)]
        {
            let non_file = resolve_run_secret_overrides(
                &["token=file:/dev/null".to_string()],
                |_| None,
                read_run_secret_file,
            );
            assert!(non_file.is_err());
        }
    }

    #[test]
    fn run_secret_files_are_private_independent_and_removed_with_the_runtime() {
        let first_root = create_private_runtime_root(
            &format!("cladding-run-{}", generate_uuid_v4().unwrap()),
            Path::new("/workspace"),
        )
        .unwrap();
        let second_root = create_private_runtime_root(
            &format!("cladding-run-{}", generate_uuid_v4().unwrap()),
            Path::new("/workspace"),
        )
        .unwrap();
        let project_root = first_root
            .parent()
            .unwrap()
            .join(format!("project-{}", generate_uuid_v4().unwrap()));
        let project_secrets = project_root.join("credentials/baffle/secrets");
        fs::create_dir_all(&project_secrets).unwrap();
        fs::write(project_secrets.join("github-token"), b"persistent-token").unwrap();
        fs::write(project_secrets.join("other-token"), b"project-token").unwrap();
        let first = ResolvedRunSecret {
            name: "github-token".to_string(),
            contents: b"first-run-secret".to_vec(),
        };
        let second = ResolvedRunSecret {
            name: "github-token".to_string(),
            contents: b"second-run-secret".to_vec(),
        };

        let (first_mounts, second_mounts) = std::thread::scope(|scope| {
            let first_task = scope.spawn(|| {
                materialize_run_secret_overrides(
                    &first_root,
                    &project_root,
                    std::slice::from_ref(&first),
                )
            });
            let second_task = scope.spawn(|| {
                materialize_run_secret_overrides(
                    &second_root,
                    &project_root,
                    std::slice::from_ref(&second),
                )
            });
            (
                first_task.join().unwrap().unwrap().unwrap(),
                second_task.join().unwrap().unwrap().unwrap(),
            )
        });
        let first_path = first_mounts.directory.join("github-token");
        let second_path = second_mounts.directory.join("github-token");

        assert_ne!(first_path, second_path);
        assert_eq!(fs::read(&first_path).unwrap(), b"first-run-secret");
        assert_eq!(fs::read(&second_path).unwrap(), b"second-run-secret");
        assert_eq!(
            fs::read(first_mounts.directory.join("other-token")).unwrap(),
            b""
        );
        assert_eq!(
            fs::read(second_mounts.directory.join("other-token")).unwrap(),
            b""
        );
        for mounts in [&first_mounts, &second_mounts] {
            assert_eq!(mounts.project_secrets.len(), 1);
            assert_eq!(mounts.project_secrets[0].name, "other-token");
            assert_eq!(
                mounts.project_secrets[0].source,
                project_secrets.join("other-token")
            );
        }
        assert_eq!(
            fs::read(project_secrets.join("github-token")).unwrap(),
            b"persistent-token"
        );
        #[cfg(unix)]
        assert_eq!(
            fs::metadata(&first_mounts.directory)
                .unwrap()
                .permissions()
                .mode()
                & 0o777,
            0o700
        );
        #[cfg(unix)]
        assert_eq!(
            fs::metadata(&first_path).unwrap().permissions().mode() & 0o777,
            0o600
        );
        #[cfg(unix)]
        assert_eq!(
            fs::metadata(first_mounts.directory.join("other-token"))
                .unwrap()
                .permissions()
                .mode()
                & 0o777,
            0o600
        );
        #[cfg(unix)]
        assert_eq!(
            fs::metadata(second_mounts.directory.join("other-token"))
                .unwrap()
                .permissions()
                .mode()
                & 0o777,
            0o600
        );

        remove_private_runtime_root(&first_root).unwrap();
        remove_private_runtime_root(&second_root).unwrap();
        assert!(!first_root.exists());
        assert!(!second_root.exists());
        assert_eq!(
            fs::read(project_secrets.join("github-token")).unwrap(),
            b"persistent-token"
        );
        fs::remove_dir_all(project_root).unwrap();
    }
}
