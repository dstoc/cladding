use cladding::config::Config;
use cladding::config::MountConfig;
use cladding::network::resolve_network_settings;
use cladding::pods::render_pods_yaml;
use serde::Deserialize;
use serde_yaml::Value;
use std::path::Path;
use std::path::PathBuf;

#[test]
fn render_pods_yaml_replaces_placeholders() {
    let settings = resolve_network_settings("demo", 1).unwrap();
    assert_eq!(settings.proxy_name, "demo-proxy");
    assert_eq!(settings.sandbox_name, "demo-nw-sandbox");
    assert_eq!(settings.agent_name, "demo-agent");

    let config = Config {
        name: "demo".to_string(),
        nw_sandbox_image: "sandbox:image".to_string(),
        agent_image: "agent:image".to_string(),
        mounts: Vec::new(),
    };
    let rendered = render_pods_yaml(Path::new("/tmp/project/.cladding"), &config, &settings);

    assert!(!rendered.contains("REPLACE_PROXY_NAME"));
    assert!(!rendered.contains("REPLACE_NW_SANDBOX_IMAGE"));
    assert!(!rendered.contains("REPLACE_AGENT_IMAGE"));
    assert!(rendered.contains("demo-proxy"));
    assert!(rendered.contains("demo-nw-sandbox"));
    assert!(rendered.contains("demo-agent"));
    assert!(rendered.contains("nw-sandbox"));
    assert!(rendered.contains("agent"));
    assert!(rendered.contains("RUN_REMOTE_SERVER"));
    assert!(rendered.contains("http://demo-nw-sandbox:3000/raw"));
    assert!(rendered.contains("sandbox:image"));
    assert!(rendered.contains("agent:image"));
    for old_name in [
        "proxy-pod",
        "sandbox-pod",
        "cli-pod",
        "sandbox-app",
        "cli-app",
    ] {
        assert!(
            !rendered.contains(old_name),
            "rendered YAML contains {old_name}"
        );
    }
}

fn container_mount_paths(rendered: &str, container_name: &str) -> Vec<String> {
    let docs = serde_yaml::Deserializer::from_str(rendered)
        .map(|doc| Value::deserialize(doc).map_err(|_| ()))
        .collect::<std::result::Result<Vec<_>, _>>()
        .unwrap_or_default();

    let mut paths = Vec::new();
    for doc in docs {
        let Some(mapping) = doc.as_mapping() else {
            continue;
        };
        let Some(spec) = mapping.get(Value::String("spec".into())) else {
            continue;
        };
        let Some(spec_mapping) = spec.as_mapping() else {
            continue;
        };
        let Some(containers) = spec_mapping
            .get(Value::String("containers".into()))
            .and_then(Value::as_sequence)
        else {
            continue;
        };
        for container in containers {
            let Some(container_mapping) = container.as_mapping() else {
                continue;
            };
            let Some(name) = container_mapping
                .get(Value::String("name".into()))
                .and_then(Value::as_str)
            else {
                continue;
            };
            if name != container_name {
                continue;
            }
            let Some(mounts) = container_mapping
                .get(Value::String("volumeMounts".into()))
                .and_then(Value::as_sequence)
            else {
                continue;
            };
            for mount in mounts {
                let Some(mount_mapping) = mount.as_mapping() else {
                    continue;
                };
                let Some(path) = mount_mapping
                    .get(Value::String("mountPath".into()))
                    .and_then(Value::as_str)
                else {
                    continue;
                };
                paths.push(path.to_string());
            }
        }
    }

    paths
}

#[test]
fn nw_sandbox_only_mounts_skip_agent() {
    let settings = resolve_network_settings("demo", 1).unwrap();
    let config = Config {
        name: "demo".to_string(),
        nw_sandbox_image: "sandbox:image".to_string(),
        agent_image: "agent:image".to_string(),
        mounts: vec![MountConfig {
            mount_path: "/opt/nw-sandbox-only".to_string(),
            host_path: Some(PathBuf::from("/tmp/nw-sandbox-only")),
            volume: None,
            read_only: true,
            nw_sandbox_only: true,
            ignore: false,
        }],
    };
    let rendered = render_pods_yaml(Path::new("/tmp/project/.cladding"), &config, &settings);
    let sandbox_mounts = container_mount_paths(&rendered, "nw-sandbox");
    let agent_mounts = container_mount_paths(&rendered, "agent");

    assert!(sandbox_mounts.contains(&"/opt/nw-sandbox-only".to_string()));
    assert!(!agent_mounts.contains(&"/opt/nw-sandbox-only".to_string()));
}

#[test]
fn ignore_mount_removes_default_mount() {
    let settings = resolve_network_settings("demo", 1).unwrap();
    let config = Config {
        name: "demo".to_string(),
        nw_sandbox_image: "sandbox:image".to_string(),
        agent_image: "agent:image".to_string(),
        mounts: vec![MountConfig {
            mount_path: "/opt/config".to_string(),
            host_path: None,
            volume: None,
            read_only: true,
            nw_sandbox_only: false,
            ignore: true,
        }],
    };
    let rendered = render_pods_yaml(Path::new("/tmp/project/.cladding"), &config, &settings);
    let sandbox_mounts = container_mount_paths(&rendered, "nw-sandbox");
    let agent_mounts = container_mount_paths(&rendered, "agent");

    assert!(!sandbox_mounts.contains(&"/opt/config".to_string()));
    assert!(!agent_mounts.contains(&"/opt/config".to_string()));
}

#[test]
fn nw_sandbox_only_ignore_keeps_agent_default_mount() {
    let settings = resolve_network_settings("demo", 1).unwrap();
    let config = Config {
        name: "demo".to_string(),
        nw_sandbox_image: "sandbox:image".to_string(),
        agent_image: "agent:image".to_string(),
        mounts: vec![MountConfig {
            mount_path: "/opt/config".to_string(),
            host_path: None,
            volume: None,
            read_only: true,
            nw_sandbox_only: true,
            ignore: true,
        }],
    };
    let rendered = render_pods_yaml(Path::new("/tmp/project/.cladding"), &config, &settings);
    let sandbox_mounts = container_mount_paths(&rendered, "nw-sandbox");
    let agent_mounts = container_mount_paths(&rendered, "agent");

    assert!(!sandbox_mounts.contains(&"/opt/config".to_string()));
    assert!(agent_mounts.contains(&"/opt/config".to_string()));
}
