use cladding::config::{
    ExecutionComponentConfig, ExecutionConfig, MountTarget, ResolvedMountConfig,
};
use cladding::network::resolve_network_settings_for_config;
use cladding::pods::render_pods_yaml_v2;
use serde::Deserialize;
use serde_yaml::Value;
use std::collections::HashMap;
use std::path::Path;
use std::path::PathBuf;

#[test]
fn render_pods_yaml_v2_default_config_renders_enabled_components() {
    let config = execution_config(true, false, vec![]);
    let settings = resolve_network_settings_for_config("demo", 1, &config).unwrap();
    let rendered = render_pods_yaml_v2(Path::new("/tmp/project/.cladding"), &config, &settings);

    assert_eq!(
        rendered_doc_names(&rendered),
        vec![
            "empty-mask".to_string(),
            "demo-proxy".to_string(),
            "demo-nw-sandbox".to_string(),
            "demo-agent".to_string(),
        ]
    );
    assert!(!rendered.contains("demo-fs-sandbox"));
    assert!(!rendered.contains("RUN_REMOTE_SERVER"));

    let agent_env = container_env(&rendered, "demo-agent", "instance");
    assert_eq!(
        agent_env.get("RUN_NW_SANDBOX_SERVER").map(String::as_str),
        Some("http://demo-nw-sandbox:3000/raw")
    );
    assert!(!agent_env.contains_key("RUN_FS_SANDBOX_SERVER"));
}

#[test]
fn render_pods_yaml_v2_disabled_nw_removes_nw_rendering_and_envs() {
    let config = execution_config(false, false, vec![]);
    let settings = resolve_network_settings_for_config("demo", 1, &config).unwrap();
    let rendered = render_pods_yaml_v2(Path::new("/tmp/project/.cladding"), &config, &settings);

    assert!(!rendered.contains("demo-nw-sandbox"));
    assert!(!rendered.contains("RUN_NW_SANDBOX_SERVER"));
    assert!(!rendered.contains("RUN_REMOTE_SERVER"));

    let proxy_env = container_env(&rendered, "demo-proxy", "instance");
    assert!(!proxy_env.contains_key("CLADDING_SANDBOX_NAME"));

    let agent_env = container_env(&rendered, "demo-agent", "instance");
    assert!(!agent_env.contains_key("CLADDING_SANDBOX_NAME"));
    assert!(!agent_env.contains_key("RUN_NW_SANDBOX_SERVER"));

    let agent_aliases = host_alias_names(&rendered, "demo-agent");
    assert!(!agent_aliases.contains(&"demo-nw-sandbox".to_string()));
}

#[test]
fn render_pods_yaml_v2_enabled_fs_adds_fs_pod_and_agent_endpoint() {
    let config = execution_config(true, true, vec![]);
    let settings = resolve_network_settings_for_config("demo", 1, &config).unwrap();
    let rendered = render_pods_yaml_v2(Path::new("/tmp/project/.cladding"), &config, &settings);

    assert!(rendered_doc_names(&rendered).contains(&"demo-fs-sandbox".to_string()));

    let fs_container_env = container_env(&rendered, "demo-fs-sandbox", "instance");
    assert_eq!(
        fs_container_env.get("POLICY_DIR").map(String::as_str),
        Some("/opt/config/fs_sandbox")
    );
    assert!(!fs_container_env.contains_key("http_proxy"));
    assert!(!fs_container_env.contains_key("https_proxy"));
    assert_eq!(
        container_command(&rendered, "demo-fs-sandbox", "instance"),
        vec!["mcp-run".to_string()]
    );
    assert!(rendered.contains("jail_fs_sandbox.sh"));

    let agent_env = container_env(&rendered, "demo-agent", "instance");
    assert_eq!(
        agent_env.get("RUN_FS_SANDBOX_SERVER").map(String::as_str),
        Some("http://demo-fs-sandbox:3000/raw")
    );
    let agent_init_env = init_container_env(&rendered, "demo-agent", "agent-node");
    assert_eq!(
        agent_init_env
            .get("RUN_FS_SANDBOX_SERVER")
            .map(String::as_str),
        Some("http://demo-fs-sandbox:3000/raw")
    );
    assert!(host_alias_names(&rendered, "demo-fs-sandbox").is_empty());
}

#[test]
fn targeted_mounts_apply_per_component() {
    let config = execution_config(
        true,
        true,
        vec![
            ResolvedMountConfig {
                mount_path: "/workspace".to_string(),
                host_path: Some(PathBuf::from("/tmp/workspace-ro")),
                volume: None,
                read_only: true,
                targets: vec![MountTarget::Agent],
                ignore: false,
            },
            ResolvedMountConfig {
                mount_path: "/workspace".to_string(),
                host_path: Some(PathBuf::from("/tmp/workspace-fs")),
                volume: None,
                read_only: false,
                targets: vec![MountTarget::FsSandbox],
                ignore: false,
            },
        ],
    );
    let settings = resolve_network_settings_for_config("demo", 1, &config).unwrap();
    let rendered = render_pods_yaml_v2(Path::new("/tmp/project/.cladding"), &config, &settings);

    assert_eq!(
        container_mount_host_path(&rendered, "demo-agent", "instance", "/workspace"),
        Some("/tmp/workspace-ro".to_string())
    );
    assert_eq!(
        container_mount_host_path(&rendered, "demo-fs-sandbox", "instance", "/workspace"),
        Some("/tmp/workspace-fs".to_string())
    );
}

#[test]
fn ignore_mount_removes_default_mount_for_target() {
    let config = execution_config(
        true,
        false,
        vec![ResolvedMountConfig {
            mount_path: "/opt/config".to_string(),
            host_path: None,
            volume: None,
            read_only: true,
            targets: vec![MountTarget::Agent],
            ignore: true,
        }],
    );
    let settings = resolve_network_settings_for_config("demo", 1, &config).unwrap();
    let rendered = render_pods_yaml_v2(Path::new("/tmp/project/.cladding"), &config, &settings);

    assert!(
        !container_mount_paths(&rendered, "demo-agent", "instance")
            .contains(&"/opt/config".to_string())
    );
    assert!(
        container_mount_paths(&rendered, "demo-nw-sandbox", "instance")
            .contains(&"/opt/config".to_string())
    );
}

fn execution_config(
    nw_enabled: bool,
    fs_enabled: bool,
    mounts: Vec<ResolvedMountConfig>,
) -> ExecutionConfig {
    ExecutionConfig {
        name: "demo".to_string(),
        agent: ExecutionComponentConfig {
            enabled: true,
            image: "agent:image".to_string(),
        },
        nw_sandbox: nw_enabled.then(|| ExecutionComponentConfig {
            enabled: true,
            image: "sandbox:image".to_string(),
        }),
        fs_sandbox: fs_enabled.then(|| ExecutionComponentConfig {
            enabled: true,
            image: "fs:image".to_string(),
        }),
        mounts,
    }
}

fn rendered_docs(rendered: &str) -> Vec<Value> {
    serde_yaml::Deserializer::from_str(rendered)
        .map(|doc| Value::deserialize(doc).expect("parse yaml doc"))
        .collect()
}

fn rendered_doc_names(rendered: &str) -> Vec<String> {
    rendered_docs(rendered)
        .into_iter()
        .filter_map(|doc| {
            let mapping = doc.as_mapping()?;
            let metadata = mapping
                .get(Value::String("metadata".into()))?
                .as_mapping()?;
            metadata
                .get(Value::String("name".into()))
                .and_then(Value::as_str)
                .map(|value| value.to_string())
        })
        .collect()
}

fn container_env(rendered: &str, pod_name: &str, container_name: &str) -> HashMap<String, String> {
    let container = container_doc(rendered, pod_name, container_name).expect("container");
    env_from_container_value(&container)
}

fn init_container_env(
    rendered: &str,
    pod_name: &str,
    container_name: &str,
) -> HashMap<String, String> {
    let container = init_container_doc(rendered, pod_name, container_name).expect("init container");
    env_from_container_value(&container)
}

fn env_from_container_value(container: &Value) -> HashMap<String, String> {
    let envs = container
        .get(Value::String("env".into()))
        .and_then(Value::as_sequence)
        .cloned()
        .unwrap_or_default();

    let mut output = HashMap::new();
    for env in envs {
        let Some(mapping) = env.as_mapping() else {
            continue;
        };
        let Some(name) = mapping
            .get(Value::String("name".into()))
            .and_then(Value::as_str)
        else {
            continue;
        };
        let Some(value) = mapping
            .get(Value::String("value".into()))
            .and_then(Value::as_str)
        else {
            continue;
        };
        output.insert(name.to_string(), value.to_string());
    }

    output
}

fn container_command(rendered: &str, pod_name: &str, container_name: &str) -> Vec<String> {
    let container = container_doc(rendered, pod_name, container_name).expect("container");
    container
        .get(Value::String("command".into()))
        .and_then(Value::as_sequence)
        .map(|items| {
            items
                .iter()
                .filter_map(Value::as_str)
                .map(|value| value.to_string())
                .collect()
        })
        .unwrap_or_default()
}

fn host_alias_names(rendered: &str, pod_name: &str) -> Vec<String> {
    let doc = pod_doc(rendered, pod_name).expect("pod doc");
    let host_aliases = doc
        .get(Value::String("spec".into()))
        .and_then(Value::as_mapping)
        .and_then(|spec| spec.get(Value::String("hostAliases".into())))
        .and_then(Value::as_sequence)
        .cloned()
        .unwrap_or_default();

    let mut output = Vec::new();
    for alias in host_aliases {
        let Some(mapping) = alias.as_mapping() else {
            continue;
        };
        let Some(hostnames) = mapping
            .get(Value::String("hostnames".into()))
            .and_then(Value::as_sequence)
        else {
            continue;
        };
        for hostname in hostnames {
            if let Some(hostname) = hostname.as_str() {
                output.push(hostname.to_string());
            }
        }
    }

    output
}

fn container_mount_paths(rendered: &str, pod_name: &str, container_name: &str) -> Vec<String> {
    container_mounts(rendered, pod_name, container_name)
        .into_keys()
        .collect()
}

fn container_mount_host_path(
    rendered: &str,
    pod_name: &str,
    container_name: &str,
    mount_path: &str,
) -> Option<String> {
    container_mounts(rendered, pod_name, container_name)
        .get(mount_path)
        .cloned()
}

fn container_mounts(
    rendered: &str,
    pod_name: &str,
    container_name: &str,
) -> HashMap<String, String> {
    let doc = pod_doc(rendered, pod_name).expect("pod doc");
    let spec = doc
        .get(Value::String("spec".into()))
        .and_then(Value::as_mapping)
        .expect("spec");
    let containers = spec
        .get(Value::String("containers".into()))
        .and_then(Value::as_sequence)
        .expect("containers");
    let volumes = spec
        .get(Value::String("volumes".into()))
        .and_then(Value::as_sequence)
        .cloned()
        .unwrap_or_default();

    let mut volume_lookup = HashMap::new();
    for volume in volumes {
        let Some(mapping) = volume.as_mapping() else {
            continue;
        };
        let Some(name) = mapping
            .get(Value::String("name".into()))
            .and_then(Value::as_str)
        else {
            continue;
        };
        let Some(host_path) = mapping
            .get(Value::String("hostPath".into()))
            .and_then(Value::as_mapping)
            .and_then(|host_path| host_path.get(Value::String("path".into())))
            .and_then(Value::as_str)
        else {
            continue;
        };
        volume_lookup.insert(name.to_string(), host_path.to_string());
    }

    for container in containers {
        let Some(mapping) = container.as_mapping() else {
            continue;
        };
        if mapping
            .get(Value::String("name".into()))
            .and_then(Value::as_str)
            != Some(container_name)
        {
            continue;
        }

        let mounts = mapping
            .get(Value::String("volumeMounts".into()))
            .and_then(Value::as_sequence)
            .cloned()
            .unwrap_or_default();

        let mut output = HashMap::new();
        for mount in mounts {
            let Some(mount_map) = mount.as_mapping() else {
                continue;
            };
            let Some(mount_path) = mount_map
                .get(Value::String("mountPath".into()))
                .and_then(Value::as_str)
            else {
                continue;
            };
            let Some(volume_name) = mount_map
                .get(Value::String("name".into()))
                .and_then(Value::as_str)
            else {
                continue;
            };
            if let Some(host_path) = volume_lookup.get(volume_name) {
                output.insert(mount_path.to_string(), host_path.clone());
            }
        }
        return output;
    }

    HashMap::new()
}

fn pod_doc(rendered: &str, pod_name: &str) -> Option<Value> {
    rendered_docs(rendered).into_iter().find(|doc| {
        let Some(mapping) = doc.as_mapping() else {
            return false;
        };
        let Some(metadata) = mapping
            .get(Value::String("metadata".into()))
            .and_then(Value::as_mapping)
        else {
            return false;
        };
        metadata
            .get(Value::String("name".into()))
            .and_then(Value::as_str)
            == Some(pod_name)
    })
}

fn container_doc(rendered: &str, pod_name: &str, container_name: &str) -> Option<Value> {
    pod_container_doc(rendered, pod_name, "containers", container_name)
}

fn init_container_doc(rendered: &str, pod_name: &str, container_name: &str) -> Option<Value> {
    pod_container_doc(rendered, pod_name, "initContainers", container_name)
}

fn pod_container_doc(
    rendered: &str,
    pod_name: &str,
    container_key: &str,
    container_name: &str,
) -> Option<Value> {
    let doc = pod_doc(rendered, pod_name)?;
    let spec = doc
        .get(Value::String("spec".into()))
        .and_then(Value::as_mapping)?;
    let containers = spec
        .get(Value::String(container_key.into()))
        .and_then(Value::as_sequence)?;

    containers.iter().find_map(|container| {
        let mapping = container.as_mapping()?;
        if mapping
            .get(Value::String("name".into()))
            .and_then(Value::as_str)
            != Some(container_name)
        {
            return None;
        }
        Some(container.clone())
    })
}
