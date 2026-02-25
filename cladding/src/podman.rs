use crate::assets::containerfile;
use crate::error::{Error, Result};
use crate::network::{is_ipv4_cidr, NetworkSettings};
use anyhow::Context as _;
use std::env;
use std::process::{Command, ExitStatus, Stdio};

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
                return ensure_success(output.status, "podman network inspect");
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

pub fn build_mcp_run(cladding_root: &std::path::Path) -> Result<()> {
    let status = Command::new("podman")
        .args([
            "run",
            "--rm",
            "-e",
            "CARGO_TARGET_DIR=/work/mcp-run/target",
            "-v",
            &format!(
                "{}:/work/mcp-run",
                cladding_root.join("crates/mcp-run").display()
            ),
            "-w",
            "/work/mcp-run",
            "docker.io/library/rust:latest",
            "cargo",
            "build",
            "--manifest-path",
            "/work/mcp-run/Cargo.toml",
            "--release",
            "--locked",
            "--bin",
            "mcp-run",
            "--bin",
            "run-remote",
        ])
        .status()
        .with_context(|| "failed to run podman for build")?;

    ensure_success(status, "podman run")
}

pub fn podman_build_image(
    cladding_root: &std::path::Path,
    image: &str,
    host_uid: u32,
    host_gid: u32,
) -> Result<()> {
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
        &cladding_root.display().to_string(),
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
        return ensure_success(output.status, "podman network ls").map(|_| Vec::new());
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

fn command_exists(command: &str) -> bool {
    env::var_os("PATH").map_or(false, |paths| {
        env::split_paths(&paths).any(|path| {
            let candidate = path.join(command);
            candidate.is_file()
        })
    })
}
