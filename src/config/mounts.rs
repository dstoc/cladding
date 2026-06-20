use super::types::{ExecutionConfig, MountTarget, ResolvedMountConfig};
use crate::error::{Error, Result};
use std::collections::HashSet;
use std::path::{Path, PathBuf};

pub(super) fn parse_mounts_v2(
    project_root: &Path,
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
                    project_root.join(candidate)
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

        let read_only = match object.get("readOnly") {
            Some(value) => value.as_bool().ok_or_else(|| {
                eprintln!("error: cladding.json invalid field 'mounts[{index}].readOnly' (expected boolean)");
                eprintln!("file: {}", config_path.display());
                Error::message("invalid cladding.json")
            })?,
            None => false,
        };

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

        if volume.is_some() && read_only {
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

        if ignore && object.get("readOnly").is_some() {
            eprintln!(
                "error: cladding.json invalid field 'mounts[{index}].readOnly' (readOnly not supported when ignore is true)"
            );
            eprintln!("file: {}", config_path.display());
            return Err(Error::message("invalid cladding.json"));
        }

        let read_only = if host_path.is_none() && volume.is_none() {
            true
        } else {
            read_only
        };

        mounts.push(ResolvedMountConfig {
            mount_path: mount_path.to_string(),
            host_path,
            volume,
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
        "mount", "hostPath", "volume", "readOnly", "targets", "ignore",
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
            mounts: Vec::new(),
        }
    }

    fn component(enabled: bool) -> ExecutionComponentConfig {
        ExecutionComponentConfig {
            enabled,
            image: "image:latest".to_string(),
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
