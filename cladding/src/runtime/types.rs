use crate::config::{ExecutionConfig, MountTarget};
use std::collections::BTreeMap;
use std::path::PathBuf;

#[derive(Debug, Clone)]
pub struct RuntimeSpec {
    pub project_name: String,
    pub project_root: PathBuf,
    pub use_runsc: bool,
    pub proxy: RuntimePod,
    pub agent: RuntimePod,
    pub nw_sandbox: Option<RuntimePod>,
    pub fs_sandbox: Option<RuntimePod>,
}

#[derive(Debug, Clone)]
pub(super) struct RuntimeNames {
    pub(super) proxy_name: String,
    pub(super) agent_name: String,
    pub(super) nw_sandbox_name: Option<String>,
    pub(super) fs_sandbox_name: Option<String>,
}

impl RuntimeNames {
    pub(super) fn from_config(config: &ExecutionConfig) -> Self {
        Self {
            proxy_name: format!("{}-proxy", config.name),
            agent_name: format!("{}-agent", config.name),
            nw_sandbox_name: config
                .nw_sandbox_enabled()
                .then(|| format!("{}-nw-sandbox", config.name)),
            fs_sandbox_name: config
                .fs_sandbox_enabled()
                .then(|| format!("{}-fs-sandbox", config.name)),
        }
    }
}

#[derive(Debug, Clone)]
pub struct RuntimePod {
    pub name: String,
    pub placement: RuntimePlacement,
    pub use_runsc: bool,
    pub labels: BTreeMap<String, String>,
    pub network_name: String,
    pub containers: Vec<RuntimeContainer>,
    pub userns_keep_id: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RuntimePlacement {
    Pod,
    Standalone,
}

#[derive(Debug, Clone)]
pub struct RuntimeContainer {
    pub name: String,
    pub image: String,
    pub command: Vec<String>,
    pub workdir: Option<String>,
    pub env: Vec<RuntimeEnvVar>,
    pub mounts: Vec<RuntimeMount>,
    pub ports: Vec<u16>,
    pub stdin: bool,
    pub tty: bool,
}

#[derive(Debug, Clone)]
pub struct RuntimeEnvVar {
    pub name: String,
    pub value: String,
}

#[derive(Debug, Clone)]
pub struct RuntimeMount {
    pub mount_path: String,
    pub read_only: bool,
    pub source: RuntimeMountSource,
}

#[derive(Debug, Clone)]
pub struct RuntimeCustomMount {
    pub mount_path: String,
    pub read_only: bool,
    pub source: RuntimeMountSource,
    pub targets: Vec<MountTarget>,
    pub ignore: bool,
}

#[derive(Debug, Clone)]
pub enum RuntimeMountSource {
    HostPath { path: PathBuf },
    NamedVolume { claim_name: String },
    GeneratedEmptyMask { path: PathBuf },
    EmptyDir,
}
