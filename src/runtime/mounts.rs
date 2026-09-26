use super::sockets::is_generated_runtime_mount_path;
use super::types::{RuntimeCustomMount, RuntimeMount, RuntimeMountSource, RuntimePod, RuntimeSpec};
use crate::config::{MountTarget, MountType, ResolvedMountConfig};
use std::collections::BTreeSet;
use std::fs;
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
    workspace_root: &Path,
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
                path: workspace_root.to_path_buf(),
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
    workspace_root: &Path,
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
                path: workspace_root.to_path_buf(),
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
    project_root: &Path,
    project_name: &str,
    mounts: &[ResolvedMountConfig],
) -> Vec<RuntimeCustomMount> {
    let mut runtime_mounts = mounts
        .iter()
        .map(|mount| RuntimeCustomMount {
            mount_path: mount.mount_path.clone(),
            read_only: mount.read_only,
            source: build_mount_source(project_name, mount),
            targets: mount.targets.clone(),
            ignore: mount.ignore,
        })
        .collect::<Vec<_>>();

    let empty_mask = project_root.join("runtime/empty-mask");
    let protected_root = canonical_or_normalized(project_root);
    for mount in mounts.iter().filter(|mount| {
        !mount.ignore && matches!(mount.mount_type, MountType::Readonly | MountType::Overlay)
    }) {
        let Some(host_path) = mount.host_path.as_deref() else {
            continue;
        };
        let host_path = canonical_or_normalized(host_path);
        let Ok(private_path) = protected_root.strip_prefix(&host_path) else {
            continue;
        };

        let private_mount_path = if private_path.as_os_str().is_empty() {
            mount.mount_path.clone()
        } else {
            let private_mount_path = Path::new(&mount.mount_path).join(private_path);
            let Some(private_mount_path) = private_mount_path.to_str() else {
                continue;
            };
            private_mount_path.to_string()
        };
        runtime_mounts.push(RuntimeCustomMount {
            mount_path: private_mount_path,
            read_only: true,
            source: RuntimeMountSource::GeneratedEmptyMask {
                path: empty_mask.clone(),
            },
            targets: mount.targets.clone(),
            ignore: false,
        });
    }

    runtime_mounts
}

fn build_mount_source(project_name: &str, mount: &ResolvedMountConfig) -> RuntimeMountSource {
    match (mount.mount_type, &mount.host_path, &mount.volume) {
        (MountType::Overlay, Some(path), None) => {
            RuntimeMountSource::OverlayHostPath { path: path.clone() }
        }
        (MountType::Tmpfs, None, None) => RuntimeMountSource::Tmpfs {
            size_bytes: mount.tmpfs_size_bytes,
        },
        (_, Some(path), None) => RuntimeMountSource::HostPath { path: path.clone() },
        (_, None, Some(name)) => RuntimeMountSource::NamedVolume {
            claim_name: format!("{project_name}-{name}"),
        },
        (_, None, None) => RuntimeMountSource::EmptyDir,
        (_, Some(_), Some(_)) => RuntimeMountSource::EmptyDir,
    }
}

fn normalize_path(path: &Path) -> PathBuf {
    let mut normalized = PathBuf::new();
    for component in path.components() {
        match component {
            std::path::Component::CurDir => {}
            std::path::Component::ParentDir => {
                if normalized.file_name().is_some() {
                    normalized.pop();
                }
            }
            other => normalized.push(other.as_os_str()),
        }
    }
    normalized
}

fn canonical_or_normalized(path: &Path) -> PathBuf {
    fs::canonicalize(path).unwrap_or_else(|_| normalize_path(path))
}

fn collect_required_host_paths(pod: &RuntimePod, paths: &mut BTreeSet<PathBuf>) {
    for container in &pod.containers {
        for mount in &container.mounts {
            match &mount.source {
                RuntimeMountSource::HostPath { path }
                | RuntimeMountSource::OverlayHostPath { path }
                    if !is_generated_runtime_mount_path(path) =>
                {
                    paths.insert(path.clone());
                }
                _ => {}
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
                build: None,
            },
            nw_sandbox: nw_enabled.then(|| ExecutionComponentConfig {
                enabled: true,
                image: "nw:image".to_string(),
                build: None,
            }),
            fs_sandbox: fs_enabled.then(|| ExecutionComponentConfig {
                enabled: true,
                image: "fs:image".to_string(),
                build: None,
            }),
            proxy: None,
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
                mount_type: MountType::Bind,
                tmpfs_size_bytes: None,
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
                    mount_type: MountType::Bind,
                    tmpfs_size_bytes: None,
                    read_only: true,
                    targets: vec![MountTarget::Agent],
                    ignore: true,
                },
                ResolvedMountConfig {
                    mount_path: "/inactive".to_string(),
                    host_path: Some(PathBuf::from("/tmp/inactive")),
                    volume: None,
                    mount_type: MountType::Bind,
                    tmpfs_size_bytes: None,
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

    #[test]
    fn custom_overlay_keeps_cladding_mask_and_tmpfs_is_a_distinct_source() {
        let project_root = Path::new("/tmp/cladding-mount-mask/.cladding");
        let config = execution_config(
            true,
            true,
            vec![
                ResolvedMountConfig {
                    mount_path: "/home/user/workspace".to_string(),
                    host_path: Some(project_root.join("..")),
                    volume: None,
                    mount_type: MountType::Overlay,
                    tmpfs_size_bytes: None,
                    read_only: false,
                    targets: vec![
                        MountTarget::Agent,
                        MountTarget::NwSandbox,
                        MountTarget::FsSandbox,
                    ],
                    ignore: false,
                },
                ResolvedMountConfig {
                    mount_path: "/tmp".to_string(),
                    host_path: None,
                    volume: None,
                    mount_type: MountType::Tmpfs,
                    tmpfs_size_bytes: Some(64 * 1024 * 1024),
                    read_only: false,
                    targets: vec![MountTarget::Agent],
                    ignore: false,
                },
            ],
            false,
        );
        let custom_mounts = build_custom_mounts(project_root, "demo", &config.mounts);

        assert!(matches!(
            &custom_mounts[0].source,
            RuntimeMountSource::OverlayHostPath { .. }
        ));
        assert!(matches!(
            &custom_mounts[1].source,
            RuntimeMountSource::Tmpfs {
                size_bytes: Some(67_108_864)
            }
        ));
        assert_eq!(
            custom_mounts[2].mount_path,
            "/home/user/workspace/.cladding"
        );
        assert!(matches!(
            &custom_mounts[2].source,
            RuntimeMountSource::GeneratedEmptyMask { .. }
        ));

        let workspace_root = project_root.join("..");
        let agent_mounts = apply_custom_mounts(
            build_agent_mounts(project_root, &workspace_root, &custom_mounts),
            &custom_mounts,
            MountTarget::Agent,
        );
        let nw_sandbox_mounts = apply_custom_mounts(
            build_sandbox_mounts(project_root, &workspace_root, &custom_mounts),
            &custom_mounts,
            MountTarget::NwSandbox,
        );
        for mounts in [&agent_mounts, &nw_sandbox_mounts] {
            let overlay = mounts
                .iter()
                .find(|mount| mount.mount_path == "/home/user/workspace")
                .expect("workspace overlay mount");
            assert!(matches!(
                &overlay.source,
                RuntimeMountSource::OverlayHostPath { .. }
            ));
            let mask = mounts
                .iter()
                .find(|mount| mount.mount_path == "/home/user/workspace/.cladding")
                .expect("workspace .cladding mask");
            assert!(matches!(
                &mask.source,
                RuntimeMountSource::GeneratedEmptyMask { .. }
            ));
        }

        let fs_mounts = apply_custom_mounts(
            build_fs_sandbox_mounts(project_root, &custom_mounts),
            &custom_mounts,
            MountTarget::FsSandbox,
        );
        assert_eq!(
            fs_mounts
                .iter()
                .map(|mount| mount.mount_path.as_str())
                .collect::<Vec<_>>(),
            vec![
                "/opt/config",
                "/opt/tools",
                "/home/user/workspace",
                "/home/user/workspace/.cladding"
            ]
        );
    }
}
