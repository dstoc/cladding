use crate::error::{Error, Result};
use crate::runtime::{
    RuntimeContainer, RuntimeMountSource, RuntimePlacement, RuntimePod, RuntimeSpec,
    RuntimeUserNamespace,
};
use anyhow::Context as _;
use std::collections::{BTreeSet, HashMap};
use std::fs;
use std::io::ErrorKind;
use std::path::Path;
use std::process::{Command, Output};

use super::command::{
    PodmanRuntimeOptions, ensure_success_output, ensure_success_output_with_diagnostics,
    forward_output, podman_command_with_options, run_helper_command, trace_command,
};
use super::mounts::{append_mount_args, generated_empty_mask_dirs};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RuntimeResource {
    pub kind: &'static str,
    pub name: String,
    pub state: String,
}

#[derive(Debug, Clone)]
pub struct RuntimeInventory {
    expected_count: usize,
    pub resources: Vec<RuntimeResource>,
}

impl RuntimeInventory {
    pub fn is_empty(&self) -> bool {
        self.resources.is_empty()
    }

    pub fn is_fully_running(&self) -> bool {
        self.resources.len() == self.expected_count
            && self
                .resources
                .iter()
                .all(|resource| resource.state.eq_ignore_ascii_case("running"))
    }
}

/// Direct runtime helpers for creating and cleaning up Podman resources.
pub fn runtime_create(spec: &RuntimeSpec, verbose: bool, quiet_helpers: bool) -> Result<()> {
    prepare_runtime_socket_dirs(spec)?;
    ensure_runtime_empty_mask_dir(spec)?;
    ensure_proxy_relay_volumes(spec, verbose, quiet_helpers)?;

    for pod in runtime_pods(spec) {
        if pod.placement == RuntimePlacement::Pod {
            pod_create(pod.use_runsc, pod, verbose, quiet_helpers)?;
        }
    }

    for pod in runtime_pods(spec) {
        for container in &pod.containers {
            container_run(pod.use_runsc, pod, container, verbose, quiet_helpers)?;
            if should_install_baffle_ca(spec, pod) {
                install_baffle_ca(container, verbose, quiet_helpers)?;
            }
        }
    }

    Ok(())
}

/// Initialize the project CA with the same Baffle binary, daemon config, and
/// credentials mount that the proxy runtime will use.
pub fn initialize_baffle_ca(spec: &RuntimeSpec, verbose: bool, quiet_helpers: bool) -> Result<()> {
    let mut cmd = build_baffle_ca_init_command(spec);
    trace_command(&cmd, verbose);
    let output = cmd.output().with_context(|| {
        format!(
            "failed to run Baffle CA initialization for project '{}'",
            spec.project_name
        )
    })?;
    if output.status.success() {
        if verbose {
            forward_output(&output)?;
        }
        return Ok(());
    }
    if quiet_helpers {
        return ensure_success_output_with_diagnostics(&output, "Baffle CA initialization");
    }
    if verbose {
        forward_output(&output)?;
    }
    ensure_success_output(&output, "Baffle CA initialization")
}

fn build_baffle_ca_init_command(spec: &RuntimeSpec) -> Command {
    let proxy_image = spec
        .proxy
        .containers
        .first()
        .map(|container| container.image.as_str())
        .expect("runtime spec must include a proxy container");
    let mounts = [
        crate::runtime::RuntimeMount {
            mount_path: "/opt/config".to_string(),
            read_only: true,
            source: crate::runtime::RuntimeMountSource::HostPath {
                path: spec.project_root.join("config"),
            },
        },
        crate::runtime::RuntimeMount {
            mount_path: "/opt/credentials/baffle".to_string(),
            read_only: false,
            source: crate::runtime::RuntimeMountSource::HostPath {
                path: spec.project_root.join("credentials/baffle"),
            },
        },
        crate::runtime::RuntimeMount {
            mount_path: "/opt/tools/bin/baffle".to_string(),
            read_only: true,
            source: crate::runtime::RuntimeMountSource::HostPath {
                path: spec.project_root.join("tools/bin/baffle"),
            },
        },
    ];

    let mut cmd = Command::new("podman");
    cmd.args(["run", "--rm", "--network", "none", "--userns", "keep-id"]);
    append_mount_args(&mut cmd, &spec.proxy.name, &mounts);
    cmd.args(["--entrypoint", "/opt/tools/bin/baffle"]);
    cmd.arg(proxy_image);
    cmd.args(["ca", "init", "--config", "/opt/config/proxy/daemon.toml"]);
    cmd
}

pub fn runtime_inventory(
    spec: &RuntimeSpec,
    verbose: bool,
    quiet_helpers: bool,
) -> Result<RuntimeInventory> {
    let mut expected_count = 0;
    let mut resources = Vec::new();

    for pod in runtime_pods(spec) {
        if pod.placement == RuntimePlacement::Pod {
            expected_count += 1;
            if let Some(state) = inspect_resource_state("pod", &pod.name, verbose, quiet_helpers)? {
                resources.push(RuntimeResource {
                    kind: "pod",
                    name: pod.name.clone(),
                    state,
                });
            }
        }

        for container in &pod.containers {
            expected_count += 1;
            if let Some(state) =
                inspect_resource_state("container", &container.name, verbose, quiet_helpers)?
            {
                resources.push(RuntimeResource {
                    kind: "container",
                    name: container.name.clone(),
                    state,
                });
            }
        }
    }

    Ok(RuntimeInventory {
        expected_count,
        resources,
    })
}

pub fn runtime_cleanup(spec: &RuntimeSpec, verbose: bool, quiet_helpers: bool) -> Result<()> {
    for pod in runtime_pods(spec) {
        for container in &pod.containers {
            container_rm(&container.name, verbose, quiet_helpers)?;
        }
        // For standalone components this is best-effort cleanup for projects
        // started by older builds where execution components were still pods.
        pod_rm(&pod.name, verbose, quiet_helpers)?;
    }

    remove_proxy_relay_volumes(spec, verbose, quiet_helpers)
}

/// Clean up resources only when their Podman labels identify this runtime.
/// This is used by one-off startup and teardown paths so a name collision
/// cannot cause cleanup to remove another project's resources.
pub fn runtime_cleanup_owned(spec: &RuntimeSpec, verbose: bool, quiet_helpers: bool) -> Result<()> {
    let inventory = runtime_inventory(spec, verbose, quiet_helpers)?;
    if inventory.is_empty() {
        return remove_proxy_relay_volumes(spec, verbose, quiet_helpers);
    }
    let exists = |kind: &str, name: &str| {
        inventory
            .resources
            .iter()
            .any(|resource| resource.kind == kind && resource.name == name)
    };

    let mut owned_resources = HashMap::<(String, String), bool>::new();
    for pod in runtime_pods(spec) {
        let pod_exists = pod.placement == RuntimePlacement::Pod && exists("pod", &pod.name);
        let pod_owned = if pod_exists {
            resource_labels_match("pod", &pod.name, spec, verbose, quiet_helpers)?
        } else {
            false
        };
        if pod_exists {
            owned_resources.insert(("pod".to_string(), pod.name.clone()), pod_owned);
        }
        let pod_id = if pod_owned {
            Some(inspect_value(
                "pod",
                &pod.name,
                "{{.Id}}",
                verbose,
                quiet_helpers,
            )?)
        } else {
            None
        };
        for container in &pod.containers {
            if !exists("container", &container.name) {
                continue;
            }

            let owned = match pod.placement {
                RuntimePlacement::Standalone => resource_labels_match(
                    "container",
                    &container.name,
                    spec,
                    verbose,
                    quiet_helpers,
                )?,
                RuntimePlacement::Pod => {
                    if let Some(pod_id) = pod_id.as_deref() {
                        let container_pod = inspect_value(
                            "container",
                            &container.name,
                            "{{.Pod}}",
                            verbose,
                            quiet_helpers,
                        )?;
                        container_pod == pod_id || container_pod == pod.name
                    } else {
                        false
                    }
                }
            };
            owned_resources.insert(("container".to_string(), container.name.clone()), owned);
        }
    }

    if inventory.resources.iter().all(|resource| {
        owned_resources
            .get(&(resource.kind.to_string(), resource.name.clone()))
            .copied()
            .unwrap_or(false)
    }) {
        return runtime_cleanup(spec, verbose, quiet_helpers);
    }

    let mut cleanup_error = None;
    for pod in runtime_pods(spec) {
        let pod_exists = pod.placement == RuntimePlacement::Pod && exists("pod", &pod.name);
        let pod_owned = owned_resources
            .get(&("pod".to_string(), pod.name.clone()))
            .copied()
            .unwrap_or(false);
        let mut pod_cleanup_failed = false;

        for container in &pod.containers {
            if !exists("container", &container.name) {
                continue;
            }
            let container_owned = owned_resources
                .get(&("container".to_string(), container.name.clone()))
                .copied()
                .unwrap_or(false);
            if !container_owned {
                pod_cleanup_failed = true;
                record_cleanup_result(
                    &mut cleanup_error,
                    Err(crate::error::Error::message(format!(
                        "refusing to remove container '{}' because it is not owned by one-off instance '{}'",
                        container.name, spec.project_name
                    ))),
                );
                continue;
            }
            let result = container_rm(&container.name, verbose, quiet_helpers);
            if result.is_err() {
                pod_cleanup_failed = true;
            }
            record_cleanup_result(&mut cleanup_error, result);
        }

        if pod_exists {
            if pod_owned && !pod_cleanup_failed {
                record_cleanup_result(
                    &mut cleanup_error,
                    pod_rm(&pod.name, verbose, quiet_helpers),
                );
            } else if !pod_owned {
                record_cleanup_result(
                    &mut cleanup_error,
                    Err(crate::error::Error::message(format!(
                        "refusing to remove pod '{}' because it is not owned by one-off instance '{}'",
                        pod.name, spec.project_name
                    ))),
                );
            }
        }
    }

    match cleanup_error {
        Some(err) => Err(err),
        None => Ok(()),
    }
}

fn proxy_relay_volume_names(spec: &RuntimeSpec) -> Vec<String> {
    let mut names = BTreeSet::new();
    for pod in runtime_pods(spec) {
        for container in &pod.containers {
            for mount in &container.mounts {
                if let RuntimeMountSource::NamedVolumeChown { claim_name } = &mount.source {
                    names.insert(claim_name.clone());
                }
            }
        }
    }
    names.into_iter().collect()
}

fn ensure_proxy_relay_volumes(
    spec: &RuntimeSpec,
    verbose: bool,
    quiet_helpers: bool,
) -> Result<()> {
    for name in proxy_relay_volume_names(spec) {
        if inspect_resource_state("volume", &name, verbose, quiet_helpers)?.is_some() {
            if !proxy_relay_volume_labels_match(&name, spec, verbose, quiet_helpers)? {
                return Err(Error::message(format!(
                    "refusing to use Podman volume '{name}' because it is not owned by project '{}'",
                    spec.project_name
                )));
            }
            continue;
        }

        let mut cmd = build_volume_create_command(&name, spec);
        trace_command(&cmd, verbose);
        let output = cmd
            .output()
            .with_context(|| format!("failed to create Podman relay volume '{name}'"))?;
        if !output.status.success() {
            if quiet_helpers {
                ensure_success_output_with_diagnostics(&output, "podman volume create")?;
            }
            if verbose {
                forward_output(&output)?;
            }
            ensure_success_output(&output, "podman volume create")?;
        }
        if verbose {
            forward_output(&output)?;
        }
    }

    Ok(())
}

fn remove_proxy_relay_volumes(
    spec: &RuntimeSpec,
    verbose: bool,
    quiet_helpers: bool,
) -> Result<()> {
    for name in proxy_relay_volume_names(spec) {
        if inspect_resource_state("volume", &name, verbose, quiet_helpers)?.is_none() {
            continue;
        }
        if !proxy_relay_volume_labels_match(&name, spec, verbose, quiet_helpers)? {
            return Err(Error::message(format!(
                "refusing to remove Podman volume '{name}' because it is not owned by project '{}'",
                spec.project_name
            )));
        }

        let mut cmd = build_volume_rm_command(&name);
        trace_command(&cmd, verbose);
        let output = cmd
            .output()
            .with_context(|| format!("failed to remove Podman relay volume '{name}'"))?;
        if output.status.success() {
            if verbose {
                forward_output(&output)?;
            }
            continue;
        }
        if remove_output_is_missing_volume(&output) {
            continue;
        }
        if quiet_helpers {
            ensure_success_output_with_diagnostics(&output, "podman volume rm")?;
        }
        if verbose {
            forward_output(&output)?;
        }
        ensure_success_output(&output, "podman volume rm")?;
    }

    Ok(())
}

fn proxy_relay_volume_labels_match(
    name: &str,
    spec: &RuntimeSpec,
    verbose: bool,
    quiet_helpers: bool,
) -> Result<bool> {
    let labels = inspect_value("volume", name, "{{json .Labels}}", verbose, quiet_helpers)?;
    let labels: serde_json::Value = serde_json::from_str(&labels)
        .with_context(|| format!("failed to parse labels for Podman volume {name}"))?;

    Ok(labels.get("cladding").and_then(serde_json::Value::as_str)
        == Some(spec.project_name.as_str())
        && labels
            .get("project_root")
            .and_then(serde_json::Value::as_str)
            == Some(spec.project_root.to_string_lossy().as_ref())
        && labels
            .get("cladding_resource")
            .and_then(serde_json::Value::as_str)
            == Some("baffle-proxy-relay"))
}

fn build_volume_create_command(name: &str, spec: &RuntimeSpec) -> Command {
    let mut cmd = Command::new("podman");
    cmd.args([
        "volume",
        "create",
        "--label",
        &format!("cladding={}", spec.project_name),
        "--label",
        &format!("project_root={}", spec.project_root.display()),
        "--label",
        "cladding_resource=baffle-proxy-relay",
        name,
    ]);
    cmd
}

fn build_volume_rm_command(name: &str) -> Command {
    let mut cmd = Command::new("podman");
    cmd.args(["volume", "rm", name]);
    cmd
}

fn remove_output_is_missing_volume(output: &Output) -> bool {
    let stderr = String::from_utf8_lossy(&output.stderr).to_ascii_lowercase();
    stderr.contains("no such volume") || stderr.contains("no volume with name or id")
}

fn record_cleanup_result(target: &mut Option<crate::error::Error>, result: Result<()>) {
    if let Err(err) = result
        && target.is_none()
    {
        *target = Some(err);
    }
}

fn resource_labels_match(
    kind: &str,
    name: &str,
    spec: &RuntimeSpec,
    verbose: bool,
    quiet_helpers: bool,
) -> Result<bool> {
    let format = match kind {
        "container" => "{{json .Config.Labels}}",
        _ => "{{json .Labels}}",
    };
    let labels = inspect_value(kind, name, format, verbose, quiet_helpers)?;
    let labels: serde_json::Value = serde_json::from_str(&labels)
        .with_context(|| format!("failed to parse labels for {kind} {name}"))?;

    Ok(labels.get("cladding").and_then(serde_json::Value::as_str)
        == Some(spec.project_name.as_str())
        && labels
            .get("project_root")
            .and_then(serde_json::Value::as_str)
            == Some(spec.project_root.to_string_lossy().as_ref()))
}

fn inspect_value(
    kind: &str,
    name: &str,
    format: &str,
    verbose: bool,
    quiet_helpers: bool,
) -> Result<String> {
    let mut cmd = Command::new("podman");
    cmd.args([kind, "inspect", "--format", format, name]);
    trace_command(&cmd, verbose);
    let output = cmd
        .output()
        .with_context(|| format!("failed to inspect {kind} {name}"))?;
    if quiet_helpers {
        ensure_success_output_with_diagnostics(&output, "podman inspect")?;
    } else {
        ensure_success_output(&output, "podman inspect")?;
    }
    Ok(String::from_utf8_lossy(&output.stdout).trim().to_string())
}

fn inspect_resource_state(
    kind: &'static str,
    name: &str,
    verbose: bool,
    quiet_helpers: bool,
) -> Result<Option<String>> {
    let mut exists_cmd = build_resource_exists_command(kind, name);
    trace_command(&exists_cmd, verbose);
    let exists_output = if quiet_helpers {
        Some(
            exists_cmd
                .output()
                .with_context(|| format!("failed to check whether {kind} exists: {name}"))?,
        )
    } else {
        None
    };
    let exists = match &exists_output {
        Some(output) => output.status,
        None => exists_cmd
            .status()
            .with_context(|| format!("failed to check whether {kind} exists: {name}"))?,
    };

    match exists.code() {
        Some(1) => return Ok(None),
        Some(0) => {}
        _ => {
            if let Some(output) = &exists_output {
                ensure_success_output_with_diagnostics(output, "podman exists")?;
            }
            eprintln!("error: failed to check whether {kind} exists: {name}");
            return Err(crate::error::Error::message(format!(
                "podman {kind} exists failed"
            )));
        }
    }

    let mut inspect_cmd = build_resource_inspect_command(kind, name);
    trace_command(&inspect_cmd, verbose);
    let output = inspect_cmd
        .output()
        .with_context(|| format!("failed to inspect {kind}: {name}"))?;
    if !output.status.success() {
        if quiet_helpers {
            ensure_success_output_with_diagnostics(&output, "podman inspect")?;
        }
        return ensure_success_output(&output, "podman inspect").map(|_| None);
    }

    Ok(Some(
        String::from_utf8_lossy(&output.stdout).trim().to_string(),
    ))
}

fn build_resource_exists_command(kind: &str, name: &str) -> Command {
    let mut cmd = Command::new("podman");
    cmd.args([kind, "exists", name]);
    cmd
}

fn build_resource_inspect_command(kind: &str, name: &str) -> Command {
    let mut cmd = Command::new("podman");
    let format = match kind {
        "pod" => "{{.State}}",
        "volume" => "{{.Name}}",
        _ => "{{.State.Status}}",
    };
    cmd.args([kind, "inspect", "--format", format, name]);
    cmd
}

pub fn pod_create(
    use_runsc: bool,
    pod: &RuntimePod,
    verbose: bool,
    quiet_helpers: bool,
) -> Result<()> {
    let mut cmd = build_pod_create_command(use_runsc, pod);
    run_helper_command(&mut cmd, verbose, quiet_helpers, "podman pod create")
}

pub fn pod_rm(pod_name: &str, verbose: bool, quiet_helpers: bool) -> Result<()> {
    let mut cmd = build_pod_rm_command(pod_name);
    trace_command(&cmd, verbose);
    let output = cmd
        .output()
        .with_context(|| format!("failed to run podman pod rm for {pod_name}"))?;

    if output.status.success() {
        if verbose {
            forward_output(&output)?;
        }
        return Ok(());
    }
    if remove_output_is_missing_pod(&output) {
        return Ok(());
    }

    if quiet_helpers {
        ensure_success_output_with_diagnostics(&output, "podman pod rm")?;
    }
    if verbose {
        forward_output(&output)?;
    }
    ensure_success_output(&output, "podman pod rm")
}

pub fn container_run(
    use_runsc: bool,
    pod: &RuntimePod,
    container: &RuntimeContainer,
    verbose: bool,
    quiet_helpers: bool,
) -> Result<()> {
    let mut cmd = build_container_run_command(use_runsc, pod, container);
    run_helper_command(&mut cmd, verbose, quiet_helpers, "podman run")
}

fn should_install_baffle_ca(spec: &RuntimeSpec, pod: &RuntimePod) -> bool {
    pod.name == spec.agent.name
        || spec
            .nw_sandbox
            .as_ref()
            .is_some_and(|sandbox| sandbox.name == pod.name)
}

fn install_baffle_ca(
    container: &RuntimeContainer,
    verbose: bool,
    quiet_helpers: bool,
) -> Result<()> {
    let mut cmd = build_baffle_ca_install_command(&container.name);
    run_helper_command(
        &mut cmd,
        verbose,
        quiet_helpers,
        "Baffle CA installation",
    )
    .inspect_err(|_err| {
        eprintln!(
            "error: failed to install the Baffle CA in execution container '{}'",
            container.name
        );
        eprintln!(
            "hint: ensure the image provides sh, cp, and update-ca-certificates with a writable system trust store"
        );
    })
}

fn build_baffle_ca_install_command(container_name: &str) -> Command {
    let mut cmd = Command::new("podman");
    cmd.args([
        "exec",
        "--user",
        "0",
        container_name,
        "sh",
        "-ec",
        "cp /run/cladding/ca/baffle.crt /usr/local/share/ca-certificates/baffle.crt\nupdate-ca-certificates",
    ]);
    cmd
}

pub fn container_rm(container_name: &str, verbose: bool, quiet_helpers: bool) -> Result<()> {
    let mut cmd = build_container_rm_command(container_name);
    trace_command(&cmd, verbose);
    let output = cmd
        .output()
        .with_context(|| format!("failed to run podman rm for {container_name}"))?;

    if output.status.success() {
        if verbose {
            forward_output(&output)?;
        }
        return Ok(());
    }
    if remove_output_is_missing_container(&output) {
        return Ok(());
    }

    if quiet_helpers {
        ensure_success_output_with_diagnostics(&output, "podman rm")?;
    }
    if verbose {
        forward_output(&output)?;
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
        set_restrictive_dir_permissions(&socket_dir)?;
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
    append_user_namespace_args(&mut cmd, pod.user_namespace);
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
    cmd.arg("--init");
    match pod.placement {
        RuntimePlacement::Pod => {
            cmd.arg("--pod");
            cmd.arg(&pod.name);
        }
        RuntimePlacement::Standalone => {
            append_label_args(&mut cmd, &pod.labels);
            cmd.arg("--network");
            cmd.arg(&pod.network_name);
            append_user_namespace_args(&mut cmd, pod.user_namespace);
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

fn append_user_namespace_args(cmd: &mut Command, user_namespace: RuntimeUserNamespace) {
    let mode = match user_namespace {
        RuntimeUserNamespace::Default => return,
        RuntimeUserNamespace::KeepId => "keep-id",
    };
    cmd.arg("--userns");
    cmd.arg(mode);
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
    use crate::config::{ExecutionComponentConfig, ExecutionConfig};
    use crate::runtime::{
        RuntimeContainer, RuntimeEnvVar, RuntimeMount, RuntimeMountSource, RuntimePlacement,
        RuntimePod, RuntimeUserNamespace,
    };

    fn command_args(cmd: &Command) -> Vec<String> {
        cmd.get_args()
            .map(|arg| arg.to_string_lossy().into_owned())
            .collect()
    }

    #[cfg(unix)]
    #[test]
    fn runtime_socket_tree_has_existing_private_proxy_and_session_directories() {
        use std::os::unix::fs::PermissionsExt as _;

        let project_root =
            std::env::temp_dir().join(format!("cladding-runtime-sockets-{}", std::process::id()));
        let _ = fs::remove_dir_all(&project_root);
        fs::create_dir_all(&project_root).expect("create temporary project root");
        let config = ExecutionConfig {
            name: "demo".to_string(),
            use_runsc: false,
            agent: ExecutionComponentConfig {
                enabled: true,
                image: "agent:image".to_string(),
                build: None,
            },
            nw_sandbox: None,
            fs_sandbox: None,
            proxy: None,
            mounts: Vec::new(),
        };
        let spec = RuntimeSpec::build(&project_root, &config);

        prepare_runtime_socket_dirs(&spec).expect("prepare socket directories");

        for path in spec.generated_runtime_socket_dirs() {
            let metadata = fs::metadata(&path).expect("generated socket directory exists");
            assert_eq!(
                metadata.permissions().mode() & 0o777,
                0o700,
                "{}",
                path.display()
            );
        }
        assert!(project_root.join("runtime/sockets/proxy/agent").is_dir());
        assert!(
            !project_root
                .join("runtime/sockets/proxy/nw-sandbox")
                .exists()
        );
        fs::remove_dir_all(project_root).expect("remove temporary project root");
    }

    #[test]
    fn baffle_ca_install_runs_as_container_root_with_system_trust_update() {
        let cmd = build_baffle_ca_install_command("demo-agent-instance");

        assert_eq!(
            command_args(&cmd),
            vec![
                "exec",
                "--user",
                "0",
                "demo-agent-instance",
                "sh",
                "-ec",
                "cp /run/cladding/ca/baffle.crt /usr/local/share/ca-certificates/baffle.crt\nupdate-ca-certificates",
            ]
        );
    }

    #[test]
    fn baffle_ca_initialization_uses_proxy_image_config_and_private_credentials_mount() {
        let proxy = RuntimePod {
            name: "demo-proxy".to_string(),
            placement: RuntimePlacement::Pod,
            use_runsc: false,
            labels: std::collections::BTreeMap::new(),
            network_name: "default".to_string(),
            containers: vec![RuntimeContainer {
                name: "demo-proxy-instance".to_string(),
                image: "localhost/cladding-proxy:latest".to_string(),
                command: Vec::new(),
                workdir: None,
                env: Vec::new(),
                mounts: Vec::new(),
                ports: Vec::new(),
                stdin: false,
                tty: false,
            }],
            user_namespace: RuntimeUserNamespace::KeepId,
        };
        let empty_pod = |name: &str| RuntimePod {
            name: name.to_string(),
            placement: RuntimePlacement::Standalone,
            use_runsc: false,
            labels: std::collections::BTreeMap::new(),
            network_name: "none".to_string(),
            containers: Vec::new(),
            user_namespace: RuntimeUserNamespace::KeepId,
        };
        let spec = RuntimeSpec {
            project_name: "demo".to_string(),
            project_root: "/tmp/demo/.cladding".into(),
            runtime_root: "/tmp/demo/.cladding".into(),
            use_runsc: false,
            proxy,
            agent: empty_pod("demo-agent"),
            nw_sandbox: None,
            fs_sandbox: None,
        };

        let args = command_args(&build_baffle_ca_init_command(&spec));
        assert!(!args.iter().any(|arg| arg == "--init"));
        assert_eq!(
            args,
            vec![
                "run",
                "--rm",
                "--network",
                "none",
                "--userns",
                "keep-id",
                "--volume",
                "/tmp/demo/.cladding/config:/opt/config:ro",
                "--volume",
                "/tmp/demo/.cladding/credentials/baffle:/opt/credentials/baffle",
                "--volume",
                "/tmp/demo/.cladding/tools/bin/baffle:/opt/tools/bin/baffle:ro",
                "--entrypoint",
                "/opt/tools/bin/baffle",
                "localhost/cladding-proxy:latest",
                "ca",
                "init",
                "--config",
                "/opt/config/proxy/daemon.toml",
            ]
        );
    }

    #[test]
    fn baffle_ca_install_targets_agent_and_enabled_network_sandbox_only() {
        let pod = |name: &str| RuntimePod {
            name: name.to_string(),
            placement: RuntimePlacement::Standalone,
            use_runsc: false,
            labels: std::collections::BTreeMap::new(),
            network_name: "none".to_string(),
            containers: Vec::new(),
            user_namespace: RuntimeUserNamespace::KeepId,
        };
        let spec = RuntimeSpec {
            project_name: "demo".to_string(),
            project_root: "/tmp/demo/.cladding".into(),
            runtime_root: "/tmp/demo/.cladding".into(),
            use_runsc: false,
            proxy: pod("demo-proxy"),
            agent: pod("demo-agent"),
            nw_sandbox: Some(pod("demo-nw-sandbox")),
            fs_sandbox: Some(pod("demo-fs-sandbox")),
        };

        assert!(should_install_baffle_ca(&spec, &spec.agent));
        assert!(should_install_baffle_ca(
            &spec,
            spec.nw_sandbox.as_ref().expect("network sandbox")
        ));
        assert!(!should_install_baffle_ca(&spec, &spec.proxy));
        assert!(!should_install_baffle_ca(
            &spec,
            spec.fs_sandbox.as_ref().expect("filesystem sandbox")
        ));
    }

    #[test]
    fn runtime_containers_enable_podman_init_for_every_component() {
        let config = ExecutionConfig {
            name: "demo".to_string(),
            use_runsc: false,
            agent: ExecutionComponentConfig {
                enabled: true,
                image: "agent:image".to_string(),
                build: None,
            },
            nw_sandbox: Some(ExecutionComponentConfig {
                enabled: true,
                image: "nw-sandbox:image".to_string(),
                build: None,
            }),
            fs_sandbox: Some(ExecutionComponentConfig {
                enabled: true,
                image: "fs-sandbox:image".to_string(),
                build: None,
            }),
            proxy: None,
            mounts: Vec::new(),
        };
        let spec = RuntimeSpec::build(Path::new("/tmp/demo/.cladding"), &config);
        let pods = runtime_pods(&spec);

        assert_eq!(pods.len(), 4);
        for pod in pods {
            for container in &pod.containers {
                let args =
                    command_args(&build_container_run_command(pod.use_runsc, pod, container));
                assert!(
                    args.iter().any(|arg| arg == "--init"),
                    "{} did not enable Podman init",
                    container.name
                );
            }
        }
    }

    fn successful_status() -> std::process::ExitStatus {
        std::process::Command::new("true").status().expect("status")
    }

    #[test]
    fn runtime_inventory_requires_every_resource_to_be_running() {
        let complete = RuntimeInventory {
            expected_count: 2,
            resources: vec![
                RuntimeResource {
                    kind: "pod",
                    name: "demo-proxy".to_string(),
                    state: "Running".to_string(),
                },
                RuntimeResource {
                    kind: "container",
                    name: "demo-agent-instance".to_string(),
                    state: "running".to_string(),
                },
            ],
        };
        assert!(complete.is_fully_running());

        let partial = RuntimeInventory {
            expected_count: 2,
            resources: complete.resources[..1].to_vec(),
        };
        assert!(!partial.is_fully_running());

        let stopped = RuntimeInventory {
            expected_count: 2,
            resources: vec![
                complete.resources[0].clone(),
                RuntimeResource {
                    state: "exited".to_string(),
                    ..complete.resources[1].clone()
                },
            ],
        };
        assert!(!stopped.is_fully_running());
    }

    #[test]
    fn resource_inspection_commands_target_pods_and_containers() {
        assert_eq!(
            command_args(&build_resource_exists_command("pod", "demo-proxy")),
            vec!["pod", "exists", "demo-proxy"]
        );
        assert_eq!(
            command_args(&build_resource_inspect_command("pod", "demo-proxy")),
            vec!["pod", "inspect", "--format", "{{.State}}", "demo-proxy"]
        );
        assert_eq!(
            command_args(&build_resource_inspect_command(
                "container",
                "demo-agent-instance"
            )),
            vec![
                "container",
                "inspect",
                "--format",
                "{{.State.Status}}",
                "demo-agent-instance"
            ]
        );
    }

    #[test]
    fn remove_output_is_missing_container_matches_expected_errors() {
        let output = Output {
            status: successful_status(),
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
            user_namespace: RuntimeUserNamespace::KeepId,
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
    fn proxy_pod_preserves_the_invoking_user_identity() {
        let pod = RuntimePod {
            name: "demo-proxy".to_string(),
            placement: RuntimePlacement::Pod,
            use_runsc: false,
            labels: std::collections::BTreeMap::new(),
            network_name: "default".to_string(),
            containers: Vec::new(),
            user_namespace: RuntimeUserNamespace::KeepId,
        };

        let cmd = build_pod_create_command(false, &pod);

        assert_eq!(
            command_args(&cmd),
            vec![
                "pod",
                "create",
                "--name",
                "demo-proxy",
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
            user_namespace: RuntimeUserNamespace::Default,
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
            user_namespace: RuntimeUserNamespace::KeepId,
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
                "--init",
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
                    mount_path: "/run/cladding/proxy/agent".to_string(),
                    read_only: false,
                    source: RuntimeMountSource::NamedVolume {
                        claim_name: "demo-proxy-agent-relay".to_string(),
                    },
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
            user_namespace: RuntimeUserNamespace::Default,
        };

        let cmd = build_container_run_command(false, &pod, &container);
        assert_eq!(
            command_args(&cmd),
            vec![
                "run",
                "-d",
                "--init",
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
                "demo-proxy-agent-relay:/run/cladding/proxy/agent",
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
    fn container_cleanup_removes_the_container_that_owns_disposable_mounts() {
        let cmd = build_container_rm_command("demo-agent-instance");

        assert_eq!(command_args(&cmd), vec!["rm", "-f", "demo-agent-instance"]);
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
            user_namespace: RuntimeUserNamespace::Default,
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
                "--init",
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
            user_namespace: RuntimeUserNamespace::KeepId,
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
                "--init",
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
