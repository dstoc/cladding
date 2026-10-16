mod commands;
mod components;
mod mounts;
mod sockets;
mod types;

pub use types::{
    ManagedVolumeKind, RuntimeComponent, RuntimeContainer, RuntimeCustomMount, RuntimeEnvVar,
    RuntimeMount, RuntimeMountSource, RuntimeSpec,
};

pub fn runtime_container_name(component_name: &str) -> String {
    format!("{component_name}-instance")
}
