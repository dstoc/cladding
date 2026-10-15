use crate::runtime::{RuntimeMount, RuntimeMountSource, RuntimeSpec};
use std::process::Command;

pub(super) fn append_mount_args(cmd: &mut Command, mounts: &[RuntimeMount]) {
    for mount in mounts {
        if matches!(
            &mount.source,
            RuntimeMountSource::Tmpfs { .. } | RuntimeMountSource::EmptyDir
        ) {
            let empty_dir = matches!(&mount.source, RuntimeMountSource::EmptyDir);
            let size_bytes = match &mount.source {
                RuntimeMountSource::Tmpfs { size_bytes } => *size_bytes,
                RuntimeMountSource::EmptyDir => None,
                _ => unreachable!("tmpfs sources handled above"),
            };
            let mut spec = format!("type=tmpfs,dst={},tmpfs-mode=1777,U=true", mount.mount_path);
            if let Some(size_bytes) = size_bytes {
                spec.push_str(&format!(",tmpfs-size={size_bytes}"));
            }
            if empty_dir {
                spec.push_str(",notmpcopyup");
            }
            if mount.read_only {
                spec.push_str(",ro=true");
            }
            cmd.arg("--mount");
            cmd.arg(spec);
            continue;
        }

        if let RuntimeMountSource::ManagedVolume { claim_name, .. } = &mount.source {
            let mut spec = format!("type=volume,src={claim_name},dst={}", mount.mount_path);
            if mount.read_only {
                spec.push_str(",ro=true");
            }
            cmd.arg("--mount");
            cmd.arg(spec);
            continue;
        }

        let source = match &mount.source {
            RuntimeMountSource::HostPath { path } => path.display().to_string(),
            RuntimeMountSource::NamedVolume { claim_name } => claim_name.clone(),
            RuntimeMountSource::NamedVolumeChown { claim_name } => claim_name.clone(),
            RuntimeMountSource::ManagedVolume { .. } => {
                unreachable!("managed volumes handled above")
            }
            RuntimeMountSource::GeneratedEmptyMask { path } => path.display().to_string(),
            RuntimeMountSource::Tmpfs { .. } | RuntimeMountSource::EmptyDir => {
                unreachable!("tmpfs sources handled above")
            }
        };

        let mut volume = format!("{source}:{}", mount.mount_path);
        if mount.read_only {
            volume.push_str(":ro");
        }
        if matches!(&mount.source, RuntimeMountSource::NamedVolumeChown { .. }) {
            volume.push_str(":U");
        }
        cmd.arg("--volume");
        cmd.arg(volume);
    }
}

pub(super) fn generated_empty_mask_dirs(spec: &RuntimeSpec) -> Vec<std::path::PathBuf> {
    let mut paths = std::collections::BTreeSet::new();

    for component in runtime_components(spec) {
        for mount in &component.container.mounts {
            if let RuntimeMountSource::GeneratedEmptyMask { path } = &mount.source {
                paths.insert(path.clone());
            }
        }
    }

    paths.into_iter().collect()
}

fn runtime_components(spec: &RuntimeSpec) -> Vec<&crate::runtime::RuntimeComponent> {
    let mut components = vec![&spec.proxy, &spec.agent];
    if let Some(component) = &spec.nw_sandbox {
        components.push(component);
    }
    if let Some(component) = &spec.fs_sandbox {
        components.push(component);
    }
    components
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::{
        ExecutionComponentConfig, ExecutionConfig, MountTarget, MountType, ResolvedMountConfig,
    };
    use crate::runtime::{ManagedVolumeKind, RuntimeSpec};

    fn command_args(cmd: &Command) -> Vec<String> {
        cmd.get_args()
            .map(|arg| arg.to_string_lossy().into_owned())
            .collect()
    }

    #[test]
    fn append_mount_args_includes_mount_sources_and_read_only_flag() {
        let mut cmd = Command::new("podman");
        append_mount_args(
            &mut cmd,
            &[
                RuntimeMount {
                    mount_path: "/opt/config".to_string(),
                    read_only: true,
                    source: RuntimeMountSource::HostPath {
                        path: "/tmp/demo/config".into(),
                    },
                },
                RuntimeMount {
                    mount_path: "/workspace/data".to_string(),
                    read_only: false,
                    source: RuntimeMountSource::NamedVolume {
                        claim_name: "demo-cache".to_string(),
                    },
                },
                RuntimeMount {
                    mount_path: "/run/cladding/proxy".to_string(),
                    read_only: false,
                    source: RuntimeMountSource::NamedVolumeChown {
                        claim_name: "demo-socket-baffle-agent".to_string(),
                    },
                },
                RuntimeMount {
                    mount_path: "/workspace/tmp".to_string(),
                    read_only: true,
                    source: RuntimeMountSource::EmptyDir,
                },
                RuntimeMount {
                    mount_path: "/home/user/workspace/.cladding".to_string(),
                    read_only: true,
                    source: RuntimeMountSource::GeneratedEmptyMask {
                        path: "/tmp/demo/runtime/empty-mask".into(),
                    },
                },
            ],
        );

        assert_eq!(
            command_args(&cmd),
            vec![
                "--volume",
                "/tmp/demo/config:/opt/config:ro",
                "--volume",
                "demo-cache:/workspace/data",
                "--volume",
                "demo-socket-baffle-agent:/run/cladding/proxy:U",
                "--mount",
                "type=tmpfs,dst=/workspace/tmp,tmpfs-mode=1777,U=true,notmpcopyup,ro=true",
                "--volume",
                "/tmp/demo/runtime/empty-mask:/home/user/workspace/.cladding:ro",
            ]
        );
    }

    #[test]
    fn explicit_volume_mount_args_are_verbatim_for_persistent_and_one_off_specs() {
        let project_root = std::path::Path::new("/tmp/project/.cladding");
        for (name, runtime_root) in [
            ("demo", project_root),
            (
                "cladding-run-00000000-0000-4000-8000-000000000000",
                std::path::Path::new("/tmp/project/.cladding/runtime/runs/instance"),
            ),
        ] {
            let config = ExecutionConfig {
                name: name.to_string(),
                use_runsc: false,
                agent: ExecutionComponentConfig {
                    enabled: true,
                    init: true,
                    image: "agent:image".to_string(),
                    build: None,
                    security_opts: Vec::new(),
                },
                nw_sandbox: None,
                fs_sandbox: None,
                proxy: None,
                mounts: vec![ResolvedMountConfig {
                    mount_path: "/home/user/.cache/kache".to_string(),
                    host_path: None,
                    volume: Some("cladding-kache".to_string()),
                    mount_type: MountType::Bind,
                    tmpfs_size_bytes: None,
                    read_only: false,
                    targets: vec![MountTarget::Agent],
                    ignore: false,
                }],
            };
            let spec = RuntimeSpec::build_with_roots(
                project_root,
                &project_root.join(".."),
                runtime_root,
                &config,
            );
            let container = &spec.agent.container;
            let mut cmd = Command::new("podman");
            append_mount_args(&mut cmd, &container.mounts);

            assert!(
                command_args(&cmd)
                    .windows(2)
                    .any(|pair| { pair == ["--volume", "cladding-kache:/home/user/.cache/kache"] })
            );
        }
    }

    #[test]
    fn append_mount_args_formats_managed_volumes_and_ephemeral_tmpfs_options() {
        let mut cmd = Command::new("podman");
        append_mount_args(
            &mut cmd,
            &[
                RuntimeMount {
                    mount_path: "/workspace".to_string(),
                    read_only: false,
                    source: RuntimeMountSource::ManagedVolume {
                        claim_name: "cladding-mount-copy".to_string(),
                        kind: crate::runtime::ManagedVolumeKind::Copy {
                            source: "/tmp/project".into(),
                        },
                    },
                },
                RuntimeMount {
                    mount_path: "/tmp/cache".to_string(),
                    read_only: false,
                    source: RuntimeMountSource::Tmpfs {
                        size_bytes: Some(1024 * 1024 * 1024),
                    },
                },
                RuntimeMount {
                    mount_path: "/readonly".to_string(),
                    read_only: true,
                    source: RuntimeMountSource::ManagedVolume {
                        claim_name: "cladding-mount-readonly".to_string(),
                        kind: crate::runtime::ManagedVolumeKind::Copy {
                            source: "/tmp/read-only-source".into(),
                        },
                    },
                },
                RuntimeMount {
                    mount_path: "/shared".to_string(),
                    read_only: false,
                    source: RuntimeMountSource::ManagedVolume {
                        claim_name: "cladding-mount-tmpfs".to_string(),
                        kind: ManagedVolumeKind::Tmpfs {
                            size_bytes: 32 * 1024 * 1024,
                        },
                    },
                },
                RuntimeMount {
                    mount_path: "/run".to_string(),
                    read_only: false,
                    source: RuntimeMountSource::Tmpfs { size_bytes: None },
                },
            ],
        );

        assert_eq!(
            command_args(&cmd),
            vec![
                "--mount",
                "type=volume,src=cladding-mount-copy,dst=/workspace",
                "--mount",
                "type=tmpfs,dst=/tmp/cache,tmpfs-mode=1777,U=true,tmpfs-size=1073741824",
                "--mount",
                "type=volume,src=cladding-mount-readonly,dst=/readonly,ro=true",
                "--mount",
                "type=volume,src=cladding-mount-tmpfs,dst=/shared",
                "--mount",
                "type=tmpfs,dst=/run,tmpfs-mode=1777,U=true",
            ]
        );
    }
}
