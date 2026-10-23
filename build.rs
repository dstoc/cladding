use serde_json::Value;
use std::collections::BTreeSet;
use std::env;
use std::fs;
use std::path::{Path, PathBuf};

const EMPTY_MANIFEST: &str = "{\"schema_version\":1,\"cladding_release\":null,\"platforms\":{}}\n";
const UPSTREAM_PINS: &str = include_str!("scripts/dependency-pins.json");

fn main() {
    ensure_supported_platforms();
    let target = env::var("TARGET").unwrap_or_default();
    let architecture = linux_architecture(&target).unwrap_or_else(|error| panic!("{error}"));
    let out_dir = PathBuf::from(env::var("OUT_DIR").unwrap());

    println!("cargo:rerun-if-changed=Cargo.toml");
    println!("cargo:rerun-if-changed=scripts/dependency-pins.json");
    println!("cargo:rerun-if-env-changed=CLADDING_DEPENDENCY_MANIFEST");
    println!("cargo:rerun-if-env-changed=CLADDING_GANTRY_DOCS_DIR");
    println!("cargo:rustc-env=CLADDING_LINUX_ARCHITECTURE={architecture}");

    let manifest = match env::var_os("CLADDING_DEPENDENCY_MANIFEST") {
        Some(path) => {
            let path = PathBuf::from(path);
            println!("cargo:rerun-if-changed={}", path.display());
            let bytes = read_regular_file(&path, "dependency manifest");
            validate_release_manifest(
                &bytes,
                &env::var("CARGO_PKG_VERSION").unwrap(),
                UPSTREAM_PINS,
            )
            .unwrap_or_else(|error| panic!("{error}"));
            bytes
        }
        None => EMPTY_MANIFEST.as_bytes().to_vec(),
    };
    fs::write(out_dir.join("dependency-manifest.json"), manifest)
        .expect("write embedded dependency manifest");

    let gantry_docs = match env::var_os("CLADDING_GANTRY_DOCS_DIR") {
        Some(path) => {
            let path = PathBuf::from(path);
            println!("cargo:rerun-if-changed={}", path.display());
            Some(path)
        }
        None => None,
    };
    write_document(
        gantry_docs.as_deref(),
        "LICENSE",
        &out_dir.join("gantry-LICENSE"),
    );
    write_document(
        gantry_docs.as_deref(),
        "README.md",
        &out_dir.join("gantry-README.md"),
    );
    write_document(
        gantry_docs.as_deref(),
        "docs/reference.md",
        &out_dir.join("gantry-reference.md"),
    );
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

fn linux_architecture(target: &str) -> Result<&'static str, String> {
    let arch = match target.split('-').next().unwrap_or_default() {
        "x86_64" => "x86_64",
        "aarch64" => "aarch64",
        other => return Err(format!("unsupported Linux dependency architecture {other}")),
    };
    if !(target.contains("-unknown-linux-") || target.ends_with("-apple-darwin")) {
        return Err(format!(
            "unsupported Cladding target for Linux tools: {target}"
        ));
    }
    Ok(match arch {
        "x86_64" => "x86_64-unknown-linux-gnu",
        "aarch64" => "aarch64-unknown-linux-gnu",
        _ => unreachable!(),
    })
}

fn read_regular_file(path: &Path, description: &str) -> Vec<u8> {
    let metadata = fs::symlink_metadata(path).unwrap_or_else(|error| {
        panic!(
            "failed to inspect {description} at {}: {error}",
            path.display()
        )
    });
    if !metadata.is_file() || metadata.file_type().is_symlink() {
        panic!("{description} is not a regular file: {}", path.display());
    }
    fs::read(path).unwrap_or_else(|error| {
        panic!(
            "failed to read {description} at {}: {error}",
            path.display()
        )
    })
}

fn write_document(source_dir: Option<&Path>, relative: &str, output: &Path) {
    let contents = match source_dir {
        Some(directory) => {
            read_regular_file(&directory.join(relative), "Gantry distribution document")
        }
        None => Vec::new(),
    };
    fs::write(output, contents)
        .unwrap_or_else(|error| panic!("failed to write {}: {error}", output.display()));
}

fn validate_release_manifest(
    bytes: &[u8],
    cargo_version: &str,
    upstream_pins: &str,
) -> Result<(), String> {
    let manifest: Value = serde_json::from_slice(bytes)
        .map_err(|error| format!("dependency manifest is not valid JSON: {error}"))?;
    if manifest.get("schema_version").and_then(Value::as_u64) != Some(1) {
        return Err("dependency manifest has an unsupported schema".to_string());
    }
    let release = manifest
        .get("cladding_release")
        .and_then(Value::as_object)
        .ok_or_else(|| {
            "release dependency manifest has no Cladding release identity".to_string()
        })?;
    let version = release
        .get("version")
        .and_then(Value::as_str)
        .unwrap_or_default();
    let tag = release
        .get("tag")
        .and_then(Value::as_str)
        .unwrap_or_default();
    if version != cargo_version || tag != format!("v{cargo_version}") {
        return Err(format!(
            "dependency manifest tag {tag:?} does not match Cargo package version {cargo_version}"
        ));
    }

    let platforms = manifest
        .get("platforms")
        .and_then(Value::as_object)
        .ok_or_else(|| "dependency manifest has no platform records".to_string())?;
    let expected_platforms =
        BTreeSet::from(["x86_64-unknown-linux-gnu", "aarch64-unknown-linux-gnu"]);
    let actual_platforms = platforms
        .keys()
        .map(String::as_str)
        .collect::<BTreeSet<_>>();
    if actual_platforms != expected_platforms {
        return Err(
            "dependency manifest must contain both supported Linux architectures".to_string(),
        );
    }
    let expected_groups = BTreeSet::from(["baffle", "cladding-bridge", "gantry"]);
    let expected_executables =
        BTreeSet::from(["baffle", "cladding-bridge", "gantry", "run-remote"]);
    let pins: Value = serde_json::from_str(upstream_pins)
        .map_err(|error| format!("trusted dependency pins are invalid JSON: {error}"))?;
    for (architecture, records) in platforms {
        let records = records.as_array().ok_or_else(|| {
            format!("dependency manifest records for {architecture} are not an array")
        })?;
        if records.len() != 3 {
            return Err(format!(
                "dependency manifest must have three archive records for {architecture}"
            ));
        }
        let mut groups = BTreeSet::new();
        let mut executables = BTreeSet::new();
        for record in records {
            let name = record
                .get("name")
                .and_then(Value::as_str)
                .unwrap_or_default();
            if !groups.insert(name) {
                return Err(format!(
                    "duplicate dependency archive group {name} for {architecture}"
                ));
            }
            if record.get("architecture").and_then(Value::as_str) != Some(architecture) {
                return Err(format!(
                    "dependency archive {name} has the wrong architecture"
                ));
            }
            let archive_hash = record
                .get("archive_sha256")
                .and_then(Value::as_str)
                .unwrap_or_default();
            if !valid_sha256(archive_hash) {
                return Err(format!(
                    "dependency archive {name} has an invalid SHA-256 digest"
                ));
            }
            let repository = record
                .get("source_repository")
                .and_then(Value::as_str)
                .unwrap_or_default();
            let release_tag = record
                .get("release_tag")
                .and_then(Value::as_str)
                .unwrap_or_default();
            let filename = record
                .get("archive_filename")
                .and_then(Value::as_str)
                .unwrap_or_default();
            if name == "cladding-bridge" {
                let target = architecture;
                if repository != "dstoc/cladding"
                    || release_tag != tag
                    || filename != format!("cladding-bridge-{tag}-{target}.tar.gz")
                {
                    return Err(format!(
                        "bridge metadata for {architecture} does not match release tag {tag}"
                    ));
                }
            } else {
                let arch = architecture
                    .strip_suffix("-unknown-linux-gnu")
                    .unwrap_or_default();
                let pin = pins
                    .get("dependencies")
                    .and_then(|value| value.get(name))
                    .ok_or_else(|| format!("trusted dependency pin is missing {name}"))?;
                let version = pin
                    .get("version")
                    .and_then(Value::as_str)
                    .unwrap_or_default();
                let expected_tag = format!("v{version}");
                let expected_repository = pin
                    .get("repository")
                    .and_then(Value::as_str)
                    .unwrap_or_default();
                let expected_arch = pin
                    .get("archives")
                    .and_then(|value| value.get(arch))
                    .ok_or_else(|| format!("trusted {name} pin is missing {arch}"))?;
                let expected_filename = expected_arch
                    .get("filename")
                    .and_then(Value::as_str)
                    .unwrap_or_default();
                let expected_sha = expected_arch
                    .get("sha256")
                    .and_then(Value::as_str)
                    .unwrap_or_default();
                if repository != expected_repository
                    || release_tag != expected_tag
                    || filename != expected_filename
                    || archive_hash != expected_sha
                {
                    return Err(format!(
                        "dependency archive {name} for {architecture} does not match its trusted pin"
                    ));
                }
            }
            let records = record
                .get("executables")
                .and_then(Value::as_array)
                .ok_or_else(|| format!("dependency archive {name} has no executable records"))?;
            for executable in records {
                let path = executable
                    .get("install_path")
                    .and_then(Value::as_str)
                    .unwrap_or_default();
                let hash = executable
                    .get("sha256")
                    .and_then(Value::as_str)
                    .unwrap_or_default();
                if !valid_sha256(hash) || !executables.insert(path) {
                    return Err(format!(
                        "invalid or duplicate executable checksum for {path}"
                    ));
                }
            }
        }
        if groups != expected_groups || executables != expected_executables {
            return Err(format!(
                "dependency manifest for {architecture} must pin bridge, Gantry, run-remote, and Baffle"
            ));
        }
    }
    Ok(())
}

fn valid_sha256(value: &str) -> bool {
    value.len() == 64 && value.bytes().all(|byte| byte.is_ascii_hexdigit())
}
