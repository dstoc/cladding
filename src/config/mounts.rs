use super::types::{ExecutionConfig, MountTarget, MountType, ResolvedMountConfig};
use crate::error::{Error, Result};
use std::collections::HashSet;
use std::fs;
use std::path::{Path, PathBuf};

pub(super) fn parse_mounts_v2(
    config_dir: &Path,
    parsed: &serde_json::Value,
    config_path: &Path,
    execution_config: &ExecutionConfig,
    used_mount_targets: &mut HashSet<(MountTarget, String)>,
) -> Result<Vec<ResolvedMountConfig>> {
    let Some(raw) = parsed.get("mounts") else {
        return Ok(Vec::new());
    };

    let array = raw.as_array().ok_or_else(|| {
        eprintln!("error: cladding.json field 'mounts' must be an array");
        eprintln!("file: {}", config_path.display());
        Error::message("invalid cladding.json")
    })?;

    let mut mounts = Vec::with_capacity(array.len());
    for (index, entry) in array.iter().enumerate() {
        let Some(object) = entry.as_object() else {
            eprintln!("error: cladding.json field 'mounts[{index}]' must be an object");
            eprintln!("file: {}", config_path.display());
            return Err(Error::message("invalid cladding.json"));
        };
        validate_mount_keys(object, index, config_path)?;

        let mount_path = object
            .get("mount")
            .and_then(|value| value.as_str())
            .ok_or_else(|| {
                eprintln!(
                    "error: cladding.json invalid field 'mounts[{index}].mount' (expected string)"
                );
                eprintln!("file: {}", config_path.display());
                Error::message("invalid cladding.json")
            })?;
        ensure_absolute_mount_path(config_path, &format!("mounts[{index}].mount"), mount_path)?;
        let normalized_mount_path = normalize_path(Path::new(mount_path));

        let host_path = match object.get("hostPath") {
            Some(value) => {
                let raw = value.as_str().ok_or_else(|| {
                    eprintln!("error: cladding.json invalid field 'mounts[{index}].hostPath' (expected string)");
                    eprintln!("file: {}", config_path.display());
                    Error::message("invalid cladding.json")
                })?;
                let candidate = PathBuf::from(raw);
                Some(if candidate.is_absolute() {
                    candidate
                } else {
                    config_dir.join(candidate)
                })
            }
            None => None,
        };

        let volume = match object.get("volume") {
            Some(value) => Some(
                value
                    .as_str()
                    .ok_or_else(|| {
                        eprintln!("error: cladding.json invalid field 'mounts[{index}].volume' (expected string)");
                        eprintln!("file: {}", config_path.display());
                        Error::message("invalid cladding.json")
                    })?
                    .to_string(),
            ),
            None => None,
        };

        let targets = parse_mount_targets(object, index, config_path, execution_config)?;
        for target in &targets {
            if !used_mount_targets.insert((*target, mount_path.to_string())) {
                eprintln!(
                    "error: cladding.json duplicate mount path '{mount_path}' for target '{}'",
                    target.as_str()
                );
                eprintln!("file: {}", config_path.display());
                return Err(Error::message("duplicate mount path"));
            }
        }

        if host_path.is_some() && volume.is_some() {
            eprintln!(
                "error: cladding.json invalid field 'mounts[{index}]' (hostPath and volume are mutually exclusive)"
            );
            eprintln!("file: {}", config_path.display());
            return Err(Error::message("invalid cladding.json"));
        }

        let legacy_read_only = match object.get("readOnly") {
            Some(value) => Some(value.as_bool().ok_or_else(|| {
                eprintln!("error: cladding.json invalid field 'mounts[{index}].readOnly' (expected boolean)");
                eprintln!("file: {}", config_path.display());
                Error::message("invalid cladding.json")
            })?),
            None => None,
        };
        if legacy_read_only.is_some() {
            eprintln!(
                "warning: mounts[{index}].readOnly is deprecated; use type: \"readonly\" instead"
            );
        }

        let configured_type = match object.get("type") {
            Some(value) => {
                let mount_type = value.as_str().ok_or_else(|| {
                    eprintln!("error: cladding.json invalid field 'mounts[{index}].type' (expected string)");
                    eprintln!("file: {}", config_path.display());
                    Error::message("invalid cladding.json")
                })?;
                Some(match mount_type {
                    "bind" => MountType::Bind,
                    "readonly" => MountType::Readonly,
                    "overlay" => MountType::Overlay,
                    "tmpfs" => MountType::Tmpfs,
                    _ => {
                        eprintln!(
                            "error: cladding.json invalid field 'mounts[{index}].type' (unknown type '{mount_type}')"
                        );
                        eprintln!("hint: valid mount types are bind, readonly, overlay, and tmpfs");
                        eprintln!("file: {}", config_path.display());
                        return Err(Error::message("invalid cladding.json"));
                    }
                })
            }
            None => None,
        };

        let tmpfs_size_bytes = match object.get("size") {
            Some(value) => {
                if configured_type != Some(MountType::Tmpfs) {
                    eprintln!(
                        "error: cladding.json invalid field 'mounts[{index}].size' (size is only valid for type 'tmpfs')"
                    );
                    eprintln!("file: {}", config_path.display());
                    return Err(Error::message("invalid cladding.json"));
                }
                let size = value.as_str().ok_or_else(|| {
                    eprintln!("error: cladding.json invalid field 'mounts[{index}].size' (expected a size string such as '1GiB')");
                    eprintln!("file: {}", config_path.display());
                    Error::message("invalid cladding.json")
                })?;
                Some(parse_tmpfs_size(size).ok_or_else(|| {
                    eprintln!(
                        "error: cladding.json invalid field 'mounts[{index}].size' (invalid size '{size}')"
                    );
                    eprintln!("hint: use a non-negative integer with optional B, K, M, G, T, KiB, MiB, GiB, or TiB units");
                    eprintln!("file: {}", config_path.display());
                    Error::message("invalid cladding.json")
                })?)
            }
            None => None,
        };

        let mount_type = configured_type.unwrap_or(if legacy_read_only == Some(true) {
            MountType::Readonly
        } else {
            MountType::Bind
        });

        if mount_type == MountType::Overlay
            && execution_config.use_runsc
            && targets.contains(&MountTarget::Agent)
        {
            eprintln!(
                "error: cladding.json invalid field 'mounts[{index}].type' (overlay mounts are not supported for the agent when use_runsc is enabled)"
            );
            eprintln!("hint: remove the agent target or disable use_runsc for this runtime");
            eprintln!("file: {}", config_path.display());
            return Err(Error::message("unsupported overlay runtime"));
        }

        if let Some(mount_type) = configured_type {
            let host_path_required = matches!(
                mount_type,
                MountType::Bind | MountType::Readonly | MountType::Overlay
            );
            if host_path_required && host_path.is_none() {
                eprintln!(
                    "error: cladding.json invalid field 'mounts[{index}].hostPath' (required for type '{}')",
                    mount_type.as_str()
                );
                eprintln!("file: {}", config_path.display());
                return Err(Error::message("invalid cladding.json"));
            }
            if mount_type == MountType::Tmpfs && (host_path.is_some() || volume.is_some()) {
                eprintln!(
                    "error: cladding.json invalid field 'mounts[{index}]' (type 'tmpfs' cannot use hostPath or volume)"
                );
                eprintln!("file: {}", config_path.display());
                return Err(Error::message("invalid cladding.json"));
            }
            if volume.is_some() {
                eprintln!(
                    "error: cladding.json invalid field 'mounts[{index}].type' (mount types cannot be combined with named volumes)"
                );
                eprintln!("file: {}", config_path.display());
                return Err(Error::message("invalid cladding.json"));
            }
        }

        match (configured_type, legacy_read_only) {
            (Some(MountType::Bind), Some(true))
            | (Some(MountType::Overlay), Some(true))
            | (Some(MountType::Tmpfs), Some(true))
            | (Some(MountType::Readonly), Some(false)) => {
                eprintln!(
                    "error: cladding.json invalid field 'mounts[{index}].readOnly' (conflicts with type '{}')",
                    configured_type.expect("matched configured type").as_str()
                );
                eprintln!("file: {}", config_path.display());
                return Err(Error::message("invalid cladding.json"));
            }
            _ => {}
        }

        let host_path_is_private = host_path
            .as_deref()
            .is_some_and(|path| path_is_inside_private_config(config_dir, path));
        let mount_path_is_inside_workspace_mask =
            normalized_mount_path.starts_with("/home/user/workspace/.cladding");
        let host_path_contains_private_config = mount_type == MountType::Bind
            && mount_path_is_inside_workspace_mask
            && host_path
                .as_deref()
                .is_some_and(|path| path_contains_private_config(config_dir, path));
        if (host_path_is_private || host_path_contains_private_config)
            && (mount_type != MountType::Bind || mount_path_is_inside_workspace_mask)
        {
            eprintln!(
                "error: cladding.json invalid field 'mounts[{index}].hostPath' (cannot mount files from the private .cladding directory with type '{}')",
                mount_type.as_str()
            );
            eprintln!("file: {}", config_path.display());
            return Err(Error::message("invalid cladding.json"));
        }

        let ignore = match object.get("ignore") {
            Some(value) => value.as_bool().ok_or_else(|| {
                eprintln!(
                    "error: cladding.json invalid field 'mounts[{index}].ignore' (expected boolean)"
                );
                eprintln!("file: {}", config_path.display());
                Error::message("invalid cladding.json")
            })?,
            None => false,
        };

        if ignore
            && normalized_mount_path == Path::new("/home/user/workspace/.cladding")
            && targets
                .iter()
                .any(|target| matches!(target, MountTarget::Agent | MountTarget::NwSandbox))
        {
            eprintln!(
                "error: cladding.json invalid field 'mounts[{index}].ignore' (the workspace .cladding mask cannot be removed)"
            );
            eprintln!("file: {}", config_path.display());
            return Err(Error::message("invalid cladding.json"));
        }

        if volume.is_some() && legacy_read_only == Some(true) {
            eprintln!(
                "error: cladding.json invalid field 'mounts[{index}].readOnly' (readOnly not supported for volume mounts)"
            );
            eprintln!("file: {}", config_path.display());
            return Err(Error::message("invalid cladding.json"));
        }

        if ignore && (host_path.is_some() || volume.is_some()) {
            eprintln!(
                "error: cladding.json invalid field 'mounts[{index}]' (ignore cannot be combined with hostPath or volume)"
            );
            eprintln!("file: {}", config_path.display());
            return Err(Error::message("invalid cladding.json"));
        }

        if ignore
            && (object.get("readOnly").is_some()
                || object.get("type").is_some()
                || object.get("size").is_some())
        {
            eprintln!(
                "error: cladding.json invalid field 'mounts[{index}]' (type, size, and readOnly are not supported when ignore is true)"
            );
            eprintln!("file: {}", config_path.display());
            return Err(Error::message("invalid cladding.json"));
        }

        let read_only = mount_type == MountType::Readonly
            || (host_path.is_none() && volume.is_none() && mount_type == MountType::Bind);

        mounts.push(ResolvedMountConfig {
            mount_path: mount_path.to_string(),
            host_path,
            volume,
            mount_type,
            tmpfs_size_bytes,
            read_only,
            targets,
            ignore,
        });
    }

    Ok(mounts)
}

fn parse_mount_targets(
    object: &serde_json::Map<String, serde_json::Value>,
    index: usize,
    config_path: &Path,
    execution_config: &ExecutionConfig,
) -> Result<Vec<MountTarget>> {
    let Some(raw_targets) = object.get("targets") else {
        return Ok(execution_config.default_mount_targets());
    };

    let array = raw_targets.as_array().ok_or_else(|| {
        eprintln!("error: cladding.json field 'mounts[{index}].targets' must be an array");
        eprintln!("file: {}", config_path.display());
        Error::message("invalid cladding.json")
    })?;

    if array.is_empty() {
        eprintln!("error: cladding.json field 'mounts[{index}].targets' must not be empty");
        eprintln!("file: {}", config_path.display());
        return Err(Error::message("invalid cladding.json"));
    }

    let mut targets = Vec::with_capacity(array.len());
    let mut seen = HashSet::new();
    for (target_index, value) in array.iter().enumerate() {
        let target_name = value.as_str().ok_or_else(|| {
            eprintln!(
                "error: cladding.json invalid field 'mounts[{index}].targets[{target_index}]' (expected string)"
            );
            eprintln!("file: {}", config_path.display());
            Error::message("invalid cladding.json")
        })?;

        let target = MountTarget::from_config_name(target_name).ok_or_else(|| {
            eprintln!(
                "error: cladding.json invalid field 'mounts[{index}].targets[{target_index}]' (unknown target '{target_name}')"
            );
            eprintln!("hint: valid targets are agent, nw-sandbox, and fs-sandbox");
            eprintln!("file: {}", config_path.display());
            Error::message("invalid cladding.json")
        })?;

        if !target.is_enabled(execution_config) {
            eprintln!(
                "error: cladding.json invalid field 'mounts[{index}].targets[{target_index}]' (target '{target_name}' is disabled)"
            );
            eprintln!(
                "hint: enable '{}' in cladding.json or remove this target",
                target.config_key()
            );
            eprintln!("file: {}", config_path.display());
            return Err(Error::message("invalid cladding.json"));
        }

        if seen.insert(target) {
            targets.push(target);
        }
    }

    Ok(targets)
}

fn validate_mount_keys(
    object: &serde_json::Map<String, serde_json::Value>,
    index: usize,
    config_path: &Path,
) -> Result<()> {
    let allowed = [
        "mount", "hostPath", "volume", "type", "size", "readOnly", "targets", "ignore",
    ];
    let mut invalid = false;
    for key in object.keys() {
        if allowed.contains(&key.as_str()) {
            continue;
        }
        eprintln!("error: cladding.json unknown key: mounts[{index}].{key}");
        if key == "nwSandboxOnly" || key == "sandboxOnly" {
            eprintln!(
                "hint: replace 'mounts[{index}].{key}' with 'mounts[{index}].targets': [\"nw-sandbox\"]"
            );
        }
        eprintln!("file: {}", config_path.display());
        invalid = true;
    }

    if invalid {
        return Err(Error::message("invalid cladding.json"));
    }

    Ok(())
}

impl MountType {
    fn as_str(self) -> &'static str {
        match self {
            Self::Bind => "bind",
            Self::Readonly => "readonly",
            Self::Overlay => "overlay",
            Self::Tmpfs => "tmpfs",
        }
    }
}

fn parse_tmpfs_size(value: &str) -> Option<u64> {
    let digit_count = value.bytes().take_while(u8::is_ascii_digit).count();
    if digit_count == 0 {
        return None;
    }

    let amount = value[..digit_count].parse::<u64>().ok()?;
    let multiplier = match value[digit_count..].to_ascii_lowercase().as_str() {
        "" | "b" | "byte" | "bytes" => 1,
        "k" | "kib" => 1024,
        "kb" => 1000,
        "m" | "mib" => 1024_u64.pow(2),
        "mb" => 1000_u64.pow(2),
        "g" | "gib" => 1024_u64.pow(3),
        "gb" => 1000_u64.pow(3),
        "t" | "tib" => 1024_u64.pow(4),
        "tb" => 1000_u64.pow(4),
        _ => return None,
    };

    amount.checked_mul(multiplier)
}

fn path_is_inside_private_config(config_dir: &Path, candidate: &Path) -> bool {
    let config_dir = fs::canonicalize(config_dir).unwrap_or_else(|_| normalize_path(config_dir));
    let candidate = fs::canonicalize(candidate).unwrap_or_else(|_| normalize_path(candidate));
    candidate.starts_with(config_dir)
}

fn path_contains_private_config(config_dir: &Path, candidate: &Path) -> bool {
    let config_dir = fs::canonicalize(config_dir).unwrap_or_else(|_| normalize_path(config_dir));
    let candidate = fs::canonicalize(candidate).unwrap_or_else(|_| normalize_path(candidate));
    config_dir.starts_with(candidate)
}

fn normalize_path(path: &Path) -> PathBuf {
    let mut normalized = PathBuf::new();
    for component in path.components() {
        match component {
            std::path::Component::CurDir => {}
            std::path::Component::ParentDir => {
                if normalized.file_name().is_some() {
                    normalized.pop();
                }
            }
            other => normalized.push(other.as_os_str()),
        }
    }
    normalized
}

fn ensure_absolute_mount_path(config_path: &Path, field: &str, mount_path: &str) -> Result<()> {
    if Path::new(mount_path).is_absolute() {
        return Ok(());
    }
    eprintln!("error: cladding.json invalid field '{field}' (mount path must be absolute)");
    eprintln!("file: {}", config_path.display());
    Err(Error::message("invalid cladding.json"))
}

#[cfg(test)]
mod tests {
    use super::super::types::ExecutionComponentConfig;
    use super::*;
    use std::env;
    use std::fs;
    use std::sync::atomic::{AtomicUsize, Ordering};

    static TEMP_DIR_COUNTER: AtomicUsize = AtomicUsize::new(0);

    #[test]
    fn parse_mounts_parses_targets_and_default_targets() {
        let temp = create_temp_dir("mount-targets");
        let parsed = serde_json::json!({
            "mounts": [
                {
                    "mount": "/workspace",
                    "hostPath": "./workspace",
                    "targets": ["agent", "nw-sandbox"]
                },
                {
                    "mount": "/workspace",
                    "hostPath": "./workspace-fs",
                    "targets": ["fs-sandbox"]
                },
                {
                    "mount": "/shared",
                    "hostPath": "./shared"
                }
            ]
        });
        let config = execution_config(true, true);

        let mounts = parse_mounts(&temp, &parsed, &config).unwrap();

        assert_eq!(mounts.len(), 3);
        assert_eq!(
            mounts[0].targets,
            vec![MountTarget::Agent, MountTarget::NwSandbox]
        );
        assert_eq!(mounts[1].targets, vec![MountTarget::FsSandbox]);
        assert_eq!(
            mounts[2].targets,
            vec![MountTarget::Agent, MountTarget::NwSandbox]
        );
    }

    #[test]
    fn parse_mounts_supports_types_tmpfs_size_and_legacy_read_only() {
        let temp = create_temp_dir("mount-types");
        let parsed = serde_json::json!({
            "mounts": [
                { "mount": "/bind", "hostPath": "./workspace" },
                { "mount": "/reference", "hostPath": "../reference", "type": "readonly" },
                { "mount": "/overlay", "hostPath": "../workspace", "type": "overlay", "readOnly": false, "targets": ["agent"] },
                { "mount": "/tmp", "type": "tmpfs", "size": "1GiB", "targets": ["agent"] },
                { "mount": "/legacy-ro", "hostPath": "../config", "readOnly": true },
                { "mount": "/legacy-rw", "hostPath": "../cache", "readOnly": false }
            ]
        });

        let mounts = parse_mounts(&temp, &parsed, &execution_config(true, true)).unwrap();

        assert_eq!(mounts[0].mount_type, MountType::Bind);
        assert!(!mounts[0].read_only);
        assert_eq!(mounts[1].mount_type, MountType::Readonly);
        assert!(mounts[1].read_only);
        assert_eq!(mounts[2].mount_type, MountType::Overlay);
        assert!(!mounts[2].read_only);
        assert_eq!(mounts[3].mount_type, MountType::Tmpfs);
        assert_eq!(mounts[3].tmpfs_size_bytes, Some(1024 * 1024 * 1024));
        assert_eq!(mounts[4].mount_type, MountType::Readonly);
        assert!(mounts[4].read_only);
        assert_eq!(mounts[5].mount_type, MountType::Bind);
        assert!(!mounts[5].read_only);
    }

    #[test]
    fn parse_mounts_rejects_invalid_type_combinations_and_mode() {
        let temp = create_temp_dir("invalid-mount-types");
        let config = execution_config(true, false);
        for mount in [
            serde_json::json!({ "mount": "/data", "type": "bind" }),
            serde_json::json!({ "mount": "/data", "type": "tmpfs", "hostPath": "../data" }),
            serde_json::json!({ "mount": "/data", "hostPath": "../data", "size": "1GiB" }),
            serde_json::json!({ "mount": "/data", "hostPath": "../data", "type": "overlay", "readOnly": true }),
            serde_json::json!({ "mount": "/data", "hostPath": "../data", "type": "bind", "readOnly": true }),
            serde_json::json!({ "mount": "/data", "type": "tmpfs", "readOnly": true }),
            serde_json::json!({ "mount": "/data", "hostPath": "../data", "type": "readonly", "readOnly": false }),
            serde_json::json!({ "mount": "/data", "hostPath": "../data", "mode": "overlay" }),
            serde_json::json!({ "mount": "/home/user/workspace/.cladding", "ignore": true }),
        ] {
            assert!(
                parse_mounts(&temp, &serde_json::json!({ "mounts": [mount] }), &config).is_err()
            );
        }
    }

    #[test]
    fn parse_mounts_rejects_overlay_of_private_cladding_files_and_runsc_agent() {
        let temp = create_temp_dir("private-mount");
        let private_overlay = serde_json::json!({
            "mounts": [{ "mount": "/private", "hostPath": ".", "type": "overlay" }]
        });
        assert!(parse_mounts(&temp, &private_overlay, &execution_config(true, false)).is_err());

        let mut runsc_config = execution_config(true, false);
        runsc_config.use_runsc = true;
        let agent_overlay = serde_json::json!({
            "mounts": [{ "mount": "/workspace", "hostPath": "../workspace", "type": "overlay", "targets": ["agent"] }]
        });
        assert!(parse_mounts(&temp, &agent_overlay, &runsc_config).is_err());
    }

    #[test]
    fn parse_mounts_rejects_private_cladding_files_for_implicit_and_explicit_binds() {
        let temp = create_temp_dir("private-bind-mount");
        for mount in [
            serde_json::json!({
                "mount": "/home/user/workspace/.cladding",
                "hostPath": "."
            }),
            serde_json::json!({
                "mount": "/home/user/workspace/.cladding",
                "hostPath": ".",
                "type": "bind"
            }),
            serde_json::json!({
                "mount": "/home/user/workspace/.cladding",
                "hostPath": ".."
            }),
            serde_json::json!({
                "mount": "/home/user/workspace/.cladding",
                "hostPath": "..",
                "type": "bind"
            }),
            serde_json::json!({
                "mount": "/home/user/workspace/../workspace/.cladding",
                "hostPath": "."
            }),
            serde_json::json!({
                "mount": "/home/user/workspace/../workspace/.cladding",
                "hostPath": ".",
                "type": "bind"
            }),
        ] {
            assert!(
                parse_mounts(
                    &temp,
                    &serde_json::json!({ "mounts": [mount] }),
                    &execution_config(true, false)
                )
                .is_err()
            );
        }
    }

    #[test]
    fn parse_mounts_accepts_tmpfs_without_a_size() {
        let temp = create_temp_dir("tmpfs-default-size");
        let parsed = serde_json::json!({
            "mounts": [{ "mount": "/tmp", "type": "tmpfs" }]
        });

        let mounts = parse_mounts(&temp, &parsed, &execution_config(true, false)).unwrap();

        assert_eq!(mounts[0].mount_type, MountType::Tmpfs);
        assert_eq!(mounts[0].tmpfs_size_bytes, None);
        assert!(!mounts[0].read_only);
    }

    #[test]
    fn parse_tmpfs_size_validates_and_converts_units_to_bytes() {
        assert_eq!(parse_tmpfs_size("1GiB"), Some(1024_u64.pow(3)));
        assert_eq!(parse_tmpfs_size("2MB"), Some(2_000_000));
        assert_eq!(parse_tmpfs_size("12"), Some(12));
        assert_eq!(parse_tmpfs_size("1.5GiB"), None);
        assert_eq!(parse_tmpfs_size("1GB-extra"), None);
        assert_eq!(parse_tmpfs_size("18446744073709551615K"), None);
    }

    #[test]
    fn parse_mounts_rejects_disabled_mount_target() {
        let temp = create_temp_dir("disabled-mount-target");
        let parsed = serde_json::json!({
            "mounts": [
                {
                    "mount": "/workspace",
                    "hostPath": "./one",
                    "targets": ["fs-sandbox"]
                }
            ]
        });
        let config = execution_config(false, false);

        assert!(parse_mounts(&temp, &parsed, &config).is_err());
    }

    #[test]
    fn parse_mounts_rejects_legacy_mount_target_keys() {
        let temp = create_temp_dir("legacy-mount-target");
        let parsed = serde_json::json!({
            "mounts": [
                {
                    "mount": "/workspace",
                    "hostPath": "./one",
                    "nwSandboxOnly": true
                }
            ]
        });
        let config = execution_config(false, false);

        assert!(parse_mounts(&temp, &parsed, &config).is_err());
    }

    #[test]
    fn parse_mounts_rejects_duplicate_mount_paths_per_target() {
        let temp = create_temp_dir("duplicate-mounts");
        let config = execution_config(true, false);
        let separate_targets = serde_json::json!({
            "mounts": [
                {
                    "mount": "/workspace",
                    "hostPath": "./one",
                    "targets": ["agent"]
                },
                {
                    "mount": "/workspace",
                    "hostPath": "./two",
                    "targets": ["nw-sandbox"]
                }
            ]
        });

        assert!(parse_mounts(&temp, &separate_targets, &config).is_ok());

        let duplicate_default_targets = serde_json::json!({
            "mounts": [
                {
                    "mount": "/workspace",
                    "hostPath": "./one"
                },
                {
                    "mount": "/workspace",
                    "hostPath": "./two"
                }
            ]
        });

        assert!(parse_mounts(&temp, &duplicate_default_targets, &config).is_err());
    }

    fn parse_mounts(
        project_root: &Path,
        parsed: &serde_json::Value,
        execution_config: &ExecutionConfig,
    ) -> Result<Vec<ResolvedMountConfig>> {
        let mut used_mount_targets = HashSet::new();
        parse_mounts_v2(
            project_root,
            parsed,
            &project_root.join("cladding.json"),
            execution_config,
            &mut used_mount_targets,
        )
    }

    fn execution_config(nw_sandbox_enabled: bool, fs_sandbox_enabled: bool) -> ExecutionConfig {
        ExecutionConfig {
            name: "demo".to_string(),
            use_runsc: false,
            agent: component(true),
            nw_sandbox: Some(component(nw_sandbox_enabled)),
            fs_sandbox: Some(component(fs_sandbox_enabled)),
            proxy: None,
            mounts: Vec::new(),
        }
    }

    fn component(enabled: bool) -> ExecutionComponentConfig {
        ExecutionComponentConfig {
            enabled,
            image: "image:latest".to_string(),
            build: None,
        }
    }

    fn create_temp_dir(name: &str) -> PathBuf {
        let unique = TEMP_DIR_COUNTER.fetch_add(1, Ordering::Relaxed);
        let path =
            env::temp_dir().join(format!("cladding-{name}-{}-{}", std::process::id(), unique));
        if path.exists() {
            fs::remove_dir_all(&path).unwrap();
        }
        fs::create_dir_all(&path).unwrap();
        path
    }
}
