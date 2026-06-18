use super::args::{InjectArgs, InjectHostEndpoint};
use super::context::{Context, project_runtime_status};
use super::exec::agent_runtime_names;
use super::expose::socat_required;
use anyhow::Context as _;
use cladding::config::load_cladding_config_v2;
use cladding::error::{Error, Result};
use cladding::podman::{podman_container_exists, podman_required};
use signal_hook::consts::signal::{SIGINT, SIGTERM};
use signal_hook::iterator::Signals;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Child, Command};
use std::sync::{
    Arc,
    atomic::{AtomicBool, Ordering},
};
use std::thread;
use std::time::Duration;

pub(super) fn cmd_inject(context: &Context, args: &InjectArgs) -> Result<()> {
    podman_required("podman (required for cladding inject)")?;
    socat_required("inject")?;

    let config = load_cladding_config_v2(&context.project_root)?;
    let status = project_runtime_status(context, &config, false)?;
    if !status.already_running {
        eprintln!("error: cladding project '{}' is not running", config.name);
        eprintln!("hint: run 'cladding up'");
        return Err(Error::message("project is not running"));
    }

    let (_, agent_container_name) = agent_runtime_names(&config.name);
    if !podman_container_exists(&agent_container_name)? {
        eprintln!(
            "error: target container '{}' is missing for project '{}'",
            agent_container_name, config.name
        );
        eprintln!("hint: run 'cladding up'");
        return Err(Error::message("missing agent container"));
    }

    if !agent_inject_mount_available(&agent_container_name)? {
        eprintln!(
            "error: target container '{}' is missing /run/cladding/agent/inject for project '{}'",
            agent_container_name, config.name
        );
        eprintln!("hint: run 'cladding down' then 'cladding up'");
        return Err(Error::message("missing inject mount"));
    }

    let container_port = args.container_port.unwrap_or(args.host_endpoint.port);
    let host_socket_dir = inject_host_socket_dir(&context.project_root, container_port);
    let host_socket = host_socket_path(&context.project_root, container_port);
    ensure_inject_socket_path_available(&host_socket)?;
    fs::create_dir_all(&host_socket_dir).with_context(|| {
        format!(
            "failed to create inject socket directory {}",
            host_socket_dir.display()
        )
    })?;

    let agent_socket = inject_agent_socket_path(container_port);
    let mut host_cmd = build_inject_host_bridge_command(&host_socket, &args.host_endpoint);
    let mut host_child = match host_cmd.spawn() {
        Ok(child) => child,
        Err(err) => {
            cleanup_inject_socket_artifacts(&host_socket_dir, &host_socket);
            return Err(anyhow::Error::new(err)
                .context("failed to run host socat for cladding inject")
                .into());
        }
    };

    let agent_listener_args = build_inject_agent_listener_args(&agent_socket, container_port);
    let mut agent_cmd =
        build_inject_agent_exec_command(&agent_container_name, &agent_listener_args);
    let mut agent_child = match agent_cmd.spawn() {
        Ok(child) => child,
        Err(err) => {
            let _ = host_child.kill();
            let _ = host_child.wait();
            cleanup_inject_socket_artifacts(&host_socket_dir, &host_socket);
            return Err(anyhow::Error::new(err)
                .context("failed to run podman exec for cladding inject")
                .into());
        }
    };

    let result = supervise_inject_children(
        &mut host_child,
        &mut agent_child,
        &agent_container_name,
        container_port,
    );

    cleanup_inject_socket_artifacts(&host_socket_dir, &host_socket);
    result
}

fn agent_inject_mount_available(agent_container_name: &str) -> Result<bool> {
    let status = Command::new("podman")
        .args([
            "exec",
            agent_container_name,
            "test",
            "-d",
            "/run/cladding/agent/inject",
        ])
        .status()
        .with_context(|| "failed to check inject mount availability")?;

    Ok(status.success())
}

fn inject_host_socket_dir(project_root: &Path, container_port: u16) -> PathBuf {
    project_root
        .join("runtime/sockets/agent/inject")
        .join(container_port.to_string())
}

fn host_socket_path(project_root: &Path, container_port: u16) -> PathBuf {
    inject_host_socket_dir(project_root, container_port).join("host.sock")
}

fn inject_agent_socket_path(container_port: u16) -> String {
    format!("/run/cladding/agent/inject/{container_port}/host.sock")
}

fn ensure_inject_socket_path_available(host_socket: &Path) -> Result<()> {
    if host_socket.exists() {
        eprintln!(
            "error: inject socket already exists at {}",
            host_socket.display()
        );
        return Err(Error::message("inject socket already exists"));
    }
    Ok(())
}

fn build_inject_host_bridge_command(host_socket: &Path, endpoint: &InjectHostEndpoint) -> Command {
    let mut cmd = Command::new("socat");
    cmd.arg(format!(
        "UNIX-LISTEN:{},fork,reuseaddr",
        host_socket.display()
    ));
    cmd.arg(format!("TCP:{}:{}", endpoint.host, endpoint.port));
    cmd
}

fn build_inject_agent_listener_args(agent_socket: &str, container_port: u16) -> Vec<String> {
    vec![
        "socat".to_string(),
        format!("TCP-LISTEN:{container_port},bind=127.0.0.1,fork,reuseaddr"),
        format!("UNIX-CONNECT:{agent_socket}"),
    ]
}

fn build_inject_agent_exec_command(
    agent_container_name: &str,
    listener_args: &[String],
) -> Command {
    let mut cmd = Command::new("podman");
    cmd.args(["exec", "-i", agent_container_name]);
    for arg in listener_args {
        cmd.arg(arg);
    }
    cmd
}

fn build_inject_agent_cleanup_command(
    agent_container_name: &str,
    listener_pattern: &str,
) -> Command {
    let mut cmd = Command::new("podman");
    cmd.args([
        "exec",
        agent_container_name,
        "pkill",
        "-f",
        listener_pattern,
    ]);
    cmd
}

fn supervise_inject_children(
    host_child: &mut Child,
    agent_child: &mut Child,
    agent_container_name: &str,
    container_port: u16,
) -> Result<()> {
    let interrupted = Arc::new(AtomicBool::new(false));
    let mut signals =
        Signals::new([SIGINT, SIGTERM]).with_context(|| "failed to install signal handlers")?;
    let signal_handle = signals.handle();
    let signal_flag = Arc::clone(&interrupted);
    let signal_thread = thread::spawn(move || {
        if signals.forever().next().is_some() {
            signal_flag.store(true, Ordering::Relaxed);
        }
    });

    enum ExitCause {
        Interrupted,
        Host(std::process::ExitStatus),
        Agent(std::process::ExitStatus),
    }

    let exit_cause = loop {
        if interrupted.load(Ordering::Relaxed) {
            break ExitCause::Interrupted;
        }

        if let Some(status) = host_child
            .try_wait()
            .with_context(|| "failed to wait on host socat for cladding inject")?
        {
            break ExitCause::Host(status);
        }

        if let Some(status) = agent_child
            .try_wait()
            .with_context(|| "failed to wait on podman exec for cladding inject")?
        {
            break ExitCause::Agent(status);
        }

        thread::sleep(Duration::from_millis(100));
    };

    let _ = host_child.kill();
    let _ = agent_child.kill();
    let host_status = host_child.wait();
    let agent_status = agent_child.wait();

    signal_handle.close();
    let _ = signal_thread.join();

    let listener_args =
        build_inject_agent_listener_args(&inject_agent_socket_path(container_port), container_port);
    let listener_pattern = listener_args.join(" ");
    let _cleanup_status =
        build_inject_agent_cleanup_command(agent_container_name, &listener_pattern)
            .status()
            .ok();

    let _ = host_status;
    let _ = agent_status;

    match exit_cause {
        ExitCause::Interrupted => Ok(()),
        ExitCause::Host(status) => {
            if status.success() {
                Err(Error::message("inject host bridge exited unexpectedly"))
            } else {
                Err(Error::CommandFailed {
                    context: "inject host socat",
                    code: status.code().unwrap_or(1),
                })
            }
        }
        ExitCause::Agent(status) => {
            if status.success() {
                Err(Error::message("inject agent listener exited unexpectedly"))
            } else {
                Err(Error::CommandFailed {
                    context: "podman exec",
                    code: status.code().unwrap_or(1),
                })
            }
        }
    }
}

fn cleanup_inject_socket_artifacts(host_socket_dir: &Path, host_socket: &Path) {
    let _ = fs::remove_file(host_socket);
    let _ = fs::remove_dir_all(host_socket_dir);
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::env;
    use std::sync::atomic::{AtomicU64, Ordering};

    static TEMP_DIR_COUNTER: AtomicU64 = AtomicU64::new(0);

    fn command_args(cmd: &Command) -> Vec<String> {
        cmd.get_args()
            .map(|arg| arg.to_string_lossy().into_owned())
            .collect()
    }

    #[test]
    fn inject_socket_helpers_build_expected_paths() {
        let project_root = Path::new("/tmp/project/.cladding");
        assert_eq!(
            inject_host_socket_dir(project_root, 15432),
            PathBuf::from("/tmp/project/.cladding/runtime/sockets/agent/inject/15432")
        );
        assert_eq!(
            host_socket_path(project_root, 15432),
            PathBuf::from("/tmp/project/.cladding/runtime/sockets/agent/inject/15432/host.sock")
        );
        assert_eq!(
            inject_agent_socket_path(15432),
            "/run/cladding/agent/inject/15432/host.sock"
        );
    }

    #[test]
    fn ensure_inject_socket_path_available_detects_conflict() {
        let temp = create_temp_dir("inject-socket-conflict");
        let socket = host_socket_path(&temp, 15432);
        if let Some(parent) = socket.parent() {
            fs::create_dir_all(parent).expect("create socket parent");
        }
        fs::write(&socket, b"busy").expect("create conflicting socket path");

        assert!(ensure_inject_socket_path_available(&socket).is_err());
    }

    #[test]
    fn build_inject_host_bridge_command_uses_requested_endpoint() {
        let cmd = build_inject_host_bridge_command(
            Path::new("/tmp/project/.cladding/runtime/sockets/agent/inject/15432/host.sock"),
            &InjectHostEndpoint {
                host: "db.internal".to_string(),
                port: 5432,
            },
        );
        let args = command_args(&cmd);

        assert_eq!(cmd.get_program().to_string_lossy(), "socat");
        assert_eq!(
            args,
            vec![
                "UNIX-LISTEN:/tmp/project/.cladding/runtime/sockets/agent/inject/15432/host.sock,fork,reuseaddr",
                "TCP:db.internal:5432"
            ]
        );
    }

    #[test]
    fn build_inject_agent_listener_args_match_prd_shape() {
        let args =
            build_inject_agent_listener_args("/run/cladding/agent/inject/15432/host.sock", 15432);
        assert_eq!(
            args,
            vec![
                "socat".to_string(),
                "TCP-LISTEN:15432,bind=127.0.0.1,fork,reuseaddr".to_string(),
                "UNIX-CONNECT:/run/cladding/agent/inject/15432/host.sock".to_string(),
            ]
        );
    }

    #[test]
    fn build_inject_agent_exec_command_has_no_runtime_flags() {
        let cmd = build_inject_agent_exec_command(
            "demo-agent-instance",
            &build_inject_agent_listener_args("/run/cladding/agent/inject/15432/host.sock", 15432),
        );
        let args = command_args(&cmd);

        assert_eq!(
            args,
            vec![
                "exec".to_string(),
                "-i".to_string(),
                "demo-agent-instance".to_string(),
                "socat".to_string(),
                "TCP-LISTEN:15432,bind=127.0.0.1,fork,reuseaddr".to_string(),
                "UNIX-CONNECT:/run/cladding/agent/inject/15432/host.sock".to_string(),
            ]
        );
        assert!(
            !args
                .iter()
                .any(|arg| arg == "--runtime" || arg == "--runtime-flag")
        );
    }

    #[test]
    fn build_inject_agent_cleanup_command_targets_exact_pattern() {
        let cmd = build_inject_agent_cleanup_command(
            "demo-agent-instance",
            "socat TCP-LISTEN:15432,bind=127.0.0.1,fork,reuseaddr UNIX-CONNECT:/run/cladding/agent/inject/15432/host.sock",
        );
        let args = command_args(&cmd);

        assert_eq!(cmd.get_program().to_string_lossy(), "podman");
        assert_eq!(
            args,
            vec![
                "exec".to_string(),
                "demo-agent-instance".to_string(),
                "pkill".to_string(),
                "-f".to_string(),
                "socat TCP-LISTEN:15432,bind=127.0.0.1,fork,reuseaddr UNIX-CONNECT:/run/cladding/agent/inject/15432/host.sock".to_string(),
            ]
        );
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
