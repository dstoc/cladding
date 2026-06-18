use crate::error::{Error, Result};
use crate::fs_utils::is_executable;
use anyhow::Context as _;
use serde_json::Value;
use std::collections::HashMap;
use std::env;
use std::process::Command;

use super::command::{ensure_success_output, trace_command};

pub fn podman_required(message: &str) -> Result<()> {
    if command_exists("podman") {
        Ok(())
    } else {
        eprintln!("missing: {message}");
        Err(Error::message("missing podman"))
    }
}

pub fn runsc_available(verbose: bool) -> Result<bool> {
    if command_exists("runsc") {
        return Ok(true);
    }

    if !command_exists("podman") {
        return Ok(false);
    }

    let mut cmd = Command::new("podman");
    cmd.args(["info", "--format", "{{json .Host.OCIRuntimes}}"]);
    trace_command(&cmd, verbose);
    let output = cmd.output().with_context(|| "failed to run podman info")?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr).trim().to_string();
        let message = if stderr.is_empty() {
            "failed to inspect podman runtimes".to_string()
        } else {
            format!("failed to inspect podman runtimes: {stderr}")
        };
        return Err(Error::message(message));
    }

    let runtimes: Value = serde_json::from_slice(&output.stdout)
        .with_context(|| "failed to parse podman info output")?;
    Ok(podman_info_reports_runtime_named(&runtimes, "runsc"))
}

fn command_exists(command: &str) -> bool {
    command_exists_in_path(command, env::var_os("PATH").as_deref())
}

fn command_exists_in_path(command: &str, paths: Option<&std::ffi::OsStr>) -> bool {
    paths.is_some_and(|paths| {
        env::split_paths(paths).any(|path| {
            let candidate = path.join(command);
            is_executable(&candidate)
        })
    })
}

fn podman_info_reports_runtime_named(value: &Value, runtime_name: &str) -> bool {
    value
        .as_object()
        .is_some_and(|runtimes| runtimes.contains_key(runtime_name))
}

#[derive(Debug, Clone)]
pub struct RunningProject {
    pub name: String,
    pub project_root: String,
    pub pod_count: usize,
}

pub fn list_running_projects(verbose: bool) -> Result<Vec<RunningProject>> {
    let items = list_running_pod_items(verbose)?;
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

#[derive(Debug, Clone)]
struct RunningPodItem {
    name: String,
    project_root: String,
}

fn list_running_pod_items(verbose: bool) -> Result<Vec<RunningPodItem>> {
    let mut cmd = Command::new("podman");
    cmd.args([
        "pod",
        "ps",
        "--filter",
        "label=cladding",
        "--filter",
        "status=running",
        "--format",
        "json",
    ]);
    trace_command(&cmd, verbose);
    let output = cmd
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
        pods.push(RunningPodItem {
            name: name.to_string(),
            project_root: project_root.to_string(),
        });
    }

    Ok(pods)
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

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

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
    fn command_exists_in_path_detects_executable_entries() {
        let temp =
            std::env::temp_dir().join(format!("cladding-command-exists-{}", std::process::id()));
        if temp.exists() {
            std::fs::remove_dir_all(&temp).expect("cleanup temp dir");
        }
        std::fs::create_dir_all(&temp).expect("create temp dir");

        let command_path = temp.join("runsc");
        std::fs::write(&command_path, b"#!/bin/sh\nexit 0\n").expect("write command");
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mut permissions = std::fs::metadata(&command_path)
                .expect("metadata")
                .permissions();
            permissions.set_mode(0o755);
            std::fs::set_permissions(&command_path, permissions).expect("chmod");
        }

        assert!(command_exists_in_path("runsc", Some(temp.as_os_str())));
        assert!(!command_exists_in_path("podman", Some(temp.as_os_str())));

        std::fs::remove_dir_all(&temp).expect("cleanup temp dir");
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
