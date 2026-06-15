use crate::config::{ExecutionConfig, MountTarget, ResolvedMountConfig};
use crate::network::NetworkSettings;
use serde::Deserialize;
use serde_yaml::{Mapping, Value};
use std::collections::{HashMap, HashSet};
use std::path::Path;

const CONFIGMAP_TEMPLATE: &str = include_str!("pod_templates/configmap.yaml");
const PROXY_TEMPLATE: &str = include_str!("pod_templates/proxy.yaml");
const NW_SANDBOX_TEMPLATE: &str = include_str!("pod_templates/nw-sandbox.yaml");
const FS_SANDBOX_TEMPLATE: &str = include_str!("pod_templates/fs-sandbox.yaml");
const AGENT_TEMPLATE: &str = include_str!("pod_templates/agent.yaml");

pub fn render_pods_yaml_v2(
    project_root: &Path,
    config: &ExecutionConfig,
    network_settings: &NetworkSettings,
) -> String {
    let templates = build_templates(project_root, config, network_settings);
    let mut docs = Vec::new();
    let mut rendered_docs = Vec::new();

    for template in templates {
        let rendered = render_template(template.template, &template.replacements);
        rendered_docs.push(rendered.clone());
        let Some(mut doc) = parse_yaml_doc(&rendered) else {
            return rendered_docs.join("---\n");
        };
        apply_component_mutations(&mut doc, template.kind, network_settings);
        apply_custom_mounts(&mut doc, template.kind, &config.name, &config.mounts);
        docs.push(doc);
    }

    serialize_docs(&docs).unwrap_or_else(|| rendered_docs.join("---\n"))
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
    targets: Vec<MountTarget>,
    ignore: bool,
}

#[derive(Clone)]
enum CustomVolume {
    HostPath { path: String },
    EmptyDir,
    Named { claim_name: String },
}

#[derive(Clone, Copy)]
enum TemplateKind {
    ConfigMap,
    Proxy,
    NwSandbox,
    FsSandbox,
    Agent,
}

struct TemplateDoc {
    kind: TemplateKind,
    template: &'static str,
    replacements: Vec<(&'static str, String)>,
}

fn build_templates(
    project_root: &Path,
    config: &ExecutionConfig,
    network_settings: &NetworkSettings,
) -> Vec<TemplateDoc> {
    let root = project_root.display().to_string();
    let fs_settings = network_settings.fs_sandbox.as_ref().cloned();

    let mut docs = vec![TemplateDoc {
        kind: TemplateKind::ConfigMap,
        template: CONFIGMAP_TEMPLATE,
        replacements: Vec::new(),
    }];

    docs.push(TemplateDoc {
        kind: TemplateKind::Proxy,
        template: PROXY_TEMPLATE,
        replacements: vec![
            ("PROJECT_ROOT", root.clone()),
            ("CLADDING_NAME", config.name.clone()),
            ("REPLACE_PROXY_NAME", network_settings.proxy_name.clone()),
            ("REPLACE_AGENT_NAME", network_settings.agent_name.clone()),
            ("REPLACE_AGENT_IP", network_settings.agent_ip.clone()),
            (
                "REPLACE_SANDBOX_NAME",
                network_settings
                    .nw_sandbox
                    .as_ref()
                    .map(|component| component.name.clone())
                    .unwrap_or_default(),
            ),
            (
                "REPLACE_SANDBOX_IP",
                network_settings
                    .nw_sandbox
                    .as_ref()
                    .map(|component| component.ip.clone())
                    .unwrap_or_default(),
            ),
        ],
    });

    if let Some(nw_settings) = network_settings.nw_sandbox.as_ref() {
        docs.push(TemplateDoc {
            kind: TemplateKind::NwSandbox,
            template: NW_SANDBOX_TEMPLATE,
            replacements: vec![
                ("PROJECT_ROOT", root.clone()),
                ("CLADDING_NAME", config.name.clone()),
                ("REPLACE_PROXY_NAME", network_settings.proxy_name.clone()),
                ("REPLACE_SANDBOX_NAME", nw_settings.name.clone()),
                ("REPLACE_AGENT_NAME", network_settings.agent_name.clone()),
                (
                    "REPLACE_NW_SANDBOX_IMAGE",
                    config.nw_sandbox_image().to_string(),
                ),
            ],
        });
    }

    if let Some(fs_settings_for_template) = fs_settings.clone() {
        docs.push(TemplateDoc {
            kind: TemplateKind::FsSandbox,
            template: FS_SANDBOX_TEMPLATE,
            replacements: vec![
                ("PROJECT_ROOT", root.clone()),
                ("CLADDING_NAME", config.name.clone()),
                (
                    "REPLACE_FS_SANDBOX_NAME",
                    fs_settings_for_template.name.clone(),
                ),
                ("REPLACE_FS_SANDBOX_IP", fs_settings_for_template.ip.clone()),
                (
                    "REPLACE_FS_SANDBOX_IMAGE",
                    config.fs_sandbox_image().to_string(),
                ),
            ],
        });
    }

    docs.push(TemplateDoc {
        kind: TemplateKind::Agent,
        template: AGENT_TEMPLATE,
        replacements: vec![
            ("PROJECT_ROOT", root),
            ("CLADDING_NAME", config.name.clone()),
            ("REPLACE_AGENT_IMAGE", config.agent_image().to_string()),
            ("REPLACE_PROXY_NAME", network_settings.proxy_name.clone()),
            ("REPLACE_PROXY_IP", network_settings.proxy_ip.clone()),
            ("REPLACE_AGENT_NAME", network_settings.agent_name.clone()),
            ("REPLACE_AGENT_IP", network_settings.agent_ip.clone()),
            (
                "REPLACE_SANDBOX_NAME",
                network_settings
                    .nw_sandbox
                    .as_ref()
                    .map(|component| component.name.clone())
                    .unwrap_or_default(),
            ),
            (
                "REPLACE_SANDBOX_IP",
                network_settings
                    .nw_sandbox
                    .as_ref()
                    .map(|component| component.ip.clone())
                    .unwrap_or_default(),
            ),
            (
                "REPLACE_FS_SANDBOX_NAME",
                fs_settings
                    .as_ref()
                    .map(|component| component.name.clone())
                    .unwrap_or_default(),
            ),
            (
                "REPLACE_FS_SANDBOX_IP",
                fs_settings
                    .as_ref()
                    .map(|component| component.ip.clone())
                    .unwrap_or_default(),
            ),
            (
                "REPLACE_RUN_NW_SANDBOX_SERVER",
                network_settings
                    .nw_sandbox
                    .as_ref()
                    .map(|component| format!("http://{}:3000/raw", component.name))
                    .unwrap_or_default(),
            ),
            (
                "REPLACE_RUN_FS_SANDBOX_SERVER",
                fs_settings
                    .as_ref()
                    .map(|component| format!("http://{}:3000/raw", component.name))
                    .unwrap_or_default(),
            ),
            ("REPLACE_NO_PROXY", build_no_proxy(network_settings)),
        ],
    });

    docs
}

fn build_no_proxy(network_settings: &NetworkSettings) -> String {
    let mut entries = Vec::new();
    if let Some(nw) = &network_settings.nw_sandbox {
        entries.push(nw.name.clone());
    }
    if let Some(fs) = &network_settings.fs_sandbox {
        entries.push(fs.name.clone());
    }
    entries.push("localhost".to_string());
    entries.push("127.0.0.1".to_string());
    entries.join(",")
}

fn apply_component_mutations(
    doc: &mut Value,
    kind: TemplateKind,
    network_settings: &NetworkSettings,
) {
    match kind {
        TemplateKind::ConfigMap => {}
        TemplateKind::Proxy => customize_proxy_doc(doc, network_settings),
        TemplateKind::NwSandbox => customize_nw_doc(doc, network_settings),
        TemplateKind::FsSandbox => customize_fs_doc(doc, network_settings),
        TemplateKind::Agent => customize_agent_doc(doc, network_settings),
    }
}

fn customize_proxy_doc(doc: &mut Value, network_settings: &NetworkSettings) {
    let Some(spec) = pod_spec_mut(doc) else {
        return;
    };

    let mut allowed_hostnames = HashSet::from([network_settings.agent_name.as_str()]);
    if let Some(nw) = &network_settings.nw_sandbox {
        allowed_hostnames.insert(nw.name.as_str());
    }

    retain_host_aliases(spec, &allowed_hostnames);
    set_host_alias_ip(
        spec,
        &network_settings.agent_name,
        &network_settings.agent_ip,
    );
    if let Some(nw) = &network_settings.nw_sandbox {
        set_host_alias_ip(spec, &nw.name, &nw.ip);
    }

    let Some(containers) = spec
        .get_mut(Value::String("containers".into()))
        .and_then(Value::as_sequence_mut)
    else {
        return;
    };

    let Some(container) = container_by_name_mut(containers, "instance") else {
        return;
    };

    let allowed_envs = if network_settings.nw_sandbox.is_some() {
        HashSet::from([
            "CLADDING_PROXY_NAME",
            "CLADDING_SANDBOX_NAME",
            "CLADDING_AGENT_NAME",
        ])
    } else {
        HashSet::from(["CLADDING_PROXY_NAME", "CLADDING_AGENT_NAME"])
    };
    retain_env_names(container, &allowed_envs);
    set_env_value(
        container,
        "CLADDING_PROXY_NAME",
        Some(network_settings.proxy_name.clone()),
    );
    set_env_value(
        container,
        "CLADDING_AGENT_NAME",
        Some(network_settings.agent_name.clone()),
    );
    if let Some(nw) = &network_settings.nw_sandbox {
        set_env_value(container, "CLADDING_SANDBOX_NAME", Some(nw.name.clone()));
    }
}

fn customize_nw_doc(_doc: &mut Value, _network_settings: &NetworkSettings) {}

fn customize_fs_doc(_doc: &mut Value, _network_settings: &NetworkSettings) {}

fn customize_agent_doc(doc: &mut Value, network_settings: &NetworkSettings) {
    let Some(spec) = pod_spec_mut(doc) else {
        return;
    };

    let mut allowed_hostnames = HashSet::from([network_settings.proxy_name.as_str()]);
    if let Some(nw) = &network_settings.nw_sandbox {
        allowed_hostnames.insert(nw.name.as_str());
    }
    if let Some(fs) = &network_settings.fs_sandbox {
        allowed_hostnames.insert(fs.name.as_str());
    }

    retain_host_aliases(spec, &allowed_hostnames);
    set_host_alias_ip(
        spec,
        &network_settings.proxy_name,
        &network_settings.proxy_ip,
    );
    if let Some(nw) = &network_settings.nw_sandbox {
        set_host_alias_ip(spec, &nw.name, &nw.ip);
    }
    if let Some(fs) = &network_settings.fs_sandbox {
        set_host_alias_ip(spec, &fs.name, &fs.ip);
    }

    let Some(containers) = spec
        .get_mut(Value::String("containers".into()))
        .and_then(Value::as_sequence_mut)
    else {
        return;
    };

    let Some(container) = container_by_name_mut(containers, "instance") else {
        return;
    };

    let mut allowed_envs = HashSet::from([
        "PATH",
        "CLADDING_PROXY_NAME",
        "CLADDING_AGENT_NAME",
        "http_proxy",
        "https_proxy",
        "HTTP_PROXY",
        "HTTPS_PROXY",
        "no_proxy",
        "NO_PROXY",
    ]);
    if network_settings.nw_sandbox.is_some() {
        allowed_envs.insert("CLADDING_SANDBOX_NAME");
        allowed_envs.insert("RUN_NW_SANDBOX_SERVER");
    }
    if network_settings.fs_sandbox.is_some() {
        allowed_envs.insert("RUN_FS_SANDBOX_SERVER");
    }

    retain_env_names(container, &allowed_envs);
    set_env_value(
        container,
        "PATH",
        Some(
            "/opt/tools/bin:/usr/local/sbin:/usr/local/bin:/usr/sbin:/usr/bin:/sbin:/bin"
                .to_string(),
        ),
    );
    set_env_value(
        container,
        "CLADDING_PROXY_NAME",
        Some(network_settings.proxy_name.clone()),
    );
    set_env_value(
        container,
        "CLADDING_AGENT_NAME",
        Some(network_settings.agent_name.clone()),
    );
    if let Some(nw) = &network_settings.nw_sandbox {
        set_env_value(container, "CLADDING_SANDBOX_NAME", Some(nw.name.clone()));
        set_env_value(
            container,
            "RUN_NW_SANDBOX_SERVER",
            Some(format!("http://{}:3000/raw", nw.name)),
        );
    }
    if let Some(fs) = &network_settings.fs_sandbox {
        set_env_value(
            container,
            "RUN_FS_SANDBOX_SERVER",
            Some(format!("http://{}:3000/raw", fs.name)),
        );
    }
    set_env_value(
        container,
        "http_proxy",
        Some(format!("http://{}:8080", network_settings.proxy_name)),
    );
    set_env_value(
        container,
        "https_proxy",
        Some(format!("http://{}:8080", network_settings.proxy_name)),
    );
    set_env_value(
        container,
        "HTTP_PROXY",
        Some(format!("http://{}:8080", network_settings.proxy_name)),
    );
    set_env_value(
        container,
        "HTTPS_PROXY",
        Some(format!("http://{}:8080", network_settings.proxy_name)),
    );
    set_env_value(
        container,
        "no_proxy",
        Some(build_no_proxy(network_settings)),
    );
    set_env_value(
        container,
        "NO_PROXY",
        Some(build_no_proxy(network_settings)),
    );

    let Some(init_containers) = spec
        .get_mut(Value::String("initContainers".into()))
        .and_then(Value::as_sequence_mut)
    else {
        return;
    };

    let Some(init_container) = container_by_name_mut(init_containers, "agent-node") else {
        return;
    };

    let mut init_allowed_envs = HashSet::from(["CLADDING_PROXY_NAME", "CLADDING_AGENT_NAME"]);
    if network_settings.nw_sandbox.is_some() {
        init_allowed_envs.insert("CLADDING_SANDBOX_NAME");
        init_allowed_envs.insert("RUN_NW_SANDBOX_SERVER");
    }
    if network_settings.fs_sandbox.is_some() {
        init_allowed_envs.insert("RUN_FS_SANDBOX_SERVER");
    }
    retain_env_names(init_container, &init_allowed_envs);
    set_env_value(
        init_container,
        "CLADDING_PROXY_NAME",
        Some(network_settings.proxy_name.clone()),
    );
    set_env_value(
        init_container,
        "CLADDING_AGENT_NAME",
        Some(network_settings.agent_name.clone()),
    );
    if let Some(nw) = &network_settings.nw_sandbox {
        set_env_value(
            init_container,
            "CLADDING_SANDBOX_NAME",
            Some(nw.name.clone()),
        );
        set_env_value(
            init_container,
            "RUN_NW_SANDBOX_SERVER",
            Some(format!("http://{}:3000/raw", nw.name)),
        );
    }
    if let Some(fs) = &network_settings.fs_sandbox {
        set_env_value(
            init_container,
            "RUN_FS_SANDBOX_SERVER",
            Some(format!("http://{}:3000/raw", fs.name)),
        );
    }
}

fn parse_yaml_doc(rendered: &str) -> Option<Value> {
    serde_yaml::from_str(rendered).ok()
}

fn serialize_docs(docs: &[Value]) -> Option<String> {
    let mut output = String::new();
    for (index, doc) in docs.iter().enumerate() {
        let mut serialized = serde_yaml::to_string(doc).ok()?;
        if let Some(stripped) = serialized.strip_prefix("---\n") {
            serialized = stripped.to_string();
        }
        if index > 0 {
            output.push_str("---\n");
        }
        output.push_str(&serialized);
    }
    Some(output)
}

fn render_template(template: &str, replacements: &[(&'static str, String)]) -> String {
    replacements
        .iter()
        .fold(template.to_string(), |acc, (from, to)| {
            acc.replace(from, to)
        })
}

fn build_custom_mounts(project_name: &str, mounts: &[ResolvedMountConfig]) -> Vec<CustomMount> {
    let mut custom_mounts = Vec::new();

    for ResolvedMountConfig {
        mount_path,
        host_path,
        volume,
        read_only,
        targets,
        ignore,
    } in mounts
    {
        let volume = match (host_path, volume) {
            (Some(path), None) => CustomVolume::HostPath {
                path: path.display().to_string(),
            },
            (None, Some(name)) => CustomVolume::Named {
                claim_name: format!("{project_name}-{name}"),
            },
            (None, None) => CustomVolume::EmptyDir,
            (Some(_), Some(_)) => CustomVolume::EmptyDir,
        };
        custom_mounts.push(CustomMount {
            mount_path: mount_path.clone(),
            read_only: *read_only,
            volume,
            targets: targets.clone(),
            ignore: *ignore,
        });
    }

    custom_mounts
}

fn apply_custom_mounts(
    doc: &mut Value,
    kind: TemplateKind,
    project_name: &str,
    mounts: &[ResolvedMountConfig],
) {
    let Some(target) = template_mount_target(kind) else {
        return;
    };

    let custom_mounts = build_custom_mounts(project_name, mounts);

    let Some(spec) = pod_spec_mut(doc) else {
        return;
    };
    let spec_map: &mut Mapping = spec;

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
        if mapping_get(container_map, "name").and_then(Value::as_str) != Some("instance") {
            continue;
        }

        let Some(volume_mounts) = seq_get_mut_mapping(container_map, "volumeMounts") else {
            continue;
        };

        let mut mount_entries = parse_volume_mounts(volume_mounts);
        let mut mount_index = mount_index_by_path(&mount_entries);
        let mut next_custom_index = 0usize;

        for custom in custom_mounts
            .iter()
            .filter(|mount| mount.targets.contains(&target))
        {
            if let Some(&idx) = mount_index.get(&custom.mount_path) {
                if custom.ignore {
                    mount_entries.remove(idx);
                    mount_index = mount_index_by_path(&mount_entries);
                    continue;
                }
                let mount_name = mount_entries[idx].name.clone();
                mount_entries[idx].read_only = custom.read_only;
                volume_index = ensure_volume_definition(volumes, volume_index, &mount_name, custom);
            } else if custom.ignore {
                continue;
            } else {
                next_custom_index += 1;
                let mount_name = format!("custom-mount-{next_custom_index}");
                mount_entries.push(VolumeMountEntry {
                    name: mount_name.clone(),
                    mount_path: custom.mount_path.clone(),
                    read_only: custom.read_only,
                });
                mount_index.insert(custom.mount_path.clone(), mount_entries.len() - 1);
                volume_index = ensure_volume_definition(volumes, volume_index, &mount_name, custom);
            }
        }

        *volume_mounts = mount_entries
            .into_iter()
            .map(|entry| entry.into_value())
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
    fn into_value(self) -> Value {
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
            .get(Value::String("name".into()))
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_string();
        let mount_path = mapping
            .get(Value::String("mountPath".into()))
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_string();
        let read_only = mapping
            .get(Value::String("readOnly".into()))
            .and_then(Value::as_bool)
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

fn mount_index_by_path(entries: &[VolumeMountEntry]) -> HashMap<String, usize> {
    let mut index = HashMap::new();
    for (i, entry) in entries.iter().enumerate() {
        index.insert(entry.mount_path.clone(), i);
    }
    index
}

fn volume_index_by_name(volumes: &[Value]) -> HashMap<String, usize> {
    let mut index = HashMap::new();
    for (i, volume) in volumes.iter().enumerate() {
        let Some(mapping) = volume.as_mapping() else {
            continue;
        };
        let name = mapping
            .get(Value::String("name".into()))
            .and_then(Value::as_str)
            .unwrap_or_default();
        if !name.is_empty() {
            index.insert(name.to_string(), i);
        }
    }
    index
}

fn ensure_volume_definition(
    volumes: &mut Vec<Value>,
    mut volume_index: HashMap<String, usize>,
    name: &str,
    custom: &CustomMount,
) -> HashMap<String, usize> {
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
    mapping.insert(
        Value::String("name".into()),
        Value::String(name.to_string()),
    );
    match &custom.volume {
        CustomVolume::HostPath { path } => {
            let mut host_path = Mapping::new();
            host_path.insert(Value::String("path".into()), Value::String(path.clone()));
            mapping.insert(Value::String("hostPath".into()), Value::Mapping(host_path));
        }
        CustomVolume::EmptyDir => {
            let mut config_map = Mapping::new();
            config_map.insert(
                Value::String("name".into()),
                Value::String("empty-mask".into()),
            );
            mapping.insert(
                Value::String("configMap".into()),
                Value::Mapping(config_map),
            );
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
    mapping.get(Value::String(key.into()))
}

fn mapping_get_mut<'a>(value: &'a mut Value, key: &str) -> Option<&'a mut Value> {
    let mapping = value.as_mapping_mut()?;
    mapping.get_mut(Value::String(key.into()))
}

fn seq_get_mut_mapping<'a>(mapping: &'a mut Mapping, key: &str) -> Option<&'a mut Vec<Value>> {
    mapping
        .get_mut(Value::String(key.into()))?
        .as_sequence_mut()
}

fn pod_spec_mut(doc: &mut Value) -> Option<&mut Mapping> {
    let spec = mapping_get_mut(doc, "spec")?;
    spec.as_mapping_mut()
}

fn container_by_name_mut<'a>(containers: &'a mut [Value], name: &str) -> Option<&'a mut Mapping> {
    for container in containers.iter_mut() {
        let Some(container_map) = container.as_mapping_mut() else {
            continue;
        };
        if mapping_get(container_map, "name").and_then(Value::as_str) == Some(name) {
            return Some(container_map);
        }
    }
    None
}

fn template_mount_target(kind: TemplateKind) -> Option<MountTarget> {
    match kind {
        TemplateKind::Agent => Some(MountTarget::Agent),
        TemplateKind::NwSandbox => Some(MountTarget::NwSandbox),
        TemplateKind::FsSandbox => Some(MountTarget::FsSandbox),
        TemplateKind::ConfigMap | TemplateKind::Proxy => None,
    }
}

fn retain_host_aliases(spec: &mut Mapping, allowed: &HashSet<&str>) {
    let Some(host_aliases) = spec
        .get_mut(Value::String("hostAliases".into()))
        .and_then(Value::as_sequence_mut)
    else {
        return;
    };

    host_aliases.retain(|entry| {
        host_alias_hostname(entry)
            .map(|hostname| allowed.contains(hostname))
            .unwrap_or(false)
    });
}

fn set_host_alias_ip(spec: &mut Mapping, hostname: &str, ip: &str) {
    let Some(host_aliases) = spec
        .get_mut(Value::String("hostAliases".into()))
        .and_then(Value::as_sequence_mut)
    else {
        return;
    };

    for entry in host_aliases.iter_mut() {
        let hostname_matches = host_alias_hostname(entry) == Some(hostname);
        let Some(mapping) = entry.as_mapping_mut() else {
            continue;
        };
        if hostname_matches {
            mapping.insert(Value::String("ip".into()), Value::String(ip.to_string()));
            return;
        }
    }
}

fn host_alias_hostname(entry: &Value) -> Option<&str> {
    let mapping = entry.as_mapping()?;
    let hostnames = mapping
        .get(Value::String("hostnames".into()))?
        .as_sequence()?;
    hostnames.first()?.as_str()
}

fn retain_env_names(container: &mut Mapping, allowed: &HashSet<&str>) {
    let Some(envs) = container
        .get_mut(Value::String("env".into()))
        .and_then(Value::as_sequence_mut)
    else {
        return;
    };

    envs.retain(|entry| {
        env_name(entry)
            .map(|name| allowed.contains(name))
            .unwrap_or(false)
    });
}

fn set_env_value(container: &mut Mapping, name: &str, value: Option<String>) {
    let Some(envs) = container
        .get_mut(Value::String("env".into()))
        .and_then(Value::as_sequence_mut)
    else {
        return;
    };

    if let Some(value) = value {
        for entry in envs.iter_mut() {
            let name_matches = env_name(entry) == Some(name);
            let Some(mapping) = entry.as_mapping_mut() else {
                continue;
            };
            if name_matches {
                mapping.insert(Value::String("value".into()), Value::String(value));
                return;
            }
        }

        let mut mapping = Mapping::new();
        mapping.insert(
            Value::String("name".into()),
            Value::String(name.to_string()),
        );
        mapping.insert(Value::String("value".into()), Value::String(value));
        envs.push(Value::Mapping(mapping));
    } else {
        envs.retain(|entry| env_name(entry) != Some(name));
    }
}

fn env_name(entry: &Value) -> Option<&str> {
    let mapping = entry.as_mapping()?;
    mapping
        .get(Value::String("name".into()))
        .and_then(Value::as_str)
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
