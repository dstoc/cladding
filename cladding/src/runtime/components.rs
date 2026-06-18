use super::commands::{build_idle_supervisor_command, build_mcp_run_supervisor_command};
use super::mounts::{
    apply_custom_mounts, build_agent_mounts, build_custom_mounts, build_fs_sandbox_mounts,
    build_proxy_mounts, build_sandbox_mounts,
};
use super::sockets::{
    RUNTIME_AGENT_INJECT_MOUNT_PATH, RUNTIME_AGENT_INJECT_SOCKET_DIR,
    RUNTIME_PROXY_AGENT_MOUNT_PATH, RUNTIME_PROXY_AGENT_SOCKET_DIR,
    RUNTIME_PROXY_NW_SANDBOX_MOUNT_PATH, RUNTIME_PROXY_NW_SANDBOX_SOCKET_DIR,
    RUNTIME_RUN_FS_SANDBOX_MOUNT_PATH, RUNTIME_RUN_FS_SANDBOX_SOCKET_DIR,
    RUNTIME_RUN_NW_SANDBOX_MOUNT_PATH, RUNTIME_RUN_NW_SANDBOX_SOCKET_DIR,
    build_scoped_socket_mount, runtime_socket_mount_path,
};
use super::types::{
    RuntimeContainer, RuntimeCustomMount, RuntimeEnvVar, RuntimeNames, RuntimePlacement,
    RuntimePod, RuntimeSpec,
};
use crate::config::{ExecutionConfig, MountTarget};
use std::collections::BTreeMap;
use std::path::Path;

const LOOPBACK: &str = "127.0.0.1";
const NETWORK_DEFAULT: &str = "default";
const NETWORK_NONE: &str = "none";

impl RuntimeSpec {
    pub fn build(project_root: &Path, config: &ExecutionConfig) -> Self {
        let project_root = project_root.to_path_buf();
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
            use_runsc: config.use_runsc,
            proxy,
            agent,
            nw_sandbox,
            fs_sandbox,
        }
    }
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

    containers.push(build_proxy_bridge_sidecar(
        &names.proxy_name,
        project_root,
        names.nw_sandbox_name.is_some(),
    ));

    RuntimePod {
        name: names.proxy_name.clone(),
        placement: RuntimePlacement::Pod,
        use_runsc: false,
        labels: build_labels(&config.name, project_root, "proxy"),
        network_name: NETWORK_DEFAULT.to_string(),
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
    let mut mounts = apply_custom_mounts(
        build_agent_mounts(project_root, custom_mounts),
        custom_mounts,
        MountTarget::Agent,
    );
    mounts.extend(build_scoped_socket_mount(
        project_root,
        RUNTIME_AGENT_INJECT_SOCKET_DIR,
        RUNTIME_AGENT_INJECT_MOUNT_PATH,
    ));
    mounts.extend(build_scoped_socket_mount(
        project_root,
        RUNTIME_PROXY_AGENT_SOCKET_DIR,
        RUNTIME_PROXY_AGENT_MOUNT_PATH,
    ));
    if names.nw_sandbox_name.is_some() {
        mounts.extend(build_scoped_socket_mount(
            project_root,
            RUNTIME_RUN_NW_SANDBOX_SOCKET_DIR,
            RUNTIME_RUN_NW_SANDBOX_MOUNT_PATH,
        ));
    }
    if names.fs_sandbox_name.is_some() {
        mounts.extend(build_scoped_socket_mount(
            project_root,
            RUNTIME_RUN_FS_SANDBOX_SOCKET_DIR,
            RUNTIME_RUN_FS_SANDBOX_MOUNT_PATH,
        ));
    }

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
            name: "RUN_NW_SANDBOX_SOCKET".to_string(),
            value: runtime_socket_mount_path(RUNTIME_RUN_NW_SANDBOX_MOUNT_PATH, "run.sock"),
        });
    }
    if names.fs_sandbox_name.is_some() {
        env.push(RuntimeEnvVar {
            name: "RUN_FS_SANDBOX_SOCKET".to_string(),
            value: runtime_socket_mount_path(RUNTIME_RUN_FS_SANDBOX_MOUNT_PATH, "run.sock"),
        });
    }

    RuntimePod {
        name: names.agent_name.clone(),
        placement: RuntimePlacement::Standalone,
        use_runsc: config.use_runsc,
        labels: build_labels(&config.name, project_root, "agent"),
        network_name: NETWORK_NONE.to_string(),
        containers: vec![RuntimeContainer {
            name: runtime_container_name(&names.agent_name),
            image: config.agent_image().to_string(),
            command: build_idle_supervisor_command(
                "socat TCP-LISTEN:3128,bind=127.0.0.1,fork,reuseaddr UNIX-CONNECT:/run/cladding/proxy/agent/proxy.sock",
                "sleep infinity",
            ),
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
    names: &RuntimeNames,
    component_name: &str,
    custom_mounts: &[RuntimeCustomMount],
) -> RuntimePod {
    let mut mounts = apply_custom_mounts(
        build_sandbox_mounts(project_root, custom_mounts),
        custom_mounts,
        MountTarget::NwSandbox,
    );
    mounts.extend(build_scoped_socket_mount(
        project_root,
        RUNTIME_PROXY_NW_SANDBOX_SOCKET_DIR,
        RUNTIME_PROXY_NW_SANDBOX_MOUNT_PATH,
    ));
    mounts.extend(build_scoped_socket_mount(
        project_root,
        RUNTIME_RUN_NW_SANDBOX_SOCKET_DIR,
        RUNTIME_RUN_NW_SANDBOX_MOUNT_PATH,
    ));

    let mut env = vec![
        RuntimeEnvVar {
            name: "PATH".to_string(),
            value: "/opt/tools/bin:/usr/local/sbin:/usr/local/bin:/usr/sbin:/usr/bin:/sbin:/bin"
                .to_string(),
        },
        RuntimeEnvVar {
            name: "MCP_BIND_UDS".to_string(),
            value: runtime_socket_mount_path(RUNTIME_RUN_NW_SANDBOX_MOUNT_PATH, "run.sock"),
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

    RuntimePod {
        name: component_name.to_string(),
        placement: RuntimePlacement::Standalone,
        use_runsc: config.use_runsc,
        labels: build_labels(&config.name, project_root, "nw-sandbox"),
        network_name: NETWORK_NONE.to_string(),
        containers: vec![RuntimeContainer {
            name: runtime_container_name(component_name),
            image: config.nw_sandbox_image().to_string(),
            command: build_mcp_run_supervisor_command(
                "socat TCP-LISTEN:3128,bind=127.0.0.1,fork,reuseaddr UNIX-CONNECT:/run/cladding/proxy/nw-sandbox/proxy.sock",
                "mcp-run",
            ),
            workdir: Some("/home/user/workspace".to_string()),
            env,
            mounts,
            ports: Vec::new(),
            stdin: false,
            tty: false,
        }],
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
    let mut mounts = apply_custom_mounts(
        build_fs_sandbox_mounts(project_root, custom_mounts),
        custom_mounts,
        MountTarget::FsSandbox,
    );
    mounts.extend(build_scoped_socket_mount(
        project_root,
        RUNTIME_RUN_FS_SANDBOX_SOCKET_DIR,
        RUNTIME_RUN_FS_SANDBOX_MOUNT_PATH,
    ));

    RuntimePod {
        name: component_name.to_string(),
        placement: RuntimePlacement::Standalone,
        use_runsc: config.use_runsc,
        labels: build_labels(&config.name, project_root, "fs-sandbox"),
        network_name: NETWORK_NONE.to_string(),
        containers: vec![RuntimeContainer {
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
                    name: "MCP_BIND_UDS".to_string(),
                    value: runtime_socket_mount_path(RUNTIME_RUN_FS_SANDBOX_MOUNT_PATH, "run.sock"),
                },
                RuntimeEnvVar {
                    name: "POLICY_DIR".to_string(),
                    value: "/opt/config/fs_sandbox".to_string(),
                },
            ],
            mounts,
            ports: Vec::new(),
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

fn build_no_proxy() -> String {
    ["localhost", LOOPBACK].join(",")
}

fn runtime_container_name(pod_name: &str) -> String {
    format!("{pod_name}-instance")
}

fn build_proxy_bridge_sidecar(
    pod_name: &str,
    project_root: &Path,
    nw_sandbox_enabled: bool,
) -> RuntimeContainer {
    let mut bridge_commands = vec![format!(
        "socat UNIX-LISTEN:{},fork,reuseaddr TCP:127.0.0.1:3128",
        runtime_socket_mount_path(RUNTIME_PROXY_AGENT_MOUNT_PATH, "proxy.sock")
    )];
    let mut mounts = build_scoped_socket_mount(
        project_root,
        RUNTIME_PROXY_AGENT_SOCKET_DIR,
        RUNTIME_PROXY_AGENT_MOUNT_PATH,
    );
    if nw_sandbox_enabled {
        bridge_commands.push(format!(
            "socat UNIX-LISTEN:{},fork,reuseaddr TCP:127.0.0.1:3129",
            runtime_socket_mount_path(RUNTIME_PROXY_NW_SANDBOX_MOUNT_PATH, "proxy.sock")
        ));
        mounts.extend(build_scoped_socket_mount(
            project_root,
            RUNTIME_PROXY_NW_SANDBOX_SOCKET_DIR,
            RUNTIME_PROXY_NW_SANDBOX_MOUNT_PATH,
        ));
    }

    let mut script = String::from("set -eu\n");
    script.push_str("pids=\"\"\n");
    for command in &bridge_commands {
        script.push_str(command);
        script.push_str(" &\n");
        script.push_str("pids=\"$pids $!\"\n");
    }
    script.push_str("trap 'kill $pids 2>/dev/null || true' INT TERM\n");
    script.push_str(
        r#"
while true; do
  for pid in $pids; do
    if ! kill -0 "$pid" 2>/dev/null; then
      wait "$pid" 2>/dev/null || true
      exit 1
    fi
  done
  sleep 1
done
"#,
    );

    RuntimeContainer {
        name: format!("{pod_name}-bridge"),
        image: "alpine/socat".to_string(),
        command: vec!["/bin/sh".to_string(), "-ec".to_string(), script],
        workdir: None,
        env: Vec::new(),
        mounts,
        ports: Vec::new(),
        stdin: false,
        tty: false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::{ExecutionComponentConfig, ExecutionConfig, ResolvedMountConfig};
    use std::collections::BTreeSet;
    use std::path::PathBuf;

    fn execution_config(
        nw_enabled: bool,
        fs_enabled: bool,
        mounts: Vec<ResolvedMountConfig>,
        use_runsc: bool,
    ) -> ExecutionConfig {
        ExecutionConfig {
            name: "demo".to_string(),
            use_runsc,
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

    fn container<'a>(pod: &'a RuntimePod, name: &str) -> &'a RuntimeContainer {
        pod.containers
            .iter()
            .find(|container| container.name == name)
            .expect("container")
    }

    fn env_value<'a>(container: &'a RuntimeContainer, name: &str) -> Option<&'a str> {
        container
            .env
            .iter()
            .find(|env| env.name == name)
            .map(|env| env.value.as_str())
    }

    fn mount_paths(container: &RuntimeContainer) -> BTreeSet<&str> {
        container
            .mounts
            .iter()
            .map(|mount| mount.mount_path.as_str())
            .collect()
    }

    #[test]
    fn build_runtime_spec_fully_enabled_uses_five_containers() {
        let config = execution_config(true, true, Vec::new(), false);
        let spec = RuntimeSpec::build(Path::new("/tmp/project/.cladding"), &config);

        assert_eq!(spec.project_name, "demo");
        assert!(!spec.use_runsc);
        assert_eq!(spec.proxy.name, "demo-proxy");
        assert_eq!(spec.agent.containers[0].name, "demo-agent-instance");
        assert_eq!(
            spec.proxy.labels.get("project_root").map(String::as_str),
            Some("/tmp/project/.cladding")
        );
        assert_eq!(spec.proxy.network_name, "default");
        assert_eq!(spec.proxy.placement, RuntimePlacement::Pod);
        assert_eq!(spec.agent.network_name, "none");
        assert!(spec.agent.userns_keep_id);
        assert_eq!(spec.agent.placement, RuntimePlacement::Standalone);
        assert_eq!(spec.proxy.containers.len(), 2);
        assert_eq!(spec.agent.containers.len(), 1);
        assert_eq!(
            spec.nw_sandbox.as_ref().expect("nw pod").containers.len(),
            1
        );
        assert_eq!(
            spec.fs_sandbox.as_ref().expect("fs pod").containers.len(),
            1
        );
        assert_eq!(spec.proxy.containers[0].name, "demo-proxy-instance");
        assert_eq!(spec.proxy.containers[1].name, "demo-proxy-bridge");
        assert_eq!(spec.agent.containers[0].name, "demo-agent-instance");
        assert_eq!(
            spec.nw_sandbox.as_ref().expect("nw pod").containers[0].name,
            "demo-nw-sandbox-instance"
        );
        assert_eq!(
            spec.fs_sandbox.as_ref().expect("fs pod").containers[0].name,
            "demo-fs-sandbox-instance"
        );

        let proxy_bridge = container(&spec.proxy, "demo-proxy-bridge");
        let agent = container(&spec.agent, "demo-agent-instance");
        let nw = container(
            spec.nw_sandbox.as_ref().expect("nw pod"),
            "demo-nw-sandbox-instance",
        );
        let fs = container(
            spec.fs_sandbox.as_ref().expect("fs pod"),
            "demo-fs-sandbox-instance",
        );

        assert!(
            proxy_bridge
                .command
                .iter()
                .any(|arg| arg.contains("proxy/agent/proxy.sock"))
        );
        assert!(
            proxy_bridge
                .command
                .iter()
                .any(|arg| arg.contains("proxy/nw-sandbox/proxy.sock"))
        );
        assert!(
            agent
                .command
                .iter()
                .any(|arg| arg.contains("proxy/agent/proxy.sock"))
        );
        assert!(
            agent
                .command
                .iter()
                .any(|arg| arg.contains("sleep infinity"))
        );
        assert!(
            nw.command
                .iter()
                .any(|arg| arg.contains("proxy/nw-sandbox/proxy.sock"))
        );
        assert!(nw.command.iter().any(|arg| arg.contains("mcp-run")));
        assert_eq!(
            env_value(agent, "RUN_NW_SANDBOX_SOCKET"),
            Some("/run/cladding/run/nw-sandbox/run.sock")
        );
        assert_eq!(
            env_value(agent, "RUN_FS_SANDBOX_SOCKET"),
            Some("/run/cladding/run/fs-sandbox/run.sock")
        );
        assert_eq!(env_value(agent, "RUN_NW_SANDBOX_SERVER"), None);
        assert_eq!(env_value(agent, "RUN_FS_SANDBOX_SERVER"), None);
        assert_eq!(
            env_value(nw, "MCP_BIND_UDS"),
            Some("/run/cladding/run/nw-sandbox/run.sock")
        );
        assert_eq!(env_value(nw, "MCP_BIND_ADDR"), None);
        assert_eq!(
            env_value(fs, "MCP_BIND_UDS"),
            Some("/run/cladding/run/fs-sandbox/run.sock")
        );
        assert_eq!(env_value(fs, "MCP_BIND_ADDR"), None);

        assert!(mount_paths(agent).contains("/run/cladding/proxy/agent"));
        assert!(mount_paths(agent).contains("/run/cladding/agent/inject"));
        assert!(mount_paths(agent).contains("/run/cladding/run/nw-sandbox"));
        assert!(mount_paths(agent).contains("/run/cladding/run/fs-sandbox"));
        assert!(mount_paths(nw).contains("/run/cladding/proxy/nw-sandbox"));
        assert!(mount_paths(nw).contains("/run/cladding/run/nw-sandbox"));
        assert!(mount_paths(fs).contains("/run/cladding/run/fs-sandbox"));
        assert!(mount_paths(proxy_bridge).contains("/run/cladding/proxy/agent"));
        assert!(mount_paths(proxy_bridge).contains("/run/cladding/proxy/nw-sandbox"));
        let fs_mounts = mount_paths(fs);
        assert!(fs_mounts.contains("/opt/config"));
        assert!(fs_mounts.contains("/opt/tools"));
        assert!(!fs_mounts.contains("/home/user"));
        assert!(!fs_mounts.contains("/home/user/workspace"));
        assert!(!fs_mounts.contains("/home/user/workspace/.cladding"));

        for container in [&spec.proxy.containers[1], agent, nw, fs] {
            assert!(
                container
                    .command
                    .iter()
                    .all(|arg| !arg.contains("/run/cladding/sockets"))
            );
        }
        for container in [&spec.proxy.containers[1], agent, nw, fs] {
            assert!(container.name != "demo-agent-proxy-client");
            assert!(container.name != "demo-agent-nw-sandbox-run-client");
            assert!(container.name != "demo-agent-fs-sandbox-run-client");
            assert!(container.name != "demo-nw-sandbox-proxy-client");
            assert!(container.name != "demo-nw-sandbox-run-server");
            assert!(container.name != "demo-fs-sandbox-run-server");
        }
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
                PathBuf::from("/tmp/project/.cladding/tools"),
            ]
            .into_iter()
            .collect()
        );
    }

    #[test]
    fn build_runtime_spec_only_includes_enabled_components() {
        let config = execution_config(true, false, Vec::new(), false);
        let spec = RuntimeSpec::build(Path::new("/tmp/project/.cladding"), &config);
        let agent = container(&spec.agent, "demo-agent-instance");

        assert!(spec.nw_sandbox.is_some());
        assert!(spec.fs_sandbox.is_none());
        assert_eq!(spec.proxy.containers.len(), 2);
        assert_eq!(spec.agent.containers.len(), 1);
        assert_eq!(
            spec.nw_sandbox.as_ref().expect("nw pod").containers.len(),
            1
        );
        assert!(
            spec.agent
                .containers
                .iter()
                .all(|container| container.name != "demo-agent-proxy-client")
        );
        assert!(
            spec.agent
                .containers
                .iter()
                .all(|container| container.name != "demo-agent-nw-sandbox-run-client")
        );
        assert!(
            spec.agent
                .containers
                .iter()
                .all(|container| container.name != "demo-agent-fs-sandbox-run-client")
        );
        assert!(
            spec.proxy
                .containers
                .iter()
                .all(|container| container.name != "demo-proxy-agent-proxy-socket")
        );
        assert!(
            spec.proxy
                .containers
                .iter()
                .all(|container| container.name != "demo-proxy-nw-sandbox-proxy-socket")
        );
        assert_eq!(
            env_value(agent, "RUN_NW_SANDBOX_SOCKET"),
            Some("/run/cladding/run/nw-sandbox/run.sock")
        );
        assert_eq!(env_value(agent, "RUN_FS_SANDBOX_SOCKET"), None);
        assert_eq!(env_value(agent, "RUN_NW_SANDBOX_SERVER"), None);
        assert_eq!(env_value(agent, "RUN_FS_SANDBOX_SERVER"), None);
    }

    #[test]
    fn runtime_spec_threads_use_runsc_flag() {
        let config = execution_config(false, false, Vec::new(), true);
        let spec = RuntimeSpec::build(Path::new("/tmp/project/.cladding"), &config);

        assert!(spec.use_runsc);
        assert_eq!(spec.proxy.placement, RuntimePlacement::Pod);
        assert!(!spec.proxy.use_runsc);
        assert_eq!(spec.agent.placement, RuntimePlacement::Standalone);
        assert!(spec.agent.use_runsc);
        assert!(spec.agent.userns_keep_id);
    }

    #[test]
    fn execution_components_are_standalone_containers() {
        let config = execution_config(true, true, Vec::new(), true);
        let spec = RuntimeSpec::build(Path::new("/tmp/project/.cladding"), &config);

        assert_eq!(spec.proxy.placement, RuntimePlacement::Pod);
        assert!(!spec.proxy.use_runsc);
        assert_eq!(spec.agent.placement, RuntimePlacement::Standalone);
        assert!(spec.agent.use_runsc);
        assert_eq!(
            spec.nw_sandbox.as_ref().expect("nw pod").placement,
            RuntimePlacement::Standalone
        );
        assert!(spec.nw_sandbox.as_ref().expect("nw pod").use_runsc);
        assert_eq!(
            spec.fs_sandbox.as_ref().expect("fs pod").placement,
            RuntimePlacement::Standalone
        );
        assert!(spec.fs_sandbox.as_ref().expect("fs pod").use_runsc);
    }
}
