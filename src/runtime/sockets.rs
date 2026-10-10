use super::types::{RuntimeComponent, RuntimeMount, RuntimeMountSource, RuntimeSpec};
use std::collections::BTreeSet;
use std::path::{Path, PathBuf};

pub(super) const RUNTIME_SOCKET_DIR: &str = "runtime/sockets";
pub(super) const RUNTIME_AGENT_INJECT_SOCKET_DIR: &str = "agent/inject";
pub(super) const RUNTIME_PROXY_AGENT_SOCKET_DIR: &str = "proxy/agent";
pub(super) const RUNTIME_PROXY_NW_SANDBOX_SOCKET_DIR: &str = "proxy/nw-sandbox";
pub(super) const RUNTIME_PROXY_SOCKET_DIR: &str = "proxy";
pub(super) const RUNTIME_RUN_NW_SANDBOX_SOCKET_DIR: &str = "run/nw-sandbox";
pub(super) const RUNTIME_RUN_FS_SANDBOX_SOCKET_DIR: &str = "run/fs-sandbox";
pub(super) const RUNTIME_AGENT_INJECT_MOUNT_PATH: &str = "/run/cladding/agent/inject";
pub(super) const RUNTIME_PROXY_AGENT_MOUNT_PATH: &str = "/run/cladding/proxy/agent";
pub(super) const RUNTIME_PROXY_NW_SANDBOX_MOUNT_PATH: &str = "/run/cladding/proxy/nw-sandbox";
pub(super) const RUNTIME_PROXY_MOUNT_PATH: &str = "/run/cladding/proxy";
pub(super) const RUNTIME_PROXY_AGENT_RELAY_MOUNT_PATH: &str = "/run/cladding/proxy/agent";
pub(super) const RUNTIME_PROXY_NW_SANDBOX_RELAY_MOUNT_PATH: &str = "/run/cladding/proxy/nw-sandbox";
pub(super) const RUNTIME_RUN_NW_SANDBOX_MOUNT_PATH: &str = "/run/cladding/run/nw-sandbox";
pub(super) const RUNTIME_RUN_FS_SANDBOX_MOUNT_PATH: &str = "/run/cladding/run/fs-sandbox";

impl RuntimeSpec {
    pub fn generated_runtime_socket_dirs(&self) -> Vec<PathBuf> {
        let mut paths = BTreeSet::new();
        paths.insert(self.runtime_root.join(RUNTIME_SOCKET_DIR));
        paths.insert(runtime_scoped_socket_dir(
            &self.runtime_root,
            RUNTIME_PROXY_SOCKET_DIR,
        ));
        paths.insert(runtime_scoped_socket_dir(
            &self.runtime_root,
            RUNTIME_PROXY_AGENT_SOCKET_DIR,
        ));
        if self.nw_sandbox.is_some() {
            paths.insert(runtime_scoped_socket_dir(
                &self.runtime_root,
                RUNTIME_PROXY_NW_SANDBOX_SOCKET_DIR,
            ));
        }

        collect_generated_runtime_socket_dirs(&self.proxy, &mut paths);
        collect_generated_runtime_socket_dirs(&self.agent, &mut paths);
        if let Some(component) = &self.nw_sandbox {
            collect_generated_runtime_socket_dirs(component, &mut paths);
        }
        if let Some(component) = &self.fs_sandbox {
            collect_generated_runtime_socket_dirs(component, &mut paths);
        }

        paths.into_iter().collect()
    }
}

fn runtime_socket_dir(project_root: &Path) -> PathBuf {
    project_root.join(RUNTIME_SOCKET_DIR)
}

fn runtime_scoped_socket_dir(project_root: &Path, socket_dir: &str) -> PathBuf {
    runtime_socket_dir(project_root).join(socket_dir)
}

pub(super) fn runtime_socket_mount_path(mount_dir: &str, socket_name: &str) -> String {
    format!("{mount_dir}/{socket_name}")
}

pub(super) fn build_scoped_socket_mount(
    runtime_root: &Path,
    socket_dir: &str,
    mount_path: &str,
) -> Vec<RuntimeMount> {
    vec![RuntimeMount {
        mount_path: mount_path.to_string(),
        read_only: false,
        source: RuntimeMountSource::HostPath {
            path: runtime_scoped_socket_dir(runtime_root, socket_dir),
        },
    }]
}

pub(super) fn build_proxy_relay_volume_mount(
    project_name: &str,
    component: &str,
    mount_path: &str,
    chown: bool,
) -> RuntimeMount {
    let claim_name = proxy_relay_volume_name(project_name, component);
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

pub(super) fn proxy_relay_volume_name(project_name: &str, component: &str) -> String {
    format!("cladding-{project_name}-baffle-relay-{component}")
}

fn collect_generated_runtime_socket_dirs(
    component: &RuntimeComponent,
    paths: &mut BTreeSet<PathBuf>,
) {
    for container in &component.containers {
        for mount in &container.mounts {
            if let RuntimeMountSource::HostPath { path } = &mount.source
                && is_generated_runtime_socket_path(path)
            {
                paths.insert(path.clone());
            }
        }
    }
}

pub(super) fn is_generated_runtime_mount_path(path: &Path) -> bool {
    is_runtime_child_path(path, "sockets") || is_runtime_child_path(path, "scripts")
}

fn is_generated_runtime_socket_path(path: &Path) -> bool {
    is_runtime_child_path(path, "sockets")
}

fn is_runtime_child_path(path: &Path, child_name: &str) -> bool {
    let mut saw_runtime = false;
    for component in path.components() {
        if let std::path::Component::Normal(name) = component {
            if saw_runtime && name == child_name {
                return true;
            }
            saw_runtime = name == "runtime";
        } else {
            saw_runtime = false;
        }
    }
    false
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::{ExecutionComponentConfig, ExecutionConfig, ResolvedMountConfig};

    fn execution_config(
        nw_enabled: bool,
        fs_enabled: bool,
        mounts: Vec<ResolvedMountConfig>,
        use_runsc: bool,
    ) -> ExecutionConfig {
        ExecutionConfig {
            name: "demo".to_string(),
            use_runsc,
            agent: ExecutionComponentConfig {
                enabled: true,
                image: "agent:image".to_string(),
                build: None,
                security_opts: Vec::new(),
            },
            nw_sandbox: nw_enabled.then(|| ExecutionComponentConfig {
                enabled: true,
                image: "nw:image".to_string(),
                build: None,
                security_opts: Vec::new(),
            }),
            fs_sandbox: fs_enabled.then(|| ExecutionComponentConfig {
                enabled: true,
                image: "fs:image".to_string(),
                build: None,
                security_opts: Vec::new(),
            }),
            proxy: None,
            mounts,
        }
    }

    #[test]
    fn generated_runtime_socket_dirs_include_nested_socket_mounts() {
        let config = execution_config(true, true, Vec::new(), true);
        let spec = RuntimeSpec::build(Path::new("/tmp/project/.cladding"), &config);
        let generated = spec.generated_runtime_socket_dirs();

        assert_eq!(
            generated.into_iter().collect::<BTreeSet<_>>(),
            [
                PathBuf::from("/tmp/project/.cladding/runtime/sockets"),
                PathBuf::from("/tmp/project/.cladding/runtime/sockets/proxy"),
                PathBuf::from("/tmp/project/.cladding/runtime/sockets/agent/inject"),
                PathBuf::from("/tmp/project/.cladding/runtime/sockets/proxy/agent"),
                PathBuf::from("/tmp/project/.cladding/runtime/sockets/proxy/nw-sandbox"),
                PathBuf::from("/tmp/project/.cladding/runtime/sockets/run/nw-sandbox"),
                PathBuf::from("/tmp/project/.cladding/runtime/sockets/run/fs-sandbox"),
            ]
            .into_iter()
            .collect()
        );
    }

    #[test]
    fn proxy_relay_volumes_are_scoped_to_the_component_and_chowned_in_proxy() {
        let mount = build_proxy_relay_volume_mount(
            "demo",
            "agent",
            RUNTIME_PROXY_AGENT_RELAY_MOUNT_PATH,
            true,
        );

        assert_eq!(mount.mount_path, RUNTIME_PROXY_AGENT_RELAY_MOUNT_PATH);
        assert!(matches!(
            mount.source,
            RuntimeMountSource::NamedVolumeChown { claim_name }
                if claim_name == "cladding-demo-baffle-relay-agent"
        ));

        let nw_mount = build_proxy_relay_volume_mount(
            "demo",
            "nw-sandbox",
            RUNTIME_PROXY_NW_SANDBOX_RELAY_MOUNT_PATH,
            false,
        );
        assert!(matches!(
            nw_mount.source,
            RuntimeMountSource::NamedVolume { claim_name }
                if claim_name == "cladding-demo-baffle-relay-nw-sandbox"
        ));
    }
}
