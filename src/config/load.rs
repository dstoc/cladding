use super::mounts::parse_mounts_v2;
use super::types::{
    DEFAULT_COMPONENT_IMAGE, DEFAULT_PROXY_IMAGE, ExecutionComponentConfig, ExecutionConfig,
    ExecutionProxyConfig, ImageBuildConfig,
};
use crate::error::{Error, Result};
use anyhow::Context as _;
use std::collections::{BTreeMap, HashSet};
use std::fs;
use std::path::{Path, PathBuf};

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
    let agent = parse_component_object(&parsed, "agent", true, &name, &config_path)?;
    let nw_sandbox = parse_component_object(&parsed, "nw_sandbox", false, &name, &config_path)?;
    let fs_sandbox = parse_component_object(&parsed, "fs_sandbox", false, &name, &config_path)?;
    let proxy = parse_proxy_object(&parsed, &name, &config_path)?;
    let execution_config = ExecutionConfig {
        name: name.clone(),
        use_runsc,
        agent: agent.expect("required component already validated"),
        nw_sandbox,
        fs_sandbox,
        proxy,
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
        "proxy",
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
    project_name: &str,
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

    let build = parse_build_config(object.get("build"), key, config_path)?;
    let image = match object.get("image") {
        Some(value) => value
            .as_str()
            .ok_or_else(|| {
                eprintln!("error: cladding.json invalid field '{key}.image' (expected string)");
                eprintln!("file: {}", config_path.display());
                Error::message("invalid cladding.json")
            })?
            .to_string(),
        None if build.is_some() => generated_image_tag(project_name, key),
        None => DEFAULT_COMPONENT_IMAGE.to_string(),
    };

    if image.is_empty() {
        eprintln!("error: cladding.json invalid field '{key}.image' (must not be empty)");
        eprintln!("file: {}", config_path.display());
        return Err(Error::message("invalid cladding.json"));
    }

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
            build,
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
        image,
        build,
    }))
}

fn validate_component_keys(
    object: &serde_json::Map<String, serde_json::Value>,
    component_key: &str,
    config_path: &Path,
) -> Result<()> {
    let allowed = ["image", "enabled", "build"];

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

fn parse_proxy_object(
    parsed: &serde_json::Value,
    project_name: &str,
    config_path: &Path,
) -> Result<Option<ExecutionProxyConfig>> {
    let Some(raw) = parsed.get("proxy") else {
        return Ok(None);
    };
    let key = "proxy";
    let object = raw.as_object().ok_or_else(|| {
        eprintln!("error: cladding.json field 'proxy' must be an object");
        eprintln!("file: {}", config_path.display());
        Error::message("invalid cladding.json")
    })?;

    let allowed = ["image", "build"];
    for field in object.keys() {
        if !allowed.contains(&field.as_str()) {
            eprintln!("error: cladding.json unknown key: proxy.{field}");
            eprintln!("file: {}", config_path.display());
            return Err(Error::message("invalid cladding.json"));
        }
    }

    let build = parse_build_config(object.get("build"), key, config_path)?;
    let image = if let Some(value) = object.get("image") {
        let Some(image) = value.as_str() else {
            eprintln!("error: cladding.json invalid field 'proxy.image' (expected string)");
            eprintln!("file: {}", config_path.display());
            return Err(Error::message("invalid cladding.json"));
        };
        if image.is_empty() {
            eprintln!("error: cladding.json invalid field 'proxy.image' (must not be empty)");
            eprintln!("file: {}", config_path.display());
            return Err(Error::message("invalid cladding.json"));
        }
        image.to_string()
    } else if build.is_some() {
        generated_image_tag(project_name, key)
    } else {
        DEFAULT_PROXY_IMAGE.to_string()
    };

    Ok(Some(ExecutionProxyConfig { image, build }))
}

fn parse_build_config(
    raw: Option<&serde_json::Value>,
    component_key: &str,
    config_path: &Path,
) -> Result<Option<ImageBuildConfig>> {
    let Some(raw) = raw else {
        return Ok(None);
    };
    let field = format!("{component_key}.build");
    let Some(object) = raw.as_object() else {
        eprintln!("error: cladding.json field '{field}' must be an object");
        eprintln!("file: {}", config_path.display());
        return Err(Error::message("invalid cladding.json"));
    };

    for key in object.keys() {
        if !["containerfile", "context", "args"].contains(&key.as_str()) {
            eprintln!("error: cladding.json unknown key: {field}.{key}");
            eprintln!("file: {}", config_path.display());
            return Err(Error::message("invalid cladding.json"));
        }
    }

    let containerfile = object
        .get("containerfile")
        .and_then(serde_json::Value::as_str)
        .filter(|value| !value.is_empty())
        .ok_or_else(|| {
            eprintln!(
                "error: cladding.json field '{field}.containerfile' must be a non-empty string"
            );
            eprintln!("file: {}", config_path.display());
            Error::message("invalid cladding.json")
        })?;
    let context = match object.get("context") {
        Some(value) => value
            .as_str()
            .filter(|value| !value.is_empty())
            .ok_or_else(|| {
                eprintln!(
                    "error: cladding.json field '{field}.context' must be a non-empty string"
                );
                eprintln!("file: {}", config_path.display());
                Error::message("invalid cladding.json")
            })?,
        None => ".",
    };

    let mut args = BTreeMap::new();
    if let Some(value) = object.get("args") {
        let Some(values) = value.as_object() else {
            eprintln!("error: cladding.json field '{field}.args' must be an object of strings");
            eprintln!("file: {}", config_path.display());
            return Err(Error::message("invalid cladding.json"));
        };
        for (name, value) in values {
            let Some(value) = value.as_str() else {
                eprintln!("error: cladding.json field '{field}.args.{name}' must be a string");
                eprintln!("file: {}", config_path.display());
                return Err(Error::message("invalid cladding.json"));
            };
            if name.is_empty() {
                eprintln!(
                    "error: cladding.json field '{field}.args.{name}' must not have an empty name"
                );
                eprintln!("file: {}", config_path.display());
                return Err(Error::message("invalid cladding.json"));
            }
            args.insert(name.clone(), value.to_string());
        }
    }

    let base = config_path.parent().unwrap_or(Path::new("."));
    Ok(Some(ImageBuildConfig {
        containerfile: resolve_path(base, Path::new(containerfile)),
        context: resolve_path(base, Path::new(context)),
        args,
    }))
}

fn generated_image_tag(project_name: &str, component_key: &str) -> String {
    format!("localhost/cladding-{project_name}-{component_key}:latest")
}

fn resolve_path(base: &Path, path: &Path) -> PathBuf {
    if path.is_absolute() {
        normalize_path(path)
    } else {
        let base = if base.is_absolute() {
            base.to_path_buf()
        } else {
            std::env::current_dir()
                .unwrap_or_else(|_| PathBuf::from("."))
                .join(base)
        };
        normalize_path(&base.join(path))
    }
}

fn normalize_path(path: &Path) -> PathBuf {
    let mut normalized = PathBuf::new();
    for component in path.components() {
        match component {
            std::path::Component::CurDir => {}
            std::path::Component::ParentDir => {
                if normalized.file_name().is_some() {
                    normalized.pop();
                } else if !normalized.has_root() {
                    normalized.push("..");
                }
            }
            other => normalized.push(other.as_os_str()),
        }
    }
    normalized
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
        assert_eq!(config.proxy_image(), DEFAULT_PROXY_IMAGE);
        assert!(config.agent.build.is_none());
    }

    #[test]
    fn load_cladding_config_resolves_build_paths_and_generated_image_tags() {
        let temp = create_temp_dir("build-config");
        fs::write(
            temp.join("cladding.json"),
            r#"{
  "name": "demo",
  "agent": {
    "build": {
      "containerfile": "containers/agent.Containerfile",
      "context": "..",
      "args": { "FEATURE": "enabled" }
    }
  },
  "nw_sandbox": {
    "enabled": true,
    "build": { "containerfile": "containers/nw.Containerfile" }
  },
  "fs_sandbox": {
    "build": { "containerfile": "containers/fs.Containerfile", "context": "../src" }
  },
  "proxy": {
    "image": "localhost/custom-proxy:latest",
    "build": { "containerfile": "containers/proxy.Containerfile", "context": "../proxy" }
  }
}"#,
        )
        .unwrap();

        let config = load_cladding_config_v2(&temp).unwrap();
        assert_eq!(config.agent.image, "localhost/cladding-demo-agent:latest");
        let agent_build = config.agent.build.as_ref().unwrap();
        assert_eq!(
            agent_build.containerfile,
            temp.join("containers/agent.Containerfile")
        );
        assert_eq!(agent_build.context, temp.parent().unwrap());
        assert_eq!(agent_build.args.get("FEATURE").unwrap(), "enabled");
        assert_eq!(
            config.nw_sandbox.as_ref().unwrap().image,
            "localhost/cladding-demo-nw_sandbox:latest"
        );
        assert_eq!(
            config
                .fs_sandbox
                .as_ref()
                .unwrap()
                .build
                .as_ref()
                .unwrap()
                .context,
            temp.parent().unwrap().join("src")
        );
        let proxy = config.proxy.as_ref().unwrap();
        assert_eq!(proxy.image, "localhost/custom-proxy:latest");
        assert_eq!(
            proxy.build.as_ref().unwrap().containerfile,
            temp.join("containers/proxy.Containerfile")
        );
    }

    #[test]
    fn load_cladding_config_accepts_ordinary_build_argument_names() {
        let temp = create_temp_dir("ordinary-build-arg");
        fs::write(
            temp.join("cladding.json"),
            r#"{
  "name": "demo",
  "agent": {
    "build": {
      "containerfile": "Containerfile",
      "args": { "API_TOKEN": "ordinary-value", "AWS_ACCESS_KEY_ID": "ordinary-value" }
    }
  }
}"#,
        )
        .unwrap();

        let config = load_cladding_config_v2(&temp).unwrap();
        let args = &config.agent.build.as_ref().unwrap().args;
        assert_eq!(args.get("API_TOKEN").unwrap(), "ordinary-value");
        assert_eq!(args.get("AWS_ACCESS_KEY_ID").unwrap(), "ordinary-value");
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
