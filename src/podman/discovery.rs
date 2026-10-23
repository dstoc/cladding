use crate::error::{Error, Result};
use crate::fs_utils::command_exists_in_path;
use crate::runtime_metrics;
use anyhow::Context as _;
use serde_json::Value;
use std::collections::{HashMap, HashSet};
use std::env;
use std::path::Path;
use std::process::Command;

use super::command::{
    ensure_success_output, ensure_success_output_with_diagnostics, forward_output, trace_command,
};

pub fn podman_required(message: &str) -> Result<()> {
    if command_exists("podman") {
        Ok(())
    } else {
        eprintln!("missing: {message}");
        Err(Error::message("missing podman"))
    }
}

pub fn runsc_available(verbose: bool, quiet_helpers: bool) -> Result<bool> {
    if command_exists("runsc") {
        return Ok(true);
    }

    if !command_exists("podman") {
        return Ok(false);
    }

    let mut cmd = Command::new("podman");
    cmd.args(["info", "--format", "{{json .Host.OCIRuntimes}}"]);
    trace_command(&cmd, verbose);
    let output = runtime_metrics::output(&mut cmd).with_context(|| "failed to run podman info")?;

    if !output.status.success() {
        if quiet_helpers {
            ensure_success_output_with_diagnostics(&output, "podman info")?;
        }
        let stderr = String::from_utf8_lossy(&output.stderr).trim().to_string();
        let message = if stderr.is_empty() {
            "failed to inspect podman runtimes".to_string()
        } else {
            format!("failed to inspect podman runtimes: {stderr}")
        };
        return Err(Error::message(message));
    }

    if verbose {
        forward_output(&output)?;
    }

    let runtimes: Value = serde_json::from_slice(&output.stdout)
        .with_context(|| "failed to parse podman info output")?;
    Ok(podman_info_reports_runtime_named(&runtimes, "runsc"))
}

fn command_exists(command: &str) -> bool {
    command_exists_in_path(command, env::var_os("PATH").as_deref())
}

fn podman_info_reports_runtime_named(value: &Value, runtime_name: &str) -> bool {
    value
        .as_object()
        .is_some_and(|runtimes| runtimes.contains_key(runtime_name))
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ContainerSnapshot {
    pub(crate) containers: Vec<ContainerSnapshotItem>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ContainerSnapshotItem {
    pub(crate) id: Option<String>,
    pub(crate) names: Vec<String>,
    pub(crate) state: String,
    pub(crate) labels: HashMap<String, String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RunComponent {
    pub role: String,
    pub id: Option<String>,
    pub name: String,
    pub state: String,
    pub cwd_mappings: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DiscoveredRun {
    pub uuid: String,
    pub name: String,
    pub project_root: Option<String>,
    pub metadata_version: Option<String>,
    pub control_socket: Option<String>,
    pub expected_roles: Option<String>,
    pub components: Vec<RunComponent>,
    pub helper_containers: Vec<RunComponent>,
    pub volumes: Vec<String>,
    pub malformed: Option<String>,
}

impl ContainerSnapshot {
    pub(crate) fn named(&self, name: &str) -> Option<&ContainerSnapshotItem> {
        self.containers
            .iter()
            .find(|container| container.names.iter().any(|candidate| candidate == name))
    }

    pub fn runs(&self) -> Vec<DiscoveredRun> {
        let mut grouped = HashMap::<String, Vec<&ContainerSnapshotItem>>::new();
        for item in &self.containers {
            let Some(name) = item.labels.get("cladding") else {
                continue;
            };
            let Some(uuid) = run_uuid(name) else {
                continue;
            };
            grouped.entry(uuid).or_default().push(item);
        }

        let mut runs = grouped
            .into_iter()
            .map(|(uuid, items)| run_from_items(uuid, items))
            .collect::<Vec<_>>();
        runs.sort_by(|a, b| a.uuid.cmp(&b.uuid));
        runs
    }
}

pub fn select_run<'a>(runs: &'a [DiscoveredRun], selector: &str) -> Result<&'a DiscoveredRun> {
    if selector.is_empty() {
        return Err(Error::message("run selector cannot be empty"));
    }
    let normalized = normalize_run_selector(selector);
    let exact = runs
        .iter()
        .filter(|run| {
            normalize_run_selector(&run.uuid) == normalized
                || normalize_run_selector(&run.name) == normalized
        })
        .collect::<Vec<_>>();
    let matches = if exact.is_empty() {
        runs.iter()
            .filter(|run| {
                normalize_run_selector(&run.uuid).contains(&normalized)
                    || normalize_run_selector(&run.name).contains(&normalized)
            })
            .collect::<Vec<_>>()
    } else {
        exact
    };
    match matches.as_slice() {
        [run] => Ok(*run),
        [] => Err(Error::message(format!(
            "no run matches selector '{selector}'"
        ))),
        _ => {
            let ids = matches
                .iter()
                .map(|run| run.uuid.as_str())
                .collect::<Vec<_>>()
                .join(", ");
            Err(Error::message(format!(
                "run selector '{selector}' is ambiguous; matching UUIDs: {ids}"
            )))
        }
    }
}

fn normalize_run_selector(value: &str) -> String {
    value.to_ascii_lowercase()
}

fn run_uuid(name: &str) -> Option<String> {
    let uuid = name.strip_prefix("cladding-run-")?;
    let bytes = uuid.as_bytes();
    if bytes.len() != 36
        || ![8, 13, 18, 23]
            .iter()
            .all(|position| bytes[*position] == b'-')
        || !bytes
            .iter()
            .enumerate()
            .all(|(index, byte)| [8, 13, 18, 23].contains(&index) || byte.is_ascii_hexdigit())
    {
        return None;
    }
    Some(uuid.to_ascii_lowercase())
}

fn run_from_items(uuid: String, items: Vec<&ContainerSnapshotItem>) -> DiscoveredRun {
    let name = format!("cladding-run-{uuid}");
    let mut roots = HashSet::new();
    let mut versions = HashSet::new();
    let mut sockets = HashSet::new();
    let mut expected_roles = HashSet::new();
    let mut roles = HashSet::new();
    let mut ids = HashSet::new();
    let mut components = Vec::with_capacity(items.len());
    let mut helper_containers = Vec::new();
    let mut malformed = None;
    for item in items {
        for label in [
            "project_root",
            "cladding_run_metadata_version",
            "cladding_run_control_socket",
            "cladding_run_expected_roles",
        ] {
            if !item.labels.contains_key(label) {
                malformed.get_or_insert_with(|| {
                    format!("run resource is missing required identity label '{label}'")
                });
            }
        }
        if let Some(root) = item.labels.get("project_root") {
            roots.insert(root.clone());
        }
        if let Some(version) = item.labels.get("cladding_run_metadata_version") {
            versions.insert(version.clone());
        }
        if let Some(socket) = item.labels.get("cladding_run_control_socket") {
            sockets.insert(socket.clone());
        }
        if let Some(roles) = item.labels.get("cladding_run_expected_roles") {
            expected_roles.insert(roles.clone());
        }
        if let Some(id) = &item.id
            && !ids.insert(id)
        {
            malformed.get_or_insert_with(|| {
                format!("run inventory contains duplicate container ID '{id}'")
            });
        }
        let resource = item.labels.get("cladding_resource").map(String::as_str);
        let app = item.labels.get("app").map(String::as_str);
        let helper_role = match (resource, app) {
            (Some("managed-mount-seed-helper"), _) => Some("managed-mount-seed-helper"),
            (_, Some("baffle-socket-volume-permissions")) => {
                Some("baffle-socket-volume-permissions")
            }
            _ => None,
        };
        if let Some(role) = helper_role {
            if item.id.is_none() {
                malformed.get_or_insert_with(|| {
                    format!("run helper '{role}' is missing its immutable container ID")
                });
            }
            let helper_name = item.names.first().map(String::as_str).unwrap_or_default();
            let expected_name = match role {
                "baffle-socket-volume-permissions" => {
                    format!("{}-baffle-socket-volume-permissions", name)
                }
                "managed-mount-seed-helper" => item
                    .labels
                    .get("cladding_mount_volume")
                    .map(|volume| format!("{volume}-seed"))
                    .unwrap_or_default(),
                _ => unreachable!("all helper roles are matched above"),
            };
            if helper_name != expected_name {
                malformed.get_or_insert_with(|| {
                    format!("run helper '{role}' has an unexpected container name")
                });
            }
            helper_containers.push(RunComponent {
                role: role.to_string(),
                id: item.id.clone(),
                name: helper_name.to_string(),
                state: item.state.to_ascii_lowercase(),
                cwd_mappings: None,
            });
            continue;
        }
        let Some(role) = item.labels.get("app") else {
            malformed
                .get_or_insert_with(|| "run component is missing its app role label".to_string());
            continue;
        };
        if !["agent", "proxy", "nw-sandbox", "fs-sandbox"].contains(&role.as_str()) {
            malformed.get_or_insert_with(|| format!("unknown component role '{role}'"));
        }
        if item.id.is_none() {
            malformed.get_or_insert_with(|| {
                "run component is missing its immutable container ID".to_string()
            });
        }
        let expected_name = format!("{name}-{role}-instance");
        if !item
            .names
            .iter()
            .any(|candidate| candidate == &expected_name)
        {
            malformed.get_or_insert_with(|| {
                format!("component role '{role}' has an unexpected container name")
            });
        }
        let cwd_mappings = item.labels.get("cladding_run_cwd_v1").cloned();
        if !cwd_mappings.as_deref().is_some_and(valid_cwd_mappings) {
            malformed.get_or_insert_with(|| {
                format!("component role '{role}' has missing or malformed CWD mapping metadata")
            });
        }
        if !roles.insert(role.clone()) {
            malformed.get_or_insert_with(|| format!("duplicate component role '{role}'"));
        }
        components.push(RunComponent {
            role: role.clone(),
            id: item.id.clone(),
            name: item.names.first().cloned().unwrap_or_default(),
            state: item.state.to_ascii_lowercase(),
            cwd_mappings,
        });
    }
    if roots.len() > 1 {
        malformed.get_or_insert_with(|| {
            "run resources have conflicting project_root labels".to_string()
        });
    }
    if versions.len() > 1 || sockets.len() > 1 || expected_roles.len() > 1 {
        malformed.get_or_insert_with(|| {
            "run resources have conflicting supervisor metadata".to_string()
        });
    }
    if versions.iter().any(|version| version != "1") {
        malformed.get_or_insert_with(|| "run metadata version is unsupported".to_string());
    }
    if !expected_roles
        .iter()
        .all(|roles| valid_expected_roles(roles))
    {
        malformed.get_or_insert_with(|| "run expected component metadata is malformed".to_string());
    }
    if let Some(expected) = expected_roles.iter().next() {
        let expected = expected.split(',').collect::<HashSet<_>>();
        if let Some(extra) = roles.iter().find(|role| !expected.contains(role.as_str())) {
            malformed.get_or_insert_with(|| {
                format!("observed component role '{extra}' is not enabled for this run")
            });
        }
    }
    if sockets
        .iter()
        .any(|socket| !std::path::Path::new(socket).is_absolute())
    {
        malformed.get_or_insert_with(|| {
            "run supervisor endpoint is not an absolute host path".to_string()
        });
    }
    if roots.len() != 1 {
        malformed.get_or_insert_with(|| {
            "run resources are missing a single project_root identity".to_string()
        });
    }
    if roots.iter().any(|root| {
        !Path::new(root).is_absolute()
            || Path::new(root)
                .file_name()
                .is_none_or(|name| name != ".cladding")
    }) {
        malformed.get_or_insert_with(|| {
            "project identity is not an absolute .cladding path".to_string()
        });
    }
    if versions.len() != 1 || sockets.len() != 1 || expected_roles.len() != 1 {
        malformed
            .get_or_insert_with(|| "run resources are missing supervisor metadata".to_string());
    }
    DiscoveredRun {
        uuid,
        name,
        project_root: roots.into_iter().next(),
        metadata_version: versions.into_iter().next(),
        control_socket: sockets.into_iter().next(),
        expected_roles: expected_roles.into_iter().next(),
        components,
        helper_containers,
        volumes: Vec::new(),
        malformed,
    }
}

pub fn list_container_snapshot(verbose: bool, quiet_helpers: bool) -> Result<ContainerSnapshot> {
    let mut cmd = build_container_snapshot_command();
    trace_command(&cmd, verbose);
    let output =
        runtime_metrics::output(&mut cmd).with_context(|| "failed to run podman container ps")?;

    if !output.status.success() {
        if quiet_helpers {
            ensure_success_output_with_diagnostics(&output, "podman container ps")?;
        }
        return ensure_success_output(&output, "podman container ps").map(|_| ContainerSnapshot {
            containers: Vec::new(),
        });
    }

    if verbose {
        forward_output(&output)?;
    }

    let stdout = String::from_utf8_lossy(&output.stdout);
    parse_container_snapshot(&stdout)
}

#[derive(Debug, Clone)]
struct VolumeSnapshotItem {
    name: String,
    labels: HashMap<String, String>,
}

pub fn list_discovered_runs(verbose: bool, quiet_helpers: bool) -> Result<Vec<DiscoveredRun>> {
    let mut runs = list_container_snapshot(verbose, quiet_helpers)?.runs();
    let mut command = Command::new("podman");
    command.args(["volume", "ls", "--format", "json"]);
    trace_command(&command, verbose);
    let output =
        runtime_metrics::output(&mut command).with_context(|| "failed to run podman volume ls")?;
    if !output.status.success() {
        if quiet_helpers {
            ensure_success_output_with_diagnostics(&output, "podman volume ls")?;
        }
        ensure_success_output(&output, "podman volume ls")?;
    }
    if verbose {
        forward_output(&output)?;
    }
    let volumes = parse_volume_snapshot(&String::from_utf8_lossy(&output.stdout))?;
    let mut by_uuid = HashMap::<String, Vec<VolumeSnapshotItem>>::new();
    for volume in volumes {
        let Some(name) = volume.labels.get("cladding") else {
            continue;
        };
        let Some(uuid) = run_uuid(name) else {
            continue;
        };
        if !matches!(
            volume.labels.get("cladding_resource").map(String::as_str),
            Some("inter-container-socket" | "managed-mount")
        ) {
            continue;
        }
        by_uuid.entry(uuid).or_default().push(volume);
    }
    for (uuid, run_volumes) in by_uuid {
        if let Some(run) = runs.iter_mut().find(|run| run.uuid == uuid) {
            merge_run_volumes(run, run_volumes);
        } else {
            runs.push(volume_only_run(uuid, run_volumes));
        }
    }
    runs.sort_by(|a, b| a.uuid.cmp(&b.uuid));
    Ok(runs)
}

fn parse_volume_snapshot(output: &str) -> Result<Vec<VolumeSnapshotItem>> {
    let parsed: Value = serde_json::from_str(output)
        .with_context(|| "failed to parse podman volume ls json output")?;
    let items = parsed.as_array().ok_or_else(|| {
        Error::message("failed to parse podman volume ls json output: expected an array")
    })?;
    items
        .iter()
        .enumerate()
        .map(|(index, item)| {
            let object = item
                .as_object()
                .ok_or_else(|| Error::message(format!("invalid volume list item {index}")))?;
            let name = object
                .get("Name")
                .or_else(|| object.get("name"))
                .and_then(Value::as_str)
                .filter(|name| !name.is_empty())
                .ok_or_else(|| Error::message(format!("volume list item {index} has no name")))?
                .to_string();
            let labels = object.get("Labels").map(parse_labels).unwrap_or_default();
            Ok(VolumeSnapshotItem { name, labels })
        })
        .collect()
}

fn merge_run_volumes(run: &mut DiscoveredRun, volumes: Vec<VolumeSnapshotItem>) {
    for volume in volumes {
        if volume.labels.get("project_root") != run.project_root.as_ref()
            || volume.labels.get("cladding_run_metadata_version") != run.metadata_version.as_ref()
            || volume.labels.get("cladding_run_control_socket") != run.control_socket.as_ref()
            || volume.labels.get("cladding_run_expected_roles") != run.expected_roles.as_ref()
        {
            run.malformed.get_or_insert_with(|| {
                "run-owned volumes conflict with component identity metadata".to_string()
            });
        }
        let volume_identity_matches =
            match volume.labels.get("cladding_resource").map(String::as_str) {
                Some("inter-container-socket") => {
                    volume.labels.get("cladding_socket_volume") == Some(&volume.name)
                }
                Some("managed-mount") => {
                    volume.labels.get("cladding_mount_volume") == Some(&volume.name)
                }
                _ => false,
            };
        if !volume_identity_matches {
            run.malformed.get_or_insert_with(|| {
                format!(
                    "run-owned volume '{}' has malformed ownership metadata",
                    volume.name
                )
            });
        }
        if !run.volumes.contains(&volume.name) {
            run.volumes.push(volume.name);
        }
    }
    run.volumes.sort();
}

fn volume_only_run(uuid: String, volumes: Vec<VolumeSnapshotItem>) -> DiscoveredRun {
    let name = format!("cladding-run-{uuid}");
    let mut roots = HashSet::new();
    let mut versions = HashSet::new();
    let mut sockets = HashSet::new();
    let mut expected_roles = HashSet::new();
    let mut malformed = None;
    for volume in &volumes {
        for label in [
            "project_root",
            "cladding_run_metadata_version",
            "cladding_run_control_socket",
            "cladding_run_expected_roles",
        ] {
            if !volume.labels.contains_key(label) {
                malformed.get_or_insert_with(|| {
                    format!("run-owned volume is missing required identity label '{label}'")
                });
            }
        }
        if let Some(value) = volume.labels.get("project_root") {
            roots.insert(value.clone());
        }
        if let Some(value) = volume.labels.get("cladding_run_metadata_version") {
            versions.insert(value.clone());
        }
        if let Some(value) = volume.labels.get("cladding_run_control_socket") {
            sockets.insert(value.clone());
        }
        if let Some(value) = volume.labels.get("cladding_run_expected_roles") {
            expected_roles.insert(value.clone());
        }
    }
    if roots.len() != 1 || versions.len() != 1 || sockets.len() != 1 || expected_roles.len() != 1 {
        malformed.get_or_insert_with(|| {
            "owned volumes have incomplete or conflicting run identity metadata".to_string()
        });
    }
    if versions.iter().any(|version| version != "1") {
        malformed.get_or_insert_with(|| "run metadata version is unsupported".to_string());
    }
    if !expected_roles
        .iter()
        .all(|roles| valid_expected_roles(roles))
    {
        malformed.get_or_insert_with(|| "run expected component metadata is malformed".to_string());
    }
    if sockets
        .iter()
        .any(|socket| !std::path::Path::new(socket).is_absolute())
    {
        malformed.get_or_insert_with(|| {
            "run supervisor endpoint is not an absolute host path".to_string()
        });
    }
    if roots.iter().any(|root| {
        !Path::new(root).is_absolute()
            || Path::new(root)
                .file_name()
                .is_none_or(|name| name != ".cladding")
    }) {
        malformed.get_or_insert_with(|| {
            "project identity is not an absolute .cladding path".to_string()
        });
    }
    for volume in &volumes {
        let volume_identity_matches =
            match volume.labels.get("cladding_resource").map(String::as_str) {
                Some("inter-container-socket") => {
                    volume.labels.get("cladding_socket_volume") == Some(&volume.name)
                }
                Some("managed-mount") => {
                    volume.labels.get("cladding_mount_volume") == Some(&volume.name)
                }
                _ => false,
            };
        if !volume_identity_matches {
            malformed.get_or_insert_with(|| {
                format!(
                    "run-owned volume '{}' has malformed ownership metadata",
                    volume.name
                )
            });
        }
    }
    let mut volume_names = volumes
        .into_iter()
        .map(|volume| volume.name)
        .collect::<Vec<_>>();
    volume_names.sort();
    DiscoveredRun {
        uuid,
        name,
        project_root: roots.into_iter().next(),
        metadata_version: versions.into_iter().next(),
        control_socket: sockets.into_iter().next(),
        expected_roles: expected_roles.into_iter().next(),
        components: Vec::new(),
        helper_containers: Vec::new(),
        volumes: volume_names,
        malformed,
    }
}

fn valid_expected_roles(value: &str) -> bool {
    let values = value.split(',').collect::<Vec<_>>();
    let allowed = ["agent", "proxy", "nw-sandbox", "fs-sandbox"];
    let unique = values.iter().copied().collect::<HashSet<_>>();
    values.len() == unique.len()
        && values.contains(&"agent")
        && values.contains(&"proxy")
        && values
            .iter()
            .all(|role| allowed.contains(role) && !role.is_empty())
}

fn valid_cwd_mappings(raw: &str) -> bool {
    let Ok(value) = serde_json::from_str::<Value>(raw) else {
        return false;
    };
    let Some(mappings) = value.as_array() else {
        return false;
    };
    let mut destinations = HashSet::new();
    mappings.iter().all(|mapping| {
        let Some(object) = mapping.as_object() else {
            return false;
        };
        let host_source = object
            .get("host_source")
            .and_then(Value::as_str)
            .map(std::path::Path::new);
        let destination = object
            .get("destination")
            .and_then(Value::as_str)
            .map(std::path::Path::new);
        let mount_type = object.get("mount_type").and_then(Value::as_str);
        let read_only = object.get("read_only").and_then(Value::as_bool);
        let volume_name = object.get("volume_name");
        let valid_path = |path: Option<&Path>| {
            path.is_some_and(|path| {
                path.is_absolute()
                    && path.components().all(|component| {
                        matches!(
                            component,
                            std::path::Component::RootDir | std::path::Component::Normal(_)
                        )
                    })
            })
        };
        let unique_destination = destination.is_some_and(|path| destinations.insert(path));
        object.len() == 5
            && valid_path(host_source)
            && valid_path(destination)
            && unique_destination
            && matches!(mount_type, Some("bind" | "readonly" | "copy"))
            && read_only.is_some()
            && (mount_type != Some("readonly") || read_only == Some(true))
            && match (mount_type, volume_name) {
                (Some("copy"), Some(Value::String(name))) => !name.is_empty(),
                (Some("bind" | "readonly"), Some(Value::Null)) => true,
                _ => false,
            }
    })
}

fn build_container_snapshot_command() -> Command {
    let mut cmd = Command::new("podman");
    cmd.args(["container", "ps", "--all", "--format", "json"]);
    cmd
}

fn parse_container_snapshot(output: &str) -> Result<ContainerSnapshot> {
    let parsed: Value = serde_json::from_str(output)
        .with_context(|| "failed to parse podman container ps json output")?;
    let items = parsed.as_array().ok_or_else(|| {
        Error::message("failed to parse podman container ps json output: expected an array")
    })?;

    let mut containers = Vec::with_capacity(items.len());
    let mut seen_names = HashSet::new();
    for (index, item) in items.iter().enumerate() {
        let item = item.as_object().ok_or_else(|| {
            Error::message(format!(
                "failed to parse podman container ps json output: item {index} is not an object"
            ))
        })?;
        let names = item
            .get("Names")
            .and_then(Value::as_array)
            .and_then(|values| {
                values
                    .iter()
                    .map(Value::as_str)
                    .collect::<Option<Vec<_>>>()
            })
            .filter(|names| !names.is_empty())
            .ok_or_else(|| {
                Error::message(format!(
                    "failed to parse podman container ps json output: item {index} has invalid Names"
                ))
            })?
            .into_iter()
            .map(str::to_string)
            .collect::<Vec<_>>();
        let state = item
            .get("State")
            .and_then(Value::as_str)
            .filter(|state| !state.is_empty())
            .ok_or_else(|| {
                Error::message(format!(
                    "failed to parse podman container ps json output: item {index} has invalid State"
                ))
            })?
            .to_string();

        for name in &names {
            if !seen_names.insert(name.clone()) {
                return Err(Error::message(format!(
                    "failed to parse podman container ps json output: duplicate container name {name:?}"
                )));
            }
        }

        let labels = item.get("Labels").map(parse_labels).unwrap_or_default();
        let id = item
            .get("Id")
            .or_else(|| item.get("ID"))
            .and_then(Value::as_str)
            .filter(|id| !id.is_empty())
            .map(str::to_string);
        containers.push(ContainerSnapshotItem {
            id,
            names,
            state,
            labels,
        });
    }

    Ok(ContainerSnapshot { containers })
}

pub fn podman_container_exists(container_name: &str) -> Result<bool> {
    let mut cmd = Command::new("podman");
    cmd.args(["container", "exists", container_name]);
    let status = runtime_metrics::status(&mut cmd)
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

fn parse_labels(value: &Value) -> HashMap<String, String> {
    let mut labels = HashMap::new();
    if let Value::Object(map) = value {
        for (key, val) in map {
            if let Some(s) = val.as_str() {
                labels.insert(key.clone(), s.to_string());
            }
        }
    }
    labels
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn parse_labels_accepts_json_objects_only() {
        let object_labels = parse_labels(&json!({
            "cladding": "demo",
            "project_root": "/tmp/demo",
            "cladding_expose": "true"
        }));
        assert_eq!(
            object_labels.get("cladding_expose").map(String::as_str),
            Some("true")
        );
        assert!(
            parse_labels(&Value::String(
                "cladding=demo,project_root=/tmp/demo".into()
            ))
            .is_empty()
        );
    }

    #[test]
    fn container_snapshot_keeps_stopped_and_unowned_name_collisions() {
        let snapshot = parse_container_snapshot(
            r#"[
                {"Names":["demo-agent-instance"],"State":"exited","Labels":{"cladding":"demo","project_root":"/tmp/demo"}},
                {"Names":["demo-proxy-instance"],"State":"running","Labels":{}},
                {"Names":["other-agent-instance"],"State":"running","Labels":{"cladding":"other","project_root":"/tmp/other"}}
            ]"#,
        )
        .expect("valid container snapshot");

        assert_eq!(
            snapshot.named("demo-agent-instance").unwrap().state,
            "exited"
        );
        assert!(
            snapshot
                .named("demo-proxy-instance")
                .unwrap()
                .labels
                .is_empty()
        );
        assert!(snapshot.named("missing-instance").is_none());
    }

    #[test]
    fn container_snapshot_rejects_malformed_inventory_output() {
        for output in [
            "{}",
            r#"[{"Names":[],"State":"running","Labels":{}}]"#,
            r#"[{"Names":["demo-instance"],"Labels":{}}]"#,
            r#"[{"Names":["demo-instance"],"State":"running","Labels":{}},{"Names":["demo-instance"],"State":"exited","Labels":{}}]"#,
        ] {
            assert!(
                parse_container_snapshot(output).is_err(),
                "accepted {output}"
            );
        }
    }

    #[test]
    fn container_snapshot_command_lists_all_names_and_states() {
        let args = build_container_snapshot_command()
            .get_args()
            .map(|argument| argument.to_string_lossy().into_owned())
            .collect::<Vec<_>>();
        assert_eq!(args, ["container", "ps", "--all", "--format", "json"]);
    }

    fn run_snapshot(uuid: &str, role: &str, id: &str, state: &str) -> ContainerSnapshot {
        parse_container_snapshot(&format!(
            r#"[{{"Id":"{id}","Names":["cladding-run-{uuid}-{role}-instance"],"State":"{state}","Labels":{{"cladding":"cladding-run-{uuid}","project_root":"/tmp/project/.cladding","app":"{role}","cladding_run_metadata_version":"1","cladding_run_control_socket":"/tmp/cladding-{uuid}.sock","cladding_run_expected_roles":"agent,proxy","cladding_run_cwd_v1":"[]"}}}}]"#
        ))
        .unwrap()
    }

    #[test]
    fn run_discovery_keeps_immutable_ids_and_resolves_exact_then_unique_substrings() {
        let first = run_snapshot(
            "aabbccdd-1111-4222-8333-123456789abc",
            "agent",
            "id-one",
            "running",
        );
        let second = run_snapshot(
            "aabbccdd-2222-4222-8333-123456789abc",
            "agent",
            "id-two",
            "exited",
        );
        let mut combined = first.runs();
        combined.extend(second.runs());

        assert_eq!(
            select_run(
                &combined,
                "cladding-run-AABBCCDD-1111-4222-8333-123456789ABC"
            )
            .unwrap()
            .uuid,
            "aabbccdd-1111-4222-8333-123456789abc"
        );
        assert_eq!(
            select_run(&combined, "2222-4222").unwrap().components[0]
                .id
                .as_deref(),
            Some("id-two")
        );
        assert!(
            select_run(&combined, "aabbccdd")
                .unwrap_err()
                .to_string()
                .contains("ambiguous")
        );
        assert!(select_run(&combined, "missing").is_err());
    }

    #[test]
    fn run_discovery_rejects_duplicate_roles_and_incompatible_metadata() {
        let uuid = "aabbccdd-1111-4222-8333-123456789abc";
        let make_item = |id: &str, state: &str| ContainerSnapshotItem {
            id: Some(id.to_string()),
            names: vec![format!("cladding-run-{uuid}-agent-instance")],
            state: state.to_string(),
            labels: HashMap::from([
                ("cladding".to_string(), format!("cladding-run-{uuid}")),
                ("project_root".to_string(), "/tmp/p/.cladding".to_string()),
                ("app".to_string(), "agent".to_string()),
                ("cladding_run_metadata_version".to_string(), "1".to_string()),
                (
                    "cladding_run_control_socket".to_string(),
                    "/tmp/run.sock".to_string(),
                ),
                (
                    "cladding_run_expected_roles".to_string(),
                    "agent,proxy".to_string(),
                ),
                ("cladding_run_cwd_v1".to_string(), "[]".to_string()),
            ]),
        };
        let first = make_item("one", "running");
        let second = make_item("two", "exited");
        let run = run_from_items(uuid.to_string(), vec![&first, &second]);
        assert!(
            run.malformed
                .as_deref()
                .unwrap()
                .contains("duplicate component role")
        );
    }

    #[test]
    fn run_discovery_keeps_seed_helpers_separate_from_component_roles() {
        let uuid = "aabbccdd-1111-4222-8333-123456789abc";
        let snapshot = parse_container_snapshot(&format!(
            r#"[
                {{"Id":"agent-id","Names":["cladding-run-{uuid}-agent-instance"],"State":"running","Labels":{{"cladding":"cladding-run-{uuid}","project_root":"/tmp/p/.cladding","app":"agent","cladding_run_metadata_version":"1","cladding_run_control_socket":"/tmp/run.sock","cladding_run_expected_roles":"agent,proxy","cladding_run_cwd_v1":"[]"}}}},
                {{"Id":"seed-id","Names":["cladding-mount-test-seed"],"State":"exited","Labels":{{"cladding":"cladding-run-{uuid}","project_root":"/tmp/p/.cladding","cladding_resource":"managed-mount-seed-helper","cladding_mount_volume":"cladding-mount-test","cladding_run_metadata_version":"1","cladding_run_control_socket":"/tmp/run.sock","cladding_run_expected_roles":"agent,proxy"}}}}
            ]"#
        ))
        .unwrap();
        let run = &snapshot.runs()[0];
        assert!(run.malformed.is_none());
        assert_eq!(run.components.len(), 1);
        assert_eq!(run.helper_containers.len(), 1);
        assert_eq!(run.helper_containers[0].id.as_deref(), Some("seed-id"));
    }

    #[test]
    fn volume_only_run_retains_owned_volume_identity() {
        let uuid = "aabbccdd-1111-4222-8333-123456789abc";
        let metadata = VolumeSnapshotItem {
            name: "run-mount-volume".to_string(),
            labels: HashMap::from([
                ("cladding".to_string(), format!("cladding-run-{uuid}")),
                ("project_root".to_string(), "/tmp/p/.cladding".to_string()),
                ("cladding_resource".to_string(), "managed-mount".to_string()),
                (
                    "cladding_mount_volume".to_string(),
                    "run-mount-volume".to_string(),
                ),
                ("cladding_run_metadata_version".to_string(), "1".to_string()),
                (
                    "cladding_run_control_socket".to_string(),
                    "/tmp/run.sock".to_string(),
                ),
                (
                    "cladding_run_expected_roles".to_string(),
                    "agent,proxy".to_string(),
                ),
            ]),
        };
        let run = volume_only_run(uuid.to_string(), vec![metadata]);
        assert!(run.malformed.is_none());
        assert!(run.components.is_empty());
        assert_eq!(run.volumes, ["run-mount-volume"]);
    }

    #[test]
    fn podman_info_reports_runtime_named_detects_configured_runtime() {
        let runtimes = json!({
            "crun": { "path": "/usr/bin/crun" },
            "runsc": { "path": "/usr/bin/runsc" }
        });

        assert!(podman_info_reports_runtime_named(&runtimes, "runsc"));
        assert!(!podman_info_reports_runtime_named(&runtimes, "runc"));
    }
}
