mod init;
mod load;
mod mounts;
mod types;

pub use init::write_default_cladding_config;
pub use load::load_cladding_config_v2;
pub use types::{
    BUILTIN_SQUID_MITM_IMAGE, BuiltinProxy, DEFAULT_COMPONENT_IMAGE, DEFAULT_PROXY_IMAGE,
    ExecutionComponentConfig, ExecutionConfig, ExecutionProxyConfig, ImageBuildConfig, MountTarget,
    ResolvedMountConfig,
};
