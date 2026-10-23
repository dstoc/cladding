use super::control::{self, ControlStatus};
use cladding::error::{Error, Result};
use cladding::fs_utils::canonicalize_path;
use cladding::podman::{DiscoveredRun, list_discovered_runs, select_run};
use std::path::{Path, PathBuf};

#[derive(Debug, Clone)]
pub(super) struct SelectedRun {
    pub(super) run: DiscoveredRun,
    pub(super) control: Option<ControlStatus>,
}

pub(super) fn resolve_run(
    selector: Option<&str>,
    project_root: Option<&Path>,
    allow_unreachable_supervisor: bool,
) -> Result<SelectedRun> {
    let runs = list_discovered_runs(false, false)?;
    let run = if let Some(selector) = selector {
        select_run(&runs, selector)?.clone()
    } else {
        let project_root = project_root
            .ok_or_else(|| Error::message("implicit run selection requires a project directory"))?;
        let project_identity = canonical_or_original(project_root);
        let matches = runs
            .iter()
            .filter(|run| {
                run.project_root
                    .as_deref()
                    .is_some_and(|root| canonical_or_original(Path::new(root)) == project_identity)
            })
            .collect::<Vec<_>>();
        match matches.as_slice() {
            [run] => (*run).clone(),
            [] => {
                return Err(Error::message(format!(
                    "no cladding run belongs to project {}",
                    project_root.display()
                )));
            }
            _ => {
                return Err(Error::message(format!(
                    "multiple cladding runs belong to project {}; use --run to select one",
                    project_root.display()
                )));
            }
        }
    };

    if let Some(reason) = &run.malformed {
        return Err(Error::message(format!(
            "run '{}' has malformed or incompatible metadata: {reason}",
            run.uuid
        )));
    }
    if run.metadata_version.as_deref() != Some("1") {
        return Err(Error::message(format!(
            "run '{}' does not have compatible run metadata",
            run.uuid
        )));
    }
    let socket = verified_control_socket_path(&run)?;
    let control = match control::status(&socket, &run.uuid) {
        Ok(status)
            if valid_expected_roles(&status.expected_roles)
                && same_expected_roles(run.expected_roles.as_deref(), &status.expected_roles) =>
        {
            Some(status)
        }
        Ok(_) => {
            return Err(Error::message(format!(
                "run '{}' supervisor returned invalid component metadata",
                run.uuid
            )));
        }
        Err(_) if allow_unreachable_supervisor => None,
        Err(error) => {
            return Err(Error::message(format!(
                "run '{}' supervisor state is unknown or unreachable: {error}",
                run.uuid
            )));
        }
    };
    Ok(SelectedRun { run, control })
}

pub(super) fn verified_control_socket_path(run: &DiscoveredRun) -> Result<PathBuf> {
    let socket = run.control_socket.as_deref().ok_or_else(|| {
        Error::message(format!(
            "run '{}' has no supervisor control endpoint",
            run.uuid
        ))
    })?;
    let project_root = run.project_root.as_deref().ok_or_else(|| {
        Error::message(format!(
            "run '{}' has no project identity for its supervisor endpoint",
            run.uuid
        ))
    })?;
    let project_root = Path::new(project_root);
    if !project_root.is_absolute()
        || project_root
            .file_name()
            .is_none_or(|name| name != ".cladding")
    {
        return Err(Error::message(format!(
            "run '{}' has an invalid project identity for its supervisor endpoint",
            run.uuid
        )));
    }
    let runtime_root = project_root.join("runtime/runs").join(&run.name);
    let expected = control::expected_control_socket_path(&runtime_root, &run.uuid);
    if Path::new(socket) != expected {
        return Err(Error::message(format!(
            "run '{}' supervisor endpoint does not match its private runtime path",
            run.uuid
        )));
    }
    Ok(expected)
}

pub(super) fn same_expected_roles(recorded: Option<&str>, supervisor: &[String]) -> bool {
    let Some(recorded) = recorded else {
        return false;
    };
    let mut recorded = recorded.split(',').collect::<Vec<_>>();
    let mut supervisor = supervisor.iter().map(String::as_str).collect::<Vec<_>>();
    recorded.sort_unstable();
    supervisor.sort_unstable();
    recorded == supervisor
}

fn canonical_or_original(path: &Path) -> PathBuf {
    canonicalize_path(path).unwrap_or_else(|_| path.to_path_buf())
}

fn valid_expected_roles(roles: &[String]) -> bool {
    let allowed = ["agent", "proxy", "nw-sandbox", "fs-sandbox"];
    let mut observed = std::collections::HashSet::new();
    !roles.is_empty()
        && roles
            .iter()
            .all(|role| allowed.contains(&role.as_str()) && observed.insert(role))
}

impl SelectedRun {
    pub(super) fn uuid(&self) -> &str {
        &self.run.uuid
    }

    pub(super) fn component(&self, role: &str) -> Result<&cladding::podman::RunComponent> {
        if !self.expects_role(role) {
            return Err(Error::message(format!(
                "target '{role}' is disabled for run {}",
                self.run.uuid
            )));
        }
        let component = self.run.components.iter().find(|component| component.role == role)
            .ok_or_else(|| Error::message(format!("target '{role}' is missing for run {}; expected components do not match observed resources", self.run.uuid)))?;
        if !component.state.eq_ignore_ascii_case("running") {
            return Err(Error::message(format!(
                "target '{role}' for run {} is {}",
                self.run.uuid, component.state
            )));
        }
        if component.id.is_none() {
            return Err(Error::message(format!(
                "target '{role}' for run {} has no immutable container ID",
                self.run.uuid
            )));
        }
        Ok(component)
    }

    pub(super) fn expects_role(&self, role: &str) -> bool {
        self.run
            .expected_roles
            .as_deref()
            .is_some_and(|roles| roles.split(',').any(|expected| expected == role))
            && self.control.as_ref().is_none_or(|control| {
                control
                    .expected_roles
                    .iter()
                    .any(|expected| expected == role)
            })
    }

    pub(super) fn require_ready(&self) -> Result<()> {
        let Some(control) = &self.control else {
            return Err(Error::message(format!(
                "run {} supervisor state is unknown or unreachable",
                self.run.uuid
            )));
        };
        if control.state != "ready" {
            return Err(Error::message(format!(
                "run {} is {} and cannot accept this operation",
                self.run.uuid, control.state
            )));
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn supervisor_roles_must_match_resource_metadata() {
        assert!(same_expected_roles(
            Some("proxy,agent,nw-sandbox"),
            &[
                "nw-sandbox".to_string(),
                "agent".to_string(),
                "proxy".to_string(),
            ]
        ));
        assert!(!same_expected_roles(
            Some("agent,proxy"),
            &["agent".to_string()]
        ));
        assert!(!same_expected_roles(None, &["agent".to_string()]));
    }

    #[test]
    fn supervisor_endpoint_must_match_the_private_run_path() {
        let project_root = std::env::temp_dir().join("selection-project/.cladding");
        let run = DiscoveredRun {
            uuid: "aabbccdd-1111-4222-8333-123456789abc".to_string(),
            name: "cladding-run-aabbccdd-1111-4222-8333-123456789abc".to_string(),
            project_root: Some(project_root.display().to_string()),
            metadata_version: Some("1".to_string()),
            control_socket: Some(
                control::expected_control_socket_path(
                    &project_root
                        .join("runtime/runs/cladding-run-aabbccdd-1111-4222-8333-123456789abc"),
                    "aabbccdd-1111-4222-8333-123456789abc",
                )
                .display()
                .to_string(),
            ),
            expected_roles: Some("agent,proxy".to_string()),
            components: Vec::new(),
            helper_containers: Vec::new(),
            volumes: Vec::new(),
            malformed: None,
        };

        assert!(verified_control_socket_path(&run).is_ok());
        let mut redirected = run;
        redirected.control_socket = Some("/tmp/unrelated.sock".to_string());
        assert!(verified_control_socket_path(&redirected).is_err());
    }
}
