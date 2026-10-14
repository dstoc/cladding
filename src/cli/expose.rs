use super::args::ExposeArgs;
use super::context::{Context, project_runtime_status};
use super::exec::agent_runtime_names;
use anyhow::Context as _;
use cladding::error::{Error, Result};
use cladding::fs_utils::is_executable;
use cladding::podman::{podman_container_exists, podman_required};
use std::env;
use std::net::IpAddr;
use std::process::Command;

pub(super) fn cmd_expose(context: &Context, args: &ExposeArgs) -> Result<()> {
    podman_required("podman (required for cladding expose)")?;
    socat_required("expose")?;

    let config = context.load_config()?;
    let status = project_runtime_status(context, &config, false, false)?;
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

    let host_port = args.host_port.unwrap_or(args.container_port);
    let status = build_blocking_expose_command(
        &agent_container_name,
        args.container_port,
        host_port,
        args.bind_address,
    )
    .status()
    .with_context(|| "failed to run socat")?;

    cladding::podman::ensure_success(status, "socat")
}

pub(super) fn socat_required(command_name: &str) -> Result<()> {
    if command_exists_on_path("socat") {
        Ok(())
    } else {
        eprintln!("missing: socat (required for cladding {command_name})");
        Err(Error::message("missing socat"))
    }
}

fn command_exists_on_path(command: &str) -> bool {
    env::var_os("PATH")
        .as_deref()
        .is_some_and(|paths| env::split_paths(paths).any(|path| is_executable(&path.join(command))))
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
        let (_, agent_container_name) = agent_runtime_names("selected-config");
        let cmd = build_blocking_expose_command(
            &agent_container_name,
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
            "EXEC:podman exec -i 'selected-config-agent-instance' socat STDIO TCP\\:127.0.0.1\\:5432"
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
