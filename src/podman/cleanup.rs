use crate::error::{Error, Result};
use crate::runtime::{
    RunIdentityGuard, preserve_run_identity, read_run_identity, read_sidecar_identity,
    remove_run_identity_sidecar, try_lock_run_identity,
};
use serde_json::Value;
use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};

const RUN_PREFIX: &str = "cladding-run-";
const RUN_IDENTITY_SIDECAR_SUFFIX: &str = ".cladding-run-identity";

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
    Preserved,
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
    let (mut private_roots, skipped_roots) = match discover_private_run_roots(allowed_bases) {
        Ok(discovery) => discovery,
        Err(error) => {
            report
                .failures
                .push(format!("private runtime root discovery failed: {error}"));
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
    let resource_owners = groups.keys().cloned().collect::<BTreeSet<_>>();
    let mut skipped_roots = skipped_roots
        .into_iter()
        .map(|skipped| (skipped.owner.clone(), skipped))
        .collect::<BTreeMap<_, _>>();
    private_roots.retain(|root| !skipped_roots.contains_key(&root.owner));

    'runs: for (owner, listed_resources) in groups {
        if let Some(skipped) = skipped_roots.remove(&owner) {
            report.skipped_unverifiable.push(skipped);
            continue;
        }
        let mut group = match validate_group(&owner, &listed_resources, allowed_bases) {
            Ok(group) => group,
            Err(reason) => {
                report
                    .skipped_unverifiable
                    .push(SkippedRun { owner, reason });
                continue;
            }
        };

        let root_path = group
            .root
            .as_deref()
            .expect("validated run has a runtime root");
        let root_exists = match fs::symlink_metadata(root_path) {
            Ok(_) => true,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => false,
            Err(error) => {
                report.skipped_unverifiable.push(SkippedRun {
                    owner,
                    reason: format!("could not inspect private runtime root: {error}"),
                });
                continue;
            }
        };
        if root_exists {
            group.identity_guard = match try_lock_run_identity(root_path, &owner, &group.identity) {
                Ok(Some(guard)) => Some(guard),
                Ok(None) => {
                    report.skipped_live.push(SkippedRun {
                        owner,
                        reason: "private runtime root is locked by an active run".to_string(),
                    });
                    continue;
                }
                Err(error) => {
                    report.skipped_unverifiable.push(SkippedRun {
                        owner,
                        reason: format!("could not claim private runtime root: {error:#}"),
                    });
                    continue;
                }
            };
        }
        if let Err(error) = verify_runtime_root_identity(
            group
                .root
                .as_deref()
                .expect("validated run has a runtime root"),
            &owner,
            &group.identity,
        ) {
            report.skipped_unverifiable.push(SkippedRun {
                owner,
                reason: error.to_string(),
            });
            continue;
        }

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

        // Refresh inventory before any deletion so a newly appeared container
        // can make the whole run ineligible for cleanup.
        let listed_ids = group
            .resources
            .iter()
            .map(|resource| (resource.kind, resource.name.clone()))
            .collect::<BTreeSet<_>>();
        let mut known_ids = current
            .iter()
            .map(|resource| (resource.kind, resource.name.clone()))
            .collect::<BTreeSet<_>>();
        let refreshed = match backend.discover() {
            Ok(resources) => resources,
            Err(error) => {
                report.skipped_unverifiable.push(SkippedRun {
                    owner: owner.clone(),
                    reason: format!("could not refresh run inventory: {error}"),
                });
                continue 'runs;
            }
        };
        for resource in refreshed.into_iter().filter(|resource| {
            resource.labels.get("cladding") == Some(&owner)
                || listed_ids.contains(&(resource.kind, resource.name.clone()))
        }) {
            if !known_ids.insert((resource.kind, resource.name.clone())) {
                continue;
            }
            match backend.inspect(resource.kind, &resource.name) {
                Ok(Some(snapshot)) => current.push(snapshot),
                Ok(None) => {}
                Err(error) => {
                    report.skipped_unverifiable.push(SkippedRun {
                        owner: owner.clone(),
                        reason: format!(
                            "could not inspect newly discovered {} '{}': {error}",
                            resource.kind.as_str(),
                            resource.name
                        ),
                    });
                    continue 'runs;
                }
            }
        }
        if let Some(reason) = current.iter().find_map(|resource| {
            validate_resource(
                &owner,
                &group.project_root,
                &group.root,
                &group.identity,
                resource,
            )
            .err()
        }) {
            report
                .skipped_unverifiable
                .push(SkippedRun { owner, reason });
            continue 'runs;
        }

        for resource in current
            .iter()
            .filter(|resource| resource.kind == ResourceKind::Container)
        {
            match container_state(resource.state.as_deref()) {
                ContainerState::Stopped => {}
                ContainerState::Live => {
                    report.skipped_live.push(SkippedRun {
                        owner: owner.clone(),
                        reason: format!(
                            "container '{}' has live state '{}'",
                            resource.name,
                            display_state(resource.state.as_deref())
                        ),
                    });
                    continue 'runs;
                }
                ContainerState::Unknown => {
                    report.skipped_unverifiable.push(SkippedRun {
                        owner: owner.clone(),
                        reason: format!(
                            "container '{}' has unverifiable state '{}'",
                            resource.name,
                            display_state(resource.state.as_deref())
                        ),
                    });
                    continue 'runs;
                }
            }
        }

        // Recheck every container as a set before the first removal. This
        // avoids retiring one stopped container when another has become live.
        for resource in current
            .iter()
            .filter(|resource| resource.kind == ResourceKind::Container)
        {
            let latest = match backend.inspect(resource.kind, &resource.name) {
                Ok(Some(snapshot)) => snapshot,
                Ok(None) => continue,
                Err(error) => {
                    report.skipped_unverifiable.push(SkippedRun {
                        owner: owner.clone(),
                        reason: format!(
                            "could not recheck container '{}' before cleanup: {error}",
                            resource.name
                        ),
                    });
                    continue 'runs;
                }
            };
            if let Err(reason) = validate_resource(
                &owner,
                &group.project_root,
                &group.root,
                &group.identity,
                &latest,
            ) {
                report.skipped_unverifiable.push(SkippedRun {
                    owner: owner.clone(),
                    reason,
                });
                continue 'runs;
            }
            match container_state(latest.state.as_deref()) {
                ContainerState::Stopped => {}
                ContainerState::Live => {
                    report.skipped_live.push(SkippedRun {
                        owner: owner.clone(),
                        reason: format!(
                            "container '{}' became live with state '{}'",
                            latest.name,
                            display_state(latest.state.as_deref())
                        ),
                    });
                    continue 'runs;
                }
                ContainerState::Unknown => {
                    report.skipped_unverifiable.push(SkippedRun {
                        owner: owner.clone(),
                        reason: format!(
                            "container '{}' has unverifiable state '{}' before cleanup",
                            latest.name,
                            display_state(latest.state.as_deref())
                        ),
                    });
                    continue 'runs;
                }
            }
        }

        // Keep the private root as retry evidence until Podman resources are
        // retired and the final inventory confirms that none appeared.
        let mut root_disposition = RootDisposition::Preserved;
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
            if let Err(reason) = validate_resource(
                &owner,
                &group.project_root,
                &group.root,
                &group.identity,
                &latest,
            ) {
                stop_for_safety = Some(reason);
                break;
            }
            if latest.kind == ResourceKind::Container {
                match container_state(latest.state.as_deref()) {
                    ContainerState::Stopped => {}
                    ContainerState::Live => {
                        became_live = Some(format!(
                            "container '{}' became live with state '{}'",
                            latest.name,
                            display_state(latest.state.as_deref())
                        ));
                        break;
                    }
                    ContainerState::Unknown => {
                        stop_for_safety = Some(format!(
                            "container '{}' has unverifiable state '{}' before removal",
                            latest.name,
                            display_state(latest.state.as_deref())
                        ));
                        break;
                    }
                }
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
                        Ok(Some(current)) => match container_state(current.state.as_deref()) {
                            ContainerState::Live => {
                                became_live = Some(format!(
                                    "container '{}' became live with state '{}'",
                                    current.name,
                                    display_state(current.state.as_deref())
                                ));
                                break;
                            }
                            ContainerState::Unknown => {
                                stop_for_safety = Some(format!(
                                    "container '{}' has unverifiable state '{}' after removal failed",
                                    current.name,
                                    display_state(current.state.as_deref())
                                ));
                                break;
                            }
                            ContainerState::Stopped => removal_failures.push(format!(
                                "failed to remove {} '{}': {error}",
                                latest.kind.as_str(),
                                latest.name
                            )),
                        },
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
        let original_ids = current
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
            if resource.kind == ResourceKind::Container {
                match backend.inspect(resource.kind, &resource.name) {
                    Ok(Some(snapshot)) => {
                        if let Err(reason) = validate_resource(
                            &owner,
                            &group.project_root,
                            &group.root,
                            &group.identity,
                            &snapshot,
                        ) {
                            report.skipped_unverifiable.push(SkippedRun {
                                owner: owner.clone(),
                                reason,
                            });
                            report.cleaned.push(CleanedRun {
                                owner,
                                resources: removed_resources,
                                runtime_root: RootDisposition::Preserved,
                                complete: false,
                            });
                            continue;
                        }
                        match container_state(snapshot.state.as_deref()) {
                            ContainerState::Live => report.skipped_live.push(SkippedRun {
                                owner: owner.clone(),
                                reason: format!(
                                    "new associated container '{}' has live state '{}'",
                                    snapshot.name,
                                    display_state(snapshot.state.as_deref())
                                ),
                            }),
                            ContainerState::Unknown => {
                                report.skipped_unverifiable.push(SkippedRun {
                                    owner: owner.clone(),
                                    reason: format!(
                                        "new associated container '{}' has unverifiable state '{}'",
                                        snapshot.name,
                                        display_state(snapshot.state.as_deref())
                                    ),
                                })
                            }
                            ContainerState::Stopped => {}
                        }
                    }
                    Ok(None) => {}
                    Err(error) => report.skipped_unverifiable.push(SkippedRun {
                        owner: owner.clone(),
                        reason: format!(
                            "could not verify newly discovered container '{}': {error}",
                            resource.name
                        ),
                    }),
                }
                if report
                    .skipped_live
                    .last()
                    .is_some_and(|run| run.owner == owner)
                    || report
                        .skipped_unverifiable
                        .last()
                        .is_some_and(|run| run.owner == owner)
                {
                    report.cleaned.push(CleanedRun {
                        owner,
                        resources: removed_resources,
                        runtime_root: RootDisposition::Preserved,
                        complete: false,
                    });
                    continue;
                }
            }
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

        root_disposition = match remove_verified_runtime_root(
            group.root.as_deref(),
            &owner,
            &group.identity,
            allowed_bases,
            group.identity_guard.as_ref(),
        ) {
            Ok(disposition) => disposition,
            Err(error) => {
                report.failures.push(format!("{owner}: {error}"));
                report.cleaned.push(CleanedRun {
                    owner,
                    resources: removed_resources,
                    runtime_root: RootDisposition::Preserved,
                    complete: false,
                });
                continue;
            }
        };

        report.cleaned.push(CleanedRun {
            owner,
            resources: removed_resources,
            runtime_root: root_disposition,
            complete: true,
        });
    }

    report
        .skipped_unverifiable
        .extend(skipped_roots.into_values());

    // A process can be killed after creating its private runtime root but
    // before Podman creates any labeled resources. Its private marker is the
    // durable ownership record for this otherwise resource-free retry.
    for root in private_roots {
        if resource_owners.contains(&root.owner) {
            continue;
        }
        let resources = match backend.discover() {
            Ok(resources) => resources,
            Err(error) => {
                report.failures.push(format!(
                    "{}: could not verify Podman resources before removing its private root: {error}",
                    root.owner
                ));
                continue;
            }
        };
        if resources
            .iter()
            .any(|resource| resource.labels.get("cladding") == Some(&root.owner))
        {
            report.skipped_unverifiable.push(SkippedRun {
                owner: root.owner,
                reason: "run resources appeared after inventory; rerun cleanup".to_string(),
            });
            continue;
        }
        let root_guard = match try_lock_run_identity(&root.path, &root.owner, &root.identity) {
            Ok(Some(guard)) => guard,
            Ok(None) => {
                report.skipped_live.push(SkippedRun {
                    owner: root.owner,
                    reason: "private runtime root is locked by an active run".to_string(),
                });
                continue;
            }
            Err(error) => {
                let sidecar = root
                    .path
                    .parent()
                    .expect("validated runtime root has a parent")
                    .join(format!(".{}{RUN_IDENTITY_SIDECAR_SUFFIX}", root.owner));
                let root_gone = matches!(
                    fs::symlink_metadata(&root.path),
                    Err(ref error) if error.kind() == std::io::ErrorKind::NotFound
                );
                let sidecar_gone = matches!(
                    fs::symlink_metadata(&sidecar),
                    Err(ref error) if error.kind() == std::io::ErrorKind::NotFound
                );
                if root_gone && sidecar_gone {
                    report.cleaned.push(CleanedRun {
                        owner: root.owner,
                        resources: Vec::new(),
                        runtime_root: RootDisposition::Missing,
                        complete: true,
                    });
                    continue;
                }
                report.skipped_unverifiable.push(SkippedRun {
                    owner: root.owner,
                    reason: format!("could not claim private runtime root: {error:#}"),
                });
                continue;
            }
        };
        match remove_verified_runtime_root(
            Some(&root.path),
            &root.owner,
            &root.identity,
            allowed_bases,
            Some(&root_guard),
        ) {
            Ok(disposition) => report.cleaned.push(CleanedRun {
                owner: root.owner,
                resources: Vec::new(),
                runtime_root: disposition,
                complete: true,
            }),
            Err(error) => report.failures.push(format!("{}: {error}", root.owner)),
        }
    }

    report
}

struct ValidatedGroup {
    project_root: String,
    root: Option<PathBuf>,
    identity: String,
    resources: Vec<ResourceSnapshot>,
    identity_guard: Option<RunIdentityGuard>,
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
    let mut runtime_identities = BTreeSet::new();
    for resource in resources {
        if let Some(root) = resource.labels.get("cladding_runtime_root") {
            runtime_roots.insert(root.clone());
        }
        if let Some(identity) = resource.labels.get("cladding_runtime_identity") {
            runtime_identities.insert(identity.clone());
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
    if runtime_identities.len() > 1 {
        return Err("run resources have mismatched cladding_runtime_identity labels".to_string());
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
    let identity = runtime_identities.into_iter().next().ok_or_else(|| {
        "run resources are missing their cladding_runtime_identity labels".to_string()
    })?;
    if !is_uuid(&identity) {
        return Err("run resources have an invalid cladding_runtime_identity label".to_string());
    }
    let root = PathBuf::from(raw_root);
    verify_allowed_run_root(&root, owner, allowed_bases).map_err(|error| error.to_string())?;
    verify_runtime_root_path(&root).map_err(|error| error.to_string())?;
    verify_runtime_root_identity(&root, owner, &identity).map_err(|error| error.to_string())?;
    let root = Some(root);

    for resource in resources {
        validate_resource(owner, &project_root, &root, &identity, resource)?;
    }

    Ok(ValidatedGroup {
        project_root,
        root,
        identity,
        resources: resources.to_vec(),
        identity_guard: None,
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
    runtime_identity: &str,
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
    if resource
        .labels
        .get("cladding_runtime_identity")
        .map(String::as_str)
        != Some(runtime_identity)
    {
        return Err(format!(
            "resource '{}' has a mismatched cladding_runtime_identity label",
            resource.name
        ));
    }
    if let Some(root) = runtime_root {
        verify_runtime_root_identity(root, owner, runtime_identity)
            .map_err(|error| error.to_string())?;
    }
    Ok(())
}

fn is_run_owner(owner: &str) -> bool {
    let Some(uuid) = owner.strip_prefix(RUN_PREFIX) else {
        return false;
    };
    is_uuid(uuid)
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ContainerState {
    Stopped,
    Live,
    Unknown,
}

fn container_state(state: Option<&str>) -> ContainerState {
    match state.map(str::trim).map(str::to_ascii_lowercase).as_deref() {
        Some("exited" | "stopped") => ContainerState::Stopped,
        Some(
            "running" | "paused" | "restarting" | "starting" | "created" | "initialized"
            | "configured" | "stopping" | "removing",
        ) => ContainerState::Live,
        _ => ContainerState::Unknown,
    }
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

fn is_uuid(value: &str) -> bool {
    let bytes = value.as_bytes();
    bytes.len() == 36
        && bytes.iter().enumerate().all(|(index, byte)| match index {
            8 | 13 | 18 | 23 => *byte == b'-',
            _ => byte.is_ascii_hexdigit(),
        })
}

fn verify_runtime_root_identity(
    root: &Path,
    owner: &str,
    identity: &str,
) -> std::result::Result<(), Error> {
    match read_run_identity(root) {
        Ok(None) => Ok(()),
        Ok(Some(metadata)) if metadata.owner == owner && metadata.id == identity => Ok(()),
        Ok(Some(_)) => Err(Error::message(format!(
            "private runtime root {} has a mismatched run identity",
            root.display()
        ))),
        Err(error) => Err(Error::message(format!(
            "failed to verify private runtime root identity at {}: {error}",
            root.display()
        ))),
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct PrivateRunRoot {
    owner: String,
    identity: String,
    path: PathBuf,
}

fn discover_private_run_roots(
    allowed_bases: &[PathBuf],
) -> std::result::Result<(Vec<PrivateRunRoot>, Vec<SkippedRun>), Error> {
    let mut roots = BTreeMap::<String, PrivateRunRoot>::new();
    let mut skipped = Vec::new();
    for base in allowed_bases {
        let entries = fs::read_dir(base).map_err(|error| {
            Error::message(format!(
                "failed to list temporary base {}: {error}",
                base.display()
            ))
        })?;
        for entry in entries {
            let entry = entry.map_err(|error| {
                Error::message(format!(
                    "failed to read an entry under temporary base {}: {error}",
                    base.display()
                ))
            })?;
            let Some(entry_name) = entry.file_name().to_str().map(str::to_string) else {
                continue;
            };
            if is_run_owner(&entry_name) {
                let path = entry.path();
                let metadata = match fs::symlink_metadata(&path) {
                    Ok(metadata) => metadata,
                    Err(_) => continue,
                };
                if metadata.file_type().is_symlink() || !metadata.is_dir() {
                    continue;
                }
                let identity = match read_run_identity(&path) {
                    Ok(Some(identity)) if identity.owner == entry_name && is_uuid(&identity.id) => {
                        identity
                    }
                    _ => continue,
                };
                if verify_allowed_run_root(&path, &entry_name, allowed_bases).is_err()
                    || verify_runtime_root_path(&path).is_err()
                {
                    continue;
                }
                roots.insert(
                    entry_name.clone(),
                    PrivateRunRoot {
                        owner: entry_name,
                        identity: identity.id,
                        path,
                    },
                );
                continue;
            }

            let Some(owner) = entry_name
                .strip_prefix('.')
                .and_then(|name| name.strip_suffix(RUN_IDENTITY_SIDECAR_SUFFIX))
                .filter(|owner| is_run_owner(owner))
                .map(str::to_string)
            else {
                continue;
            };
            let path = base.join(&owner);
            let identity = match read_sidecar_identity(&path) {
                Ok(Some(identity)) if identity.owner == owner && is_uuid(&identity.id) => identity,
                Ok(Some(_)) => {
                    roots.remove(&owner);
                    skipped.push(SkippedRun {
                        owner,
                        reason: "private runtime retry marker has a mismatched owner or identity"
                            .to_string(),
                    });
                    continue;
                }
                Ok(None) => continue,
                Err(error) => {
                    roots.remove(&owner);
                    skipped.push(SkippedRun {
                        owner,
                        reason: format!("could not read private runtime retry marker: {error:#}"),
                    });
                    continue;
                }
            };
            match fs::symlink_metadata(&path) {
                Ok(metadata) if metadata.file_type().is_symlink() || !metadata.is_dir() => {
                    roots.remove(&owner);
                    skipped.push(SkippedRun {
                        owner,
                        reason: "private runtime root is not a regular directory".to_string(),
                    });
                    continue;
                }
                Ok(_) => match read_run_identity(&path) {
                    Ok(Some(root_identity))
                        if root_identity.owner == identity.owner
                            && root_identity.id == identity.id => {}
                    Ok(Some(_)) => {
                        roots.remove(&owner);
                        skipped.push(SkippedRun {
                            owner,
                            reason: "private runtime root does not match its retry marker"
                                .to_string(),
                        });
                        continue;
                    }
                    Ok(None) => {
                        roots.remove(&owner);
                        skipped.push(SkippedRun {
                            owner,
                            reason: "private runtime root has no verifiable run identity"
                                .to_string(),
                        });
                        continue;
                    }
                    Err(error) => {
                        roots.remove(&owner);
                        skipped.push(SkippedRun {
                            owner,
                            reason: format!("private runtime root is unverifiable: {error:#}"),
                        });
                        continue;
                    }
                },
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
                Err(error) => {
                    roots.remove(&owner);
                    skipped.push(SkippedRun {
                        owner,
                        reason: format!("could not inspect private runtime root: {error}"),
                    });
                    continue;
                }
            }
            if let Err(error) = verify_allowed_run_root(&path, &owner, allowed_bases)
                .and_then(|()| verify_runtime_root_path(&path))
            {
                roots.remove(&owner);
                skipped.push(SkippedRun {
                    owner,
                    reason: error.to_string(),
                });
                continue;
            }
            roots.insert(
                owner.clone(),
                PrivateRunRoot {
                    owner,
                    identity: identity.id,
                    path,
                },
            );
        }
    }
    Ok((roots.into_values().collect(), skipped))
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
    identity: &str,
    allowed_bases: &[PathBuf],
    guard: Option<&RunIdentityGuard>,
) -> std::result::Result<RootDisposition, Error> {
    let Some(root) = root else {
        return Ok(RootDisposition::NotAssociated);
    };
    verify_allowed_run_root(root, owner, allowed_bases)?;
    let root_exists = match fs::symlink_metadata(root) {
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => false,
        Err(error) => {
            return Err(Error::message(format!(
                "failed to inspect private runtime root {}: {error}",
                root.display()
            )));
        }
        Ok(_) => true,
    };
    if !root_exists {
        verify_runtime_root_identity(root, owner, identity)?;
        remove_run_identity_sidecar(root, owner, identity).map_err(|error| {
            Error::message(format!(
                "failed to remove run identity sidecar for {}: {error:#}",
                root.display()
            ))
        })?;
        return Ok(RootDisposition::Missing);
    }
    if guard.is_none() {
        return Err(Error::message(format!(
            "private runtime root {} appeared without a cleanup lock",
            root.display()
        )));
    }
    verify_runtime_root_path(root)?;
    verify_runtime_root_identity(root, owner, identity)?;
    preserve_run_identity(root, owner, identity).map_err(|error| {
        Error::message(format!(
            "failed to preserve run identity for {}: {error:#}",
            root.display()
        ))
    })?;
    fs::remove_dir_all(root).map_err(|error| {
        Error::message(format!(
            "failed to remove private runtime root {}: {error}",
            root.display()
        ))
    })?;
    remove_run_identity_sidecar(root, owner, identity).map_err(|error| {
        Error::message(format!(
            "failed to remove run identity sidecar for {}: {error:#}",
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
                RootDisposition::Preserved => "; private runtime root preserved",
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
    use crate::runtime::{create_run_identity_guard, preserve_run_identity, write_run_identity};
    use std::sync::atomic::{AtomicUsize, Ordering};

    const OWNER: &str = "cladding-run-12345678-1234-4234-8234-123456789abc";
    const IDENTITY: &str = "fedcba98-7654-4321-8765-cba987654321";
    static ROOT_COUNTER: AtomicUsize = AtomicUsize::new(0);

    #[derive(Default)]
    struct FakeBackend {
        resources: BTreeMap<(ResourceKind, String), ResourceSnapshot>,
        removed: Vec<(ResourceKind, String)>,
        fail_once: BTreeSet<(ResourceKind, String)>,
        inspect_counts: BTreeMap<(ResourceKind, String), usize>,
        state_changes_on_inspect: BTreeMap<(ResourceKind, String), (usize, String)>,
        inspect_failures: BTreeSet<(ResourceKind, String, usize)>,
        add_on_discover: BTreeMap<usize, Vec<ResourceSnapshot>>,
        discover_count: usize,
        replace_root_on_inspect: Option<(ResourceKind, String, usize, PathBuf)>,
    }

    impl CleanupBackend for FakeBackend {
        fn discover(&mut self) -> Result<Vec<ResourceSnapshot>> {
            self.discover_count += 1;
            if let Some(additions) = self.add_on_discover.remove(&self.discover_count) {
                for resource in additions {
                    self.resources
                        .insert((resource.kind, resource.name.clone()), resource);
                }
            }
            Ok(self.resources.values().cloned().collect())
        }

        fn inspect(&mut self, kind: ResourceKind, name: &str) -> Result<Option<ResourceSnapshot>> {
            let key = (kind, name.to_string());
            let count = self.inspect_counts.entry(key.clone()).or_default();
            *count += 1;
            let inspection = *count;
            if self
                .inspect_failures
                .remove(&(kind, name.to_string(), inspection))
            {
                return Err(Error::message("injected inspection failure"));
            }
            if let Some((at, state)) = self.state_changes_on_inspect.get(&key)
                && *at == inspection
                && let Some(resource) = self.resources.get_mut(&key)
            {
                resource.state = Some(state.clone());
            }
            if let Some((target_kind, target_name, at, root)) = &self.replace_root_on_inspect
                && *target_kind == kind
                && target_name == name
                && *at == inspection
            {
                fs::remove_dir_all(root).unwrap();
                fs::create_dir(root).unwrap();
                #[cfg(unix)]
                {
                    use std::os::unix::fs::PermissionsExt as _;
                    fs::set_permissions(root, fs::Permissions::from_mode(0o700)).unwrap();
                }
                fs::write(root.join("unrelated"), "collision").unwrap();
            }
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
        write_run_identity(&root, OWNER, IDENTITY).unwrap();
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
                    "cladding_runtime_identity".to_string(),
                    IDENTITY.to_string(),
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
                    "cladding_runtime_identity".to_string(),
                    IDENTITY.to_string(),
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
                    "cladding_runtime_identity".to_string(),
                    IDENTITY.to_string(),
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
                    "cladding_runtime_identity".to_string(),
                    IDENTITY.to_string(),
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
        assert_eq!(report.skipped_unverifiable.len(), 1);
        assert!(report.needs_failure_status());
        assert_eq!(unknown.resources.len(), 2);

        for state in [Some(""), Some("unknown-ish")] {
            let mut uncertain = backend_with(vec![
                ResourceSnapshot {
                    state: state.map(str::to_string),
                    ..container(&format!("{OWNER}-agent-instance"), "agent", "exited", &base)
                },
                socket_volume("run-socket", &base),
            ]);
            let report = cleanup_with(&mut uncertain, std::slice::from_ref(&base));
            assert_eq!(report.skipped_unverifiable.len(), 1);
            assert!(report.needs_failure_status());
            assert_eq!(uncertain.resources.len(), 2);
        }
        let _ = fs::remove_dir_all(root);
        let _ = fs::remove_dir_all(base);
    }

    #[test]
    fn container_that_becomes_live_on_final_inspection_keeps_the_run_root_and_resources() {
        let base = temp_base("late-live");
        let root = make_private_root(&base);
        let container_name = format!("{OWNER}-agent-instance");
        let mut backend = backend_with(vec![
            container(&container_name, "agent", "exited", &base),
            socket_volume("run-socket", &base),
        ]);
        backend.state_changes_on_inspect.insert(
            (ResourceKind::Container, container_name.clone()),
            (2, "running".to_string()),
        );

        let report = cleanup_with(&mut backend, std::slice::from_ref(&base));

        assert_eq!(report.skipped_live.len(), 1);
        assert_eq!(backend.resources.len(), 2);
        assert!(backend.removed.is_empty());
        assert!(root.exists());
        let _ = fs::remove_dir_all(base);
    }

    #[test]
    fn failed_final_container_inspection_preserves_the_entire_run() {
        let base = temp_base("inspect-failure");
        let root = make_private_root(&base);
        let container_name = format!("{OWNER}-agent-instance");
        let mut backend = backend_with(vec![
            container(&container_name, "agent", "exited", &base),
            socket_volume("run-socket", &base),
        ]);
        backend
            .inspect_failures
            .insert((ResourceKind::Container, container_name, 2));

        let report = cleanup_with(&mut backend, std::slice::from_ref(&base));

        assert_eq!(report.skipped_unverifiable.len(), 1);
        assert!(report.needs_failure_status());
        assert_eq!(backend.resources.len(), 2);
        assert!(backend.removed.is_empty());
        assert!(root.exists());
        let _ = fs::remove_dir_all(base);
    }

    #[test]
    fn a_new_live_container_found_during_refresh_leaves_all_run_resources_untouched() {
        let base = temp_base("new-container");
        let root = make_private_root(&base);
        let agent = container(&format!("{OWNER}-agent-instance"), "agent", "exited", &base);
        let volume = socket_volume("run-socket", &base);
        let new_container = container(
            &format!("{OWNER}-proxy-instance"),
            "proxy",
            "running",
            &base,
        );
        let mut backend = backend_with(vec![agent, volume]);
        backend
            .add_on_discover
            .insert(2, vec![new_container.clone()]);

        let report = cleanup_with(&mut backend, std::slice::from_ref(&base));

        assert_eq!(report.skipped_live.len(), 1);
        assert_eq!(backend.resources.len(), 3);
        assert!(backend.removed.is_empty());
        assert!(root.exists());
        let _ = fs::remove_dir_all(base);
    }

    #[test]
    fn replaced_runtime_directory_does_not_match_the_recorded_run_identity() {
        let base = temp_base("root-collision");
        let root = make_private_root(&base);
        let container_name = format!("{OWNER}-agent-instance");
        let mut backend = backend_with(vec![
            container(&container_name, "agent", "exited", &base),
            socket_volume("run-socket", &base),
        ]);
        backend.replace_root_on_inspect =
            Some((ResourceKind::Container, container_name, 2, root.clone()));

        let report = cleanup_with(&mut backend, std::slice::from_ref(&base));

        assert_eq!(report.skipped_unverifiable.len(), 1);
        assert!(report.needs_failure_status());
        assert_eq!(backend.resources.len(), 2);
        assert!(backend.removed.is_empty());
        assert!(root.join("unrelated").exists());
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
    fn mismatched_runtime_identity_label_preserves_the_entire_run() {
        let base = temp_base("identity-label");
        let root = make_private_root(&base);
        let mut volume = socket_volume("run-socket", &base);
        volume.labels.insert(
            "cladding_runtime_identity".to_string(),
            "00000000-0000-4000-8000-000000000000".to_string(),
        );
        let mut backend = backend_with(vec![
            container(&format!("{OWNER}-agent-instance"), "agent", "exited", &base),
            volume,
        ]);

        let report = cleanup_with(&mut backend, std::slice::from_ref(&base));

        assert_eq!(report.skipped_unverifiable.len(), 1);
        assert!(report.needs_failure_status());
        assert_eq!(backend.resources.len(), 2);
        assert!(backend.removed.is_empty());
        assert!(root.exists());
        let _ = fs::remove_dir_all(base);
    }

    #[test]
    fn cleanup_retries_a_marked_private_root_without_podman_resources() {
        let base = temp_base("root-only");
        let root = make_private_root(&base);
        let mut backend = backend_with(Vec::new());

        let report = cleanup_with(&mut backend, std::slice::from_ref(&base));

        assert_eq!(report.cleaned.len(), 1);
        assert!(report.cleaned[0].complete);
        assert!(report.cleaned[0].resources.is_empty());
        assert_eq!(report.cleaned[0].runtime_root, RootDisposition::Removed);
        assert!(!root.exists());
        let _ = fs::remove_dir_all(base);
    }

    #[test]
    fn cleanup_preserves_a_replacement_directory_with_a_stale_sidecar() {
        let base = temp_base("replacement-root");
        let root = make_private_root(&base);
        let sidecar = preserve_run_identity(&root, OWNER, IDENTITY).unwrap();
        fs::remove_dir_all(&root).unwrap();

        fs::create_dir(&root).unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt as _;
            fs::set_permissions(&root, fs::Permissions::from_mode(0o700)).unwrap();
        }
        let unrelated = root.join("unrelated-secret");
        fs::write(&unrelated, "keep this replacement directory").unwrap();
        let mut backend = backend_with(Vec::new());

        let report = cleanup_with(&mut backend, std::slice::from_ref(&base));

        assert_eq!(report.skipped_unverifiable.len(), 1);
        assert!(report.needs_failure_status());
        assert!(report.cleaned.is_empty());
        assert_eq!(
            fs::read_to_string(&unrelated).unwrap(),
            "keep this replacement directory"
        );
        assert!(sidecar.exists());

        let mut backend = backend_with(vec![
            container(&format!("{OWNER}-agent-instance"), "agent", "exited", &base),
            socket_volume("run-socket", &base),
        ]);
        let report = cleanup_with(&mut backend, std::slice::from_ref(&base));

        assert_eq!(report.skipped_unverifiable.len(), 1);
        assert!(backend.removed.is_empty());
        assert_eq!(backend.resources.len(), 2);
        assert_eq!(
            fs::read_to_string(&unrelated).unwrap(),
            "keep this replacement directory"
        );
        let _ = fs::remove_dir_all(base);
    }

    #[test]
    fn cleanup_skips_a_run_held_before_its_first_podman_resource() {
        let base = temp_base("active-startup");
        let root = base.join(OWNER);
        fs::create_dir(&root).unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt as _;
            fs::set_permissions(&root, fs::Permissions::from_mode(0o700)).unwrap();
        }
        // This guard represents the creator between private-root setup and
        // the first Podman resource. Its lifetime matches the production run.
        let creator_guard = create_run_identity_guard(&root, OWNER, IDENTITY).unwrap();
        fs::write(root.join("startup-state"), "still starting").unwrap();
        let mut backend = backend_with(Vec::new());

        let report = cleanup_with(&mut backend, std::slice::from_ref(&base));

        assert_eq!(report.skipped_live.len(), 1);
        assert!(report.cleaned.is_empty());
        assert!(root.join("startup-state").exists());
        drop(creator_guard);

        let retry = cleanup_with(&mut backend, std::slice::from_ref(&base));
        assert_eq!(retry.cleaned.len(), 1);
        assert!(retry.cleaned[0].complete);
        assert!(!root.exists());
        assert!(
            !base
                .join(format!(".{OWNER}.cladding-run-identity"))
                .exists()
        );
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
        assert!(root.exists());
        assert_eq!(backend.resources.len(), 1);

        let retry = cleanup_with(&mut backend, std::slice::from_ref(&base));
        assert_eq!(retry.failures.len(), 0);
        assert_eq!(retry.cleaned.len(), 1);
        assert_eq!(retry.cleaned[0].resources, ["volume managed-data"]);
        assert_eq!(retry.cleaned[0].runtime_root, RootDisposition::Removed);
        assert!(backend.resources.is_empty());
        assert!(!root.exists());
        let _ = fs::remove_dir_all(base);
    }

    #[cfg(unix)]
    #[test]
    fn failed_private_root_removal_keeps_identity_for_retry_after_repair() {
        if unsafe { libc::geteuid() } == 0 {
            // Root can traverse mode-0500 directories, so this failure cannot
            // be exercised reliably under a privileged test runner.
            return;
        }
        use std::os::unix::fs::PermissionsExt as _;

        let base = temp_base("root-content-retry");
        let root = make_private_root(&base);
        let blocked = root.join("blocked");
        fs::create_dir(&blocked).unwrap();
        fs::write(blocked.join("private"), "fixture").unwrap();
        fs::set_permissions(&blocked, fs::Permissions::from_mode(0o500)).unwrap();
        let sidecar = base.join(format!(".{OWNER}.cladding-run-identity"));
        let mut backend = backend_with(Vec::new());

        let first = cleanup_with(&mut backend, std::slice::from_ref(&base));

        assert_eq!(first.failures.len(), 1);
        assert!(root.exists());
        assert!(sidecar.exists());
        // Model the failure point reproduced in review: the root marker may
        // already have been removed when recursive deletion hits a child it
        // cannot remove. The sibling hard link remains authoritative.
        let marker = root.join(".cladding-run-identity");
        if marker.exists() {
            fs::remove_file(&marker).unwrap();
        }
        assert!(
            read_run_identity(&root)
                .unwrap()
                .is_some_and(|identity| identity.owner == OWNER && identity.id == IDENTITY)
        );

        fs::set_permissions(&blocked, fs::Permissions::from_mode(0o700)).unwrap();
        let retry = cleanup_with(&mut backend, std::slice::from_ref(&base));

        assert!(retry.failures.is_empty());
        assert_eq!(retry.cleaned.len(), 1);
        assert!(retry.cleaned[0].complete);
        assert!(!root.exists());
        assert!(!sidecar.exists());
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
