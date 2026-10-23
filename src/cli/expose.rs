use super::args::ExposeArgs;
use super::control;
use super::selection::SelectedRun;
use anyhow::Context as _;
use cladding::error::{Error, Result};
use cladding::fs_utils::command_exists_in_path;
use cladding::podman::podman_required;
use signal_hook::consts::signal::{SIGINT, SIGTERM};
use signal_hook::iterator::Signals;
use std::net::IpAddr;
use std::process::{Command, Stdio};
use std::thread;
use std::time::{Duration, Instant};

pub(super) fn cmd_expose_selected(selected: &SelectedRun, args: &ExposeArgs) -> Result<()> {
    podman_required("podman (required for cladding expose)")?;
    socat_required("expose")?;
    selected.require_ready()?;
    let socket = selected
        .run
        .control_socket
        .as_deref()
        .ok_or_else(|| Error::message("selected run has no supervisor control endpoint"))?;
    let _ = control::admit(std::path::Path::new(socket), selected.uuid(), "agent")?;
    let agent = selected.component("agent")?;
    let id = agent
        .id
        .as_deref()
        .ok_or_else(|| Error::message("selected agent has no immutable container ID"))?;
    let host_port = args.host_port.unwrap_or(args.container_port);
    let mut command =
        build_blocking_expose_command(id, args.container_port, host_port, args.bind_address);
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt as _;
        command.process_group(0);
    }
    let mut child = command
        .stdin(Stdio::inherit())
        .stdout(Stdio::inherit())
        .stderr(Stdio::inherit())
        .spawn()
        .with_context(|| "failed to run socat")?;
    let signals = match Signals::new([SIGINT, SIGTERM]) {
        Ok(signals) => signals,
        Err(error) => {
            stop_listener_process(&mut child);
            return Err(anyhow::Error::new(error)
                .context("failed to install expose signal handlers")
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
    let socket = selected.run.control_socket.as_deref().unwrap_or_default();
    let mut last_control_check = Instant::now();
    let result = loop {
        match child.try_wait() {
            Ok(Some(status)) => break cladding::podman::ensure_success(status, "socat"),
            Ok(None) => {}
            Err(error) => {
                stop_listener_process(&mut child);
                break Err(Error::message(format!("failed to wait for socat: {error}")));
            }
        }
        if let Ok(signal) = signal_rx.try_recv() {
            stop_listener_process(&mut child);
            break Err(Error::CommandFailed {
                context: "socat",
                code: 128 + signal,
            });
        }
        if last_control_check.elapsed() >= Duration::from_millis(200) {
            last_control_check = Instant::now();
            match control::status(std::path::Path::new(socket), selected.uuid()) {
                Ok(status) if status.state == "ready" => {}
                Ok(_) | Err(_) => {
                    stop_listener_process(&mut child);
                    break Err(Error::message(
                        "selected run stopped or lost its supervisor; expose listener was closed",
                    ));
                }
            }
        }
        thread::sleep(Duration::from_millis(25));
    };
    handle.close();
    let _ = signal_thread.join();
    result
}

fn stop_listener_process(child: &mut std::process::Child) {
    #[cfg(unix)]
    {
        // SAFETY: this process group was created for the socat listener above.
        unsafe {
            libc::kill(-(child.id() as i32), signal_hook::consts::signal::SIGTERM);
        }
        let deadline = Instant::now() + Duration::from_secs(2);
        while Instant::now() < deadline {
            if child.try_wait().ok().flatten().is_some() {
                return;
            }
            thread::sleep(Duration::from_millis(20));
        }
        // SAFETY: this is the same listener process group.
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

pub(super) fn socat_required(command_name: &str) -> Result<()> {
    if command_exists_in_path("socat", std::env::var_os("PATH").as_deref()) {
        Ok(())
    } else {
        eprintln!("missing: socat (required for cladding {command_name})");
        Err(Error::message("missing socat"))
    }
}

fn build_blocking_expose_command(
    agent_container_name: &str,
    container_port: u16,
    host_port: u16,
    bind_address: IpAddr,
) -> Command {
    let mut cmd = Command::new("socat");
    let listener = match bind_address {
        IpAddr::V4(_) => "TCP4-LISTEN",
        IpAddr::V6(_) => "TCP6-LISTEN",
    };
    cmd.arg(format!(
        "{listener}:{host_port},bind={bind_address},reuseaddr,fork"
    ));
    let agent_container_name = shell_single_quote(agent_container_name);
    cmd.arg(format!(
        "EXEC:podman exec -i {agent_container_name} socat STDIO TCP\\:127.0.0.1\\:{container_port}"
    ));
    cmd
}

fn shell_single_quote(value: &str) -> String {
    let escaped = value.replace('\'', "'\\''");
    format!("'{escaped}'")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn command_args(cmd: &Command) -> Vec<String> {
        cmd.get_args()
            .map(|arg| arg.to_string_lossy().into_owned())
            .collect()
    }

    #[test]
    fn build_blocking_expose_command_targets_the_selected_agent_without_starting_a_runtime() {
        let cmd = build_blocking_expose_command(
            "immutable-agent-id",
            5432,
            15432,
            "127.0.0.1".parse::<IpAddr>().unwrap(),
        );
        let args = command_args(&cmd);

        assert_eq!(cmd.get_program().to_string_lossy(), "socat");
        assert_eq!(args.len(), 2);
        assert_eq!(args[0], "TCP4-LISTEN:15432,bind=127.0.0.1,reuseaddr,fork");
        assert_eq!(
            args[1],
            "EXEC:podman exec -i 'immutable-agent-id' socat STDIO TCP\\:127.0.0.1\\:5432"
        );
        assert!(!args.iter().any(|arg| arg.contains("cladding run")));
    }

    #[test]
    fn build_blocking_expose_command_uses_ipv6_listener_for_ipv6_bind() {
        let cmd = build_blocking_expose_command(
            "demo-agent-instance",
            3000,
            9000,
            "::1".parse::<IpAddr>().unwrap(),
        );
        let args = command_args(&cmd);

        assert_eq!(args[0], "TCP6-LISTEN:9000,bind=::1,reuseaddr,fork");
    }

    #[test]
    fn shell_single_quote_escapes_agent_names() {
        assert_eq!(
            shell_single_quote("agent's-container"),
            "'agent'\\''s-container'"
        );
    }
}
