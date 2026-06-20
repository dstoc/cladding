use crate::error::Result;
use crate::runtime::{RuntimeContainer, RuntimePlacement, RuntimePod, RuntimeSpec};
use anyhow::Context as _;
use std::fs;
use std::io::ErrorKind;
use std::path::Path;
use std::process::{Command, Output};

use super::command::{
    PodmanRuntimeOptions, ensure_success, ensure_success_output, podman_command_with_options,
    trace_command,
};
use super::mounts::{append_mount_args, generated_empty_mask_dirs};

/// Direct runtime helpers for creating and cleaning up Podman resources.
pub fn runtime_create(spec: &RuntimeSpec, verbose: bool) -> Result<()> {
    prepare_runtime_socket_dirs(spec)?;
    ensure_runtime_empty_mask_dir(spec)?;

    for pod in runtime_pods(spec) {
        if pod.placement == RuntimePlacement::Pod {
            pod_create(pod.use_runsc, pod, verbose)?;
        }
    }

    for pod in runtime_pods(spec) {
        for container in &pod.containers {
            container_run(pod.use_runsc, pod, container, verbose)?;
        }
    }

    Ok(())
}

pub fn runtime_cleanup(spec: &RuntimeSpec, verbose: bool) -> Result<()> {
    for pod in runtime_pods(spec) {
        for container in &pod.containers {
            container_rm(&container.name, verbose)?;
        }
        // For standalone components this is best-effort cleanup for projects
        // started by older builds where execution components were still pods.
        pod_rm(&pod.name, verbose)?;
    }

    Ok(())
}

pub fn pod_create(use_runsc: bool, pod: &RuntimePod, verbose: bool) -> Result<()> {
    let mut cmd = build_pod_create_command(use_runsc, pod);
    trace_command(&cmd, verbose);
    let status = cmd
        .status()
        .with_context(|| format!("failed to run podman pod create for {}", pod.name))?;
    ensure_success(status, "podman pod create")
}

pub fn pod_rm(pod_name: &str, verbose: bool) -> Result<()> {
    let mut cmd = build_pod_rm_command(pod_name);
    trace_command(&cmd, verbose);
    let output = cmd
        .output()
        .with_context(|| format!("failed to run podman pod rm for {pod_name}"))?;

    if output.status.success() || remove_output_is_missing_pod(&output) {
        return Ok(());
    }

    ensure_success_output(&output, "podman pod rm")
}

pub fn container_run(
    use_runsc: bool,
    pod: &RuntimePod,
    container: &RuntimeContainer,
    verbose: bool,
) -> Result<()> {
    let mut cmd = build_container_run_command(use_runsc, pod, container);
    trace_command(&cmd, verbose);
    let status = cmd.status().with_context(|| {
        format!(
            "failed to run podman container {} in pod {}",
            container.name, pod.name
        )
    })?;
    ensure_success(status, "podman run")
}

pub fn container_rm(container_name: &str, verbose: bool) -> Result<()> {
    let mut cmd = build_container_rm_command(container_name);
    trace_command(&cmd, verbose);
    let output = cmd
        .output()
        .with_context(|| format!("failed to run podman rm for {container_name}"))?;

    if output.status.success() || remove_output_is_missing_container(&output) {
        return Ok(());
    }

    ensure_success_output(&output, "podman rm")
}

fn remove_output_is_missing_container(output: &Output) -> bool {
    let stderr = String::from_utf8_lossy(&output.stderr).to_ascii_lowercase();
    stderr.contains("no such container") || stderr.contains("no container with name or id")
}

fn remove_output_is_missing_pod(output: &Output) -> bool {
    let stderr = String::from_utf8_lossy(&output.stderr).to_ascii_lowercase();
    stderr.contains("no such pod") || stderr.contains("no pod with name or id")
}

fn runtime_pods(spec: &RuntimeSpec) -> Vec<&RuntimePod> {
    let mut pods = vec![&spec.proxy, &spec.agent];
    if let Some(pod) = &spec.nw_sandbox {
        pods.push(pod);
    }
    if let Some(pod) = &spec.fs_sandbox {
        pods.push(pod);
    }
    pods
}

fn prepare_runtime_socket_dirs(spec: &RuntimeSpec) -> Result<()> {
    let mut socket_dirs = spec.generated_runtime_socket_dirs().into_iter();
    let Some(root_socket_dir) = socket_dirs.next() else {
        return Ok(());
    };

    clear_path(&root_socket_dir)?;
    fs::create_dir_all(&root_socket_dir).with_context(|| {
        format!(
            "failed to create runtime socket directory {}",
            root_socket_dir.display()
        )
    })?;
    set_restrictive_dir_permissions(&root_socket_dir)?;

    for socket_dir in socket_dirs {
        fs::create_dir_all(&socket_dir).with_context(|| {
            format!(
                "failed to create runtime socket directory {}",
                socket_dir.display()
            )
        })?;
    }
    Ok(())
}

fn clear_path(path: &Path) -> Result<()> {
    match fs::symlink_metadata(path) {
        Ok(metadata) => {
            if metadata.is_dir() {
                fs::remove_dir_all(path)
                    .with_context(|| format!("failed to remove directory {}", path.display()))?;
            } else {
                fs::remove_file(path)
                    .with_context(|| format!("failed to remove file {}", path.display()))?;
            }
        }
        Err(err) if err.kind() == ErrorKind::NotFound => {}
        Err(err) => {
            return Err(anyhow::Error::new(err).into());
        }
    }
    Ok(())
}

#[cfg(unix)]
fn set_restrictive_dir_permissions(path: &Path) -> Result<()> {
    use std::os::unix::fs::PermissionsExt;

    let mut permissions = fs::metadata(path)
        .with_context(|| format!("failed to read permissions for {}", path.display()))?
        .permissions();
    permissions.set_mode(0o700);
    fs::set_permissions(path, permissions).with_context(|| {
        format!(
            "failed to set restrictive permissions on {}",
            path.display()
        )
    })?;
    Ok(())
}

#[cfg(not(unix))]
fn set_restrictive_dir_permissions(_path: &Path) -> Result<()> {
    Ok(())
}

fn ensure_runtime_empty_mask_dir(spec: &RuntimeSpec) -> Result<()> {
    for empty_mask in generated_empty_mask_dirs(spec) {
        fs::create_dir_all(&empty_mask).with_context(|| {
            format!("failed to create runtime mask dir {}", empty_mask.display())
        })?;
    }
    Ok(())
}

fn build_pod_create_command(use_runsc: bool, pod: &RuntimePod) -> Command {
    let mut cmd = podman_command_with_options(
        PodmanRuntimeOptions::new(use_runsc).with_network_none(pod.network_name == "none"),
    );
    cmd.args(["pod", "create", "--name", &pod.name]);
    append_label_args(&mut cmd, &pod.labels);
    cmd.arg("--network");
    cmd.arg(&pod.network_name);
    if pod.userns_keep_id {
        cmd.arg("--userns");
        cmd.arg("keep-id");
    }
    cmd
}

fn build_pod_rm_command(pod_name: &str) -> Command {
    let mut cmd = Command::new("podman");
    cmd.args(["pod", "rm", "-f", pod_name]);
    cmd
}

fn build_container_run_command(
    use_runsc: bool,
    pod: &RuntimePod,
    container: &RuntimeContainer,
) -> Command {
    let mut cmd =
        podman_command_with_options(PodmanRuntimeOptions::new(use_runsc).with_network_none(
            pod.placement == RuntimePlacement::Standalone && pod.network_name == "none",
        ));
    cmd.arg("run");
    cmd.arg("-d");
    match pod.placement {
        RuntimePlacement::Pod => {
            cmd.arg("--pod");
            cmd.arg(&pod.name);
        }
        RuntimePlacement::Standalone => {
            append_label_args(&mut cmd, &pod.labels);
            cmd.arg("--network");
            cmd.arg(&pod.network_name);
            if pod.userns_keep_id {
                cmd.arg("--userns");
                cmd.arg("keep-id");
            }
            cmd.arg("--hostname");
            cmd.arg(&pod.name);
        }
    }
    cmd.arg("--name");
    cmd.arg(&container.name);
    if let Some(workdir) = &container.workdir {
        cmd.arg("--workdir");
        cmd.arg(workdir);
    }
    if container.stdin {
        cmd.arg("-i");
    }
    if container.tty {
        cmd.arg("-t");
    }

    append_env_args(&mut cmd, &container.env);
    append_mount_args(&mut cmd, &pod.name, &container.mounts);
    append_port_args(&mut cmd, &container.ports);
    append_entrypoint_arg(&mut cmd, &container.command);

    cmd.arg(&container.image);
    append_command_args(&mut cmd, &container.command);
    cmd
}

fn build_container_rm_command(container_name: &str) -> Command {
    let mut cmd = Command::new("podman");
    cmd.args(["rm", "-f", container_name]);
    cmd
}

fn append_label_args(cmd: &mut Command, labels: &std::collections::BTreeMap<String, String>) {
    for (key, value) in labels {
        cmd.arg("--label");
        cmd.arg(format!("{key}={value}"));
    }
}

fn append_env_args(cmd: &mut Command, env: &[crate::runtime::RuntimeEnvVar]) {
    for var in env {
        cmd.arg("--env");
        cmd.arg(format!("{}={}", var.name, var.value));
    }
}

fn append_port_args(cmd: &mut Command, ports: &[u16]) {
    for port in ports {
        cmd.arg("--expose");
        cmd.arg(port.to_string());
    }
}

fn append_command_args(cmd: &mut Command, command: &[String]) {
    for arg in command.iter().skip(1) {
        cmd.arg(arg);
    }
}

fn append_entrypoint_arg(cmd: &mut Command, command: &[String]) {
    if let Some(entrypoint) = command.first() {
        cmd.arg("--entrypoint");
        cmd.arg(entrypoint);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::runtime::{
        RuntimeContainer, RuntimeEnvVar, RuntimeMount, RuntimeMountSource, RuntimePlacement,
        RuntimePod,
    };

    fn command_args(cmd: &Command) -> Vec<String> {
        cmd.get_args()
            .map(|arg| arg.to_string_lossy().into_owned())
            .collect()
    }

    #[test]
    fn remove_output_is_missing_container_matches_expected_errors() {
        let output = Output {
            status: std::process::Command::new("true").status().expect("status"),
            stdout: Vec::new(),
            stderr: b"Error: no such container".to_vec(),
        };
        assert!(remove_output_is_missing_container(&output));
    }

    #[test]
    fn build_pod_create_command_includes_labels_network_and_userns() {
        let pod = RuntimePod {
            name: "demo-agent".to_string(),
            placement: RuntimePlacement::Pod,
            use_runsc: false,
            labels: std::collections::BTreeMap::from([
                ("app".to_string(), "agent".to_string()),
                ("cladding".to_string(), "demo".to_string()),
                (
                    "project_root".to_string(),
                    "/tmp/demo/.cladding".to_string(),
                ),
            ]),
            network_name: "default".to_string(),
            containers: Vec::new(),
            userns_keep_id: true,
        };

        let cmd = build_pod_create_command(false, &pod);
        assert_eq!(
            command_args(&cmd),
            vec![
                "pod",
                "create",
                "--name",
                "demo-agent",
                "--label",
                "app=agent",
                "--label",
                "cladding=demo",
                "--label",
                "project_root=/tmp/demo/.cladding",
                "--network",
                "default",
                "--userns",
                "keep-id",
            ]
        );
    }

    #[test]
    fn build_pod_create_command_does_not_include_runtime_flags() {
        let pod = RuntimePod {
            name: "demo-agent".to_string(),
            placement: RuntimePlacement::Pod,
            use_runsc: false,
            labels: std::collections::BTreeMap::new(),
            network_name: "default".to_string(),
            containers: Vec::new(),
            userns_keep_id: false,
        };

        let cmd = build_pod_create_command(false, &pod);
        let args = command_args(&cmd);
        assert!(!args.iter().any(|arg| arg == "--runtime"));
        assert!(!args.iter().any(|arg| arg == "--runtime-flag"));
    }

    #[test]
    fn build_container_run_command_uses_standalone_container_flags() {
        let pod = RuntimePod {
            name: "demo-agent".to_string(),
            placement: RuntimePlacement::Standalone,
            use_runsc: false,
            labels: std::collections::BTreeMap::from([
                ("app".to_string(), "agent".to_string()),
                ("cladding".to_string(), "demo".to_string()),
                (
                    "project_root".to_string(),
                    "/tmp/demo/.cladding".to_string(),
                ),
            ]),
            network_name: "none".to_string(),
            containers: Vec::new(),
            userns_keep_id: true,
        };
        let container = RuntimeContainer {
            name: "demo-agent-instance".to_string(),
            image: "demo:image".to_string(),
            command: vec!["sleep".to_string(), "infinity".to_string()],
            workdir: None,
            env: Vec::new(),
            mounts: Vec::new(),
            ports: Vec::new(),
            stdin: false,
            tty: false,
        };

        let cmd = build_container_run_command(true, &pod, &container);
        assert_eq!(
            command_args(&cmd),
            vec![
                "--runtime",
                "runsc",
                "--runtime-flag",
                "ignore-cgroups",
                "--runtime-flag",
                "host-uds=all",
                "--runtime-flag",
                "network=none",
                "run",
                "-d",
                "--label",
                "app=agent",
                "--label",
                "cladding=demo",
                "--label",
                "project_root=/tmp/demo/.cladding",
                "--network",
                "none",
                "--userns",
                "keep-id",
                "--hostname",
                "demo-agent",
                "--name",
                "demo-agent-instance",
                "--entrypoint",
                "sleep",
                "demo:image",
                "infinity",
            ]
        );
    }

    #[test]
    fn build_container_run_command_includes_mounts_env_and_io_flags() {
        let container = RuntimeContainer {
            name: "demo-agent-instance".to_string(),
            image: "demo:image".to_string(),
            command: vec!["sleep".to_string(), "infinity".to_string()],
            workdir: Some("/home/user/workspace".to_string()),
            env: vec![RuntimeEnvVar {
                name: "PATH".to_string(),
                value: "/opt/tools/bin".to_string(),
            }],
            mounts: vec![
                RuntimeMount {
                    mount_path: "/opt/config".to_string(),
                    read_only: true,
                    source: RuntimeMountSource::HostPath {
                        path: "/tmp/demo/config".into(),
                    },
                },
                RuntimeMount {
                    mount_path: "/workspace/data".to_string(),
                    read_only: false,
                    source: RuntimeMountSource::NamedVolume {
                        claim_name: "demo-cache".to_string(),
                    },
                },
                RuntimeMount {
                    mount_path: "/workspace/tmp".to_string(),
                    read_only: false,
                    source: RuntimeMountSource::EmptyDir,
                },
                RuntimeMount {
                    mount_path: "/home/user/workspace/.cladding".to_string(),
                    read_only: true,
                    source: RuntimeMountSource::GeneratedEmptyMask {
                        path: "/tmp/demo/runtime/empty-mask".into(),
                    },
                },
            ],
            ports: vec![3000],
            stdin: true,
            tty: true,
        };

        let pod = RuntimePod {
            name: "demo-agent".to_string(),
            placement: RuntimePlacement::Pod,
            use_runsc: false,
            labels: std::collections::BTreeMap::new(),
            network_name: "default".to_string(),
            containers: Vec::new(),
            userns_keep_id: false,
        };

        let cmd = build_container_run_command(false, &pod, &container);
        assert_eq!(
            command_args(&cmd),
            vec![
                "run",
                "-d",
                "--pod",
                "demo-agent",
                "--name",
                "demo-agent-instance",
                "--workdir",
                "/home/user/workspace",
                "-i",
                "-t",
                "--env",
                "PATH=/opt/tools/bin",
                "--volume",
                "/tmp/demo/config:/opt/config:ro",
                "--volume",
                "demo-cache:/workspace/data",
                "--volume",
                "cladding-demo-agent-empty-workspace-tmp:/workspace/tmp",
                "--volume",
                "/tmp/demo/runtime/empty-mask:/home/user/workspace/.cladding:ro",
                "--expose",
                "3000",
                "--entrypoint",
                "sleep",
                "demo:image",
                "infinity",
            ]
        );
    }

    #[test]
    fn build_container_run_command_adds_runsc_flags_when_enabled() {
        let container = RuntimeContainer {
            name: "demo-agent-instance".to_string(),
            image: "demo:image".to_string(),
            command: vec!["sleep".to_string(), "infinity".to_string()],
            workdir: None,
            env: Vec::new(),
            mounts: Vec::new(),
            ports: Vec::new(),
            stdin: false,
            tty: false,
        };

        let pod = RuntimePod {
            name: "demo-agent".to_string(),
            placement: RuntimePlacement::Pod,
            use_runsc: false,
            labels: std::collections::BTreeMap::new(),
            network_name: "default".to_string(),
            containers: Vec::new(),
            userns_keep_id: false,
        };

        let cmd = build_container_run_command(true, &pod, &container);
        assert_eq!(
            command_args(&cmd),
            vec![
                "--runtime",
                "runsc",
                "--runtime-flag",
                "ignore-cgroups",
                "--runtime-flag",
                "host-uds=all",
                "run",
                "-d",
                "--pod",
                "demo-agent",
                "--name",
                "demo-agent-instance",
                "--entrypoint",
                "sleep",
                "demo:image",
                "infinity",
            ]
        );
    }

    #[test]
    fn build_container_run_command_adds_network_for_standalone_containers() {
        let pod = RuntimePod {
            name: "demo-agent".to_string(),
            placement: RuntimePlacement::Standalone,
            use_runsc: false,
            labels: std::collections::BTreeMap::new(),
            network_name: "none".to_string(),
            containers: Vec::new(),
            userns_keep_id: true,
        };
        let container = RuntimeContainer {
            name: "demo-agent-instance".to_string(),
            image: "demo:image".to_string(),
            command: vec!["sleep".to_string(), "infinity".to_string()],
            workdir: None,
            env: Vec::new(),
            mounts: Vec::new(),
            ports: Vec::new(),
            stdin: false,
            tty: false,
        };

        let cmd = build_container_run_command(true, &pod, &container);
        assert_eq!(
            command_args(&cmd),
            vec![
                "--runtime",
                "runsc",
                "--runtime-flag",
                "ignore-cgroups",
                "--runtime-flag",
                "host-uds=all",
                "--runtime-flag",
                "network=none",
                "run",
                "-d",
                "--network",
                "none",
                "--userns",
                "keep-id",
                "--hostname",
                "demo-agent",
                "--name",
                "demo-agent-instance",
                "--entrypoint",
                "sleep",
                "demo:image",
                "infinity",
            ]
        );
    }

    #[test]
    fn remove_output_is_missing_pod_matches_expected_errors() {
        let output = Output {
            status: std::process::Command::new("true").status().expect("status"),
            stdout: Vec::new(),
            stderr: b"Error: no such pod".to_vec(),
        };
        assert!(remove_output_is_missing_pod(&output));
    }
}
