use super::mounts::parse_mounts_v2;
use super::types::{DEFAULT_COMPONENT_IMAGE, ExecutionComponentConfig, ExecutionConfig};
use crate::error::{Error, Result};
use anyhow::Context as _;
use std::collections::HashSet;
use std::fs;
use std::path::Path;

pub fn load_cladding_config_v2(project_root: &Path) -> Result<ExecutionConfig> {
    let config_path = project_root.join("cladding.json");

    if !config_path.exists() {
        eprintln!("missing: cladding.json ({})", config_path.display());
        eprintln!("hint: run cladding init");
        return Err(Error::message("missing cladding.json"));
    }

    let raw = fs::read_to_string(&config_path)
        .with_context(|| format!("failed to read {}", config_path.display()))?;

    let parsed: serde_json::Value = serde_json::from_str(&raw).map_err(|_| {
        eprintln!("error: cladding.json must include string key: name");
        Error::message("invalid cladding.json")
    })?;

    validate_top_level_keys(&parsed, &config_path)?;

    let name = get_config_string(&parsed, "name", &config_path)?;
    let use_runsc = get_config_bool(&parsed, "use_runsc", &config_path)?;
    let agent = parse_component_object(&parsed, "agent", true, &config_path)?;
    let nw_sandbox = parse_component_object(&parsed, "nw_sandbox", false, &config_path)?;
    let fs_sandbox = parse_component_object(&parsed, "fs_sandbox", false, &config_path)?;
    let execution_config = ExecutionConfig {
        name: name.clone(),
        use_runsc,
        agent: agent.expect("required component already validated"),
        nw_sandbox,
        fs_sandbox,
        mounts: Vec::new(),
    };
    let mut used_mount_targets = HashSet::new();
    let mounts = parse_mounts_v2(
        project_root,
        &parsed,
        &config_path,
        &execution_config,
        &mut used_mount_targets,
    )?;
    let execution_config = ExecutionConfig {
        mounts,
        ..execution_config
    };

    if !is_lowercase_alnum(&name) {
        eprintln!("error: config key 'name' must be lowercase alphanumeric ([a-z0-9]+)");
        eprintln!("file: {}", config_path.display());
        return Err(Error::message("invalid name"));
    }

    Ok(execution_config)
}

fn validate_top_level_keys(parsed: &serde_json::Value, config_path: &Path) -> Result<()> {
    let Some(object) = parsed.as_object() else {
        eprintln!("error: cladding.json must be a JSON object");
        eprintln!("file: {}", config_path.display());
        return Err(Error::message("invalid cladding.json"));
    };

    let allowed = [
        "name",
        "use_runsc",
        "agent",
        "nw_sandbox",
        "fs_sandbox",
        "mounts",
    ];
    let mut invalid = false;
    for key in object.keys() {
        if allowed.contains(&key.as_str()) {
            continue;
        }
        eprintln!("error: cladding.json unknown key: {key}");
        if let Some(replacement) = legacy_config_key_replacement(key) {
            eprintln!("hint: replace '{key}' with '{replacement}'");
        }
        eprintln!("file: {}", config_path.display());
        invalid = true;
    }

    if invalid {
        return Err(Error::message("invalid cladding.json"));
    }

    Ok(())
}

fn legacy_config_key_replacement(key: &str) -> Option<&'static str> {
    match key {
        "nw_sandbox_image" => Some("nw_sandbox.image"),
        "agent_image" => Some("agent.image"),
        "sandbox_image" => Some("nw_sandbox.image"),
        "cli_image" => Some("agent.image"),
        _ => None,
    }
}

fn get_config_string(parsed: &serde_json::Value, key: &str, config_path: &Path) -> Result<String> {
    parsed
        .get(key)
        .and_then(|value| value.as_str())
        .map(|value| value.to_string())
        .ok_or_else(|| {
            eprintln!("error: cladding.json must include string key: {key}");
            eprintln!("file: {}", config_path.display());
            Error::message("invalid cladding.json")
        })
}

fn get_config_bool(parsed: &serde_json::Value, key: &str, config_path: &Path) -> Result<bool> {
    match parsed.get(key) {
        Some(value) => value.as_bool().ok_or_else(|| {
            eprintln!("error: cladding.json invalid field '{key}' (expected boolean)");
            eprintln!("file: {}", config_path.display());
            Error::message("invalid cladding.json")
        }),
        None => Ok(false),
    }
}

fn parse_component_object(
    parsed: &serde_json::Value,
    key: &str,
    required: bool,
    config_path: &Path,
) -> Result<Option<ExecutionComponentConfig>> {
    let Some(raw) = parsed.get(key) else {
        if required {
            eprintln!("error: cladding.json must include object key: {key}");
            eprintln!("file: {}", config_path.display());
            return Err(Error::message("invalid cladding.json"));
        }
        return Ok(None);
    };

    let object = raw.as_object().ok_or_else(|| {
        eprintln!("error: cladding.json field '{key}' must be an object");
        eprintln!("file: {}", config_path.display());
        Error::message("invalid cladding.json")
    })?;

    validate_component_keys(object, key, config_path)?;

    let image = match object.get("image") {
        Some(value) => value.as_str().ok_or_else(|| {
            eprintln!("error: cladding.json invalid field '{key}.image' (expected string)");
            eprintln!("file: {}", config_path.display());
            Error::message("invalid cladding.json")
        })?,
        None => DEFAULT_COMPONENT_IMAGE,
    };

    if key == "agent" {
        if object.contains_key("enabled") {
            eprintln!(
                "error: cladding.json invalid field 'agent.enabled' (agent cannot be disabled)"
            );
            eprintln!("hint: remove 'agent.enabled'");
            eprintln!("file: {}", config_path.display());
            return Err(Error::message("invalid cladding.json"));
        }

        return Ok(Some(ExecutionComponentConfig {
            enabled: true,
            image: image.to_string(),
        }));
    }

    let enabled = match object.get("enabled") {
        Some(value) => value.as_bool().ok_or_else(|| {
            eprintln!("error: cladding.json invalid field '{key}.enabled' (expected boolean)");
            eprintln!("file: {}", config_path.display());
            Error::message("invalid cladding.json")
        })?,
        None => true,
    };

    Ok(Some(ExecutionComponentConfig {
        enabled,
        image: image.to_string(),
    }))
}

fn validate_component_keys(
    object: &serde_json::Map<String, serde_json::Value>,
    component_key: &str,
    config_path: &Path,
) -> Result<()> {
    let allowed = ["image", "enabled"];

    let mut invalid = false;
    for key in object.keys() {
        if allowed.contains(&key.as_str()) {
            continue;
        }
        eprintln!("error: cladding.json unknown key: {component_key}.{key}");
        eprintln!("file: {}", config_path.display());
        invalid = true;
    }

    if invalid {
        return Err(Error::message("invalid cladding.json"));
    }

    Ok(())
}

fn is_lowercase_alnum(name: &str) -> bool {
    !name.is_empty()
        && name
            .chars()
            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::env;
    use std::path::PathBuf;
    use std::sync::atomic::{AtomicUsize, Ordering};

    static TEMP_DIR_COUNTER: AtomicUsize = AtomicUsize::new(0);

    #[test]
    fn load_cladding_config_parses_component_objects() {
        let temp = create_temp_dir("component-config");
        fs::write(
            temp.join("cladding.json"),
            r#"{
  "name": "demo",
  "agent": {
    "image": "agent:image"
  },
  "nw_sandbox": {
    "image": "sandbox:image"
  },
  "fs_sandbox": {
    "enabled": true,
    "image": "fs:image"
  }
}"#,
        )
        .unwrap();

        let config = load_cladding_config_v2(&temp).unwrap();
        assert!(!config.use_runsc);
        assert_eq!(config.agent_image(), "agent:image");
        assert_eq!(config.nw_sandbox_image(), "sandbox:image");
        assert!(config.nw_sandbox_enabled());
        assert!(config.fs_sandbox_enabled());
        assert_eq!(config.fs_sandbox_image(), "fs:image");
        assert!(config.mounts.is_empty());
    }

    #[test]
    fn load_cladding_config_defaults_use_runsc_to_false() {
        let temp = create_temp_dir("default-use-runsc");
        fs::write(
            temp.join("cladding.json"),
            r#"{
  "name": "demo",
  "agent": {
    "image": "agent:image"
  }
}"#,
        )
        .unwrap();

        let config = load_cladding_config_v2(&temp).unwrap();
        assert!(!config.use_runsc);
    }

    #[test]
    fn load_cladding_config_accepts_explicit_use_runsc_values() {
        let temp = create_temp_dir("explicit-use-runsc");
        fs::write(
            temp.join("cladding.json"),
            r#"{
  "name": "demo",
  "use_runsc": true,
  "agent": {
    "image": "agent:image"
  }
}"#,
        )
        .unwrap();

        let config = load_cladding_config_v2(&temp).unwrap();
        assert!(config.use_runsc);

        fs::write(
            temp.join("cladding.json"),
            r#"{
  "name": "demo",
  "use_runsc": false,
  "agent": {
    "image": "agent:image"
  }
}"#,
        )
        .unwrap();

        let config = load_cladding_config_v2(&temp).unwrap();
        assert!(!config.use_runsc);
    }

    #[test]
    fn load_cladding_config_rejects_non_boolean_use_runsc() {
        let temp = create_temp_dir("non-boolean-use-runsc");
        fs::write(
            temp.join("cladding.json"),
            r#"{
  "name": "demo",
  "use_runsc": "yes",
  "agent": {
    "image": "agent:image"
  }
}"#,
        )
        .unwrap();

        assert!(load_cladding_config_v2(&temp).is_err());
    }

    #[test]
    fn load_cladding_config_rejects_legacy_top_level_keys() {
        let temp = create_temp_dir("legacy-top-level");
        fs::write(
            temp.join("cladding.json"),
            r#"{
  "name": "demo",
  "agent_image": "agent:image",
  "nw_sandbox_image": "sandbox:image"
}"#,
        )
        .unwrap();

        assert!(load_cladding_config_v2(&temp).is_err());
    }

    #[test]
    fn load_cladding_config_rejects_agent_enabled() {
        let temp = create_temp_dir("agent-enabled");
        fs::write(
            temp.join("cladding.json"),
            r#"{
  "name": "demo",
  "agent": {
    "enabled": false,
    "image": "agent:image"
  }
}"#,
        )
        .unwrap();

        assert!(load_cladding_config_v2(&temp).is_err());
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
