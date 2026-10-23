use super::context::Context;
use super::control::RunControl;
use super::{exec, lifecycle};
use anyhow::Context as _;
use cladding::error::{Error, Result};
use cladding::identity::invoking_uid;
use serde::Deserialize;
use signal_hook::consts::signal::{SIGINT, SIGTERM};
use signal_hook::iterator::{Handle as SignalHandle, Signals};
use std::collections::{BTreeMap, HashSet};
use std::ffi::OsString;
use std::fs::{self, File, OpenOptions};
use std::io::{Read as _, Write as _};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, ExitStatus, Stdio};
use std::sync::Arc;
use std::sync::atomic::{AtomicI32, Ordering};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

const BAFFLE_READY_TIMEOUT: Duration = Duration::from_secs(30);
const BAFFLE_READY_ATTEMPT_TIMEOUT: Duration = Duration::from_secs(2);
const BAFFLE_SESSION_CREATE_TIMEOUT: Duration = Duration::from_secs(30);
const MAX_BAFFLE_OUTPUT_BYTES: usize = 64 * 1024;

pub(super) fn cmd_run(
    source_context: &Context,
    env_vars: &[String],
    secret_overrides: &[String],
    args: &[String],
    config_uses_stdin: bool,
    verbose: bool,
) -> Result<()> {
    let preparation = cladding::runtime_metrics::phase("preparation/preflight");
    if args.is_empty() {
        return Err(Error::message("missing run command"));
    }

    source_context.require_initialized_project()?;

    let secret_overrides = resolve_run_secret_overrides(
        secret_overrides,
        |name| std::env::var_os(name),
        read_run_secret_file,
    )?;
    let instance_name = format!("cladding-run-{}", generate_uuid_v4()?);
    if verbose {
        eprintln!("starting one-off instance: {instance_name}");
    }

    let mut config = source_context.load_config().map_err(|err| {
        Error::message(format!(
            "one-off instance '{instance_name}' failed to load config: {err}"
        ))
    })?;
    config.name = instance_name.clone();
    let fake_mappings = configured_fake_credential_mappings(&config.fake_credential_env, env_vars);

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
        &source_context.project_root,
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
            drop(preparation);
            let _cleanup = cladding::runtime_metrics::phase("cleanup");
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
    let mut context = source_context.with_resolved_config(runtime_root.clone(), config);
    context = match secret_mounts {
        Some(secret_mounts) => context.with_run_secret_mounts(secret_mounts),
        None => context,
    };
    context = context.with_deferred_agent_session_creation();
    let config = context
        .resolved_config()
        .expect("one-off run context stores its resolved configuration");
    let mut spec = match context.runtime_spec(config) {
        Ok(spec) => spec,
        Err(setup_error) => {
            drop(preparation);
            let _cleanup = cladding::runtime_metrics::phase("cleanup");
            let root_cleanup = remove_private_runtime_root(&runtime_root).map_err(Error::from);
            let signal = signals.finish();
            if let Err(cleanup_error) = root_cleanup {
                report_cleanup_failure(&instance_name, &runtime_root, &cleanup_error);
            }
            if let Some(signal) = signal {
                return Err(signal_status_error(signal));
            }
            return Err(Error::message(format!(
                "one-off instance '{instance_name}' failed to prepare its runtime: {setup_error}"
            )));
        }
    };

    if let Err(setup_error) = lifecycle::prepare_run_runtime_root(&runtime_root) {
        drop(preparation);
        let _cleanup = cladding::runtime_metrics::phase("cleanup");
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

    let mut expected_roles = vec!["proxy".to_string(), "agent".to_string()];
    if spec.nw_sandbox.is_some() {
        expected_roles.push("nw-sandbox".to_string());
    }
    if spec.fs_sandbox.is_some() {
        expected_roles.push("fs-sandbox".to_string());
    }
    let mut control = match RunControl::start(
        &runtime_root,
        instance_name.trim_start_matches("cladding-run-"),
        &expected_roles,
    ) {
        Ok(control) => control,
        Err(setup_error) => {
            drop(preparation);
            let _cleanup = cladding::runtime_metrics::phase("cleanup");
            let root_cleanup = remove_private_runtime_root(&runtime_root).map_err(Error::from);
            let signal = signals.finish();
            if let Err(cleanup_error) = root_cleanup {
                report_cleanup_failure(&instance_name, &runtime_root, &cleanup_error);
            }
            if let Some(signal) = signal {
                return Err(signal_status_error(signal));
            }
            return Err(Error::message(format!(
                "one-off instance '{instance_name}' failed to acquire run ownership: {setup_error}"
            )));
        }
    };
    if let Err(setup_error) =
        spec.set_run_control_metadata(control.socket_path(), control.private_identity(), config)
    {
        drop(preparation);
        let _cleanup = cladding::runtime_metrics::phase("cleanup");
        let cleanup_error = cleanup_control_only(&runtime_root, &mut control);
        let signal = signals.finish();
        if let Err(cleanup_error) = cleanup_error {
            report_cleanup_failure(&instance_name, &runtime_root, &cleanup_error);
        }
        if let Some(signal) = signal {
            return Err(signal_status_error(signal));
        }
        return Err(Error::message(format!(
            "one-off instance '{instance_name}' failed to prepare run metadata: {setup_error}"
        )));
    }
    let mut resource_attempts = cladding::podman::RuntimeResourceAttempts::for_spec(&spec);

    drop(preparation);
    let (startup_result, runtime_create_attempted) =
        lifecycle::start_run_runtime(&context, config, &spec, &mut resource_attempts, verbose);

    if let Err(startup_error) = startup_result {
        let _cleanup = cladding::runtime_metrics::phase("cleanup");
        let cleanup_error = if runtime_create_attempted {
            cleanup_ephemeral_runtime(
                &spec,
                &resource_attempts,
                &runtime_root,
                &instance_name,
                verbose,
                &mut control,
            )
        } else {
            cleanup_control_only(&runtime_root, &mut control)
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

    let command_result = match exec::capture_run_mount_identities(&spec)
        .and_then(|identities| control.set_component_mount_identities(identities))
    {
        Err(error) => Err(error),
        Ok(()) => {
            let _command = cladding::runtime_metrics::phase("user command");
            match signals.received() {
                Some(signal) => Err(signal_status_error(signal)),
                None => match create_agent_session_and_fake_environment(
                    &spec.proxy.container.name,
                    config.agent_session_config(),
                    &fake_mappings,
                    &signals,
                ) {
                    Ok(fake_environment) => run_if_not_cancelled(signals.received(), || {
                        if !control.mark_ready(fake_environment.clone()) {
                            return Err(signal_status_error(SIGTERM));
                        }
                        eprintln!("run: {}", instance_name.trim_start_matches("cladding-run-"));
                        let cancellation_check = || {
                            signals
                                .received()
                                .or_else(|| control.stop_requested().then_some(SIGTERM))
                        };
                        let mut effective_env = env_vars.to_vec();
                        effective_env.extend(fake_environment);
                        exec::cmd_run_command(
                            config,
                            &spec,
                            &effective_env,
                            args,
                            !config_uses_stdin,
                            &cancellation_check,
                        )
                    }),
                    Err(error) => Err(error),
                },
            }
        }
    };
    let cleanup_error = {
        let _cleanup = cladding::runtime_metrics::phase("cleanup");
        cleanup_ephemeral_runtime(
            &spec,
            &resource_attempts,
            &runtime_root,
            &instance_name,
            verbose,
            &mut control,
        )
    };
    let signal = signals.finish();

    finish_run(
        &instance_name,
        &runtime_root,
        command_result,
        cleanup_error,
        signal,
    )
}

fn configured_fake_credential_mappings(
    configured: &BTreeMap<String, String>,
    explicit_environment: &[String],
) -> BTreeMap<String, String> {
    let mut mappings = configured.clone();
    for value in explicit_environment {
        let environment_name = value.split_once('=').map(|(name, _)| name).unwrap_or(value);
        mappings.remove(environment_name);
    }
    mappings
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct CreatedAgentSession {
    id: String,
    socket: String,
    persistent: bool,
    #[serde(default)]
    ca_certificate_pem: Option<String>,
    credential_seed_base64url: Option<String>,
}

struct CapturedProcessOutput {
    status: ExitStatus,
    stdout: Vec<u8>,
}

fn create_agent_session_and_fake_environment(
    proxy_container: &str,
    session_config: &str,
    mappings: &BTreeMap<String, String>,
    signals: &SignalWatcher,
) -> Result<Vec<String>> {
    wait_for_baffle_ready(proxy_container, signals)?;
    if let Some(signal) = signals.received() {
        return Err(signal_status_error(signal));
    }

    let output = run_podman_exec_capture(
        &[
            "exec",
            proxy_container,
            "/opt/tools/bin/baffle",
            "create",
            session_config,
        ],
        BAFFLE_SESSION_CREATE_TIMEOUT,
        signals,
        "Baffle agent session creation",
    )?;
    if !output.status.success() {
        return Err(Error::message("Baffle failed to create the agent session"));
    }
    let session = parse_created_agent_session(&output.stdout)?;
    let _ = session.ca_certificate_pem;
    let seed = session
        .credential_seed_base64url
        .as_deref()
        .ok_or_else(|| Error::message("Baffle did not return an agent session credential seed"))?;

    derive_with_cancellation_check(
        || signals.received(),
        || {
            let values = cladding_fake_credentials::derive_environment(seed, mappings)
                .map_err(Error::message)?;
            Ok(values
                .into_iter()
                .map(|(name, value)| format!("{name}={value}"))
                .collect())
        },
    )
}

fn parse_created_agent_session(output: &[u8]) -> Result<CreatedAgentSession> {
    let session: CreatedAgentSession = serde_json::from_slice(output)
        .map_err(|_| Error::message("Baffle returned an invalid agent session response"))?;
    if session.id.trim().is_empty() || session.id.chars().any(char::is_control) {
        return Err(Error::message(
            "Baffle returned an unexpected agent session identifier",
        ));
    }
    if session.socket != "/run/cladding/proxy/agent/proxy.sock" {
        return Err(Error::message(
            "Baffle returned an unexpected agent session socket",
        ));
    }
    if !session.persistent {
        return Err(Error::message(
            "Baffle returned a non-persistent agent session",
        ));
    }
    let seed = session
        .credential_seed_base64url
        .as_deref()
        .ok_or_else(|| Error::message("Baffle did not return an agent session credential seed"))?;
    cladding_fake_credentials::validate_seed(seed).map_err(Error::message)?;
    Ok(session)
}

fn wait_for_baffle_ready(proxy_container: &str, signals: &SignalWatcher) -> Result<()> {
    let deadline = Instant::now() + BAFFLE_READY_TIMEOUT;
    while Instant::now() < deadline {
        if let Some(signal) = signals.received() {
            return Err(signal_status_error(signal));
        }
        let remaining = deadline.saturating_duration_since(Instant::now());
        let attempt_timeout = remaining.min(BAFFLE_READY_ATTEMPT_TIMEOUT);
        let readiness = run_podman_exec_capture(
            &["exec", proxy_container, "/opt/tools/bin/baffle", "list"],
            attempt_timeout,
            signals,
            "Baffle readiness check",
        );
        if let Ok(output) = readiness {
            if output.status.success() {
                return Ok(());
            }
        } else if let Some(signal) = signals.received() {
            return Err(signal_status_error(signal));
        }

        let sleep_until = (Instant::now() + Duration::from_millis(50)).min(deadline);
        while Instant::now() < sleep_until {
            if let Some(signal) = signals.received() {
                return Err(signal_status_error(signal));
            }
            thread::sleep(Duration::from_millis(5));
        }
    }
    if let Some(signal) = signals.received() {
        return Err(signal_status_error(signal));
    }
    Err(Error::message(
        "timed out waiting for the Baffle daemon to become ready",
    ))
}

fn run_podman_exec_capture(
    args: &[&str],
    timeout: Duration,
    signals: &SignalWatcher,
    operation: &str,
) -> Result<CapturedProcessOutput> {
    let mut command = Command::new("podman");
    command
        .args(args)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null());
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt as _;
        command.process_group(0);
    }
    capture_command_output(&mut command, timeout, || signals.received(), operation)
}

fn capture_command_output(
    command: &mut Command,
    timeout: Duration,
    mut cancelled: impl FnMut() -> Option<i32>,
    operation: &str,
) -> Result<CapturedProcessOutput> {
    let mut child = command
        .spawn()
        .map_err(|_| Error::message(format!("failed to start {operation}")))?;
    let stdout = child
        .stdout
        .take()
        .expect("captured command stdout is piped");
    let reader = thread::spawn(move || read_capped_output(stdout));
    let deadline = Instant::now() + timeout;

    loop {
        if let Some(signal) = cancelled() {
            stop_process_group(&mut child);
            let _ = reader.join();
            return Err(signal_status_error(signal));
        }
        if Instant::now() >= deadline {
            stop_process_group(&mut child);
            let _ = reader.join();
            return Err(Error::message(format!("{operation} timed out")));
        }
        match child.try_wait() {
            Ok(Some(status)) => {
                let (stdout, overflowed) = reader
                    .join()
                    .map_err(|_| Error::message(format!("failed to read {operation} output")))?
                    .map_err(|_| Error::message(format!("failed to read {operation} output")))?;
                if overflowed {
                    return Err(Error::message(format!(
                        "{operation} returned an oversized response"
                    )));
                }
                return Ok(CapturedProcessOutput { status, stdout });
            }
            Ok(None) => thread::sleep(Duration::from_millis(10)),
            Err(_) => {
                stop_process_group(&mut child);
                let _ = reader.join();
                return Err(Error::message(format!("failed to wait for {operation}")));
            }
        }
    }
}

fn read_capped_output(mut reader: impl std::io::Read) -> std::io::Result<(Vec<u8>, bool)> {
    let mut output = Vec::new();
    let mut buffer = [0_u8; 8192];
    let mut overflowed = false;
    loop {
        let read = reader.read(&mut buffer)?;
        if read == 0 {
            return Ok((output, overflowed));
        }
        let remaining = MAX_BAFFLE_OUTPUT_BYTES.saturating_sub(output.len());
        let keep = remaining.min(read);
        output.extend_from_slice(&buffer[..keep]);
        overflowed |= keep < read;
    }
}

fn stop_process_group(child: &mut Child) {
    #[cfg(unix)]
    {
        let process_group = -(child.id() as libc::pid_t);
        // SAFETY: this signals the private process group created for this child.
        unsafe {
            libc::kill(process_group, libc::SIGKILL);
        }
    }
    let _ = child.kill();
    let _ = child.wait();
}

fn run_if_not_cancelled<T>(signal: Option<i32>, launch: impl FnOnce() -> Result<T>) -> Result<T> {
    match signal {
        Some(signal) => Err(signal_status_error(signal)),
        None => launch(),
    }
}

fn derive_with_cancellation_check<T>(
    mut cancelled: impl FnMut() -> Option<i32>,
    derive: impl FnOnce() -> Result<T>,
) -> Result<T> {
    if let Some(signal) = cancelled() {
        return Err(signal_status_error(signal));
    }
    let values = derive()?;
    if let Some(signal) = cancelled() {
        return Err(signal_status_error(signal));
    }
    Ok(values)
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
    spec: &cladding::runtime::RuntimeSpec,
    resource_attempts: &cladding::podman::RuntimeResourceAttempts,
    runtime_root: &Path,
    instance_name: &str,
    verbose: bool,
    control: &mut RunControl,
) -> Result<()> {
    if verbose {
        eprintln!("cleaning up one-off instance: {instance_name}");
    }
    control.mark_stopping();
    let (runtime_cleanup, runtime_root_safe_to_remove) =
        lifecycle::cleanup_run_runtime(spec, resource_attempts, verbose);
    let socket_cleanup = control.unlink_socket_for_shutdown();
    let local_cleanup = if runtime_root_safe_to_remove && socket_cleanup.is_ok() {
        remove_private_runtime_root(runtime_root)
            .with_context(|| {
                format!(
                    "failed to remove private runtime root {}",
                    runtime_root.display()
                )
            })
            .map_err(Error::from)
    } else {
        Ok(())
    };
    let result = match (runtime_cleanup, socket_cleanup, local_cleanup) {
        (Ok(()), Ok(()), Ok(())) => Ok(()),
        (Err(err), Ok(()), Ok(())) | (Ok(()), Err(err), Ok(())) | (Ok(()), Ok(()), Err(err)) => {
            Err(err)
        }
        (Err(runtime_err), socket_result, local_result) => {
            let mut errors = vec![runtime_err.to_string()];
            if let Err(error) = socket_result {
                errors.push(error.to_string());
            }
            if let Err(error) = local_result {
                errors.push(error.to_string());
            }
            Err(Error::message(errors.join("\n")))
        }
        (Ok(()), Err(socket_err), Err(local_err)) => Err(Error::message(format!(
            "{socket_err}\nprivate runtime cleanup also failed: {local_err}"
        ))),
    };
    let stop_error = result.as_ref().err().map(ToString::to_string);
    control.mark_stopped(stop_error);
    control.finish();
    result
}

fn cleanup_control_only(runtime_root: &Path, control: &mut RunControl) -> Result<()> {
    control.mark_stopping();
    let socket_cleanup = control.unlink_socket_for_shutdown();
    let root_cleanup = if socket_cleanup.is_ok() {
        remove_private_runtime_root(runtime_root)
            .with_context(|| {
                format!(
                    "failed to remove private runtime root {}",
                    runtime_root.display()
                )
            })
            .map_err(Error::from)
    } else {
        Ok(())
    };
    let result = match (socket_cleanup, root_cleanup) {
        (Ok(()), Ok(())) => Ok(()),
        (Err(error), _) | (Ok(()), Err(error)) => Err(error),
    };
    control.mark_stopped(result.as_ref().err().map(ToString::to_string));
    control.finish();
    result
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

fn create_private_runtime_root(instance_name: &str, project_root: &Path) -> Result<PathBuf> {
    let runtime_dir = project_root.join("runtime");
    ensure_run_runtime_directory(&runtime_dir, false)?;

    let runs_dir = runtime_dir.join("runs");
    ensure_run_runtime_directory(&runs_dir, true)?;

    let root = runs_dir.join(instance_name);
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
        fs::set_permissions(&root, fs::Permissions::from_mode(0o700))
            .with_context(|| format!("failed to secure private runtime root {}", root.display()))?;
        use std::os::unix::fs::MetadataExt as _;
        let owner = invoking_uid();
        let metadata = fs::symlink_metadata(&root).with_context(|| {
            format!("failed to inspect private runtime root {}", root.display())
        })?;
        if metadata.uid() != owner {
            return Err(Error::message(format!(
                "private runtime root {} is not owned by the invoking user",
                root.display()
            )));
        }
    }
    Ok(root)
}

fn ensure_run_runtime_directory(path: &Path, private: bool) -> anyhow::Result<()> {
    let metadata = match fs::symlink_metadata(path) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            #[cfg(unix)]
            {
                use std::os::unix::fs::DirBuilderExt as _;
                let mut builder = fs::DirBuilder::new();
                builder.mode(if private { 0o700 } else { 0o755 });
                match builder.create(path) {
                    Ok(()) => {}
                    Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {}
                    Err(error) => {
                        return Err(error).with_context(|| {
                            format!("failed to create runtime directory {}", path.display())
                        });
                    }
                }
            }
            fs::symlink_metadata(path).with_context(|| {
                format!("failed to inspect runtime directory {}", path.display())
            })?
        }
        Err(error) => {
            return Err(error).with_context(|| {
                format!("failed to inspect runtime directory {}", path.display())
            });
        }
    };

    if metadata.file_type().is_symlink() || !metadata.is_dir() {
        anyhow::bail!(
            "runtime path is not a regular directory: {}",
            path.display()
        );
    }

    #[cfg(unix)]
    {
        use std::os::unix::fs::{MetadataExt as _, PermissionsExt as _};
        let owner = invoking_uid();
        if private && metadata.uid() != owner {
            anyhow::bail!(
                "private runtime directory {} is not owned by the invoking user",
                path.display()
            );
        }
        if private {
            fs::set_permissions(path, fs::Permissions::from_mode(0o700)).with_context(|| {
                format!(
                    "failed to secure private runtime directory {}",
                    path.display()
                )
            })?;
        }
    }

    Ok(())
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
    use crate::cli::context::{ConfigSource, Context};
    #[cfg(unix)]
    use std::os::unix::fs::PermissionsExt;

    #[test]
    fn run_with_external_config_requires_initialized_project_before_creating_runtime_state() {
        let root = std::env::temp_dir().join(format!(
            "cladding-run-uninitialized-{}-{}",
            std::process::id(),
            generate_uuid_v4().unwrap()
        ));
        let project_root = root.join(".cladding");
        let external_config = root.join("custom.json");
        fs::create_dir_all(&project_root).unwrap();
        fs::write(&external_config, r#"{"name":"demo","agent":{}}"#).unwrap();
        let context = Context::new(
            project_root.clone(),
            root.clone(),
            ConfigSource::File(external_config),
        );

        let error = cmd_run(&context, &[], &[], &["echo".to_string()], false, false)
            .expect_err("run must require the project config created by init");

        assert!(
            error
                .to_string()
                .contains("missing initialized .cladding project")
        );
        assert!(!project_root.join("runtime").exists());
        fs::remove_dir_all(root).unwrap();
    }

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
    fn explicit_environment_values_override_configured_fake_mappings() {
        let configured = BTreeMap::from([
            ("GITHUB_TOKEN".to_string(), "github-api".to_string()),
            ("GH_TOKEN".to_string(), "github-api".to_string()),
            ("OPENAI_API_KEY".to_string(), "openai-api".to_string()),
        ]);
        let explicit = vec!["GITHUB_TOKEN=ordinary-value".to_string()];

        let effective = configured_fake_credential_mappings(&configured, &explicit);

        assert_eq!(
            effective,
            BTreeMap::from([
                ("GH_TOKEN".to_string(), "github-api".to_string()),
                ("OPENAI_API_KEY".to_string(), "openai-api".to_string()),
            ])
        );
        assert_eq!(explicit, ["GITHUB_TOKEN=ordinary-value"]);
    }

    #[test]
    fn explicit_environment_values_without_fake_mappings_remain_unchanged() {
        let explicit = vec!["TOKEN=ordinary-value".to_string()];
        assert!(configured_fake_credential_mappings(&BTreeMap::new(), &explicit).is_empty());
        assert_eq!(explicit, ["TOKEN=ordinary-value"]);
    }

    #[test]
    fn baffle_session_creation_response_requires_valid_expected_details_and_seed() {
        let valid = br#"{
            "id": "session-123",
            "socket": "/run/cladding/proxy/agent/proxy.sock",
            "persistent": true,
            "ca_certificate_pem": null,
            "credential_seed_base64url": "AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA"
        }"#;
        let parsed = parse_created_agent_session(valid).unwrap();
        assert_eq!(parsed.id, "session-123");

        for invalid in [
            &b"not-json"[..],
            br#"{"id":"session-123","socket":"/run/cladding/proxy/agent/proxy.sock","persistent":true}"#,
            br#"{"id":"session-123","socket":"/wrong.sock","persistent":true,"credential_seed_base64url":"AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA"}"#,
            br#"{"id":"session-123","socket":"/run/cladding/proxy/agent/proxy.sock","persistent":false,"credential_seed_base64url":"AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA"}"#,
            br#"{"id":"session-123","socket":"/run/cladding/proxy/agent/proxy.sock","persistent":true,"credential_seed_base64url":"bad-seed"}"#,
            br#"{"id":"session-123","socket":"/run/cladding/proxy/agent/proxy.sock","persistent":true,"credential_seed_base64url":"AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA","unexpected":true}"#,
        ] {
            assert!(parse_created_agent_session(invalid).is_err());
        }
    }

    #[cfg(unix)]
    #[test]
    fn cancellation_during_session_creation_prevents_workload_launch() {
        let temp = std::env::temp_dir().join(format!(
            "cladding-run-cancelled-session-{}-{}",
            std::process::id(),
            generate_uuid_v4().unwrap()
        ));
        fs::create_dir_all(&temp).unwrap();
        let marker = temp.join("workload-launched");
        let mut command = Command::new("/bin/sh");
        command
            .args(["-c", "sleep 5; touch \"$1\"", "cancelled-session-fixture"])
            .arg(&marker)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::null());
        use std::os::unix::process::CommandExt as _;
        command.process_group(0);

        let started = Instant::now();
        let result = capture_command_output(
            &mut command,
            Duration::from_secs(5),
            || (started.elapsed() >= Duration::from_millis(100)).then_some(SIGTERM),
            "session creation fixture",
        );
        assert!(result.is_err());
        let launch_result = run_if_not_cancelled(Some(SIGTERM), || {
            fs::write(&marker, b"launched")
                .map_err(|error| Error::message(format!("fixture failed: {error}")))
        });
        assert!(launch_result.is_err());
        assert!(!marker.exists());
        fs::remove_dir_all(temp).unwrap();
    }

    #[test]
    fn cancellation_during_fake_derivation_discards_values_before_workload_launch() {
        let signal = AtomicI32::new(0);
        let values = derive_with_cancellation_check(
            || match signal.load(Ordering::SeqCst) {
                0 => None,
                signal => Some(signal),
            },
            || {
                signal.store(SIGTERM, Ordering::SeqCst);
                Ok(vec!["TOKEN=fake-value".to_string()])
            },
        );
        assert!(values.is_err());

        let workload_launched = std::sync::atomic::AtomicBool::new(false);
        let launch = run_if_not_cancelled(Some(signal.load(Ordering::SeqCst)), || {
            workload_launched.store(true, Ordering::SeqCst);
            Ok(())
        });
        assert!(launch.is_err());
        assert!(!workload_launched.load(Ordering::SeqCst));
    }

    #[test]
    fn private_runtime_root_is_under_project_state_and_separate_from_persistent_runtime() {
        let name = format!("cladding-run-{}", generate_uuid_v4().unwrap());
        let root = std::env::temp_dir().join(format!(
            "cladding-run-project-{}-{}",
            std::process::id(),
            generate_uuid_v4().unwrap()
        ));
        let project_root = root.join(".cladding");
        fs::create_dir_all(&project_root).unwrap();

        let runtime_root = create_private_runtime_root(&name, &project_root).unwrap();

        assert_eq!(runtime_root, project_root.join("runtime/runs").join(&name));
        assert!(!runtime_root.starts_with(project_root.join("runtime/sockets")));
        assert!(runtime_root.is_dir());
        #[cfg(unix)]
        {
            use std::os::unix::fs::MetadataExt as _;
            assert_eq!(
                fs::metadata(&runtime_root).unwrap().permissions().mode() & 0o777,
                0o700
            );
            assert_eq!(fs::metadata(&runtime_root).unwrap().uid(), invoking_uid());
            assert_eq!(
                fs::metadata(runtime_root.parent().unwrap())
                    .unwrap()
                    .permissions()
                    .mode()
                    & 0o777,
                0o700
            );
        }
        let _ = fs::remove_dir_all(&runtime_root);
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn concurrent_first_runs_initialize_shared_runtime_directories() {
        const RUNS: usize = 16;

        let root = std::env::temp_dir().join(format!(
            "cladding-run-concurrent-{}-{}",
            std::process::id(),
            generate_uuid_v4().unwrap()
        ));
        let project_root = root.join(".cladding");
        fs::create_dir_all(&project_root).unwrap();
        let barrier = Arc::new(std::sync::Barrier::new(RUNS));

        let runtime_roots = thread::scope(|scope| {
            let workers = (0..RUNS)
                .map(|run| {
                    let barrier = Arc::clone(&barrier);
                    let project_root = &project_root;
                    scope.spawn(move || {
                        barrier.wait();
                        create_private_runtime_root(&format!("cladding-run-{run}"), project_root)
                    })
                })
                .collect::<Vec<_>>();

            workers
                .into_iter()
                .map(|worker| worker.join().unwrap().unwrap())
                .collect::<Vec<_>>()
        });

        assert_eq!(runtime_roots.len(), RUNS);
        assert!(runtime_roots.iter().all(|path| path.is_dir()));
        assert!(
            runtime_roots
                .iter()
                .all(|path| path.parent() == Some(project_root.join("runtime/runs").as_path()))
        );
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn private_runtime_root_rejects_an_existing_instance_directory() {
        let name = format!("cladding-run-{}", generate_uuid_v4().unwrap());
        let root = std::env::temp_dir().join(format!(
            "cladding-run-collision-{}-{}",
            std::process::id(),
            generate_uuid_v4().unwrap()
        ));
        let project_root = root.join(".cladding");
        let existing_root = project_root.join("runtime/runs").join(&name);
        fs::create_dir_all(&existing_root).unwrap();
        fs::write(existing_root.join("marker"), b"existing instance").unwrap();

        let error = create_private_runtime_root(&name, &project_root).unwrap_err();

        assert!(error.to_string().contains("name collisions are not reused"));
        assert_eq!(
            fs::read(existing_root.join("marker")).unwrap(),
            b"existing instance"
        );
        fs::remove_dir_all(root).unwrap();
    }

    #[cfg(unix)]
    #[test]
    fn private_runtime_root_rejects_a_symlinked_runs_directory() {
        use std::os::unix::fs::symlink;

        let name = format!("cladding-run-{}", generate_uuid_v4().unwrap());
        let root = std::env::temp_dir().join(format!(
            "cladding-run-symlink-{}-{}",
            std::process::id(),
            generate_uuid_v4().unwrap()
        ));
        let project_root = root.join(".cladding");
        let external_dir = root.join("external-runs");
        fs::create_dir_all(project_root.join("runtime")).unwrap();
        fs::create_dir_all(&external_dir).unwrap();
        symlink(&external_dir, project_root.join("runtime/runs")).unwrap();

        let error = create_private_runtime_root(&name, &project_root)
            .unwrap_err()
            .to_string();

        assert!(error.contains("not a regular directory"));
        assert!(!external_dir.join(name).exists());
        fs::remove_dir_all(root).unwrap();
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
        let test_root = std::env::temp_dir().join(format!(
            "cladding-run-secret-roots-{}-{}",
            std::process::id(),
            generate_uuid_v4().unwrap()
        ));
        let project_root = test_root.join(".cladding");
        fs::create_dir_all(&project_root).unwrap();
        let first_root = create_private_runtime_root(
            &format!("cladding-run-{}", generate_uuid_v4().unwrap()),
            &project_root,
        )
        .unwrap();
        let second_root = create_private_runtime_root(
            &format!("cladding-run-{}", generate_uuid_v4().unwrap()),
            &project_root,
        )
        .unwrap();
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
        fs::remove_dir_all(test_root).unwrap();
    }
}
