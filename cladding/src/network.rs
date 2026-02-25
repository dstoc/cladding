use crate::error::{Error, Result};

#[derive(Debug, Clone)]
pub struct NetworkSettings {
    pub network: String,
    pub network_subnet: String,
    pub proxy_ip: String,
    pub sandbox_ip: String,
    pub cli_ip: String,
    pub proxy_pod_name: String,
    pub sandbox_pod_name: String,
    pub cli_pod_name: String,
}

pub fn resolve_network_settings(name: &str, subnet: &str) -> Result<NetworkSettings> {
    let subnet = subnet.trim();
    let (subnet_ip, subnet_prefix) = match subnet.split_once('/') {
        Some((ip, prefix)) if !ip.is_empty() && !prefix.is_empty() => (ip, prefix),
        _ => {
            eprintln!(
                "error: config key 'subnet' must be in CIDR notation (example: 10.90.0.0/24)"
            );
            return Err(Error::message("invalid subnet format"));
        }
    };

    let subnet_prefix: u8 = subnet_prefix.parse().map_err(|_| {
        eprintln!("error: subnet prefix must be numeric: {}", subnet);
        Error::message("invalid subnet prefix")
    })?;

    if subnet_prefix > 32 {
        eprintln!("error: subnet prefix out of range (0-32): {}", subnet);
        return Err(Error::message("invalid subnet prefix"));
    }

    let subnet_ip_int = ipv4_to_int(subnet_ip).ok_or_else(|| {
        eprintln!("error: invalid IPv4 subnet address: {}", subnet);
        Error::message("invalid subnet ip")
    })?;

    let subnet_mask_int = if subnet_prefix == 0 {
        0
    } else {
        (!0u32) << (32 - subnet_prefix)
    };
    let subnet_network_int = subnet_ip_int & subnet_mask_int;
    let subnet_broadcast_int = subnet_network_int | (!subnet_mask_int);

    let proxy_ip_int = subnet_network_int + 2;
    let sandbox_ip_int = subnet_network_int + 3;
    let cli_ip_int = subnet_network_int + 4;

    if cli_ip_int >= subnet_broadcast_int {
        eprintln!(
            "error: subnet too small, need usable IPs for gateway + 3 pods: {}",
            subnet
        );
        return Err(Error::message("subnet too small"));
    }

    let network = format!("{}_cladding_net", name);
    let network_subnet = format!("{}/{}", int_to_ipv4(subnet_network_int), subnet_prefix);
    let proxy_ip = int_to_ipv4(proxy_ip_int);
    let sandbox_ip = int_to_ipv4(sandbox_ip_int);
    let cli_ip = int_to_ipv4(cli_ip_int);

    Ok(NetworkSettings {
        network,
        network_subnet,
        proxy_ip,
        sandbox_ip,
        cli_ip,
        proxy_pod_name: format!("{}-proxy-pod", name),
        sandbox_pod_name: format!("{}-sandbox-pod", name),
        cli_pod_name: format!("{}-cli-pod", name),
    })
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
