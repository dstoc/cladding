use super::args::CommandSpec;
use cladding::config::ExecutionConfig;
use cladding::error::{Error, Result};
use cladding::fs_utils::canonicalize_path;
use cladding::podman::list_running_projects;
use std::path::{Path, PathBuf};

#[derive(Debug, Clone)]
pub(super) struct Context {
    pub(super) project_root: PathBuf,
}

fn find_project_root(start: &Path) -> Option<PathBuf> {
    let mut current = start;
    loop {
        let candidate = current.join(".cladding");
        if candidate.is_dir() {
            return Some(candidate);
        }
        match current.parent() {
            Some(parent) => current = parent,
            None => return None,
        }
    }
}

pub(super) fn resolve_project_root(
    cwd: &Path,
    override_root: Option<&PathBuf>,
    command: &CommandSpec,
) -> Result<PathBuf> {
    if let Some(root) = override_root {
        return Ok(root.to_path_buf());
    }

    match find_project_root(cwd) {
        Some(root) => Ok(root),
        None => match command {
            CommandSpec::Init { .. } => Ok(cwd.join(".cladding")),
            CommandSpec::Ps => Ok(cwd.join(".cladding")),
            _ => {
                eprintln!(
                    "error: no .cladding directory found in {} or any parent directory",
                    cwd.display()
                );
                eprintln!("hint: run 'cladding init' from the project directory to create one");
                Err(Error::message("missing .cladding"))
            }
        },
    }
}

pub(super) struct ProjectRuntimeStatus {
    pub(super) current_project_root: String,
    pub(super) already_running: bool,
}

fn current_project_root(context: &Context) -> Result<String> {
    Ok(canonicalize_path(&context.project_root)?
        .display()
        .to_string())
}

pub(super) fn project_runtime_status(
    context: &Context,
    config: &ExecutionConfig,
    verbose: bool,
) -> Result<ProjectRuntimeStatus> {
    let current_project_root = current_project_root(context)?;

    let mut conflicting_roots = Vec::new();
    let mut already_running = false;
    for project in list_running_projects(verbose)? {
        if project.name != config.name {
            continue;
        }

        let normalized_root = canonicalize_path(Path::new(&project.project_root))
            .map(|path| path.display().to_string())
            .unwrap_or_else(|_| project.project_root.clone());

        if normalized_root == current_project_root {
            already_running = true;
        } else {
            conflicting_roots.push(project.project_root);
        }
    }

    if !conflicting_roots.is_empty() {
        eprintln!(
            "error: cladding project '{}' is already running from a different PROJECT_ROOT",
            config.name
        );
        eprintln!("current PROJECT_ROOT: {current_project_root}");
        for root in conflicting_roots {
            eprintln!("running PROJECT_ROOT: {root}");
        }
        return Err(Error::message(
            "project already running from different PROJECT_ROOT",
        ));
    }

    Ok(ProjectRuntimeStatus {
        current_project_root,
        already_running,
    })
}
