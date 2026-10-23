use super::CONTAINER_HOME_DIR;
use super::args::{ExecTarget, LogsTarget};
use super::control::{self, ComponentMountIdentity, MountIdentity};
use super::selection::SelectedRun;
use super::terminal::ForegroundTerminal;
use anyhow::Context as _;
use cladding::config::{ExecutionConfig, MountTarget, MountType, ResolvedMountConfig};
use cladding::error::{Error, Result};
use cladding::podman::podman_required;
use cladding::runtime::{
    ManagedVolumeKind, RuntimeComponent, RuntimeMount, RuntimeMountSource, RuntimeSpec,
    runtime_container_name,
};
use cladding::runtime_metrics;
use serde::Deserialize;
use signal_hook::consts::signal::{SIGINT, SIGTERM};
use signal_hook::iterator::Signals;
use std::env;
use std::fs;
use std::io::{self, IsTerminal};
use std::path::{Path, PathBuf};
use std::process::{Child, Command};
use std::thread;

struct PodmanExec<'a> {
    command_name: &'a str,
    mount_target: MountTarget,
    container_name: &'a str,
    env_vars: &'a [String],
    args: &'a [String],
    allow_interactive: bool,
    forward_stdin: bool,
    forward_signals: bool,
    cancellation_check: Option<&'a dyn Fn() -> Option<i32>>,
}

pub(super) fn cmd_exec_selected(
    selected: &SelectedRun,
    target: ExecTarget,
    env_vars: &[String],
    args: &[String],
) -> Result<()> {
    if args.is_empty() {
        return Err(Error::message("missing exec command"));
    }
    let role = target.as_str();
    let component = selected.component(role)?;
    selected.require_ready()?;
    let container_id = component
        .id
        .as_deref()
        .ok_or_else(|| Error::message("selected component has no immutable container ID"))?;
    let socket = selected
        .run
        .control_socket
        .as_deref()
        .ok_or_else(|| Error::message("selected run has no supervisor control endpoint"))?;
    let admission = control::admit(Path::new(socket), selected.uuid(), role, container_id)?;
    let cwd = env::current_dir().with_context(|| "failed to determine current directory")?;
    let workdir = inspect_run_workdir(
        container_id,
        role,
        &cwd,
        component.cwd_mappings.as_deref(),
        &admission.mount_identity,
    )?;
    let interactive = io::stdin().is_terminal() && io::stdout().is_terminal();
    let mut command = Command::new("podman");
    if interactive {
        command.args(["exec", "-it"]);
    } else {
        command.args(["exec", "-i"]);
    }
    command.arg("-w").arg(workdir);
    command.args(["--env", "LANG=C.UTF-8"]);
    if interactive {
        let colorterm = env::var("COLORTERM").unwrap_or_else(|_| "truecolor".to_string());
        let force_color = env::var("FORCE_COLOR").unwrap_or_else(|_| "3".to_string());
        command.args(["--env", "TERM=xterm-256color"]);
        command.arg("--env").arg(format!("COLORTERM={colorterm}"));
        command
            .arg("--env")
            .arg(format!("FORCE_COLOR={force_color}"));
    }
    let overridden = env_vars
        .iter()
        .map(|value| value.split_once('=').map(|(key, _)| key).unwrap_or(value))
        .collect::<std::collections::HashSet<_>>();
    for value in admission.fake_environment {
        let key = value.split_once('=').map(|(key, _)| key).unwrap_or(&value);
        if !overridden.contains(key) {
            command.arg("--env").arg(value);
        }
    }
    for value in env_vars {
        command.arg("--env").arg(value);
    }
    command.arg(container_id).args(args);
    let foreground_terminal = interactive
        .then(|| ForegroundTerminal::capture(0))
        .transpose()
        .with_context(|| "failed to prepare controlling terminal for podman exec")?;
    configure_podman_process_group(&mut command, foreground_terminal.as_ref());
    let mut child = command
        .spawn()
        .with_context(|| "failed to run podman exec")?;
    let result = wait_for_attached_exec(&mut child, Path::new(socket), selected.uuid());
    drop(foreground_terminal);
    result
}

fn wait_for_attached_exec(child: &mut Child, socket: &Path, uuid: &str) -> Result<()> {
    let signals = match Signals::new([SIGINT, SIGTERM]) {
        Ok(signals) => signals,
        Err(error) => {
            terminate_attached_exec(child, SIGTERM);
            return Err(anyhow::Error::new(error)
                .context("failed to install exec signal handlers")
                .into());
        }
    };
    let handle = signals.handle();
    let (signal_tx, signal_rx) = std::sync::mpsc::channel();
    let signal_thread = thread::spawn(move || {
        let mut signals = signals;
        if let Some(signal) = signals.forever().next() {
            let _ = signal_tx.send(signal);
        }
    });
    let mut last_control_check = std::time::Instant::now();
    let result = loop {
        match child.try_wait() {
            Ok(Some(status)) => break command_status(status, "podman exec"),
            Ok(None) => {}
            Err(error) => {
                terminate_attached_exec(child, SIGTERM);
                break Err(Error::message(format!(
                    "failed to wait for podman exec: {error}"
                )));
            }
        }
        if let Ok(signal) = signal_rx.try_recv() {
            terminate_attached_exec(child, signal);
            break Err(Error::CommandFailed {
                context: "podman exec",
                code: 128 + signal,
            });
        }
        if last_control_check.elapsed() >= std::time::Duration::from_millis(200) {
            last_control_check = std::time::Instant::now();
            match control::status(socket, uuid) {
                Ok(status) if status.state == "ready" => {}
                _ => {
                    terminate_attached_exec(child, SIGTERM);
                    break Err(Error::message(
                        "selected run stopped or lost its supervisor; exec was closed",
                    ));
                }
            }
        }
        thread::sleep(std::time::Duration::from_millis(25));
    };
    handle.close();
    let _ = signal_thread.join();
    result
}

fn terminate_attached_exec(child: &mut Child, signal: i32) {
    #[cfg(unix)]
    {
        // SAFETY: the podman child owns this process group.
        unsafe {
            libc::kill(-(child.id() as i32), signal);
        }
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(2);
        while std::time::Instant::now() < deadline {
            if child.try_wait().ok().flatten().is_some() {
                return;
            }
            thread::sleep(std::time::Duration::from_millis(20));
        }
        // SAFETY: this is the same process group started for podman exec.
        unsafe {
            libc::kill(-(child.id() as i32), signal_hook::consts::signal::SIGKILL);
        }
    }
    #[cfg(not(unix))]
    {
        let _ = child.kill();
    }
    let _ = child.wait();
}

pub(super) fn cmd_run_command(
    config: &ExecutionConfig,
    runtime_spec: &RuntimeSpec,
    env_vars: &[String],
    args: &[String],
    forward_stdin: bool,
    cancellation_check: &dyn Fn() -> Option<i32>,
) -> Result<()> {
    let container_name = runtime_container_name(&project_component_name(&config.name, "agent"));
    run_podman_exec(
        config,
        runtime_spec,
        PodmanExec {
            command_name: "run",
            mount_target: MountTarget::Agent,
            container_name: &container_name,
            env_vars,
            args,
            allow_interactive: forward_stdin,
            forward_stdin,
            forward_signals: true,
            cancellation_check: Some(cancellation_check),
        },
    )
}

pub(super) fn capture_run_mount_identities(
    runtime_spec: &RuntimeSpec,
) -> Result<Vec<ComponentMountIdentity>> {
    let mut components = vec![
        ("proxy", &runtime_spec.proxy),
        ("agent", &runtime_spec.agent),
    ];
    if let Some(component) = &runtime_spec.nw_sandbox {
        components.push(("nw-sandbox", component));
    }
    if let Some(component) = &runtime_spec.fs_sandbox {
        components.push(("fs-sandbox", component));
    }

    let mut identities = Vec::with_capacity(components.len());
    for (role, component) in components {
        let output = Command::new("podman")
            .args(["inspect", &component.container.name])
            .output()
            .with_context(|| format!("failed to inspect run {role} component mounts"))?;
        if !output.status.success() {
            return Err(Error::message(format!(
                "failed to inspect run {role} component mounts"
            )));
        }
        let document: serde_json::Value = serde_json::from_slice(&output.stdout).map_err(|_| {
            Error::message(format!("podman returned invalid inspection for run {role}"))
        })?;
        let object = document
            .as_array()
            .and_then(|items| items.first())
            .or_else(|| document.as_object().map(|_| &document))
            .ok_or_else(|| {
                Error::message(format!("podman returned no inspection for run {role}"))
            })?;
        let id = object
            .get("Id")
            .and_then(serde_json::Value::as_str)
            .filter(|id| !id.is_empty())
            .ok_or_else(|| Error::message(format!("run {role} has no immutable container ID")))?;
        let labels = object
            .get("Config")
            .and_then(|config| config.get("Labels"))
            .and_then(serde_json::Value::as_object)
            .ok_or_else(|| Error::message(format!("run {role} inspection omitted labels")))?;
        let expected_identity = runtime_spec
            .run_private_identity
            .as_deref()
            .ok_or_else(|| Error::message("run private identity metadata was not initialized"))?;
        let expected_socket = runtime_spec
            .run_control_socket
            .as_deref()
            .ok_or_else(|| Error::message("run control endpoint metadata was not initialized"))?;
        let expected_project_root = runtime_spec.project_root.to_string_lossy();
        let expected_socket = expected_socket.display().to_string();
        let expected_roles = runtime_spec.run_expected_roles.join(",");
        for (label, expected) in [
            ("cladding", runtime_spec.project_name.as_str()),
            ("project_root", expected_project_root.as_ref()),
            ("app", role),
            ("cladding_run_metadata_version", "2"),
            ("cladding_run_control_socket", expected_socket.as_str()),
            ("cladding_run_private_identity", expected_identity),
            ("cladding_run_expected_roles", expected_roles.as_str()),
        ] {
            if labels.get(label).and_then(serde_json::Value::as_str) != Some(expected) {
                return Err(Error::message(format!(
                    "run {role} inspection has conflicting {label} metadata"
                )));
            }
        }
        let mounts = object
            .get("Mounts")
            .and_then(serde_json::Value::as_array)
            .ok_or_else(|| Error::message(format!("run {role} inspection omitted mounts")))?;
        let mounts = mounts
            .iter()
            .map(|mount| {
                let string_field = |name: &str| {
                    mount
                        .get(name)
                        .and_then(serde_json::Value::as_str)
                        .filter(|value| !value.is_empty())
                        .map(str::to_string)
                };
                let destination = string_field("Destination").ok_or_else(|| {
                    Error::message(format!(
                        "run {role} inspection has a mount without destination"
                    ))
                })?;
                let mount_type = string_field("Type").ok_or_else(|| {
                    Error::message(format!("run {role} inspection has a mount without type"))
                })?;
                let read_only = mount
                    .get("RW")
                    .and_then(serde_json::Value::as_bool)
                    .map(|read_write| !read_write)
                    .ok_or_else(|| {
                        Error::message(format!("run {role} inspection omitted mount access mode"))
                    })?;
                let identity = MountIdentity {
                    destination,
                    mount_type,
                    source: string_field("Source"),
                    name: string_field("Name"),
                    read_only,
                };
                Ok(identity)
            })
            .collect::<Result<Vec<_>>>()?;
        identities.push(ComponentMountIdentity {
            role: role.to_string(),
            container_id: id.to_string(),
            mounts,
        });
    }
    Ok(identities)
}

pub(super) fn cmd_logs_selected(
    selected: &SelectedRun,
    target: LogsTarget,
    args: &[String],
) -> Result<()> {
    podman_required("podman (required for cladding logs)")?;
    if !selected.expects_role(target.as_str()) {
        return Err(Error::message(format!(
            "target '{}' is disabled for run {}",
            target.as_str(),
            selected.uuid()
        )));
    }
    let component = selected
        .run
        .components
        .iter()
        .find(|component| component.role == target.as_str())
        .ok_or_else(|| {
            Error::message(format!(
                "target '{}' is missing for run {}",
                target.as_str(),
                selected.uuid()
            ))
        })?;
    let id = component
        .id
        .as_deref()
        .ok_or_else(|| Error::message("selected component has no immutable container ID"))?;
    let mut command = Command::new("podman");
    command.arg("logs").args(args).arg(id);
    let status = command
        .status()
        .with_context(|| "failed to run podman logs")?;
    command_status(status, "podman logs")
}

fn run_podman_exec(
    config: &ExecutionConfig,
    runtime_spec: &RuntimeSpec,
    request: PodmanExec<'_>,
) -> Result<()> {
    let PodmanExec {
        command_name,
        mount_target,
        container_name,
        env_vars,
        args,
        allow_interactive,
        forward_stdin,
        forward_signals,
        cancellation_check,
    } = request;
    if args.is_empty() {
        eprintln!("usage: cladding {command_name} [--env KEY[=VALUE] ...] <command> [args...]");
        return Err(Error::message(format!("missing {command_name} command")));
    }

    let cwd = env::current_dir().with_context(|| "failed to determine current directory")?;

    runtime_spec.validate_custom_mount_sources()?;
    let component = runtime_component_for_target(runtime_spec, mount_target)?;
    let runtime_container = &component.container;
    if runtime_container.name != container_name {
        return Err(Error::message(format!(
            "runtime target '{}' has no container named '{container_name}'",
            mount_target_name(mount_target)
        )));
    }
    let container_workdir = resolve_target_workdir(
        &runtime_container.mounts,
        &config.mounts,
        mount_target,
        runtime_container.workdir.as_deref(),
        &cwd,
    )?;

    let interactive = allow_interactive && io::stdin().is_terminal() && io::stdout().is_terminal();

    let mut cmd = Command::new("podman");
    if interactive {
        let colorterm = env::var("COLORTERM").unwrap_or_else(|_| "truecolor".to_string());
        let force_color = env::var("FORCE_COLOR").unwrap_or_else(|_| "3".to_string());
        cmd.args([
            "exec",
            "-it",
            "-w",
            &container_workdir.display().to_string(),
            "--env",
            "LANG=C.UTF-8",
            "--env",
            "TERM=xterm-256color",
            "--env",
            &format!("COLORTERM={colorterm}"),
            "--env",
            &format!("FORCE_COLOR={force_color}"),
        ]);
    } else if forward_stdin {
        cmd.args([
            "exec",
            "-i",
            "-w",
            &container_workdir.display().to_string(),
            "--env",
            "LANG=C.UTF-8",
        ]);
    } else {
        cmd.args([
            "exec",
            "-w",
            &container_workdir.display().to_string(),
            "--env",
            "LANG=C.UTF-8",
        ]);
    }

    for env_var in env_vars {
        cmd.arg("--env").arg(env_var);
    }

    cmd.arg(container_name);

    for arg in args {
        cmd.arg(arg);
    }

    let command_started = std::time::Instant::now();
    let foreground_terminal = interactive
        .then(|| ForegroundTerminal::capture(0))
        .transpose()
        .with_context(|| "failed to prepare controlling terminal for podman exec")?;
    let mut child = spawn_podman_exec(
        &mut cmd,
        command_name,
        cancellation_check,
        foreground_terminal.as_ref(),
    )?;

    let mut signal_handle = None;
    let mut signal_thread = None;
    if !interactive || forward_signals {
        let kill_pattern = args.join(" ");
        let mut signals =
            Signals::new([SIGINT, SIGTERM]).with_context(|| "failed to install signal handlers")?;
        signal_handle = Some(signals.handle());
        let container_name = container_name.to_string();
        let metrics_context = runtime_metrics::capture_context();
        signal_thread = Some(thread::spawn(move || {
            if let Some(signal) = signals.forever().next()
                && !kill_pattern.is_empty()
            {
                let signal = format!("-{signal}");
                let mut command = Command::new("podman");
                command.args([
                    "exec",
                    &container_name,
                    "pkill",
                    &signal,
                    "-f",
                    &kill_pattern,
                ]);
                let _ =
                    runtime_metrics::status_with_context(&mut command, metrics_context.as_ref());
            }
        }));
    }

    let status = wait_for_podman_exec(&mut child, cancellation_check)
        .with_context(|| format!("failed to run podman exec for {command_name}"))?;
    runtime_metrics::record_command_elapsed(&cmd, command_started.elapsed());

    if let Some(handle) = signal_handle {
        handle.close();
    }
    if let Some(thread) = signal_thread {
        let _ = thread.join();
    }
    drop(foreground_terminal);

    if let Some(code) = status.code() {
        if code == 0 {
            Ok(())
        } else {
            Err(Error::CommandFailed {
                context: "podman exec",
                code,
            })
        }
    } else {
        use std::os::unix::process::ExitStatusExt as _;
        let code = status.signal().map(|signal| 128 + signal).unwrap_or(1);
        Err(Error::CommandFailed {
            context: "podman exec",
            code,
        })
    }
}

fn spawn_podman_exec(
    command: &mut Command,
    command_name: &str,
    cancellation_check: Option<&dyn Fn() -> Option<i32>>,
    foreground_terminal: Option<&ForegroundTerminal>,
) -> Result<Child> {
    if let Some(signal) = cancellation_check.and_then(|check| check()) {
        return Err(Error::CommandFailed {
            context: "cladding run",
            code: 128 + signal,
        });
    }

    configure_podman_process_group(command, foreground_terminal);
    Ok(command
        .spawn()
        .with_context(|| format!("failed to run podman exec for {command_name}"))?)
}

fn configure_podman_process_group(
    command: &mut Command,
    foreground_terminal: Option<&ForegroundTerminal>,
) {
    #[cfg(unix)]
    {
        if let Some(terminal) = foreground_terminal {
            terminal.configure_child(command);
        } else {
            use std::os::unix::process::CommandExt as _;
            command.process_group(0);
        }
    }
    #[cfg(not(unix))]
    let _ = (command, foreground_terminal);
}

fn wait_for_podman_exec(
    child: &mut Child,
    cancellation_check: Option<&dyn Fn() -> Option<i32>>,
) -> std::io::Result<std::process::ExitStatus> {
    loop {
        if let Some(signal) = cancellation_check.and_then(|check| check()) {
            terminate_podman_exec(child);
            return Err(std::io::Error::new(
                std::io::ErrorKind::Interrupted,
                format!("run was stopped by signal {signal}"),
            ));
        }
        if let Some(status) = child.try_wait()? {
            return Ok(status);
        }
        thread::sleep(std::time::Duration::from_millis(20));
    }
}

fn terminate_podman_exec(child: &mut Child) {
    #[cfg(unix)]
    {
        // SAFETY: the process group was created by this command immediately before spawn.
        unsafe {
            libc::kill(-(child.id() as i32), SIGTERM);
        }
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(2);
        while std::time::Instant::now() < deadline {
            if child.try_wait().ok().flatten().is_some() {
                return;
            }
            thread::sleep(std::time::Duration::from_millis(20));
        }
        // SAFETY: this is the same process group started for this command.
        unsafe {
            libc::kill(-(child.id() as i32), signal_hook::consts::signal::SIGKILL);
        }
    }
    #[cfg(not(unix))]
    {
        let _ = child.kill();
    }
    let _ = child.wait();
}

pub(super) fn cmd_reload_proxy_selected(selected: &SelectedRun) -> Result<()> {
    selected.require_ready()?;
    let socket = selected
        .run
        .control_socket
        .as_deref()
        .ok_or_else(|| Error::message("selected run has no supervisor control endpoint"))?;
    let component = selected.component("proxy")?;
    let id = component
        .id
        .as_deref()
        .ok_or_else(|| Error::message("selected proxy has no immutable container ID"))?;
    let _ = control::admit(Path::new(socket), selected.uuid(), "proxy", id)?;
    let status = Command::new("podman")
        .args(reload_proxy_args(id))
        .status()
        .with_context(|| "failed to run Baffle reload via podman exec")?;
    command_status(status, "Baffle reload via podman exec")
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct CapturedCwdMapping {
    host_source: PathBuf,
    destination: PathBuf,
    mount_type: String,
    read_only: bool,
    volume_name: Option<String>,
}

fn inspect_run_workdir(
    container_id: &str,
    role: &str,
    cwd: &Path,
    captured: Option<&str>,
    launch_identity: &ComponentMountIdentity,
) -> Result<PathBuf> {
    if launch_identity.container_id != container_id || launch_identity.role != role {
        return Err(Error::message(
            "selected run component identity changed after startup",
        ));
    }
    let output = Command::new("podman")
        .args(["inspect", container_id])
        .output()
        .with_context(|| "failed to inspect selected run component")?;
    if !output.status.success() {
        return Err(Error::message("failed to inspect selected run component"));
    }
    let document: serde_json::Value = serde_json::from_slice(&output.stdout).map_err(|_| {
        Error::message("podman inspect returned invalid JSON for selected run component")
    })?;
    let object = document
        .as_array()
        .and_then(|items| items.first())
        .or_else(|| document.as_object().map(|_| &document))
        .ok_or_else(|| Error::message("podman inspect returned no selected run component"))?;
    let config = object.get("Config").and_then(serde_json::Value::as_object);
    let target_workdir = config
        .and_then(|config| config.get("WorkingDir"))
        .and_then(serde_json::Value::as_str)
        .filter(|path| path.starts_with('/'));
    let mounts = object
        .get("Mounts")
        .and_then(serde_json::Value::as_array)
        .cloned()
        .unwrap_or_default();
    let raw_mappings = captured
        .ok_or_else(|| Error::message("selected run is missing required CWD mapping metadata"))?;
    let mappings: Vec<CapturedCwdMapping> = serde_json::from_str(raw_mappings)
        .map_err(|_| Error::message("selected run has malformed CWD mapping metadata"))?;
    let mut best_match: Option<(usize, PathBuf, PathBuf)> = None;
    let cwd = canonicalize_path_preserving_missing(cwd)
        .with_context(|| format!("failed to resolve working directory {}", cwd.display()))?;
    for mapping in mappings {
        if !mapping.host_source.is_absolute() || !mapping.destination.is_absolute() {
            return Err(Error::message(
                "selected run has a non-absolute CWD mapping",
            ));
        }
        let matching_mounts = mounts
            .iter()
            .filter(|actual| {
                actual
                    .get("Destination")
                    .and_then(serde_json::Value::as_str)
                    == mapping.destination.to_str()
            })
            .collect::<Vec<_>>();
        let [actual_mount] = matching_mounts.as_slice() else {
            return Err(Error::message(format!(
                "selected run CWD mount {} is missing or ambiguous",
                mapping.destination.display()
            )));
        };
        let actual_type = actual_mount
            .get("Type")
            .and_then(serde_json::Value::as_str)
            .ok_or_else(|| Error::message("selected run mount inspection omitted its type"))?;
        let actual_rw = actual_mount
            .get("RW")
            .and_then(serde_json::Value::as_bool)
            .ok_or_else(|| {
                Error::message("selected run mount inspection omitted its access mode")
            })?;
        let actual_read_only = !actual_rw;
        let expected_mount = launch_identity
            .mounts
            .iter()
            .find(|mount| mount.destination == mapping.destination.to_string_lossy())
            .ok_or_else(|| {
                Error::message(format!(
                    "selected run has no launch-time identity for CWD mount {}",
                    mapping.destination.display()
                ))
            })?;
        let identity_matches = inspected_cwd_mount_matches(
            actual_mount,
            &mapping,
            expected_mount,
            actual_type,
            actual_read_only,
        );
        if !identity_matches {
            return Err(Error::message(format!(
                "selected run CWD mount identity changed at {}",
                mapping.destination.display()
            )));
        }
        let source =
            canonicalize_path_preserving_missing(&mapping.host_source).with_context(|| {
                format!(
                    "failed to resolve CWD mount source {}",
                    mapping.host_source.display()
                )
            })?;
        let Ok(relative) = cwd.strip_prefix(&source) else {
            continue;
        };
        let specificity = source.as_os_str().len();
        if best_match
            .as_ref()
            .is_none_or(|(best, _, _)| specificity > *best)
        {
            best_match = Some((specificity, mapping.destination, relative.to_path_buf()));
        }
    }
    if let Some((_, destination, relative)) = best_match {
        let mut container_path = destination;
        if !relative.as_os_str().is_empty() {
            container_path.push(relative);
        }
        return Ok(container_path);
    }

    let destinations = mounts
        .iter()
        .filter_map(|mount| {
            mount
                .get("Destination")
                .and_then(serde_json::Value::as_str)
                .map(PathBuf::from)
        })
        .collect::<Vec<_>>();
    let home = Path::new(CONTAINER_HOME_DIR);
    let home_available = target_workdir.is_some_and(|workdir| {
        Path::new(workdir).is_absolute() && Path::new(workdir).starts_with(home)
    }) || destinations
        .iter()
        .any(|destination| destination.starts_with(home) || home.starts_with(destination));
    if home_available {
        return Ok(home.to_path_buf());
    }
    if let Some(workdir) = target_workdir
        && Path::new(workdir).is_absolute()
    {
        return Ok(PathBuf::from(workdir));
    }
    let _ = role;
    Err(Error::message(
        "selected runtime target has no safe fallback working directory",
    ))
}

fn inspected_cwd_mount_matches(
    actual_mount: &serde_json::Value,
    mapping: &CapturedCwdMapping,
    expected: &MountIdentity,
    actual_type: &str,
    actual_read_only: bool,
) -> bool {
    if expected.destination != mapping.destination.to_string_lossy()
        || expected.read_only != mapping.read_only
        || actual_read_only != mapping.read_only
        || expected.mount_type != actual_type
    {
        return false;
    }
    match mapping.mount_type.as_str() {
        "bind" | "readonly" => {
            actual_type == "bind"
                && actual_mount
                    .get("Source")
                    .and_then(serde_json::Value::as_str)
                    == expected.source.as_deref()
        }
        "copy" => {
            actual_type == "volume"
                && mapping.volume_name.as_deref().is_some_and(|name| {
                    actual_mount.get("Name").and_then(serde_json::Value::as_str) == Some(name)
                        && expected.name.as_deref() == Some(name)
                })
        }
        _ => false,
    }
}

fn command_status(status: std::process::ExitStatus, context: &'static str) -> Result<()> {
    if status.success() {
        return Ok(());
    }
    #[cfg(unix)]
    {
        use std::os::unix::process::ExitStatusExt as _;
        let code = status
            .code()
            .unwrap_or_else(|| 128 + status.signal().unwrap_or(1));
        Err(Error::CommandFailed { context, code })
    }
    #[cfg(not(unix))]
    Err(Error::CommandFailed {
        context,
        code: status.code().unwrap_or(1),
    })
}

fn reload_proxy_args(container_id: &str) -> [String; 5] {
    [
        "exec".to_string(),
        container_id.to_string(),
        // The proxy image does not add /opt/tools/bin to PATH.
        "/opt/tools/bin/baffle".to_string(),
        "reload".to_string(),
        "--all".to_string(),
    ]
}

fn project_component_name(project_name: &str, component: &str) -> String {
    format!("{project_name}-{component}")
}

#[cfg(test)]
mod exec_tests {
    use super::{
        CapturedCwdMapping, inspected_cwd_mount_matches, reload_proxy_args, spawn_podman_exec,
    };
    use crate::cli::control::MountIdentity;
    use signal_hook::consts::signal::SIGTERM;
    use std::fs;
    use std::process::Command;
    use std::time::{SystemTime, UNIX_EPOCH};

    #[test]
    fn reload_proxy_targets_all_baffle_sessions_in_the_proxy_container() {
        assert_eq!(
            reload_proxy_args("immutable-container-id"),
            [
                "exec",
                "immutable-container-id",
                "/opt/tools/bin/baffle",
                "reload",
                "--all"
            ]
        );
    }

    #[test]
    fn bind_cwd_mapping_checks_engine_source_separately_from_host_translation() {
        let mapping = CapturedCwdMapping {
            host_source: std::path::PathBuf::from("/Users/alice/project"),
            destination: std::path::PathBuf::from("/workspace"),
            mount_type: "bind".to_string(),
            read_only: false,
            volume_name: None,
        };
        let launch_identity = MountIdentity {
            destination: "/workspace".to_string(),
            mount_type: "bind".to_string(),
            source: Some("/var/home/user/project".to_string()),
            name: None,
            read_only: false,
        };
        let actual = serde_json::json!({
            "Destination": "/workspace",
            "Type": "bind",
            "Source": "/var/home/user/project",
            "RW": true,
        });

        assert!(inspected_cwd_mount_matches(
            &actual,
            &mapping,
            &launch_identity,
            "bind",
            false,
        ));
        let host_cwd = std::path::Path::new("/Users/alice/project/src")
            .strip_prefix(&mapping.host_source)
            .unwrap();
        assert_eq!(
            mapping.destination.join(host_cwd),
            std::path::PathBuf::from("/workspace/src")
        );

        let replaced = serde_json::json!({
            "Destination": "/workspace",
            "Type": "bind",
            "Source": "/var/home/user/other-project",
            "RW": true,
        });
        assert!(!inspected_cwd_mount_matches(
            &replaced,
            &mapping,
            &launch_identity,
            "bind",
            false,
        ));
    }

    #[cfg(unix)]
    #[test]
    fn run_checks_cancellation_immediately_before_spawning_the_workload() {
        let unique = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let marker = std::env::temp_dir().join(format!(
            "cladding-cancelled-workload-{}-{unique}",
            std::process::id()
        ));
        let mut command = Command::new("/bin/sh");
        command
            .args(["-c", "touch \"$1\"", "cancelled-workload"])
            .arg(&marker);
        let cancelled = || Some(SIGTERM);

        let result = spawn_podman_exec(&mut command, "run", Some(&cancelled), None);

        assert!(result.is_err());
        assert!(!marker.exists(), "the cancelled workload must not start");
        let _ = fs::remove_file(marker);
    }
}

fn runtime_component_for_target(
    runtime_spec: &RuntimeSpec,
    target: MountTarget,
) -> Result<&RuntimeComponent> {
    let component = match target {
        MountTarget::Agent => Some(&runtime_spec.agent),
        MountTarget::NwSandbox => runtime_spec.nw_sandbox.as_ref(),
        MountTarget::FsSandbox => runtime_spec.fs_sandbox.as_ref(),
    };
    component.ok_or_else(|| {
        Error::message(format!(
            "runtime target '{}' is not configured",
            mount_target_name(target)
        ))
    })
}

fn mount_target_name(target: MountTarget) -> &'static str {
    match target {
        MountTarget::Agent => "agent",
        MountTarget::NwSandbox => "nw-sandbox",
        MountTarget::FsSandbox => "fs-sandbox",
    }
}

fn resolve_target_workdir(
    runtime_mounts: &[RuntimeMount],
    configured_mounts: &[ResolvedMountConfig],
    target: MountTarget,
    target_workdir: Option<&str>,
    cwd: &Path,
) -> Result<PathBuf> {
    let mounts = active_explicit_host_mounts(runtime_mounts, configured_mounts, target);
    resolve_container_workdir(&mounts, target_workdir, cwd)
}

fn active_explicit_host_mounts(
    runtime_mounts: &[RuntimeMount],
    configured_mounts: &[ResolvedMountConfig],
    target: MountTarget,
) -> Vec<RuntimeMount> {
    runtime_mounts
        .iter()
        .filter(|runtime_mount| {
            configured_mounts.iter().any(|configured_mount| {
                if configured_mount.ignore
                    || !configured_mount.targets.contains(&target)
                    || configured_mount.mount_path != runtime_mount.mount_path
                {
                    return false;
                }

                let Some(host_path) = configured_mount.host_path.as_deref() else {
                    return false;
                };

                match (configured_mount.mount_type, &runtime_mount.source) {
                    (
                        MountType::Bind | MountType::Readonly,
                        RuntimeMountSource::HostPath { path },
                    ) => path == host_path,
                    (
                        MountType::Copy,
                        RuntimeMountSource::ManagedVolume {
                            kind: ManagedVolumeKind::Copy { source },
                            ..
                        },
                    ) => source == host_path,
                    _ => false,
                }
            })
        })
        .cloned()
        .collect()
}

fn resolve_container_workdir(
    mounts: &[RuntimeMount],
    target_workdir: Option<&str>,
    cwd: &Path,
) -> Result<PathBuf> {
    let cwd = canonicalize_path_preserving_missing(cwd)
        .with_context(|| format!("failed to resolve working directory {}", cwd.display()))?;

    let mut best_match: Option<(usize, &RuntimeMount, PathBuf)> = None;
    for mount in mounts {
        let Some(host_path) = host_path_for_mount(mount) else {
            continue;
        };
        let host_path = canonicalize_path_preserving_missing(host_path).with_context(|| {
            format!(
                "failed to resolve host source for container mount '{}' at {}",
                mount.mount_path,
                host_path.display()
            )
        })?;
        let Ok(relative_path) = cwd.strip_prefix(&host_path) else {
            continue;
        };
        let specificity = host_path.as_os_str().len();
        if best_match
            .as_ref()
            .is_none_or(|(best_specificity, _, _)| specificity > *best_specificity)
        {
            best_match = Some((specificity, mount, relative_path.to_path_buf()));
        }
    }

    if let Some((_, mount, relative_path)) = best_match {
        let mut container_path = PathBuf::from(&mount.mount_path);
        if !relative_path.as_os_str().is_empty() {
            container_path.push(relative_path);
        }
        return Ok(container_path);
    }

    safe_fallback_workdir(mounts, target_workdir).ok_or_else(|| {
        Error::message("selected runtime target has no safe fallback working directory")
    })
}

fn host_path_for_mount(mount: &RuntimeMount) -> Option<&Path> {
    match &mount.source {
        RuntimeMountSource::HostPath { path } => Some(path),
        RuntimeMountSource::ManagedVolume {
            kind: ManagedVolumeKind::Copy { source },
            ..
        } => Some(source),
        RuntimeMountSource::NamedVolume { .. }
        | RuntimeMountSource::NamedVolumeChown { .. }
        | RuntimeMountSource::ManagedVolume { .. }
        | RuntimeMountSource::GeneratedEmptyMask { .. }
        | RuntimeMountSource::Tmpfs { .. }
        | RuntimeMountSource::EmptyDir => None,
    }
}

fn canonicalize_path_preserving_missing(path: &Path) -> anyhow::Result<PathBuf> {
    let absolute_path = if path.is_absolute() {
        path.to_path_buf()
    } else {
        env::current_dir()?.join(path)
    };
    let mut current = absolute_path.as_path();
    let mut missing_suffix = Vec::new();

    loop {
        match fs::canonicalize(current) {
            Ok(mut canonical) => {
                for component in missing_suffix.iter().rev() {
                    canonical.push(component);
                }
                return Ok(canonical);
            }
            Err(error) if error.kind() == io::ErrorKind::NotFound => {
                let Some(file_name) = current.file_name() else {
                    return Err(error.into());
                };
                missing_suffix.push(file_name.to_os_string());
                let Some(parent) = current.parent() else {
                    return Err(error.into());
                };
                current = parent;
            }
            Err(error) => return Err(error.into()),
        }
    }
}

fn safe_fallback_workdir(mounts: &[RuntimeMount], target_workdir: Option<&str>) -> Option<PathBuf> {
    let home = Path::new(CONTAINER_HOME_DIR);
    let home_available = target_workdir.is_some_and(|workdir| {
        Path::new(workdir).is_absolute() && Path::new(workdir).starts_with(home)
    }) || mounts.iter().any(|mount| {
        let destination = Path::new(&mount.mount_path);
        destination.starts_with(home) || home.starts_with(destination)
    });

    if home_available {
        return Some(home.to_path_buf());
    }

    target_workdir
        .map(Path::new)
        .filter(|workdir| workdir.is_absolute())
        .map(Path::to_path_buf)
}

#[cfg(test)]
mod tests {
    use super::*;
    use cladding::config::{ExecutionComponentConfig, MountType};
    use std::fs;
    use std::sync::atomic::{AtomicU64, Ordering};

    const CONTAINER_WORKSPACE_DIR: &str = "/home/user/workspace";

    static TEMP_DIR_COUNTER: AtomicU64 = AtomicU64::new(0);

    #[test]
    fn runtime_container_name_matches_direct_runtime_names() {
        assert_eq!(
            runtime_container_name("demo-fs-sandbox"),
            "demo-fs-sandbox-instance"
        );
        assert_eq!(runtime_container_name("demo-agent"), "demo-agent-instance");
    }

    fn resolve_with_runtime_mounts(
        mounts: &[RuntimeMount],
        target_workdir: Option<&str>,
        cwd: &Path,
    ) -> Result<PathBuf> {
        resolve_container_workdir(mounts, target_workdir, cwd)
    }

    #[test]
    fn resolve_container_workdir_uses_home_when_workspace_is_not_mounted() {
        let temp = create_temp_dir("default-workdir");
        let workspace = temp.join("workspace");
        let project_root = workspace.join(".cladding");
        let cwd = workspace.join("src/module");
        fs::create_dir_all(&cwd).expect("create nested workspace directory");
        fs::create_dir_all(&project_root).expect("create project root");

        let runtime = runtime_spec(&project_root, Vec::new(), false);
        let container = &runtime.agent.container;
        let resolved = resolve_target_workdir(
            &container.mounts,
            &[],
            MountTarget::Agent,
            container.workdir.as_deref(),
            &cwd,
        )
        .expect("workdir");

        assert_eq!(resolved, PathBuf::from(CONTAINER_HOME_DIR));
    }

    #[test]
    fn resolve_container_workdir_uses_home_for_a_cwd_in_the_default_home_mount() {
        let temp = create_temp_dir("default-home");
        let workspace = temp.join("workspace");
        let project_root = workspace.join(".cladding");
        let cwd = project_root.join("home/packages");
        fs::create_dir_all(&cwd).expect("create home directory");
        fs::create_dir_all(&project_root).expect("create project root");

        let runtime = runtime_spec(&project_root, Vec::new(), false);
        let container = &runtime.agent.container;
        let resolved = resolve_target_workdir(
            &container.mounts,
            &[],
            MountTarget::Agent,
            container.workdir.as_deref(),
            &cwd,
        )
        .expect("workdir");

        assert_eq!(resolved, PathBuf::from(CONTAINER_HOME_DIR));
    }

    #[cfg(unix)]
    #[test]
    fn resolve_container_workdir_uses_home_for_a_cwd_in_symlinked_home() {
        let temp = create_temp_dir("symlinked-home");
        let workspace = temp.join("workspace");
        let project_root = workspace.join(".cladding");
        let real_home = temp.join("shared-home");
        let cwd = real_home.join("projects/sample");
        fs::create_dir_all(&cwd).expect("create real home directory");
        fs::create_dir_all(&project_root).expect("create project root");
        std::os::unix::fs::symlink(&real_home, project_root.join("home"))
            .expect("link configured home source");

        let runtime = runtime_spec(&project_root, Vec::new(), false);
        let container = &runtime.agent.container;
        let resolved = resolve_target_workdir(
            &container.mounts,
            &[],
            MountTarget::Agent,
            container.workdir.as_deref(),
            &cwd,
        )
        .expect("workdir");

        assert_eq!(resolved, PathBuf::from(CONTAINER_HOME_DIR));
    }

    #[test]
    fn resolve_container_workdir_does_not_resolve_through_an_ignored_workspace() {
        let temp = create_temp_dir("ignored-workspace");
        let workspace = temp.join("workspace");
        let project_root = workspace.join(".cladding");
        let cwd = workspace.join("src");
        fs::create_dir_all(&cwd).expect("create workspace directory");
        fs::create_dir_all(&project_root).expect("create project root");

        let ignored_workspace = ResolvedMountConfig {
            mount_path: CONTAINER_WORKSPACE_DIR.to_string(),
            host_path: None,
            volume: None,
            mount_type: MountType::Bind,
            tmpfs_size_bytes: None,
            read_only: false,
            targets: vec![MountTarget::Agent],
            ignore: true,
        };
        let runtime = runtime_spec(&project_root, vec![ignored_workspace.clone()], false);
        let container = &runtime.agent.container;
        let resolved = resolve_target_workdir(
            &container.mounts,
            &[ignored_workspace],
            MountTarget::Agent,
            container.workdir.as_deref(),
            &cwd,
        )
        .expect("fallback workdir");

        assert_eq!(resolved, PathBuf::from(CONTAINER_HOME_DIR));
    }

    #[test]
    fn resolve_container_workdir_uses_the_filesystem_sandbox_mounts() {
        let temp = create_temp_dir("fs-sandbox-mounts");
        let workspace = temp.join("workspace");
        let project_root = workspace.join(".cladding");
        let cwd = workspace.join("src/module");
        fs::create_dir_all(&cwd).expect("create workspace directory");
        fs::create_dir_all(&project_root).expect("create project root");

        let runtime = runtime_spec(&project_root, Vec::new(), true);
        let component = runtime_component_for_target(&runtime, MountTarget::FsSandbox)
            .expect("filesystem sandbox component");
        let container = &component.container;
        let resolved = resolve_target_workdir(
            &container.mounts,
            &[],
            MountTarget::FsSandbox,
            container.workdir.as_deref(),
            &cwd,
        )
        .expect("fallback workdir");

        assert_eq!(resolved, PathBuf::from(CONTAINER_HOME_DIR));
    }

    #[test]
    fn resolve_container_workdir_maps_custom_copy_mount_sources() {
        let temp = create_temp_dir("custom-mount");
        let workspace = temp.join("workspace");
        let project_root = workspace.join(".cladding");
        let data = temp.join("data");
        let cwd = data.join("reports/weekly");
        fs::create_dir_all(&cwd).expect("create custom mount directory");
        fs::create_dir_all(&project_root).expect("create project root");
        let custom_mount = ResolvedMountConfig {
            mount_path: "/data".to_string(),
            host_path: Some(data),
            volume: None,
            mount_type: MountType::Copy,
            tmpfs_size_bytes: None,
            read_only: false,
            targets: vec![MountTarget::Agent],
            ignore: false,
        };
        let runtime = runtime_spec(&project_root, vec![custom_mount.clone()], false);
        let container = &runtime.agent.container;

        let resolved = resolve_target_workdir(
            &container.mounts,
            &[custom_mount],
            MountTarget::Agent,
            container.workdir.as_deref(),
            &cwd,
        )
        .expect("workdir");

        assert_eq!(resolved, PathBuf::from("/data/reports/weekly"));
    }

    #[test]
    fn resolve_container_workdir_maps_an_explicit_host_workspace_mount() {
        let temp = create_temp_dir("explicit-workspace-mount");
        let workspace = temp.join("workspace");
        let project_root = workspace.join(".cladding");
        let cwd = workspace.join("src/module");
        fs::create_dir_all(&cwd).expect("create nested workspace directory");
        fs::create_dir_all(&project_root).expect("create project root");
        let explicit_mount = ResolvedMountConfig {
            mount_path: "/host-workspace".to_string(),
            host_path: Some(workspace.clone()),
            volume: None,
            mount_type: MountType::Bind,
            tmpfs_size_bytes: None,
            read_only: false,
            targets: vec![MountTarget::Agent],
            ignore: false,
        };
        let runtime = runtime_spec(&project_root, vec![explicit_mount.clone()], false);
        let container = &runtime.agent.container;

        let resolved = resolve_target_workdir(
            &container.mounts,
            std::slice::from_ref(&explicit_mount),
            MountTarget::Agent,
            container.workdir.as_deref(),
            &cwd,
        )
        .expect("workdir");

        assert_eq!(resolved, PathBuf::from("/host-workspace/src/module"));

        let resolved_for_other_target = resolve_target_workdir(
            &container.mounts,
            std::slice::from_ref(&explicit_mount),
            MountTarget::NwSandbox,
            container.workdir.as_deref(),
            &cwd,
        )
        .expect("fallback workdir for other target");
        assert_eq!(resolved_for_other_target, PathBuf::from(CONTAINER_HOME_DIR));
    }

    #[cfg(unix)]
    #[test]
    fn prepared_runtime_rejects_a_custom_mount_symlink_retargeted_after_preparation() {
        let temp = create_temp_dir("changed-custom-mount-symlink");
        let project_root = temp.join("project/.cladding");
        let original_target = temp.join("original");
        let changed_target = project_root.join("private");
        let link = temp.join("mount-source");
        fs::create_dir_all(&project_root).expect("create project root");
        fs::create_dir_all(&original_target).expect("create original target");
        fs::create_dir_all(&changed_target).expect("create changed target");
        std::os::unix::fs::symlink(&original_target, &link).expect("create mount symlink");

        let configured_mount = ResolvedMountConfig {
            mount_path: "/workspace".to_string(),
            host_path: Some(link.clone()),
            volume: None,
            mount_type: MountType::Bind,
            tmpfs_size_bytes: None,
            read_only: false,
            targets: vec![MountTarget::Agent],
            ignore: false,
        };
        let spec = runtime_spec(&project_root, vec![configured_mount.clone()], false);

        fs::remove_file(&link).expect("remove original symlink");
        std::os::unix::fs::symlink(&changed_target, &link).expect("retarget mount symlink");

        let error = spec
            .validate_custom_mount_sources()
            .expect_err("changed custom mount source must be rejected");
        assert!(
            error
                .to_string()
                .contains("changed after runtime preparation")
        );

        let agent = &spec.agent.container;
        let cwd = changed_target.join("secrets");
        fs::create_dir_all(&cwd).expect("create path under changed target");
        assert_eq!(
            resolve_target_workdir(
                &agent.mounts,
                &[configured_mount],
                MountTarget::Agent,
                agent.workdir.as_deref(),
                &cwd,
            )
            .expect("workdir mapping uses current symlink target"),
            PathBuf::from("/workspace/secrets")
        );
        let _ = fs::remove_dir_all(temp);
    }

    #[test]
    fn resolve_container_workdir_uses_the_most_specific_overlapping_host_mount() {
        let temp = create_temp_dir("overlapping-mounts");
        let root = temp.join("data");
        let nested = root.join("project");
        let cwd = nested.join("src");
        fs::create_dir_all(&cwd).expect("create nested custom mount directory");
        let mounts = vec![host_mount("/data", root), host_mount("/project", nested)];

        let resolved = resolve_with_runtime_mounts(&mounts, Some("/home/user/workspace"), &cwd)
            .expect("workdir");

        assert_eq!(resolved, PathBuf::from("/project/src"));
    }

    #[test]
    fn resolve_container_workdir_preserves_a_missing_cwd_suffix() {
        let temp = create_temp_dir("missing-cwd-suffix");
        let root = temp.join("data");
        fs::create_dir_all(&root).expect("create custom mount source");
        let cwd = root.join("new/child");
        let mounts = vec![host_mount("/data", root)];

        let resolved = resolve_with_runtime_mounts(&mounts, Some("/home/user/workspace"), &cwd)
            .expect("workdir");

        assert_eq!(resolved, PathBuf::from("/data/new/child"));
    }

    #[test]
    fn resolve_container_workdir_ignores_non_host_mount_sources() {
        let temp = create_temp_dir("non-host-mounts");
        let cwd = temp.join("volume-source-lookalike");
        fs::create_dir_all(&cwd).expect("create cwd");
        let mounts = vec![
            RuntimeMount {
                mount_path: "/volume".to_string(),
                read_only: false,
                source: RuntimeMountSource::NamedVolume {
                    claim_name: "project-cache".to_string(),
                },
            },
            RuntimeMount {
                mount_path: "/tmp/data".to_string(),
                read_only: false,
                source: RuntimeMountSource::Tmpfs { size_bytes: None },
            },
            RuntimeMount {
                mount_path: "/empty".to_string(),
                read_only: false,
                source: RuntimeMountSource::EmptyDir,
            },
        ];

        let resolved = resolve_with_runtime_mounts(&mounts, Some("/home/user/workspace"), &cwd)
            .expect("fallback workdir");

        assert_eq!(resolved, PathBuf::from(CONTAINER_HOME_DIR));
    }

    #[test]
    fn resolve_container_workdir_uses_a_target_fallback_when_no_mount_matches() {
        let temp = create_temp_dir("no-mount-match");
        let cwd = temp.join("outside");
        fs::create_dir_all(&cwd).expect("create outside directory");

        let resolved = resolve_with_runtime_mounts(&[], Some("/home/user/workspace"), &cwd)
            .expect("fallback workdir");

        assert_eq!(resolved, PathBuf::from(CONTAINER_HOME_DIR));
    }

    #[test]
    fn resolve_container_workdir_errors_when_the_target_has_no_safe_fallback() {
        let temp = create_temp_dir("no-safe-fallback");
        let cwd = temp.join("outside");
        fs::create_dir_all(&cwd).expect("create outside directory");
        let mounts = vec![host_mount("/mnt/data", temp.join("other"))];

        let error = resolve_with_runtime_mounts(&mounts, None, &cwd).expect_err("missing fallback");

        assert!(
            error
                .to_string()
                .contains("no safe fallback working directory")
        );
    }

    fn runtime_spec(
        project_root: &Path,
        mounts: Vec<ResolvedMountConfig>,
        fs_sandbox_enabled: bool,
    ) -> RuntimeSpec {
        let config = ExecutionConfig {
            name: "demo".to_string(),
            use_runsc: false,
            fake_credential_env: std::collections::BTreeMap::new(),
            agent: ExecutionComponentConfig {
                enabled: true,
                init: true,
                image: "agent:image".to_string(),
                build: None,
                security_opts: Vec::new(),
            },
            nw_sandbox: None,
            fs_sandbox: fs_sandbox_enabled.then_some(ExecutionComponentConfig {
                enabled: true,
                init: true,
                image: "fs-sandbox:image".to_string(),
                build: None,
                security_opts: Vec::new(),
            }),
            proxy: None,
            mounts,
        };
        RuntimeSpec::build(project_root, &config)
    }

    fn host_mount(mount_path: &str, host_path: PathBuf) -> RuntimeMount {
        RuntimeMount {
            mount_path: mount_path.to_string(),
            read_only: false,
            source: RuntimeMountSource::HostPath { path: host_path },
        }
    }

    fn create_temp_dir(name: &str) -> PathBuf {
        let unique = TEMP_DIR_COUNTER.fetch_add(1, Ordering::Relaxed);
        let path =
            env::temp_dir().join(format!("cladding-{name}-{}-{}", std::process::id(), unique));
        if path.exists() {
            fs::remove_dir_all(&path).expect("cleanup stale temp dir");
        }
        fs::create_dir_all(&path).expect("create temp dir");
        path
    }
}
