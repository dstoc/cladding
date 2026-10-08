use crate::runtime::{RuntimeMount, RuntimeMountSource, RuntimeSpec};
use std::process::Command;

pub(super) fn append_mount_args(cmd: &mut Command, component_name: &str, mounts: &[RuntimeMount]) {
    for mount in mounts {
        if let RuntimeMountSource::Tmpfs { size_bytes } = &mount.source {
            let mut spec = format!("type=tmpfs,dst={},tmpfs-mode=1777,U=true", mount.mount_path);
            if let Some(size_bytes) = size_bytes {
                spec.push_str(&format!(",tmpfs-size={size_bytes}"));
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
            RuntimeMountSource::Tmpfs { .. } => unreachable!("tmpfs mounts handled above"),
            RuntimeMountSource::EmptyDir => {
                empty_dir_volume_name(component_name, &mount.mount_path)
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

fn empty_dir_volume_name(component_name: &str, mount_path: &str) -> String {
    format!(
        "cladding-{component_name}-empty-{}",
        sanitize_volume_fragment(mount_path)
    )
}

pub(super) fn generated_empty_mask_dirs(spec: &RuntimeSpec) -> Vec<std::path::PathBuf> {
    let mut paths = std::collections::BTreeSet::new();

    for component in runtime_components(spec) {
        for container in &component.containers {
            for mount in &container.mounts {
                if let RuntimeMountSource::GeneratedEmptyMask { path } = &mount.source {
                    paths.insert(path.clone());
                }
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

fn sanitize_volume_fragment(value: &str) -> String {
    let fragment = value.trim_matches('/').replace('/', "-");
    let fragment = fragment
        .chars()
        .map(|ch| {
            if ch.is_ascii_alphanumeric() || matches!(ch, '.' | '_' | '-') {
                ch
            } else {
                '-'
            }
        })
        .collect::<String>();
    if fragment.is_empty() {
        "root".to_string()
    } else {
        fragment
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::runtime::ManagedVolumeKind;

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
            "demo-agent",
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
                    mount_path: "/run/cladding/proxy/agent".to_string(),
                    read_only: false,
                    source: RuntimeMountSource::NamedVolumeChown {
                        claim_name: "demo-proxy-relay-agent".to_string(),
                    },
                },
                RuntimeMount {
                    mount_path: "/workspace/tmp".to_string(),
                    read_only: false,
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
                "demo-proxy-relay-agent:/run/cladding/proxy/agent:U",
                "--volume",
                "cladding-demo-agent-empty-workspace-tmp:/workspace/tmp",
                "--volume",
                "/tmp/demo/runtime/empty-mask:/home/user/workspace/.cladding:ro",
            ]
        );
    }

    #[test]
    fn append_mount_args_formats_managed_volumes_and_ephemeral_tmpfs_options() {
        let mut cmd = Command::new("podman");
        append_mount_args(
            &mut cmd,
            "demo-agent",
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

    #[test]
    fn empty_dir_volume_name_sanitizes_mount_path() {
        assert_eq!(
            empty_dir_volume_name("demo-agent", "/workspace/tmp"),
            "cladding-demo-agent-empty-workspace-tmp"
        );
        assert_eq!(
            empty_dir_volume_name("demo-agent", "/"),
            "cladding-demo-agent-empty-root"
        );
        assert_eq!(
            empty_dir_volume_name("demo-agent", "/path/with spaces"),
            "cladding-demo-agent-empty-path-with-spaces"
        );
    }
}
