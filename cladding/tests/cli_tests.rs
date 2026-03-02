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
    let config = Config {
        name: "demo".to_string(),
        sandbox_image: "sandbox:image".to_string(),
        cli_image: "cli:image".to_string(),
        mounts: Vec::new(),
    };
    let rendered = render_pods_yaml(Path::new("/tmp/project/.cladding"), &config, &settings);

    assert!(!rendered.contains("REPLACE_PROXY_POD_NAME"));
    assert!(!rendered.contains("REPLACE_CLI_IMAGE"));
    assert!(rendered.contains("demo-proxy-pod"));
    assert!(rendered.contains("sandbox:image"));
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
        let Some(spec) = mapping.get(&Value::String("spec".into())) else {
            continue;
        };
        let Some(spec_mapping) = spec.as_mapping() else {
            continue;
        };
        let Some(containers) = spec_mapping
            .get(&Value::String("containers".into()))
            .and_then(Value::as_sequence)
        else {
            continue;
        };
        for container in containers {
            let Some(container_mapping) = container.as_mapping() else {
                continue;
            };
            let Some(name) = container_mapping
                .get(&Value::String("name".into()))
                .and_then(Value::as_str)
            else {
                continue;
            };
            if name != container_name {
                continue;
            }
            let Some(mounts) = container_mapping
                .get(&Value::String("volumeMounts".into()))
                .and_then(Value::as_sequence)
            else {
                continue;
            };
            for mount in mounts {
                let Some(mount_mapping) = mount.as_mapping() else {
                    continue;
                };
                let Some(path) = mount_mapping
                    .get(&Value::String("mountPath".into()))
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
fn sandbox_only_mounts_skip_cli() {
    let settings = resolve_network_settings("demo", 1).unwrap();
    let config = Config {
        name: "demo".to_string(),
        sandbox_image: "sandbox:image".to_string(),
        cli_image: "cli:image".to_string(),
        mounts: vec![MountConfig {
            mount_path: "/opt/sandbox-only".to_string(),
            host_path: Some(PathBuf::from("/tmp/sandbox-only")),
            volume: None,
            read_only: true,
            sandbox_only: true,
        }],
    };
    let rendered = render_pods_yaml(Path::new("/tmp/project/.cladding"), &config, &settings);
    let sandbox_mounts = container_mount_paths(&rendered, "sandbox-app");
    let cli_mounts = container_mount_paths(&rendered, "cli-app");

    assert!(sandbox_mounts.contains(&"/opt/sandbox-only".to_string()));
    assert!(!cli_mounts.contains(&"/opt/sandbox-only".to_string()));
}
