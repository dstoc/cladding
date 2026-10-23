use super::commands::build_supervisor_command;
use super::mounts::{
    apply_custom_mounts, build_custom_mounts_with_snapshots, build_execution_mounts,
    build_fs_sandbox_mounts, build_proxy_mounts, canonical_or_normalized,
};
use super::runtime_container_name;
use super::sockets::{
    RUNTIME_PROXY_AGENT_MOUNT_PATH, RUNTIME_PROXY_NW_SANDBOX_MOUNT_PATH,
    RUNTIME_RUN_FS_SANDBOX_MOUNT_PATH, RUNTIME_RUN_NW_SANDBOX_MOUNT_PATH,
    build_socket_volume_mount, runtime_socket_mount_path,
};
use super::types::{
    ManagedVolumeKind, RuntimeComponent, RuntimeContainer, RuntimeCustomMount, RuntimeEnvVar,
    RuntimeNames, RuntimeSpec,
};
use crate::config::{ExecutionConfig, MountTarget, MountType, ResolvedMountConfig};
use crate::error::{Error, Result};
use serde::Serialize;
use std::collections::BTreeMap;
use std::path::Path;

const LOOPBACK: &str = "127.0.0.1";
const NETWORK_DEFAULT: &str = "default";
const NETWORK_NONE: &str = "none";

impl RuntimeSpec {
    pub fn build(project_root: &Path, config: &ExecutionConfig) -> Self {
        Self::build_with_runtime_root(project_root, project_root, config)
    }

    pub fn build_with_runtime_root(
        project_root: &Path,
        runtime_root: &Path,
        config: &ExecutionConfig,
    ) -> Self {
        let project_root = project_root.to_path_buf();
        let runtime_root = runtime_root.to_path_buf();
        let (custom_mounts, custom_mount_sources) = build_custom_mounts_with_snapshots(
            &project_root,
            &runtime_root,
            &config.name,
            &config.mounts,
        );
        crate::runtime_metrics::record_runtime_spec_build(custom_mount_sources.len());
        let names = RuntimeNames::from_config(config);

        let proxy = build_proxy_component(&project_root, &runtime_root, config, &names);
        let agent = build_agent_component(&project_root, config, &names, &custom_mounts);
        let nw_sandbox = names.nw_sandbox_name.as_ref().map(|component_name| {
            build_nw_sandbox_component(&project_root, config, component_name, &custom_mounts)
        });
        let fs_sandbox = names.fs_sandbox_name.as_ref().map(|component_name| {
            build_fs_sandbox_component(&project_root, config, component_name, &custom_mounts)
        });

        Self {
            project_name: config.name.clone(),
            project_root,
            runtime_root,
            use_runsc: config.use_runsc,
            proxy,
            agent,
            nw_sandbox,
            fs_sandbox,
            custom_mount_sources,
            run_control_socket: None,
            run_expected_roles: Vec::new(),
        }
    }

    pub fn set_run_control_metadata(
        &mut self,
        socket_path: &Path,
        config: &ExecutionConfig,
    ) -> Result<()> {
        let mut expected_roles = vec!["proxy".to_string(), "agent".to_string()];
        if self.nw_sandbox.is_some() {
            expected_roles.push("nw-sandbox".to_string());
        }
        if self.fs_sandbox.is_some() {
            expected_roles.push("fs-sandbox".to_string());
        }
        self.run_control_socket = Some(socket_path.to_path_buf());
        self.run_expected_roles = expected_roles.clone();
        set_run_component_metadata(&mut self.proxy, socket_path, &expected_roles, Vec::new())?;
        let agent_mappings = run_cwd_mappings(
            &self.agent.container.mounts,
            &config.mounts,
            MountTarget::Agent,
        );
        set_run_component_metadata(
            &mut self.agent,
            socket_path,
            &expected_roles,
            agent_mappings,
        )?;
        if let Some(component) = &mut self.nw_sandbox {
            let mappings = run_cwd_mappings(
                &component.container.mounts,
                &config.mounts,
                MountTarget::NwSandbox,
            );
            set_run_component_metadata(component, socket_path, &expected_roles, mappings)?;
        }
        if let Some(component) = &mut self.fs_sandbox {
            let mappings = run_cwd_mappings(
                &component.container.mounts,
                &config.mounts,
                MountTarget::FsSandbox,
            );
            set_run_component_metadata(component, socket_path, &expected_roles, mappings)?;
        }
        Ok(())
    }

    pub(crate) fn components(&self) -> Vec<&RuntimeComponent> {
        let mut components = vec![&self.proxy, &self.agent];
        if let Some(component) = &self.nw_sandbox {
            components.push(component);
        }
        if let Some(component) = &self.fs_sandbox {
            components.push(component);
        }
        components
    }
}

#[derive(Serialize)]
struct RunCwdMapping {
    host_source: String,
    destination: String,
    mount_type: &'static str,
    read_only: bool,
    volume_name: Option<String>,
}

fn set_run_component_metadata(
    component: &mut RuntimeComponent,
    socket_path: &Path,
    expected_roles: &[String],
    mappings: Vec<RunCwdMapping>,
) -> Result<()> {
    component
        .labels
        .insert("cladding_run_metadata_version".to_string(), "1".to_string());
    component.labels.insert(
        "cladding_run_control_socket".to_string(),
        socket_path.display().to_string(),
    );
    component.labels.insert(
        "cladding_run_expected_roles".to_string(),
        expected_roles.join(","),
    );
    component.labels.insert(
        "cladding_run_cwd_v1".to_string(),
        serde_json::to_string(&mappings)
            .map_err(|_| Error::message("failed to encode run working-directory metadata"))?,
    );
    Ok(())
}

fn run_cwd_mappings(
    runtime_mounts: &[super::types::RuntimeMount],
    configured_mounts: &[ResolvedMountConfig],
    target: MountTarget,
) -> Vec<RunCwdMapping> {
    configured_mounts
        .iter()
        .filter(|mount| {
            !mount.ignore
                && mount.targets.contains(&target)
                && matches!(
                    mount.mount_type,
                    MountType::Bind | MountType::Readonly | MountType::Copy
                )
        })
        .filter_map(|mount| {
            let configured_host_source = mount.host_path.as_ref()?;
            let host_source = canonical_or_normalized(configured_host_source);
            let host_source_label = host_source.to_str()?;
            if !host_source.is_absolute()
                || host_source
                    .components()
                    .any(|component| matches!(component, std::path::Component::ParentDir))
                || Path::new(&mount.mount_path)
                    .components()
                    .any(|component| matches!(component, std::path::Component::ParentDir))
            {
                return None;
            }
            let actual = runtime_mounts.iter().find(|runtime| {
                if runtime.mount_path != mount.mount_path {
                    return false;
                }
                match (mount.mount_type, &runtime.source) {
                    (
                        MountType::Bind | MountType::Readonly,
                        super::types::RuntimeMountSource::HostPath { path },
                    ) => path == configured_host_source,
                    (
                        MountType::Copy,
                        super::types::RuntimeMountSource::ManagedVolume {
                            kind: ManagedVolumeKind::Copy { source },
                            ..
                        },
                    ) => source == configured_host_source,
                    _ => false,
                }
            })?;
            let volume_name = match &actual.source {
                super::types::RuntimeMountSource::ManagedVolume { claim_name, .. } => {
                    Some(claim_name.clone())
                }
                _ => None,
            };
            Some(RunCwdMapping {
                host_source: host_source_label.to_string(),
                destination: mount.mount_path.clone(),
                mount_type: match mount.mount_type {
                    MountType::Bind => "bind",
                    MountType::Readonly => "readonly",
                    MountType::Copy => "copy",
                    MountType::Tmpfs => return None,
                },
                read_only: mount.read_only,
                volume_name,
            })
        })
        .collect()
}

fn build_proxy_component(
    project_root: &Path,
    runtime_root: &Path,
    config: &ExecutionConfig,
    names: &RuntimeNames,
) -> RuntimeComponent {
    let mut mounts = build_proxy_mounts(project_root, runtime_root);
    mounts.push(build_socket_volume_mount(
        &config.name,
        "baffle-agent",
        RUNTIME_PROXY_AGENT_MOUNT_PATH,
        true,
    ));
    if config.nw_sandbox_enabled() {
        mounts.push(build_socket_volume_mount(
            &config.name,
            "baffle-nw-sandbox",
            RUNTIME_PROXY_NW_SANDBOX_MOUNT_PATH,
            true,
        ));
    }
    let container = RuntimeContainer {
        name: runtime_container_name(&names.proxy_name),
        image: config.proxy_image().to_string(),
        command: vec![
            "/bin/sh".to_string(),
            "/opt/scripts/proxy_startup.sh".to_string(),
        ],
        workdir: None,
        env: vec![
            RuntimeEnvVar {
                name: "CLADDING_NW_SANDBOX_ENABLED".to_string(),
                value: config.nw_sandbox_enabled().to_string(),
            },
            RuntimeEnvVar {
                name: "CLADDING_AGENT_SESSION_CONFIG".to_string(),
                value: config.agent_session_config().to_string(),
            },
            RuntimeEnvVar {
                name: "CLADDING_NW_SANDBOX_SESSION_CONFIG".to_string(),
                value: config.nw_sandbox_session_config().to_string(),
            },
        ],
        mounts,
        stdin: false,
        tty: false,
    };

    RuntimeComponent {
        name: names.proxy_name.clone(),
        use_runsc: false,
        init: true,
        security_opts: Vec::new(),
        labels: build_labels(&config.name, project_root, "proxy"),
        network_name: NETWORK_DEFAULT.to_string(),
        container,
    }
}

fn build_agent_component(
    project_root: &Path,
    config: &ExecutionConfig,
    names: &RuntimeNames,
    custom_mounts: &[RuntimeCustomMount],
) -> RuntimeComponent {
    let mut mounts = apply_custom_mounts(
        build_execution_mounts(project_root),
        custom_mounts,
        MountTarget::Agent,
    );
    mounts.push(build_socket_volume_mount(
        &config.name,
        "baffle-agent",
        RUNTIME_PROXY_AGENT_MOUNT_PATH,
        false,
    ));
    if names.nw_sandbox_name.is_some() {
        mounts.push(build_socket_volume_mount(
            &config.name,
            "run-nw-sandbox",
            RUNTIME_RUN_NW_SANDBOX_MOUNT_PATH,
            false,
        ));
    }
    if names.fs_sandbox_name.is_some() {
        mounts.push(build_socket_volume_mount(
            &config.name,
            "run-fs-sandbox",
            RUNTIME_RUN_FS_SANDBOX_MOUNT_PATH,
            false,
        ));
    }

    let mut env = vec![
        RuntimeEnvVar {
            name: "PATH".to_string(),
            value: "/opt/tools/bin:/usr/local/sbin:/usr/local/bin:/usr/sbin:/usr/bin:/sbin:/bin"
                .to_string(),
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
    if names.nw_sandbox_name.is_some() {
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

    RuntimeComponent {
        name: names.agent_name.clone(),
        use_runsc: config.use_runsc,
        init: config.agent.init,
        security_opts: config.agent.security_opts.clone(),
        labels: build_labels(&config.name, project_root, "agent"),
        network_name: NETWORK_NONE.to_string(),
        container: RuntimeContainer {
            name: runtime_container_name(&names.agent_name),
            image: config.agent_image().to_string(),
            command: build_supervisor_command(
                "socat TCP-LISTEN:3128,bind=127.0.0.1,fork,reuseaddr UNIX-CONNECT:/run/cladding/proxy/agent/proxy.sock",
                "sleep infinity",
            ),
            workdir: Some("/home/user".to_string()),
            env,
            mounts,
            stdin: true,
            tty: true,
        },
    }
}

fn build_nw_sandbox_component(
    project_root: &Path,
    config: &ExecutionConfig,
    component_name: &str,
    custom_mounts: &[RuntimeCustomMount],
) -> RuntimeComponent {
    let mut mounts = apply_custom_mounts(
        build_execution_mounts(project_root),
        custom_mounts,
        MountTarget::NwSandbox,
    );
    mounts.push(build_socket_volume_mount(
        &config.name,
        "baffle-nw-sandbox",
        RUNTIME_PROXY_NW_SANDBOX_MOUNT_PATH,
        false,
    ));
    mounts.push(build_socket_volume_mount(
        &config.name,
        "run-nw-sandbox",
        RUNTIME_RUN_NW_SANDBOX_MOUNT_PATH,
        true,
    ));

    let mut env = vec![
        RuntimeEnvVar {
            name: "PATH".to_string(),
            value: "/opt/tools/bin:/usr/local/sbin:/usr/local/bin:/usr/sbin:/usr/bin:/sbin:/bin"
                .to_string(),
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

    RuntimeComponent {
        name: component_name.to_string(),
        use_runsc: config.use_runsc,
        init: config
            .nw_sandbox
            .as_ref()
            .map(|component| component.init)
            .unwrap_or(true),
        security_opts: config
            .nw_sandbox
            .as_ref()
            .map(|component| component.security_opts.clone())
            .unwrap_or_default(),
        labels: build_labels(&config.name, project_root, "nw-sandbox"),
        network_name: NETWORK_NONE.to_string(),
        container: RuntimeContainer {
            name: runtime_container_name(component_name),
            image: config.nw_sandbox_image().to_string(),
            command: build_supervisor_command(
                "socat TCP-LISTEN:3128,bind=127.0.0.1,fork,reuseaddr UNIX-CONNECT:/run/cladding/proxy/nw-sandbox/proxy.sock",
                "chmod 0700 /run/cladding/run/nw-sandbox && umask 077 && exec gantry --backend direct --bind-uds /run/cladding/run/nw-sandbox/run.sock --policy-dir /opt/config/nw_sandbox",
            ),
            workdir: Some("/home/user".to_string()),
            env,
            mounts,
            stdin: false,
            tty: false,
        },
    }
}

fn build_fs_sandbox_component(
    project_root: &Path,
    config: &ExecutionConfig,
    component_name: &str,
    custom_mounts: &[RuntimeCustomMount],
) -> RuntimeComponent {
    let mut mounts = apply_custom_mounts(
        build_fs_sandbox_mounts(project_root),
        custom_mounts,
        MountTarget::FsSandbox,
    );
    mounts.push(build_socket_volume_mount(
        &config.name,
        "run-fs-sandbox",
        RUNTIME_RUN_FS_SANDBOX_MOUNT_PATH,
        true,
    ));

    RuntimeComponent {
        name: component_name.to_string(),
        use_runsc: config.use_runsc,
        init: config
            .fs_sandbox
            .as_ref()
            .map(|component| component.init)
            .unwrap_or(true),
        security_opts: config
            .fs_sandbox
            .as_ref()
            .map(|component| component.security_opts.clone())
            .unwrap_or_default(),
        labels: build_labels(&config.name, project_root, "fs-sandbox"),
        network_name: NETWORK_NONE.to_string(),
        container: RuntimeContainer {
            name: runtime_container_name(component_name),
            image: config.fs_sandbox_image().to_string(),
            command: vec![
                "/bin/sh".to_string(),
                "-ec".to_string(),
                "chmod 0700 /run/cladding/run/fs-sandbox && umask 077 && exec gantry --backend direct --bind-uds /run/cladding/run/fs-sandbox/run.sock --policy-dir /opt/config/fs_sandbox"
                    .to_string(),
            ],
            workdir: Some("/home/user".to_string()),
            env: vec![
                RuntimeEnvVar {
                    name: "PATH".to_string(),
                    value: "/opt/tools/bin:/usr/local/sbin:/usr/local/bin:/usr/sbin:/usr/bin:/sbin:/bin"
                        .to_string(),
                },
            ],
            mounts,
            stdin: false,
            tty: false,
        },
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::{
        DEFAULT_PROXY_IMAGE, ExecutionComponentConfig, ExecutionConfig, ExecutionProxyConfig,
        ResolvedMountConfig,
    };
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
            fake_credential_env: std::collections::BTreeMap::new(),
            agent: ExecutionComponentConfig {
                enabled: true,
                init: true,
                image: "agent:image".to_string(),
                build: None,
                security_opts: Vec::new(),
            },
            nw_sandbox: nw_enabled.then(|| ExecutionComponentConfig {
                enabled: true,
                init: true,
                image: "nw:image".to_string(),
                build: None,
                security_opts: Vec::new(),
            }),
            fs_sandbox: fs_enabled.then(|| ExecutionComponentConfig {
                enabled: true,
                init: true,
                image: "fs:image".to_string(),
                build: None,
                security_opts: Vec::new(),
            }),
            proxy: None,
            mounts,
        }
    }

    fn container<'a>(component: &'a RuntimeComponent, name: &str) -> &'a RuntimeContainer {
        assert_eq!(component.container.name, name);
        &component.container
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

    fn host_mount_path<'a>(container: &'a RuntimeContainer, mount_path: &str) -> Option<&'a Path> {
        container
            .mounts
            .iter()
            .find(|mount| mount.mount_path == mount_path)
            .and_then(|mount| match &mount.source {
                super::super::types::RuntimeMountSource::HostPath { path } => Some(path.as_path()),
                _ => None,
            })
    }

    #[test]
    fn run_metadata_records_only_active_host_backed_mounts_for_each_target() {
        let mounts = vec![
            ResolvedMountConfig {
                mount_path: "/workspace".to_string(),
                host_path: Some(PathBuf::from("/tmp/../tmp/workspace")),
                volume: None,
                mount_type: MountType::Bind,
                tmpfs_size_bytes: None,
                read_only: true,
                targets: vec![MountTarget::Agent, MountTarget::NwSandbox],
                ignore: false,
            },
            ResolvedMountConfig {
                mount_path: "/workspace/src".to_string(),
                host_path: Some(PathBuf::from("/tmp/workspace/src")),
                volume: None,
                mount_type: MountType::Copy,
                tmpfs_size_bytes: None,
                read_only: false,
                targets: vec![MountTarget::Agent],
                ignore: false,
            },
            ResolvedMountConfig {
                mount_path: "/ignored".to_string(),
                host_path: Some(PathBuf::from("/tmp/ignored")),
                volume: None,
                mount_type: MountType::Bind,
                tmpfs_size_bytes: None,
                read_only: false,
                targets: vec![MountTarget::Agent],
                ignore: true,
            },
        ];
        let config = execution_config(true, false, mounts, false);
        let mut spec = RuntimeSpec::build(Path::new("/tmp/project/.cladding"), &config);
        spec.set_run_control_metadata(Path::new("/tmp/run/control.sock"), &config)
            .unwrap();

        let agent: serde_json::Value =
            serde_json::from_str(spec.agent.labels.get("cladding_run_cwd_v1").unwrap()).unwrap();
        let nw: serde_json::Value = serde_json::from_str(
            spec.nw_sandbox
                .as_ref()
                .unwrap()
                .labels
                .get("cladding_run_cwd_v1")
                .unwrap(),
        )
        .unwrap();
        assert_eq!(agent.as_array().unwrap().len(), 2);
        assert_eq!(agent[0]["mount_type"], "bind");
        assert_eq!(agent[1]["mount_type"], "copy");
        assert!(agent[1]["volume_name"].as_str().is_some());
        assert_eq!(nw.as_array().unwrap().len(), 1);
        assert_eq!(nw[0]["host_source"], "/tmp/workspace");
        assert_eq!(nw[0]["read_only"], true);
    }

    fn assert_socket_volume(
        container: &RuntimeContainer,
        mount_path: &str,
        name: &str,
        chown: bool,
    ) {
        let mount = container
            .mounts
            .iter()
            .find(|mount| mount.mount_path == mount_path)
            .expect("socket volume mount");
        match (&mount.source, chown) {
            (super::super::types::RuntimeMountSource::NamedVolumeChown { claim_name }, true) => {
                assert_eq!(claim_name, name);
            }
            (super::super::types::RuntimeMountSource::NamedVolume { claim_name }, false) => {
                assert_eq!(claim_name, name);
            }
            _ => panic!("unexpected socket mount source at {mount_path}: {mount:?}"),
        }
    }

    #[test]
    fn build_runtime_spec_fully_enabled_uses_standalone_containers() {
        let config = execution_config(true, true, Vec::new(), false);
        let spec = RuntimeSpec::build(Path::new("/tmp/project/.cladding"), &config);

        assert_eq!(spec.project_name, "demo");
        assert!(!spec.use_runsc);
        assert_eq!(spec.proxy.name, "demo-proxy");
        assert_eq!(spec.agent.container.name, "demo-agent-instance");
        assert_eq!(
            spec.proxy.labels.get("project_root").map(String::as_str),
            Some("/tmp/project/.cladding")
        );
        assert_eq!(spec.proxy.network_name, "default");
        assert_eq!(spec.agent.network_name, "none");
        assert_eq!(spec.proxy.container.name, "demo-proxy-instance");
        assert_eq!(
            spec.proxy.container.command,
            ["/bin/sh", "/opt/scripts/proxy_startup.sh"]
        );
        assert_eq!(
            env_value(&spec.proxy.container, "CLADDING_NW_SANDBOX_ENABLED"),
            Some("true")
        );
        assert_eq!(
            spec.proxy
                .container
                .env
                .iter()
                .map(|env| env.name.as_str())
                .collect::<Vec<_>>(),
            [
                "CLADDING_NW_SANDBOX_ENABLED",
                "CLADDING_AGENT_SESSION_CONFIG",
                "CLADDING_NW_SANDBOX_SESSION_CONFIG",
            ]
        );
        assert_eq!(
            spec.proxy.container.image,
            "localhost/cladding-proxy:latest"
        );
        assert_eq!(spec.agent.container.name, "demo-agent-instance");
        assert_eq!(
            spec.nw_sandbox
                .as_ref()
                .expect("network sandbox")
                .container
                .name,
            "demo-nw-sandbox-instance"
        );
        assert_eq!(
            spec.fs_sandbox
                .as_ref()
                .expect("filesystem sandbox")
                .container
                .name,
            "demo-fs-sandbox-instance"
        );

        let proxy = container(&spec.proxy, "demo-proxy-instance");
        assert!(mount_paths(proxy).contains("/opt/config"));
        assert!(mount_paths(proxy).contains("/opt/credentials/baffle"));
        assert!(mount_paths(proxy).contains("/opt/tools/bin/baffle"));
        assert!(mount_paths(proxy).contains("/opt/scripts/proxy_startup.sh"));
        assert!(mount_paths(proxy).contains("/run/cladding/proxy"));
        assert!(!mount_paths(proxy).contains("/run/baffle"));
        let baffle_binary = proxy
            .mounts
            .iter()
            .find(|mount| mount.mount_path == "/opt/tools/bin/baffle")
            .expect("Baffle executable mount");
        assert!(baffle_binary.read_only);
        let agent = container(&spec.agent, "demo-agent-instance");
        assert!(!mount_paths(agent).contains("/home/user/workspace"));
        assert_eq!(agent.workdir.as_deref(), Some("/home/user"));
        assert!(!mount_paths(agent).contains("/run/baffle"));
        let nw = container(
            spec.nw_sandbox.as_ref().expect("network sandbox"),
            "demo-nw-sandbox-instance",
        );
        assert!(!mount_paths(nw).contains("/home/user/workspace"));
        assert_eq!(nw.workdir.as_deref(), Some("/home/user"));
        assert!(!mount_paths(nw).contains("/run/baffle"));
        let fs = container(
            spec.fs_sandbox.as_ref().expect("filesystem sandbox"),
            "demo-fs-sandbox-instance",
        );
        assert!(!mount_paths(fs).contains("/home/user/workspace"));
        assert_eq!(fs.workdir.as_deref(), Some("/home/user"));
        assert!(!mount_paths(fs).contains("/run/baffle"));

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
        for (component, socket, policy_dir) in [
            (
                nw,
                "/run/cladding/run/nw-sandbox/run.sock",
                "/opt/config/nw_sandbox",
            ),
            (
                fs,
                "/run/cladding/run/fs-sandbox/run.sock",
                "/opt/config/fs_sandbox",
            ),
        ] {
            assert!(component.command.iter().any(|arg| arg.contains("gantry")));
            assert!(
                component
                    .command
                    .iter()
                    .any(|arg| arg.contains("--backend direct"))
            );
            assert!(component.command.iter().any(|arg| arg.contains(socket)));
            assert!(component.command.iter().any(|arg| arg.contains(policy_dir)));
        }
        assert_eq!(env_value(proxy, "CLADDING_SANDBOX_NAME"), None);
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
        assert_eq!(env_value(nw, "MCP_BIND_UDS"), None);
        assert_eq!(env_value(nw, "POLICY_DIR"), None);
        assert_eq!(env_value(nw, "MCP_BIND_ADDR"), None);
        assert_eq!(env_value(fs, "MCP_BIND_UDS"), None);
        assert_eq!(env_value(fs, "POLICY_DIR"), None);
        assert_eq!(env_value(fs, "MCP_BIND_ADDR"), None);

        assert!(mount_paths(agent).contains("/run/cladding/proxy"));
        assert!(!mount_paths(agent).contains("/run/cladding/agent/inject"));
        assert!(mount_paths(agent).contains("/run/cladding/run/nw-sandbox"));
        assert!(mount_paths(agent).contains("/run/cladding/run/fs-sandbox"));
        assert!(mount_paths(nw).contains("/run/cladding/proxy/nw-sandbox"));
        assert!(mount_paths(nw).contains("/run/cladding/run/nw-sandbox"));
        assert!(mount_paths(fs).contains("/run/cladding/run/fs-sandbox"));
        let proxy_credentials = proxy
            .mounts
            .iter()
            .find(|mount| mount.mount_path == "/opt/credentials/baffle")
            .expect("proxy credentials mount");
        assert!(proxy_credentials.read_only);
        assert!(matches!(
            &proxy_credentials.source,
            super::super::types::RuntimeMountSource::HostPath { path }
                if path == &PathBuf::from("/tmp/project/.cladding/credentials/baffle")
        ));
        for execution in [agent, nw] {
            let public_ca = execution
                .mounts
                .iter()
                .find(|mount| mount.mount_path == "/run/cladding/ca/baffle.crt")
                .expect("public CA mount");
            assert!(public_ca.read_only);
            assert!(matches!(
                &public_ca.source,
                super::super::types::RuntimeMountSource::HostPath { path }
                    if path == &PathBuf::from("/tmp/project/.cladding/credentials/baffle/ca.crt")
            ));
            assert!(execution.mounts.iter().all(|mount| {
                !matches!(
                    &mount.source,
                    super::super::types::RuntimeMountSource::HostPath { path }
                        if path.ends_with("credentials/baffle")
                            || path.ends_with("credentials/baffle/ca-key.pem")
                            || path.ends_with("credentials/baffle/secrets")
                )
            }));
        }
        assert!(!mount_paths(fs).contains("/run/cladding/ca/baffle.crt"));
        let fs_mounts = mount_paths(fs);
        assert!(fs_mounts.contains("/opt/config"));
        assert!(fs_mounts.contains("/opt/tools"));
        assert!(!fs_mounts.contains("/home/user"));
        assert!(!fs_mounts.contains("/home/user/workspace"));
        assert!(!fs_mounts.contains("/home/user/workspace/.cladding"));

        for container in [proxy, agent, nw, fs] {
            assert!(
                container
                    .command
                    .iter()
                    .all(|arg| !arg.contains("/run/cladding/sockets"))
            );
        }
        for container in [proxy, agent, nw, fs] {
            assert!(container.name != "demo-agent-proxy-client");
            assert!(container.name != "demo-agent-nw-sandbox-run-client");
            assert!(container.name != "demo-agent-fs-sandbox-run-client");
            assert!(container.name != "demo-nw-sandbox-proxy-client");
            assert!(container.name != "demo-nw-sandbox-run-server");
            assert!(container.name != "demo-fs-sandbox-run-server");
        }
        assert_eq!(
            spec.fs_sandbox
                .as_ref()
                .expect("filesystem sandbox")
                .container
                .image,
            "fs:image"
        );
        assert_eq!(
            spec.required_host_paths()
                .into_iter()
                .collect::<BTreeSet<_>>(),
            [
                PathBuf::from("/tmp/project/.cladding/config"),
                PathBuf::from("/tmp/project/.cladding/credentials/baffle"),
                PathBuf::from("/tmp/project/.cladding/credentials/baffle/ca.crt"),
                PathBuf::from("/tmp/project/.cladding/home"),
                PathBuf::from("/tmp/project/.cladding/tools"),
                PathBuf::from("/tmp/project/.cladding/tools/bin/baffle"),
            ]
            .into_iter()
            .collect()
        );
    }

    #[test]
    fn build_does_not_add_an_implicit_workspace_mount() {
        let config = execution_config(true, false, Vec::new(), false);
        let spec = RuntimeSpec::build(Path::new("/tmp/other/.cladding"), &config);
        let agent = container(&spec.agent, "demo-agent-instance");
        assert!(!mount_paths(agent).contains("/home/user/workspace"));
        assert_eq!(agent.workdir.as_deref(), Some("/home/user"));

        let nw = container(
            spec.nw_sandbox.as_ref().expect("nw sandbox"),
            "demo-nw-sandbox-instance",
        );
        assert!(!mount_paths(nw).contains("/home/user/workspace"));
        assert_eq!(nw.workdir.as_deref(), Some("/home/user"));

        let config_mount = agent
            .mounts
            .iter()
            .find(|mount| mount.mount_path == "/opt/config")
            .expect("configuration mount");
        assert!(matches!(
            &config_mount.source,
            super::super::types::RuntimeMountSource::HostPath { path }
                if path == &PathBuf::from("/tmp/other/.cladding/config")
        ));
    }

    #[test]
    fn build_runtime_spec_only_includes_enabled_components() {
        let config = execution_config(true, false, Vec::new(), false);
        let spec = RuntimeSpec::build(Path::new("/tmp/project/.cladding"), &config);
        let proxy = container(&spec.proxy, "demo-proxy-instance");
        let agent = container(&spec.agent, "demo-agent-instance");

        assert!(spec.nw_sandbox.is_some());
        assert!(spec.fs_sandbox.is_none());
        assert_eq!(spec.proxy.container.name, "demo-proxy-instance");
        assert_eq!(spec.agent.container.name, "demo-agent-instance");
        assert_eq!(
            spec.nw_sandbox.as_ref().unwrap().container.name,
            "demo-nw-sandbox-instance"
        );
        assert_eq!(env_value(proxy, "CLADDING_SANDBOX_NAME"), None);
        assert_eq!(
            env_value(agent, "RUN_NW_SANDBOX_SOCKET"),
            Some("/run/cladding/run/nw-sandbox/run.sock")
        );
        assert_eq!(env_value(agent, "RUN_FS_SANDBOX_SOCKET"), None);
        assert_eq!(env_value(agent, "RUN_NW_SANDBOX_SERVER"), None);
        assert_eq!(env_value(agent, "RUN_FS_SANDBOX_SERVER"), None);
    }

    #[test]
    fn build_runtime_spec_uses_selected_proxy_image_without_a_bridge_sidecar() {
        let mut config = execution_config(false, false, Vec::new(), false);
        config.proxy = Some(ExecutionProxyConfig {
            image: "localhost/custom-proxy:latest".to_string(),
            build: None,
            agent_session_config: "agent.toml".to_string(),
            nw_sandbox_session_config: "nw-sandbox.toml".to_string(),
        });

        let spec = RuntimeSpec::build(Path::new("/tmp/project/.cladding"), &config);

        assert_eq!(spec.proxy.container.image, "localhost/custom-proxy:latest");
    }

    #[test]
    fn proxy_container_receives_independent_session_config_paths() {
        let mut config = execution_config(true, false, Vec::new(), false);
        config.proxy = Some(ExecutionProxyConfig {
            image: DEFAULT_PROXY_IMAGE.to_string(),
            build: None,
            agent_session_config: "agent/restricted.toml".to_string(),
            nw_sandbox_session_config: "nw/limited.toml".to_string(),
        });

        let spec = RuntimeSpec::build(Path::new("/tmp/project/.cladding"), &config);
        let proxy = container(&spec.proxy, "demo-proxy-instance");

        assert_eq!(
            env_value(proxy, "CLADDING_AGENT_SESSION_CONFIG"),
            Some("agent/restricted.toml")
        );
        assert_eq!(
            env_value(proxy, "CLADDING_NW_SANDBOX_SESSION_CONFIG"),
            Some("nw/limited.toml")
        );
    }

    #[test]
    fn proxy_only_mounts_the_agent_socket_when_network_sandbox_is_disabled() {
        let config = execution_config(false, false, Vec::new(), false);
        let spec = RuntimeSpec::build(Path::new("/tmp/project/.cladding"), &config);
        let proxy = container(&spec.proxy, "demo-proxy-instance");
        let agent = container(&spec.agent, "demo-agent-instance");

        assert_eq!(
            env_value(proxy, "CLADDING_NW_SANDBOX_ENABLED"),
            Some("false")
        );
        assert!(mount_paths(proxy).contains("/run/cladding/proxy"));
        assert!(mount_paths(agent).contains("/run/cladding/proxy"));
        assert!(spec.nw_sandbox.is_none());
    }

    #[test]
    fn managed_socket_volumes_are_mounted_only_into_their_participants() {
        let config = execution_config(true, true, Vec::new(), false);
        let spec = RuntimeSpec::build(Path::new("/tmp/project/.cladding"), &config);
        let proxy = container(&spec.proxy, "demo-proxy-instance");
        let agent = container(&spec.agent, "demo-agent-instance");
        let nw = container(
            spec.nw_sandbox.as_ref().expect("network sandbox"),
            "demo-nw-sandbox-instance",
        );
        let fs = container(
            spec.fs_sandbox.as_ref().expect("filesystem sandbox"),
            "demo-fs-sandbox-instance",
        );

        assert_eq!(spec.proxy.container.name, "demo-proxy-instance");
        assert!(mount_paths(proxy).contains("/run/cladding/proxy"));
        assert_eq!(host_mount_path(proxy, "/run/cladding/proxy"), None);
        assert_socket_volume(
            proxy,
            "/run/cladding/proxy",
            "cladding-demo-socket-baffle-agent",
            true,
        );
        assert_socket_volume(
            proxy,
            "/run/cladding/proxy/nw-sandbox",
            "cladding-demo-socket-baffle-nw-sandbox",
            true,
        );
        assert_socket_volume(
            agent,
            "/run/cladding/proxy",
            "cladding-demo-socket-baffle-agent",
            false,
        );
        assert_socket_volume(
            nw,
            "/run/cladding/proxy/nw-sandbox",
            "cladding-demo-socket-baffle-nw-sandbox",
            false,
        );
        assert_socket_volume(
            agent,
            "/run/cladding/run/nw-sandbox",
            "cladding-demo-socket-run-nw-sandbox",
            false,
        );
        assert_socket_volume(
            nw,
            "/run/cladding/run/nw-sandbox",
            "cladding-demo-socket-run-nw-sandbox",
            true,
        );
        assert_socket_volume(
            agent,
            "/run/cladding/run/fs-sandbox",
            "cladding-demo-socket-run-fs-sandbox",
            false,
        );
        assert_socket_volume(
            fs,
            "/run/cladding/run/fs-sandbox",
            "cladding-demo-socket-run-fs-sandbox",
            true,
        );

        assert_eq!(host_mount_path(agent, "/run/cladding/agent/inject"), None);
        assert!(mount_paths(agent).contains("/run/cladding/proxy"));
        assert!(!mount_paths(agent).contains("/run/cladding/proxy/nw-sandbox"));
        assert!(mount_paths(nw).contains("/run/cladding/proxy/nw-sandbox"));
        assert!(!mount_paths(nw).contains("/run/cladding/proxy"));
        for (component, container) in [
            (&spec.agent, agent),
            (spec.nw_sandbox.as_ref().expect("network sandbox"), nw),
        ] {
            assert_eq!(component.network_name, "none");
            assert!(
                container
                    .command
                    .iter()
                    .any(|arg| arg.contains("TCP-LISTEN:3128"))
            );
            assert!(
                container
                    .command
                    .iter()
                    .any(|arg| arg.contains("UNIX-CONNECT:"))
            );
            assert_eq!(
                env_value(container, "http_proxy"),
                Some("http://127.0.0.1:3128")
            );
            assert_eq!(
                env_value(container, "https_proxy"),
                Some("http://127.0.0.1:3128")
            );
        }

        assert!(
            !mount_paths(fs)
                .iter()
                .any(|path| path.starts_with("/run/cladding/proxy"))
        );
        assert!(!mount_paths(fs).contains("/run/cladding/run/nw-sandbox"));
        assert!(!mount_paths(nw).contains("/run/cladding/run/fs-sandbox"));
        for execution in [agent, nw, fs] {
            assert!(execution.mounts.iter().all(|mount| {
                !matches!(
                    &mount.source,
                    super::super::types::RuntimeMountSource::HostPath { path }
                        if path.ends_with("credentials/baffle")
                            || path.ends_with("credentials/baffle/ca-key.pem")
                            || path.ends_with("credentials/baffle/secrets")
                )
            }));
        }
    }

    #[test]
    fn runtime_spec_threads_use_runsc_flag() {
        let config = execution_config(false, false, Vec::new(), true);
        let spec = RuntimeSpec::build(Path::new("/tmp/project/.cladding"), &config);
        let proxy = container(&spec.proxy, "demo-proxy-instance");

        assert!(spec.use_runsc);
        assert!(!spec.proxy.use_runsc);
        assert_eq!(env_value(proxy, "CLADDING_SANDBOX_NAME"), None);
        assert!(spec.agent.use_runsc);
    }

    #[test]
    fn execution_components_are_standalone_containers() {
        let config = execution_config(true, true, Vec::new(), true);
        let spec = RuntimeSpec::build(Path::new("/tmp/project/.cladding"), &config);

        assert!(!spec.proxy.use_runsc);
        assert!(spec.agent.use_runsc);
        assert!(spec.nw_sandbox.as_ref().expect("network sandbox").use_runsc);
        assert!(
            spec.fs_sandbox
                .as_ref()
                .expect("filesystem sandbox")
                .use_runsc
        );
    }
}
