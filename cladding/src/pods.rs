use crate::config::{Config, MountConfig};
use crate::network::NetworkSettings;
use serde::Deserialize;
use serde_yaml::{Mapping, Value};
use std::path::Path;

const PODS_YAML: &str = include_str!("../../pods.yaml");

pub fn render_pods_yaml(
    project_root: &Path,
    config: &Config,
    network_settings: &NetworkSettings,
) -> String {
    let rendered = PODS_YAML
        .replace("PROJECT_ROOT", &project_root.display().to_string())
        .replace("CLADDING_NAME", &config.name)
        .replace(
            "REPLACE_PROXY_POD_NAME",
            &network_settings.proxy_pod_name,
        )
        .replace(
            "REPLACE_SANDBOX_POD_NAME",
            &network_settings.sandbox_pod_name,
        )
        .replace("REPLACE_CLI_POD_NAME", &network_settings.cli_pod_name)
        .replace("REPLACE_SANDBOX_IMAGE", &config.sandbox_image)
        .replace("REPLACE_CLI_IMAGE", &config.cli_image)
        .replace("REPLACE_PROXY_IP", &network_settings.proxy_ip)
        .replace("REPLACE_SANDBOX_IP", &network_settings.sandbox_ip)
        .replace("REPLACE_CLI_IP", &network_settings.cli_ip);

    let mut docs = match serde_yaml::Deserializer::from_str(&rendered)
        .map(|doc| Value::deserialize(doc).map_err(|_| ()))
        .collect::<std::result::Result<Vec<_>, _>>()
    {
        Ok(docs) => docs,
        Err(_) => return rendered,
    };

    if !config.mounts.is_empty() {
        let custom_mounts = build_custom_mounts(config);
        for doc in &mut docs {
            apply_custom_mounts(doc, &custom_mounts);
        }
    }

    let mut output = String::new();
    for (index, doc) in docs.iter().enumerate() {
        let mut serialized = match serde_yaml::to_string(doc) {
            Ok(serialized) => serialized,
            Err(_) => return rendered,
        };
        if let Some(stripped) = serialized.strip_prefix("---\n") {
            serialized = stripped.to_string();
        }
        if index > 0 {
            output.push_str("---\n");
        }
        output.push_str(&serialized);
    }

    output
}

pub fn host_paths_from_rendered(rendered: &str) -> Vec<String> {
    let docs = match serde_yaml::Deserializer::from_str(rendered)
        .map(|doc| Value::deserialize(doc).map_err(|_| ()))
        .collect::<std::result::Result<Vec<_>, _>>()
    {
        Ok(docs) => docs,
        Err(_) => return Vec::new(),
    };

    let mut paths = Vec::new();
    for doc in docs {
        collect_host_paths_from_doc(&doc, &mut paths);
    }
    paths
}

#[derive(Clone)]
struct CustomMount {
    mount_path: String,
    read_only: bool,
    volume: CustomVolume,
}

#[derive(Clone)]
enum CustomVolume {
    HostPath { path: String },
    EmptyDir,
    Named { claim_name: String },
}

fn build_custom_mounts(config: &Config) -> Vec<CustomMount> {
    let mut mounts = Vec::new();

    for MountConfig {
        mount_path,
        host_path,
        volume,
        read_only,
    } in &config.mounts
    {
        let volume = match (host_path, volume) {
            (Some(path), None) => CustomVolume::HostPath {
                path: path.display().to_string(),
            },
            (None, Some(name)) => CustomVolume::Named {
                claim_name: format!("{}-{name}", config.name),
            },
            (None, None) => CustomVolume::EmptyDir,
            (Some(_), Some(_)) => CustomVolume::EmptyDir,
        };
        mounts.push(CustomMount {
            mount_path: mount_path.clone(),
            read_only: *read_only,
            volume,
        });
    }

    mounts
}

fn apply_custom_mounts(doc: &mut Value, custom_mounts: &[CustomMount]) {
    let Some(spec) = mapping_get_mut(doc, "spec") else {
        return;
    };
    let Some(spec_map) = spec.as_mapping_mut() else {
        return;
    };

    let volumes_key = Value::String("volumes".into());
    let containers_key = Value::String("containers".into());

    let Some(mut volumes_value) = spec_map.remove(&volumes_key) else {
        return;
    };
    let Some(volumes) = volumes_value.as_sequence_mut() else {
        return;
    };
    let Some(containers) = spec_map
        .get_mut(&containers_key)
        .and_then(Value::as_sequence_mut)
    else {
        spec_map.insert(volumes_key, volumes_value);
        return;
    };

    let mut volume_index = volume_index_by_name(volumes);

    for container in containers.iter_mut() {
        let Some(container_map) = container.as_mapping_mut() else {
            continue;
        };
        let Some(name_value) = mapping_get(container_map, "name") else {
            continue;
        };
        let Some(name) = name_value.as_str() else {
            continue;
        };
        if name != "sandbox-app" && name != "cli-app" {
            continue;
        }

        let Some(volume_mounts) = seq_get_mut_mapping(container_map, "volumeMounts") else {
            continue;
        };

        let mut mount_entries = parse_volume_mounts(volume_mounts);
        let mut mount_index = mount_index_by_path(&mount_entries);
        let mut next_custom_index = 0usize;

        for custom in custom_mounts {
            if let Some(&idx) = mount_index.get(&custom.mount_path) {
                let mount_name = mount_entries[idx].name.clone();
                mount_entries[idx].read_only = custom.read_only;
                volume_index = ensure_volume_definition(
                    volumes,
                    volume_index,
                    &mount_name,
                    custom,
                );
            } else {
                next_custom_index += 1;
                let mount_name = format!("custom-mount-{next_custom_index}");
                mount_entries.push(VolumeMountEntry {
                    name: mount_name.clone(),
                    mount_path: custom.mount_path.clone(),
                    read_only: custom.read_only,
                });
                mount_index.insert(custom.mount_path.clone(), mount_entries.len() - 1);
                volume_index = ensure_volume_definition(
                    volumes,
                    volume_index,
                    &mount_name,
                    custom,
                );
            }
        }

        *volume_mounts = mount_entries
            .into_iter()
            .map(|entry| entry.to_value())
            .collect();
    }

    spec_map.insert(volumes_key, volumes_value);
}

#[derive(Clone)]
struct VolumeMountEntry {
    name: String,
    mount_path: String,
    read_only: bool,
}

impl VolumeMountEntry {
    fn to_value(self) -> Value {
        let mut mapping = Mapping::new();
        mapping.insert(Value::String("name".into()), Value::String(self.name));
        mapping.insert(
            Value::String("mountPath".into()),
            Value::String(self.mount_path),
        );
        if self.read_only {
            mapping.insert(Value::String("readOnly".into()), Value::Bool(true));
        }
        Value::Mapping(mapping)
    }
}

fn parse_volume_mounts(volume_mounts: &[Value]) -> Vec<VolumeMountEntry> {
    let mut entries = Vec::new();
    for mount in volume_mounts.iter() {
        let Some(mapping) = mount.as_mapping() else {
            continue;
        };
        let name = mapping
            .get(&Value::String("name".into()))
            .and_then(|value| value.as_str())
            .unwrap_or_default()
            .to_string();
        let mount_path = mapping
            .get(&Value::String("mountPath".into()))
            .and_then(|value| value.as_str())
            .unwrap_or_default()
            .to_string();
        let read_only = mapping
            .get(&Value::String("readOnly".into()))
            .and_then(|value| value.as_bool())
            .unwrap_or(false);

        if !name.is_empty() && !mount_path.is_empty() {
            entries.push(VolumeMountEntry {
                name,
                mount_path,
                read_only,
            });
        }
    }
    entries
}

fn mount_index_by_path(entries: &[VolumeMountEntry]) -> std::collections::HashMap<String, usize> {
    let mut index = std::collections::HashMap::new();
    for (i, entry) in entries.iter().enumerate() {
        index.insert(entry.mount_path.clone(), i);
    }
    index
}

fn volume_index_by_name(volumes: &[Value]) -> std::collections::HashMap<String, usize> {
    let mut index = std::collections::HashMap::new();
    for (i, volume) in volumes.iter().enumerate() {
        let Some(mapping) = volume.as_mapping() else {
            continue;
        };
        let name = mapping
            .get(&Value::String("name".into()))
            .and_then(|value| value.as_str())
            .unwrap_or_default();
        if !name.is_empty() {
            index.insert(name.to_string(), i);
        }
    }
    index
}

fn ensure_volume_definition(
    volumes: &mut Vec<Value>,
    mut volume_index: std::collections::HashMap<String, usize>,
    name: &str,
    custom: &CustomMount,
) -> std::collections::HashMap<String, usize> {
    let volume_value = build_volume_value(name, custom);
    if let Some(idx) = volume_index.get(name).copied() {
        volumes[idx] = volume_value;
    } else {
        volumes.push(volume_value);
        volume_index.insert(name.to_string(), volumes.len() - 1);
    }
    volume_index
}

fn build_volume_value(name: &str, custom: &CustomMount) -> Value {
    let mut mapping = Mapping::new();
    mapping.insert(Value::String("name".into()), Value::String(name.to_string()));
    match &custom.volume {
        CustomVolume::HostPath { path } => {
            let mut host_path = Mapping::new();
            host_path.insert(Value::String("path".into()), Value::String(path.clone()));
            mapping.insert(Value::String("hostPath".into()), Value::Mapping(host_path));
        }
        CustomVolume::EmptyDir => {
            let mut empty_dir = Mapping::new();
            empty_dir.insert(Value::String("medium".into()), Value::String("Memory".into()));
            mapping.insert(Value::String("emptyDir".into()), Value::Mapping(empty_dir));
        }
        CustomVolume::Named { claim_name } => {
            let mut pvc = Mapping::new();
            pvc.insert(
                Value::String("claimName".into()),
                Value::String(claim_name.clone()),
            );
            mapping.insert(
                Value::String("persistentVolumeClaim".into()),
                Value::Mapping(pvc),
            );
        }
    }
    Value::Mapping(mapping)
}

fn mapping_get<'a>(mapping: &'a Mapping, key: &str) -> Option<&'a Value> {
    mapping.get(&Value::String(key.into()))
}

fn mapping_get_mut<'a>(value: &'a mut Value, key: &str) -> Option<&'a mut Value> {
    let mapping = value.as_mapping_mut()?;
    mapping.get_mut(&Value::String(key.into()))
}

fn seq_get_mut_mapping<'a>(mapping: &'a mut Mapping, key: &str) -> Option<&'a mut Vec<Value>> {
    mapping
        .get_mut(&Value::String(key.into()))?
        .as_sequence_mut()
}

fn collect_host_paths_from_doc(doc: &Value, output: &mut Vec<String>) {
    let Some(mapping) = doc.as_mapping() else {
        return;
    };
    let Some(spec) = mapping_get(mapping, "spec") else {
        return;
    };
    let Some(spec_mapping) = spec.as_mapping() else {
        return;
    };
    let Some(volumes) = mapping_get(spec_mapping, "volumes").and_then(Value::as_sequence) else {
        return;
    };

    for volume in volumes {
        let Some(volume_mapping) = volume.as_mapping() else {
            continue;
        };
        let Some(host_path) = mapping_get(volume_mapping, "hostPath") else {
            continue;
        };
        let Some(host_path_mapping) = host_path.as_mapping() else {
            continue;
        };
        let Some(path_value) = mapping_get(host_path_mapping, "path").and_then(Value::as_str)
        else {
            continue;
        };
        output.push(path_value.to_string());
    }
}
