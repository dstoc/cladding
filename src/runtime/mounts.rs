use super::sockets::is_generated_runtime_mount_path;
use super::types::{RuntimeCustomMount, RuntimeMount, RuntimeMountSource, RuntimePod, RuntimeSpec};
use crate::config::{MountTarget, ResolvedMountConfig};
use std::collections::BTreeSet;
use std::path::{Path, PathBuf};

impl RuntimeSpec {
    pub fn required_host_paths(&self) -> Vec<PathBuf> {
        let mut paths = BTreeSet::new();

        collect_required_host_paths(&self.proxy, &mut paths);
        collect_required_host_paths(&self.agent, &mut paths);
        if let Some(pod) = &self.nw_sandbox {
            collect_required_host_paths(pod, &mut paths);
        }
        if let Some(pod) = &self.fs_sandbox {
            collect_required_host_paths(pod, &mut paths);
        }

        paths.into_iter().collect()
    }
}

pub(super) fn build_proxy_mounts(
    project_root: &Path,
    _custom_mounts: &[RuntimeCustomMount],
) -> Vec<RuntimeMount> {
    vec![
        RuntimeMount {
            mount_path: "/opt/config".to_string(),
            read_only: true,
            source: RuntimeMountSource::HostPath {
                path: project_root.join("config"),
            },
        },
        RuntimeMount {
            mount_path: "/opt/scripts".to_string(),
            read_only: true,
            source: RuntimeMountSource::HostPath {
                path: project_root.join("runtime/scripts"),
            },
        },
    ]
}

pub(super) fn build_agent_mounts(
    project_root: &Path,
    _custom_mounts: &[RuntimeCustomMount],
) -> Vec<RuntimeMount> {
    vec![
        RuntimeMount {
            mount_path: "/opt/config".to_string(),
            read_only: true,
            source: RuntimeMountSource::HostPath {
                path: project_root.join("config"),
            },
        },
        RuntimeMount {
            mount_path: "/opt/tools".to_string(),
            read_only: true,
            source: RuntimeMountSource::HostPath {
                path: project_root.join("tools"),
            },
        },
        RuntimeMount {
            mount_path: "/home/user".to_string(),
            read_only: false,
            source: RuntimeMountSource::HostPath {
                path: project_root.join("home"),
            },
        },
        RuntimeMount {
            mount_path: "/home/user/workspace".to_string(),
            read_only: false,
            source: RuntimeMountSource::HostPath {
                path: project_root.join(".."),
            },
        },
        RuntimeMount {
            mount_path: "/home/user/workspace/.cladding".to_string(),
            read_only: true,
            source: RuntimeMountSource::GeneratedEmptyMask {
                path: project_root.join("runtime/empty-mask"),
            },
        },
    ]
}

pub(super) fn build_sandbox_mounts(
    project_root: &Path,
    _custom_mounts: &[RuntimeCustomMount],
) -> Vec<RuntimeMount> {
    vec![
        RuntimeMount {
            mount_path: "/opt/config".to_string(),
            read_only: true,
            source: RuntimeMountSource::HostPath {
                path: project_root.join("config"),
            },
        },
        RuntimeMount {
            mount_path: "/opt/tools".to_string(),
            read_only: true,
            source: RuntimeMountSource::HostPath {
                path: project_root.join("tools"),
            },
        },
        RuntimeMount {
            mount_path: "/home/user".to_string(),
            read_only: false,
            source: RuntimeMountSource::HostPath {
                path: project_root.join("home"),
            },
        },
        RuntimeMount {
            mount_path: "/home/user/workspace".to_string(),
            read_only: false,
            source: RuntimeMountSource::HostPath {
                path: project_root.join(".."),
            },
        },
        RuntimeMount {
            mount_path: "/home/user/workspace/.cladding".to_string(),
            read_only: true,
            source: RuntimeMountSource::GeneratedEmptyMask {
                path: project_root.join("runtime/empty-mask"),
            },
        },
    ]
}

pub(super) fn build_fs_sandbox_mounts(
    project_root: &Path,
    _custom_mounts: &[RuntimeCustomMount],
) -> Vec<RuntimeMount> {
    vec![
        RuntimeMount {
            mount_path: "/opt/config".to_string(),
            read_only: true,
            source: RuntimeMountSource::HostPath {
                path: project_root.join("config"),
            },
        },
        RuntimeMount {
            mount_path: "/opt/tools".to_string(),
            read_only: true,
            source: RuntimeMountSource::HostPath {
                path: project_root.join("tools"),
            },
        },
    ]
}

pub(super) fn apply_custom_mounts(
    mut mounts: Vec<RuntimeMount>,
    custom_mounts: &[RuntimeCustomMount],
    target: MountTarget,
) -> Vec<RuntimeMount> {
    for custom in custom_mounts
        .iter()
        .filter(|mount| mount.targets.contains(&target))
    {
        if let Some(index) = mounts
            .iter()
            .position(|mount| mount.mount_path == custom.mount_path)
        {
            if custom.ignore {
                mounts.remove(index);
                continue;
            }

            mounts[index] = RuntimeMount {
                mount_path: custom.mount_path.clone(),
                read_only: custom.read_only,
                source: custom.source.clone(),
            };
        } else if !custom.ignore {
            mounts.push(RuntimeMount {
                mount_path: custom.mount_path.clone(),
                read_only: custom.read_only,
                source: custom.source.clone(),
            });
        }
    }

    mounts
}

pub(super) fn build_custom_mounts(
    project_name: &str,
    mounts: &[ResolvedMountConfig],
) -> Vec<RuntimeCustomMount> {
    mounts
        .iter()
        .map(|mount| RuntimeCustomMount {
            mount_path: mount.mount_path.clone(),
            read_only: mount.read_only,
            source: build_mount_source(project_name, mount),
            targets: mount.targets.clone(),
            ignore: mount.ignore,
        })
        .collect()
}

fn build_mount_source(project_name: &str, mount: &ResolvedMountConfig) -> RuntimeMountSource {
    match (&mount.host_path, &mount.volume) {
        (Some(path), None) => RuntimeMountSource::HostPath { path: path.clone() },
        (None, Some(name)) => RuntimeMountSource::NamedVolume {
            claim_name: format!("{project_name}-{name}"),
        },
        (None, None) => RuntimeMountSource::EmptyDir,
        (Some(_), Some(_)) => RuntimeMountSource::EmptyDir,
    }
}

fn collect_required_host_paths(pod: &RuntimePod, paths: &mut BTreeSet<PathBuf>) {
    for container in &pod.containers {
        for mount in &container.mounts {
            if let RuntimeMountSource::HostPath { path } = &mount.source {
                if is_generated_runtime_mount_path(path) {
                    continue;
                }
                paths.insert(path.clone());
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::{ExecutionComponentConfig, ExecutionConfig};

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
            },
            nw_sandbox: nw_enabled.then(|| ExecutionComponentConfig {
                enabled: true,
                image: "nw:image".to_string(),
            }),
            fs_sandbox: fs_enabled.then(|| ExecutionComponentConfig {
                enabled: true,
                image: "fs:image".to_string(),
            }),
            mounts,
        }
    }

    #[test]
    fn required_host_paths_skip_generated_mask_but_include_custom_mounts() {
        let config = execution_config(
            true,
            false,
            vec![ResolvedMountConfig {
                mount_path: "/workspace".to_string(),
                host_path: Some(PathBuf::from("/tmp/workspace")),
                volume: None,
                read_only: true,
                targets: vec![MountTarget::Agent],
                ignore: false,
            }],
            false,
        );
        let spec = RuntimeSpec::build(Path::new("/tmp/project/.cladding"), &config);
        let required = spec.required_host_paths();

        assert!(required.contains(&PathBuf::from("/tmp/project/.cladding/config")));
        assert!(!required.contains(&PathBuf::from("/tmp/project/.cladding/runtime/scripts")));
        assert!(required.contains(&PathBuf::from("/tmp/project/.cladding/tools")));
        assert!(required.contains(&PathBuf::from("/tmp/project/.cladding/home")));
        assert!(required.contains(&PathBuf::from("/tmp/project/.cladding/..")));
        assert!(!required.contains(&PathBuf::from("/tmp/project/.cladding/runtime/empty-mask")));
        assert!(required.contains(&PathBuf::from("/tmp/workspace")));
    }

    #[test]
    fn required_host_paths_exclude_ignored_or_inactive_custom_mounts() {
        let config = execution_config(
            false,
            false,
            vec![
                ResolvedMountConfig {
                    mount_path: "/ignored".to_string(),
                    host_path: Some(PathBuf::from("/tmp/ignored")),
                    volume: None,
                    read_only: true,
                    targets: vec![MountTarget::Agent],
                    ignore: true,
                },
                ResolvedMountConfig {
                    mount_path: "/inactive".to_string(),
                    host_path: Some(PathBuf::from("/tmp/inactive")),
                    volume: None,
                    read_only: true,
                    targets: vec![MountTarget::NwSandbox],
                    ignore: false,
                },
            ],
            false,
        );
        let spec = RuntimeSpec::build(Path::new("/tmp/project/.cladding"), &config);
        let required = spec.required_host_paths();

        assert!(!required.contains(&PathBuf::from("/tmp/ignored")));
        assert!(!required.contains(&PathBuf::from("/tmp/inactive")));
    }
}
