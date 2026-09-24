use super::args::CommandSpec;
use cladding::config::{
    ExecutionConfig, load_cladding_config_v2, load_cladding_config_v2_from_path,
    load_cladding_config_v2_from_str, write_default_cladding_config,
};
use cladding::error::{Error, Result};
use cladding::fs_utils::canonicalize_path;
use cladding::podman::list_running_projects;
use std::path::{Path, PathBuf};

#[derive(Debug, Clone)]
pub(super) struct Context {
    pub(super) project_root: PathBuf,
    pub(super) workspace_root: PathBuf,
    config_source: ConfigSource,
}

#[derive(Debug, Clone)]
pub(super) enum ConfigSource {
    Default,
    OnceDefault { base_dir: PathBuf },
    File(PathBuf),
    Stdin { raw: String, base_dir: PathBuf },
    Resolved(Box<ExecutionConfig>),
}

impl Context {
    pub(super) fn new(
        project_root: PathBuf,
        workspace_root: PathBuf,
        config_source: ConfigSource,
    ) -> Self {
        Self {
            project_root,
            workspace_root,
            config_source,
        }
    }

    #[cfg(test)]
    pub(super) fn default_for_project(project_root: PathBuf) -> Self {
        let workspace_root = project_root.parent().unwrap_or(&project_root).to_path_buf();
        Self::new(project_root, workspace_root, ConfigSource::Default)
    }

    pub(super) fn load_config(&self) -> Result<ExecutionConfig> {
        match &self.config_source {
            ConfigSource::Default => load_cladding_config_v2(&self.project_root),
            ConfigSource::OnceDefault { base_dir } => {
                let raw = write_default_cladding_config(
                    Some("once"),
                    super::DEFAULT_SANDBOX_BUILD_IMAGE,
                    super::DEFAULT_CLI_BUILD_IMAGE,
                )?;
                load_cladding_config_v2_from_str(&base_dir.join("cladding.json"), &raw)
            }
            ConfigSource::File(path) => load_cladding_config_v2_from_path(path),
            ConfigSource::Stdin { raw, base_dir } => {
                load_cladding_config_v2_from_str(&base_dir.join("cladding.json"), raw)
            }
            ConfigSource::Resolved(config) => Ok(config.as_ref().clone()),
        }
    }

    pub(super) fn with_resolved_config(
        &self,
        project_root: PathBuf,
        config: ExecutionConfig,
    ) -> Self {
        Self::new(
            project_root,
            self.workspace_root.clone(),
            ConfigSource::Resolved(Box::new(config)),
        )
    }

    pub(super) fn config_uses_stdin(&self) -> bool {
        matches!(self.config_source, ConfigSource::Stdin { .. })
    }
}

pub(super) fn stdin_config_base_dir(cwd: &Path, cladding_dir: Option<&Path>) -> PathBuf {
    cladding_dir
        .and_then(Path::parent)
        .map(Path::to_path_buf)
        .unwrap_or_else(|| cwd.to_path_buf())
}

fn find_project_root(start: &Path) -> Option<PathBuf> {
    let mut current = start;
    loop {
        let candidate = current.join(".cladding");
        if candidate.is_dir() {
            return Some(candidate);
        }
        current = current.parent()?;
    }
}

fn find_git_root(start: &Path) -> Option<PathBuf> {
    let mut current = start;
    loop {
        if current.join(".git").exists() {
            return Some(current.to_path_buf());
        }
        current = current.parent()?;
    }
}

pub(super) fn resolve_workspace_root(cwd: &Path) -> PathBuf {
    find_project_root(cwd)
        .and_then(|root| root.parent().map(Path::to_path_buf))
        .or_else(|| find_git_root(cwd))
        .unwrap_or_else(|| cwd.to_path_buf())
}

pub(super) fn resolve_project_root(
    cwd: &Path,
    override_root: Option<&PathBuf>,
    command: &CommandSpec,
) -> Result<PathBuf> {
    if let Some(root) = override_root {
        if matches!(command, CommandSpec::Init { .. }) {
            if root.exists() && !root.is_dir() {
                eprintln!(
                    "error: selected .cladding path exists but is not a directory: {}",
                    root.display()
                );
                return Err(Error::message("invalid .cladding path"));
            }
        } else if !root.is_dir() {
            eprintln!(
                "error: selected .cladding directory does not exist or is not a directory: {}",
                root.display()
            );
            eprintln!("hint: pass the path to an existing .cladding directory");
            return Err(Error::message("invalid .cladding directory"));
        }
        return Ok(root.to_path_buf());
    }

    match find_project_root(cwd) {
        Some(root) => Ok(root),
        None => match command {
            CommandSpec::Init { .. } | CommandSpec::Once { .. } => Ok(cwd.join(".cladding")),
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

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use std::sync::atomic::{AtomicUsize, Ordering};

    static TEMP_DIR_COUNTER: AtomicUsize = AtomicUsize::new(0);

    #[test]
    fn explicit_config_load_is_independent_of_selected_runtime_directory() {
        let temp = create_temp_dir("external-config");
        let runtime_dir = temp.join("runtime/.cladding");
        let config_dir = temp.join("configs");
        fs::create_dir_all(&runtime_dir).unwrap();
        fs::create_dir_all(&config_dir).unwrap();
        let config_path = config_dir.join("custom.json");
        fs::write(
            &config_path,
            r#"{"name":"custom","agent":{"image":"agent:test"}}"#,
        )
        .unwrap();

        let workspace_dir = temp.join("checkout");
        let context = Context::new(
            runtime_dir.clone(),
            workspace_dir.clone(),
            ConfigSource::File(config_path),
        );
        let config = context.load_config().unwrap();

        assert_eq!(context.project_root, runtime_dir);
        assert_eq!(context.workspace_root, workspace_dir);
        assert_eq!(config.name, "custom");
    }

    #[test]
    fn stdin_config_references_use_the_provided_base_directory() {
        let temp = create_temp_dir("stdin-config");
        let runtime_dir = temp.join("runtime/.cladding");
        let config_base = temp.join("project");
        fs::create_dir_all(&runtime_dir).unwrap();
        fs::create_dir_all(&config_base).unwrap();
        let raw = r#"{
            "name":"custom",
            "agent":{"build":{"containerfile":"containers/agent.Containerfile"}}
        }"#;
        let context = Context::new(
            runtime_dir,
            config_base.clone(),
            ConfigSource::Stdin {
                raw: raw.to_string(),
                base_dir: config_base.clone(),
            },
        );

        let config = context.load_config().unwrap();

        assert_eq!(
            config.agent.build.unwrap().containerfile,
            config_base.join("containers/agent.Containerfile")
        );
    }

    #[test]
    fn explicit_directory_and_default_discovery_select_the_expected_root() {
        let temp = create_temp_dir("directory-discovery");
        let project_root = temp.join("project/.cladding");
        let cwd = temp.join("project/src");
        fs::create_dir_all(&project_root).unwrap();
        fs::create_dir_all(&cwd).unwrap();
        let command = CommandSpec::Up { verbose: false };

        assert_eq!(
            resolve_project_root(&cwd, None, &command).unwrap(),
            project_root
        );
        assert_eq!(
            resolve_project_root(&cwd, Some(&project_root), &command).unwrap(),
            project_root
        );
    }

    #[test]
    fn once_can_select_a_missing_default_directory_without_creating_it() {
        let temp = create_temp_dir("once-missing-directory");
        let project_root = temp.join("project");
        fs::create_dir_all(&project_root).unwrap();
        let selected = resolve_project_root(
            &project_root,
            None,
            &CommandSpec::Once {
                args: vec!["echo".to_string()],
            },
        )
        .unwrap();

        assert_eq!(selected, project_root.join(".cladding"));
        assert!(!selected.exists());
    }

    #[test]
    fn once_default_config_uses_shared_config_loading_without_persistent_state() {
        let temp = create_temp_dir("once-default-config");
        let selected_root = temp.join(".cladding");
        let context = Context::new(
            selected_root.clone(),
            temp.clone(),
            ConfigSource::OnceDefault {
                base_dir: temp.clone(),
            },
        );

        let config = context.load_config().unwrap();

        assert_eq!(config.name, "once");
        assert!(config.nw_sandbox_enabled());
        assert!(!selected_root.exists());
    }

    #[test]
    fn workspace_root_uses_git_root_without_discovered_cladding() {
        let temp = create_temp_dir("git-workspace-root");
        let checkout = temp.join("checkout");
        let cwd = checkout.join("src/module");
        fs::create_dir_all(checkout.join(".git")).unwrap();
        fs::create_dir_all(&cwd).unwrap();

        assert_eq!(resolve_workspace_root(&cwd), checkout);
    }

    #[test]
    fn workspace_root_falls_back_to_invocation_directory_without_project_markers() {
        let temp = create_temp_dir("workspace-cwd-fallback");
        let cwd = temp.join("current");
        fs::create_dir_all(&cwd).unwrap();

        assert_eq!(resolve_workspace_root(&cwd), cwd);
    }

    #[test]
    fn workspace_root_keeps_discovered_cladding_parent() {
        let temp = create_temp_dir("workspace-discovery");
        let project = temp.join("project");
        let cwd = project.join("src/module");
        fs::create_dir_all(project.join(".cladding")).unwrap();
        fs::create_dir_all(&cwd).unwrap();

        assert_eq!(resolve_workspace_root(&cwd), project);
    }

    #[test]
    fn init_can_create_an_explicit_cladding_directory() {
        let temp = create_temp_dir("init-directory");
        let selected_root = temp.join("project/.cladding");

        let root = resolve_project_root(
            &temp,
            Some(&selected_root),
            &CommandSpec::Init { name: None },
        )
        .unwrap();

        assert_eq!(root, selected_root);
        assert!(!root.exists());
    }

    #[test]
    fn stdin_config_base_uses_selected_directory_parent_or_invocation_directory() {
        let cwd = Path::new("/work/current");
        let selected = Path::new("/tmp/project/.cladding");

        assert_eq!(
            stdin_config_base_dir(cwd, Some(selected)),
            PathBuf::from("/tmp/project")
        );
        assert_eq!(
            stdin_config_base_dir(cwd, None),
            PathBuf::from("/work/current")
        );
    }

    fn create_temp_dir(name: &str) -> PathBuf {
        let unique = TEMP_DIR_COUNTER.fetch_add(1, Ordering::Relaxed);
        let path = std::env::temp_dir().join(format!(
            "cladding-context-{name}-{}-{unique}",
            std::process::id()
        ));
        if path.exists() {
            fs::remove_dir_all(&path).unwrap();
        }
        fs::create_dir_all(&path).unwrap();
        path
    }
}
