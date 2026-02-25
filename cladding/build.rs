use std::env;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

fn main() {
    let manifest_dir = PathBuf::from(env::var("CARGO_MANIFEST_DIR").unwrap());
    let workspace_root = manifest_dir.parent().unwrap();

    println!("cargo:rerun-if-changed=../crates/mcp-run/Cargo.toml");
    println!("cargo:rerun-if-changed=../crates/mcp-run/src");

    let target_dir = PathBuf::from(env::var("OUT_DIR").unwrap()).join("mcp-run-target");

    let target_triple = env::var("TARGET").ok();
    let build_target = env::var("CARGO_BUILD_TARGET").ok();
    let effective_target = build_target.or(target_triple);

    let mut cargo = Command::new("cargo");
    cargo.current_dir(workspace_root);
    cargo.arg("build").arg("-p").arg("mcp-run").arg("--release");
    cargo.arg("--bin").arg("mcp-run");
    cargo.arg("--bin").arg("run-remote");
    cargo.arg("--target-dir").arg(&target_dir);

    if let Some(target) = effective_target.as_deref() {
        cargo.arg("--target").arg(target);
    }

    let status = cargo.status().expect("failed to run cargo build for mcp-run");
    if !status.success() {
        panic!("cargo build -p mcp-run failed");
    }

    let out_dir = PathBuf::from(env::var("OUT_DIR").unwrap());
    let release_dir = release_dir(&target_dir, effective_target.as_deref());

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

fn bin_name(base: &str) -> String {
    if cfg!(windows) {
        format!("{base}.exe")
    } else {
        base.to_string()
    }
}
