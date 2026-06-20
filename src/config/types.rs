use std::path::PathBuf;

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

impl MountTarget {
    pub(super) fn from_config_name(name: &str) -> Option<Self> {
        match name {
            "agent" => Some(Self::Agent),
            "nw-sandbox" => Some(Self::NwSandbox),
            "fs-sandbox" => Some(Self::FsSandbox),
            _ => None,
        }
    }

    pub(super) fn config_key(self) -> &'static str {
        match self {
            Self::Agent => "agent",
            Self::NwSandbox => "nw_sandbox",
            Self::FsSandbox => "fs_sandbox",
        }
    }

    pub(super) fn as_str(self) -> &'static str {
        match self {
            Self::Agent => "agent",
            Self::NwSandbox => "nw-sandbox",
            Self::FsSandbox => "fs-sandbox",
        }
    }

    pub(super) fn is_enabled(self, config: &ExecutionConfig) -> bool {
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

    pub fn default_mount_targets(&self) -> Vec<MountTarget> {
        let mut targets = vec![MountTarget::Agent];
        if self.nw_sandbox_enabled() {
            targets.push(MountTarget::NwSandbox);
        }
        targets
    }
}
