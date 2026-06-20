use crate::runtime::{RuntimeMount, RuntimeMountSource, RuntimeSpec};
use std::process::Command;

pub(super) fn append_mount_args(cmd: &mut Command, pod_name: &str, mounts: &[RuntimeMount]) {
    for mount in mounts {
        let source = match &mount.source {
            RuntimeMountSource::HostPath { path } => path.display().to_string(),
            RuntimeMountSource::NamedVolume { claim_name } => claim_name.clone(),
            RuntimeMountSource::GeneratedEmptyMask { path } => path.display().to_string(),
            RuntimeMountSource::EmptyDir => empty_dir_volume_name(pod_name, &mount.mount_path),
        };

        let mut volume = format!("{source}:{}", mount.mount_path);
        if mount.read_only {
            volume.push_str(":ro");
        }
        cmd.arg("--volume");
        cmd.arg(volume);
    }
}

fn empty_dir_volume_name(pod_name: &str, mount_path: &str) -> String {
    format!(
        "cladding-{pod_name}-empty-{}",
        sanitize_volume_fragment(mount_path)
    )
}

pub(super) fn generated_empty_mask_dirs(spec: &RuntimeSpec) -> Vec<std::path::PathBuf> {
    let mut paths = std::collections::BTreeSet::new();

    for pod in runtime_pods(spec) {
        for container in &pod.containers {
            for mount in &container.mounts {
                if let RuntimeMountSource::GeneratedEmptyMask { path } = &mount.source {
                    paths.insert(path.clone());
                }
            }
        }
    }

    paths.into_iter().collect()
}

fn runtime_pods(spec: &RuntimeSpec) -> Vec<&crate::runtime::RuntimePod> {
    let mut pods = vec![&spec.proxy, &spec.agent];
    if let Some(pod) = &spec.nw_sandbox {
        pods.push(pod);
    }
    if let Some(pod) = &spec.fs_sandbox {
        pods.push(pod);
    }
    pods
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
                "cladding-demo-agent-empty-workspace-tmp:/workspace/tmp",
                "--volume",
                "/tmp/demo/runtime/empty-mask:/home/user/workspace/.cladding:ro",
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
