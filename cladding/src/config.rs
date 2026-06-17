use crate::error::{Error, Result};
use anyhow::Context as _;
use std::collections::HashSet;
use std::env;
use std::ffi::OsStr;
use std::fs;
use std::path::{Path, PathBuf};

pub const DEFAULT_COMPONENT_IMAGE: &str = "localhost/cladding-default:latest";

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ExecutionComponentConfig {
    pub enabled: bool,
    pub image: String,
}

#[derive(Debug, Clone)]
pub struct ExecutionConfig {
    pub name: String,
    pub use_runsc: bool,
    pub agent: ExecutionComponentConfig,
    pub nw_sandbox: Option<ExecutionComponentConfig>,
    pub fs_sandbox: Option<ExecutionComponentConfig>,
    pub mounts: Vec<ResolvedMountConfig>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum MountTarget {
    Agent,
    NwSandbox,
    FsSandbox,
}

#[derive(Debug, Clone)]
pub struct ResolvedMountConfig {
    pub mount_path: String,
    pub host_path: Option<PathBuf>,
    pub volume: Option<String>,
    pub read_only: bool,
    pub targets: Vec<MountTarget>,
    pub ignore: bool,
}

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

pub fn write_default_cladding_config(
    name_override: Option<&str>,
    default_sandbox_image: &str,
    default_cli_image: &str,
) -> Result<String> {
    let name = if let Some(name_override) = name_override {
        normalize_cladding_name_arg(name_override)?
    } else {
        derive_cladding_name_from_pwd()?
    };

    Ok(format!(
        "{{\n  \"name\": \"{}\",\n  \"agent\": {{\n    \"image\": \"{}\"\n  }},\n  \"nw_sandbox\": {{\n    \"enabled\": true,\n    \"image\": \"{}\"\n  }}\n}}\n",
        name, default_cli_image, default_sandbox_image
    ))
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

fn parse_mounts_v2(
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
            Some(value) => Some(value.as_str().ok_or_else(|| {
                eprintln!("error: cladding.json invalid field 'mounts[{index}].volume' (expected string)");
                eprintln!("file: {}", config_path.display());
                Error::message("invalid cladding.json")
                })?.to_string()),
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
        return Ok(execution_config.enabled_mount_targets());
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

impl MountTarget {
    fn from_config_name(name: &str) -> Option<Self> {
        match name {
            "agent" => Some(Self::Agent),
            "nw-sandbox" => Some(Self::NwSandbox),
            "fs-sandbox" => Some(Self::FsSandbox),
            _ => None,
        }
    }

    fn config_key(self) -> &'static str {
        match self {
            Self::Agent => "agent",
            Self::NwSandbox => "nw_sandbox",
            Self::FsSandbox => "fs_sandbox",
        }
    }

    fn as_str(self) -> &'static str {
        match self {
            Self::Agent => "agent",
            Self::NwSandbox => "nw-sandbox",
            Self::FsSandbox => "fs-sandbox",
        }
    }

    fn is_enabled(self, config: &ExecutionConfig) -> bool {
        match self {
            Self::Agent => true,
            Self::NwSandbox => config
                .nw_sandbox
                .as_ref()
                .map(|component| component.enabled)
                .unwrap_or(false),
            Self::FsSandbox => config
                .fs_sandbox
                .as_ref()
                .map(|component| component.enabled)
                .unwrap_or(false),
        }
    }
}

impl ExecutionConfig {
    pub fn agent_image(&self) -> &str {
        &self.agent.image
    }

    pub fn nw_sandbox_image(&self) -> &str {
        self.nw_sandbox
            .as_ref()
            .map(|component| component.image.as_str())
            .unwrap_or(DEFAULT_COMPONENT_IMAGE)
    }

    pub fn fs_sandbox_image(&self) -> &str {
        self.fs_sandbox
            .as_ref()
            .map(|component| component.image.as_str())
            .unwrap_or(DEFAULT_COMPONENT_IMAGE)
    }

    pub fn nw_sandbox_enabled(&self) -> bool {
        self.nw_sandbox
            .as_ref()
            .map(|component| component.enabled)
            .unwrap_or(false)
    }

    pub fn fs_sandbox_enabled(&self) -> bool {
        self.fs_sandbox
            .as_ref()
            .map(|component| component.enabled)
            .unwrap_or(false)
    }

    pub fn enabled_mount_targets(&self) -> Vec<MountTarget> {
        let mut targets = vec![MountTarget::Agent];
        if self.nw_sandbox_enabled() {
            targets.push(MountTarget::NwSandbox);
        }
        if self.fs_sandbox_enabled() {
            targets.push(MountTarget::FsSandbox);
        }
        targets
    }
}

fn ensure_absolute_mount_path(config_path: &Path, field: &str, mount_path: &str) -> Result<()> {
    if Path::new(mount_path).is_absolute() {
        return Ok(());
    }
    eprintln!("error: cladding.json invalid field '{field}' (mount path must be absolute)");
    eprintln!("file: {}", config_path.display());
    Err(Error::message("invalid cladding.json"))
}

fn is_lowercase_alnum(name: &str) -> bool {
    !name.is_empty()
        && name
            .chars()
            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit())
}

fn derive_cladding_name_from_pwd() -> Result<String> {
    let cwd = env::current_dir().with_context(|| "failed to determine current directory")?;
    let raw_name = cwd.file_name().and_then(OsStr::to_str).unwrap_or("");
    let name = raw_name
        .chars()
        .filter(|c| c.is_ascii_alphanumeric())
        .map(|c| c.to_ascii_lowercase())
        .collect::<String>();

    if name.is_empty() {
        eprintln!(
            "error: could not derive an alphanumeric name from directory: {}",
            cwd.display()
        );
        return Err(Error::message("could not derive name"));
    }

    Ok(name)
}

fn normalize_cladding_name_arg(name_arg: &str) -> Result<String> {
    let name = name_arg.to_ascii_lowercase();
    if name.is_empty() || !name.chars().all(|c| c.is_ascii_alphanumeric()) {
        eprintln!("error: init name must be alphanumeric ([a-zA-Z0-9]+)");
        return Err(Error::message("invalid init name"));
    }
    Ok(name)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use std::sync::atomic::{AtomicUsize, Ordering};

    static TEMP_DIR_COUNTER: AtomicUsize = AtomicUsize::new(0);

    #[test]
    fn normalize_init_name() {
        assert_eq!(
            normalize_cladding_name_arg("MyProject").unwrap(),
            "myproject"
        );
        assert!(normalize_cladding_name_arg("bad-name").is_err());
    }

    #[test]
    fn write_default_cladding_config_uses_component_objects() {
        let generated =
            write_default_cladding_config(Some("Demo"), "sandbox:image", "agent:image").unwrap();
        assert_eq!(
            generated,
            "{\n  \"name\": \"demo\",\n  \"agent\": {\n    \"image\": \"agent:image\"\n  },\n  \"nw_sandbox\": {\n    \"enabled\": true,\n    \"image\": \"sandbox:image\"\n  }\n}\n"
        );
    }

    #[test]
    fn load_cladding_config_parses_component_objects_and_mount_targets() {
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
  },
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
    }
  ]
}"#,
        )
        .unwrap();

        let config = load_cladding_config_v2(&temp).unwrap();
        assert!(!config.use_runsc);
        assert_eq!(config.agent_image(), "agent:image");
        assert_eq!(config.nw_sandbox_image(), "sandbox:image");
        assert!(config.nw_sandbox_enabled());
        assert!(config.fs_sandbox_enabled());
        assert_eq!(
            config.enabled_mount_targets(),
            vec![
                MountTarget::Agent,
                MountTarget::NwSandbox,
                MountTarget::FsSandbox
            ]
        );
        assert_eq!(config.mounts.len(), 2);
        assert_eq!(
            config.mounts[0].targets,
            vec![MountTarget::Agent, MountTarget::NwSandbox]
        );
        assert_eq!(config.mounts[1].targets, vec![MountTarget::FsSandbox]);
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

    #[test]
    fn load_cladding_config_rejects_disabled_mount_target() {
        let temp = create_temp_dir("disabled-mount-target");
        fs::write(
            temp.join("cladding.json"),
            r#"{
  "name": "demo",
  "agent": {
    "image": "agent:image"
  },
  "mounts": [
    {
      "mount": "/workspace",
      "hostPath": "./one",
      "targets": ["fs-sandbox"]
    }
  ]
}"#,
        )
        .unwrap();

        assert!(load_cladding_config_v2(&temp).is_err());
    }

    #[test]
    fn load_cladding_config_rejects_legacy_mount_target_keys() {
        let temp = create_temp_dir("legacy-mount-target");
        fs::write(
            temp.join("cladding.json"),
            r#"{
  "name": "demo",
  "agent": {
    "image": "agent:image"
  },
  "mounts": [
    {
      "mount": "/workspace",
      "hostPath": "./one",
      "nwSandboxOnly": true
    }
  ]
}"#,
        )
        .unwrap();

        assert!(load_cladding_config_v2(&temp).is_err());
    }

    #[test]
    fn load_cladding_config_rejects_duplicate_mount_paths_per_target() {
        let temp = create_temp_dir("duplicate-mounts");
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
}"#,
        )
        .unwrap();

        assert!(load_cladding_config_v2(&temp).is_ok());

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
