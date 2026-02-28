use std::env;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

fn main() {
    let manifest_dir = PathBuf::from(env::var("CARGO_MANIFEST_DIR").unwrap());
    let workspace_root = manifest_dir.parent().unwrap();

    println!("cargo:rerun-if-changed=../crates/mcp-run/Cargo.toml");
    println!("cargo:rerun-if-changed=../crates/mcp-run/src");

    let target_triple = env::var("TARGET").ok();
    let build_target = env::var("CARGO_BUILD_TARGET").ok();
    let effective_target = build_target.or(target_triple);

    let out_dir = PathBuf::from(env::var("OUT_DIR").unwrap());
    let (target_dir, release_target) = if cfg!(target_os = "linux") {
        let target_dir = out_dir.join("mcp-run-target");
        build_locally(workspace_root, &target_dir, effective_target.as_deref());
        (target_dir, effective_target.as_deref())
    } else {
        let crate_dir = workspace_root.join("crates").join("mcp-run");
        let target_dir = crate_dir.join("target");
        build_with_podman(&crate_dir);
        (target_dir, None)
    };

    let release_dir = release_dir(&target_dir, release_target);

    copy_bin(&release_dir.join(bin_name("mcp-run")), &out_dir.join("mcp-run"));
    copy_bin(
        &release_dir.join(bin_name("run-remote")),
        &out_dir.join("run-remote"),
    );
}

fn release_dir(target_dir: &Path, target: Option<&str>) -> PathBuf {
    match target {
        Some(target) => target_dir.join(target).join("release"),
        None => target_dir.join("release"),
    }
}

fn copy_bin(src: &Path, dst: &Path) {
    fs::copy(src, dst).unwrap_or_else(|err| {
        panic!("failed to copy {} to {}: {err}", src.display(), dst.display())
    });
}

fn build_locally(workspace_root: &Path, target_dir: &Path, target: Option<&str>) {
    let mut cargo = Command::new("cargo");
    cargo.current_dir(workspace_root);
    cargo.arg("build").arg("-p").arg("mcp-run").arg("--release");
    cargo.arg("--bin").arg("mcp-run");
    cargo.arg("--bin").arg("run-remote");
    cargo.arg("--target-dir").arg(target_dir);

    if let Some(target) = target {
        cargo.arg("--target").arg(target);
    }

    let status = cargo.status().expect("failed to run cargo build for mcp-run");
    if !status.success() {
        panic!("cargo build -p mcp-run failed");
    }
}

fn build_with_podman(crate_dir: &Path) {
    let status = Command::new("podman")
        .arg("run")
        .arg("--rm")
        .arg("-e")
        .arg("CARGO_TARGET_DIR=/work/mcp-run/target")
        .arg("-v")
        .arg(format!("{}:/work/mcp-run", crate_dir.display()))
        .arg("-w")
        .arg("/work/mcp-run")
        .arg("docker.io/library/rust:latest")
        .arg("cargo")
        .arg("build")
        .arg("--manifest-path")
        .arg("/work/mcp-run/Cargo.toml")
        .arg("--release")
        .arg("--locked")
        .arg("--bin")
        .arg("mcp-run")
        .arg("--bin")
        .arg("run-remote")
        .status()
        .expect("failed to run podman build for mcp-run");

    if !status.success() {
        panic!("podman cargo build for mcp-run failed");
    }
}

fn bin_name(base: &str) -> String {
    if cfg!(windows) {
        format!("{base}.exe")
    } else {
        base.to_string()
    }
}
