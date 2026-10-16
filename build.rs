use std::env;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

const BAFFLE_VERSION: &str = "1.1.1";

fn main() {
    ensure_supported_platforms();

    let manifest_dir = PathBuf::from(env::var("CARGO_MANIFEST_DIR").unwrap());
    let workspace_root = manifest_dir.as_path();

    println!("cargo:rerun-if-changed=Cargo.toml");
    println!("cargo:rerun-if-changed=Cargo.lock");
    println!("cargo:rerun-if-changed=crates/mcp-run/Cargo.toml");
    println!("cargo:rerun-if-changed=crates/mcp-run/src");
    println!("cargo:rerun-if-changed=crates/cladding-bridge/Cargo.toml");
    println!("cargo:rerun-if-changed=crates/cladding-bridge/src");
    println!("cargo:rerun-if-env-changed=CLADDING_MCP_RUN_BIN");
    println!("cargo:rerun-if-env-changed=CLADDING_RUN_REMOTE_BIN");
    println!("cargo:rerun-if-env-changed=CLADDING_BRIDGE_BIN");
    println!("cargo:rerun-if-env-changed=CLADDING_BAFFLE_BIN");

    let out_dir = PathBuf::from(env::var("OUT_DIR").unwrap());
    build_baffle(&out_dir);

    // Escape hatch: when prebuilt Linux helpers are provided (e.g. a macOS
    // release build that cannot run a Linux container), copy them straight in
    // and skip the nested helper build. All three must be set together.
    let prebuilt_helpers = [
        env::var("CLADDING_MCP_RUN_BIN").ok(),
        env::var("CLADDING_RUN_REMOTE_BIN").ok(),
        env::var("CLADDING_BRIDGE_BIN").ok(),
    ];
    match prebuilt_helpers {
        [Some(mcp_run), Some(run_remote), Some(bridge)] => {
            println!(
                "cargo:warning=Using prebuilt embedded helpers; skipping nested release build"
            );
            copy_bin(Path::new(&mcp_run), &out_dir.join("mcp-run"));
            copy_bin(Path::new(&run_remote), &out_dir.join("run-remote"));
            let bridge_path = PathBuf::from(bridge);
            let expected_arch = target_arch().unwrap_or_else(|err| panic!("{err}"));
            let bridge_bytes = fs::read(&bridge_path).unwrap_or_else(|err| {
                panic!(
                    "failed to read CLADDING_BRIDGE_BIN at {}: {err}",
                    bridge_path.display()
                )
            });
            validate_bridge_elf(&bridge_bytes, expected_arch, &bridge_path)
                .unwrap_or_else(|err| panic!("{err}"));
            copy_bin(&bridge_path, &out_dir.join("cladding-bridge"));
            return;
        }
        [None, None, None] => {}
        _ => panic!(
            "CLADDING_MCP_RUN_BIN, CLADDING_RUN_REMOTE_BIN, and CLADDING_BRIDGE_BIN must be set together"
        ),
    }

    let target = env::var("TARGET").unwrap_or_default();
    let helper_target = linux_helper_target(&target).unwrap_or_else(|err| panic!("{err}"));
    let target_settings = cargo_target_environment(&helper_target);
    for (key, _) in &target_settings {
        println!("cargo:rerun-if-env-changed={}", key.to_string_lossy());
    }
    let target_dir = helper_target_dir(&out_dir);
    fs::create_dir_all(&target_dir).unwrap_or_else(|err| {
        panic!(
            "failed to create helper target directory {}: {err}",
            target_dir.display()
        )
    });

    if cfg!(target_os = "linux") {
        build_locally(
            workspace_root,
            &target_dir,
            &helper_target,
            &target_settings,
        );
    } else {
        build_with_podman(
            workspace_root,
            &target_dir,
            &helper_target,
            &target_settings,
        );
    }
    let release_dir = release_dir(&target_dir, &helper_target);

    copy_bin(&release_dir.join("mcp-run"), &out_dir.join("mcp-run"));
    copy_bin(&release_dir.join("run-remote"), &out_dir.join("run-remote"));
    let bridge_path = release_dir.join("cladding-bridge");
    let bridge_bytes = fs::read(&bridge_path)
        .unwrap_or_else(|err| panic!("failed to read {}: {err}", bridge_path.display()));
    let expected_arch = target_arch().unwrap_or_else(|err| panic!("{err}"));
    validate_bridge_elf(&bridge_bytes, expected_arch, &bridge_path)
        .unwrap_or_else(|err| panic!("{err}"));
    copy_bin(&bridge_path, &out_dir.join("cladding-bridge"));
}

fn ensure_supported_platforms() {
    let host = env::var("HOST").unwrap_or_default();
    let target = env::var("TARGET").unwrap_or_default();
    if !is_supported_platform(&host) || !is_supported_platform(&target) {
        panic!(
            "Cladding supports Linux and macOS hosts and targets on x86_64 and aarch64 only (build host '{host}', target '{target}')"
        );
    }
}

fn is_supported_platform(triple: &str) -> bool {
    let Some((arch, _)) = triple.split_once('-') else {
        return false;
    };

    matches!(arch, "x86_64" | "aarch64")
        && (triple.contains("-linux-") || triple.ends_with("-apple-darwin"))
}

fn validate_bridge_elf(bytes: &[u8], expected_arch: &str, path: &Path) -> Result<(), String> {
    if bytes.len() < 20
        || &bytes[..4] != b"\x7fELF"
        || bytes[4] != 2
        || bytes[5] != 1
        || !matches!(bytes[7], 0 | 3)
    {
        return Err(format!(
            "{} is not a 64-bit little-endian Linux ELF executable",
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
            "{} contains a {actual_arch} Linux executable, but Cladding target requires {expected_arch}",
            path.display()
        ));
    }
    Ok(())
}

fn build_baffle(out_dir: &Path) {
    let expected_arch = target_arch().unwrap_or_else(|err| panic!("{err}"));
    let out_path = out_dir.join("baffle");

    let prebuilt = env::var_os("CLADDING_BAFFLE_BIN").map(PathBuf::from).unwrap_or_else(|| {
        panic!(
            "CLADDING_BAFFLE_BIN is required; acquire the pinned Baffle {BAFFLE_VERSION} release with scripts/fetch_baffle.sh {expected_arch} <destination> and set CLADDING_BAFFLE_BIN to <destination>/baffle"
        )
    });
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
}

fn target_arch() -> Result<&'static str, String> {
    let target = env::var("TARGET").unwrap_or_default();
    target_arch_for(&target)
}

fn target_arch_for(target: &str) -> Result<&'static str, String> {
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

fn release_dir(target_dir: &Path, target: &str) -> PathBuf {
    target_dir.join(target).join("release")
}

fn helper_target_dir(out_dir: &Path) -> PathBuf {
    out_dir.join("mcp-run-target")
}

fn linux_helper_target(cladding_target: &str) -> Result<String, String> {
    let arch = target_arch_for(cladding_target)?;
    Ok(format!("{arch}-unknown-linux-gnu"))
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

fn build_locally(
    workspace_root: &Path,
    target_dir: &Path,
    target: &str,
    target_settings: &[(std::ffi::OsString, std::ffi::OsString)],
) {
    let mut cargo = Command::new("cargo");
    cargo.current_dir(workspace_root);
    configure_nested_cargo_environment(&mut cargo, target_settings);
    cargo
        .arg("build")
        .arg("--locked")
        .arg("--release")
        .arg("-p")
        .arg("mcp-run")
        .arg("-p")
        .arg("cladding-bridge");
    cargo.arg("--bin").arg("mcp-run");
    cargo.arg("--bin").arg("run-remote");
    cargo.arg("--bin").arg("cladding-bridge");
    cargo.arg("--target-dir").arg(target_dir);
    cargo.arg("--target").arg(target);
    cargo.env("CARGO_BUILD_TARGET", target);

    let status = cargo
        .status()
        .expect("failed to run cargo build for mcp-run");
    if !status.success() {
        panic!("cargo build -p mcp-run failed");
    }
}

fn configure_nested_cargo_environment(
    cargo: &mut Command,
    target_settings: &[(std::ffi::OsString, std::ffi::OsString)],
) {
    // Cargo exposes this value to build scripts after resolving flags for the
    // outer target. Let the nested Cargo process resolve flags for its own
    // helper target from raw settings and Cargo config instead.
    cargo.env_remove("CARGO_ENCODED_RUSTFLAGS");
    cargo.envs(target_settings.iter().cloned());
}

fn build_with_podman(
    workspace_root: &Path,
    target_dir: &Path,
    target: &str,
    target_settings: &[(std::ffi::OsString, std::ffi::OsString)],
) {
    let status = podman_build_command(workspace_root, target_dir, target, target_settings)
        .status()
        .expect("failed to run podman build for mcp-run");

    if !status.success() {
        panic!("podman cargo build for mcp-run failed");
    }
}

fn podman_build_command(
    workspace_root: &Path,
    target_dir: &Path,
    target: &str,
    target_settings: &[(std::ffi::OsString, std::ffi::OsString)],
) -> Command {
    let arch = target.split('-').next().unwrap_or_default();
    let mut command = Command::new("podman");
    command
        .arg("run")
        .arg("--rm")
        .arg("--platform")
        .arg(format!("linux/{arch}"))
        .arg("-e")
        .arg("CARGO_TARGET_DIR=/out")
        .arg("-e")
        .arg(format!("CARGO_BUILD_TARGET={target}"));
    for (key, value) in target_settings {
        let mut setting = key.clone();
        setting.push("=");
        setting.push(value);
        command.arg("-e").arg(setting);
    }
    command
        .arg("-v")
        .arg(format!("{}:/work:ro", workspace_root.display()))
        .arg("-v")
        .arg(format!("{}:/out:rw", target_dir.display()))
        .arg("-w")
        .arg("/work")
        .arg("docker.io/library/rust:1.96.0")
        .arg("cargo")
        .arg("build")
        .arg("--manifest-path")
        .arg("/work/Cargo.toml")
        .arg("--release")
        .arg("--locked")
        .arg("--target")
        .arg(target)
        .arg("--bin")
        .arg("mcp-run")
        .arg("--bin")
        .arg("run-remote")
        .arg("--bin")
        .arg("cladding-bridge");
    command
}

fn cargo_target_environment(target: &str) -> Vec<(std::ffi::OsString, std::ffi::OsString)> {
    cargo_target_environment_from(target, env::vars_os())
}

fn cargo_target_environment_from(
    target: &str,
    variables: impl IntoIterator<Item = (std::ffi::OsString, std::ffi::OsString)>,
) -> Vec<(std::ffi::OsString, std::ffi::OsString)> {
    let prefix = format!(
        "CARGO_TARGET_{}_",
        target.to_ascii_uppercase().replace('-', "_")
    );
    variables
        .into_iter()
        .filter(|(key, _)| key == "RUSTFLAGS" || key.to_string_lossy().starts_with(&prefix))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::{
        cargo_target_environment_from, configure_nested_cargo_environment, helper_target_dir,
        is_supported_platform, linux_helper_target, podman_build_command, release_dir,
    };
    use std::ffi::OsString;
    use std::path::Path;
    use std::process::Command;

    #[test]
    fn supports_linux_and_macos_architectures() {
        for triple in [
            "x86_64-unknown-linux-gnu",
            "aarch64-unknown-linux-gnu",
            "x86_64-apple-darwin",
            "aarch64-apple-darwin",
        ] {
            assert!(is_supported_platform(triple), "{triple}");
        }
    }

    #[test]
    fn rejects_unsupported_operating_systems_and_architectures() {
        for triple in [
            "x86_64-pc-windows-gnu",
            "aarch64-pc-windows-msvc",
            "x86_64-unknown-freebsd",
            "i686-unknown-linux-gnu",
            "x86_64-apple-ios",
        ] {
            assert!(!is_supported_platform(triple), "{triple}");
        }
    }

    #[test]
    fn selects_linux_helper_target_from_each_supported_cladding_target() {
        for (target, helper) in [
            ("x86_64-unknown-linux-gnu", "x86_64-unknown-linux-gnu"),
            ("aarch64-unknown-linux-gnu", "aarch64-unknown-linux-gnu"),
            ("x86_64-apple-darwin", "x86_64-unknown-linux-gnu"),
            ("aarch64-apple-darwin", "aarch64-unknown-linux-gnu"),
        ] {
            assert_eq!(linux_helper_target(target).unwrap(), helper);
        }
    }

    #[test]
    fn helper_artifacts_stay_under_cargo_output_directory() {
        let out_dir = Path::new("target/debug/build/cladding-abc/out");
        let target_dir = helper_target_dir(out_dir);
        assert_eq!(target_dir, out_dir.join("mcp-run-target"));
        assert!(target_dir.starts_with(out_dir));
        assert_eq!(
            release_dir(&target_dir, "aarch64-unknown-linux-gnu"),
            target_dir.join("aarch64-unknown-linux-gnu/release")
        );
    }

    #[test]
    fn podman_build_targets_the_selected_linux_architecture_and_output_dir() {
        let target_dir = Path::new("/cargo/out/mcp-run-target");
        let settings = vec![(
            OsString::from("CARGO_TARGET_AARCH64_UNKNOWN_LINUX_GNU_RUSTFLAGS"),
            OsString::from("-C target-feature=+crt-static"),
        )];
        let command = podman_build_command(
            Path::new("/workspace/cladding"),
            target_dir,
            "aarch64-unknown-linux-gnu",
            &settings,
        );
        let args = command
            .get_args()
            .map(|arg| arg.to_string_lossy().into_owned())
            .collect::<Vec<_>>();

        for expected in [
            "--platform",
            "linux/aarch64",
            "CARGO_TARGET_DIR=/out",
            "CARGO_BUILD_TARGET=aarch64-unknown-linux-gnu",
            "CARGO_TARGET_AARCH64_UNKNOWN_LINUX_GNU_RUSTFLAGS=-C target-feature=+crt-static",
            "/workspace/cladding:/work:ro",
            "/cargo/out/mcp-run-target:/out:rw",
            "docker.io/library/rust:1.96.0",
            "--target",
            "aarch64-unknown-linux-gnu",
        ] {
            assert!(
                args.iter().any(|arg| arg == expected),
                "missing {expected:?} in {args:?}"
            );
        }
    }

    #[test]
    fn nested_cargo_uses_helper_target_flags_not_outer_encoded_flags() {
        let helper_target = "aarch64-unknown-linux-gnu";
        let outer_target = "aarch64-apple-darwin";
        for outer_encoded_flags in ["", "--cfg=outer_encoded_flag"] {
            let target_env_name = |target: &str| {
                format!(
                    "CARGO_TARGET_{}_RUSTFLAGS",
                    target.to_ascii_uppercase().replace('-', "_")
                )
            };
            let parent_environment = vec![
                (
                    OsString::from("CARGO_ENCODED_RUSTFLAGS"),
                    OsString::from(outer_encoded_flags),
                ),
                (
                    OsString::from(target_env_name(outer_target)),
                    OsString::from("--cfg=outer_target_flag"),
                ),
                (
                    OsString::from(target_env_name(helper_target)),
                    OsString::from("--cfg=helper_target_flag"),
                ),
            ];
            let target_settings =
                cargo_target_environment_from(helper_target, parent_environment.clone());
            let outer_target_key = OsString::from(target_env_name(outer_target));
            assert!(
                target_settings
                    .iter()
                    .all(|(key, _)| key != "CARGO_ENCODED_RUSTFLAGS")
            );
            assert!(
                target_settings
                    .iter()
                    .all(|(key, _)| key != &outer_target_key)
            );

            let temp_root = std::env::temp_dir().join(format!(
                "cladding-build-flags-{}-{}",
                std::process::id(),
                outer_encoded_flags.is_empty()
            ));
            let _ = std::fs::remove_dir_all(&temp_root);
            std::fs::create_dir_all(temp_root.join("src")).unwrap();
            std::fs::write(
                temp_root.join("Cargo.toml"),
                "[package]\nname = \"flag-probe\"\nversion = \"0.1.0\"\nedition = \"2024\"\n",
            )
            .unwrap();
            std::fs::write(temp_root.join("src/lib.rs"), "pub fn probe() {}\n").unwrap();
            std::fs::create_dir_all(temp_root.join("cargo-home")).unwrap();

            let mut cargo = Command::new("cargo");
            cargo.env_clear();
            cargo.env("CARGO_HOME", temp_root.join("cargo-home"));
            for key in ["PATH", "HOME", "RUSTUP_HOME", "RUSTUP_TOOLCHAIN"] {
                if let Some(value) = std::env::var_os(key) {
                    cargo.env(key, value);
                }
            }
            cargo.env("CARGO_ENCODED_RUSTFLAGS", outer_encoded_flags);
            configure_nested_cargo_environment(&mut cargo, &target_settings);
            let output = cargo
                .arg("rustc")
                .arg("--offline")
                .arg("--manifest-path")
                .arg(temp_root.join("Cargo.toml"))
                .arg("--target")
                .arg(helper_target)
                .arg("--lib")
                .arg("--")
                .arg("--print")
                .arg("cfg")
                .output()
                .expect("failed to run nested Cargo flag probe");
            let _ = std::fs::remove_dir_all(&temp_root);
            assert!(
                output.status.success(),
                "nested Cargo flag probe failed: {}",
                String::from_utf8_lossy(&output.stderr)
            );
            let cfg = String::from_utf8_lossy(&output.stdout);
            assert!(
                cfg.lines().any(|line| line == "helper_target_flag"),
                "{cfg}"
            );
            assert!(
                !cfg.lines().any(|line| line == "outer_encoded_flag"),
                "{cfg}"
            );
            assert!(
                !cfg.lines().any(|line| line == "outer_target_flag"),
                "{cfg}"
            );
        }
    }
}
