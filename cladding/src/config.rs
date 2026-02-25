use crate::error::{Error, Result};
use crate::network::is_ipv4_cidr;
use crate::podman::{list_podman_ipv4_subnets, podman_network_exists, podman_required};
use anyhow::Context as _;
use std::env;
use std::ffi::OsStr;
use std::fs;
use std::path::Path;

#[derive(Debug, Clone)]
pub struct Config {
    pub name: String,
    pub subnet: String,
    pub sandbox_image: String,
    pub cli_image: String,
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

    let name = get_config_string(&parsed, "name")?;
    let subnet = get_config_string(&parsed, "subnet")?;
    let sandbox_image = get_config_string(&parsed, "sandbox_image")?;
    let cli_image = get_config_string(&parsed, "cli_image")?;

    if !is_lowercase_alnum(&name) {
        eprintln!("error: config key 'name' must be lowercase alphanumeric ([a-z0-9]+)");
        return Err(Error::message("invalid name"));
    }

    if !is_ipv4_cidr(&subnet) {
        // subnet-specific error gets printed in network::resolve_network_settings
    }

    Ok(Config {
        name,
        subnet,
        sandbox_image,
        cli_image,
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

fn get_config_string(parsed: &serde_json::Value, key: &str) -> Result<String> {
    parsed
        .get(key)
        .and_then(|value| value.as_str())
        .map(|value| value.to_string())
        .ok_or_else(|| {
            eprintln!("error: cladding.json must include string key: {key}");
            Error::message("invalid cladding.json")
        })
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
