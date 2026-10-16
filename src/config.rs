mod init;
mod load;
mod mounts;
mod schema;
mod session;
mod session_files;
mod types;

pub use init::write_default_cladding_config;
pub use load::{
    load_cladding_config_v2, load_cladding_config_v2_from_path, load_cladding_config_v2_from_str,
    validate_session_config_path,
};
pub use session::{validate_baffle_session_config, validate_baffle_session_config_for_socket};
pub use session_files::{validate_baffle_config_layout, validate_baffle_session_files};
pub use types::{
    DEFAULT_AGENT_SESSION_CONFIG, DEFAULT_COMPONENT_IMAGE, DEFAULT_NW_SANDBOX_SESSION_CONFIG,
    DEFAULT_PROXY_IMAGE, ExecutionComponentConfig, ExecutionConfig, ExecutionProxyConfig,
    ImageBuildConfig, MountTarget, MountType, ResolvedMountConfig,
};
