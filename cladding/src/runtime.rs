use crate::config::{ExecutionConfig, MountTarget, ResolvedMountConfig};
use crate::network::{ComponentNetworkSettings, NetworkSettings};
use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};

#[derive(Debug, Clone)]
pub struct RuntimeSpec {
    pub project_name: String,
    pub project_root: PathBuf,
    pub project_root_label: String,
    pub network_settings: NetworkSettings,
    pub custom_mounts: Vec<RuntimeCustomMount>,
    pub proxy: RuntimePod,
    pub agent: RuntimePod,
    pub nw_sandbox: Option<RuntimePod>,
    pub fs_sandbox: Option<RuntimePod>,
}

impl RuntimeSpec {
    pub fn build(
        project_root: &Path,
        config: &ExecutionConfig,
        network_settings: &NetworkSettings,
    ) -> Self {
        let project_root = project_root.to_path_buf();
        let project_root_label = project_root.display().to_string();
        let custom_mounts = build_custom_mounts(&config.name, &config.mounts);

        let proxy = build_proxy_pod(&project_root, config, network_settings, &custom_mounts);
        let agent = build_agent_pod(&project_root, config, network_settings, &custom_mounts);
        let nw_sandbox = network_settings.nw_sandbox.as_ref().map(|component| {
            build_nw_sandbox_pod(
                &project_root,
                config,
                network_settings,
                component,
                &custom_mounts,
            )
        });
        let fs_sandbox = network_settings.fs_sandbox.as_ref().map(|component| {
            build_fs_sandbox_pod(
                &project_root,
                config,
                network_settings,
                component,
                &custom_mounts,
            )
        });

        Self {
            project_name: config.name.clone(),
            project_root,
            project_root_label,
            network_settings: network_settings.clone(),
            custom_mounts,
            proxy,
            agent,
            nw_sandbox,
            fs_sandbox,
        }
    }

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

#[derive(Debug, Clone)]
pub struct RuntimePod {
    pub name: String,
    pub labels: BTreeMap<String, String>,
    pub network_name: String,
    pub ip: String,
    pub host_aliases: Vec<RuntimeHostAlias>,
    pub init_tasks: Vec<RuntimeTask>,
    pub containers: Vec<RuntimeContainer>,
    pub userns_keep_id: bool,
}

#[derive(Debug, Clone)]
pub struct RuntimeContainer {
    pub name: String,
    pub image: String,
    pub command: Vec<String>,
    pub workdir: Option<String>,
    pub env: Vec<RuntimeEnvVar>,
    pub mounts: Vec<RuntimeMount>,
    pub ports: Vec<u16>,
    pub stdin: bool,
    pub tty: bool,
}

#[derive(Debug, Clone)]
pub struct RuntimeTask {
    pub name: String,
    pub image: String,
    pub command: Vec<String>,
    pub env: Vec<RuntimeEnvVar>,
    pub mounts: Vec<RuntimeMount>,
    pub run_as_user: Option<u32>,
    pub run_as_group: Option<u32>,
    pub added_capabilities: Vec<String>,
}

#[derive(Debug, Clone)]
pub struct RuntimeEnvVar {
    pub name: String,
    pub value: String,
}

#[derive(Debug, Clone)]
pub struct RuntimeMount {
    pub mount_path: String,
    pub read_only: bool,
    pub source: RuntimeMountSource,
}

#[derive(Debug, Clone)]
pub struct RuntimeCustomMount {
    pub mount_path: String,
    pub read_only: bool,
    pub source: RuntimeMountSource,
    pub targets: Vec<MountTarget>,
    pub ignore: bool,
}

#[derive(Debug, Clone)]
pub enum RuntimeMountSource {
    HostPath { path: PathBuf },
    NamedVolume { claim_name: String },
    GeneratedEmptyMask { path: PathBuf },
    EmptyDir,
}

#[derive(Debug, Clone)]
pub struct RuntimeHostAlias {
    pub ip: String,
    pub hostnames: Vec<String>,
}

fn build_proxy_pod(
    project_root: &Path,
    config: &ExecutionConfig,
    network_settings: &NetworkSettings,
    custom_mounts: &[RuntimeCustomMount],
) -> RuntimePod {
    let mounts = build_proxy_mounts(project_root, custom_mounts);

    let mut env = vec![
        RuntimeEnvVar {
            name: "CLADDING_PROXY_NAME".to_string(),
            value: network_settings.proxy_name.clone(),
        },
        RuntimeEnvVar {
            name: "CLADDING_AGENT_NAME".to_string(),
            value: network_settings.agent_name.clone(),
        },
    ];
    if let Some(nw) = &network_settings.nw_sandbox {
        env.insert(
            1,
            RuntimeEnvVar {
                name: "CLADDING_SANDBOX_NAME".to_string(),
                value: nw.name.clone(),
            },
        );
    }

    let mut host_aliases = vec![RuntimeHostAlias {
        ip: network_settings.agent_ip.clone(),
        hostnames: vec![network_settings.agent_name.clone()],
    }];
    if let Some(nw) = &network_settings.nw_sandbox {
        host_aliases.push(RuntimeHostAlias {
            ip: nw.ip.clone(),
            hostnames: vec![nw.name.clone()],
        });
    }

    RuntimePod {
        name: network_settings.proxy_name.clone(),
        labels: build_labels(&config.name, project_root, "proxy"),
        network_name: network_settings.network.clone(),
        ip: network_settings.proxy_ip.clone(),
        host_aliases,
        init_tasks: Vec::new(),
        containers: vec![RuntimeContainer {
            name: runtime_container_name(&network_settings.proxy_name),
            image: "docker.io/ubuntu/squid:latest".to_string(),
            command: vec![
                "/bin/sh".to_string(),
                "/opt/scripts/proxy_startup.sh".to_string(),
            ],
            workdir: None,
            env,
            mounts,
            ports: vec![8080],
            stdin: false,
            tty: false,
        }],
        userns_keep_id: false,
    }
}

fn build_agent_pod(
    project_root: &Path,
    config: &ExecutionConfig,
    network_settings: &NetworkSettings,
    custom_mounts: &[RuntimeCustomMount],
) -> RuntimePod {
    let mounts = apply_custom_mounts(
        build_agent_mounts(project_root, custom_mounts),
        custom_mounts,
        MountTarget::Agent,
    );

    let mut env = vec![
        RuntimeEnvVar {
            name: "PATH".to_string(),
            value: "/opt/tools/bin:/usr/local/sbin:/usr/local/bin:/usr/sbin:/usr/bin:/sbin:/bin"
                .to_string(),
        },
        RuntimeEnvVar {
            name: "CLADDING_PROXY_NAME".to_string(),
            value: network_settings.proxy_name.clone(),
        },
        RuntimeEnvVar {
            name: "CLADDING_AGENT_NAME".to_string(),
            value: network_settings.agent_name.clone(),
        },
        RuntimeEnvVar {
            name: "http_proxy".to_string(),
            value: format!("http://{}:8080", network_settings.proxy_name),
        },
        RuntimeEnvVar {
            name: "https_proxy".to_string(),
            value: format!("http://{}:8080", network_settings.proxy_name),
        },
        RuntimeEnvVar {
            name: "HTTP_PROXY".to_string(),
            value: format!("http://{}:8080", network_settings.proxy_name),
        },
        RuntimeEnvVar {
            name: "HTTPS_PROXY".to_string(),
            value: format!("http://{}:8080", network_settings.proxy_name),
        },
        RuntimeEnvVar {
            name: "no_proxy".to_string(),
            value: build_no_proxy(network_settings),
        },
        RuntimeEnvVar {
            name: "NO_PROXY".to_string(),
            value: build_no_proxy(network_settings),
        },
    ];
    if let Some(nw) = &network_settings.nw_sandbox {
        env.insert(
            3,
            RuntimeEnvVar {
                name: "CLADDING_SANDBOX_NAME".to_string(),
                value: nw.name.clone(),
            },
        );
        env.push(RuntimeEnvVar {
            name: "RUN_NW_SANDBOX_SERVER".to_string(),
            value: format!("http://{}:3000/raw", nw.name),
        });
    }
    if let Some(fs) = &network_settings.fs_sandbox {
        env.push(RuntimeEnvVar {
            name: "RUN_FS_SANDBOX_SERVER".to_string(),
            value: format!("http://{}:3000/raw", fs.name),
        });
    }

    let mut host_aliases = vec![RuntimeHostAlias {
        ip: network_settings.proxy_ip.clone(),
        hostnames: vec![network_settings.proxy_name.clone()],
    }];
    if let Some(nw) = &network_settings.nw_sandbox {
        host_aliases.push(RuntimeHostAlias {
            ip: nw.ip.clone(),
            hostnames: vec![nw.name.clone()],
        });
    }
    if let Some(fs) = &network_settings.fs_sandbox {
        host_aliases.push(RuntimeHostAlias {
            ip: fs.ip.clone(),
            hostnames: vec![fs.name.clone()],
        });
    }

    let mut init_env = vec![
        RuntimeEnvVar {
            name: "CLADDING_PROXY_NAME".to_string(),
            value: network_settings.proxy_name.clone(),
        },
        RuntimeEnvVar {
            name: "CLADDING_AGENT_NAME".to_string(),
            value: network_settings.agent_name.clone(),
        },
    ];
    if let Some(nw) = &network_settings.nw_sandbox {
        init_env.push(RuntimeEnvVar {
            name: "CLADDING_SANDBOX_NAME".to_string(),
            value: nw.name.clone(),
        });
        init_env.push(RuntimeEnvVar {
            name: "RUN_NW_SANDBOX_SERVER".to_string(),
            value: format!("http://{}:3000/raw", nw.name),
        });
    }
    if let Some(fs) = &network_settings.fs_sandbox {
        init_env.push(RuntimeEnvVar {
            name: "RUN_FS_SANDBOX_SERVER".to_string(),
            value: format!("http://{}:3000/raw", fs.name),
        });
    }

    let init_tasks = vec![RuntimeTask {
        name: "agent-node".to_string(),
        image: "alpine:latest".to_string(),
        command: vec![
            "/bin/sh".to_string(),
            "/opt/scripts/jail_agent.sh".to_string(),
        ],
        env: init_env,
        mounts: vec![
            RuntimeMount {
                mount_path: "/opt/scripts".to_string(),
                read_only: true,
                source: RuntimeMountSource::HostPath {
                    path: project_root.join("scripts"),
                },
            },
            RuntimeMount {
                mount_path: "/opt/config".to_string(),
                read_only: true,
                source: RuntimeMountSource::HostPath {
                    path: project_root.join("config"),
                },
            },
        ],
        run_as_user: Some(0),
        run_as_group: Some(0),
        added_capabilities: vec!["NET_ADMIN".to_string()],
    }];

    RuntimePod {
        name: network_settings.agent_name.clone(),
        labels: build_labels(&config.name, project_root, "agent"),
        network_name: network_settings.network.clone(),
        ip: network_settings.agent_ip.clone(),
        host_aliases,
        init_tasks,
        containers: vec![RuntimeContainer {
            name: runtime_container_name(&network_settings.agent_name),
            image: config.agent_image().to_string(),
            command: vec!["sleep".to_string(), "infinity".to_string()],
            workdir: Some("/home/user/workspace".to_string()),
            env,
            mounts,
            ports: Vec::new(),
            stdin: true,
            tty: true,
        }],
        userns_keep_id: true,
    }
}

fn build_nw_sandbox_pod(
    project_root: &Path,
    config: &ExecutionConfig,
    network_settings: &NetworkSettings,
    component: &ComponentNetworkSettings,
    custom_mounts: &[RuntimeCustomMount],
) -> RuntimePod {
    let mounts = apply_custom_mounts(
        build_sandbox_mounts(project_root, custom_mounts),
        custom_mounts,
        MountTarget::NwSandbox,
    );

    let mut env = vec![
        RuntimeEnvVar {
            name: "PATH".to_string(),
            value: "/opt/tools/bin:/usr/local/sbin:/usr/local/bin:/usr/sbin:/usr/bin:/sbin:/bin"
                .to_string(),
        },
        RuntimeEnvVar {
            name: "MCP_BIND_ADDR".to_string(),
            value: "0.0.0.0:3000".to_string(),
        },
        RuntimeEnvVar {
            name: "POLICY_DIR".to_string(),
            value: "/opt/config/nw_sandbox".to_string(),
        },
        RuntimeEnvVar {
            name: "CLADDING_PROXY_NAME".to_string(),
            value: network_settings.proxy_name.clone(),
        },
        RuntimeEnvVar {
            name: "CLADDING_AGENT_NAME".to_string(),
            value: network_settings.agent_name.clone(),
        },
        RuntimeEnvVar {
            name: "CLADDING_SANDBOX_NAME".to_string(),
            value: component.name.clone(),
        },
        RuntimeEnvVar {
            name: "http_proxy".to_string(),
            value: format!("http://{}:8080", network_settings.proxy_name),
        },
        RuntimeEnvVar {
            name: "https_proxy".to_string(),
            value: format!("http://{}:8080", network_settings.proxy_name),
        },
        RuntimeEnvVar {
            name: "HTTP_PROXY".to_string(),
            value: format!("http://{}:8080", network_settings.proxy_name),
        },
        RuntimeEnvVar {
            name: "HTTPS_PROXY".to_string(),
            value: format!("http://{}:8080", network_settings.proxy_name),
        },
    ];
    let no_proxy = format!("{},localhost,127.0.0.1", component.name);
    env.push(RuntimeEnvVar {
        name: "no_proxy".to_string(),
        value: no_proxy.clone(),
    });
    env.push(RuntimeEnvVar {
        name: "NO_PROXY".to_string(),
        value: no_proxy,
    });

    RuntimePod {
        name: component.name.clone(),
        labels: build_labels(&config.name, project_root, "nw-sandbox"),
        network_name: network_settings.network.clone(),
        ip: component.ip.clone(),
        host_aliases: vec![RuntimeHostAlias {
            ip: network_settings.proxy_ip.clone(),
            hostnames: vec![network_settings.proxy_name.clone()],
        }],
        init_tasks: vec![RuntimeTask {
            name: "sandbox-node".to_string(),
            image: "alpine:latest".to_string(),
            command: vec![
                "/bin/sh".to_string(),
                "/opt/scripts/jail_nw_sandbox.sh".to_string(),
            ],
            env: vec![
                RuntimeEnvVar {
                    name: "CLADDING_PROXY_NAME".to_string(),
                    value: network_settings.proxy_name.clone(),
                },
                RuntimeEnvVar {
                    name: "CLADDING_AGENT_NAME".to_string(),
                    value: network_settings.agent_name.clone(),
                },
                RuntimeEnvVar {
                    name: "CLADDING_SANDBOX_NAME".to_string(),
                    value: component.name.clone(),
                },
            ],
            mounts: vec![RuntimeMount {
                mount_path: "/opt/scripts".to_string(),
                read_only: true,
                source: RuntimeMountSource::HostPath {
                    path: project_root.join("scripts"),
                },
            }],
            run_as_user: Some(0),
            run_as_group: Some(0),
            added_capabilities: vec!["NET_ADMIN".to_string()],
        }],
        containers: vec![RuntimeContainer {
            name: runtime_container_name(&component.name),
            image: config.nw_sandbox_image().to_string(),
            command: vec!["mcp-run".to_string()],
            workdir: Some("/home/user/workspace".to_string()),
            env,
            mounts,
            ports: vec![3000],
            stdin: false,
            tty: false,
        }],
        userns_keep_id: true,
    }
}

fn build_fs_sandbox_pod(
    project_root: &Path,
    config: &ExecutionConfig,
    network_settings: &NetworkSettings,
    component: &ComponentNetworkSettings,
    custom_mounts: &[RuntimeCustomMount],
) -> RuntimePod {
    let mounts = apply_custom_mounts(
        build_sandbox_mounts(project_root, custom_mounts),
        custom_mounts,
        MountTarget::FsSandbox,
    );

    RuntimePod {
        name: component.name.clone(),
        labels: build_labels(&config.name, project_root, "fs-sandbox"),
        network_name: network_settings.network.clone(),
        ip: component.ip.clone(),
        host_aliases: Vec::new(),
        init_tasks: vec![RuntimeTask {
            name: "fs-sandbox-node".to_string(),
            image: "alpine:latest".to_string(),
            command: vec!["/bin/sh".to_string(), "/opt/scripts/jail_fs_sandbox.sh".to_string()],
            env: Vec::new(),
            mounts: vec![RuntimeMount {
                mount_path: "/opt/scripts".to_string(),
                read_only: true,
                source: RuntimeMountSource::HostPath {
                    path: project_root.join("scripts"),
                },
            }],
            run_as_user: Some(0),
            run_as_group: Some(0),
            added_capabilities: vec!["NET_ADMIN".to_string()],
        }],
        containers: vec![RuntimeContainer {
            name: runtime_container_name(&component.name),
            image: config.fs_sandbox_image().to_string(),
            command: vec!["mcp-run".to_string()],
            workdir: Some("/home/user/workspace".to_string()),
            env: vec![
                RuntimeEnvVar {
                    name: "PATH".to_string(),
                    value: "/opt/tools/bin:/usr/local/sbin:/usr/local/bin:/usr/sbin:/usr/bin:/sbin:/bin"
                        .to_string(),
                },
                RuntimeEnvVar {
                    name: "MCP_BIND_ADDR".to_string(),
                    value: "0.0.0.0:3000".to_string(),
                },
                RuntimeEnvVar {
                    name: "POLICY_DIR".to_string(),
                    value: "/opt/config/fs_sandbox".to_string(),
                },
            ],
            mounts,
            ports: vec![3000],
            stdin: false,
            tty: false,
        }],
        userns_keep_id: true,
    }
}

fn build_labels(project_name: &str, project_root: &Path, app: &str) -> BTreeMap<String, String> {
    BTreeMap::from([
        ("app".to_string(), app.to_string()),
        ("cladding".to_string(), project_name.to_string()),
        (
            "project_root".to_string(),
            project_root.display().to_string(),
        ),
    ])
}

fn build_proxy_mounts(
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
                path: project_root.join("scripts"),
            },
        },
    ]
}

fn build_agent_mounts(
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

fn build_sandbox_mounts(
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

fn apply_custom_mounts(
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

fn build_custom_mounts(
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

fn runtime_container_name(pod_name: &str) -> String {
    format!("{pod_name}-instance")
}

fn collect_required_host_paths(pod: &RuntimePod, paths: &mut BTreeSet<PathBuf>) {
    for task in &pod.init_tasks {
        for mount in &task.mounts {
            if let RuntimeMountSource::HostPath { path } = &mount.source {
                paths.insert(path.clone());
            }
        }
    }

    for container in &pod.containers {
        for mount in &container.mounts {
            if let RuntimeMountSource::HostPath { path } = &mount.source {
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
    ) -> ExecutionConfig {
        ExecutionConfig {
            name: "demo".to_string(),
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
    fn build_runtime_spec_preserves_component_metadata() {
        let config = execution_config(true, true, Vec::new());
        let network_settings = NetworkSettings {
            pool_index: 1,
            network: "cladding-1".to_string(),
            network_subnet: "10.90.1.0/24".to_string(),
            proxy_ip: "10.90.1.2".to_string(),
            proxy_name: "demo-proxy".to_string(),
            agent_ip: "10.90.1.5".to_string(),
            agent_name: "demo-agent".to_string(),
            nw_sandbox: Some(ComponentNetworkSettings {
                ip: "10.90.1.3".to_string(),
                name: "demo-nw-sandbox".to_string(),
            }),
            fs_sandbox: Some(ComponentNetworkSettings {
                ip: "10.90.1.4".to_string(),
                name: "demo-fs-sandbox".to_string(),
            }),
        };

        let spec = RuntimeSpec::build(
            Path::new("/tmp/project/.cladding"),
            &config,
            &network_settings,
        );

        assert_eq!(spec.project_name, "demo");
        assert_eq!(spec.proxy.name, "demo-proxy");
        assert_eq!(spec.agent.containers[0].name, "demo-agent-instance");
        assert_eq!(
            spec.proxy.labels.get("project_root").map(String::as_str),
            Some("/tmp/project/.cladding")
        );
        assert_eq!(spec.proxy.host_aliases.len(), 2);
        assert!(spec.agent.userns_keep_id);
        assert_eq!(
            spec.agent.containers[0].workdir.as_deref(),
            Some("/home/user/workspace")
        );
        assert_eq!(spec.agent.containers[0].ports, Vec::<u16>::new());
        assert_eq!(spec.agent.containers[0].name, "demo-agent-instance");
        assert_eq!(spec.proxy.containers[0].name, "demo-proxy-instance");
        assert_eq!(
            spec.nw_sandbox.as_ref().expect("nw pod").containers[0].name,
            "demo-nw-sandbox-instance"
        );
        assert_eq!(
            spec.nw_sandbox.as_ref().expect("nw pod").containers[0].ports,
            vec![3000]
        );
        assert_eq!(
            spec.fs_sandbox.as_ref().expect("fs pod").containers[0].image,
            "fs:image"
        );
    }

    #[test]
    fn build_runtime_spec_only_includes_enabled_components() {
        let config = execution_config(true, false, Vec::new());
        let network_settings = NetworkSettings {
            pool_index: 1,
            network: "cladding-1".to_string(),
            network_subnet: "10.90.1.0/24".to_string(),
            proxy_ip: "10.90.1.2".to_string(),
            proxy_name: "demo-proxy".to_string(),
            agent_ip: "10.90.1.5".to_string(),
            agent_name: "demo-agent".to_string(),
            nw_sandbox: Some(ComponentNetworkSettings {
                ip: "10.90.1.3".to_string(),
                name: "demo-nw-sandbox".to_string(),
            }),
            fs_sandbox: None,
        };

        let spec = RuntimeSpec::build(
            Path::new("/tmp/project/.cladding"),
            &config,
            &network_settings,
        );

        assert!(spec.nw_sandbox.is_some());
        assert!(spec.fs_sandbox.is_none());
        assert_eq!(spec.proxy.containers[0].name, "demo-proxy-instance");
        assert_eq!(spec.agent.containers[0].name, "demo-agent-instance");
        assert_eq!(
            spec.nw_sandbox.as_ref().expect("nw pod").containers[0].name,
            "demo-nw-sandbox-instance"
        );
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
        );
        let network_settings = NetworkSettings {
            pool_index: 1,
            network: "cladding-1".to_string(),
            network_subnet: "10.90.1.0/24".to_string(),
            proxy_ip: "10.90.1.2".to_string(),
            proxy_name: "demo-proxy".to_string(),
            agent_ip: "10.90.1.5".to_string(),
            agent_name: "demo-agent".to_string(),
            nw_sandbox: Some(ComponentNetworkSettings {
                ip: "10.90.1.3".to_string(),
                name: "demo-nw-sandbox".to_string(),
            }),
            fs_sandbox: None,
        };

        let spec = RuntimeSpec::build(
            Path::new("/tmp/project/.cladding"),
            &config,
            &network_settings,
        );
        let required = spec.required_host_paths();

        assert!(required.contains(&PathBuf::from("/tmp/project/.cladding/config")));
        assert!(required.contains(&PathBuf::from("/tmp/project/.cladding/scripts")));
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
        );
        let network_settings = NetworkSettings {
            pool_index: 1,
            network: "cladding-1".to_string(),
            network_subnet: "10.90.1.0/24".to_string(),
            proxy_ip: "10.90.1.2".to_string(),
            proxy_name: "demo-proxy".to_string(),
            agent_ip: "10.90.1.5".to_string(),
            agent_name: "demo-agent".to_string(),
            nw_sandbox: None,
            fs_sandbox: None,
        };

        let spec = RuntimeSpec::build(
            Path::new("/tmp/project/.cladding"),
            &config,
            &network_settings,
        );
        let required = spec.required_host_paths();

        assert!(!required.contains(&PathBuf::from("/tmp/ignored")));
        assert!(!required.contains(&PathBuf::from("/tmp/inactive")));
    }
}
