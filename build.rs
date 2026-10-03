use std::env;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

const BAFFLE_VERSION: &str = "1.0.0";
const BAFFLE_MIN_RUST_VERSION: (u32, u32) = (1, 96);

fn main() {
    let manifest_dir = PathBuf::from(env::var("CARGO_MANIFEST_DIR").unwrap());
    let workspace_root = manifest_dir.as_path();

    println!("cargo:rerun-if-changed=Cargo.toml");
    println!("cargo:rerun-if-changed=Cargo.lock");
    println!("cargo:rerun-if-changed=crates/mcp-run/Cargo.toml");
    println!("cargo:rerun-if-changed=crates/mcp-run/src");
    println!("cargo:rerun-if-env-changed=CLADDING_MCP_RUN_BIN");
    println!("cargo:rerun-if-env-changed=CLADDING_RUN_REMOTE_BIN");
    println!("cargo:rerun-if-env-changed=CLADDING_BAFFLE_BIN");

    let out_dir = PathBuf::from(env::var("OUT_DIR").unwrap());
    build_baffle(&out_dir);

    // Escape hatch: when prebuilt Linux helpers are provided (e.g. a macOS
    // release build that cannot run a Linux container), copy them straight in
    // and skip building mcp-run. Both must be set together.
    if let (Ok(mcp_run), Ok(run_remote)) = (
        env::var("CLADDING_MCP_RUN_BIN"),
        env::var("CLADDING_RUN_REMOTE_BIN"),
    ) {
        println!("cargo:warning=Using prebuilt embedded helpers; skipping nested release build");
        copy_bin(Path::new(&mcp_run), &out_dir.join("mcp-run"));
        copy_bin(Path::new(&run_remote), &out_dir.join("run-remote"));
        return;
    }

    let target_triple = env::var("TARGET").ok();
    let build_target = env::var("CARGO_BUILD_TARGET").ok();
    let effective_target = build_target.or(target_triple);

    let (target_dir, release_target) = if cfg!(target_os = "linux") {
        let target_dir = out_dir.join("mcp-run-target");
        build_locally(workspace_root, &target_dir, effective_target.as_deref());
        (target_dir, effective_target.as_deref())
    } else {
        let crate_dir = workspace_root.join("crates").join("mcp-run");
        let target_dir = crate_dir.join("target");
        build_with_podman(workspace_root);
        (target_dir, None)
    };

    let release_dir = release_dir(&target_dir, release_target);

    copy_bin(
        &release_dir.join(bin_name("mcp-run")),
        &out_dir.join("mcp-run"),
    );
    copy_bin(
        &release_dir.join(bin_name("run-remote")),
        &out_dir.join("run-remote"),
    );
}

fn build_baffle(out_dir: &Path) {
    let expected_arch = target_arch().unwrap_or_else(|err| panic!("{err}"));
    let out_path = out_dir.join("baffle");

    if let Some(prebuilt) = env::var_os("CLADDING_BAFFLE_BIN") {
        let prebuilt = PathBuf::from(prebuilt);
        let bytes = fs::read(&prebuilt).unwrap_or_else(|err| {
            panic!(
                "failed to read CLADDING_BAFFLE_BIN at {}: {err}",
                prebuilt.display()
            )
        });
        validate_baffle_elf(&bytes, expected_arch, &prebuilt).unwrap_or_else(|err| panic!("{err}"));
        copy_bin(&prebuilt, &out_path);
        println!(
            "cargo:warning=Using prebuilt Linux Baffle binary from {}",
            prebuilt.display()
        );
        return;
    }

    let host = env::var("HOST").unwrap_or_default();
    let target = env::var("TARGET").unwrap_or_default();
    let host_arch = host.split('-').next().unwrap_or_default();
    if !host.contains("-unknown-linux-gnu") || host_arch != expected_arch {
        panic!(
            "cannot build Linux Baffle for Cladding target {target} on host {host}; set CLADDING_BAFFLE_BIN to a compatible 64-bit Linux GNU executable (expected {expected_arch})"
        );
    }

    check_baffle_build_environment();

    let install_root = out_dir.join("baffle-install");
    let status = Command::new("cargo")
        .current_dir(env::var_os("CARGO_MANIFEST_DIR").unwrap())
        .env("CARGO_TARGET_DIR", out_dir.join("baffle-target"))
        .arg("install")
        .arg("baffle-proxy")
        .arg("--version")
        .arg(format!("={BAFFLE_VERSION}"))
        .arg("--locked")
        .arg("--force")
        .arg("--bin")
        .arg("baffle")
        .arg("--root")
        .arg(&install_root)
        .status()
        .expect("failed to run cargo install for baffle-proxy");
    if !status.success() {
        panic!(
            "cargo install for baffle-proxy {BAFFLE_VERSION} failed; install Rust 1.96 or newer, CMake, Clang and libclang, or set CLADDING_BAFFLE_BIN to a compatible prebuilt Linux binary"
        );
    }

    let installed = install_root.join("bin").join(bin_name("baffle"));
    let bytes = fs::read(&installed).unwrap_or_else(|err| {
        panic!(
            "cargo install completed but could not read {}: {err}",
            installed.display()
        )
    });
    validate_baffle_elf(&bytes, expected_arch, &installed).unwrap_or_else(|err| panic!("{err}"));
    copy_bin(&installed, &out_path);
}

fn target_arch() -> Result<&'static str, String> {
    let target = env::var("TARGET").unwrap_or_default();
    let arch = match target.split('-').next().unwrap_or_default() {
        "x86_64" => Ok("x86_64"),
        "aarch64" => Ok("aarch64"),
        arch => Err(format!(
            "unsupported Cladding target architecture '{arch}' for embedded Baffle; supported Linux proxy architectures are x86_64 and aarch64. Set CLADDING_BAFFLE_BIN to a compatible 64-bit Linux GNU executable."
        )),
    }?;

    if !target.contains("-unknown-linux-") && !target.ends_with("-apple-darwin") {
        return Err(format!(
            "unsupported Cladding target '{target}' for embedded Baffle; supported targets are x86_64 or aarch64 Linux and macOS. Set CLADDING_BAFFLE_BIN only for a supported target with a compatible Linux GNU executable."
        ));
    }
    Ok(arch)
}

fn check_baffle_build_environment() {
    let rustc = Command::new("rustc")
        .arg("--version")
        .output()
        .expect("failed to check rustc version for baffle-proxy");
    if !rustc.status.success() {
        panic!("failed to check rustc version for baffle-proxy");
    }
    let rustc_version = String::from_utf8_lossy(&rustc.stdout);
    let version = rustc_version
        .split_whitespace()
        .nth(1)
        .unwrap_or_default()
        .split('.')
        .take(2)
        .filter_map(|part| part.parse::<u32>().ok())
        .collect::<Vec<_>>();
    let found = match version.as_slice() {
        [major, minor] => (*major, *minor),
        _ => panic!("could not parse rustc version: {}", rustc_version.trim()),
    };
    if found < BAFFLE_MIN_RUST_VERSION {
        panic!(
            "baffle-proxy {BAFFLE_VERSION} requires Rust {}.{} or newer (found {}); upgrade Rust or set CLADDING_BAFFLE_BIN",
            BAFFLE_MIN_RUST_VERSION.0,
            BAFFLE_MIN_RUST_VERSION.1,
            rustc_version.trim()
        );
    }

    for program in ["cmake", "clang"] {
        let available = Command::new(program).arg("--version").output();
        if !available.is_ok_and(|output| output.status.success()) {
            panic!(
                "baffle-proxy {BAFFLE_VERSION} requires {program} and libclang to build; install the native build dependencies or set CLADDING_BAFFLE_BIN"
            );
        }
    }

    let libclang = Command::new("clang")
        .arg("-print-file-name=libclang.so")
        .output()
        .expect("failed to check for libclang");
    let libclang_path = String::from_utf8_lossy(&libclang.stdout).trim().to_string();
    if !libclang.status.success()
        || libclang_path == "libclang.so"
        || !Path::new(&libclang_path).exists()
    {
        panic!(
            "baffle-proxy {BAFFLE_VERSION} requires libclang development files; install libclang-dev or set CLADDING_BAFFLE_BIN"
        );
    }
}

fn validate_baffle_elf(bytes: &[u8], expected_arch: &str, path: &Path) -> Result<(), String> {
    if bytes.len() < 64
        || &bytes[..4] != b"\x7fELF"
        || bytes[4] != 2
        || bytes[5] != 1
        || !matches!(bytes[7], 0 | 3)
    {
        return Err(format!(
            "{} is not a 64-bit little-endian Linux ELF executable; CLADDING_BAFFLE_BIN must point to a compatible Linux GNU binary",
            path.display()
        ));
    }

    let machine = u16::from_le_bytes([bytes[18], bytes[19]]);
    let actual_arch = match machine {
        62 => "x86_64",
        183 => "aarch64",
        _ => "unsupported",
    };
    if actual_arch != expected_arch {
        return Err(format!(
            "{} contains a {actual_arch} Linux executable, but Cladding target requires {expected_arch}; supply a matching CLADDING_BAFFLE_BIN",
            path.display()
        ));
    }

    let program_offset = read_u64(bytes, 32)
        .ok_or_else(|| format!("{} has an invalid ELF program header", path.display()))?
        as usize;
    let program_entry_size = read_u16(bytes, 54).unwrap_or_default() as usize;
    let program_count = read_u16(bytes, 56).unwrap_or_default() as usize;
    let loader_suffix = match expected_arch {
        "x86_64" => "ld-linux-x86-64.so.2",
        "aarch64" => "ld-linux-aarch64.so.1",
        _ => unreachable!(),
    };
    let mut has_gnu_loader = false;
    for index in 0..program_count {
        let header = program_offset.saturating_add(index.saturating_mul(program_entry_size));
        if header.saturating_add(56) > bytes.len() || program_entry_size < 56 {
            break;
        }
        if read_u32(bytes, header) != Some(3) {
            continue;
        }
        let offset = read_u64(bytes, header + 8).unwrap_or(u64::MAX) as usize;
        let size = read_u64(bytes, header + 32).unwrap_or_default() as usize;
        let end = offset.saturating_add(size);
        if end > bytes.len() {
            break;
        }
        let interpreter = bytes[offset..end]
            .split(|byte| *byte == 0)
            .next()
            .unwrap_or_default();
        has_gnu_loader = String::from_utf8_lossy(interpreter).ends_with(loader_suffix);
        break;
    }
    if !has_gnu_loader {
        return Err(format!(
            "{} does not use the {loader_suffix} GNU libc loader required by the selected Linux proxy runtime",
            path.display()
        ));
    }
    Ok(())
}

fn read_u16(bytes: &[u8], offset: usize) -> Option<u16> {
    let value = bytes.get(offset..offset.checked_add(2)?)?;
    Some(u16::from_le_bytes([value[0], value[1]]))
}

fn read_u32(bytes: &[u8], offset: usize) -> Option<u32> {
    let value = bytes.get(offset..offset.checked_add(4)?)?;
    Some(u32::from_le_bytes([value[0], value[1], value[2], value[3]]))
}

fn read_u64(bytes: &[u8], offset: usize) -> Option<u64> {
    let value = bytes.get(offset..offset.checked_add(8)?)?;
    Some(u64::from_le_bytes([
        value[0], value[1], value[2], value[3], value[4], value[5], value[6], value[7],
    ]))
}

fn release_dir(target_dir: &Path, target: Option<&str>) -> PathBuf {
    match target {
        Some(target) => target_dir.join(target).join("release"),
        None => target_dir.join("release"),
    }
}

fn copy_bin(src: &Path, dst: &Path) {
    fs::copy(src, dst).unwrap_or_else(|err| {
        panic!(
            "failed to copy {} to {}: {err}",
            src.display(),
            dst.display()
        )
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

    let status = cargo
        .status()
        .expect("failed to run cargo build for mcp-run");
    if !status.success() {
        panic!("cargo build -p mcp-run failed");
    }
}

fn build_with_podman(workspace_root: &Path) {
    let status = Command::new("podman")
        .arg("run")
        .arg("--rm")
        .arg("-e")
        .arg("CARGO_TARGET_DIR=/work/crates/mcp-run/target")
        .arg("-v")
        .arg(format!("{}:/work", workspace_root.display()))
        .arg("-w")
        .arg("/work")
        .arg("docker.io/library/rust:latest")
        .arg("cargo")
        .arg("build")
        .arg("--manifest-path")
        .arg("/work/Cargo.toml")
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
