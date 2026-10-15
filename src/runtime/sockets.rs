use super::types::{RuntimeMount, RuntimeMountSource};

pub(super) const RUNTIME_PROXY_AGENT_MOUNT_PATH: &str = "/run/cladding/proxy/agent";
pub(super) const RUNTIME_PROXY_NW_SANDBOX_MOUNT_PATH: &str = "/run/cladding/proxy/nw-sandbox";
pub(super) const RUNTIME_RUN_NW_SANDBOX_MOUNT_PATH: &str = "/run/cladding/run/nw-sandbox";
pub(super) const RUNTIME_RUN_FS_SANDBOX_MOUNT_PATH: &str = "/run/cladding/run/fs-sandbox";

pub(super) fn runtime_socket_mount_path(mount_dir: &str, socket_name: &str) -> String {
    format!("{mount_dir}/{socket_name}")
}

pub(super) fn build_socket_volume_mount(
    project_name: &str,
    channel: &str,
    mount_path: &str,
    chown: bool,
) -> RuntimeMount {
    let claim_name = socket_volume_name(project_name, channel);
    RuntimeMount {
        mount_path: mount_path.to_string(),
        read_only: false,
        source: if chown {
            RuntimeMountSource::NamedVolumeChown { claim_name }
        } else {
            RuntimeMountSource::NamedVolume { claim_name }
        },
    }
}

pub(super) fn socket_volume_name(project_name: &str, channel: &str) -> String {
    format!("cladding-{project_name}-socket-{channel}")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn socket_volumes_are_scoped_to_the_channel_and_chowned_by_the_owner() {
        let mount =
            build_socket_volume_mount("demo", "baffle-agent", RUNTIME_PROXY_AGENT_MOUNT_PATH, true);

        assert_eq!(mount.mount_path, RUNTIME_PROXY_AGENT_MOUNT_PATH);
        assert!(matches!(
            mount.source,
            RuntimeMountSource::NamedVolumeChown { claim_name }
                if claim_name == "cladding-demo-socket-baffle-agent"
        ));

        let nw_mount = build_socket_volume_mount(
            "demo",
            "baffle-nw-sandbox",
            RUNTIME_PROXY_NW_SANDBOX_MOUNT_PATH,
            false,
        );
        assert!(matches!(
            nw_mount.source,
            RuntimeMountSource::NamedVolume { claim_name }
                if claim_name == "cladding-demo-socket-baffle-nw-sandbox"
        ));
    }
}
