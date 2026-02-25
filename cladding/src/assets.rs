use crate::error::Result;
use crate::fs_utils::set_permissions;
use anyhow::Context as _;
use std::fs;
use std::path::Path;

const CONTAINERFILE_CLADDING: &str = include_str!("../../Containerfile.cladding");
const PODS_YAML: &str = include_str!("../../pods.yaml");

pub struct EmbeddedFile {
    pub path: &'static str,
    pub contents: &'static [u8],
    pub mode: u32,
}

pub const EMBEDDED_CONFIG_FILES: &[EmbeddedFile] = &[
    EmbeddedFile {
        path: "cli_domains.lst",
        contents: include_bytes!("../../config-template/cli_domains.lst"),
        mode: 0o644,
    },
    EmbeddedFile {
        path: "cli_host_ports.lst",
        contents: include_bytes!("../../config-template/cli_host_ports.lst"),
        mode: 0o644,
    },
    EmbeddedFile {
        path: "sandbox_domains.lst",
        contents: include_bytes!("../../config-template/sandbox_domains.lst"),
        mode: 0o644,
    },
    EmbeddedFile {
        path: "squid.conf",
        contents: include_bytes!("../../config-template/squid.conf"),
        mode: 0o644,
    },
    EmbeddedFile {
        path: "sandbox_commands/main.rego",
        contents: include_bytes!("../../config-template/sandbox_commands/main.rego"),
        mode: 0o644,
    },
    EmbeddedFile {
        path: "sandbox_commands/curl.rego",
        contents: include_bytes!("../../config-template/sandbox_commands/curl.rego"),
        mode: 0o644,
    },
];

pub const EMBEDDED_SCRIPTS: &[EmbeddedFile] = &[
    EmbeddedFile {
        path: "jail_cli.sh",
        contents: include_bytes!("../../scripts/jail_cli.sh"),
        mode: 0o755,
    },
    EmbeddedFile {
        path: "jail_sandbox.sh",
        contents: include_bytes!("../../scripts/jail_sandbox.sh"),
        mode: 0o755,
    },
    EmbeddedFile {
        path: "proxy_startup.sh",
        contents: include_bytes!("../../scripts/proxy_startup.sh"),
        mode: 0o755,
    },
];

pub const CONFIG_TOP_LEVEL: &[&str] = &[
    "cli_domains.lst",
    "cli_host_ports.lst",
    "sandbox_domains.lst",
    "squid.conf",
    "sandbox_commands",
];

pub fn materialize_embedded_files(base_dir: &Path, files: &[EmbeddedFile]) -> Result<()> {
    for file in files {
        let target = base_dir.join(file.path);
        if target.exists() {
            continue;
        }
        if let Some(parent) = target.parent() {
            fs::create_dir_all(parent)
                .with_context(|| format!("failed to create {}", parent.display()))?;
        }
        fs::write(&target, file.contents)
            .with_context(|| format!("failed to write {}", target.display()))?;
        set_permissions(&target, file.mode)?;
    }

    Ok(())
}

pub fn containerfile() -> &'static str {
    CONTAINERFILE_CLADDING
}

pub fn render_pods_yaml(
    project_root: &Path,
    config_sandbox_image: &str,
    config_cli_image: &str,
    proxy_pod_name: &str,
    sandbox_pod_name: &str,
    cli_pod_name: &str,
    proxy_ip: &str,
    sandbox_ip: &str,
    cli_ip: &str,
) -> String {
    PODS_YAML
        .replace("PROJECT_ROOT", &project_root.display().to_string())
        .replace("CLADDING_ROOT", &project_root.display().to_string())
        .replace("REPLACE_PROXY_POD_NAME", proxy_pod_name)
        .replace("REPLACE_SANDBOX_POD_NAME", sandbox_pod_name)
        .replace("REPLACE_CLI_POD_NAME", cli_pod_name)
        .replace("REPLACE_SANDBOX_IMAGE", config_sandbox_image)
        .replace("REPLACE_CLI_IMAGE", config_cli_image)
        .replace("REPLACE_PROXY_IP", proxy_ip)
        .replace("REPLACE_SANDBOX_IP", sandbox_ip)
        .replace("REPLACE_CLI_IP", cli_ip)
}
