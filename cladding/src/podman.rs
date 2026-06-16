use crate::assets::containerfile;
use crate::error::{Error, Result};
use crate::network::{NetworkSettings, is_ipv4_cidr, parse_cladding_pool_index};
use crate::runtime::{
    RuntimeContainer, RuntimeMount, RuntimeMountSource, RuntimePod, RuntimeSpec, RuntimeTask,
};
use anyhow::Context as _;
use serde_json::Value;
use std::collections::HashMap;
use std::env;
use std::fs;
use std::process::{Command, ExitStatus, Output, Stdio};

pub fn podman_required(message: &str) -> Result<()> {
    if command_exists("podman") {
        Ok(())
    } else {
        eprintln!("missing: {message}");
        Err(Error::message("missing podman"))
    }
}

pub fn ensure_network_settings(network_settings: &NetworkSettings) -> Result<()> {
    let status = Command::new("podman")
        .args(["network", "exists", &network_settings.network])
        .status()
        .with_context(|| "failed to check existing networks via podman")?;

    match status.code() {
        Some(0) => {
            let output = Command::new("podman")
                .args(["network", "inspect", &network_settings.network])
                .output()
                .with_context(|| "failed to inspect podman network")?;

            if !output.status.success() {
                return ensure_success_output(&output, "podman network inspect");
            }

            let stdout = String::from_utf8_lossy(&output.stdout);
            if !stdout.contains(&format!(
                "\"subnet\": \"{}\"",
                network_settings.network_subnet
            )) {
                eprintln!(
                    "error: network {} exists but is not on {}",
                    network_settings.network, network_settings.network_subnet
                );
                eprintln!(
                    "hint: run 'podman network rm {}' and retry",
                    network_settings.network
                );
                return Err(Error::message("network subnet mismatch"));
            }
        }
        Some(1) => {
            let status = Command::new("podman")
                .args([
                    "network",
                    "create",
                    "--subnet",
                    &network_settings.network_subnet,
                    &network_settings.network,
                ])
                .status()
                .with_context(|| "failed to create podman network")?;
            ensure_success(status, "podman network create")?;
        }
        _ => {
            eprintln!("error: failed to check existing networks via podman");
            return Err(Error::message("podman network exists failed"));
        }
    }

    Ok(())
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EnsureNetworkOutcome {
    Ready,
    SubnetMismatch,
}

pub fn ensure_pool_network_settings(
    network_settings: &NetworkSettings,
) -> Result<EnsureNetworkOutcome> {
    let status = Command::new("podman")
        .args(["network", "exists", &network_settings.network])
        .status()
        .with_context(|| "failed to check existing networks via podman")?;

    match status.code() {
        Some(0) => {
            let output = Command::new("podman")
                .args(["network", "inspect", &network_settings.network])
                .output()
                .with_context(|| "failed to inspect podman network")?;

            if !output.status.success() {
                return ensure_success_output(&output, "podman network inspect")
                    .map(|_| EnsureNetworkOutcome::Ready);
            }

            let stdout = String::from_utf8_lossy(&output.stdout);
            if stdout.contains(&format!(
                "\"subnet\": \"{}\"",
                network_settings.network_subnet
            )) {
                Ok(EnsureNetworkOutcome::Ready)
            } else {
                Ok(EnsureNetworkOutcome::SubnetMismatch)
            }
        }
        Some(1) => {
            let status = Command::new("podman")
                .args([
                    "network",
                    "create",
                    "--subnet",
                    &network_settings.network_subnet,
                    &network_settings.network,
                ])
                .status()
                .with_context(|| "failed to create podman network")?;
            ensure_success(status, "podman network create")?;
            Ok(EnsureNetworkOutcome::Ready)
        }
        _ => {
            eprintln!("error: failed to check existing networks via podman");
            Err(Error::message("podman network exists failed"))
        }
    }
}

pub fn podman_build_image(image: &str, host_uid: u32, host_gid: u32) -> Result<()> {
    let mut cmd = Command::new("podman");
    cmd.args([
        "build",
        "--build-arg",
        &format!("UID={host_uid}"),
        "--build-arg",
        &format!("GID={host_gid}"),
        "-t",
        image,
        "-f",
        "-",
        ".",
    ])
    .stdin(Stdio::piped());

    let mut child = cmd.spawn().with_context(|| "failed to run podman build")?;

    if let Some(mut stdin) = child.stdin.take() {
        use std::io::Write;
        stdin
            .write_all(containerfile().as_bytes())
            .and_then(|_| stdin.flush())
            .with_context(|| "failed to write Containerfile to podman")?;
    }

    let status = child
        .wait()
        .with_context(|| "failed to wait on podman build")?;

    ensure_success(status, "podman build")
}

#[derive(Debug, Clone)]
pub struct NetworkSubnet {
    pub name: String,
    pub subnet: String,
}

pub fn list_podman_network_subnets() -> Result<Vec<NetworkSubnet>> {
    let output = Command::new("podman")
        .args(["network", "ls", "--format", "{{.Name}}"])
        .output()
        .with_context(|| "failed to list podman networks")?;

    if !output.status.success() {
        return ensure_success_output(&output, "podman network ls").map(|_| Vec::new());
    }

    let stdout = String::from_utf8_lossy(&output.stdout);
    let mut subnets = Vec::new();

    for name in stdout.lines().map(str::trim).filter(|s| !s.is_empty()) {
        let output = Command::new("podman")
            .args([
                "network",
                "inspect",
                "-f",
                "{{range .Subnets}}{{.Subnet}}{{\"\\n\"}}{{end}}",
                name,
            ])
            .output()
            .with_context(|| "failed to inspect podman network")?;

        if !output.status.success() {
            return ensure_success_output(&output, "podman network inspect").map(|_| Vec::new());
        }

        let stdout = String::from_utf8_lossy(&output.stdout);
        for line in stdout.lines().map(str::trim) {
            if is_ipv4_cidr(line) {
                subnets.push(NetworkSubnet {
                    name: name.to_string(),
                    subnet: line.to_string(),
                });
            }
        }
    }

    Ok(subnets)
}

/// Direct runtime helpers that assume the caller has already selected and ensured the
/// desired `cladding-N` network.
pub fn runtime_create(spec: &RuntimeSpec) -> Result<()> {
    ensure_runtime_empty_mask_dir(spec)?;

    for pod in runtime_pods(spec) {
        pod_create(pod)?;
    }

    for pod in runtime_pods(spec) {
        for task in &pod.init_tasks {
            run_pod_task(pod, task)?;
        }
    }

    for pod in runtime_pods(spec) {
        for container in &pod.containers {
            container_create(&pod.name, container)?;
        }
    }

    for pod in runtime_pods(spec) {
        for container in &pod.containers {
            container_start(&container.name)?;
        }
    }

    Ok(())
}

pub fn runtime_cleanup(spec: &RuntimeSpec) -> Result<()> {
    for pod in runtime_pods(spec) {
        for container in &pod.containers {
            container_rm(&container.name)?;
        }
        pod_rm(&pod.name)?;
    }

    Ok(())
}

pub fn pod_create(pod: &RuntimePod) -> Result<()> {
    let status = build_pod_create_command(pod)
        .status()
        .with_context(|| format!("failed to run podman pod create for {}", pod.name))?;
    ensure_success(status, "podman pod create")
}

pub fn pod_rm(pod_name: &str) -> Result<()> {
    let output = build_pod_rm_command(pod_name)
        .output()
        .with_context(|| format!("failed to run podman pod rm for {pod_name}"))?;

    if output.status.success() || remove_output_is_missing_pod(&output) {
        return Ok(());
    }

    ensure_success_output(&output, "podman pod rm")
}

pub fn container_create(pod_name: &str, container: &RuntimeContainer) -> Result<()> {
    let status = build_container_create_command(pod_name, container)
        .status()
        .with_context(|| {
            format!(
                "failed to run podman container create for {} in pod {pod_name}",
                container.name
            )
        })?;
    ensure_success(status, "podman container create")
}

pub fn container_start(container_name: &str) -> Result<()> {
    let status = build_container_start_command(container_name)
        .status()
        .with_context(|| format!("failed to run podman container start for {container_name}"))?;
    ensure_success(status, "podman container start")
}

pub fn container_wait(container_name: &str) -> Result<i32> {
    let output = build_container_wait_command(container_name)
        .output()
        .with_context(|| format!("failed to run podman container wait for {container_name}"))?;

    if !output.status.success() {
        return ensure_success_output(&output, "podman container wait").map(|_| 0);
    }

    let stdout = String::from_utf8_lossy(&output.stdout);
    let code = stdout
        .trim()
        .parse::<i32>()
        .with_context(|| format!("failed to parse podman wait output for {container_name}"))?;
    Ok(code)
}

pub fn container_rm(container_name: &str) -> Result<()> {
    let output = build_container_rm_command(container_name)
        .output()
        .with_context(|| format!("failed to run podman rm for {container_name}"))?;

    if output.status.success() || remove_output_is_missing_container(&output) {
        return Ok(());
    }

    ensure_success_output(&output, "podman rm")
}

pub fn ensure_success(status: ExitStatus, context: &'static str) -> Result<()> {
    if status.success() {
        return Ok(());
    }

    let code = status.code().unwrap_or(1);
    eprintln!("error: {context} failed (exit code {code})");
    Err(Error::CommandFailed { context, code })
}

pub fn ensure_success_output(output: &Output, context: &'static str) -> Result<()> {
    if output.status.success() {
        return Ok(());
    }

    let code = output.status.code().unwrap_or(1);
    eprintln!("error: {context} failed (exit code {code})");
    let stderr = String::from_utf8_lossy(&output.stderr);
    if !stderr.trim().is_empty() {
        eprintln!("{stderr}");
    }
    Err(Error::CommandFailed { context, code })
}

fn command_exists(command: &str) -> bool {
    env::var_os("PATH").is_some_and(|paths| {
        env::split_paths(&paths).any(|path| {
            let candidate = path.join(command);
            candidate.is_file()
        })
    })
}

#[derive(Debug, Clone)]
pub struct RunningProject {
    pub name: String,
    pub project_root: String,
    pub pod_count: usize,
}

#[derive(Debug, Clone)]
pub struct RunningProjectNetwork {
    pub name: String,
    pub project_root: String,
    pub network: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ExposeProxy {
    pub id: String,
    pub name: String,
    pub host_port: u16,
    pub container_port: u16,
    pub status: String,
}

pub fn list_running_projects() -> Result<Vec<RunningProject>> {
    let items = list_running_pod_items()?;
    let mut projects: HashMap<(String, String), usize> = HashMap::new();
    for item in items {
        let key = (item.name, item.project_root);
        let count = projects.entry(key).or_insert(0);
        *count += 1;
    }

    let mut results: Vec<RunningProject> = projects
        .into_iter()
        .map(|((name, project_root), pod_count)| RunningProject {
            name,
            project_root,
            pod_count,
        })
        .collect();

    results.sort_by(|a, b| {
        a.name
            .cmp(&b.name)
            .then_with(|| a.project_root.cmp(&b.project_root))
    });

    Ok(results)
}

pub fn list_running_project_networks() -> Result<Vec<RunningProjectNetwork>> {
    let items = list_running_pod_items()?;
    let mut networks: HashMap<(String, String), String> = HashMap::new();

    for item in items {
        let network = inspect_pool_network_for_pod(&item.pod_id)?;
        let Some(network) = network else {
            continue;
        };

        let key = (item.name.clone(), item.project_root.clone());
        if let Some(existing) = networks.get(&key) {
            if existing != &network {
                eprintln!(
                    "error: running project '{}' has pods on multiple cladding networks",
                    item.name
                );
                eprintln!("project_root: {}", item.project_root);
                eprintln!("networks: {existing}, {network}");
                return Err(Error::message("inconsistent active network"));
            }
            continue;
        }
        networks.insert(key, network);
    }

    let mut results: Vec<RunningProjectNetwork> = networks
        .into_iter()
        .map(|((name, project_root), network)| RunningProjectNetwork {
            name,
            project_root,
            network,
        })
        .collect();

    results.sort_by(|a, b| {
        a.name
            .cmp(&b.name)
            .then_with(|| a.project_root.cmp(&b.project_root))
    });

    Ok(results)
}

pub fn list_project_expose_proxies(
    project_name: &str,
    project_root: &str,
    include_stopped: bool,
) -> Result<Vec<ExposeProxy>> {
    let items = list_expose_proxy_items(project_name, include_stopped)?;
    let mut results = Vec::new();

    for item in items {
        if item.project_root != project_root {
            continue;
        }
        if item.target != "agent" {
            continue;
        }
        results.push(item.proxy);
    }

    results.sort_by(|a, b| {
        a.host_port
            .cmp(&b.host_port)
            .then_with(|| a.container_port.cmp(&b.container_port))
            .then_with(|| a.name.cmp(&b.name))
    });

    Ok(results)
}

pub fn podman_container_exists(container_name: &str) -> Result<bool> {
    let status = Command::new("podman")
        .args(["container", "exists", container_name])
        .status()
        .with_context(|| "failed to run podman container exists")?;

    match status.code() {
        Some(0) => Ok(true),
        Some(1) => Ok(false),
        _ => {
            eprintln!("error: failed to check whether container exists: {container_name}");
            Err(Error::message("podman container exists failed"))
        }
    }
}

pub fn podman_remove_containers(
    container_ids: &[String],
    force: bool,
    ignore_missing: bool,
) -> Result<()> {
    for container_id in container_ids {
        let mut cmd = Command::new("podman");
        cmd.arg("rm");
        if force {
            cmd.arg("-f");
        }
        cmd.arg(container_id);

        let output = cmd.output().with_context(|| "failed to run podman rm")?;

        if output.status.success() {
            continue;
        }

        if ignore_missing && remove_output_is_missing_container(&output) {
            continue;
        }

        return ensure_success_output(&output, "podman rm");
    }

    Ok(())
}

#[derive(Debug, Clone)]
struct RunningPodItem {
    pod_id: String,
    name: String,
    project_root: String,
}

#[derive(Debug, Clone)]
struct ExposeProxyItem {
    proxy: ExposeProxy,
    project_root: String,
    target: String,
}

fn list_running_pod_items() -> Result<Vec<RunningPodItem>> {
    let output = Command::new("podman")
        .args([
            "pod",
            "ps",
            "--filter",
            "label=cladding",
            "--filter",
            "status=running",
            "--format",
            "json",
        ])
        .output()
        .with_context(|| "failed to run podman pod ps")?;

    if !output.status.success() {
        return ensure_success_output(&output, "podman pod ps").map(|_| Vec::new());
    }

    let stdout = String::from_utf8_lossy(&output.stdout);
    let parsed: Value = serde_json::from_str(&stdout)
        .with_context(|| "failed to parse podman pod ps json output")?;
    let Some(items) = parsed.as_array() else {
        return Ok(Vec::new());
    };

    let mut pods = Vec::new();
    for item in items {
        let Some(labels_value) = item.get("Labels") else {
            continue;
        };
        let labels = parse_labels(labels_value);
        let Some(name) = labels.get("cladding") else {
            continue;
        };
        let Some(project_root) = labels.get("project_root") else {
            continue;
        };
        let pod_id = item
            .get("Id")
            .and_then(Value::as_str)
            .or_else(|| item.get("ID").and_then(Value::as_str))
            .unwrap_or_default();
        if pod_id.is_empty() {
            continue;
        }
        pods.push(RunningPodItem {
            pod_id: pod_id.to_string(),
            name: name.to_string(),
            project_root: project_root.to_string(),
        });
    }

    Ok(pods)
}

fn list_expose_proxy_items(
    project_name: &str,
    include_stopped: bool,
) -> Result<Vec<ExposeProxyItem>> {
    let mut cmd = Command::new("podman");
    cmd.arg("ps");
    if include_stopped {
        cmd.arg("-a");
    }
    cmd.args([
        "--filter",
        "label=cladding_expose=true",
        "--filter",
        &format!("label=cladding={project_name}"),
        "--format",
        "json",
    ]);

    let output = cmd
        .output()
        .with_context(|| "failed to run podman ps for expose proxies")?;

    if !output.status.success() {
        return ensure_success_output(&output, "podman ps").map(|_| Vec::new());
    }

    let stdout = String::from_utf8_lossy(&output.stdout);
    let parsed: Value =
        serde_json::from_str(&stdout).with_context(|| "failed to parse podman ps json output")?;

    Ok(parse_expose_proxy_items(&parsed))
}

fn inspect_pool_network_for_pod(pod_id: &str) -> Result<Option<String>> {
    let inspect = Command::new("podman")
        .args(["pod", "inspect", pod_id, "--format", "json"])
        .output()
        .with_context(|| "failed to inspect running pod")?;
    if !inspect.status.success() {
        return ensure_success_output(&inspect, "podman pod inspect").map(|_| None);
    }

    let inspect_stdout = String::from_utf8_lossy(&inspect.stdout);
    let parsed: Value = serde_json::from_str(&inspect_stdout)
        .with_context(|| "failed to parse podman pod inspect json output")?;
    let Some(infra_id) = find_infra_container_id(&parsed) else {
        return Ok(None);
    };

    let inspect_infra = Command::new("podman")
        .args(["container", "inspect", &infra_id, "--format", "json"])
        .output()
        .with_context(|| "failed to inspect pod infra container")?;
    if !inspect_infra.status.success() {
        return ensure_success_output(&inspect_infra, "podman container inspect").map(|_| None);
    }

    let inspect_infra_stdout = String::from_utf8_lossy(&inspect_infra.stdout);
    let parsed: Value = serde_json::from_str(&inspect_infra_stdout)
        .with_context(|| "failed to parse podman container inspect json output")?;
    let Some(networks_obj) = find_networks_object(&parsed) else {
        return Ok(None);
    };

    for key in networks_obj.keys() {
        if parse_cladding_pool_index(key).is_some() {
            return Ok(Some(key.to_string()));
        }
    }
    Ok(None)
}

fn find_infra_container_id(value: &Value) -> Option<String> {
    if let Some(items) = value.as_array() {
        for item in items {
            if let Some(id) = find_infra_container_id(item) {
                return Some(id);
            }
        }
        return None;
    }
    value
        .get("InfraContainerID")
        .and_then(Value::as_str)
        .filter(|id| !id.is_empty())
        .map(ToString::to_string)
}

fn find_networks_object(value: &Value) -> Option<&serde_json::Map<String, Value>> {
    if let Some(items) = value.as_array() {
        for item in items {
            if let Some(networks) = find_networks_object(item) {
                return Some(networks);
            }
        }
        return None;
    }
    value
        .get("NetworkSettings")
        .and_then(|network_settings| network_settings.get("Networks"))
        .and_then(Value::as_object)
}

fn parse_labels(value: &Value) -> HashMap<String, String> {
    let mut labels = HashMap::new();
    match value {
        Value::Object(map) => {
            for (key, val) in map {
                if let Some(s) = val.as_str() {
                    labels.insert(key.clone(), s.to_string());
                }
            }
        }
        Value::String(raw) => {
            for entry in raw.split(',') {
                let entry = entry.trim();
                if entry.is_empty() {
                    continue;
                }
                let mut parts = entry.splitn(2, '=');
                let key = parts.next().unwrap_or("").trim();
                let val = parts.next().unwrap_or("").trim();
                if !key.is_empty() && !val.is_empty() {
                    labels.insert(key.to_string(), val.to_string());
                }
            }
        }
        _ => {}
    }
    labels
}

fn parse_expose_proxy_items(value: &Value) -> Vec<ExposeProxyItem> {
    let Some(items) = value.as_array() else {
        return Vec::new();
    };

    let mut proxies = Vec::new();
    for item in items {
        let Some(proxy) = parse_expose_proxy_item(item) else {
            continue;
        };
        proxies.push(proxy);
    }
    proxies
}

fn parse_expose_proxy_item(value: &Value) -> Option<ExposeProxyItem> {
    let labels = value.get("Labels").map(parse_labels).unwrap_or_default();
    if labels.get("cladding_expose").map(String::as_str) != Some("true") {
        return None;
    }

    let project_root = labels.get("project_root")?.to_string();
    let target = labels.get("cladding_expose_target")?.to_string();
    let container_port = labels
        .get("cladding_expose_container_port")?
        .parse::<u16>()
        .ok()?;
    let host_port = labels
        .get("cladding_expose_host_port")?
        .parse::<u16>()
        .ok()?;

    let id = get_json_string(value, &["Id", "ID"])?;
    let name = get_json_name(value)?;
    let status =
        get_json_string(value, &["Status"]).or_else(|| get_json_string(value, &["State"]))?;

    Some(ExposeProxyItem {
        proxy: ExposeProxy {
            id,
            name,
            host_port,
            container_port,
            status,
        },
        project_root,
        target,
    })
}

fn get_json_string(value: &Value, keys: &[&str]) -> Option<String> {
    for key in keys {
        let Some(raw) = value.get(*key) else {
            continue;
        };
        if let Some(string) = raw.as_str().filter(|s| !s.is_empty()) {
            return Some(string.to_string());
        }
    }
    None
}

fn get_json_name(value: &Value) -> Option<String> {
    for key in ["Names", "Name"] {
        let Some(raw) = value.get(key) else {
            continue;
        };
        match raw {
            Value::String(name) if !name.is_empty() => return Some(name.to_string()),
            Value::Array(items) => {
                for item in items {
                    if let Some(name) = item.as_str().filter(|s| !s.is_empty()) {
                        return Some(name.to_string());
                    }
                }
            }
            _ => {}
        }
    }
    None
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

fn ensure_runtime_empty_mask_dir(spec: &RuntimeSpec) -> Result<()> {
    for empty_mask in generated_empty_mask_dirs(spec) {
        fs::create_dir_all(&empty_mask).with_context(|| {
            format!("failed to create runtime mask dir {}", empty_mask.display())
        })?;
    }
    Ok(())
}

fn run_pod_task(pod: &RuntimePod, task: &RuntimeTask) -> Result<()> {
    let mut cmd = Command::new("podman");
    cmd.arg("run");
    cmd.arg("--rm");
    cmd.arg("--pod");
    cmd.arg(&pod.name);
    cmd.arg("--name");
    cmd.arg(format!("{}-{}", pod.name, task.name));

    if let Some(user) = task.run_as_user {
        let group = task.run_as_group.unwrap_or(user);
        cmd.arg("--user");
        cmd.arg(format!("{user}:{group}"));
    }

    for capability in &task.added_capabilities {
        cmd.arg("--cap-add");
        cmd.arg(capability);
    }

    append_env_args(&mut cmd, &task.env);
    append_mount_args(&mut cmd, &pod.name, &task.mounts);
    append_entrypoint_arg(&mut cmd, &task.command);
    cmd.arg(&task.image);
    append_command_args(&mut cmd, &task.command);

    let status = cmd.status().with_context(|| {
        format!(
            "failed to run podman task {} in pod {}",
            task.name, pod.name
        )
    })?;
    ensure_success(status, "podman run")
}

fn build_pod_create_command(pod: &RuntimePod) -> Command {
    let mut cmd = Command::new("podman");
    cmd.args(["pod", "create", "--name", &pod.name]);
    append_label_args(&mut cmd, &pod.labels);
    cmd.arg("--network");
    cmd.arg(&pod.network_name);
    cmd.arg("--ip");
    cmd.arg(&pod.ip);
    if pod.userns_keep_id {
        cmd.arg("--userns");
        cmd.arg("keep-id");
    }
    append_host_alias_args(&mut cmd, &pod.host_aliases);
    cmd
}

fn build_pod_rm_command(pod_name: &str) -> Command {
    let mut cmd = Command::new("podman");
    cmd.args(["pod", "rm", "-f", pod_name]);
    cmd
}

fn build_container_create_command(pod_name: &str, container: &RuntimeContainer) -> Command {
    let mut cmd = Command::new("podman");
    cmd.arg("create");
    cmd.arg("--pod");
    cmd.arg(pod_name);
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
    append_mount_args(&mut cmd, pod_name, &container.mounts);
    append_port_args(&mut cmd, &container.ports);
    append_entrypoint_arg(&mut cmd, &container.command);

    cmd.arg(&container.image);
    append_command_args(&mut cmd, &container.command);
    cmd
}

fn build_container_start_command(container_name: &str) -> Command {
    let mut cmd = Command::new("podman");
    cmd.args(["start", container_name]);
    cmd
}

fn build_container_wait_command(container_name: &str) -> Command {
    let mut cmd = Command::new("podman");
    cmd.args(["wait", container_name]);
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

fn append_host_alias_args(cmd: &mut Command, host_aliases: &[crate::runtime::RuntimeHostAlias]) {
    for alias in host_aliases {
        for hostname in &alias.hostnames {
            cmd.arg("--add-host");
            cmd.arg(format!("{hostname}:{}", alias.ip));
        }
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

fn append_mount_args(cmd: &mut Command, pod_name: &str, mounts: &[RuntimeMount]) {
    for mount in mounts {
        let source = match &mount.source {
            RuntimeMountSource::HostPath { path } => path.display().to_string(),
            RuntimeMountSource::NamedVolume { claim_name } => claim_name.clone(),
            RuntimeMountSource::GeneratedEmptyMask { path } => path.display().to_string(),
            RuntimeMountSource::EmptyDir => empty_dir_volume_name(pod_name, &mount.mount_path),
        };

        let mut volume = format!("{source}:{}", mount.mount_path);
        if mount.read_only {
            volume.push_str(":ro");
        }
        cmd.arg("--volume");
        cmd.arg(volume);
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

fn empty_dir_volume_name(pod_name: &str, mount_path: &str) -> String {
    format!(
        "cladding-{pod_name}-empty-{}",
        sanitize_volume_fragment(mount_path)
    )
}

fn generated_empty_mask_dirs(spec: &RuntimeSpec) -> Vec<std::path::PathBuf> {
    let mut paths = std::collections::BTreeSet::new();

    for pod in runtime_pods(spec) {
        for task in &pod.init_tasks {
            for mount in &task.mounts {
                if let RuntimeMountSource::GeneratedEmptyMask { path } = &mount.source {
                    paths.insert(path.clone());
                }
            }
        }
        for container in &pod.containers {
            for mount in &container.mounts {
                if let RuntimeMountSource::GeneratedEmptyMask { path } = &mount.source {
                    paths.insert(path.clone());
                }
            }
        }
    }

    paths.into_iter().collect()
}

fn sanitize_volume_fragment(value: &str) -> String {
    let fragment = value.trim_matches('/').replace('/', "-");
    let fragment = fragment
        .chars()
        .map(|ch| {
            if ch.is_ascii_alphanumeric() || matches!(ch, '.' | '_' | '-') {
                ch
            } else {
                '-'
            }
        })
        .collect::<String>();
    if fragment.is_empty() {
        "root".to_string()
    } else {
        fragment
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::runtime::{
        RuntimeContainer, RuntimeEnvVar, RuntimeHostAlias, RuntimeMount, RuntimeMountSource,
        RuntimePod,
    };
    use serde_json::json;

    fn command_args(cmd: &Command) -> Vec<String> {
        cmd.get_args()
            .map(|arg| arg.to_string_lossy().into_owned())
            .collect()
    }

    #[test]
    fn parse_labels_supports_string_and_object_forms() {
        let string_labels = parse_labels(&Value::String(
            "cladding=demo, project_root=/tmp/demo, cladding_expose=true".into(),
        ));
        assert_eq!(
            string_labels.get("cladding").map(String::as_str),
            Some("demo")
        );
        assert_eq!(
            string_labels.get("project_root").map(String::as_str),
            Some("/tmp/demo")
        );

        let object_labels = parse_labels(&json!({
            "cladding": "demo",
            "project_root": "/tmp/demo",
            "cladding_expose": "true"
        }));
        assert_eq!(
            object_labels.get("cladding_expose").map(String::as_str),
            Some("true")
        );
    }

    #[test]
    fn parse_expose_proxy_items_filters_and_extracts_expected_fields() {
        let parsed = json!([
            {
                "Id": "abc123",
                "Names": ["demo-expose-3000-9000"],
                "Status": "Up 3 seconds",
                "Labels": {
                    "cladding": "demo",
                    "project_root": "/tmp/demo/.cladding",
                    "cladding_expose": "true",
                    "cladding_expose_target": "agent",
                    "cladding_expose_container_port": "3000",
                    "cladding_expose_host_port": "9000"
                }
            },
            {
                "Id": "skip-me",
                "Names": ["not-an-expose-proxy"],
                "Status": "Up 3 seconds",
                "Labels": {
                    "cladding": "demo"
                }
            }
        ]);

        let items = parse_expose_proxy_items(&parsed);
        assert_eq!(items.len(), 1);
        assert_eq!(items[0].proxy.id, "abc123");
        assert_eq!(items[0].proxy.name, "demo-expose-3000-9000");
        assert_eq!(items[0].proxy.container_port, 3000);
        assert_eq!(items[0].proxy.host_port, 9000);
        assert_eq!(items[0].project_root, "/tmp/demo/.cladding");
        assert_eq!(items[0].target, "agent");
    }

    #[test]
    fn parse_expose_proxy_item_accepts_string_names_and_state_fallback() {
        let parsed = json!({
            "ID": "xyz789",
            "Names": "demo-expose-4000-9100",
            "State": "running",
            "Labels": "cladding=demo,project_root=/tmp/demo/.cladding,cladding_expose=true,cladding_expose_target=agent,cladding_expose_container_port=4000,cladding_expose_host_port=9100"
        });

        let item = parse_expose_proxy_item(&parsed).expect("proxy item");
        assert_eq!(item.proxy.id, "xyz789");
        assert_eq!(item.proxy.name, "demo-expose-4000-9100");
        assert_eq!(item.proxy.status, "running");
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
    fn build_pod_create_command_includes_labels_network_and_aliases() {
        let pod = RuntimePod {
            name: "demo-agent".to_string(),
            labels: std::collections::BTreeMap::from([
                ("app".to_string(), "agent".to_string()),
                ("cladding".to_string(), "demo".to_string()),
                (
                    "project_root".to_string(),
                    "/tmp/demo/.cladding".to_string(),
                ),
            ]),
            network_name: "cladding-1".to_string(),
            ip: "10.90.1.5".to_string(),
            host_aliases: vec![RuntimeHostAlias {
                ip: "10.90.1.2".to_string(),
                hostnames: vec!["demo-proxy".to_string(), "proxy-alias".to_string()],
            }],
            init_tasks: Vec::new(),
            containers: Vec::new(),
            userns_keep_id: true,
        };

        let cmd = build_pod_create_command(&pod);
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
                "cladding-1",
                "--ip",
                "10.90.1.5",
                "--userns",
                "keep-id",
                "--add-host",
                "demo-proxy:10.90.1.2",
                "--add-host",
                "proxy-alias:10.90.1.2",
            ]
        );
    }

    #[test]
    fn build_container_create_command_includes_mounts_env_and_io_flags() {
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

        let cmd = build_container_create_command("demo-agent", &container);
        assert_eq!(
            command_args(&cmd),
            vec![
                "create",
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
    fn build_task_run_command_sets_root_user_and_capabilities() {
        let pod = RuntimePod {
            name: "demo-agent".to_string(),
            labels: std::collections::BTreeMap::new(),
            network_name: "cladding-1".to_string(),
            ip: "10.90.1.5".to_string(),
            host_aliases: Vec::new(),
            init_tasks: Vec::new(),
            containers: Vec::new(),
            userns_keep_id: false,
        };
        let task = RuntimeTask {
            name: "agent-node".to_string(),
            image: "alpine:latest".to_string(),
            command: vec![
                "/bin/sh".to_string(),
                "/opt/scripts/jail_agent.sh".to_string(),
            ],
            env: vec![RuntimeEnvVar {
                name: "CLADDING_PROXY_NAME".to_string(),
                value: "demo-proxy".to_string(),
            }],
            mounts: vec![RuntimeMount {
                mount_path: "/opt/scripts".to_string(),
                read_only: true,
                source: RuntimeMountSource::HostPath {
                    path: "/tmp/demo/scripts".into(),
                },
            }],
            run_as_user: Some(0),
            run_as_group: Some(0),
            added_capabilities: vec!["NET_ADMIN".to_string()],
        };

        let mut cmd = Command::new("podman");
        cmd.arg("run");
        cmd.arg("--rm");
        cmd.arg("--pod");
        cmd.arg(&pod.name);
        cmd.arg("--name");
        cmd.arg(format!("{}-{}", pod.name, task.name));
        cmd.arg("--user");
        cmd.arg("0:0");
        cmd.arg("--cap-add");
        cmd.arg("NET_ADMIN");
        append_env_args(&mut cmd, &task.env);
        append_mount_args(&mut cmd, &pod.name, &task.mounts);
        append_entrypoint_arg(&mut cmd, &task.command);
        cmd.arg(&task.image);
        append_command_args(&mut cmd, &task.command);

        assert_eq!(
            command_args(&cmd),
            vec![
                "run",
                "--rm",
                "--pod",
                "demo-agent",
                "--name",
                "demo-agent-agent-node",
                "--user",
                "0:0",
                "--cap-add",
                "NET_ADMIN",
                "--env",
                "CLADDING_PROXY_NAME=demo-proxy",
                "--volume",
                "/tmp/demo/scripts:/opt/scripts:ro",
                "--entrypoint",
                "/bin/sh",
                "alpine:latest",
                "/opt/scripts/jail_agent.sh",
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
