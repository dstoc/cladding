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
        if item.target != "cli-app" {
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

        let output = cmd
            .output()
            .with_context(|| "failed to run podman rm")?;

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

fn list_expose_proxy_items(project_name: &str, include_stopped: bool) -> Result<Vec<ExposeProxyItem>> {
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

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn parse_labels_supports_string_and_object_forms() {
        let string_labels = parse_labels(&Value::String(
            "cladding=demo, project_root=/tmp/demo, cladding_expose=true".into(),
        ));
        assert_eq!(string_labels.get("cladding").map(String::as_str), Some("demo"));
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
                    "cladding_expose_target": "cli-app",
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
        assert_eq!(items[0].target, "cli-app");
    }

    #[test]
    fn parse_expose_proxy_item_accepts_string_names_and_state_fallback() {
        let parsed = json!({
            "ID": "xyz789",
            "Names": "demo-expose-4000-9100",
            "State": "running",
            "Labels": "cladding=demo,project_root=/tmp/demo/.cladding,cladding_expose=true,cladding_expose_target=cli-app,cladding_expose_container_port=4000,cladding_expose_host_port=9100"
        });

        let item = parse_expose_proxy_item(&parsed).expect("proxy item");
        assert_eq!(item.proxy.id, "xyz789");
        assert_eq!(item.proxy.name, "demo-expose-4000-9100");
        assert_eq!(item.proxy.status, "running");
    }

    #[test]
    fn remove_output_is_missing_container_matches_expected_errors() {
        let output = Output {
            status: std::process::Command::new("true")
                .status()
                .expect("status"),
            stdout: Vec::new(),
            stderr: b"Error: no such container".to_vec(),
        };
        assert!(remove_output_is_missing_container(&output));
    }
}
