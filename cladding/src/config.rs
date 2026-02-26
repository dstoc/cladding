use crate::error::{Error, Result};
use crate::network::is_ipv4_cidr;
use crate::podman::{list_podman_ipv4_subnets, podman_network_exists, podman_required};
use anyhow::Context as _;
use std::env;
use std::ffi::OsStr;
use std::fs;
use std::collections::HashSet;
use std::path::{Path, PathBuf};

#[derive(Debug, Clone)]
pub struct Config {
    pub name: String,
    pub subnet: String,
    pub sandbox_image: String,
    pub cli_image: String,
    pub mounts: Vec<MountConfig>,
}

#[derive(Debug, Clone)]
pub struct MountConfig {
    pub mount_path: String,
    pub host_path: Option<PathBuf>,
    pub volume: Option<String>,
    pub read_only: bool,
}

pub fn load_cladding_config(project_root: &Path) -> Result<Config> {
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

    let name = get_config_string(&parsed, "name", &config_path)?;
    let subnet = get_config_string(&parsed, "subnet", &config_path)?;
    let sandbox_image = get_config_string(&parsed, "sandbox_image", &config_path)?;
    let cli_image = get_config_string(&parsed, "cli_image", &config_path)?;
    let mut used_mount_paths = HashSet::new();
    let mounts = parse_mounts(project_root, &parsed, &config_path, &mut used_mount_paths)?;

    if !is_lowercase_alnum(&name) {
        eprintln!("error: config key 'name' must be lowercase alphanumeric ([a-z0-9]+)");
        eprintln!("file: {}", config_path.display());
        return Err(Error::message("invalid name"));
    }

    if !is_ipv4_cidr(&subnet) {
        eprintln!(
            "error: config key 'subnet' must be in CIDR notation (example: 10.90.0.0/24)"
        );
        eprintln!("file: {}", config_path.display());
        return Err(Error::message("invalid subnet format"));
    }

    Ok(Config {
        name,
        subnet,
        sandbox_image,
        cli_image,
        mounts,
    })
}

pub fn write_default_cladding_config(
    name_override: Option<&str>,
    default_sandbox_image: &str,
    default_cli_image: &str,
) -> Result<String> {
    podman_required("podman (required for cladding init to choose name/subnet)")?;

    let name = if let Some(name_override) = name_override {
        normalize_cladding_name_arg(name_override)?
    } else {
        derive_cladding_name_from_pwd()?
    };

    let network_name = format!("{}_cladding_net", name);
    match podman_network_exists(&network_name)? {
        Some(true) => {
            eprintln!("error: network already exists for generated name: {network_name}");
            eprintln!(
                "hint: run cladding init from a different directory name, or remove the existing network"
            );
            return Err(Error::message("network already exists"));
        }
        Some(false) => {}
        None => {
            eprintln!("error: failed to check existing networks via podman");
            return Err(Error::message("podman network exists failed"));
        }
    }

    let subnet = pick_available_subnet().map_err(|code| {
        match code {
            1 => eprintln!("error: failed to inspect existing network subnets via podman"),
            2 => eprintln!(
                "error: could not find an unused subnet in 10.90.0.0/16 (/24 slices)"
            ),
            _ => eprintln!("error: unexpected failure while selecting subnet"),
        }
        Error::message("failed to select subnet")
    })?;

    Ok(format!(
        "{{\n  \"sandbox_image\": \"{}\",\n  \"cli_image\": \"{}\",\n  \"name\": \"{}\",\n  \"subnet\": \"{}\"\n}}\n",
        default_sandbox_image, default_cli_image, name, subnet
    ))
}

fn get_config_string(
    parsed: &serde_json::Value,
    key: &str,
    config_path: &Path,
) -> Result<String> {
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

fn parse_mounts(
    project_root: &Path,
    parsed: &serde_json::Value,
    config_path: &Path,
    used_mount_paths: &mut HashSet<String>,
) -> Result<Vec<MountConfig>> {
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

        if !used_mount_paths.insert(mount_path.to_string()) {
            eprintln!(
                "error: cladding.json duplicate mount path '{mount_path}' in mounts"
            );
            eprintln!("file: {}", config_path.display());
            return Err(Error::message("duplicate mount path"));
        }

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

        if volume.is_some() && read_only {
            eprintln!(
                "error: cladding.json invalid field 'mounts[{index}].readOnly' (readOnly not supported for volume mounts)"
            );
            eprintln!("file: {}", config_path.display());
            return Err(Error::message("invalid cladding.json"));
        }

        let read_only = if host_path.is_none() && volume.is_none() {
            true
        } else {
            read_only
        };

        mounts.push(MountConfig {
            mount_path: mount_path.to_string(),
            host_path,
            volume,
            read_only,
        });
    }

    Ok(mounts)
}

fn ensure_absolute_mount_path(
    config_path: &Path,
    field: &str,
    mount_path: &str,
) -> Result<()> {
    if Path::new(mount_path).is_absolute() {
        return Ok(());
    }
    eprintln!(
        "error: cladding.json invalid field '{field}' (mount path must be absolute)"
    );
    eprintln!("file: {}", config_path.display());
    Err(Error::message("invalid cladding.json"))
}

fn is_lowercase_alnum(name: &str) -> bool {
    !name.is_empty() && name.chars().all(|c| c.is_ascii_lowercase() || c.is_ascii_digit())
}

fn derive_cladding_name_from_pwd() -> Result<String> {
    let cwd = env::current_dir().with_context(|| "failed to determine current directory")?;
    let raw_name = cwd
        .file_name()
        .and_then(OsStr::to_str)
        .unwrap_or("");
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

fn pick_available_subnet() -> std::result::Result<String, i32> {
    let used_subnets = match list_podman_ipv4_subnets() {
        Ok(subnets) => subnets,
        Err(_) => return Err(1),
    };
    for i in 0..=255 {
        let candidate = format!("10.90.{i}.0/24");
        if !used_subnets.iter().any(|subnet| subnet == &candidate) {
            return Ok(candidate);
        }
    }

    Err(2)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn normalize_init_name() {
        assert_eq!(normalize_cladding_name_arg("MyProject").unwrap(), "myproject");
        assert!(normalize_cladding_name_arg("bad-name").is_err());
    }
}
