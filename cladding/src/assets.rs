use crate::error::Result;
use crate::fs_utils::set_permissions;
use anyhow::Context as _;
use include_dir::{include_dir, Dir};
use std::fs;
use std::path::Path;

const CONTAINERFILE_CLADDING: &str = include_str!("../../Containerfile.cladding");
const PODS_YAML: &str = include_str!("../../pods.yaml");

static CONFIG_DIR: Dir<'_> = include_dir!("$CARGO_MANIFEST_DIR/../config-template");
static SCRIPTS_DIR: Dir<'_> = include_dir!("$CARGO_MANIFEST_DIR/../scripts");

pub const CONFIG_TOP_LEVEL: &[&str] = &[
    "cli_domains.lst",
    "cli_host_ports.lst",
    "sandbox_domains.lst",
    "squid.conf",
    "sandbox_commands",
];

pub fn materialize_config(base_dir: &Path) -> Result<()> {
    materialize_dir(base_dir, &CONFIG_DIR)
}

pub fn materialize_scripts(base_dir: &Path) -> Result<()> {
    materialize_dir(base_dir, &SCRIPTS_DIR)
}

fn materialize_dir(base_dir: &Path, dir: &Dir<'_>) -> Result<()> {
    for entry in dir.files() {
        let rel_path = entry.path();
        let target = base_dir.join(rel_path);
        if target.exists() {
            continue;
        }
        if let Some(parent) = target.parent() {
            fs::create_dir_all(parent)
                .with_context(|| format!("failed to create {}", parent.display()))?;
        }
        fs::write(&target, entry.contents())
            .with_context(|| format!("failed to write {}", target.display()))?;
        let mode = if target.extension().and_then(|s| s.to_str()) == Some("sh") {
            0o755
        } else {
            0o644
        };
        set_permissions(&target, mode)?;
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
