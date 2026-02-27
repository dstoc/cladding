use crate::assets::containerfile;
use crate::error::{Error, Result};
use crate::network::{is_ipv4_cidr, parse_cladding_pool_index, NetworkSettings};
use anyhow::Context as _;
use serde_json::Value;
use std::collections::HashMap;
use std::env;
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
            if !stdout.contains(&format!("\"subnet\": \"{}\"", network_settings.network_subnet))
            {
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
            if stdout.contains(&format!("\"subnet\": \"{}\"", network_settings.network_subnet)) {
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

    let status = child.wait().with_context(|| "failed to wait on podman build")?;

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
            return ensure_success_output(&output, "podman network inspect")
                .map(|_| Vec::new());
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

pub fn podman_play_kube(
    rendered: &str,
    network: &NetworkSettings,
    down: bool,
) -> Result<()> {
    let mut cmd = Command::new("podman");
    cmd.arg("play").arg("kube");
    if down {
        cmd.arg("--down");
    } else {
        cmd.args([
            "--network",
            &network.network,
            "--ip",
            &network.proxy_ip,
            "--ip",
            &network.sandbox_ip,
            "--ip",
            &network.cli_ip,
        ]);
    }
    cmd.arg("-");
    cmd.stdin(Stdio::piped());

    let mut child = cmd.spawn().with_context(|| "failed to run podman play kube")?;

    if let Some(mut stdin) = child.stdin.take() {
        use std::io::Write;
        stdin
            .write_all(rendered.as_bytes())
            .with_context(|| "failed to write pods.yaml to podman")?;
    }

    let status = child.wait().with_context(|| "failed to wait on podman play kube")?;

    ensure_success(status, "podman play kube")
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
    env::var_os("PATH").map_or(false, |paths| {
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

#[derive(Debug, Clone)]
struct RunningPodItem {
    pod_id: String,
    name: String,
    project_root: String,
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
