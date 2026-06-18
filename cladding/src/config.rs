mod init;
mod load;
mod mounts;
mod types;

pub use init::write_default_cladding_config;
pub use load::load_cladding_config_v2;
pub use types::{
    DEFAULT_COMPONENT_IMAGE, ExecutionComponentConfig, ExecutionConfig, MountTarget,
    ResolvedMountConfig,
};
