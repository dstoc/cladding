use super::args::{InjectArgs, InjectHostEndpoint};
use super::control;
use super::selection::SelectedRun;
use anyhow::Context as _;
use cladding::error::{Error, Result};
use cladding::podman::podman_required;
use cladding_bridge::{BridgeError, HostTarget, run_host_session};
use signal_hook::consts::signal::{SIGINT, SIGTERM};
use signal_hook::iterator::Signals;
use std::process::ExitStatus;
use std::process::Stdio;
use std::thread;
use std::time::Duration;
use tokio::process::{Child, Command};

const BRIDGE_BINARY: &str = "/opt/tools/bin/cladding-bridge";
const EXEC_GRACE_PERIOD: Duration = Duration::from_secs(5);

pub(super) fn cmd_inject_selected(selected: &SelectedRun, args: &InjectArgs) -> Result<()> {
    podman_required("podman (required for cladding inject)")?;
    selected.require_ready()?;
    let agent = selected.component("agent")?;
    let agent_id = agent
        .id
        .as_deref()
        .ok_or_else(|| Error::message("selected agent has no immutable container ID"))?;
    let socket = selected
        .run
        .control_socket
        .as_deref()
        .ok_or_else(|| Error::message("selected run has no supervisor control endpoint"))?;
    let _ = control::admit(std::path::Path::new(socket), selected.uuid(), "agent")?;
    if !agent_bridge_available(agent_id)? {
        return Err(Error::message(format!(
            "selected agent is missing {BRIDGE_BINARY}"
        )));
    }
    let container_port = args.container_port.unwrap_or(args.host_endpoint.port);
    run_inject_bridge_with_control(
        agent_id,
        &args.host_endpoint,
        container_port,
        (
            std::path::PathBuf::from(socket),
            selected.uuid().to_string(),
        ),
    )
}

fn agent_bridge_available(agent_container_name: &str) -> Result<bool> {
    let status = build_agent_bridge_check_command(agent_container_name)
        .status()
        .with_context(|| "failed to check cladding-bridge availability")?;

    Ok(status.success())
}

fn build_agent_bridge_check_command(agent_container_name: &str) -> std::process::Command {
    let mut cmd = std::process::Command::new("podman");
    cmd.args(["exec", agent_container_name, "test", "-x", BRIDGE_BINARY]);
    cmd
}

fn build_inject_agent_exec_command(agent_container_name: &str, container_port: u16) -> Command {
    let mut cmd = Command::new("podman");
    cmd.args([
        "exec",
        "-i",
        agent_container_name,
        BRIDGE_BINARY,
        "listen",
        "--tcp",
    ])
    .arg(format!("127.0.0.1:{container_port}"))
    .stdin(Stdio::piped())
    .stdout(Stdio::piped())
    .stderr(Stdio::inherit())
    .kill_on_drop(true);
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt as _;
        cmd.as_std_mut().process_group(0);
    }
    cmd
}

fn host_target(endpoint: &InjectHostEndpoint) -> HostTarget {
    HostTarget::tcp(endpoint.host.clone(), endpoint.port)
}

fn run_inject_bridge_with_control(
    agent_container_name: &str,
    endpoint: &InjectHostEndpoint,
    container_port: u16,
    control_endpoint: (std::path::PathBuf, String),
) -> Result<()> {
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .with_context(|| "failed to create inject bridge runtime")?;

    runtime.block_on(run_inject_bridge_async_with_control(
        agent_container_name,
        endpoint,
        container_port,
        control_endpoint,
    ))
}

async fn run_inject_bridge_async_with_control(
    agent_container_name: &str,
    endpoint: &InjectHostEndpoint,
    container_port: u16,
    control_endpoint: (std::path::PathBuf, String),
) -> Result<()> {
    let signals = Signals::new([SIGINT, SIGTERM])
        .with_context(|| "failed to install inject signal handlers")?;
    let mut child = build_inject_agent_exec_command(agent_container_name, container_port)
        .spawn()
        .with_context(|| "failed to run podman exec for cladding inject")?;

    let stdout = child
        .stdout
        .take()
        .context("podman exec did not provide a stdout pipe")?;
    let stdin = child
        .stdin
        .take()
        .context("podman exec did not provide a stdin pipe")?;
    let transport = tokio::io::join(stdout, stdin);

    let (signal_tx, signal_rx) = tokio::sync::oneshot::channel();
    let signal_handle = signals.handle();
    let signal_thread = thread::spawn(move || {
        let mut signals = signals;
        if signals.forever().next().is_some() {
            let _ = signal_tx.send(());
        }
    });

    enum SessionEnd {
        Interrupted,
        RunEnded,
        Transport(std::result::Result<(), cladding_bridge::BridgeError>),
    }

    let session_end = {
        let session = run_host_session(transport, host_target(endpoint));
        tokio::pin!(session);
        let run_end = wait_for_run_end(control_endpoint);
        tokio::pin!(run_end);
        tokio::select! {
            result = &mut session => SessionEnd::Transport(result),
            _ = signal_rx => SessionEnd::Interrupted,
            _ = &mut run_end => SessionEnd::RunEnded,
        }
    };

    // Dropping the session closes podman exec stdin. The remote helper sees EOF
    // and exits before the bounded child wait below completes.
    signal_handle.close();
    let _ = signal_thread.join();

    let (status, forced) = wait_for_inject_exec(&mut child).await?;
    match session_end {
        SessionEnd::Interrupted => {
            if forced {
                eprintln!("warning: terminated podman exec after its grace period");
            }
            Ok(())
        }
        SessionEnd::RunEnded => Err(Error::message(
            "selected run stopped or lost its supervisor; inject bridge was closed",
        )),
        SessionEnd::Transport(Err(error)) => Err(inject_transport_error(error)),
        SessionEnd::Transport(Ok(())) if forced => Err(Error::message(
            "podman exec did not exit after the bridge transport closed",
        )),
        SessionEnd::Transport(Ok(())) if !status.success() => Err(Error::CommandFailed {
            context: "podman exec",
            code: status.code().unwrap_or(1),
        }),
        SessionEnd::Transport(Ok(())) => Err(Error::message(
            "inject bridge transport closed unexpectedly",
        )),
    }
}

async fn wait_for_run_end(endpoint: (std::path::PathBuf, String)) {
    let (socket, uuid) = endpoint;
    loop {
        tokio::time::sleep(Duration::from_millis(150)).await;
        match control::status(&socket, &uuid) {
            Ok(status) if status.state == "ready" => {}
            _ => return,
        }
    }
}

fn inject_transport_error(error: BridgeError) -> Error {
    let context = if matches!(&error, BridgeError::ListenerBindFailure) {
        "cladding inject agent-side listener failed to bind"
    } else {
        "cladding inject bridge transport failed"
    };
    anyhow::Error::new(error).context(context).into()
}

async fn wait_for_inject_exec(child: &mut Child) -> Result<(ExitStatus, bool)> {
    wait_for_inject_exec_with_grace(child, EXEC_GRACE_PERIOD).await
}

async fn wait_for_inject_exec_with_grace(
    child: &mut Child,
    grace_period: Duration,
) -> Result<(ExitStatus, bool)> {
    match tokio::time::timeout(grace_period, child.wait()).await {
        Ok(status) => Ok((
            status.with_context(|| "failed to wait for podman exec")?,
            false,
        )),
        Err(_) => {
            #[cfg(unix)]
            {
                if let Some(pid) = child.id() {
                    // SAFETY: the child was started in its own process group.
                    unsafe {
                        libc::kill(-(pid as i32), signal_hook::consts::signal::SIGKILL);
                    }
                }
            }
            #[cfg(not(unix))]
            child
                .kill()
                .await
                .with_context(|| "failed to terminate podman exec after its grace period")?;
            let status = child
                .wait()
                .await
                .with_context(|| "failed to reap podman exec after termination")?;
            Ok((status, true))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn command_args(cmd: &Command) -> Vec<String> {
        cmd.as_std()
            .get_args()
            .map(|arg| arg.to_string_lossy().into_owned())
            .collect()
    }

    #[test]
    fn build_inject_exec_command_starts_one_interactive_bridge() {
        let cmd = build_inject_agent_exec_command("demo-agent-instance", 15432);
        let args = command_args(&cmd);

        assert_eq!(cmd.as_std().get_program().to_string_lossy(), "podman");
        assert_eq!(
            args,
            vec![
                "exec",
                "-i",
                "demo-agent-instance",
                BRIDGE_BINARY,
                "listen",
                "--tcp",
                "127.0.0.1:15432",
            ]
        );
        assert!(!args.iter().any(|arg| arg == "socat" || arg == "-t"));
    }

    #[test]
    fn inject_host_target_is_fixed_to_the_parsed_endpoint() {
        assert_eq!(
            host_target(&InjectHostEndpoint {
                host: "db.internal".to_string(),
                port: 5432,
            }),
            HostTarget::tcp("db.internal", 5432)
        );
    }

    #[test]
    fn inject_listener_bind_error_identifies_the_agent_side() {
        let error = inject_transport_error(BridgeError::ListenerBindFailure);
        assert_eq!(
            error.to_string(),
            "cladding inject agent-side listener failed to bind"
        );
    }

    #[test]
    fn other_inject_transport_errors_keep_the_generic_context() {
        let error = inject_transport_error(BridgeError::StartupFailure);
        assert_eq!(error.to_string(), "cladding inject bridge transport failed");
    }

    #[test]
    fn helper_preflight_uses_the_mounted_bridge_path() {
        let cmd = build_agent_bridge_check_command("demo-agent-instance");
        let args = cmd
            .get_args()
            .map(|arg| arg.to_string_lossy().into_owned())
            .collect::<Vec<_>>();
        assert_eq!(
            args,
            vec!["exec", "demo-agent-instance", "test", "-x", BRIDGE_BINARY]
        );
    }

    #[tokio::test]
    async fn inject_exec_exits_gracefully_after_transport_eof() {
        let mut child = Command::new("cat")
            .stdin(Stdio::piped())
            .stdout(Stdio::null())
            .spawn()
            .expect("start stdin reader");
        drop(child.stdin.take());

        let (status, forced) = wait_for_inject_exec_with_grace(&mut child, Duration::from_secs(1))
            .await
            .expect("wait for stdin reader");
        assert!(status.success());
        assert!(!forced);
    }

    #[tokio::test]
    async fn inject_exec_is_killed_after_grace_period() {
        let mut command = Command::new("sleep");
        command
            .arg("10")
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null());
        #[cfg(unix)]
        {
            use std::os::unix::process::CommandExt as _;
            command.as_std_mut().process_group(0);
        }
        let mut child = command.spawn().expect("start long-running process");

        let (status, forced) =
            wait_for_inject_exec_with_grace(&mut child, Duration::from_millis(10))
                .await
                .expect("terminate long-running process");
        assert!(!status.success());
        assert!(forced);
    }
}
