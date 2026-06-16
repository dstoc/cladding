use crate::config::{ExecutionConfig, MountTarget, ResolvedMountConfig};
use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};

const LOOPBACK: &str = "127.0.0.1";
const NETWORK_DEFAULT: &str = "default";
const NETWORK_NONE: &str = "none";
const RUNTIME_SOCKET_DIR: &str = "runtime/sockets";
#[allow(dead_code)]
const RUNTIME_EXPOSE_DIR: &str = "runtime/expose";
const RUNTIME_SOCKET_MOUNT_PATH: &str = "/run/cladding/sockets";

#[derive(Debug, Clone)]
pub struct RuntimeSpec {
    pub project_name: String,
    pub project_root: PathBuf,
    pub project_root_label: String,
    pub custom_mounts: Vec<RuntimeCustomMount>,
    pub proxy: RuntimePod,
    pub agent: RuntimePod,
    pub nw_sandbox: Option<RuntimePod>,
    pub fs_sandbox: Option<RuntimePod>,
}

#[derive(Debug, Clone)]
struct RuntimeNames {
    proxy_name: String,
    agent_name: String,
    nw_sandbox_name: Option<String>,
    fs_sandbox_name: Option<String>,
}

impl RuntimeNames {
    fn from_config(config: &ExecutionConfig) -> Self {
        Self {
            proxy_name: format!("{}-proxy", config.name),
            agent_name: format!("{}-agent", config.name),
            nw_sandbox_name: config
                .nw_sandbox_enabled()
                .then(|| format!("{}-nw-sandbox", config.name)),
            fs_sandbox_name: config
                .fs_sandbox_enabled()
                .then(|| format!("{}-fs-sandbox", config.name)),
        }
    }
}

impl RuntimeSpec {
    pub fn build(project_root: &Path, config: &ExecutionConfig) -> Self {
        let project_root = project_root.to_path_buf();
        let project_root_label = project_root.display().to_string();
        let custom_mounts = build_custom_mounts(&config.name, &config.mounts);
        let names = RuntimeNames::from_config(config);

        let proxy = build_proxy_pod(&project_root, config, &names, &custom_mounts);
        let agent = build_agent_pod(&project_root, config, &names, &custom_mounts);
        let nw_sandbox = names.nw_sandbox_name.as_ref().map(|component_name| {
            build_nw_sandbox_pod(
                &project_root,
                config,
                &names,
                component_name,
                &custom_mounts,
            )
        });
        let fs_sandbox = names.fs_sandbox_name.as_ref().map(|component_name| {
            build_fs_sandbox_pod(
                &project_root,
                config,
                &names,
                component_name,
                &custom_mounts,
            )
        });

        Self {
            project_name: config.name.clone(),
            project_root,
            project_root_label,
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
    names: &RuntimeNames,
    custom_mounts: &[RuntimeCustomMount],
) -> RuntimePod {
    let mounts = build_proxy_mounts(project_root, custom_mounts);
    let mut containers = vec![RuntimeContainer {
        name: runtime_container_name(&names.proxy_name),
        image: "docker.io/ubuntu/squid:latest".to_string(),
        command: vec![
            "/bin/sh".to_string(),
            "/opt/scripts/proxy_startup.sh".to_string(),
        ],
        workdir: None,
        env: vec![
            RuntimeEnvVar {
                name: "CLADDING_PROXY_NAME".to_string(),
                value: names.proxy_name.clone(),
            },
            RuntimeEnvVar {
                name: "CLADDING_AGENT_NAME".to_string(),
                value: names.agent_name.clone(),
            },
        ],
        mounts,
        ports: vec![3128, 3129],
        stdin: false,
        tty: false,
    }];

    if names.nw_sandbox_name.is_some() {
        containers.push(build_proxy_bridge_listener(
            &names.proxy_name,
            "nw-sandbox-proxy-socket",
            3129,
            runtime_socket_path(project_root, "nw-sandbox-proxy.sock"),
        ));
    }

    containers.push(build_proxy_bridge_listener(
        &names.proxy_name,
        "agent-proxy-socket",
        3128,
        runtime_socket_path(project_root, "agent-proxy.sock"),
    ));

    RuntimePod {
        name: names.proxy_name.clone(),
        labels: build_labels(&config.name, project_root, "proxy"),
        network_name: NETWORK_DEFAULT.to_string(),
        ip: String::new(),
        host_aliases: Vec::new(),
        init_tasks: Vec::new(),
        containers,
        userns_keep_id: false,
    }
}

fn build_agent_pod(
    project_root: &Path,
    config: &ExecutionConfig,
    names: &RuntimeNames,
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
            value: names.proxy_name.clone(),
        },
        RuntimeEnvVar {
            name: "CLADDING_AGENT_NAME".to_string(),
            value: names.agent_name.clone(),
        },
        RuntimeEnvVar {
            name: "http_proxy".to_string(),
            value: format!("http://{LOOPBACK}:3128"),
        },
        RuntimeEnvVar {
            name: "https_proxy".to_string(),
            value: format!("http://{LOOPBACK}:3128"),
        },
        RuntimeEnvVar {
            name: "HTTP_PROXY".to_string(),
            value: format!("http://{LOOPBACK}:3128"),
        },
        RuntimeEnvVar {
            name: "HTTPS_PROXY".to_string(),
            value: format!("http://{LOOPBACK}:3128"),
        },
        RuntimeEnvVar {
            name: "no_proxy".to_string(),
            value: build_no_proxy(),
        },
        RuntimeEnvVar {
            name: "NO_PROXY".to_string(),
            value: build_no_proxy(),
        },
    ];
    if let Some(nw_name) = names.nw_sandbox_name.as_ref() {
        env.insert(
            3,
            RuntimeEnvVar {
                name: "CLADDING_SANDBOX_NAME".to_string(),
                value: nw_name.clone(),
            },
        );
        env.push(RuntimeEnvVar {
            name: "RUN_NW_SANDBOX_SERVER".to_string(),
            value: format!("http://{LOOPBACK}:3001/raw"),
        });
    }
    if names.fs_sandbox_name.is_some() {
        env.push(RuntimeEnvVar {
            name: "RUN_FS_SANDBOX_SERVER".to_string(),
            value: format!("http://{LOOPBACK}:3002/raw"),
        });
    }

    let mut containers = vec![RuntimeContainer {
        name: runtime_container_name(&names.agent_name),
        image: config.agent_image().to_string(),
        command: vec!["sleep".to_string(), "infinity".to_string()],
        workdir: Some("/home/user/workspace".to_string()),
        env,
        mounts,
        ports: Vec::new(),
        stdin: true,
        tty: true,
    }];

    containers.push(build_loopback_bridge_sidecar(
        &names.agent_name,
        "proxy-client",
        3128,
        runtime_socket_path(project_root, "agent-proxy.sock"),
    ));

    if names.nw_sandbox_name.is_some() {
        containers.push(build_loopback_bridge_sidecar(
            &names.agent_name,
            "nw-sandbox-run-client",
            3001,
            runtime_socket_path(project_root, "nw-sandbox-run.sock"),
        ));
    }
    if names.fs_sandbox_name.is_some() {
        containers.push(build_loopback_bridge_sidecar(
            &names.agent_name,
            "fs-sandbox-run-client",
            3002,
            runtime_socket_path(project_root, "fs-sandbox-run.sock"),
        ));
    }

    RuntimePod {
        name: names.agent_name.clone(),
        labels: build_labels(&config.name, project_root, "agent"),
        network_name: NETWORK_NONE.to_string(),
        ip: String::new(),
        host_aliases: Vec::new(),
        init_tasks: Vec::new(),
        containers,
        userns_keep_id: true,
    }
}

fn build_nw_sandbox_pod(
    project_root: &Path,
    config: &ExecutionConfig,
    names: &RuntimeNames,
    component_name: &str,
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
            value: format!("{LOOPBACK}:3000"),
        },
        RuntimeEnvVar {
            name: "POLICY_DIR".to_string(),
            value: "/opt/config/nw_sandbox".to_string(),
        },
        RuntimeEnvVar {
            name: "CLADDING_PROXY_NAME".to_string(),
            value: names.proxy_name.clone(),
        },
        RuntimeEnvVar {
            name: "CLADDING_AGENT_NAME".to_string(),
            value: names.agent_name.clone(),
        },
        RuntimeEnvVar {
            name: "CLADDING_SANDBOX_NAME".to_string(),
            value: component_name.to_string(),
        },
        RuntimeEnvVar {
            name: "http_proxy".to_string(),
            value: format!("http://{LOOPBACK}:3128"),
        },
        RuntimeEnvVar {
            name: "https_proxy".to_string(),
            value: format!("http://{LOOPBACK}:3128"),
        },
        RuntimeEnvVar {
            name: "HTTP_PROXY".to_string(),
            value: format!("http://{LOOPBACK}:3128"),
        },
        RuntimeEnvVar {
            name: "HTTPS_PROXY".to_string(),
            value: format!("http://{LOOPBACK}:3128"),
        },
    ];
    let no_proxy = format!("localhost,{LOOPBACK}");
    env.push(RuntimeEnvVar {
        name: "no_proxy".to_string(),
        value: no_proxy.clone(),
    });
    env.push(RuntimeEnvVar {
        name: "NO_PROXY".to_string(),
        value: no_proxy,
    });

    let mut containers = vec![
        RuntimeContainer {
            name: runtime_container_name(component_name),
            image: config.nw_sandbox_image().to_string(),
            command: vec!["mcp-run".to_string()],
            workdir: Some("/home/user/workspace".to_string()),
            env,
            mounts,
            ports: vec![3000],
            stdin: false,
            tty: false,
        },
        build_loopback_bridge_sidecar(
            component_name,
            "proxy-client",
            3128,
            runtime_socket_path(project_root, "nw-sandbox-proxy.sock"),
        ),
        build_sandbox_run_server_listener(
            component_name,
            "run-server",
            "nw-sandbox-run.sock",
            project_root,
        ),
    ];

    RuntimePod {
        name: component_name.to_string(),
        labels: build_labels(&config.name, project_root, "nw-sandbox"),
        network_name: NETWORK_NONE.to_string(),
        ip: String::new(),
        host_aliases: Vec::new(),
        init_tasks: Vec::new(),
        containers: std::mem::take(&mut containers),
        userns_keep_id: true,
    }
}

fn build_fs_sandbox_pod(
    project_root: &Path,
    config: &ExecutionConfig,
    _names: &RuntimeNames,
    component_name: &str,
    custom_mounts: &[RuntimeCustomMount],
) -> RuntimePod {
    let mounts = apply_custom_mounts(
        build_sandbox_mounts(project_root, custom_mounts),
        custom_mounts,
        MountTarget::FsSandbox,
    );

    RuntimePod {
        name: component_name.to_string(),
        labels: build_labels(&config.name, project_root, "fs-sandbox"),
        network_name: NETWORK_NONE.to_string(),
        ip: String::new(),
        host_aliases: Vec::new(),
        init_tasks: Vec::new(),
        containers: vec![
            RuntimeContainer {
                name: runtime_container_name(component_name),
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
                        value: format!("{LOOPBACK}:3000"),
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
            },
            build_sandbox_run_server_listener(
                component_name,
                "run-server",
                "fs-sandbox-run.sock",
                project_root,
            ),
        ],
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

fn build_socket_dir_mount(socket_dir: &Path) -> RuntimeMount {
    RuntimeMount {
        mount_path: RUNTIME_SOCKET_MOUNT_PATH.to_string(),
        read_only: false,
        source: RuntimeMountSource::HostPath {
            path: socket_dir.to_path_buf(),
        },
    }
}

fn build_proxy_bridge_listener(
    pod_name: &str,
    listener_name: &str,
    listen_port: u16,
    socket_path: PathBuf,
) -> RuntimeContainer {
    let socket_name = socket_path
        .file_name()
        .expect("runtime socket path must have a file name")
        .to_str()
        .expect("runtime socket path must be valid UTF-8");
    RuntimeContainer {
        name: format!("{pod_name}-{listener_name}"),
        image: "alpine/socat".to_string(),
        command: vec![
            "socat".to_string(),
            format!(
                "UNIX-LISTEN:{},fork,reuseaddr",
                runtime_socket_mount_path(socket_name)
            ),
            format!("TCP:{LOOPBACK}:{listen_port}"),
        ],
        workdir: None,
        env: Vec::new(),
        mounts: vec![build_socket_dir_mount(socket_path.parent().unwrap())],
        ports: vec![listen_port],
        stdin: false,
        tty: false,
    }
}

fn build_loopback_bridge_sidecar(
    pod_name: &str,
    listener_name: &str,
    listen_port: u16,
    socket_path: PathBuf,
) -> RuntimeContainer {
    let socket_name = socket_path
        .file_name()
        .expect("runtime socket path must have a file name")
        .to_str()
        .expect("runtime socket path must be valid UTF-8");
    RuntimeContainer {
        name: format!("{pod_name}-{listener_name}"),
        image: "alpine/socat".to_string(),
        command: vec![
            "socat".to_string(),
            format!("TCP-LISTEN:{listen_port},bind={LOOPBACK},fork,reuseaddr"),
            format!("UNIX-CONNECT:{}", runtime_socket_mount_path(socket_name)),
        ],
        workdir: None,
        env: Vec::new(),
        mounts: vec![build_socket_dir_mount(socket_path.parent().unwrap())],
        ports: vec![listen_port],
        stdin: false,
        tty: false,
    }
}

fn build_sandbox_run_server_listener(
    pod_name: &str,
    listener_name: &str,
    socket_name: &str,
    project_root: &Path,
) -> RuntimeContainer {
    RuntimeContainer {
        name: format!("{pod_name}-{listener_name}"),
        image: "alpine/socat".to_string(),
        command: vec![
            "socat".to_string(),
            format!(
                "UNIX-LISTEN:{},fork,reuseaddr",
                runtime_socket_mount_path(socket_name)
            ),
            format!("TCP:{LOOPBACK}:3000"),
        ],
        workdir: None,
        env: Vec::new(),
        mounts: vec![RuntimeMount {
            mount_path: RUNTIME_SOCKET_MOUNT_PATH.to_string(),
            read_only: false,
            source: RuntimeMountSource::HostPath {
                path: runtime_socket_dir(project_root),
            },
        }],
        ports: vec![3000],
        stdin: false,
        tty: false,
    }
}

fn runtime_socket_dir(project_root: &Path) -> PathBuf {
    project_root.join(RUNTIME_SOCKET_DIR)
}

#[allow(dead_code)]
fn runtime_expose_dir(project_root: &Path) -> PathBuf {
    project_root.join(RUNTIME_EXPOSE_DIR)
}

fn runtime_socket_path(project_root: &Path, socket_name: &str) -> PathBuf {
    runtime_socket_dir(project_root).join(socket_name)
}

fn runtime_socket_mount_path(socket_name: &str) -> String {
    format!("{RUNTIME_SOCKET_MOUNT_PATH}/{socket_name}")
}

#[allow(dead_code)]
fn runtime_expose_socket_path(
    project_root: &Path,
    pod_name: &str,
    container_port: u16,
    host_port: u16,
) -> PathBuf {
    runtime_expose_dir(project_root).join(format!("{pod_name}-{container_port}-{host_port}.sock"))
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

fn build_no_proxy() -> String {
    ["localhost", LOOPBACK].join(",")
}

fn runtime_container_name(pod_name: &str) -> String {
    format!("{pod_name}-instance")
}

fn collect_required_host_paths(pod: &RuntimePod, paths: &mut BTreeSet<PathBuf>) {
    for task in &pod.init_tasks {
        for mount in &task.mounts {
            if let RuntimeMountSource::HostPath { path } = &mount.source {
                if is_generated_runtime_path(path) {
                    continue;
                }
                paths.insert(path.clone());
            }
        }
    }

    for container in &pod.containers {
        for mount in &container.mounts {
            if let RuntimeMountSource::HostPath { path } = &mount.source {
                if is_generated_runtime_path(path) {
                    continue;
                }
                paths.insert(path.clone());
            }
        }
    }
}

fn is_generated_runtime_path(path: &Path) -> bool {
    path.ends_with(RUNTIME_SOCKET_DIR)
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
        let spec = RuntimeSpec::build(Path::new("/tmp/project/.cladding"), &config);

        assert_eq!(spec.project_name, "demo");
        assert_eq!(spec.proxy.name, "demo-proxy");
        assert_eq!(spec.agent.containers[0].name, "demo-agent-instance");
        assert_eq!(
            spec.proxy.labels.get("project_root").map(String::as_str),
            Some("/tmp/project/.cladding")
        );
        assert!(spec.proxy.host_aliases.is_empty());
        assert_eq!(spec.proxy.network_name, "default");
        assert_eq!(spec.agent.network_name, "none");
        assert_eq!(spec.agent.host_aliases.len(), 0);
        assert_eq!(spec.agent.init_tasks.len(), 0);
        assert!(spec.agent.userns_keep_id);
        assert_eq!(
            spec.agent.containers[0].workdir.as_deref(),
            Some("/home/user/workspace")
        );
        assert_eq!(spec.agent.containers[0].ports, Vec::<u16>::new());
        assert_eq!(spec.agent.containers[0].name, "demo-agent-instance");
        assert_eq!(spec.proxy.containers[0].name, "demo-proxy-instance");
        assert!(
            spec.proxy
                .containers
                .iter()
                .any(|container| container.name == "demo-proxy-agent-proxy-socket")
        );
        assert!(
            spec.proxy
                .containers
                .iter()
                .any(|container| container.name == "demo-proxy-nw-sandbox-proxy-socket")
        );
        let agent_proxy_client = spec
            .agent
            .containers
            .iter()
            .find(|container| container.name == "demo-agent-proxy-client")
            .expect("agent proxy client");
        assert_eq!(
            agent_proxy_client.command,
            vec![
                "socat".to_string(),
                "TCP-LISTEN:3128,bind=127.0.0.1,fork,reuseaddr".to_string(),
                "UNIX-CONNECT:/run/cladding/sockets/agent-proxy.sock".to_string(),
            ]
        );
        assert!(
            agent_proxy_client
                .command
                .iter()
                .all(|arg| !arg.contains("/tmp/project/.cladding/runtime/sockets"))
        );
        let proxy_nw_listener = spec
            .proxy
            .containers
            .iter()
            .find(|container| container.name == "demo-proxy-nw-sandbox-proxy-socket")
            .expect("proxy nw listener");
        assert_eq!(
            proxy_nw_listener.command,
            vec![
                "socat".to_string(),
                "UNIX-LISTEN:/run/cladding/sockets/nw-sandbox-proxy.sock,fork,reuseaddr"
                    .to_string(),
                "TCP:127.0.0.1:3129".to_string(),
            ]
        );
        assert!(
            proxy_nw_listener
                .command
                .iter()
                .all(|arg| !arg.contains("/tmp/project/.cladding/runtime/sockets"))
        );
        assert_eq!(
            spec.nw_sandbox.as_ref().expect("nw pod").containers[0].name,
            "demo-nw-sandbox-instance"
        );
        let nw_proxy_client = spec
            .nw_sandbox
            .as_ref()
            .expect("nw pod")
            .containers
            .iter()
            .find(|container| container.name == "demo-nw-sandbox-proxy-client")
            .expect("nw proxy client");
        assert_eq!(
            nw_proxy_client.command,
            vec![
                "socat".to_string(),
                "TCP-LISTEN:3128,bind=127.0.0.1,fork,reuseaddr".to_string(),
                "UNIX-CONNECT:/run/cladding/sockets/nw-sandbox-proxy.sock".to_string(),
            ]
        );
        let nw_run_server = spec
            .nw_sandbox
            .as_ref()
            .expect("nw pod")
            .containers
            .iter()
            .find(|container| container.name == "demo-nw-sandbox-run-server")
            .expect("nw run server");
        assert_eq!(
            nw_run_server.command,
            vec![
                "socat".to_string(),
                "UNIX-LISTEN:/run/cladding/sockets/nw-sandbox-run.sock,fork,reuseaddr".to_string(),
                "TCP:127.0.0.1:3000".to_string(),
            ]
        );
        assert!(
            nw_proxy_client
                .command
                .iter()
                .all(|arg| !arg.contains("/tmp/project/.cladding/runtime/sockets"))
        );
        assert!(
            nw_run_server
                .command
                .iter()
                .all(|arg| !arg.contains("/tmp/project/.cladding/runtime/sockets"))
        );
        assert_eq!(
            spec.nw_sandbox
                .as_ref()
                .expect("nw pod")
                .containers
                .iter()
                .find(|container| container.name == "demo-nw-sandbox-instance")
                .expect("nw app")
                .ports,
            vec![3000]
        );
        assert!(
            spec.nw_sandbox
                .as_ref()
                .expect("nw pod")
                .containers
                .iter()
                .any(|container| container.name == "demo-nw-sandbox-run-server")
        );
        assert!(
            spec.fs_sandbox
                .as_ref()
                .expect("fs pod")
                .containers
                .iter()
                .any(|container| container.name == "demo-fs-sandbox-run-server")
        );
        assert_eq!(
            spec.agent
                .containers
                .iter()
                .find(|container| container.name == "demo-agent-instance")
                .expect("agent app")
                .env
                .iter()
                .find(|env| env.name == "RUN_NW_SANDBOX_SERVER")
                .map(|env| env.value.as_str()),
            Some("http://127.0.0.1:3001/raw")
        );
        assert_eq!(
            spec.agent
                .containers
                .iter()
                .find(|container| container.name == "demo-agent-instance")
                .expect("agent app")
                .env
                .iter()
                .find(|env| env.name == "RUN_FS_SANDBOX_SERVER")
                .map(|env| env.value.as_str()),
            Some("http://127.0.0.1:3002/raw")
        );
        assert_eq!(
            spec.fs_sandbox.as_ref().expect("fs pod").containers[0].image,
            "fs:image"
        );
        assert_eq!(
            spec.required_host_paths()
                .into_iter()
                .collect::<BTreeSet<_>>(),
            [
                PathBuf::from("/tmp/project/.cladding/.."),
                PathBuf::from("/tmp/project/.cladding/config"),
                PathBuf::from("/tmp/project/.cladding/home"),
                PathBuf::from("/tmp/project/.cladding/scripts"),
                PathBuf::from("/tmp/project/.cladding/tools"),
            ]
            .into_iter()
            .collect()
        );
    }

    #[test]
    fn build_runtime_spec_only_includes_enabled_components() {
        let config = execution_config(true, false, Vec::new());
        let spec = RuntimeSpec::build(Path::new("/tmp/project/.cladding"), &config);

        assert!(spec.nw_sandbox.is_some());
        assert!(spec.fs_sandbox.is_none());
        assert_eq!(spec.proxy.containers[0].name, "demo-proxy-instance");
        assert_eq!(spec.agent.containers[0].name, "demo-agent-instance");
        assert_eq!(
            spec.nw_sandbox.as_ref().expect("nw pod").containers[0].name,
            "demo-nw-sandbox-instance"
        );
        assert!(
            spec.agent
                .containers
                .iter()
                .any(|container| container.name == "demo-agent-nw-sandbox-run-client")
        );
        assert!(
            spec.agent
                .containers
                .iter()
                .all(|container| container.name != "demo-agent-fs-sandbox-run-client")
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
        let spec = RuntimeSpec::build(Path::new("/tmp/project/.cladding"), &config);
        let required = spec.required_host_paths();

        assert!(required.contains(&PathBuf::from("/tmp/project/.cladding/config")));
        assert!(required.contains(&PathBuf::from("/tmp/project/.cladding/scripts")));
        assert!(required.contains(&PathBuf::from("/tmp/project/.cladding/tools")));
        assert!(required.contains(&PathBuf::from("/tmp/project/.cladding/home")));
        assert!(required.contains(&PathBuf::from("/tmp/project/.cladding/..")));
        assert!(!required.contains(&PathBuf::from("/tmp/project/.cladding/runtime/empty-mask")));
        assert!(required.contains(&PathBuf::from("/tmp/workspace")));
        assert!(!required.contains(&PathBuf::from("/tmp/project/.cladding/runtime/sockets")));
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
        let spec = RuntimeSpec::build(Path::new("/tmp/project/.cladding"), &config);
        let required = spec.required_host_paths();

        assert!(!required.contains(&PathBuf::from("/tmp/ignored")));
        assert!(!required.contains(&PathBuf::from("/tmp/inactive")));
    }

    #[test]
    fn runtime_expose_socket_path_uses_stable_layout() {
        assert_eq!(
            runtime_expose_socket_path(Path::new("/tmp/project/.cladding"), "agent", 3000, 9000),
            PathBuf::from("/tmp/project/.cladding/runtime/expose/agent-3000-9000.sock")
        );
    }
}
