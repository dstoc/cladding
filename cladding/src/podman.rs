use crate::assets::containerfile;
use crate::error::{Error, Result};
use crate::runtime::{
    RuntimeContainer, RuntimeMount, RuntimeMountSource, RuntimePod, RuntimeSpec, RuntimeTask,
};
use anyhow::Context as _;
use serde_json::Value;
use std::collections::HashMap;
use std::env;
use std::ffi::OsStr;
use std::fs;
use std::io::ErrorKind;
use std::path::Path;
use std::process::{Command, ExitStatus, Output, Stdio};

pub fn podman_required(message: &str) -> Result<()> {
    if command_exists("podman") {
        Ok(())
    } else {
        eprintln!("missing: {message}");
        Err(Error::message("missing podman"))
    }
}

pub fn podman_build_image(image: &str, host_uid: u32, host_gid: u32) -> Result<()> {
    let mut cmd = Command::new("podman");
    cmd.args([
        "build",
        "--build-arg",
        &format!("UID={host_uid}"),
        "--build-arg",
        &format!("GID={host_gid}"),
        "-t",
        image,
        "-f",
        "-",
        ".",
    ])
    .stdin(Stdio::piped());

    let mut child = cmd.spawn().with_context(|| "failed to run podman build")?;

    if let Some(mut stdin) = child.stdin.take() {
        use std::io::Write;
        stdin
            .write_all(containerfile().as_bytes())
            .and_then(|_| stdin.flush())
            .with_context(|| "failed to write Containerfile to podman")?;
    }

    let status = child
        .wait()
        .with_context(|| "failed to wait on podman build")?;

    ensure_success(status, "podman build")
}

pub fn trace_command(cmd: &Command, verbose: bool) {
    if verbose {
        eprintln!("+ {}", format_command(cmd));
    }
}

fn format_command(cmd: &Command) -> String {
    std::iter::once(cmd.get_program())
        .chain(cmd.get_args())
        .map(shell_quote)
        .collect::<Vec<_>>()
        .join(" ")
}

fn shell_quote(value: &OsStr) -> String {
    let value = value.to_string_lossy();
    if value.is_empty() {
        return "''".to_string();
    }
    if value.chars().all(|ch| {
        ch.is_ascii_alphanumeric() || matches!(ch, '/' | '.' | '_' | '-' | ':' | '=' | ',')
    }) {
        return value.into_owned();
    }

    format!("'{}'", value.replace('\'', r#"'\''"#))
}

/// Direct runtime helpers for creating and cleaning up pods.
pub fn runtime_create(spec: &RuntimeSpec, verbose: bool) -> Result<()> {
    prepare_runtime_socket_dirs(spec)?;
    ensure_runtime_empty_mask_dir(spec)?;

    for pod in runtime_pods(spec) {
        pod_create(pod, verbose)?;
    }

    for pod in runtime_pods(spec) {
        for task in &pod.init_tasks {
            run_pod_task(pod, task, verbose)?;
        }
    }

    for pod in runtime_pods(spec) {
        for container in &pod.containers {
            container_create(&pod.name, container, verbose)?;
        }
    }

    for pod in runtime_pods(spec) {
        for container in &pod.containers {
            container_start(&container.name, verbose)?;
        }
    }

    Ok(())
}

pub fn runtime_cleanup(spec: &RuntimeSpec, verbose: bool) -> Result<()> {
    for pod in runtime_pods(spec) {
        for container in &pod.containers {
            container_rm(&container.name, verbose)?;
        }
        pod_rm(&pod.name, verbose)?;
    }

    Ok(())
}

pub fn pod_create(pod: &RuntimePod, verbose: bool) -> Result<()> {
    let mut cmd = build_pod_create_command(pod);
    trace_command(&cmd, verbose);
    let status = cmd
        .status()
        .with_context(|| format!("failed to run podman pod create for {}", pod.name))?;
    ensure_success(status, "podman pod create")
}

pub fn pod_rm(pod_name: &str, verbose: bool) -> Result<()> {
    let mut cmd = build_pod_rm_command(pod_name);
    trace_command(&cmd, verbose);
    let output = cmd
        .output()
        .with_context(|| format!("failed to run podman pod rm for {pod_name}"))?;

    if output.status.success() || remove_output_is_missing_pod(&output) {
        return Ok(());
    }

    ensure_success_output(&output, "podman pod rm")
}

pub fn container_create(pod_name: &str, container: &RuntimeContainer, verbose: bool) -> Result<()> {
    let mut cmd = build_container_create_command(pod_name, container);
    trace_command(&cmd, verbose);
    let status = cmd.status().with_context(|| {
        format!(
            "failed to run podman container create for {} in pod {pod_name}",
            container.name
        )
    })?;
    ensure_success(status, "podman container create")
}

pub fn container_start(container_name: &str, verbose: bool) -> Result<()> {
    let mut cmd = build_container_start_command(container_name);
    trace_command(&cmd, verbose);
    let status = cmd
        .status()
        .with_context(|| format!("failed to run podman container start for {container_name}"))?;
    ensure_success(status, "podman container start")
}

pub fn container_wait(container_name: &str) -> Result<i32> {
    let output = build_container_wait_command(container_name)
        .output()
        .with_context(|| format!("failed to run podman container wait for {container_name}"))?;

    if !output.status.success() {
        return ensure_success_output(&output, "podman container wait").map(|_| 0);
    }

    let stdout = String::from_utf8_lossy(&output.stdout);
    let code = stdout
        .trim()
        .parse::<i32>()
        .with_context(|| format!("failed to parse podman wait output for {container_name}"))?;
    Ok(code)
}

pub fn container_rm(container_name: &str, verbose: bool) -> Result<()> {
    let mut cmd = build_container_rm_command(container_name);
    trace_command(&cmd, verbose);
    let output = cmd
        .output()
        .with_context(|| format!("failed to run podman rm for {container_name}"))?;

    if output.status.success() || remove_output_is_missing_container(&output) {
        return Ok(());
    }

    ensure_success_output(&output, "podman rm")
}

pub fn ensure_success(status: ExitStatus, context: &'static str) -> Result<()> {
    if status.success() {
        return Ok(());
    }

    let code = status.code().unwrap_or(1);
    eprintln!("error: {context} failed (exit code {code})");
    Err(Error::CommandFailed { context, code })
}

pub fn ensure_success_output(output: &Output, context: &'static str) -> Result<()> {
    if output.status.success() {
        return Ok(());
    }

    let code = output.status.code().unwrap_or(1);
    eprintln!("error: {context} failed (exit code {code})");
    let stderr = String::from_utf8_lossy(&output.stderr);
    if !stderr.trim().is_empty() {
        eprintln!("{stderr}");
    }
    Err(Error::CommandFailed { context, code })
}

fn command_exists(command: &str) -> bool {
    env::var_os("PATH").is_some_and(|paths| {
        env::split_paths(&paths).any(|path| {
            let candidate = path.join(command);
            candidate.is_file()
        })
    })
}

#[derive(Debug, Clone)]
pub struct RunningProject {
    pub name: String,
    pub project_root: String,
    pub pod_count: usize,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ExposeProxy {
    pub id: String,
    pub name: String,
    pub host_port: u16,
    pub container_port: u16,
    pub status: String,
    pub role: String,
}

pub fn list_running_projects(verbose: bool) -> Result<Vec<RunningProject>> {
    let items = list_running_pod_items(verbose)?;
    let mut projects: HashMap<(String, String), usize> = HashMap::new();
    for item in items {
        let key = (item.name, item.project_root);
        let count = projects.entry(key).or_insert(0);
        *count += 1;
    }

    let mut results: Vec<RunningProject> = projects
        .into_iter()
        .map(|((name, project_root), pod_count)| RunningProject {
            name,
            project_root,
            pod_count,
        })
        .collect();

    results.sort_by(|a, b| {
        a.name
            .cmp(&b.name)
            .then_with(|| a.project_root.cmp(&b.project_root))
    });

    Ok(results)
}

pub fn list_project_expose_proxies(
    project_name: &str,
    project_root: &str,
    include_stopped: bool,
    verbose: bool,
) -> Result<Vec<ExposeProxy>> {
    list_project_expose_entries(
        project_name,
        project_root,
        include_stopped,
        Some("host-helper"),
        verbose,
    )
}

pub fn list_project_expose_containers(
    project_name: &str,
    project_root: &str,
    include_stopped: bool,
    verbose: bool,
) -> Result<Vec<ExposeProxy>> {
    list_project_expose_entries(project_name, project_root, include_stopped, None, verbose)
}

pub fn podman_container_exists(container_name: &str) -> Result<bool> {
    let status = Command::new("podman")
        .args(["container", "exists", container_name])
        .status()
        .with_context(|| "failed to run podman container exists")?;

    match status.code() {
        Some(0) => Ok(true),
        Some(1) => Ok(false),
        _ => {
            eprintln!("error: failed to check whether container exists: {container_name}");
            Err(Error::message("podman container exists failed"))
        }
    }
}

pub fn podman_remove_containers(
    container_ids: &[String],
    force: bool,
    ignore_missing: bool,
    verbose: bool,
) -> Result<()> {
    for container_id in container_ids {
        let mut cmd = Command::new("podman");
        cmd.arg("rm");
        if force {
            cmd.arg("-f");
        }
        cmd.arg(container_id);

        trace_command(&cmd, verbose);
        let output = cmd.output().with_context(|| "failed to run podman rm")?;

        if output.status.success() {
            continue;
        }

        if ignore_missing && remove_output_is_missing_container(&output) {
            continue;
        }

        return ensure_success_output(&output, "podman rm");
    }

    Ok(())
}

#[derive(Debug, Clone)]
struct RunningPodItem {
    name: String,
    project_root: String,
}

#[derive(Debug, Clone)]
struct ExposeProxyItem {
    proxy: ExposeProxy,
    project_root: String,
    target: String,
    role: String,
}

fn list_running_pod_items(verbose: bool) -> Result<Vec<RunningPodItem>> {
    let mut cmd = Command::new("podman");
    cmd.args([
        "pod",
        "ps",
        "--filter",
        "label=cladding",
        "--filter",
        "status=running",
        "--format",
        "json",
    ]);
    trace_command(&cmd, verbose);
    let output = cmd
        .output()
        .with_context(|| "failed to run podman pod ps")?;

    if !output.status.success() {
        return ensure_success_output(&output, "podman pod ps").map(|_| Vec::new());
    }

    let stdout = String::from_utf8_lossy(&output.stdout);
    let parsed: Value = serde_json::from_str(&stdout)
        .with_context(|| "failed to parse podman pod ps json output")?;
    let Some(items) = parsed.as_array() else {
        return Ok(Vec::new());
    };

    let mut pods = Vec::new();
    for item in items {
        let Some(labels_value) = item.get("Labels") else {
            continue;
        };
        let labels = parse_labels(labels_value);
        let Some(name) = labels.get("cladding") else {
            continue;
        };
        let Some(project_root) = labels.get("project_root") else {
            continue;
        };
        pods.push(RunningPodItem {
            name: name.to_string(),
            project_root: project_root.to_string(),
        });
    }

    Ok(pods)
}

fn list_expose_proxy_items(
    project_name: &str,
    include_stopped: bool,
    verbose: bool,
) -> Result<Vec<ExposeProxyItem>> {
    let mut cmd = Command::new("podman");
    cmd.arg("ps");
    if include_stopped {
        cmd.arg("-a");
    }
    cmd.args([
        "--filter",
        "label=cladding_expose=true",
        "--filter",
        &format!("label=cladding={project_name}"),
        "--format",
        "json",
    ]);

    trace_command(&cmd, verbose);
    let output = cmd
        .output()
        .with_context(|| "failed to run podman ps for expose proxies")?;

    if !output.status.success() {
        return ensure_success_output(&output, "podman ps").map(|_| Vec::new());
    }

    let stdout = String::from_utf8_lossy(&output.stdout);
    let parsed: Value =
        serde_json::from_str(&stdout).with_context(|| "failed to parse podman ps json output")?;

    Ok(parse_expose_proxy_items(&parsed))
}

fn list_project_expose_entries(
    project_name: &str,
    project_root: &str,
    include_stopped: bool,
    role_filter: Option<&str>,
    verbose: bool,
) -> Result<Vec<ExposeProxy>> {
    let items = list_expose_proxy_items(project_name, include_stopped, verbose)?;
    let mut results = project_expose_proxies_from_items(items, project_root, role_filter);

    results.sort_by(|a, b| {
        a.host_port
            .cmp(&b.host_port)
            .then_with(|| a.container_port.cmp(&b.container_port))
            .then_with(|| a.name.cmp(&b.name))
    });

    Ok(results)
}

fn project_expose_proxies_from_items(
    items: Vec<ExposeProxyItem>,
    project_root: &str,
    role_filter: Option<&str>,
) -> Vec<ExposeProxy> {
    let mut results = Vec::new();

    for item in items {
        if item.project_root != project_root {
            continue;
        }
        if item.target != "agent" {
            continue;
        }
        if role_filter.is_some_and(|role| item.role != role) {
            continue;
        }
        results.push(item.proxy);
    }

    results
}

fn parse_labels(value: &Value) -> HashMap<String, String> {
    let mut labels = HashMap::new();
    match value {
        Value::Object(map) => {
            for (key, val) in map {
                if let Some(s) = val.as_str() {
                    labels.insert(key.clone(), s.to_string());
                }
            }
        }
        Value::String(raw) => {
            for entry in raw.split(',') {
                let entry = entry.trim();
                if entry.is_empty() {
                    continue;
                }
                let mut parts = entry.splitn(2, '=');
                let key = parts.next().unwrap_or("").trim();
                let val = parts.next().unwrap_or("").trim();
                if !key.is_empty() && !val.is_empty() {
                    labels.insert(key.to_string(), val.to_string());
                }
            }
        }
        _ => {}
    }
    labels
}

fn parse_expose_proxy_items(value: &Value) -> Vec<ExposeProxyItem> {
    let Some(items) = value.as_array() else {
        return Vec::new();
    };

    let mut proxies = Vec::new();
    for item in items {
        let Some(proxy) = parse_expose_proxy_item(item) else {
            continue;
        };
        proxies.push(proxy);
    }
    proxies
}

fn parse_expose_proxy_item(value: &Value) -> Option<ExposeProxyItem> {
    let labels = value.get("Labels").map(parse_labels).unwrap_or_default();
    if labels.get("cladding_expose").map(String::as_str) != Some("true") {
        return None;
    }

    let project_root = labels.get("project_root")?.to_string();
    let target = labels.get("cladding_expose_target")?.to_string();
    let role = labels
        .get("cladding_expose_role")
        .cloned()
        .unwrap_or_else(|| "host-helper".to_string());
    let container_port = labels
        .get("cladding_expose_container_port")?
        .parse::<u16>()
        .ok()?;
    let host_port = labels
        .get("cladding_expose_host_port")?
        .parse::<u16>()
        .ok()?;

    let id = get_json_string(value, &["Id", "ID"])?;
    let name = get_json_name(value)?;
    let status =
        get_json_string(value, &["Status"]).or_else(|| get_json_string(value, &["State"]))?;

    Some(ExposeProxyItem {
        proxy: ExposeProxy {
            id,
            name,
            host_port,
            container_port,
            status,
            role: role.clone(),
        },
        project_root,
        target,
        role,
    })
}

fn get_json_string(value: &Value, keys: &[&str]) -> Option<String> {
    for key in keys {
        let Some(raw) = value.get(*key) else {
            continue;
        };
        if let Some(string) = raw.as_str().filter(|s| !s.is_empty()) {
            return Some(string.to_string());
        }
    }
    None
}

fn get_json_name(value: &Value) -> Option<String> {
    for key in ["Names", "Name"] {
        let Some(raw) = value.get(key) else {
            continue;
        };
        match raw {
            Value::String(name) if !name.is_empty() => return Some(name.to_string()),
            Value::Array(items) => {
                for item in items {
                    if let Some(name) = item.as_str().filter(|s| !s.is_empty()) {
                        return Some(name.to_string());
                    }
                }
            }
            _ => {}
        }
    }
    None
}

fn remove_output_is_missing_container(output: &Output) -> bool {
    let stderr = String::from_utf8_lossy(&output.stderr).to_ascii_lowercase();
    stderr.contains("no such container") || stderr.contains("no container with name or id")
}

fn remove_output_is_missing_pod(output: &Output) -> bool {
    let stderr = String::from_utf8_lossy(&output.stderr).to_ascii_lowercase();
    stderr.contains("no such pod") || stderr.contains("no pod with name or id")
}

fn runtime_pods(spec: &RuntimeSpec) -> Vec<&RuntimePod> {
    let mut pods = vec![&spec.proxy, &spec.agent];
    if let Some(pod) = &spec.nw_sandbox {
        pods.push(pod);
    }
    if let Some(pod) = &spec.fs_sandbox {
        pods.push(pod);
    }
    pods
}

fn prepare_runtime_socket_dirs(spec: &RuntimeSpec) -> Result<()> {
    let mut socket_dirs = spec.generated_runtime_socket_dirs().into_iter();
    let Some(root_socket_dir) = socket_dirs.next() else {
        return Ok(());
    };

    clear_path(&root_socket_dir)?;
    fs::create_dir_all(&root_socket_dir).with_context(|| {
        format!(
            "failed to create runtime socket directory {}",
            root_socket_dir.display()
        )
    })?;
    set_restrictive_dir_permissions(&root_socket_dir)?;

    for socket_dir in socket_dirs {
        fs::create_dir_all(&socket_dir).with_context(|| {
            format!(
                "failed to create runtime socket directory {}",
                socket_dir.display()
            )
        })?;
    }
    Ok(())
}

fn clear_path(path: &Path) -> Result<()> {
    match fs::symlink_metadata(path) {
        Ok(metadata) => {
            if metadata.is_dir() {
                fs::remove_dir_all(path)
                    .with_context(|| format!("failed to remove directory {}", path.display()))?;
            } else {
                fs::remove_file(path)
                    .with_context(|| format!("failed to remove file {}", path.display()))?;
            }
        }
        Err(err) if err.kind() == ErrorKind::NotFound => {}
        Err(err) => {
            return Err(anyhow::Error::new(err).into());
        }
    }
    Ok(())
}

#[cfg(unix)]
fn set_restrictive_dir_permissions(path: &Path) -> Result<()> {
    use std::os::unix::fs::PermissionsExt;

    let mut permissions = fs::metadata(path)
        .with_context(|| format!("failed to read permissions for {}", path.display()))?
        .permissions();
    permissions.set_mode(0o700);
    fs::set_permissions(path, permissions).with_context(|| {
        format!(
            "failed to set restrictive permissions on {}",
            path.display()
        )
    })?;
    Ok(())
}

#[cfg(not(unix))]
fn set_restrictive_dir_permissions(_path: &Path) -> Result<()> {
    Ok(())
}

fn ensure_runtime_empty_mask_dir(spec: &RuntimeSpec) -> Result<()> {
    for empty_mask in generated_empty_mask_dirs(spec) {
        fs::create_dir_all(&empty_mask).with_context(|| {
            format!("failed to create runtime mask dir {}", empty_mask.display())
        })?;
    }
    Ok(())
}

fn run_pod_task(pod: &RuntimePod, task: &RuntimeTask, verbose: bool) -> Result<()> {
    let mut cmd = Command::new("podman");
    cmd.arg("run");
    cmd.arg("--rm");
    cmd.arg("--pod");
    cmd.arg(&pod.name);
    cmd.arg("--name");
    cmd.arg(format!("{}-{}", pod.name, task.name));

    if let Some(user) = task.run_as_user {
        let group = task.run_as_group.unwrap_or(user);
        cmd.arg("--user");
        cmd.arg(format!("{user}:{group}"));
    }

    for capability in &task.added_capabilities {
        cmd.arg("--cap-add");
        cmd.arg(capability);
    }

    append_env_args(&mut cmd, &task.env);
    append_mount_args(&mut cmd, &pod.name, &task.mounts);
    append_entrypoint_arg(&mut cmd, &task.command);
    cmd.arg(&task.image);
    append_command_args(&mut cmd, &task.command);

    trace_command(&cmd, verbose);
    let status = cmd.status().with_context(|| {
        format!(
            "failed to run podman task {} in pod {}",
            task.name, pod.name
        )
    })?;
    ensure_success(status, "podman run")
}

fn build_pod_create_command(pod: &RuntimePod) -> Command {
    let mut cmd = Command::new("podman");
    cmd.args(["pod", "create", "--name", &pod.name]);
    append_label_args(&mut cmd, &pod.labels);
    cmd.arg("--network");
    cmd.arg(&pod.network_name);
    if !pod.ip.is_empty() {
        cmd.arg("--ip");
        cmd.arg(&pod.ip);
    }
    if pod.userns_keep_id {
        cmd.arg("--userns");
        cmd.arg("keep-id");
    }
    append_host_alias_args(&mut cmd, &pod.host_aliases);
    cmd
}

fn build_pod_rm_command(pod_name: &str) -> Command {
    let mut cmd = Command::new("podman");
    cmd.args(["pod", "rm", "-f", pod_name]);
    cmd
}

fn build_container_create_command(pod_name: &str, container: &RuntimeContainer) -> Command {
    let mut cmd = Command::new("podman");
    cmd.arg("create");
    cmd.arg("--pod");
    cmd.arg(pod_name);
    cmd.arg("--name");
    cmd.arg(&container.name);
    if let Some(workdir) = &container.workdir {
        cmd.arg("--workdir");
        cmd.arg(workdir);
    }
    if container.stdin {
        cmd.arg("-i");
    }
    if container.tty {
        cmd.arg("-t");
    }

    append_env_args(&mut cmd, &container.env);
    append_mount_args(&mut cmd, pod_name, &container.mounts);
    append_port_args(&mut cmd, &container.ports);
    append_entrypoint_arg(&mut cmd, &container.command);

    cmd.arg(&container.image);
    append_command_args(&mut cmd, &container.command);
    cmd
}

fn build_container_start_command(container_name: &str) -> Command {
    let mut cmd = Command::new("podman");
    cmd.args(["start", container_name]);
    cmd
}

fn build_container_wait_command(container_name: &str) -> Command {
    let mut cmd = Command::new("podman");
    cmd.args(["wait", container_name]);
    cmd
}

fn build_container_rm_command(container_name: &str) -> Command {
    let mut cmd = Command::new("podman");
    cmd.args(["rm", "-f", container_name]);
    cmd
}

fn append_label_args(cmd: &mut Command, labels: &std::collections::BTreeMap<String, String>) {
    for (key, value) in labels {
        cmd.arg("--label");
        cmd.arg(format!("{key}={value}"));
    }
}

fn append_host_alias_args(cmd: &mut Command, host_aliases: &[crate::runtime::RuntimeHostAlias]) {
    for alias in host_aliases {
        for hostname in &alias.hostnames {
            cmd.arg("--add-host");
            cmd.arg(format!("{hostname}:{}", alias.ip));
        }
    }
}

fn append_env_args(cmd: &mut Command, env: &[crate::runtime::RuntimeEnvVar]) {
    for var in env {
        cmd.arg("--env");
        cmd.arg(format!("{}={}", var.name, var.value));
    }
}

fn append_port_args(cmd: &mut Command, ports: &[u16]) {
    for port in ports {
        cmd.arg("--expose");
        cmd.arg(port.to_string());
    }
}

fn append_mount_args(cmd: &mut Command, pod_name: &str, mounts: &[RuntimeMount]) {
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

fn append_command_args(cmd: &mut Command, command: &[String]) {
    for arg in command.iter().skip(1) {
        cmd.arg(arg);
    }
}

fn append_entrypoint_arg(cmd: &mut Command, command: &[String]) {
    if let Some(entrypoint) = command.first() {
        cmd.arg("--entrypoint");
        cmd.arg(entrypoint);
    }
}

fn empty_dir_volume_name(pod_name: &str, mount_path: &str) -> String {
    format!(
        "cladding-{pod_name}-empty-{}",
        sanitize_volume_fragment(mount_path)
    )
}

fn generated_empty_mask_dirs(spec: &RuntimeSpec) -> Vec<std::path::PathBuf> {
    let mut paths = std::collections::BTreeSet::new();

    for pod in runtime_pods(spec) {
        for task in &pod.init_tasks {
            for mount in &task.mounts {
                if let RuntimeMountSource::GeneratedEmptyMask { path } = &mount.source {
                    paths.insert(path.clone());
                }
            }
        }
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
    use crate::runtime::{
        RuntimeContainer, RuntimeEnvVar, RuntimeHostAlias, RuntimeMount, RuntimeMountSource,
        RuntimePod,
    };
    use serde_json::json;

    fn command_args(cmd: &Command) -> Vec<String> {
        cmd.get_args()
            .map(|arg| arg.to_string_lossy().into_owned())
            .collect()
    }

    #[test]
    fn parse_labels_supports_string_and_object_forms() {
        let string_labels = parse_labels(&Value::String(
            "cladding=demo, project_root=/tmp/demo, cladding_expose=true".into(),
        ));
        assert_eq!(
            string_labels.get("cladding").map(String::as_str),
            Some("demo")
        );
        assert_eq!(
            string_labels.get("project_root").map(String::as_str),
            Some("/tmp/demo")
        );

        let object_labels = parse_labels(&json!({
            "cladding": "demo",
            "project_root": "/tmp/demo",
            "cladding_expose": "true"
        }));
        assert_eq!(
            object_labels.get("cladding_expose").map(String::as_str),
            Some("true")
        );
    }

    #[test]
    fn parse_expose_proxy_items_filters_and_extracts_expected_fields() {
        let parsed = json!([
            {
                "Id": "abc123",
                "Names": ["demo-expose-3000-9000"],
                "Status": "Up 3 seconds",
                "Labels": {
                    "cladding": "demo",
                    "project_root": "/tmp/demo/.cladding",
                    "cladding_expose": "true",
                    "cladding_expose_target": "agent",
                    "cladding_expose_role": "host-helper",
                    "cladding_expose_container_port": "3000",
                    "cladding_expose_host_port": "9000"
                }
            },
            {
                "Id": "sidecar-1",
                "Names": ["demo-expose-3000-9000-sidecar"],
                "Status": "Up 3 seconds",
                "Labels": {
                    "cladding": "demo",
                    "project_root": "/tmp/demo/.cladding",
                    "cladding_expose": "true",
                    "cladding_expose_target": "agent",
                    "cladding_expose_role": "pod-sidecar",
                    "cladding_expose_container_port": "3000",
                    "cladding_expose_host_port": "9000"
                }
            },
            {
                "Id": "skip-me",
                "Names": ["not-an-expose-proxy"],
                "Status": "Up 3 seconds",
                "Labels": {
                    "cladding": "demo"
                }
            }
        ]);

        let items = parse_expose_proxy_items(&parsed);
        assert_eq!(items.len(), 2);
        assert_eq!(items[0].proxy.id, "abc123");
        assert_eq!(items[0].proxy.name, "demo-expose-3000-9000");
        assert_eq!(items[0].proxy.container_port, 3000);
        assert_eq!(items[0].proxy.host_port, 9000);
        assert_eq!(items[0].proxy.role, "host-helper");
        assert_eq!(items[0].project_root, "/tmp/demo/.cladding");
        assert_eq!(items[0].target, "agent");
        assert_eq!(items[1].proxy.role, "pod-sidecar");
    }

    #[test]
    fn project_expose_proxies_from_items_filters_to_host_helper() {
        let items = vec![
            ExposeProxyItem {
                proxy: ExposeProxy {
                    id: "abc123".to_string(),
                    name: "demo-expose-3000-9000".to_string(),
                    host_port: 9000,
                    container_port: 3000,
                    status: "Up 3 seconds".to_string(),
                    role: "host-helper".to_string(),
                },
                project_root: "/tmp/demo/.cladding".to_string(),
                target: "agent".to_string(),
                role: "host-helper".to_string(),
            },
            ExposeProxyItem {
                proxy: ExposeProxy {
                    id: "sidecar-1".to_string(),
                    name: "demo-expose-3000-9000-sidecar".to_string(),
                    host_port: 9000,
                    container_port: 3000,
                    status: "Up 3 seconds".to_string(),
                    role: "pod-sidecar".to_string(),
                },
                project_root: "/tmp/demo/.cladding".to_string(),
                target: "agent".to_string(),
                role: "pod-sidecar".to_string(),
            },
        ];

        let proxies =
            project_expose_proxies_from_items(items, "/tmp/demo/.cladding", Some("host-helper"));

        assert_eq!(proxies.len(), 1);
        assert_eq!(proxies[0].id, "abc123");
        assert_eq!(proxies[0].role, "host-helper");
    }

    #[test]
    fn parse_expose_proxy_item_accepts_string_names_and_state_fallback() {
        let parsed = json!({
            "ID": "xyz789",
            "Names": "demo-expose-4000-9100",
            "State": "running",
            "Labels": "cladding=demo,project_root=/tmp/demo/.cladding,cladding_expose=true,cladding_expose_target=agent,cladding_expose_container_port=4000,cladding_expose_host_port=9100"
        });

        let item = parse_expose_proxy_item(&parsed).expect("proxy item");
        assert_eq!(item.proxy.id, "xyz789");
        assert_eq!(item.proxy.name, "demo-expose-4000-9100");
        assert_eq!(item.proxy.status, "running");
    }

    #[test]
    fn remove_output_is_missing_container_matches_expected_errors() {
        let output = Output {
            status: std::process::Command::new("true").status().expect("status"),
            stdout: Vec::new(),
            stderr: b"Error: no such container".to_vec(),
        };
        assert!(remove_output_is_missing_container(&output));
    }

    #[test]
    fn build_pod_create_command_includes_labels_network_and_aliases() {
        let pod = RuntimePod {
            name: "demo-agent".to_string(),
            labels: std::collections::BTreeMap::from([
                ("app".to_string(), "agent".to_string()),
                ("cladding".to_string(), "demo".to_string()),
                (
                    "project_root".to_string(),
                    "/tmp/demo/.cladding".to_string(),
                ),
            ]),
            network_name: "default".to_string(),
            ip: String::new(),
            host_aliases: vec![RuntimeHostAlias {
                ip: "10.90.1.2".to_string(),
                hostnames: vec!["demo-proxy".to_string(), "proxy-alias".to_string()],
            }],
            init_tasks: Vec::new(),
            containers: Vec::new(),
            userns_keep_id: true,
        };

        let cmd = build_pod_create_command(&pod);
        assert_eq!(
            command_args(&cmd),
            vec![
                "pod",
                "create",
                "--name",
                "demo-agent",
                "--label",
                "app=agent",
                "--label",
                "cladding=demo",
                "--label",
                "project_root=/tmp/demo/.cladding",
                "--network",
                "default",
                "--userns",
                "keep-id",
                "--add-host",
                "demo-proxy:10.90.1.2",
                "--add-host",
                "proxy-alias:10.90.1.2",
            ]
        );
    }

    #[test]
    fn build_pod_create_command_supports_network_none_without_ip() {
        let pod = RuntimePod {
            name: "demo-agent".to_string(),
            labels: std::collections::BTreeMap::new(),
            network_name: "none".to_string(),
            ip: String::new(),
            host_aliases: Vec::new(),
            init_tasks: Vec::new(),
            containers: Vec::new(),
            userns_keep_id: false,
        };

        let cmd = build_pod_create_command(&pod);
        assert_eq!(
            command_args(&cmd),
            vec!["pod", "create", "--name", "demo-agent", "--network", "none"]
        );
    }

    #[test]
    fn build_pod_create_command_does_not_include_runtime_flags() {
        let pod = RuntimePod {
            name: "demo-agent".to_string(),
            labels: std::collections::BTreeMap::new(),
            network_name: "default".to_string(),
            ip: String::new(),
            host_aliases: Vec::new(),
            init_tasks: Vec::new(),
            containers: Vec::new(),
            userns_keep_id: false,
        };

        let cmd = build_pod_create_command(&pod);
        let args = command_args(&cmd);
        assert!(!args.iter().any(|arg| arg == "--runtime"));
        assert!(!args.iter().any(|arg| arg == "--runtime-flag"));
    }

    #[test]
    fn format_command_quotes_shell_sensitive_args() {
        let mut cmd = Command::new("podman");
        cmd.args([
            "run",
            "--name",
            "demo agent",
            "image:latest",
            "echo",
            "it's ok",
        ]);

        assert_eq!(
            format_command(&cmd),
            "podman run --name 'demo agent' image:latest echo 'it'\\''s ok'"
        );
    }

    #[test]
    fn build_container_create_command_includes_mounts_env_and_io_flags() {
        let container = RuntimeContainer {
            name: "demo-agent-instance".to_string(),
            image: "demo:image".to_string(),
            command: vec!["sleep".to_string(), "infinity".to_string()],
            workdir: Some("/home/user/workspace".to_string()),
            env: vec![RuntimeEnvVar {
                name: "PATH".to_string(),
                value: "/opt/tools/bin".to_string(),
            }],
            mounts: vec![
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
            ports: vec![3000],
            stdin: true,
            tty: true,
        };

        let cmd = build_container_create_command("demo-agent", &container);
        assert_eq!(
            command_args(&cmd),
            vec![
                "create",
                "--pod",
                "demo-agent",
                "--name",
                "demo-agent-instance",
                "--workdir",
                "/home/user/workspace",
                "-i",
                "-t",
                "--env",
                "PATH=/opt/tools/bin",
                "--volume",
                "/tmp/demo/config:/opt/config:ro",
                "--volume",
                "demo-cache:/workspace/data",
                "--volume",
                "cladding-demo-agent-empty-workspace-tmp:/workspace/tmp",
                "--volume",
                "/tmp/demo/runtime/empty-mask:/home/user/workspace/.cladding:ro",
                "--expose",
                "3000",
                "--entrypoint",
                "sleep",
                "demo:image",
                "infinity",
            ]
        );
    }

    #[test]
    fn build_task_run_command_sets_root_user_and_capabilities() {
        let pod = RuntimePod {
            name: "demo-agent".to_string(),
            labels: std::collections::BTreeMap::new(),
            network_name: "cladding-1".to_string(),
            ip: "10.90.1.5".to_string(),
            host_aliases: Vec::new(),
            init_tasks: Vec::new(),
            containers: Vec::new(),
            userns_keep_id: false,
        };
        let task = RuntimeTask {
            name: "agent-node".to_string(),
            image: "alpine:latest".to_string(),
            command: vec![
                "/bin/sh".to_string(),
                "/opt/scripts/proxy_startup.sh".to_string(),
            ],
            env: vec![RuntimeEnvVar {
                name: "CLADDING_PROXY_NAME".to_string(),
                value: "demo-proxy".to_string(),
            }],
            mounts: vec![RuntimeMount {
                mount_path: "/opt/scripts".to_string(),
                read_only: true,
                source: RuntimeMountSource::HostPath {
                    path: "/tmp/demo/scripts".into(),
                },
            }],
            run_as_user: Some(0),
            run_as_group: Some(0),
            added_capabilities: vec!["NET_ADMIN".to_string()],
        };

        let mut cmd = Command::new("podman");
        cmd.arg("run");
        cmd.arg("--rm");
        cmd.arg("--pod");
        cmd.arg(&pod.name);
        cmd.arg("--name");
        cmd.arg(format!("{}-{}", pod.name, task.name));
        cmd.arg("--user");
        cmd.arg("0:0");
        cmd.arg("--cap-add");
        cmd.arg("NET_ADMIN");
        append_env_args(&mut cmd, &task.env);
        append_mount_args(&mut cmd, &pod.name, &task.mounts);
        append_entrypoint_arg(&mut cmd, &task.command);
        cmd.arg(&task.image);
        append_command_args(&mut cmd, &task.command);

        assert_eq!(
            command_args(&cmd),
            vec![
                "run",
                "--rm",
                "--pod",
                "demo-agent",
                "--name",
                "demo-agent-agent-node",
                "--user",
                "0:0",
                "--cap-add",
                "NET_ADMIN",
                "--env",
                "CLADDING_PROXY_NAME=demo-proxy",
                "--volume",
                "/tmp/demo/scripts:/opt/scripts:ro",
                "--entrypoint",
                "/bin/sh",
                "alpine:latest",
                "/opt/scripts/proxy_startup.sh",
            ]
        );
    }

    #[test]
    fn remove_output_is_missing_pod_matches_expected_errors() {
        let output = Output {
            status: std::process::Command::new("true").status().expect("status"),
            stdout: Vec::new(),
            stderr: b"Error: no such pod".to_vec(),
        };
        assert!(remove_output_is_missing_pod(&output));
    }
}
