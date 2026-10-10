use crate::error::{Error, Result};
use crate::runtime::{
    ManagedVolumeKind, RuntimeComponent, RuntimeContainer, RuntimeMountSource, RuntimeSpec,
    RuntimeUserNamespace,
};
use anyhow::Context as _;
use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::io::ErrorKind;
use std::path::Path;
use std::process::{Command, Output};

use super::command::{
    PodmanRuntimeOptions, ensure_success_output, ensure_success_output_with_diagnostics,
    forward_output, podman_command_with_options, run_helper_command, trace_command,
};
use super::mounts::{append_mount_args, generated_empty_mask_dirs};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RuntimeResource {
    pub kind: &'static str,
    pub name: String,
    pub state: String,
}

#[derive(Debug, Clone)]
pub struct RuntimeInventory {
    expected_count: usize,
    pub resources: Vec<RuntimeResource>,
}

impl RuntimeInventory {
    pub fn is_empty(&self) -> bool {
        self.resources.is_empty()
    }

    pub fn is_fully_running(&self) -> bool {
        self.resources.len() == self.expected_count
            && self.resources.iter().all(|resource| {
                resource.kind == "container" && resource.state.eq_ignore_ascii_case("running")
            })
    }
}

/// Direct runtime helpers for creating and cleaning up Podman resources.
pub fn runtime_create(spec: &RuntimeSpec, verbose: bool, quiet_helpers: bool) -> Result<()> {
    prepare_runtime_socket_dirs(spec)?;
    ensure_runtime_empty_mask_dir(spec)?;
    ensure_socket_volumes(spec, verbose, quiet_helpers)?;
    ensure_managed_mount_volumes(spec, verbose, quiet_helpers)?;

    for component in runtime_components(spec) {
        for container in &component.containers {
            container_run(
                component.use_runsc,
                component,
                container,
                verbose,
                quiet_helpers,
            )?;
            if should_install_baffle_ca(spec, component) {
                install_baffle_ca(container, verbose, quiet_helpers)?;
            }
        }
    }

    Ok(())
}

/// Initialize the project CA with the same Baffle binary, daemon config, and
/// credentials mount that the proxy runtime will use.
pub fn initialize_baffle_ca(spec: &RuntimeSpec, verbose: bool, quiet_helpers: bool) -> Result<()> {
    let mut cmd = build_baffle_ca_init_command(spec);
    trace_command(&cmd, verbose);
    let output = cmd.output().with_context(|| {
        format!(
            "failed to run Baffle CA initialization for project '{}'",
            spec.project_name
        )
    })?;
    if output.status.success() {
        if verbose {
            forward_output(&output)?;
        }
        return Ok(());
    }
    if quiet_helpers {
        return ensure_success_output_with_diagnostics(&output, "Baffle CA initialization");
    }
    if verbose {
        forward_output(&output)?;
    }
    ensure_success_output(&output, "Baffle CA initialization")
}

fn build_baffle_ca_init_command(spec: &RuntimeSpec) -> Command {
    let proxy_image = spec
        .proxy
        .containers
        .first()
        .map(|container| container.image.as_str())
        .expect("runtime spec must include a proxy container");
    let mounts = [
        crate::runtime::RuntimeMount {
            mount_path: "/opt/config".to_string(),
            read_only: true,
            source: crate::runtime::RuntimeMountSource::HostPath {
                path: spec.project_root.join("config"),
            },
        },
        crate::runtime::RuntimeMount {
            mount_path: "/opt/credentials/baffle".to_string(),
            read_only: false,
            source: crate::runtime::RuntimeMountSource::HostPath {
                path: spec.project_root.join("credentials/baffle"),
            },
        },
        crate::runtime::RuntimeMount {
            mount_path: "/opt/tools/bin/baffle".to_string(),
            read_only: true,
            source: crate::runtime::RuntimeMountSource::HostPath {
                path: spec.project_root.join("tools/bin/baffle"),
            },
        },
    ];

    let mut cmd = Command::new("podman");
    cmd.args(["run", "--rm", "--network", "none", "--userns", "keep-id"]);
    append_mount_args(&mut cmd, &spec.proxy.name, &mounts);
    cmd.args(["--entrypoint", "/opt/tools/bin/baffle"]);
    cmd.arg(proxy_image);
    cmd.args(["ca", "init", "--config", "/opt/config/proxy/daemon.toml"]);
    cmd
}

pub fn runtime_inventory(
    spec: &RuntimeSpec,
    verbose: bool,
    quiet_helpers: bool,
) -> Result<RuntimeInventory> {
    let mut expected_count = 0;
    let mut resources = Vec::new();

    for component in runtime_components(spec) {
        for container in &component.containers {
            expected_count += 1;
            if let Some(state) =
                inspect_resource_state("container", &container.name, verbose, quiet_helpers)?
            {
                resources.push(RuntimeResource {
                    kind: "container",
                    name: container.name.clone(),
                    state,
                });
            }
        }

        // Detect pods created by older Cladding versions so `up` does not
        // mistake their proxy containers for the current standalone runtime.
        if let Some(state) = inspect_resource_state("pod", &component.name, verbose, quiet_helpers)?
        {
            resources.push(RuntimeResource {
                kind: "pod",
                name: component.name.clone(),
                state,
            });
        }
    }

    Ok(RuntimeInventory {
        expected_count,
        resources,
    })
}

pub fn runtime_cleanup(spec: &RuntimeSpec, verbose: bool, quiet_helpers: bool) -> Result<()> {
    let mut cleanup_error = None;
    for component in runtime_components(spec) {
        if inspect_resource_state("pod", &component.name, verbose, quiet_helpers)?.is_some() {
            if resource_labels_match_component(
                "pod",
                &component.name,
                component,
                verbose,
                quiet_helpers,
            )? {
                record_cleanup_result(
                    &mut cleanup_error,
                    legacy_pod_rm(&component.name, verbose, quiet_helpers),
                );
            } else {
                record_cleanup_result(
                    &mut cleanup_error,
                    Err(Error::message(format!(
                        "refusing to remove pod '{}' because it is not owned by this Cladding component",
                        component.name
                    ))),
                );
            }
            // The pod may own the legacy proxy container. Skip standalone
            // container removal whenever a same-named pod exists.
            continue;
        }

        for container in &component.containers {
            if inspect_resource_state("container", &container.name, verbose, quiet_helpers)?
                .is_none()
            {
                continue;
            }
            if resource_labels_match_component(
                "container",
                &container.name,
                component,
                verbose,
                quiet_helpers,
            )? {
                record_cleanup_result(
                    &mut cleanup_error,
                    container_rm(&container.name, verbose, quiet_helpers),
                );
            } else {
                record_cleanup_result(
                    &mut cleanup_error,
                    Err(Error::message(format!(
                        "refusing to remove container '{}' because it is not owned by this Cladding component",
                        container.name
                    ))),
                );
            }
        }
    }

    record_cleanup_result(
        &mut cleanup_error,
        remove_socket_volumes(spec, verbose, quiet_helpers),
    );
    record_cleanup_result(
        &mut cleanup_error,
        remove_managed_mount_seed_helpers(spec, verbose, quiet_helpers),
    );
    record_cleanup_result(
        &mut cleanup_error,
        remove_managed_mount_volumes(spec, verbose, quiet_helpers),
    );
    match cleanup_error {
        Some(err) => Err(err),
        None => Ok(()),
    }
}

/// Clean up resources only when their Podman labels identify this runtime.
/// This is used by one-off startup and teardown paths so a name collision
/// cannot cause cleanup to remove another project's resources.
pub fn runtime_cleanup_owned(spec: &RuntimeSpec, verbose: bool, quiet_helpers: bool) -> Result<()> {
    // runtime_cleanup checks the component labels on every existing resource,
    // including legacy pods, before it removes anything.
    runtime_cleanup(spec, verbose, quiet_helpers)
}

fn socket_volume_names(spec: &RuntimeSpec) -> Vec<String> {
    let mut names = BTreeSet::new();
    for component in runtime_components(spec) {
        for container in &component.containers {
            for mount in &container.mounts {
                if let RuntimeMountSource::NamedVolumeChown { claim_name } = &mount.source {
                    names.insert(claim_name.clone());
                }
            }
        }
    }
    names.into_iter().collect()
}

fn proxy_socket_volume_names(spec: &RuntimeSpec) -> BTreeSet<String> {
    spec.proxy
        .containers
        .iter()
        .flat_map(|container| &container.mounts)
        .filter_map(|mount| match &mount.source {
            RuntimeMountSource::NamedVolumeChown { claim_name } => Some(claim_name.clone()),
            _ => None,
        })
        .collect()
}

fn ensure_socket_volumes(spec: &RuntimeSpec, verbose: bool, quiet_helpers: bool) -> Result<()> {
    let proxy_socket_volumes = proxy_socket_volume_names(spec);
    for name in socket_volume_names(spec) {
        if inspect_resource_state("volume", &name, verbose, quiet_helpers)?.is_some() {
            if !socket_volume_labels_match(&name, spec, verbose, quiet_helpers)? {
                return Err(Error::message(format!(
                    "refusing to use Podman volume '{name}' because it is not owned by this Cladding runtime"
                )));
            }
        } else {
            let mut cmd = build_socket_volume_create_command(&name, spec);
            trace_command(&cmd, verbose);
            let output = cmd
                .output()
                .with_context(|| format!("failed to create Cladding socket volume '{name}'"))?;
            if !output.status.success() {
                if quiet_helpers {
                    ensure_success_output_with_diagnostics(&output, "podman volume create")?;
                }
                if verbose {
                    forward_output(&output)?;
                }
                ensure_success_output(&output, "podman volume create")?;
            }
            if verbose {
                forward_output(&output)?;
            }
        }

        if proxy_socket_volumes.contains(&name) {
            let mut cmd = build_proxy_socket_volume_prepare_command(&name, spec);
            run_helper_command(
                &mut cmd,
                verbose,
                quiet_helpers,
                "Baffle socket volume permissions",
            )
            .with_context(|| format!("failed to secure Baffle socket volume '{name}'"))?;
        }
    }

    Ok(())
}

fn remove_socket_volumes(spec: &RuntimeSpec, verbose: bool, quiet_helpers: bool) -> Result<()> {
    let mut cleanup_error = None;
    for name in socket_volume_names(spec) {
        if inspect_resource_state("volume", &name, verbose, quiet_helpers)?.is_none() {
            continue;
        }
        if !socket_volume_labels_match(&name, spec, verbose, quiet_helpers)? {
            record_cleanup_result(
                &mut cleanup_error,
                Err(Error::message(format!(
                    "refusing to remove Podman volume '{name}' because it is not owned by this Cladding runtime"
                ))),
            );
            continue;
        }

        let mut cmd = build_volume_rm_command(&name);
        trace_command(&cmd, verbose);
        let output = cmd
            .output()
            .with_context(|| format!("failed to remove Cladding socket volume '{name}'"))?;
        if output.status.success() {
            if verbose {
                forward_output(&output)?;
            }
            continue;
        }
        if remove_output_is_missing_volume(&output) {
            continue;
        }
        if quiet_helpers {
            record_cleanup_result(
                &mut cleanup_error,
                ensure_success_output_with_diagnostics(&output, "podman volume rm"),
            );
            continue;
        }
        if verbose {
            forward_output(&output)?;
        }
        record_cleanup_result(
            &mut cleanup_error,
            ensure_success_output(&output, "podman volume rm"),
        );
    }

    match cleanup_error {
        Some(err) => Err(err),
        None => Ok(()),
    }
}

fn socket_volume_labels(spec: &RuntimeSpec, name: &str) -> BTreeMap<String, String> {
    BTreeMap::from([
        ("cladding".to_string(), spec.project_name.clone()),
        (
            "project_root".to_string(),
            spec.project_root.to_string_lossy().into_owned(),
        ),
        (
            "cladding_resource".to_string(),
            "inter-container-socket".to_string(),
        ),
        ("cladding_socket_volume".to_string(), name.to_string()),
    ])
}

fn socket_volume_labels_match(
    name: &str,
    spec: &RuntimeSpec,
    verbose: bool,
    quiet_helpers: bool,
) -> Result<bool> {
    let labels = inspect_value("volume", name, "{{json .Labels}}", verbose, quiet_helpers)?;
    let labels: serde_json::Value = serde_json::from_str(&labels)
        .with_context(|| format!("failed to parse labels for Podman volume {name}"))?;

    Ok(labels_match_expected(
        &labels,
        &socket_volume_labels(spec, name),
    ))
}

fn managed_mount_volumes(spec: &RuntimeSpec) -> BTreeMap<String, ManagedVolumeKind> {
    let mut volumes = BTreeMap::new();
    for component in runtime_components(spec) {
        for container in &component.containers {
            for mount in &container.mounts {
                if let RuntimeMountSource::ManagedVolume { claim_name, kind } = &mount.source {
                    volumes
                        .entry(claim_name.clone())
                        .or_insert_with(|| kind.clone());
                }
            }
        }
    }
    volumes
}

fn ensure_managed_mount_volumes(
    spec: &RuntimeSpec,
    verbose: bool,
    quiet_helpers: bool,
) -> Result<()> {
    for (name, kind) in managed_mount_volumes(spec) {
        if matches!(&kind, ManagedVolumeKind::Copy { .. }) {
            remove_managed_mount_seed_helper(spec, &name, false, verbose, quiet_helpers)?;
        }

        let existing = inspect_resource_state("volume", &name, verbose, quiet_helpers)?.is_some();
        if existing && !managed_mount_volume_labels_match(&name, spec, verbose, quiet_helpers)? {
            return Err(Error::message(format!(
                "refusing to use Podman volume '{name}' because it is not owned by this Cladding runtime"
            )));
        }

        match kind {
            ManagedVolumeKind::Copy { source } => {
                if existing && managed_copy_seed_is_complete(spec, &name)? {
                    continue;
                }
                if !source.is_dir() {
                    return Err(Error::message(format!(
                        "copy mount source is not a directory: {}",
                        source.display()
                    )));
                }
                remove_managed_copy_seed_marker(spec, &name)?;
                if existing {
                    remove_managed_mount_volume(spec, &name, verbose, quiet_helpers)?;
                }
                create_managed_mount_volume(spec, &name, None, verbose, quiet_helpers)?;
                seed_managed_copy_volume(spec, &name, &source, verbose, quiet_helpers)?;
                mark_managed_copy_seed_complete(spec, &name)?;
            }
            ManagedVolumeKind::Tmpfs { size_bytes } => {
                if !existing {
                    create_managed_mount_volume(
                        spec,
                        &name,
                        Some(size_bytes),
                        verbose,
                        quiet_helpers,
                    )?;
                }
            }
        }
    }

    Ok(())
}

fn managed_mount_volume_labels(spec: &RuntimeSpec, name: &str) -> BTreeMap<String, String> {
    BTreeMap::from([
        ("cladding".to_string(), spec.project_name.clone()),
        (
            "project_root".to_string(),
            spec.project_root.to_string_lossy().into_owned(),
        ),
        ("cladding_resource".to_string(), "managed-mount".to_string()),
        (
            "cladding_runtime_root".to_string(),
            spec.runtime_root.to_string_lossy().into_owned(),
        ),
        ("cladding_mount_volume".to_string(), name.to_string()),
    ])
}

fn managed_mount_seed_helper_name(name: &str) -> String {
    format!("{name}-seed")
}

fn managed_mount_seed_helper_labels(
    spec: &RuntimeSpec,
    volume_name: &str,
) -> BTreeMap<String, String> {
    let mut labels = managed_mount_volume_labels(spec, volume_name);
    labels.insert(
        "cladding_resource".to_string(),
        "managed-mount-seed-helper".to_string(),
    );
    labels
}

fn managed_mount_volume_labels_match(
    name: &str,
    spec: &RuntimeSpec,
    verbose: bool,
    quiet_helpers: bool,
) -> Result<bool> {
    let labels = inspect_value("volume", name, "{{json .Labels}}", verbose, quiet_helpers)?;
    let labels: serde_json::Value = serde_json::from_str(&labels)
        .with_context(|| format!("failed to parse labels for managed mount volume {name}"))?;
    Ok(labels_match_expected(
        &labels,
        &managed_mount_volume_labels(spec, name),
    ))
}

fn create_managed_mount_volume(
    spec: &RuntimeSpec,
    name: &str,
    tmpfs_size_bytes: Option<u64>,
    verbose: bool,
    quiet_helpers: bool,
) -> Result<()> {
    let mut cmd = build_managed_mount_volume_create_command(spec, name, tmpfs_size_bytes);
    trace_command(&cmd, verbose);
    let output = cmd
        .output()
        .with_context(|| format!("failed to create managed mount volume '{name}'"))?;
    if output.status.success() {
        if verbose {
            forward_output(&output)?;
        }
        return Ok(());
    }
    if quiet_helpers {
        return ensure_success_output_with_diagnostics(&output, "podman volume create");
    }
    if verbose {
        forward_output(&output)?;
    }
    ensure_success_output(&output, "podman volume create")
}

fn build_managed_mount_volume_create_command(
    spec: &RuntimeSpec,
    name: &str,
    tmpfs_size_bytes: Option<u64>,
) -> Command {
    let mut cmd = Command::new("podman");
    cmd.args(["volume", "create", "--opt", "nocopy"]);
    if let Some(size_bytes) = tmpfs_size_bytes {
        cmd.args(["--opt", "type=tmpfs", "--opt", "device=tmpfs"]);
        cmd.arg("--opt");
        cmd.arg(format!("o=size={size_bytes},mode=1777"));
    }
    for (key, value) in managed_mount_volume_labels(spec, name) {
        cmd.arg("--label");
        cmd.arg(format!("{key}={value}"));
    }
    cmd.arg(name);
    cmd
}

fn remove_managed_mount_volume(
    spec: &RuntimeSpec,
    name: &str,
    verbose: bool,
    quiet_helpers: bool,
) -> Result<()> {
    if inspect_resource_state("volume", name, verbose, quiet_helpers)?.is_none() {
        remove_managed_copy_seed_marker(spec, name)?;
        return Ok(());
    }
    if !managed_mount_volume_labels_match(name, spec, verbose, quiet_helpers)? {
        return Err(Error::message(format!(
            "refusing to remove Podman volume '{name}' because it is not owned by this Cladding runtime"
        )));
    }

    let mut cmd = build_volume_rm_command(name);
    trace_command(&cmd, verbose);
    let output = cmd
        .output()
        .with_context(|| format!("failed to remove managed mount volume '{name}'"))?;
    if output.status.success() || remove_output_is_missing_volume(&output) {
        if verbose {
            forward_output(&output)?;
        }
        remove_managed_copy_seed_marker(spec, name)?;
        return Ok(());
    }
    if quiet_helpers {
        return ensure_success_output_with_diagnostics(&output, "podman volume rm");
    }
    if verbose {
        forward_output(&output)?;
    }
    ensure_success_output(&output, "podman volume rm")
}

fn managed_copy_seed_marker_path(spec: &RuntimeSpec, volume_name: &str) -> std::path::PathBuf {
    spec.runtime_root
        .join("runtime/managed-mounts")
        .join(format!("{volume_name}.copy-seeded"))
}

fn managed_copy_seed_is_complete(spec: &RuntimeSpec, volume_name: &str) -> Result<bool> {
    let marker = managed_copy_seed_marker_path(spec, volume_name);
    let contents = match fs::read(&marker) {
        Ok(contents) => contents,
        Err(err) if err.kind() == ErrorKind::NotFound => return Ok(false),
        Err(err) => {
            return Err(anyhow::Error::new(err)
                .context(format!(
                    "failed to read copy mount seed marker {}",
                    marker.display()
                ))
                .into());
        }
    };
    Ok(contents == b"cladding-copy-seed-v1\n")
}

fn remove_managed_copy_seed_marker(spec: &RuntimeSpec, volume_name: &str) -> Result<()> {
    let marker = managed_copy_seed_marker_path(spec, volume_name);
    match fs::remove_file(&marker) {
        Ok(()) => Ok(()),
        Err(err) if err.kind() == ErrorKind::NotFound => Ok(()),
        Err(err) => Err(anyhow::Error::new(err)
            .context(format!(
                "failed to remove copy mount seed marker {}",
                marker.display()
            ))
            .into()),
    }
}

fn mark_managed_copy_seed_complete(spec: &RuntimeSpec, volume_name: &str) -> Result<()> {
    let marker = managed_copy_seed_marker_path(spec, volume_name);
    let parent = marker
        .parent()
        .expect("managed copy seed marker path has a parent");
    fs::create_dir_all(parent).with_context(|| {
        format!(
            "failed to create managed mount state directory {}",
            parent.display()
        )
    })?;
    set_restrictive_dir_permissions(parent)?;
    fs::write(&marker, b"cladding-copy-seed-v1\n").with_context(|| {
        format!(
            "failed to write copy mount seed marker {}",
            marker.display()
        )
    })?;
    Ok(())
}

fn seed_managed_copy_volume(
    spec: &RuntimeSpec,
    volume_name: &str,
    source: &Path,
    verbose: bool,
    quiet_helpers: bool,
) -> Result<()> {
    let mut cmd = build_managed_mount_helper_command(
        spec,
        volume_name,
        Some(source),
        managed_copy_seed_script(),
    );
    run_helper_command(
        &mut cmd,
        verbose,
        quiet_helpers,
        "copy mount volume seeding",
    )
}

fn managed_copy_seed_script() -> &'static str {
    concat!(
        "set -eu\n",
        "test -d /source || { echo 'copy mount source must be a directory' >&2; exit 1; }\n",
        "cp -a --no-preserve=ownership /source/. /volume/\n",
        "chmod --reference=/source /volume\n"
    )
}

fn build_managed_mount_helper_command(
    spec: &RuntimeSpec,
    volume_name: &str,
    source: Option<&Path>,
    script: &str,
) -> Command {
    let helper_name = managed_mount_seed_helper_name(volume_name);
    let mut cmd = Command::new("podman");
    cmd.args([
        "run",
        "--rm",
        "--pull=never",
        "--network",
        "none",
        "--userns",
        "keep-id",
        "--name",
        &helper_name,
    ]);
    for (key, value) in managed_mount_seed_helper_labels(spec, volume_name) {
        cmd.arg("--label");
        cmd.arg(format!("{key}={value}"));
    }
    if let Some(source) = source {
        cmd.arg("--volume");
        cmd.arg(format!("{}:/source:ro", source.display()));
    }
    cmd.arg("--volume");
    cmd.arg(format!("{volume_name}:/volume:U"));
    cmd.args(["--entrypoint", "/bin/sh"]);
    let proxy_image = spec
        .proxy
        .containers
        .first()
        .map(|container| container.image.as_str())
        .expect("runtime spec must include a proxy container");
    cmd.arg(proxy_image);
    cmd.args(["-ec", script]);
    cmd
}

fn remove_managed_mount_seed_helper(
    spec: &RuntimeSpec,
    volume_name: &str,
    allow_running: bool,
    verbose: bool,
    quiet_helpers: bool,
) -> Result<()> {
    let helper_name = managed_mount_seed_helper_name(volume_name);
    let Some(state) = inspect_resource_state("container", &helper_name, verbose, quiet_helpers)?
    else {
        return Ok(());
    };
    let labels = managed_mount_seed_helper_labels(spec, volume_name);
    if !resource_labels_match_expected("container", &helper_name, &labels, verbose, quiet_helpers)?
    {
        return Err(Error::message(format!(
            "refusing to remove container '{helper_name}' because it is not owned by this Cladding runtime"
        )));
    }
    if !allow_running && state.eq_ignore_ascii_case("running") {
        return Err(Error::message(format!(
            "copy mount volume '{volume_name}' already has a running seed helper"
        )));
    }
    container_rm(&helper_name, verbose, quiet_helpers)
}

fn remove_managed_mount_seed_helpers(
    spec: &RuntimeSpec,
    verbose: bool,
    quiet_helpers: bool,
) -> Result<()> {
    let mut helper_names = BTreeSet::new();
    for (name, kind) in managed_mount_volumes(spec) {
        if matches!(kind, ManagedVolumeKind::Copy { .. }) {
            helper_names.insert(managed_mount_seed_helper_name(&name));
        }
    }
    helper_names.extend(list_managed_mount_seed_helpers(
        spec,
        verbose,
        quiet_helpers,
    )?);
    for helper_name in helper_names {
        let Some(volume_name) = helper_name.strip_suffix("-seed") else {
            return Err(Error::message(format!(
                "refusing to remove unexpected managed mount helper '{helper_name}'"
            )));
        };
        remove_managed_mount_seed_helper(spec, volume_name, true, verbose, quiet_helpers)?;
    }
    Ok(())
}

fn remove_managed_mount_volumes(
    spec: &RuntimeSpec,
    verbose: bool,
    quiet_helpers: bool,
) -> Result<()> {
    let mut volume_names = managed_mount_volumes(spec)
        .into_keys()
        .collect::<BTreeSet<_>>();
    volume_names.extend(list_managed_mount_volumes(spec, verbose, quiet_helpers)?);
    for name in volume_names {
        remove_managed_mount_volume(spec, &name, verbose, quiet_helpers)?;
    }
    Ok(())
}

fn list_managed_mount_seed_helpers(
    spec: &RuntimeSpec,
    verbose: bool,
    quiet_helpers: bool,
) -> Result<Vec<String>> {
    let project_root = spec.project_root.to_string_lossy().into_owned();
    let runtime_root = spec.runtime_root.to_string_lossy().into_owned();
    list_labeled_resource_names(
        "container",
        &[
            ("cladding_resource", "managed-mount-seed-helper"),
            ("cladding", spec.project_name.as_str()),
            ("project_root", project_root.as_str()),
            ("cladding_runtime_root", runtime_root.as_str()),
        ],
        verbose,
        quiet_helpers,
    )
}

fn list_managed_mount_volumes(
    spec: &RuntimeSpec,
    verbose: bool,
    quiet_helpers: bool,
) -> Result<Vec<String>> {
    let project_root = spec.project_root.to_string_lossy().into_owned();
    let runtime_root = spec.runtime_root.to_string_lossy().into_owned();
    list_labeled_resource_names(
        "volume",
        &[
            ("cladding_resource", "managed-mount"),
            ("cladding", spec.project_name.as_str()),
            ("project_root", project_root.as_str()),
            ("cladding_runtime_root", runtime_root.as_str()),
        ],
        verbose,
        quiet_helpers,
    )
}

fn list_labeled_resource_names(
    kind: &str,
    labels: &[(&str, &str)],
    verbose: bool,
    quiet_helpers: bool,
) -> Result<Vec<String>> {
    let mut cmd = Command::new("podman");
    match kind {
        "container" => cmd.args(["ps", "--all"]),
        "volume" => cmd.args(["volume", "ls"]),
        _ => {
            return Err(Error::message(format!(
                "unsupported resource kind '{kind}'"
            )));
        }
    };
    for (key, value) in labels {
        cmd.arg("--filter");
        cmd.arg(format!("label={key}={value}"));
    }
    cmd.args([
        "--format",
        if kind == "container" {
            "{{.Names}}"
        } else {
            "{{.Name}}"
        },
    ]);
    trace_command(&cmd, verbose);
    let output = cmd
        .output()
        .with_context(|| format!("failed to list managed mount {kind}s"))?;
    if !output.status.success() {
        if quiet_helpers {
            ensure_success_output_with_diagnostics(&output, "managed mount resource listing")?;
        }
        if verbose {
            forward_output(&output)?;
        }
        ensure_success_output(&output, "managed mount resource listing")?;
    }
    Ok(String::from_utf8_lossy(&output.stdout)
        .lines()
        .map(str::trim)
        .filter(|name| !name.is_empty())
        .map(str::to_string)
        .collect())
}

fn build_socket_volume_create_command(name: &str, spec: &RuntimeSpec) -> Command {
    let mut cmd = Command::new("podman");
    cmd.args(["volume", "create", "--opt", "nocopy"]);
    for (key, value) in socket_volume_labels(spec, name) {
        cmd.arg("--label");
        cmd.arg(format!("{key}={value}"));
    }
    cmd.arg(name);
    cmd
}

fn build_proxy_socket_volume_prepare_command(name: &str, spec: &RuntimeSpec) -> Command {
    let mut cmd = Command::new("podman");
    cmd.args(["run", "--rm", "--network", "none", "--userns", "keep-id"]);
    cmd.arg("--volume");
    cmd.arg(format!("{name}:/socket:U"));
    cmd.args(["--entrypoint", "/bin/sh"]);
    let proxy_image = spec
        .proxy
        .containers
        .first()
        .map(|container| container.image.as_str())
        .expect("runtime spec must include a proxy container");
    cmd.arg(proxy_image);
    cmd.args(["-ec", "mkdir -p /socket && chmod 0700 /socket"]);
    cmd
}

fn build_volume_rm_command(name: &str) -> Command {
    let mut cmd = Command::new("podman");
    cmd.args(["volume", "rm", name]);
    cmd
}

fn remove_output_is_missing_volume(output: &Output) -> bool {
    let stderr = String::from_utf8_lossy(&output.stderr).to_ascii_lowercase();
    stderr.contains("no such volume") || stderr.contains("no volume with name or id")
}

fn record_cleanup_result(target: &mut Option<crate::error::Error>, result: Result<()>) {
    if let Err(err) = result
        && target.is_none()
    {
        *target = Some(err);
    }
}

fn resource_labels_match_component(
    kind: &str,
    name: &str,
    component: &RuntimeComponent,
    verbose: bool,
    quiet_helpers: bool,
) -> Result<bool> {
    let format = match kind {
        "container" => "{{json .Config.Labels}}",
        _ => "{{json .Labels}}",
    };
    let labels = inspect_value(kind, name, format, verbose, quiet_helpers)?;
    let labels: serde_json::Value = serde_json::from_str(&labels)
        .with_context(|| format!("failed to parse labels for {kind} {name}"))?;

    Ok(labels_match_expected(&labels, &component.labels))
}

fn resource_labels_match_expected(
    kind: &str,
    name: &str,
    expected: &BTreeMap<String, String>,
    verbose: bool,
    quiet_helpers: bool,
) -> Result<bool> {
    let format = match kind {
        "container" => "{{json .Config.Labels}}",
        _ => "{{json .Labels}}",
    };
    let labels = inspect_value(kind, name, format, verbose, quiet_helpers)?;
    let labels: serde_json::Value = serde_json::from_str(&labels)
        .with_context(|| format!("failed to parse labels for {kind} {name}"))?;
    Ok(labels_match_expected(&labels, expected))
}

fn labels_match_expected(
    labels: &serde_json::Value,
    expected: &std::collections::BTreeMap<String, String>,
) -> bool {
    expected.iter().all(|(key, expected_value)| {
        labels.get(key).and_then(serde_json::Value::as_str) == Some(expected_value.as_str())
    })
}

fn inspect_value(
    kind: &str,
    name: &str,
    format: &str,
    verbose: bool,
    quiet_helpers: bool,
) -> Result<String> {
    let mut cmd = Command::new("podman");
    cmd.args([kind, "inspect", "--format", format, name]);
    trace_command(&cmd, verbose);
    let output = cmd
        .output()
        .with_context(|| format!("failed to inspect {kind} {name}"))?;
    if quiet_helpers {
        ensure_success_output_with_diagnostics(&output, "podman inspect")?;
    } else {
        ensure_success_output(&output, "podman inspect")?;
    }
    Ok(String::from_utf8_lossy(&output.stdout).trim().to_string())
}

fn inspect_resource_state(
    kind: &'static str,
    name: &str,
    verbose: bool,
    quiet_helpers: bool,
) -> Result<Option<String>> {
    let mut exists_cmd = build_resource_exists_command(kind, name);
    trace_command(&exists_cmd, verbose);
    let exists_output = if quiet_helpers {
        Some(
            exists_cmd
                .output()
                .with_context(|| format!("failed to check whether {kind} exists: {name}"))?,
        )
    } else {
        None
    };
    let exists = match &exists_output {
        Some(output) => output.status,
        None => exists_cmd
            .status()
            .with_context(|| format!("failed to check whether {kind} exists: {name}"))?,
    };

    match exists.code() {
        Some(1) => return Ok(None),
        Some(0) => {}
        _ => {
            if let Some(output) = &exists_output {
                ensure_success_output_with_diagnostics(output, "podman exists")?;
            }
            eprintln!("error: failed to check whether {kind} exists: {name}");
            return Err(crate::error::Error::message(format!(
                "podman {kind} exists failed"
            )));
        }
    }

    let mut inspect_cmd = build_resource_inspect_command(kind, name);
    trace_command(&inspect_cmd, verbose);
    let output = inspect_cmd
        .output()
        .with_context(|| format!("failed to inspect {kind}: {name}"))?;
    if !output.status.success() {
        if quiet_helpers {
            ensure_success_output_with_diagnostics(&output, "podman inspect")?;
        }
        return ensure_success_output(&output, "podman inspect").map(|_| None);
    }

    Ok(Some(
        String::from_utf8_lossy(&output.stdout).trim().to_string(),
    ))
}

fn build_resource_exists_command(kind: &str, name: &str) -> Command {
    let mut cmd = Command::new("podman");
    cmd.args([kind, "exists", name]);
    cmd
}

fn build_resource_inspect_command(kind: &str, name: &str) -> Command {
    let mut cmd = Command::new("podman");
    let format = match kind {
        "pod" => "{{.State}}",
        "volume" => "{{.Name}}",
        _ => "{{.State.Status}}",
    };
    cmd.args([kind, "inspect", "--format", format, name]);
    cmd
}

fn legacy_pod_rm(pod_name: &str, verbose: bool, quiet_helpers: bool) -> Result<()> {
    let mut cmd = build_pod_rm_command(pod_name);
    trace_command(&cmd, verbose);
    let output = cmd
        .output()
        .with_context(|| format!("failed to run podman pod rm for {pod_name}"))?;

    if output.status.success() {
        if verbose {
            forward_output(&output)?;
        }
        return Ok(());
    }
    if remove_output_is_missing_pod(&output) {
        return Ok(());
    }

    if quiet_helpers {
        ensure_success_output_with_diagnostics(&output, "podman pod rm")?;
    }
    if verbose {
        forward_output(&output)?;
    }
    ensure_success_output(&output, "podman pod rm")
}

pub fn container_run(
    use_runsc: bool,
    component: &RuntimeComponent,
    container: &RuntimeContainer,
    verbose: bool,
    quiet_helpers: bool,
) -> Result<()> {
    let mut cmd = build_container_run_command(use_runsc, component, container);
    run_helper_command(&mut cmd, verbose, quiet_helpers, "podman run").inspect_err(|_| {
        eprintln!(
            "error: failed to start container '{}' for component '{}'",
            container.name, component.name
        );
    })
}

fn should_install_baffle_ca(spec: &RuntimeSpec, component: &RuntimeComponent) -> bool {
    component.name == spec.agent.name
        || spec
            .nw_sandbox
            .as_ref()
            .is_some_and(|sandbox| sandbox.name == component.name)
}

fn install_baffle_ca(
    container: &RuntimeContainer,
    verbose: bool,
    quiet_helpers: bool,
) -> Result<()> {
    let mut cmd = build_baffle_ca_install_command(&container.name);
    run_helper_command(
        &mut cmd,
        verbose,
        quiet_helpers,
        "Baffle CA installation",
    )
    .inspect_err(|_err| {
        eprintln!(
            "error: failed to install the Baffle CA in execution container '{}'",
            container.name
        );
        eprintln!(
            "hint: ensure the image provides sh, cp, and update-ca-certificates with a writable system trust store"
        );
    })
}

fn build_baffle_ca_install_command(container_name: &str) -> Command {
    let mut cmd = Command::new("podman");
    cmd.args([
        "exec",
        "--user",
        "0",
        container_name,
        "sh",
        "-ec",
        "cp /run/cladding/ca/baffle.crt /usr/local/share/ca-certificates/baffle.crt\nupdate-ca-certificates",
    ]);
    cmd
}

pub fn container_rm(container_name: &str, verbose: bool, quiet_helpers: bool) -> Result<()> {
    let mut cmd = build_container_rm_command(container_name);
    trace_command(&cmd, verbose);
    let output = cmd
        .output()
        .with_context(|| format!("failed to run podman rm for {container_name}"))?;

    if output.status.success() {
        if verbose {
            forward_output(&output)?;
        }
        return Ok(());
    }
    if remove_output_is_missing_container(&output) {
        return Ok(());
    }

    if quiet_helpers {
        ensure_success_output_with_diagnostics(&output, "podman rm")?;
    }
    if verbose {
        forward_output(&output)?;
    }
    ensure_success_output(&output, "podman rm")
}

fn remove_output_is_missing_container(output: &Output) -> bool {
    let stderr = String::from_utf8_lossy(&output.stderr).to_ascii_lowercase();
    stderr.contains("no such container") || stderr.contains("no container with name or id")
}

fn remove_output_is_missing_pod(output: &Output) -> bool {
    let stderr = String::from_utf8_lossy(&output.stderr).to_ascii_lowercase();
    stderr.contains("no such pod") || stderr.contains("no pod with name or id")
}

fn runtime_components(spec: &RuntimeSpec) -> Vec<&RuntimeComponent> {
    let mut components = vec![&spec.proxy, &spec.agent];
    if let Some(component) = &spec.nw_sandbox {
        components.push(component);
    }
    if let Some(component) = &spec.fs_sandbox {
        components.push(component);
    }
    components
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
        set_restrictive_dir_permissions(&socket_dir)?;
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

fn build_pod_rm_command(pod_name: &str) -> Command {
    let mut cmd = Command::new("podman");
    cmd.args(["pod", "rm", "-f", pod_name]);
    cmd
}

fn build_container_run_command(
    use_runsc: bool,
    component: &RuntimeComponent,
    container: &RuntimeContainer,
) -> Command {
    let mut cmd = podman_command_with_options(
        PodmanRuntimeOptions::new(use_runsc).with_network_none(component.network_name == "none"),
    );
    cmd.arg("run");
    for security_opt in &component.security_opts {
        cmd.arg("--security-opt");
        cmd.arg(security_opt);
    }
    cmd.arg("-d");
    cmd.arg("--init");
    append_label_args(&mut cmd, &component.labels);
    cmd.arg("--network");
    cmd.arg(&component.network_name);
    append_user_namespace_args(&mut cmd, component.user_namespace);
    cmd.arg("--hostname");
    cmd.arg(&component.name);
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
    append_mount_args(&mut cmd, &component.name, &container.mounts);
    append_port_args(&mut cmd, &container.ports);
    append_entrypoint_arg(&mut cmd, &container.command);

    cmd.arg(&container.image);
    append_command_args(&mut cmd, &container.command);
    cmd
}

fn append_user_namespace_args(cmd: &mut Command, user_namespace: RuntimeUserNamespace) {
    let mode = match user_namespace {
        RuntimeUserNamespace::Default => return,
        RuntimeUserNamespace::KeepId => "keep-id",
    };
    cmd.arg("--userns");
    cmd.arg(mode);
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::{ExecutionComponentConfig, ExecutionConfig};
    use crate::runtime::{
        RuntimeComponent, RuntimeContainer, RuntimeEnvVar, RuntimeMount, RuntimeMountSource,
        RuntimeUserNamespace,
    };

    fn command_args(cmd: &Command) -> Vec<String> {
        cmd.get_args()
            .map(|arg| arg.to_string_lossy().into_owned())
            .collect()
    }

    fn test_runtime_spec() -> RuntimeSpec {
        RuntimeSpec::build(
            Path::new("/tmp/project/.cladding"),
            &ExecutionConfig {
                name: "demo".to_string(),
                use_runsc: false,
                agent: ExecutionComponentConfig {
                    enabled: true,
                    image: "agent:image".to_string(),
                    build: None,
                    security_opts: Vec::new(),
                },
                nw_sandbox: None,
                fs_sandbox: None,
                proxy: None,
                mounts: Vec::new(),
            },
        )
    }

    #[test]
    fn build_container_run_command_passes_security_options_as_literal_component_args() {
        let config = ExecutionConfig {
            name: "demo".to_string(),
            use_runsc: false,
            agent: ExecutionComponentConfig {
                enabled: true,
                image: "agent:image".to_string(),
                build: None,
                security_opts: vec!["unmask=/proc/*".to_string(), "label=disable".to_string()],
            },
            nw_sandbox: Some(ExecutionComponentConfig {
                enabled: true,
                image: "nw:image".to_string(),
                build: None,
                security_opts: vec!["no-new-privileges".to_string()],
            }),
            fs_sandbox: Some(ExecutionComponentConfig {
                enabled: true,
                image: "fs:image".to_string(),
                build: None,
                security_opts: Vec::new(),
            }),
            proxy: None,
            mounts: Vec::new(),
        };
        let spec = RuntimeSpec::build(Path::new("/tmp/demo/.cladding"), &config);
        let security_opts = |component: &RuntimeComponent| {
            let container = component.containers.first().expect("component container");
            let args = command_args(&build_container_run_command(
                component.use_runsc,
                component,
                container,
            ));
            args.windows(2)
                .filter_map(|pair| (pair[0] == "--security-opt").then_some(pair[1].clone()))
                .collect::<Vec<_>>()
        };

        assert_eq!(
            security_opts(&spec.agent),
            vec!["unmask=/proc/*", "label=disable"]
        );
        assert_eq!(
            security_opts(spec.nw_sandbox.as_ref().expect("network sandbox")),
            vec!["no-new-privileges"]
        );
        assert!(security_opts(spec.fs_sandbox.as_ref().expect("filesystem sandbox")).is_empty());
        assert!(security_opts(&spec.proxy).is_empty());
    }

    #[test]
    fn managed_mount_seed_helper_is_offline_scoped_and_uses_the_proxy_image() {
        let spec = test_runtime_spec();
        let cmd = build_managed_mount_helper_command(
            &spec,
            "cladding-mount-0123456789abcdef",
            Some(Path::new("/tmp/project/source")),
            "copy script",
        );
        let args = command_args(&cmd);

        assert!(args.windows(2).any(|pair| pair == ["--network", "none"]));
        assert!(args.iter().any(|arg| arg == "--pull=never"));
        assert!(args.windows(2).any(|pair| pair == ["--userns", "keep-id"]));
        assert!(!args.iter().any(|arg| arg == "--user"));
        assert!(
            args.iter()
                .any(|arg| arg == "cladding-mount-0123456789abcdef-seed")
        );
        assert!(
            args.iter()
                .any(|arg| arg == "/tmp/project/source:/source:ro")
        );
        assert!(
            args.iter()
                .any(|arg| arg == "cladding-mount-0123456789abcdef:/volume:U")
        );
        assert!(
            args.iter()
                .any(|arg| arg == "localhost/cladding-proxy:latest")
        );
        assert_eq!(args[args.len() - 2..], ["-ec", "copy script"]);
    }

    #[test]
    fn managed_copy_seed_runs_as_the_keep_id_user() {
        let script = managed_copy_seed_script();

        assert!(script.contains("cp -a --no-preserve=ownership /source/. /volume/"));
        assert!(script.contains("chmod --reference=/source /volume"));
        assert!(!script.contains("chown"));
    }

    #[test]
    fn managed_copy_seed_marker_tracks_only_a_complete_seed() {
        let marker_root =
            std::env::temp_dir().join(format!("cladding-copy-seed-marker-{}", std::process::id()));
        let _ = fs::remove_dir_all(&marker_root);
        let mut spec = test_runtime_spec();
        spec.runtime_root = marker_root.clone();
        let volume_name = "cladding-mount-marker-test";

        assert!(!managed_copy_seed_is_complete(&spec, volume_name).unwrap());
        mark_managed_copy_seed_complete(&spec, volume_name).unwrap();
        assert!(managed_copy_seed_is_complete(&spec, volume_name).unwrap());

        fs::write(
            managed_copy_seed_marker_path(&spec, volume_name),
            b"incomplete\n",
        )
        .unwrap();
        assert!(!managed_copy_seed_is_complete(&spec, volume_name).unwrap());
        remove_managed_copy_seed_marker(&spec, volume_name).unwrap();
        assert!(!managed_copy_seed_is_complete(&spec, volume_name).unwrap());
        fs::remove_dir_all(marker_root).unwrap();
    }

    #[test]
    fn managed_mount_volumes_have_runtime_labels_and_bounded_tmpfs_options() {
        let spec = test_runtime_spec();
        let args = command_args(&build_managed_mount_volume_create_command(
            &spec,
            "cladding-mount-0123456789abcdef",
            Some(67_108_864),
        ));

        assert!(args.windows(2).any(|pair| pair == ["--opt", "nocopy"]));
        assert!(args.windows(2).any(|pair| pair == ["--opt", "type=tmpfs"]));
        assert!(
            args.windows(2)
                .any(|pair| pair == ["--opt", "device=tmpfs"])
        );
        assert!(
            args.windows(2)
                .any(|pair| pair == ["--opt", "o=size=67108864,mode=1777"])
        );
        assert!(
            args.windows(2)
                .any(|pair| { pair == ["--label", "cladding_resource=managed-mount"] })
        );
        assert!(
            args.windows(2).any(|pair| {
                pair == ["--label", "cladding_runtime_root=/tmp/project/.cladding"]
            })
        );
        assert_eq!(
            args.last().map(String::as_str),
            Some("cladding-mount-0123456789abcdef")
        );
    }

    #[cfg(unix)]
    #[test]
    fn runtime_socket_tree_prepares_only_host_path_socket_directories() {
        use std::os::unix::fs::PermissionsExt as _;

        let project_root =
            std::env::temp_dir().join(format!("cladding-runtime-sockets-{}", std::process::id()));
        let _ = fs::remove_dir_all(&project_root);
        fs::create_dir_all(&project_root).expect("create temporary project root");
        let config = ExecutionConfig {
            name: "demo".to_string(),
            use_runsc: false,
            agent: ExecutionComponentConfig {
                enabled: true,
                image: "agent:image".to_string(),
                build: None,
                security_opts: Vec::new(),
            },
            nw_sandbox: None,
            fs_sandbox: None,
            proxy: None,
            mounts: Vec::new(),
        };
        let spec = RuntimeSpec::build(&project_root, &config);

        prepare_runtime_socket_dirs(&spec).expect("prepare socket directories");

        for path in spec.generated_runtime_socket_dirs() {
            let metadata = fs::metadata(&path).expect("generated socket directory exists");
            assert_eq!(
                metadata.permissions().mode() & 0o777,
                0o700,
                "{}",
                path.display()
            );
        }
        assert!(project_root.join("runtime/sockets/agent/inject").is_dir());
        assert!(project_root.join("runtime/sockets/proxy/agent").is_dir());
        assert!(
            !project_root
                .join("runtime/sockets/proxy/nw-sandbox")
                .exists()
        );
        fs::remove_dir_all(project_root).expect("remove temporary project root");
    }

    #[test]
    fn baffle_ca_install_runs_as_container_root_with_system_trust_update() {
        let cmd = build_baffle_ca_install_command("demo-agent-instance");

        assert_eq!(
            command_args(&cmd),
            vec![
                "exec",
                "--user",
                "0",
                "demo-agent-instance",
                "sh",
                "-ec",
                "cp /run/cladding/ca/baffle.crt /usr/local/share/ca-certificates/baffle.crt\nupdate-ca-certificates",
            ]
        );
    }

    #[test]
    fn baffle_ca_initialization_uses_proxy_image_config_and_private_credentials_mount() {
        let proxy = RuntimeComponent {
            name: "demo-proxy".to_string(),
            use_runsc: false,
            labels: std::collections::BTreeMap::new(),
            network_name: "default".to_string(),
            containers: vec![RuntimeContainer {
                name: "demo-proxy-instance".to_string(),
                image: "localhost/cladding-proxy:latest".to_string(),
                command: Vec::new(),
                workdir: None,
                env: Vec::new(),
                mounts: Vec::new(),
                ports: Vec::new(),
                stdin: false,
                tty: false,
            }],
            user_namespace: RuntimeUserNamespace::KeepId,
            security_opts: Vec::new(),
        };
        let empty_component = |name: &str| RuntimeComponent {
            name: name.to_string(),
            use_runsc: false,
            labels: std::collections::BTreeMap::new(),
            network_name: "none".to_string(),
            containers: Vec::new(),
            user_namespace: RuntimeUserNamespace::KeepId,
            security_opts: Vec::new(),
        };
        let spec = RuntimeSpec {
            project_name: "demo".to_string(),
            project_root: "/tmp/demo/.cladding".into(),
            runtime_root: "/tmp/demo/.cladding".into(),
            use_runsc: false,
            proxy,
            agent: empty_component("demo-agent"),
            nw_sandbox: None,
            fs_sandbox: None,
        };

        let args = command_args(&build_baffle_ca_init_command(&spec));
        assert!(!args.iter().any(|arg| arg == "--init"));
        assert_eq!(
            args,
            vec![
                "run",
                "--rm",
                "--network",
                "none",
                "--userns",
                "keep-id",
                "--volume",
                "/tmp/demo/.cladding/config:/opt/config:ro",
                "--volume",
                "/tmp/demo/.cladding/credentials/baffle:/opt/credentials/baffle",
                "--volume",
                "/tmp/demo/.cladding/tools/bin/baffle:/opt/tools/bin/baffle:ro",
                "--entrypoint",
                "/opt/tools/bin/baffle",
                "localhost/cladding-proxy:latest",
                "ca",
                "init",
                "--config",
                "/opt/config/proxy/daemon.toml",
            ]
        );
    }

    #[test]
    fn baffle_ca_install_targets_agent_and_enabled_network_sandbox_only() {
        let component = |name: &str| RuntimeComponent {
            name: name.to_string(),
            use_runsc: false,
            labels: std::collections::BTreeMap::new(),
            network_name: "none".to_string(),
            containers: Vec::new(),
            user_namespace: RuntimeUserNamespace::KeepId,
            security_opts: Vec::new(),
        };
        let spec = RuntimeSpec {
            project_name: "demo".to_string(),
            project_root: "/tmp/demo/.cladding".into(),
            runtime_root: "/tmp/demo/.cladding".into(),
            use_runsc: false,
            proxy: component("demo-proxy"),
            agent: component("demo-agent"),
            nw_sandbox: Some(component("demo-nw-sandbox")),
            fs_sandbox: Some(component("demo-fs-sandbox")),
        };

        assert!(should_install_baffle_ca(&spec, &spec.agent));
        assert!(should_install_baffle_ca(
            &spec,
            spec.nw_sandbox.as_ref().expect("network sandbox")
        ));
        assert!(!should_install_baffle_ca(&spec, &spec.proxy));
        assert!(!should_install_baffle_ca(
            &spec,
            spec.fs_sandbox.as_ref().expect("filesystem sandbox")
        ));
    }

    #[test]
    fn runtime_containers_enable_podman_init_for_every_component() {
        let config = ExecutionConfig {
            name: "demo".to_string(),
            use_runsc: false,
            agent: ExecutionComponentConfig {
                enabled: true,
                image: "agent:image".to_string(),
                build: None,
                security_opts: Vec::new(),
            },
            nw_sandbox: Some(ExecutionComponentConfig {
                enabled: true,
                image: "nw-sandbox:image".to_string(),
                build: None,
                security_opts: Vec::new(),
            }),
            fs_sandbox: Some(ExecutionComponentConfig {
                enabled: true,
                image: "fs-sandbox:image".to_string(),
                build: None,
                security_opts: Vec::new(),
            }),
            proxy: None,
            mounts: Vec::new(),
        };
        let spec = RuntimeSpec::build(Path::new("/tmp/demo/.cladding"), &config);
        let components = runtime_components(&spec);

        assert_eq!(components.len(), 4);
        for component in components {
            for container in &component.containers {
                let args = command_args(&build_container_run_command(
                    component.use_runsc,
                    component,
                    container,
                ));
                assert!(
                    args.iter().any(|arg| arg == "--init"),
                    "{} did not enable Podman init",
                    container.name
                );
                assert!(
                    !args.iter().any(|arg| arg == "--pod"),
                    "{} must run as a standalone container",
                    container.name
                );
            }
        }
    }

    #[test]
    fn runtime_commands_mount_socket_volumes_only_for_channel_participants() {
        let config = ExecutionConfig {
            name: "demo".to_string(),
            use_runsc: false,
            agent: ExecutionComponentConfig {
                enabled: true,
                image: "agent:image".to_string(),
                build: None,
                security_opts: Vec::new(),
            },
            nw_sandbox: Some(ExecutionComponentConfig {
                enabled: true,
                image: "nw-sandbox:image".to_string(),
                build: None,
                security_opts: Vec::new(),
            }),
            fs_sandbox: Some(ExecutionComponentConfig {
                enabled: true,
                image: "fs-sandbox:image".to_string(),
                build: None,
                security_opts: Vec::new(),
            }),
            proxy: None,
            mounts: Vec::new(),
        };
        let spec = RuntimeSpec::build(Path::new("/tmp/project/.cladding"), &config);
        let socket_mounts = |component: &RuntimeComponent| {
            let container = component.containers.first().expect("runtime container");
            command_args(&build_container_run_command(
                component.use_runsc,
                component,
                container,
            ))
            .windows(2)
            .filter(|pair| pair[0] == "--volume" && pair[1].contains("-socket-"))
            .map(|pair| pair[1].clone())
            .collect::<Vec<_>>()
        };

        assert_eq!(
            socket_mounts(&spec.proxy),
            vec![
                "cladding-demo-socket-baffle-agent:/run/cladding/proxy/agent:U",
                "cladding-demo-socket-baffle-nw-sandbox:/run/cladding/proxy/nw-sandbox:U",
            ]
        );
        assert_eq!(
            socket_mounts(&spec.agent),
            vec![
                "cladding-demo-socket-baffle-agent:/run/cladding/proxy/agent",
                "cladding-demo-socket-run-nw-sandbox:/run/cladding/run/nw-sandbox",
                "cladding-demo-socket-run-fs-sandbox:/run/cladding/run/fs-sandbox",
            ]
        );
        let nw = spec.nw_sandbox.as_ref().expect("network sandbox");
        assert_eq!(
            socket_mounts(nw),
            vec![
                "cladding-demo-socket-baffle-nw-sandbox:/run/cladding/proxy/nw-sandbox",
                "cladding-demo-socket-run-nw-sandbox:/run/cladding/run/nw-sandbox:U",
            ]
        );
        let fs = spec.fs_sandbox.as_ref().expect("filesystem sandbox");
        assert_eq!(
            socket_mounts(fs),
            vec!["cladding-demo-socket-run-fs-sandbox:/run/cladding/run/fs-sandbox:U"]
        );

        let agent_args = command_args(&build_container_run_command(
            spec.agent.use_runsc,
            &spec.agent,
            &spec.agent.containers[0],
        ));
        assert!(agent_args.iter().any(|arg| {
            arg == "/tmp/project/.cladding/runtime/sockets/agent/inject:/run/cladding/agent/inject"
        }));
        for component in [&spec.proxy, nw, fs] {
            let args = command_args(&build_container_run_command(
                component.use_runsc,
                component,
                &component.containers[0],
            ));
            assert!(!args.iter().any(|arg| {
                arg.ends_with("/runtime/sockets/agent/inject:/run/cladding/agent/inject")
            }));
        }
    }

    fn successful_status() -> std::process::ExitStatus {
        std::process::Command::new("true").status().expect("status")
    }

    #[test]
    fn runtime_inventory_requires_every_resource_to_be_running() {
        let complete = RuntimeInventory {
            expected_count: 1,
            resources: vec![RuntimeResource {
                kind: "container",
                name: "demo-agent-instance".to_string(),
                state: "running".to_string(),
            }],
        };
        assert!(complete.is_fully_running());

        let partial = RuntimeInventory {
            expected_count: 2,
            resources: complete.resources.clone(),
        };
        assert!(!partial.is_fully_running());

        let stopped = RuntimeInventory {
            expected_count: 1,
            resources: vec![RuntimeResource {
                state: "exited".to_string(),
                ..complete.resources[0].clone()
            }],
        };
        assert!(!stopped.is_fully_running());

        let legacy_pod = RuntimeInventory {
            expected_count: 1,
            resources: vec![RuntimeResource {
                kind: "pod",
                name: "demo-proxy".to_string(),
                state: "Running".to_string(),
            }],
        };
        assert!(!legacy_pod.is_fully_running());
    }

    #[test]
    fn resource_inspection_commands_target_legacy_pods_and_containers() {
        assert_eq!(
            command_args(&build_resource_exists_command("pod", "demo-proxy")),
            vec!["pod", "exists", "demo-proxy"]
        );
        assert_eq!(
            command_args(&build_resource_inspect_command("pod", "demo-proxy")),
            vec!["pod", "inspect", "--format", "{{.State}}", "demo-proxy"]
        );
        assert_eq!(
            command_args(&build_resource_inspect_command(
                "container",
                "demo-agent-instance"
            )),
            vec![
                "container",
                "inspect",
                "--format",
                "{{.State.Status}}",
                "demo-agent-instance"
            ]
        );
    }

    #[test]
    fn remove_output_is_missing_container_matches_expected_errors() {
        let output = Output {
            status: successful_status(),
            stdout: Vec::new(),
            stderr: b"Error: no such container".to_vec(),
        };
        assert!(remove_output_is_missing_container(&output));
    }

    #[test]
    fn cleanup_requires_all_expected_component_ownership_labels() {
        let expected = std::collections::BTreeMap::from([
            ("app".to_string(), "proxy".to_string()),
            ("cladding".to_string(), "demo".to_string()),
            (
                "project_root".to_string(),
                "/tmp/demo/.cladding".to_string(),
            ),
        ]);
        let owned = serde_json::json!({
            "app": "proxy",
            "cladding": "demo",
            "project_root": "/tmp/demo/.cladding",
        });
        assert!(labels_match_expected(&owned, &expected));

        let foreign_app = serde_json::json!({
            "app": "unrelated",
            "cladding": "demo",
            "project_root": "/tmp/demo/.cladding",
        });
        assert!(!labels_match_expected(&foreign_app, &expected));

        let foreign_root = serde_json::json!({
            "app": "proxy",
            "cladding": "demo",
            "project_root": "/tmp/other/.cladding",
        });
        assert!(!labels_match_expected(&foreign_root, &expected));
    }

    #[test]
    fn managed_volume_cleanup_requires_project_runtime_and_resource_labels() {
        let spec = test_runtime_spec();
        let expected = managed_mount_volume_labels(&spec, "cladding-mount-fixture");
        let owned = serde_json::to_value(&expected).expect("serialize managed volume labels");
        assert!(labels_match_expected(&owned, &expected));

        let mut foreign_runtime = expected.clone();
        foreign_runtime.insert(
            "cladding_runtime_root".to_string(),
            "/tmp/other-runtime".to_string(),
        );
        let foreign_runtime =
            serde_json::to_value(foreign_runtime).expect("serialize foreign volume labels");
        assert!(!labels_match_expected(&foreign_runtime, &expected));

        let mut missing_resource = expected.clone();
        missing_resource.remove("cladding_resource");
        let missing_resource =
            serde_json::to_value(missing_resource).expect("serialize incomplete volume labels");
        assert!(!labels_match_expected(&missing_resource, &expected));
    }

    #[test]
    fn socket_volume_cleanup_requires_project_and_channel_labels() {
        let spec = test_runtime_spec();
        let name = "cladding-demo-socket-run-nw-sandbox";
        let expected = socket_volume_labels(&spec, name);
        let owned = serde_json::to_value(&expected).expect("serialize socket volume labels");
        assert!(labels_match_expected(&owned, &expected));

        let args = command_args(&build_socket_volume_create_command(name, &spec));
        assert!(args.windows(2).any(|pair| pair == ["--opt", "nocopy"]));
        assert!(
            args.windows(2)
                .any(|pair| { pair == ["--label", "cladding_resource=inter-container-socket"] })
        );
        assert!(args.windows(2).any(|pair| {
            pair == [
                "--label",
                "cladding_socket_volume=cladding-demo-socket-run-nw-sandbox",
            ]
        }));
        assert_eq!(args.last().map(String::as_str), Some(name));

        let mut foreign_channel = expected.clone();
        foreign_channel.insert(
            "cladding_socket_volume".to_string(),
            "cladding-demo-socket-run-fs-sandbox".to_string(),
        );
        let foreign_channel =
            serde_json::to_value(foreign_channel).expect("serialize foreign socket volume labels");
        assert!(!labels_match_expected(&foreign_channel, &expected));
    }

    #[test]
    fn baffle_socket_volume_prepare_helper_sets_private_mode_with_proxy_identity() {
        let spec = test_runtime_spec();
        let cmd =
            build_proxy_socket_volume_prepare_command("cladding-demo-socket-baffle-agent", &spec);
        let args = command_args(&cmd);

        assert!(args.windows(2).any(|pair| pair == ["--network", "none"]));
        assert!(args.windows(2).any(|pair| pair == ["--userns", "keep-id"]));
        assert!(!args.iter().any(|arg| arg == "--user"));
        assert!(
            args.windows(2).any(|pair| {
                pair == ["--volume", "cladding-demo-socket-baffle-agent:/socket:U"]
            })
        );
        assert!(
            args.windows(2)
                .any(|pair| pair == ["--entrypoint", "/bin/sh"])
        );
        assert!(
            args.iter()
                .any(|arg| arg == "localhost/cladding-proxy:latest")
        );
        assert_eq!(
            args[args.len() - 2..],
            ["-ec", "mkdir -p /socket && chmod 0700 /socket"]
        );
    }

    #[test]
    fn proxy_container_uses_default_network_and_keep_id_without_a_pod() {
        let config = ExecutionConfig {
            name: "demo".to_string(),
            use_runsc: false,
            agent: ExecutionComponentConfig {
                enabled: true,
                image: "agent:image".to_string(),
                build: None,
                security_opts: Vec::new(),
            },
            nw_sandbox: None,
            fs_sandbox: None,
            proxy: None,
            mounts: Vec::new(),
        };
        let spec = RuntimeSpec::build(Path::new("/tmp/demo/.cladding"), &config);
        let proxy = &spec.proxy;
        let container = proxy.containers.first().expect("proxy container");
        let args = command_args(&build_container_run_command(false, proxy, container));

        assert!(args.windows(2).any(|pair| pair == ["--network", "default"]));
        assert!(args.windows(2).any(|pair| pair == ["--userns", "keep-id"]));
        assert!(
            args.windows(2)
                .any(|pair| pair == ["--name", "demo-proxy-instance"])
        );
        assert!(!args.iter().any(|arg| arg == "--pod"));
        assert!(args.iter().any(|arg| arg == "app=proxy"));
    }

    #[test]
    fn build_container_run_command_uses_standalone_container_flags() {
        let component = RuntimeComponent {
            name: "demo-agent".to_string(),
            use_runsc: false,
            labels: std::collections::BTreeMap::from([
                ("app".to_string(), "agent".to_string()),
                ("cladding".to_string(), "demo".to_string()),
                (
                    "project_root".to_string(),
                    "/tmp/demo/.cladding".to_string(),
                ),
            ]),
            network_name: "none".to_string(),
            containers: Vec::new(),
            user_namespace: RuntimeUserNamespace::KeepId,
            security_opts: Vec::new(),
        };
        let container = RuntimeContainer {
            name: "demo-agent-instance".to_string(),
            image: "demo:image".to_string(),
            command: vec!["sleep".to_string(), "infinity".to_string()],
            workdir: None,
            env: Vec::new(),
            mounts: Vec::new(),
            ports: Vec::new(),
            stdin: false,
            tty: false,
        };

        let cmd = build_container_run_command(true, &component, &container);
        assert_eq!(
            command_args(&cmd),
            vec![
                "--runtime",
                "runsc",
                "--runtime-flag",
                "ignore-cgroups",
                "--runtime-flag",
                "host-uds=all",
                "--runtime-flag",
                "network=none",
                "run",
                "-d",
                "--init",
                "--label",
                "app=agent",
                "--label",
                "cladding=demo",
                "--label",
                "project_root=/tmp/demo/.cladding",
                "--network",
                "none",
                "--userns",
                "keep-id",
                "--hostname",
                "demo-agent",
                "--name",
                "demo-agent-instance",
                "--entrypoint",
                "sleep",
                "demo:image",
                "infinity",
            ]
        );
    }

    #[test]
    fn build_container_run_command_includes_mounts_env_and_io_flags() {
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
                    mount_path: "/run/cladding/proxy/agent".to_string(),
                    read_only: false,
                    source: RuntimeMountSource::NamedVolume {
                        claim_name: "demo-socket-baffle-agent".to_string(),
                    },
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

        let component = RuntimeComponent {
            name: "demo-agent".to_string(),
            use_runsc: false,
            labels: std::collections::BTreeMap::new(),
            network_name: "default".to_string(),
            containers: Vec::new(),
            user_namespace: RuntimeUserNamespace::Default,
            security_opts: Vec::new(),
        };

        let cmd = build_container_run_command(false, &component, &container);
        assert_eq!(
            command_args(&cmd),
            vec![
                "run",
                "-d",
                "--init",
                "--network",
                "default",
                "--hostname",
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
                "demo-socket-baffle-agent:/run/cladding/proxy/agent",
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
    fn container_cleanup_removes_the_container_that_owns_disposable_mounts() {
        let cmd = build_container_rm_command("demo-agent-instance");

        assert_eq!(command_args(&cmd), vec!["rm", "-f", "demo-agent-instance"]);
    }

    #[test]
    fn build_container_run_command_adds_runsc_flags_when_enabled() {
        let container = RuntimeContainer {
            name: "demo-agent-instance".to_string(),
            image: "demo:image".to_string(),
            command: vec!["sleep".to_string(), "infinity".to_string()],
            workdir: None,
            env: Vec::new(),
            mounts: Vec::new(),
            ports: Vec::new(),
            stdin: false,
            tty: false,
        };

        let component = RuntimeComponent {
            name: "demo-agent".to_string(),
            use_runsc: false,
            labels: std::collections::BTreeMap::new(),
            network_name: "default".to_string(),
            containers: Vec::new(),
            user_namespace: RuntimeUserNamespace::Default,
            security_opts: Vec::new(),
        };

        let cmd = build_container_run_command(true, &component, &container);
        assert_eq!(
            command_args(&cmd),
            vec![
                "--runtime",
                "runsc",
                "--runtime-flag",
                "ignore-cgroups",
                "--runtime-flag",
                "host-uds=all",
                "run",
                "-d",
                "--init",
                "--network",
                "default",
                "--hostname",
                "demo-agent",
                "--name",
                "demo-agent-instance",
                "--entrypoint",
                "sleep",
                "demo:image",
                "infinity",
            ]
        );
    }

    #[test]
    fn build_container_run_command_adds_network_for_standalone_containers() {
        let component = RuntimeComponent {
            name: "demo-agent".to_string(),
            use_runsc: false,
            labels: std::collections::BTreeMap::new(),
            network_name: "none".to_string(),
            containers: Vec::new(),
            user_namespace: RuntimeUserNamespace::KeepId,
            security_opts: Vec::new(),
        };
        let container = RuntimeContainer {
            name: "demo-agent-instance".to_string(),
            image: "demo:image".to_string(),
            command: vec!["sleep".to_string(), "infinity".to_string()],
            workdir: None,
            env: Vec::new(),
            mounts: Vec::new(),
            ports: Vec::new(),
            stdin: false,
            tty: false,
        };

        let cmd = build_container_run_command(true, &component, &container);
        assert_eq!(
            command_args(&cmd),
            vec![
                "--runtime",
                "runsc",
                "--runtime-flag",
                "ignore-cgroups",
                "--runtime-flag",
                "host-uds=all",
                "--runtime-flag",
                "network=none",
                "run",
                "-d",
                "--init",
                "--network",
                "none",
                "--userns",
                "keep-id",
                "--hostname",
                "demo-agent",
                "--name",
                "demo-agent-instance",
                "--entrypoint",
                "sleep",
                "demo:image",
                "infinity",
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
