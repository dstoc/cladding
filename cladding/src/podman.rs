use crate::assets::containerfile;
use crate::error::{Error, Result};
use crate::network::{is_ipv4_cidr, NetworkSettings};
use anyhow::Context as _;
use serde_json::Value;
use std::env;
use std::collections::HashMap;
use std::process::{Command, ExitStatus, Output, Stdio};

pub fn podman_required(message: &str) -> Result<()> {
    if command_exists("podman") {
        Ok(())
    } else {
        eprintln!("missing: {message}");
        Err(Error::message("missing podman"))
    }
}

pub fn podman_network_exists(network_name: &str) -> Result<Option<bool>> {
    let status = Command::new("podman")
        .args(["network", "exists", network_name])
        .status()
        .with_context(|| "failed to check existing networks via podman")?;

    Ok(match status.code() {
        Some(0) => Some(true),
        Some(1) => Some(false),
        _ => None,
    })
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
                    "hint: update cladding.json subnet to match, or run 'podman network rm {}' and retry",
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

pub fn list_podman_ipv4_subnets() -> Result<Vec<String>> {
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
            .output();

        let output = match output {
            Ok(output) => output,
            Err(_) => continue,
        };

        if !output.status.success() {
            continue;
        }

        let stdout = String::from_utf8_lossy(&output.stdout);
        for line in stdout.lines().map(str::trim) {
            if is_ipv4_cidr(line) {
                subnets.push(line.to_string());
            }
        }
    }

    Ok(subnets)
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

pub fn list_running_projects() -> Result<Vec<RunningProject>> {
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

    let mut projects: HashMap<(String, String), usize> = HashMap::new();
    let Some(items) = parsed.as_array() else {
        return Ok(Vec::new());
    };

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
        let key = (name.clone(), project_root.clone());
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
