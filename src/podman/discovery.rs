use crate::error::{Error, Result};
use crate::fs_utils::command_exists_in_path;
use crate::runtime_metrics;
use anyhow::Context as _;
use serde_json::Value;
use std::collections::{HashMap, HashSet};
use std::env;
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
pub struct RunningProject {
    pub name: String,
    pub project_root: String,
    pub container_count: usize,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ContainerSnapshot {
    pub(crate) containers: Vec<ContainerSnapshotItem>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ContainerSnapshotItem {
    pub(crate) names: Vec<String>,
    pub(crate) state: String,
    pub(crate) labels: HashMap<String, String>,
}

impl ContainerSnapshot {
    pub(crate) fn named(&self, name: &str) -> Option<&ContainerSnapshotItem> {
        self.containers
            .iter()
            .find(|container| container.names.iter().any(|candidate| candidate == name))
    }

    pub fn running_projects(&self) -> Vec<RunningProject> {
        let mut projects: HashMap<(String, String), usize> = HashMap::new();
        for item in &self.containers {
            if !item.state.eq_ignore_ascii_case("running") {
                continue;
            }
            let Some(name) = item.labels.get("cladding") else {
                continue;
            };
            let Some(project_root) = item.labels.get("project_root") else {
                continue;
            };
            let key = (name.clone(), project_root.clone());
            let count = projects.entry(key).or_insert(0);
            *count += 1;
        }

        let mut results: Vec<RunningProject> = projects
            .into_iter()
            .map(|((name, project_root), container_count)| RunningProject {
                name,
                project_root,
                container_count,
            })
            .collect();

        results.sort_by(|a, b| {
            a.name
                .cmp(&b.name)
                .then_with(|| a.project_root.cmp(&b.project_root))
        });

        results
    }
}

pub fn list_running_projects(verbose: bool, quiet_helpers: bool) -> Result<Vec<RunningProject>> {
    Ok(list_container_snapshot(verbose, quiet_helpers)?.running_projects())
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
        containers.push(ContainerSnapshotItem {
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
        assert_eq!(
            snapshot.running_projects(),
            vec![RunningProject {
                name: "other".to_string(),
                project_root: "/tmp/other".to_string(),
                container_count: 1,
            }]
        );
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
