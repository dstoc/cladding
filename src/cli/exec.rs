use super::CONTAINER_HOME_DIR;
use super::args::{ExecTarget, LogsTarget};
use super::context::{Context, project_runtime_status};
use anyhow::Context as _;
use cladding::config::{ExecutionConfig, MountTarget};
use cladding::error::{Error, Result};
use cladding::podman::podman_required;
use cladding::runtime::{
    ManagedVolumeKind, RuntimeComponent, RuntimeMount, RuntimeMountSource, RuntimeSpec,
};
use signal_hook::consts::signal::{SIGINT, SIGTERM};
use signal_hook::iterator::Signals;
use std::env;
use std::fs;
use std::io::{self, IsTerminal};
use std::path::{Path, PathBuf};
use std::process::Command;
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
}

pub(super) fn cmd_exec(
    context: &Context,
    target: ExecTarget,
    env_vars: &[String],
    args: &[String],
) -> Result<()> {
    let config = context.load_config()?;
    if !target.enabled(&config) {
        return exec_target_disabled(&config, target);
    }
    let container_name =
        runtime_container_name(&project_component_name(&config.name, target.as_str()));
    run_podman_exec(
        context,
        &config,
        PodmanExec {
            command_name: "exec",
            mount_target: target.mount_target(),
            container_name: &container_name,
            env_vars,
            args,
            allow_interactive: true,
            forward_stdin: true,
            forward_signals: false,
        },
    )
}

pub(super) fn cmd_run_command(
    context: &Context,
    env_vars: &[String],
    args: &[String],
    forward_stdin: bool,
) -> Result<()> {
    let config = context.load_config()?;
    let container_name = runtime_container_name(&project_component_name(&config.name, "agent"));
    run_podman_exec(
        context,
        &config,
        PodmanExec {
            command_name: "run",
            mount_target: MountTarget::Agent,
            container_name: &container_name,
            env_vars,
            args,
            allow_interactive: forward_stdin,
            forward_stdin,
            forward_signals: true,
        },
    )
}

fn exec_target_disabled(config: &ExecutionConfig, target: ExecTarget) -> Result<()> {
    let target_name = target.as_str();
    let hint = match target.other_sandbox().filter(|other| other.enabled(config)) {
        Some(other) => format!("hint: use '--target {}'", other.as_str()),
        None => "hint: enable 'nw_sandbox.enabled' or 'fs_sandbox.enabled'".to_string(),
    };

    eprintln!(
        "error: target '{target_name}' is disabled for project '{}'",
        config.name
    );
    eprintln!("{hint}");
    Err(Error::message("selected exec target is disabled"))
}

pub(super) fn cmd_logs(context: &Context, target: LogsTarget, args: &[String]) -> Result<()> {
    podman_required("podman (required for cladding logs)")?;

    let config = context.load_config()?;
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
    } = request;
    if args.is_empty() {
        eprintln!("usage: cladding {command_name} [--env KEY[=VALUE] ...] <command> [args...]");
        return Err(Error::message(format!("missing {command_name} command")));
    }

    let status = project_runtime_status(context, config, false, false)?;
    if !status.already_running {
        eprintln!("error: cladding project '{}' is not running", config.name);
        eprintln!("hint: run 'cladding up'");
        return Err(Error::message("project is not running"));
    }

    let cwd = env::current_dir().with_context(|| "failed to determine current directory")?;

    let runtime_spec = context.runtime_spec(config)?;
    let component = runtime_component_for_target(&runtime_spec, mount_target)?;
    let runtime_container = component
        .containers
        .iter()
        .find(|container| container.name == container_name)
        .ok_or_else(|| {
            Error::message(format!(
                "runtime target '{}' has no container named '{container_name}'",
                mount_target_name(mount_target)
            ))
        })?;
    let container_workdir = resolve_container_workdir(
        &runtime_container.mounts,
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

    let mut child = cmd
        .spawn()
        .with_context(|| format!("failed to run podman exec for {command_name}"))?;

    let mut signal_handle = None;
    let mut signal_thread = None;
    if !interactive || forward_signals {
        let kill_pattern = args.join(" ");
        let mut signals =
            Signals::new([SIGINT, SIGTERM]).with_context(|| "failed to install signal handlers")?;
        signal_handle = Some(signals.handle());
        let container_name = container_name.to_string();
        signal_thread = Some(thread::spawn(move || {
            if let Some(signal) = signals.forever().next()
                && !kill_pattern.is_empty()
            {
                let signal = format!("-{signal}");
                let _ = Command::new("podman")
                    .args([
                        "exec",
                        &container_name,
                        "pkill",
                        &signal,
                        "-f",
                        &kill_pattern,
                    ])
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
        #[cfg(unix)]
        {
            use std::os::unix::process::ExitStatusExt as _;
            let code = status.signal().map(|signal| 128 + signal).unwrap_or(1);
            Err(Error::CommandFailed {
                context: "podman exec",
                code,
            })
        }
        #[cfg(not(unix))]
        {
            Err(Error::message("podman exec failed"))
        }
    }
}

pub(super) fn cmd_reload_proxy(context: &Context) -> Result<()> {
    let config = context.load_config()?;

    let status = Command::new("podman")
        .args(reload_proxy_args(&config.name))
        .status()
        .with_context(|| "failed to run podman exec")?;

    cladding::podman::ensure_success(status, "Baffle reload via podman exec")
}

fn reload_proxy_args(project_name: &str) -> [String; 5] {
    [
        "exec".to_string(),
        runtime_container_name(&project_component_name(project_name, "proxy")),
        // The proxy image does not add /opt/tools/bin to PATH.
        "/opt/tools/bin/baffle".to_string(),
        "reload".to_string(),
        "--all".to_string(),
    ]
}

pub(super) fn runtime_container_name(pod_name: &str) -> String {
    format!("{pod_name}-instance")
}

fn project_component_name(project_name: &str, component: &str) -> String {
    format!("{project_name}-{component}")
}

#[cfg(test)]
mod reload_proxy_tests {
    use super::reload_proxy_args;

    #[test]
    fn reload_proxy_targets_all_baffle_sessions_in_the_proxy_container() {
        assert_eq!(
            reload_proxy_args("demo"),
            [
                "exec",
                "demo-proxy-instance",
                "/opt/tools/bin/baffle",
                "reload",
                "--all"
            ]
        );
    }
}

pub(super) fn agent_runtime_names(project_name: &str) -> (String, String) {
    let agent_pod_name = project_component_name(project_name, "agent");
    let agent_container_name = runtime_container_name(&agent_pod_name);
    (agent_pod_name, agent_container_name)
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
    use cladding::config::{ExecutionComponentConfig, MountType, ResolvedMountConfig};
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

    #[test]
    fn agent_runtime_names_cover_pod_and_container_names() {
        assert_eq!(
            agent_runtime_names("demo"),
            ("demo-agent".to_string(), "demo-agent-instance".to_string())
        );
    }

    fn resolve_with_runtime_mounts(
        mounts: &[RuntimeMount],
        target_workdir: Option<&str>,
        cwd: &Path,
    ) -> Result<PathBuf> {
        resolve_container_workdir(mounts, target_workdir, cwd)
    }

    #[test]
    fn resolve_container_workdir_maps_the_effective_default_workspace_mount() {
        let temp = create_temp_dir("default-workdir");
        let workspace = temp.join("workspace");
        let project_root = workspace.join(".cladding");
        let cwd = workspace.join("src/module");
        fs::create_dir_all(&cwd).expect("create nested workspace directory");
        fs::create_dir_all(&project_root).expect("create project root");

        let runtime = runtime_spec(&project_root, &workspace, Vec::new(), false);
        let container = &runtime.agent.containers[0];
        let resolved =
            resolve_with_runtime_mounts(&container.mounts, container.workdir.as_deref(), &cwd)
                .expect("workdir");

        assert_eq!(
            resolved,
            PathBuf::from(CONTAINER_WORKSPACE_DIR).join("src/module")
        );
    }

    #[test]
    fn resolve_container_workdir_maps_the_default_home_mount() {
        let temp = create_temp_dir("default-home");
        let workspace = temp.join("workspace");
        let project_root = workspace.join(".cladding");
        let cwd = project_root.join("home/packages");
        fs::create_dir_all(&cwd).expect("create home directory");
        fs::create_dir_all(&project_root).expect("create project root");

        let runtime = runtime_spec(&project_root, &workspace, Vec::new(), false);
        let container = &runtime.agent.containers[0];
        let resolved =
            resolve_with_runtime_mounts(&container.mounts, container.workdir.as_deref(), &cwd)
                .expect("workdir");

        assert_eq!(resolved, PathBuf::from(CONTAINER_HOME_DIR).join("packages"));
    }

    #[cfg(unix)]
    #[test]
    fn resolve_container_workdir_maps_a_symlinked_home_source_outside_the_workspace() {
        let temp = create_temp_dir("symlinked-home");
        let workspace = temp.join("workspace");
        let project_root = workspace.join(".cladding");
        let real_home = temp.join("shared-home");
        let cwd = real_home.join("projects/sample");
        fs::create_dir_all(&cwd).expect("create real home directory");
        fs::create_dir_all(&project_root).expect("create project root");
        std::os::unix::fs::symlink(&real_home, project_root.join("home"))
            .expect("link configured home source");

        let runtime = runtime_spec(&project_root, &workspace, Vec::new(), false);
        let container = &runtime.agent.containers[0];
        let resolved =
            resolve_with_runtime_mounts(&container.mounts, container.workdir.as_deref(), &cwd)
                .expect("workdir");

        assert_eq!(
            resolved,
            PathBuf::from(CONTAINER_HOME_DIR).join("projects/sample")
        );
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
        let runtime = runtime_spec(&project_root, &workspace, vec![ignored_workspace], false);
        let container = &runtime.agent.containers[0];
        let resolved =
            resolve_with_runtime_mounts(&container.mounts, container.workdir.as_deref(), &cwd)
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

        let runtime = runtime_spec(&project_root, &workspace, Vec::new(), true);
        let component = runtime_component_for_target(&runtime, MountTarget::FsSandbox)
            .expect("filesystem sandbox component");
        let container = &component.containers[0];
        let resolved =
            resolve_with_runtime_mounts(&container.mounts, container.workdir.as_deref(), &cwd)
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
        let runtime = runtime_spec(&project_root, &workspace, vec![custom_mount], false);
        let container = &runtime.agent.containers[0];

        let resolved =
            resolve_with_runtime_mounts(&container.mounts, container.workdir.as_deref(), &cwd)
                .expect("workdir");

        assert_eq!(resolved, PathBuf::from("/data/reports/weekly"));
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
        workspace_root: &Path,
        mounts: Vec<ResolvedMountConfig>,
        fs_sandbox_enabled: bool,
    ) -> RuntimeSpec {
        let config = ExecutionConfig {
            name: "demo".to_string(),
            use_runsc: false,
            agent: ExecutionComponentConfig {
                enabled: true,
                image: "agent:image".to_string(),
                build: None,
                security_opts: Vec::new(),
            },
            nw_sandbox: None,
            fs_sandbox: fs_sandbox_enabled.then_some(ExecutionComponentConfig {
                enabled: true,
                image: "fs-sandbox:image".to_string(),
                build: None,
                security_opts: Vec::new(),
            }),
            proxy: None,
            mounts,
        };
        RuntimeSpec::build_with_roots(project_root, workspace_root, project_root, &config)
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
