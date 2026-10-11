use crate::error::{Error, Result};
use serde_json::Value;
use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};

const RUN_PREFIX: &str = "cladding-run-";

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
enum ResourceKind {
    Container,
    Volume,
}

impl ResourceKind {
    fn as_str(self) -> &'static str {
        match self {
            Self::Container => "container",
            Self::Volume => "volume",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct ResourceSnapshot {
    kind: ResourceKind,
    name: String,
    labels: BTreeMap<String, String>,
    state: Option<String>,
}

trait CleanupBackend {
    fn discover(&mut self) -> Result<Vec<ResourceSnapshot>>;
    fn inspect(&mut self, kind: ResourceKind, name: &str) -> Result<Option<ResourceSnapshot>>;
    fn remove(&mut self, kind: ResourceKind, name: &str) -> Result<()>;
}

struct PodmanCleanupBackend {
    verbose: bool,
}

impl CleanupBackend for PodmanCleanupBackend {
    fn discover(&mut self) -> Result<Vec<ResourceSnapshot>> {
        let mut resources = Vec::new();
        for kind in [ResourceKind::Container, ResourceKind::Volume] {
            let mut command = Command::new("podman");
            match kind {
                ResourceKind::Container => command.args([
                    "container",
                    "ls",
                    "--all",
                    "--filter",
                    "label=cladding",
                    "--format",
                    "json",
                ]),
                ResourceKind::Volume => command.args([
                    "volume",
                    "ls",
                    "--filter",
                    "label=cladding",
                    "--format",
                    "json",
                ]),
            };
            resources.extend(run_list_command(kind, command, self.verbose)?);
        }
        Ok(resources)
    }

    fn inspect(&mut self, kind: ResourceKind, name: &str) -> Result<Option<ResourceSnapshot>> {
        if !podman_resource_exists(kind, name, self.verbose)? {
            return Ok(None);
        }

        let labels_format = match kind {
            ResourceKind::Container => "{{json .Config.Labels}}",
            ResourceKind::Volume => "{{json .Labels}}",
        };
        let mut labels_command = Command::new("podman");
        labels_command.args([kind.as_str(), "inspect", "--format", labels_format, name]);
        let labels_output =
            run_podman_output(&mut labels_command, self.verbose, "podman inspect labels")?;
        let labels_value: Value =
            serde_json::from_slice(&labels_output.stdout).map_err(|error| {
                Error::message(format!("failed to parse labels for {name}: {error}"))
            })?;
        let labels = parse_labels(&labels_value);

        let state = if kind == ResourceKind::Container {
            let mut state_command = Command::new("podman");
            state_command.args([
                "container",
                "inspect",
                "--format",
                "{{.State.Status}}",
                name,
            ]);
            let output = run_podman_output(
                &mut state_command,
                self.verbose,
                "podman inspect container state",
            )?;
            Some(String::from_utf8_lossy(&output.stdout).trim().to_string())
                .filter(|state| !state.is_empty())
        } else {
            None
        };

        Ok(Some(ResourceSnapshot {
            kind,
            name: name.to_string(),
            labels,
            state,
        }))
    }

    fn remove(&mut self, kind: ResourceKind, name: &str) -> Result<()> {
        let mut command = Command::new("podman");
        match kind {
            // Do not force-remove a container. If it becomes live after the
            // state check, Podman must refuse the removal.
            ResourceKind::Container => command.args(["container", "rm", name]),
            ResourceKind::Volume => command.args(["volume", "rm", name]),
        };
        run_podman_output(&mut command, self.verbose, "podman resource removal").map(|_| ())
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct CleanedRun {
    owner: String,
    resources: Vec<String>,
    runtime_root: RootDisposition,
    complete: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum RootDisposition {
    Removed,
    Missing,
    NotAssociated,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct SkippedRun {
    owner: String,
    reason: String,
}

#[derive(Debug, Default)]
struct CleanupReport {
    cleaned: Vec<CleanedRun>,
    skipped_live: Vec<SkippedRun>,
    skipped_unverifiable: Vec<SkippedRun>,
    failures: Vec<String>,
}

impl CleanupReport {
    fn needs_failure_status(&self) -> bool {
        !self.skipped_unverifiable.is_empty() || !self.failures.is_empty()
    }
}

/// Remove abandoned one-off run containers, volumes, and private runtime roots.
/// Persistent project resources are excluded by the exact run-owner label.
pub fn cleanup_abandoned_runs(verbose: bool) -> Result<()> {
    super::podman_required("podman (required for cladding cleanup)")?;
    let mut backend = PodmanCleanupBackend { verbose };
    let report = cleanup_with(&mut backend, &allowed_temp_bases());
    print_report(&report);
    if report.needs_failure_status() {
        return Err(Error::message(format!(
            "cleanup could not safely finish ({} unverifiable run(s), {} failure(s))",
            report.skipped_unverifiable.len(),
            report.failures.len()
        )));
    }
    Ok(())
}

fn cleanup_with(backend: &mut impl CleanupBackend, allowed_bases: &[PathBuf]) -> CleanupReport {
    let mut report = CleanupReport::default();
    let discovered = match backend.discover() {
        Ok(resources) => resources,
        Err(error) => {
            report
                .failures
                .push(format!("resource discovery failed: {error}"));
            return report;
        }
    };

    let mut groups = BTreeMap::<String, Vec<ResourceSnapshot>>::new();
    for resource in discovered {
        let Some(owner) = resource.labels.get("cladding") else {
            continue;
        };
        if is_run_owner(owner) {
            groups.entry(owner.clone()).or_default().push(resource);
        }
    }

    for (owner, listed_resources) in groups {
        let group = match validate_group(&owner, &listed_resources, allowed_bases) {
            Ok(group) => group,
            Err(reason) => {
                report
                    .skipped_unverifiable
                    .push(SkippedRun { owner, reason });
                continue;
            }
        };

        // Refresh every discovered resource before deciding that a run is
        // abandoned. A resource that vanished after listing is already clean.
        let mut current = Vec::new();
        let mut inspect_failure = None;
        for resource in &group.resources {
            match backend.inspect(resource.kind, &resource.name) {
                Ok(Some(snapshot)) => current.push(snapshot),
                Ok(None) => {}
                Err(error) => {
                    inspect_failure = Some(format!(
                        "could not inspect {} '{}': {error}",
                        resource.kind.as_str(),
                        resource.name
                    ));
                    break;
                }
            }
        }
        if let Some(reason) = inspect_failure {
            report
                .skipped_unverifiable
                .push(SkippedRun { owner, reason });
            continue;
        }
        if let Some(reason) = current.iter().find_map(|resource| {
            validate_resource(&owner, &group.project_root, &group.root, resource).err()
        }) {
            report
                .skipped_unverifiable
                .push(SkippedRun { owner, reason });
            continue;
        }

        if let Some(live) = current.iter().find(|resource| {
            resource.kind == ResourceKind::Container && !is_stopped(resource.state.as_deref())
        }) {
            report.skipped_live.push(SkippedRun {
                owner,
                reason: format!(
                    "container '{}' has live or unknown state '{}'",
                    live.name,
                    display_state(live.state.as_deref())
                ),
            });
            continue;
        }

        // Keep the labeled Podman resources as retry evidence until the
        // private directory is removed successfully. If root removal fails,
        // this run remains untouched. If a later Podman removal fails, its
        // labels still associate the remaining resources with the missing root.
        let root_disposition =
            match remove_verified_runtime_root(group.root.as_deref(), &owner, allowed_bases) {
                Ok(disposition) => disposition,
                Err(error) => {
                    report.failures.push(format!("{owner}: {error}"));
                    continue;
                }
            };

        let mut removed_resources = Vec::new();
        let mut stop_for_safety = None;
        let mut became_live = None;
        let mut removal_failures = Vec::new();
        current.sort_by_key(|resource| match resource.kind {
            ResourceKind::Container => 0,
            ResourceKind::Volume => 1,
        });
        for resource in &current {
            let latest = match backend.inspect(resource.kind, &resource.name) {
                Ok(Some(snapshot)) => snapshot,
                Ok(None) => continue,
                Err(error) => {
                    stop_for_safety = Some(format!(
                        "could not recheck {} '{}' before removal: {error}",
                        resource.kind.as_str(),
                        resource.name
                    ));
                    break;
                }
            };
            if let Err(reason) =
                validate_resource(&owner, &group.project_root, &group.root, &latest)
            {
                stop_for_safety = Some(reason);
                break;
            }
            if latest.kind == ResourceKind::Container && !is_stopped(latest.state.as_deref()) {
                stop_for_safety = Some(format!(
                    "container '{}' became live or has unknown state '{}'",
                    latest.name,
                    display_state(latest.state.as_deref())
                ));
                break;
            }

            match backend.remove(latest.kind, &latest.name) {
                Ok(()) => {
                    removed_resources.push(format!("{} {}", latest.kind.as_str(), latest.name))
                }
                Err(error) if latest.kind == ResourceKind::Container => {
                    match backend.inspect(latest.kind, &latest.name) {
                        Ok(None) => removed_resources.push(format!(
                            "{} {}",
                            latest.kind.as_str(),
                            latest.name
                        )),
                        Ok(Some(current)) if !is_stopped(current.state.as_deref()) => {
                            became_live = Some(format!(
                                "container '{}' became live or has unknown state '{}'",
                                current.name,
                                display_state(current.state.as_deref())
                            ));
                            break;
                        }
                        Ok(Some(_)) => removal_failures.push(format!(
                            "failed to remove {} '{}': {error}",
                            latest.kind.as_str(),
                            latest.name
                        )),
                        Err(inspect_error) => {
                            stop_for_safety = Some(format!(
                                "could not verify container '{}' after removal failed: {inspect_error}",
                                latest.name
                            ));
                            break;
                        }
                    }
                }
                Err(error) => removal_failures.push(format!(
                    "failed to remove {} '{}': {error}",
                    latest.kind.as_str(),
                    latest.name
                )),
            }
        }

        if let Some(reason) = became_live {
            report.cleaned.push(CleanedRun {
                owner: owner.clone(),
                resources: removed_resources.clone(),
                runtime_root: root_disposition,
                complete: false,
            });
            report.failures.extend(
                removal_failures
                    .into_iter()
                    .map(|failure| format!("{owner}: {failure}")),
            );
            report.skipped_live.push(SkippedRun { owner, reason });
            continue;
        }
        if let Some(reason) = stop_for_safety {
            report.cleaned.push(CleanedRun {
                owner: owner.clone(),
                resources: removed_resources.clone(),
                runtime_root: root_disposition,
                complete: false,
            });
            report.failures.extend(
                removal_failures
                    .into_iter()
                    .map(|failure| format!("{owner}: {failure}")),
            );
            report
                .skipped_unverifiable
                .push(SkippedRun { owner, reason });
            continue;
        }

        if !removal_failures.is_empty() {
            report.failures.extend(
                removal_failures
                    .into_iter()
                    .map(|reason| format!("{owner}: {reason}")),
            );
            report.cleaned.push(CleanedRun {
                owner,
                resources: removed_resources,
                runtime_root: root_disposition,
                complete: false,
            });
            continue;
        }

        // A resource can appear between the initial inventory and removal.
        // Report the run as partial if the fresh inventory is not empty.
        let original_ids = group
            .resources
            .iter()
            .map(|resource| (resource.kind, resource.name.clone()))
            .collect::<BTreeSet<_>>();
        let remaining = match backend.discover() {
            Ok(resources) => resources
                .into_iter()
                .filter(|resource| {
                    resource.labels.get("cladding") == Some(&owner)
                        || original_ids.contains(&(resource.kind, resource.name.clone()))
                })
                .collect::<Vec<_>>(),
            Err(error) => {
                report
                    .failures
                    .push(format!("{owner}: final resource discovery failed: {error}"));
                report.cleaned.push(CleanedRun {
                    owner,
                    resources: removed_resources,
                    runtime_root: root_disposition,
                    complete: false,
                });
                continue;
            }
        };
        if let Some(resource) = remaining.first() {
            let reason = if resource.labels.get("cladding") == Some(&owner) {
                format!(
                    "resource {} '{}' remains after cleanup",
                    resource.kind.as_str(),
                    resource.name
                )
            } else {
                format!(
                    "resource {} '{}' changed ownership labels during cleanup",
                    resource.kind.as_str(),
                    resource.name
                )
            };
            report.failures.push(format!("{owner}: {reason}"));
            report.cleaned.push(CleanedRun {
                owner,
                resources: removed_resources,
                runtime_root: root_disposition,
                complete: false,
            });
            continue;
        }

        report.cleaned.push(CleanedRun {
            owner,
            resources: removed_resources,
            runtime_root: root_disposition,
            complete: true,
        });
    }

    report
}

struct ValidatedGroup {
    project_root: String,
    root: Option<PathBuf>,
    resources: Vec<ResourceSnapshot>,
}

fn validate_group(
    owner: &str,
    resources: &[ResourceSnapshot],
    allowed_bases: &[PathBuf],
) -> std::result::Result<ValidatedGroup, String> {
    if resources.is_empty() {
        return Err("run has no resources to verify".to_string());
    }
    let mut project_roots = BTreeSet::new();
    let mut seen = BTreeSet::new();
    for resource in resources {
        if !seen.insert((resource.kind, resource.name.clone())) {
            return Err(format!(
                "duplicate {} '{}' in inventory",
                resource.kind.as_str(),
                resource.name
            ));
        }
        if resource.name.is_empty() {
            return Err("a run resource has no verifiable name".to_string());
        }
        if resource.labels.get("cladding").map(String::as_str) != Some(owner) {
            return Err(format!(
                "resource '{}' has a mismatched run owner label",
                resource.name
            ));
        }
        let project_root = resource
            .labels
            .get("project_root")
            .filter(|value| !value.is_empty())
            .ok_or_else(|| {
                format!(
                    "resource '{}' is missing its project_root label",
                    resource.name
                )
            })?;
        project_roots.insert(project_root.clone());
        validate_resource_shape(owner, resource)?;
    }
    if project_roots.len() != 1 {
        return Err("run resources have missing or mismatched project_root labels".to_string());
    }
    let project_root = project_roots.into_iter().next().expect("one project root");

    let mut runtime_roots = BTreeSet::new();
    for resource in resources {
        if let Some(root) = resource.labels.get("cladding_runtime_root") {
            runtime_roots.insert(root.clone());
        }
        if resource.kind == ResourceKind::Volume
            && matches!(
                resource.labels.get("cladding_resource").map(String::as_str),
                Some("managed-mount")
            )
            && !resource.labels.contains_key("cladding_runtime_root")
        {
            return Err(format!(
                "managed mount volume '{}' is missing its cladding_runtime_root label",
                resource.name
            ));
        }
        if resource.kind == ResourceKind::Container
            && resource.labels.get("cladding_resource").map(String::as_str)
                == Some("managed-mount-seed-helper")
            && !resource.labels.contains_key("cladding_runtime_root")
        {
            return Err(format!(
                "seed helper '{}' is missing its cladding_runtime_root label",
                resource.name
            ));
        }
    }
    if runtime_roots.len() > 1 {
        return Err("run resources have mismatched cladding_runtime_root labels".to_string());
    }
    if resources
        .iter()
        .any(|resource| !resource.labels.contains_key("cladding_runtime_root"))
    {
        return Err("a run resource is missing its cladding_runtime_root label".to_string());
    }
    let raw_root = runtime_roots.into_iter().next().ok_or_else(|| {
        "run resources are missing their cladding_runtime_root labels".to_string()
    })?;
    let root = PathBuf::from(raw_root);
    verify_allowed_run_root(&root, owner, allowed_bases).map_err(|error| error.to_string())?;
    verify_runtime_root_path(&root).map_err(|error| error.to_string())?;
    let root = Some(root);

    for resource in resources {
        validate_resource(owner, &project_root, &root, resource)?;
    }

    Ok(ValidatedGroup {
        project_root,
        root,
        resources: resources.to_vec(),
    })
}

fn validate_resource_shape(
    owner: &str,
    resource: &ResourceSnapshot,
) -> std::result::Result<(), String> {
    let resource_type = resource.labels.get("cladding_resource").map(String::as_str);
    match resource.kind {
        ResourceKind::Container => match resource_type {
            Some("managed-mount-seed-helper") => {
                let volume = resource
                    .labels
                    .get("cladding_mount_volume")
                    .ok_or_else(|| {
                        format!(
                            "seed helper '{}' is missing its cladding_mount_volume label",
                            resource.name
                        )
                    })?;
                if resource.name != format!("{volume}-seed") {
                    return Err(format!(
                        "seed helper '{}' does not match its volume label",
                        resource.name
                    ));
                }
            }
            Some("runtime-container") => {
                let app = resource.labels.get("app").ok_or_else(|| {
                    format!("container '{}' is missing its app label", resource.name)
                })?;
                if !matches!(
                    app.as_str(),
                    "proxy" | "agent" | "nw-sandbox" | "fs-sandbox"
                ) {
                    return Err(format!(
                        "container '{}' has an unknown app label",
                        resource.name
                    ));
                }
                let expected_name = format!("{owner}-{app}-instance");
                if resource.name != expected_name {
                    return Err(format!(
                        "container '{}' does not match its owner and app labels",
                        resource.name
                    ));
                }
            }
            None => {
                return Err(format!(
                    "container '{}' is missing its cladding_resource label",
                    resource.name
                ));
            }
            Some(_) => {
                return Err(format!(
                    "container '{}' has an unknown cladding_resource label",
                    resource.name
                ));
            }
        },
        ResourceKind::Volume => match resource_type {
            Some("managed-mount") => {
                if resource
                    .labels
                    .get("cladding_mount_volume")
                    .map(String::as_str)
                    != Some(resource.name.as_str())
                {
                    return Err(format!(
                        "managed mount volume '{}' has a mismatched volume label",
                        resource.name
                    ));
                }
            }
            Some("inter-container-socket") => {
                if resource
                    .labels
                    .get("cladding_socket_volume")
                    .map(String::as_str)
                    != Some(resource.name.as_str())
                {
                    return Err(format!(
                        "socket volume '{}' has a mismatched volume label",
                        resource.name
                    ));
                }
            }
            Some(_) => {
                return Err(format!(
                    "volume '{}' has an unknown cladding_resource label",
                    resource.name
                ));
            }
            None => {
                return Err(format!(
                    "volume '{}' is missing its cladding_resource label",
                    resource.name
                ));
            }
        },
    }
    Ok(())
}

fn validate_resource(
    owner: &str,
    project_root: &str,
    runtime_root: &Option<PathBuf>,
    resource: &ResourceSnapshot,
) -> std::result::Result<(), String> {
    validate_resource_shape(owner, resource)?;
    if resource.labels.get("cladding").map(String::as_str) != Some(owner) {
        return Err(format!(
            "resource '{}' has a mismatched run owner label",
            resource.name
        ));
    }
    if resource.labels.get("project_root").map(String::as_str) != Some(project_root) {
        return Err(format!(
            "resource '{}' has a mismatched project_root label",
            resource.name
        ));
    }
    let label_root = resource
        .labels
        .get("cladding_runtime_root")
        .ok_or_else(|| {
            format!(
                "resource '{}' is missing its cladding_runtime_root label",
                resource.name
            )
        })?;
    if runtime_root.as_deref() != Some(Path::new(label_root)) {
        return Err(format!(
            "resource '{}' has a mismatched cladding_runtime_root label",
            resource.name
        ));
    }
    Ok(())
}

fn is_run_owner(owner: &str) -> bool {
    let Some(uuid) = owner.strip_prefix(RUN_PREFIX) else {
        return false;
    };
    let bytes = uuid.as_bytes();
    bytes.len() == 36
        && bytes.iter().enumerate().all(|(index, byte)| match index {
            8 | 13 | 18 | 23 => *byte == b'-',
            _ => byte.is_ascii_hexdigit(),
        })
}

fn is_stopped(state: Option<&str>) -> bool {
    matches!(
        state.map(str::trim).map(str::to_ascii_lowercase).as_deref(),
        Some("exited" | "stopped")
    )
}

fn display_state(state: Option<&str>) -> String {
    state
        .filter(|state| !state.trim().is_empty())
        .unwrap_or("unknown")
        .to_string()
}

fn is_allowed_run_root(root: &Path, owner: &str, allowed_bases: &[PathBuf]) -> bool {
    root.file_name().and_then(|name| name.to_str()) == Some(owner)
        && allowed_bases.iter().any(|base| root == base.join(owner))
}

fn verify_allowed_run_root(
    root: &Path,
    owner: &str,
    allowed_bases: &[PathBuf],
) -> std::result::Result<(), Error> {
    if !root.is_absolute() || !is_allowed_run_root(root, owner, allowed_bases) {
        return Err(Error::message(format!(
            "private runtime root {} is outside the allowed temporary bases or does not match the run owner",
            root.display()
        )));
    }
    let parent = root
        .parent()
        .ok_or_else(|| Error::message("private runtime root has no parent directory"))?;
    let canonical_parent = parent.canonicalize().map_err(|error| {
        Error::message(format!(
            "failed to verify temporary base {}: {error}",
            parent.display()
        ))
    })?;
    if canonical_parent != parent {
        return Err(Error::message(format!(
            "temporary base {} changed through a symbolic link",
            parent.display()
        )));
    }
    Ok(())
}

fn verify_runtime_root_path(root: &Path) -> std::result::Result<(), Error> {
    let metadata = match fs::symlink_metadata(root) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(error) => {
            return Err(Error::message(format!(
                "failed to inspect private runtime root {}: {error}",
                root.display()
            )));
        }
    };
    if metadata.file_type().is_symlink() || !metadata.is_dir() {
        return Err(Error::message(format!(
            "private runtime root {} is not a regular directory",
            root.display()
        )));
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt as _;
        let expected_uid = unsafe { libc::geteuid() };
        if metadata.uid() != expected_uid || metadata.mode() & 0o077 != 0 {
            return Err(Error::message(format!(
                "private runtime root {} is not owned by this user with private permissions",
                root.display()
            )));
        }
    }
    Ok(())
}

fn remove_verified_runtime_root(
    root: Option<&Path>,
    owner: &str,
    allowed_bases: &[PathBuf],
) -> std::result::Result<RootDisposition, Error> {
    let Some(root) = root else {
        return Ok(RootDisposition::NotAssociated);
    };
    verify_allowed_run_root(root, owner, allowed_bases)?;
    match fs::symlink_metadata(root) {
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            return Ok(RootDisposition::Missing);
        }
        Err(error) => {
            return Err(Error::message(format!(
                "failed to inspect private runtime root {}: {error}",
                root.display()
            )));
        }
        Ok(_) => {}
    }
    verify_runtime_root_path(root)?;
    fs::remove_dir_all(root).map_err(|error| {
        Error::message(format!(
            "failed to remove private runtime root {}: {error}",
            root.display()
        ))
    })?;
    Ok(RootDisposition::Removed)
}

fn allowed_temp_bases() -> Vec<PathBuf> {
    let mut bases = BTreeSet::new();
    for base in [
        std::env::temp_dir(),
        PathBuf::from("/var/tmp"),
        PathBuf::from("/dev/shm"),
    ] {
        if let Ok(canonical) = base.canonicalize()
            && canonical.is_dir()
        {
            bases.insert(canonical);
        }
    }
    bases.into_iter().collect()
}

fn print_report(report: &CleanupReport) {
    for run in &report.cleaned {
        println!(
            "{} {}: {}{}",
            if run.complete {
                "cleaned"
            } else {
                "partially cleaned"
            },
            run.owner,
            if run.resources.is_empty() {
                "no Podman resources".to_string()
            } else {
                run.resources.join(", ")
            },
            match run.runtime_root {
                RootDisposition::Removed => "; private runtime root removed",
                RootDisposition::Missing => "; private runtime root already missing",
                RootDisposition::NotAssociated => "",
            }
        );
    }
    for run in &report.skipped_live {
        println!("skipped {}: {}", run.owner, run.reason);
    }
    for run in &report.skipped_unverifiable {
        eprintln!("error: skipped {}: {}", run.owner, run.reason);
    }
    for failure in &report.failures {
        eprintln!("error: {failure}");
    }
    println!(
        "cladding cleanup: {} cleaned, {} partial, {} live/unsafe skipped, {} unverifiable skipped, {} failures",
        report.cleaned.iter().filter(|run| run.complete).count(),
        report.cleaned.iter().filter(|run| !run.complete).count(),
        report.skipped_live.len(),
        report.skipped_unverifiable.len(),
        report.failures.len()
    );
}

fn run_list_command(
    kind: ResourceKind,
    mut command: Command,
    verbose: bool,
) -> Result<Vec<ResourceSnapshot>> {
    let output = run_podman_output(&mut command, verbose, "podman resource listing")?;
    let value: Value = serde_json::from_slice(&output.stdout).map_err(|error| {
        Error::message(format!(
            "failed to parse Podman {} list: {error}",
            kind.as_str()
        ))
    })?;
    let items = value.as_array().ok_or_else(|| {
        Error::message(format!(
            "Podman {} list did not return a JSON array",
            kind.as_str()
        ))
    })?;
    let mut resources = Vec::new();
    for item in items {
        let labels = parse_labels(item.get("Labels").unwrap_or(&Value::Null));
        let name = match kind {
            ResourceKind::Container => parse_container_name(item),
            ResourceKind::Volume => item
                .get("Name")
                .and_then(Value::as_str)
                .unwrap_or_default()
                .trim_start_matches('/')
                .to_string(),
        };
        let state = match kind {
            ResourceKind::Container => item
                .get("State")
                .or_else(|| item.get("Status"))
                .and_then(Value::as_str)
                .map(str::to_string),
            ResourceKind::Volume => None,
        };
        resources.push(ResourceSnapshot {
            kind,
            name,
            labels,
            state,
        });
    }
    Ok(resources)
}

fn parse_container_name(item: &Value) -> String {
    let names = item.get("Names");
    match names {
        Some(Value::Array(names)) => names.first().and_then(Value::as_str).unwrap_or_default(),
        Some(Value::String(name)) => name,
        _ => item.get("Name").and_then(Value::as_str).unwrap_or_default(),
    }
    .trim_start_matches('/')
    .to_string()
}

fn parse_labels(value: &Value) -> BTreeMap<String, String> {
    let mut labels = BTreeMap::new();
    match value {
        Value::Object(map) => {
            for (key, value) in map {
                if let Some(value) = value.as_str() {
                    labels.insert(key.clone(), value.to_string());
                }
            }
        }
        Value::String(raw) => {
            for entry in raw.split(',') {
                let Some((key, value)) = entry.trim().split_once('=') else {
                    continue;
                };
                if !key.trim().is_empty() {
                    labels.insert(key.trim().to_string(), value.trim().to_string());
                }
            }
        }
        _ => {}
    }
    labels
}

fn podman_resource_exists(kind: ResourceKind, name: &str, verbose: bool) -> Result<bool> {
    let mut command = Command::new("podman");
    command.args([kind.as_str(), "exists", name]);
    trace_command(&command, verbose);
    let output = command.output().map_err(|error| {
        Error::message(format!(
            "failed to check {} '{name}': {error}",
            kind.as_str()
        ))
    })?;
    match output.status.code() {
        Some(0) => Ok(true),
        Some(1) => Ok(false),
        _ => Err(output_error(&output, "podman exists")),
    }
}

fn run_podman_output(
    command: &mut Command,
    verbose: bool,
    context: &'static str,
) -> Result<Output> {
    trace_command(command, verbose);
    let output = command
        .output()
        .map_err(|error| Error::message(format!("failed to run {context}: {error}")))?;
    if !output.status.success() {
        return Err(output_error(&output, context));
    }
    Ok(output)
}

fn output_error(output: &Output, context: &'static str) -> Error {
    let stderr = String::from_utf8_lossy(&output.stderr).trim().to_string();
    if stderr.is_empty() {
        Error::message(format!(
            "{context} failed with exit code {:?}",
            output.status.code()
        ))
    } else {
        Error::message(format!("{context} failed: {stderr}"))
    }
}

fn trace_command(command: &Command, verbose: bool) {
    if verbose {
        super::command::trace_command(command, true);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};

    const OWNER: &str = "cladding-run-12345678-1234-4234-8234-123456789abc";
    static ROOT_COUNTER: AtomicUsize = AtomicUsize::new(0);

    #[derive(Default)]
    struct FakeBackend {
        resources: BTreeMap<(ResourceKind, String), ResourceSnapshot>,
        removed: Vec<(ResourceKind, String)>,
        fail_once: BTreeSet<(ResourceKind, String)>,
    }

    impl CleanupBackend for FakeBackend {
        fn discover(&mut self) -> Result<Vec<ResourceSnapshot>> {
            Ok(self.resources.values().cloned().collect())
        }

        fn inspect(&mut self, kind: ResourceKind, name: &str) -> Result<Option<ResourceSnapshot>> {
            Ok(self.resources.get(&(kind, name.to_string())).cloned())
        }

        fn remove(&mut self, kind: ResourceKind, name: &str) -> Result<()> {
            let key = (kind, name.to_string());
            if self.fail_once.remove(&key) {
                return Err(Error::message("injected removal failure"));
            }
            self.resources.remove(&key);
            self.removed.push(key);
            Ok(())
        }
    }

    fn temp_base(label: &str) -> PathBuf {
        let id = ROOT_COUNTER.fetch_add(1, Ordering::Relaxed);
        let base = std::env::temp_dir().join(format!(
            "cladding-cleanup-{label}-{}-{id}",
            std::process::id()
        ));
        fs::create_dir_all(&base).unwrap();
        fs::canonicalize(base).unwrap()
    }

    fn make_private_root(base: &Path) -> PathBuf {
        let root = base.join(OWNER);
        fs::create_dir(&root).unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt as _;
            fs::set_permissions(&root, fs::Permissions::from_mode(0o700)).unwrap();
        }
        fs::write(root.join("secret-override"), "fixture").unwrap();
        root
    }

    fn container(name: &str, app: &str, state: &str, base: &Path) -> ResourceSnapshot {
        ResourceSnapshot {
            kind: ResourceKind::Container,
            name: name.to_string(),
            labels: BTreeMap::from([
                ("app".to_string(), app.to_string()),
                ("cladding".to_string(), OWNER.to_string()),
                (
                    "cladding_resource".to_string(),
                    "runtime-container".to_string(),
                ),
                (
                    "cladding_runtime_root".to_string(),
                    base.join(OWNER).display().to_string(),
                ),
                (
                    "project_root".to_string(),
                    "/missing/project/.cladding".to_string(),
                ),
            ]),
            state: Some(state.to_string()),
        }
    }

    fn socket_volume(name: &str, base: &Path) -> ResourceSnapshot {
        ResourceSnapshot {
            kind: ResourceKind::Volume,
            name: name.to_string(),
            labels: BTreeMap::from([
                ("cladding".to_string(), OWNER.to_string()),
                (
                    "cladding_resource".to_string(),
                    "inter-container-socket".to_string(),
                ),
                ("cladding_socket_volume".to_string(), name.to_string()),
                (
                    "cladding_runtime_root".to_string(),
                    base.join(OWNER).display().to_string(),
                ),
                (
                    "project_root".to_string(),
                    "/missing/project/.cladding".to_string(),
                ),
            ]),
            state: None,
        }
    }

    fn managed_volume(name: &str, base: &Path) -> ResourceSnapshot {
        ResourceSnapshot {
            kind: ResourceKind::Volume,
            name: name.to_string(),
            labels: BTreeMap::from([
                ("cladding".to_string(), OWNER.to_string()),
                ("cladding_mount_volume".to_string(), name.to_string()),
                ("cladding_resource".to_string(), "managed-mount".to_string()),
                (
                    "cladding_runtime_root".to_string(),
                    base.join(OWNER).display().to_string(),
                ),
                (
                    "project_root".to_string(),
                    "/missing/project/.cladding".to_string(),
                ),
            ]),
            state: None,
        }
    }

    fn helper(name: &str, volume: &str, state: &str, base: &Path) -> ResourceSnapshot {
        ResourceSnapshot {
            kind: ResourceKind::Container,
            name: name.to_string(),
            labels: BTreeMap::from([
                ("cladding".to_string(), OWNER.to_string()),
                ("cladding_mount_volume".to_string(), volume.to_string()),
                (
                    "cladding_resource".to_string(),
                    "managed-mount-seed-helper".to_string(),
                ),
                (
                    "cladding_runtime_root".to_string(),
                    base.join(OWNER).display().to_string(),
                ),
                (
                    "project_root".to_string(),
                    "/missing/project/.cladding".to_string(),
                ),
            ]),
            state: Some(state.to_string()),
        }
    }

    fn backend_with(resources: Vec<ResourceSnapshot>) -> FakeBackend {
        FakeBackend {
            resources: resources
                .into_iter()
                .map(|resource| ((resource.kind, resource.name.clone()), resource))
                .collect(),
            ..FakeBackend::default()
        }
    }

    #[test]
    fn groups_only_exact_uuid_run_owners() {
        assert!(is_run_owner(OWNER));
        assert!(!is_run_owner("cladding-run-not-a-uuid"));
        assert!(!is_run_owner(
            "cladding-run-12345678-1234-4234-8234-123456789abc-extra"
        ));

        let base = temp_base("group");
        let mut backend = backend_with(vec![
            container(&format!("{OWNER}-agent-instance"), "agent", "exited", &base),
            ResourceSnapshot {
                kind: ResourceKind::Container,
                name: "demo-agent-instance".to_string(),
                labels: BTreeMap::from([
                    ("cladding".to_string(), "demo".to_string()),
                    ("project_root".to_string(), "/project/.cladding".to_string()),
                ]),
                state: Some("running".to_string()),
            },
        ]);
        let report = cleanup_with(&mut backend, std::slice::from_ref(&base));
        assert_eq!(report.cleaned.len(), 1);
        assert_eq!(report.skipped_live.len(), 0);
        assert_eq!(backend.resources.len(), 1);
        assert!(
            backend
                .resources
                .values()
                .any(|resource| resource.name == "demo-agent-instance")
        );
        let _ = fs::remove_dir_all(base.join(OWNER));
        let _ = fs::remove_dir_all(base);
    }

    #[test]
    fn persistent_runtime_with_a_run_shaped_name_is_preserved_without_private_root_labels() {
        let owner = OWNER;
        let mut backend = backend_with(vec![ResourceSnapshot {
            kind: ResourceKind::Container,
            name: format!("{owner}-agent-instance"),
            labels: BTreeMap::from([
                ("app".to_string(), "agent".to_string()),
                ("cladding".to_string(), owner.to_string()),
                (
                    "project_root".to_string(),
                    "/persistent/.cladding".to_string(),
                ),
            ]),
            state: Some("exited".to_string()),
        }]);
        let base = temp_base("persistent-run-shaped");

        let report = cleanup_with(&mut backend, std::slice::from_ref(&base));

        assert_eq!(report.skipped_unverifiable.len(), 1);
        assert!(report.needs_failure_status());
        assert_eq!(backend.resources.len(), 1);
        assert!(backend.removed.is_empty());
        let _ = fs::remove_dir_all(base);
    }

    #[test]
    fn live_or_unknown_container_state_skips_the_entire_run() {
        let base = temp_base("live");
        let root = make_private_root(&base);
        let volume = socket_volume("run-socket", &base);
        let mut backend = backend_with(vec![
            container(
                &format!("{OWNER}-agent-instance"),
                "agent",
                "running",
                &base,
            ),
            volume.clone(),
        ]);
        let report = cleanup_with(&mut backend, std::slice::from_ref(&base));
        assert_eq!(report.skipped_live.len(), 1);
        assert_eq!(backend.resources.len(), 2);
        assert!(root.exists());

        let mut unknown = backend_with(vec![
            ResourceSnapshot {
                state: None,
                ..container(&format!("{OWNER}-agent-instance"), "agent", "exited", &base)
            },
            volume,
        ]);
        let report = cleanup_with(&mut unknown, std::slice::from_ref(&base));
        assert_eq!(report.skipped_live.len(), 1);
        assert_eq!(unknown.resources.len(), 2);
        let _ = fs::remove_dir_all(root);
        let _ = fs::remove_dir_all(base);
    }

    #[test]
    fn strict_label_mismatches_preserve_every_resource_in_the_run() {
        let base = temp_base("labels");
        let root = make_private_root(&base);
        let mut volume = managed_volume("managed-data", &base);
        volume.labels.insert(
            "cladding_runtime_root".to_string(),
            "/tmp/foreign-run".to_string(),
        );
        let mut backend = backend_with(vec![
            container(&format!("{OWNER}-agent-instance"), "agent", "exited", &base),
            volume,
        ]);
        let report = cleanup_with(&mut backend, std::slice::from_ref(&base));
        assert_eq!(report.skipped_unverifiable.len(), 1);
        assert_eq!(backend.resources.len(), 2);
        assert!(root.exists());
        let _ = fs::remove_dir_all(root);
        let _ = fs::remove_dir_all(base);
    }

    #[test]
    fn cleans_containers_helpers_volumes_and_the_verified_private_root() {
        let base = temp_base("complete");
        let root = make_private_root(&base);
        let managed = "managed-data";
        let resources = vec![
            container(&format!("{OWNER}-agent-instance"), "agent", "exited", &base),
            helper(&format!("{managed}-seed"), managed, "stopped", &base),
            managed_volume(managed, &base),
            socket_volume("run-socket", &base),
        ];
        let mut backend = backend_with(resources);
        let report = cleanup_with(&mut backend, std::slice::from_ref(&base));
        assert_eq!(report.cleaned.len(), 1);
        assert_eq!(report.cleaned[0].resources.len(), 4);
        assert_eq!(report.cleaned[0].runtime_root, RootDisposition::Removed);
        assert!(backend.resources.is_empty());
        assert!(!root.exists());
        let _ = fs::remove_dir_all(base);
    }

    #[test]
    fn invalid_or_symlink_runtime_roots_are_preserved() {
        let base = temp_base("bad-root");
        let root = make_private_root(&base);
        let mut agent = container(&format!("{OWNER}-agent-instance"), "agent", "exited", &base);
        agent.labels.insert(
            "cladding_runtime_root".to_string(),
            base.join("nested").join(OWNER).display().to_string(),
        );
        let mut backend = backend_with(vec![agent]);
        let report = cleanup_with(&mut backend, std::slice::from_ref(&base));
        assert_eq!(report.skipped_unverifiable.len(), 1);
        assert!(root.exists());

        let _ = fs::remove_dir_all(root);
        let _ = fs::remove_dir_all(base);

        let base = temp_base("symlink-root");
        let target = base.join("target");
        fs::create_dir(&target).unwrap();
        #[cfg(unix)]
        std::os::unix::fs::symlink(&target, base.join(OWNER)).unwrap();
        let mut backend = backend_with(vec![container(
            &format!("{OWNER}-agent-instance"),
            "agent",
            "exited",
            &base,
        )]);
        let report = cleanup_with(&mut backend, std::slice::from_ref(&base));
        assert_eq!(report.skipped_unverifiable.len(), 1);
        assert!(target.exists());
        let _ = fs::remove_file(base.join(OWNER));
        let _ = fs::remove_dir_all(target);
        let _ = fs::remove_dir_all(base);
    }

    #[test]
    fn partial_failure_keeps_the_root_and_a_retry_finishes_cleanup() {
        let base = temp_base("retry");
        let root = make_private_root(&base);
        let failed_volume = "managed-data";
        let mut backend = backend_with(vec![
            container(&format!("{OWNER}-agent-instance"), "agent", "exited", &base),
            managed_volume(failed_volume, &base),
            socket_volume("run-socket", &base),
        ]);
        backend
            .fail_once
            .insert((ResourceKind::Volume, failed_volume.to_string()));

        let report = cleanup_with(&mut backend, std::slice::from_ref(&base));
        assert_eq!(report.failures.len(), 1);
        assert!(!root.exists());
        assert_eq!(backend.resources.len(), 1);

        let retry = cleanup_with(&mut backend, std::slice::from_ref(&base));
        assert_eq!(retry.failures.len(), 0);
        assert_eq!(retry.cleaned.len(), 1);
        assert_eq!(retry.cleaned[0].resources, ["volume managed-data"]);
        assert_eq!(retry.cleaned[0].runtime_root, RootDisposition::Missing);
        assert!(backend.resources.is_empty());
        assert!(!root.exists());
        let _ = fs::remove_dir_all(base);
    }

    #[test]
    fn no_candidates_is_a_successful_empty_cleanup() {
        let base = temp_base("none");
        let mut backend = backend_with(vec![ResourceSnapshot {
            kind: ResourceKind::Container,
            name: "demo-agent-instance".to_string(),
            labels: BTreeMap::from([
                ("cladding".to_string(), "demo".to_string()),
                ("project_root".to_string(), "/project/.cladding".to_string()),
            ]),
            state: Some("running".to_string()),
        }]);
        let report = cleanup_with(&mut backend, std::slice::from_ref(&base));
        assert!(report.cleaned.is_empty());
        assert!(report.skipped_live.is_empty());
        assert!(!report.needs_failure_status());
        assert_eq!(backend.resources.len(), 1);
        let _ = fs::remove_dir_all(base);
    }

    #[test]
    fn label_parsing_supports_podman_json_forms() {
        let object = parse_labels(&serde_json::json!({ "cladding": OWNER, "project_root": "/p" }));
        assert_eq!(object.get("cladding").map(String::as_str), Some(OWNER));
        let string = parse_labels(&Value::String(format!("cladding={OWNER},project_root=/p")));
        assert_eq!(string.get("project_root").map(String::as_str), Some("/p"));
    }
}
