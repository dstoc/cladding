use super::args::ExposeArgs;
use super::context::{Context, project_runtime_status};
use super::exec::agent_runtime_names;
use anyhow::Context as _;
use cladding::config::load_cladding_config_v2;
use cladding::error::{Error, Result};
use cladding::fs_utils::is_executable;
use cladding::podman::{podman_container_exists, podman_required};
use std::env;
use std::path::Path;
use std::process::Command;

pub(super) fn cmd_expose(context: &Context, args: &ExposeArgs) -> Result<()> {
    podman_required("podman (required for cladding expose)")?;
    socat_required("expose")?;

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

    let host_port = args.host_port.unwrap_or(args.container_port);
    let current_exe =
        env::current_exe().with_context(|| "failed to determine current executable")?;
    let status = build_blocking_expose_command(&current_exe, args.container_port, host_port)
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
    current_exe: &Path,
    container_port: u16,
    host_port: u16,
) -> Command {
    let mut cmd = Command::new("socat");
    cmd.arg(format!(
        "TCP-LISTEN:{host_port},bind=127.0.0.1,reuseaddr,fork"
    ));
    let current_exe = shell_single_quote_path(current_exe);
    cmd.arg(format!(
        "EXEC:{current_exe} run socat STDIO TCP\\:127.0.0.1\\:{container_port}"
    ));
    cmd
}

fn shell_single_quote_path(path: &Path) -> String {
    let path = path.to_string_lossy();
    let escaped = path.replace('\'', "'\\''");
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
    fn build_blocking_expose_command_places_ports_and_bind() {
        let cmd = build_blocking_expose_command(
            Path::new("/tmp/cladding test/bin's/cladding"),
            5432,
            15432,
        );
        let args = command_args(&cmd);

        assert_eq!(cmd.get_program().to_string_lossy(), "socat");
        assert_eq!(args.len(), 2);
        assert_eq!(args[0], "TCP-LISTEN:15432,bind=127.0.0.1,reuseaddr,fork");
        assert_eq!(
            args[1],
            "EXEC:'/tmp/cladding test/bin'\\''s/cladding' run socat STDIO TCP\\:127.0.0.1\\:5432"
        );
        assert!(!args.iter().any(|arg| arg.starts_with("--")));
    }
}
