use crate::config::ExecutionConfig;
use crate::error::{Error, Result};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ComponentNetworkSettings {
    pub ip: String,
    pub name: String,
}

#[derive(Debug, Clone)]
pub struct NetworkSettings {
    pub pool_index: u8,
    pub network: String,
    pub network_subnet: String,
    pub proxy_ip: String,
    pub proxy_name: String,
    pub agent_ip: String,
    pub agent_name: String,
    pub nw_sandbox: Option<ComponentNetworkSettings>,
    pub fs_sandbox: Option<ComponentNetworkSettings>,
}

pub fn resolve_network_settings_for_config(
    name: &str,
    pool_index: u8,
    config: &ExecutionConfig,
) -> Result<NetworkSettings> {
    resolve_network_settings_internal(
        name,
        pool_index,
        config.nw_sandbox_enabled(),
        config.fs_sandbox_enabled(),
    )
}

fn resolve_network_settings_internal(
    name: &str,
    pool_index: u8,
    nw_enabled: bool,
    fs_enabled: bool,
) -> Result<NetworkSettings> {
    let network_subnet = format!("10.90.{pool_index}.0/24");
    let network_base = ipv4_to_int(&format!("10.90.{pool_index}.0"))
        .ok_or_else(|| Error::message("invalid generated network"))?;

    let proxy = ComponentNetworkSettings {
        ip: int_to_ipv4(network_base + 2),
        name: format!("{}-proxy", name),
    };
    let nw_sandbox = nw_enabled.then(|| ComponentNetworkSettings {
        ip: int_to_ipv4(network_base + 3),
        name: format!("{}-nw-sandbox", name),
    });
    let fs_sandbox = fs_enabled.then(|| ComponentNetworkSettings {
        ip: int_to_ipv4(network_base + 4),
        name: format!("{}-fs-sandbox", name),
    });
    let agent = ComponentNetworkSettings {
        ip: int_to_ipv4(network_base + 5),
        name: format!("{}-agent", name),
    };

    Ok(NetworkSettings {
        pool_index,
        network: cladding_pool_network_name(pool_index),
        network_subnet,
        proxy_ip: proxy.ip.clone(),
        proxy_name: proxy.name.clone(),
        agent_ip: agent.ip.clone(),
        agent_name: agent.name.clone(),
        nw_sandbox,
        fs_sandbox,
    })
}

pub fn cladding_pool_network_name(pool_index: u8) -> String {
    format!("cladding-{pool_index}")
}

pub fn parse_cladding_pool_index(network_name: &str) -> Option<u8> {
    let suffix = network_name.strip_prefix("cladding-")?;
    suffix.parse::<u8>().ok()
}

pub fn is_ipv4_cidr(value: &str) -> bool {
    let (ip, prefix) = match value.split_once('/') {
        Some(parts) => parts,
        None => return false,
    };
    if prefix.parse::<u8>().ok().filter(|p| *p <= 32).is_none() {
        return false;
    }
    ipv4_to_int(ip).is_some()
}

pub fn ipv4_to_int(ip: &str) -> Option<u32> {
    let mut parts = ip.split('.');
    let a = parts.next()?.parse::<u8>().ok()?;
    let b = parts.next()?.parse::<u8>().ok()?;
    let c = parts.next()?.parse::<u8>().ok()?;
    let d = parts.next()?.parse::<u8>().ok()?;
    if parts.next().is_some() {
        return None;
    }

    Some(((a as u32) << 24) | ((b as u32) << 16) | ((c as u32) << 8) | (d as u32))
}

pub fn int_to_ipv4(value: u32) -> String {
    format!(
        "{}.{}.{}.{}",
        (value >> 24) & 0xff,
        (value >> 16) & 0xff,
        (value >> 8) & 0xff,
        value & 0xff
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::{ExecutionComponentConfig, ExecutionConfig};

    #[test]
    fn ipv4_roundtrip() {
        let ip = "10.90.12.34";
        let int = ipv4_to_int(ip).expect("parse ip");
        assert_eq!(int_to_ipv4(int), ip);
    }

    #[test]
    fn resolve_network_settings_basic() {
        let config = ExecutionConfig {
            name: "demo".to_string(),
            agent: ExecutionComponentConfig {
                enabled: true,
                image: "agent:image".to_string(),
            },
            nw_sandbox: Some(ExecutionComponentConfig {
                enabled: true,
                image: "sandbox:image".to_string(),
            }),
            fs_sandbox: None,
            mounts: Vec::new(),
        };

        let settings = resolve_network_settings_for_config("demo", 5, &config).unwrap();
        assert_eq!(settings.network, "cladding-5");
        assert_eq!(settings.network_subnet, "10.90.5.0/24");
        assert_eq!(settings.proxy_ip, "10.90.5.2");
        assert_eq!(settings.agent_ip, "10.90.5.5");
        assert_eq!(settings.proxy_name, "demo-proxy");
        assert_eq!(settings.agent_name, "demo-agent");
        assert!(settings.nw_sandbox.is_some());
        assert!(settings.fs_sandbox.is_none());
    }

    #[test]
    fn resolve_network_settings_for_config_reflects_optional_components() {
        let config = ExecutionConfig {
            name: "demo".to_string(),
            agent: ExecutionComponentConfig {
                enabled: true,
                image: "agent:image".to_string(),
            },
            nw_sandbox: None,
            fs_sandbox: Some(ExecutionComponentConfig {
                enabled: true,
                image: "fs:image".to_string(),
            }),
            mounts: Vec::new(),
        };

        let settings = resolve_network_settings_for_config("demo", 7, &config).unwrap();
        assert_eq!(settings.proxy_ip, "10.90.7.2");
        assert_eq!(settings.agent_ip, "10.90.7.5");
        assert!(settings.nw_sandbox.is_none());
        assert_eq!(
            settings.fs_sandbox.as_ref().expect("fs sandbox").ip,
            "10.90.7.4"
        );
    }

    #[test]
    fn parse_pool_index() {
        assert_eq!(parse_cladding_pool_index("cladding-0"), Some(0));
        assert_eq!(parse_cladding_pool_index("cladding-255"), Some(255));
        assert_eq!(parse_cladding_pool_index("cladding-256"), None);
        assert_eq!(parse_cladding_pool_index("demo_cladding_net"), None);
    }
}
