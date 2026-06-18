use super::CONTAINER_WORKSPACE_DIR;
use super::args::{LogsTarget, RunWithScissorsTarget};
use super::context::{Context, project_runtime_status};
use anyhow::Context as _;
use cladding::config::{ExecutionConfig, MountTarget, load_cladding_config_v2};
use cladding::error::{Error, Result};
use cladding::fs_utils::canonicalize_path;
use cladding::podman::podman_required;
use signal_hook::consts::signal::{SIGINT, SIGTERM};
use signal_hook::iterator::Signals;
use std::env;
use std::io::{self, IsTerminal};
use std::path::{Path, PathBuf};
use std::process::Command;
use std::thread;

pub(super) fn cmd_run(context: &Context, env_vars: &[String], args: &[String]) -> Result<()> {
    let config = load_cladding_config_v2(&context.project_root)?;
    let container_name = runtime_container_name(&project_component_name(&config.name, "agent"));
    run_podman_exec(
        context,
        &config,
        "run",
        MountTarget::Agent,
        &container_name,
        env_vars,
        args,
    )
}

pub(super) fn cmd_run_with_scissors(
    context: &Context,
    target: RunWithScissorsTarget,
    env_vars: &[String],
    args: &[String],
) -> Result<()> {
    let config = load_cladding_config_v2(&context.project_root)?;
    let (container_name, mount_target) = match target {
        RunWithScissorsTarget::NwSandbox => {
            if !target.enabled(&config) {
                return run_with_scissors_target_disabled(&config, target);
            }
            (
                runtime_container_name(&project_component_name(&config.name, "nw-sandbox")),
                MountTarget::NwSandbox,
            )
        }
        RunWithScissorsTarget::FsSandbox => {
            if !target.enabled(&config) {
                return run_with_scissors_target_disabled(&config, target);
            }
            (
                runtime_container_name(&project_component_name(&config.name, "fs-sandbox")),
                MountTarget::FsSandbox,
            )
        }
    };
    run_podman_exec(
        context,
        &config,
        "run-with-scissors",
        mount_target,
        &container_name,
        env_vars,
        args,
    )
}

fn run_with_scissors_target_disabled(
    config: &ExecutionConfig,
    target: RunWithScissorsTarget,
) -> Result<()> {
    let target_name = target.as_str();
    let target_key = target.config_key();
    let other = target.other();
    let hint = match (other.enabled(config), target.enabled(config)) {
        (true, false) => format!("hint: use '--target {}'", other.as_str()),
        (false, false) => "hint: enable 'nw_sandbox.enabled' or 'fs_sandbox.enabled'".to_string(),
        _ => format!("hint: enable '{target_key}.enabled' or choose a different target"),
    };

    eprintln!(
        "error: target '{target_name}' is disabled for project '{}'",
        config.name
    );
    eprintln!("{hint}");
    Err(Error::message(
        "selected run-with-scissors target is disabled",
    ))
}

pub(super) fn cmd_logs(context: &Context, target: LogsTarget, args: &[String]) -> Result<()> {
    podman_required("podman (required for cladding logs)")?;

    let config = load_cladding_config_v2(&context.project_root)?;
    let pod_name = match target {
        LogsTarget::Agent => project_component_name(&config.name, "agent"),
        LogsTarget::Proxy => project_component_name(&config.name, "proxy"),
        LogsTarget::NwSandbox if config.nw_sandbox_enabled() => {
            project_component_name(&config.name, "nw-sandbox")
        }
        LogsTarget::FsSandbox if config.fs_sandbox_enabled() => {
            project_component_name(&config.name, "fs-sandbox")
        }
        _ => return logs_target_disabled(&config, target),
    };
    let container_name = runtime_container_name(&pod_name);

    let mut cmd = Command::new("podman");
    cmd.arg("logs");
    for arg in args {
        cmd.arg(arg);
    }
    cmd.arg(container_name);

    let status = cmd.status().with_context(|| "failed to run podman logs")?;
    cladding::podman::ensure_success(status, "podman logs")
}

fn logs_target_disabled(config: &ExecutionConfig, target: LogsTarget) -> Result<()> {
    let target_name = target.as_str();
    eprintln!(
        "error: target '{target_name}' is disabled for project '{}'",
        config.name
    );
    if let Some(key) = target.config_key() {
        eprintln!("hint: enable '{key}.enabled' or choose a different target");
    }
    Err(Error::message("selected logs target is disabled"))
}

fn run_podman_exec(
    context: &Context,
    config: &ExecutionConfig,
    command_name: &str,
    mount_target: MountTarget,
    container_name: &str,
    env_vars: &[String],
    args: &[String],
) -> Result<()> {
    if args.is_empty() {
        eprintln!("usage: cladding {command_name} [--env KEY[=VALUE] ...] <command> [args...]");
        return Err(Error::message(format!("missing {command_name} command")));
    }

    let status = project_runtime_status(context, config, false)?;
    if !status.already_running {
        eprintln!("error: cladding project '{}' is not running", config.name);
        eprintln!("hint: run 'cladding up'");
        return Err(Error::message("project is not running"));
    }

    let project_dir = context
        .project_root
        .parent()
        .ok_or_else(|| Error::message("could not resolve project directory"))?
        .to_path_buf();

    let cwd = env::current_dir().with_context(|| "failed to determine current directory")?;

    let project_dir = canonicalize_path(&project_dir)?;
    let cwd = canonicalize_path(&cwd)?;
    let container_workdir = resolve_container_workdir(config, &project_dir, &cwd, mount_target)?;

    let interactive = io::stdin().is_terminal() && io::stdout().is_terminal();

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
    } else {
        cmd.args([
            "exec",
            "-i",
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

    let mut child = cmd
        .spawn()
        .with_context(|| format!("failed to run podman exec for {command_name}"))?;

    let mut signal_handle = None;
    let mut signal_thread = None;
    if !interactive {
        let kill_pattern = args.join(" ");
        let mut signals =
            Signals::new([SIGINT, SIGTERM]).with_context(|| "failed to install signal handlers")?;
        signal_handle = Some(signals.handle());
        let container_name = container_name.to_string();
        signal_thread = Some(thread::spawn(move || {
            if signals.forever().next().is_some() && !kill_pattern.is_empty() {
                let _ = Command::new("podman")
                    .args(["exec", &container_name, "pkill", "-f", &kill_pattern])
                    .status();
            }
        }));
    }

    let status = child
        .wait()
        .with_context(|| format!("failed to run podman exec for {command_name}"))?;

    if let Some(handle) = signal_handle {
        handle.close();
    }
    if let Some(thread) = signal_thread {
        let _ = thread.join();
    }

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
        Err(Error::message("podman exec failed"))
    }
}

pub(super) fn cmd_reload_proxy(context: &Context) -> Result<()> {
    let config = load_cladding_config_v2(&context.project_root)?;

    let status = Command::new("podman")
        .args([
            "exec",
            &runtime_container_name(&project_component_name(&config.name, "proxy")),
            "squid",
            "-k",
            "reconfigure",
            "-f",
            "/tmp/squid_generated.conf",
        ])
        .status()
        .with_context(|| "failed to run podman exec")?;

    cladding::podman::ensure_success(status, "podman exec")
}

pub(super) fn runtime_container_name(pod_name: &str) -> String {
    format!("{pod_name}-instance")
}

fn project_component_name(project_name: &str, component: &str) -> String {
    format!("{project_name}-{component}")
}

pub(super) fn agent_runtime_names(project_name: &str) -> (String, String) {
    let agent_pod_name = project_component_name(project_name, "agent");
    let agent_container_name = runtime_container_name(&agent_pod_name);
    (agent_pod_name, agent_container_name)
}

fn resolve_container_workdir(
    config: &ExecutionConfig,
    project_dir: &Path,
    cwd: &Path,
    target: MountTarget,
) -> Result<PathBuf> {
    if let Some(custom_workspace_host_path) = effective_cli_workspace_host_path(config, target)
        .filter(|path| path.starts_with(project_dir))
    {
        let custom_workspace_host_path = canonicalize_path(&custom_workspace_host_path)?;
        let workdir_rel = cwd.strip_prefix(&custom_workspace_host_path).map_err(|_| {
            eprintln!(
                "error: could not determine current path relative to configured workspace hostPath ({}): {}",
                custom_workspace_host_path.display(),
                cwd.display()
            );
            eprintln!(
                "hint: run cladding from {} or one of its subdirectories",
                custom_workspace_host_path.display()
            );
            Error::message("invalid working directory")
        })?;
        return Ok(join_container_workspace(workdir_rel));
    }

    let workdir_rel = cwd.strip_prefix(project_dir).map_err(|_| {
        eprintln!(
            "error: could not determine current path relative to project dir ({}): {}",
            project_dir.display(),
            cwd.display()
        );
        eprintln!(
            "hint: run cladding from {} or one of its subdirectories",
            project_dir.display()
        );
        Error::message("invalid working directory")
    })?;

    Ok(join_container_workspace(workdir_rel))
}

fn effective_cli_workspace_host_path(
    config: &ExecutionConfig,
    target: MountTarget,
) -> Option<PathBuf> {
    let custom_mount = config.mounts.iter().find(|mount| {
        mount.mount_path == CONTAINER_WORKSPACE_DIR && mount.targets.contains(&target)
    })?;

    if custom_mount.ignore {
        return None;
    }

    custom_mount.host_path.clone()
}

fn join_container_workspace(workdir_rel: &Path) -> PathBuf {
    let mut container_workdir = PathBuf::from(CONTAINER_WORKSPACE_DIR);
    if !workdir_rel.as_os_str().is_empty() {
        container_workdir = container_workdir.join(workdir_rel);
    }
    container_workdir
}

#[cfg(test)]
mod tests {
    use super::*;
    use cladding::config::{ExecutionComponentConfig, ResolvedMountConfig};
    use std::fs;
    use std::sync::atomic::{AtomicU64, Ordering};

    static TEMP_DIR_COUNTER: AtomicU64 = AtomicU64::new(0);

    #[test]
    fn runtime_container_name_matches_direct_runtime_names() {
        assert_eq!(
            runtime_container_name("demo-fs-sandbox"),
            "demo-fs-sandbox-instance"
        );
        assert_eq!(runtime_container_name("demo-agent"), "demo-agent-instance");
    }

    #[test]
    fn agent_runtime_names_cover_pod_and_container_names() {
        assert_eq!(
            agent_runtime_names("demo"),
            ("demo-agent".to_string(), "demo-agent-instance".to_string())
        );
    }

    #[test]
    fn resolve_container_workdir_uses_default_project_mapping() {
        let temp = create_temp_dir("default-workdir");
        let project_dir = temp.join("workspace");
        let nested_dir = project_dir.join("src/module");
        fs::create_dir_all(&nested_dir).expect("create nested dir");

        let config = ExecutionConfig {
            name: "demo".to_string(),
            use_runsc: false,
            agent: ExecutionComponentConfig {
                enabled: true,
                image: "agent:image".to_string(),
            },
            nw_sandbox: None,
            fs_sandbox: None,
            mounts: Vec::new(),
        };

        let resolved =
            resolve_container_workdir(&config, &project_dir, &nested_dir, MountTarget::Agent)
                .expect("workdir");
        assert_eq!(
            resolved,
            PathBuf::from(CONTAINER_WORKSPACE_DIR).join("src/module")
        );
    }

    #[test]
    fn resolve_container_workdir_uses_custom_workspace_host_path() {
        let temp = create_temp_dir("custom-workdir");
        let project_dir = temp.join("project");
        let custom_root = project_dir.join("workspace");
        let nested_dir = custom_root.join("src/module");
        fs::create_dir_all(&nested_dir).expect("create nested dir");

        let config = ExecutionConfig {
            name: "demo".to_string(),
            use_runsc: false,
            agent: ExecutionComponentConfig {
                enabled: true,
                image: "agent:image".to_string(),
            },
            nw_sandbox: None,
            fs_sandbox: None,
            mounts: vec![ResolvedMountConfig {
                mount_path: CONTAINER_WORKSPACE_DIR.to_string(),
                host_path: Some(custom_root.clone()),
                volume: None,
                read_only: false,
                targets: vec![MountTarget::Agent],
                ignore: false,
            }],
        };

        let resolved =
            resolve_container_workdir(&config, &project_dir, &nested_dir, MountTarget::Agent)
                .expect("workdir");
        assert_eq!(
            resolved,
            PathBuf::from(CONTAINER_WORKSPACE_DIR).join("src/module")
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
