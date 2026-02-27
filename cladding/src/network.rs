use crate::error::{Error, Result};

#[derive(Debug, Clone)]
pub struct NetworkSettings {
    pub pool_index: u8,
    pub network: String,
    pub network_subnet: String,
    pub proxy_ip: String,
    pub sandbox_ip: String,
    pub cli_ip: String,
    pub proxy_pod_name: String,
    pub sandbox_pod_name: String,
    pub cli_pod_name: String,
}

pub fn resolve_network_settings(name: &str, pool_index: u8) -> Result<NetworkSettings> {
    let network_subnet = format!("10.90.{pool_index}.0/24");
    let network_base = ipv4_to_int(&format!("10.90.{pool_index}.0"))
        .ok_or_else(|| Error::message("invalid generated network"))?;
    let proxy_ip = int_to_ipv4(network_base + 2);
    let sandbox_ip = int_to_ipv4(network_base + 3);
    let cli_ip = int_to_ipv4(network_base + 4);

    Ok(NetworkSettings {
        pool_index,
        network: cladding_pool_network_name(pool_index),
        network_subnet,
        proxy_ip,
        sandbox_ip,
        cli_ip,
        proxy_pod_name: format!("{}-proxy-pod", name),
        sandbox_pod_name: format!("{}-sandbox-pod", name),
        cli_pod_name: format!("{}-cli-pod", name),
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

    #[test]
    fn ipv4_roundtrip() {
        let ip = "10.90.12.34";
        let int = ipv4_to_int(ip).expect("parse ip");
        assert_eq!(int_to_ipv4(int), ip);
    }

    #[test]
    fn resolve_network_settings_basic() {
        let settings = resolve_network_settings("demo", 5).unwrap();
        assert_eq!(settings.network, "cladding-5");
        assert_eq!(settings.network_subnet, "10.90.5.0/24");
        assert_eq!(settings.proxy_ip, "10.90.5.2");
        assert_eq!(settings.sandbox_ip, "10.90.5.3");
        assert_eq!(settings.cli_ip, "10.90.5.4");
    }

    #[test]
    fn parse_pool_index() {
        assert_eq!(parse_cladding_pool_index("cladding-0"), Some(0));
        assert_eq!(parse_cladding_pool_index("cladding-255"), Some(255));
        assert_eq!(parse_cladding_pool_index("cladding-256"), None);
        assert_eq!(parse_cladding_pool_index("demo_cladding_net"), None);
    }
}
