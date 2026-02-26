use crate::error::Result;
use crate::fs_utils::set_permissions;
use anyhow::Context as _;
use include_dir::{Dir, include_dir};
use std::fs;
use std::path::Path;

const CONTAINERFILE_CLADDING: &str = include_str!("../../Containerfile.cladding");

static CONFIG_DIR: Dir<'_> = include_dir!("$CARGO_MANIFEST_DIR/../config-template");
static SCRIPTS_DIR: Dir<'_> = include_dir!("$CARGO_MANIFEST_DIR/../scripts");

static MCP_RUN_BIN: &[u8] = include_bytes!(concat!(env!("OUT_DIR"), "/mcp-run"));
static RUN_REMOTE_BIN: &[u8] = include_bytes!(concat!(env!("OUT_DIR"), "/run-remote"));
pub fn config_top_level_entries() -> Vec<String> {
    let mut names = std::collections::BTreeSet::new();
    for entry in CONFIG_DIR.dirs() {
        if let Some(component) = entry.path().components().next() {
            if let std::path::Component::Normal(name) = component {
                if let Some(name) = name.to_str() {
                    names.insert(name.to_string());
                }
            }
        }
    }
    for entry in CONFIG_DIR.files() {
        if let Some(component) = entry.path().components().next() {
            if let std::path::Component::Normal(name) = component {
                if let Some(name) = name.to_str() {
                    names.insert(name.to_string());
                }
            }
        }
    }
    names.into_iter().collect()
}

pub fn scripts_top_level_entries() -> Vec<String> {
    let mut names = std::collections::BTreeSet::new();
    for entry in SCRIPTS_DIR.dirs() {
        if let Some(component) = entry.path().components().next() {
            if let std::path::Component::Normal(name) = component {
                if let Some(name) = name.to_str() {
                    names.insert(name.to_string());
                }
            }
        }
    }
    for entry in SCRIPTS_DIR.files() {
        if let Some(component) = entry.path().components().next() {
            if let std::path::Component::Normal(name) = component {
                if let Some(name) = name.to_str() {
                    names.insert(name.to_string());
                }
            }
        }
    }
    names.into_iter().collect()
}

pub fn materialize_config(base_dir: &Path) -> Result<()> {
    materialize_dir(base_dir, &CONFIG_DIR)
}

pub fn materialize_scripts(base_dir: &Path) -> Result<()> {
    materialize_dir(base_dir, &SCRIPTS_DIR)
}

pub fn write_embedded_tools(bin_dir: &Path) -> Result<()> {
    let mcp_run_path = bin_dir.join("mcp-run");
    fs::write(&mcp_run_path, MCP_RUN_BIN)
        .with_context(|| format!("failed to write {}", mcp_run_path.display()))?;
    set_permissions(&mcp_run_path, 0o755)?;

    let run_remote_path = bin_dir.join("run-with-network");
    fs::write(&run_remote_path, RUN_REMOTE_BIN)
        .with_context(|| format!("failed to write {}", run_remote_path.display()))?;
    set_permissions(&run_remote_path, 0o755)?;

    Ok(())
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
