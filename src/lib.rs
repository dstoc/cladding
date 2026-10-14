#[cfg(not(all(
    any(target_os = "linux", target_os = "macos"),
    any(target_arch = "x86_64", target_arch = "aarch64")
)))]
compile_error!("Cladding supports Linux and macOS targets on x86_64 and aarch64 only.");

pub mod assets;
pub mod config;
pub mod credentials;
pub mod error;
pub mod fs_utils;
pub mod identity;
pub mod podman;
pub mod runtime;
