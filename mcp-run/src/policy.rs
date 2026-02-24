use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, RwLock};
use std::time::Duration;

use notify::{RecommendedWatcher, RecursiveMode, Watcher};
use regorus::Engine as RegoEngine;
use thiserror::Error;

const REGO_ALLOW_QUERY: &str = "data.sandbox.main.allow";
const WATCHER_DEBOUNCE_MS: u64 = 250;

#[derive(Debug, Error)]
pub enum ValidationError {
    #[error("Policy deny-all is active: {details}")]
    PolicyUnavailable { details: String },
    #[error("Policy evaluation failed for '{command}': {details}")]
    PolicyEvaluationFailed { command: String, details: String },
    #[error("Command not allowed: {0}")]
    CommandNotAllowed(String),
    #[error("Failed to resolve executable path for '{command}': {details}")]
    PathResolutionFailed { command: String, details: String },
    #[error("Failed to compute executable hash for '{command}': {details}")]
    HashResolutionFailed { command: String, details: String },
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PolicyMode {
    Rego,
    DenyAll,
}

#[derive(Debug, Clone)]
struct RegoPolicy {
    engine: RegoEngine,
    module_count: usize,
}

impl RegoPolicy {
    fn evaluate(&self, input: &PolicyEvaluationInput) -> Result<bool, String> {
        let mut engine = self.engine.clone();
        let input_value = serde_json::json!({
            "command": input.command,
            "path": input.path,
            "hash": input.hash,
            "args": input.args,
            "env": input.env,
        });
        engine.set_input(regorus::Value::from(input_value));
        engine
            .eval_bool_query(REGO_ALLOW_QUERY.to_string(), false)
            .map_err(|error| error.to_string())
    }
}

#[derive(Debug, Clone)]
struct PolicySnapshot {
    mode: PolicyMode,
    rego: Option<RegoPolicy>,
    deny_reason: Option<String>,
}

impl PolicySnapshot {
    fn deny_all(details: impl Into<String>) -> Self {
        Self {
            mode: PolicyMode::DenyAll,
            rego: None,
            deny_reason: Some(details.into()),
        }
    }

    fn from_rego(policy: RegoPolicy) -> Self {
        Self {
            mode: PolicyMode::Rego,
            rego: Some(policy),
            deny_reason: None,
        }
    }
}

#[derive(Debug, Clone)]
struct PolicySources {
    policy_dir: Option<PathBuf>,
}

#[derive(Debug)]
pub struct PolicyEngine {
    state: Arc<RwLock<PolicySnapshot>>,
    sources: PolicySources,
    watcher_started: AtomicBool,
}

#[derive(Debug)]
struct PolicyEvaluationInput<'a> {
    command: &'a str,
    path: &'a str,
    hash: &'a str,
    args: &'a [String],
    env: &'a BTreeMap<String, String>,
}

impl PolicyEngine {
    pub fn from_sources(policy_dir: Option<PathBuf>) -> Self {
        let sources = PolicySources { policy_dir };

        let snapshot = match load_policy_snapshot(&sources) {
            Ok(snapshot) => {
                if let Some(rego) = &snapshot.rego {
                    tracing::info!(
                        mode = "rego",
                        query = REGO_ALLOW_QUERY,
                        modules = rego.module_count,
                        "policy engine initialized",
                    );
                }
                snapshot
            }
            Err(error) => {
                tracing::warn!(error = %error, "policy engine initialized in deny-all mode");
                PolicySnapshot::deny_all(error)
            }
        };

        Self {
            state: Arc::new(RwLock::new(snapshot)),
            sources,
            watcher_started: AtomicBool::new(false),
        }
    }

    #[cfg(test)]
    pub fn from_rego_for_tests(modules: &[(&str, &str)]) -> Self {
        let rego = load_rego_modules(modules).expect("failed to load Rego test modules");
        Self {
            state: Arc::new(RwLock::new(PolicySnapshot::from_rego(rego))),
            sources: PolicySources { policy_dir: None },
            watcher_started: AtomicBool::new(false),
        }
    }

    pub fn mode(&self) -> PolicyMode {
        self.state
            .read()
            .expect("policy state read lock poisoned")
            .mode
            .clone()
    }

    pub fn validate_invocation(
        &self,
        command: &str,
        path: &str,
        hash: &str,
        args: &[String],
        env: &BTreeMap<String, String>,
    ) -> Result<(), ValidationError> {
        let snapshot = self
            .state
            .read()
            .expect("policy state read lock poisoned")
            .clone();

        let evaluation_input = PolicyEvaluationInput {
            command,
            path,
            hash,
            args,
            env,
        };

        match snapshot.mode {
            PolicyMode::Rego => {
                let rego = snapshot
                    .rego
                    .ok_or_else(|| ValidationError::PolicyUnavailable {
                        details: "internal policy state mismatch".to_string(),
                    })?;

                match rego.evaluate(&evaluation_input) {
                    Ok(true) => Ok(()),
                    Ok(false) => Err(ValidationError::CommandNotAllowed(command.to_string())),
                    Err(details) => Err(ValidationError::PolicyEvaluationFailed {
                        command: command.to_string(),
                        details,
                    }),
                }
            }
            PolicyMode::DenyAll => Err(ValidationError::PolicyUnavailable {
                details: snapshot.deny_reason.unwrap_or_else(|| {
                    "policy state is invalid and command execution is denied".to_string()
                }),
            }),
        }
    }

    pub fn reload(&self) {
        match load_policy_snapshot(&self.sources) {
            Ok(snapshot) => {
                if let Some(rego) = &snapshot.rego {
                    tracing::info!(
                        mode = "rego",
                        query = REGO_ALLOW_QUERY,
                        modules = rego.module_count,
                        "policy reload succeeded",
                    );
                }
                *self.state.write().expect("policy state write lock poisoned") = snapshot;
            }
            Err(error) => {
                tracing::error!(error = %error, "policy reload failed; deny-all activated");
                *self.state.write().expect("policy state write lock poisoned") =
                    PolicySnapshot::deny_all(error.to_string());
            }
        }
    }

    pub fn start_watcher(self: &Arc<Self>) {
        let policy_dir = match self.sources.policy_dir.clone() {
            Some(dir) => dir,
            None => return,
        };

        if self
            .watcher_started
            .compare_exchange(false, true, Ordering::SeqCst, Ordering::SeqCst)
            .is_err()
        {
            return;
        }

        let (reload_signal_tx, mut reload_signal_rx) = tokio::sync::mpsc::unbounded_channel();
        let engine_for_reload = Arc::clone(self);
        tokio::spawn(async move {
            while reload_signal_rx.recv().await.is_some() {
                tokio::time::sleep(Duration::from_millis(WATCHER_DEBOUNCE_MS)).await;
                while reload_signal_rx.try_recv().is_ok() {}
                engine_for_reload.reload();
            }
        });

        std::thread::spawn(move || {
            let (event_tx, event_rx) =
                std::sync::mpsc::channel::<Result<notify::Event, notify::Error>>();
            let mut watcher = match RecommendedWatcher::new(event_tx, notify::Config::default()) {
                Ok(watcher) => watcher,
                Err(error) => {
                    tracing::error!(
                        error = %error,
                        policy_dir = %policy_dir.display(),
                        "failed to initialize policy watcher; deny-all activated",
                    );
                    let _ = reload_signal_tx.send(());
                    return;
                }
            };

            if let Err(error) = watcher.watch(&policy_dir, RecursiveMode::Recursive) {
                tracing::error!(
                    error = %error,
                    policy_dir = %policy_dir.display(),
                    "failed to watch policy directory; deny-all activated",
                );
                let _ = reload_signal_tx.send(());
                return;
            }

            tracing::info!(policy_dir = %policy_dir.display(), "policy watcher started");

            while let Ok(event_result) = event_rx.recv() {
                match event_result {
                    Ok(event) => {
                        tracing::info!(kind = ?event.kind, paths = ?event.paths, "policy change detected");
                        let _ = reload_signal_tx.send(());
                    }
                    Err(error) => {
                        tracing::error!(error = %error, "policy watcher event error; deny-all activated");
                        let _ = reload_signal_tx.send(());
                    }
                }
            }

            tracing::warn!("policy watcher channel closed");
        });
    }
}

fn load_policy_snapshot(sources: &PolicySources) -> Result<PolicySnapshot, String> {
    let policy_dir = sources
        .policy_dir
        .as_ref()
        .ok_or_else(|| "POLICY_DIR is not configured".to_string())?;

    let rego =
        load_rego_policy_dir(policy_dir).map_err(|error| format!("rego policy load failed: {error}"))?;
    Ok(PolicySnapshot::from_rego(rego))
}

#[cfg(test)]
fn load_rego_modules(modules: &[(&str, &str)]) -> Result<RegoPolicy, String> {
    let mut engine = RegoEngine::new();
    for (name, source) in modules {
        engine
            .add_policy((*name).to_string(), (*source).to_string())
            .map_err(|error| format!("failed compiling '{name}': {error}"))?;
    }

    Ok(RegoPolicy {
        engine,
        module_count: modules.len(),
    })
}

fn load_rego_policy_dir(policy_dir: &Path) -> Result<RegoPolicy, String> {
    let mut files = Vec::new();
    collect_rego_files(policy_dir, &mut files).map_err(|error| {
        format!(
            "failed reading policy directory '{}': {error}",
            policy_dir.display()
        )
    })?;

    if files.is_empty() {
        return Err(format!(
            "no .rego files found under policy directory '{}'",
            policy_dir.display()
        ));
    }

    files.sort();

    let mut engine = RegoEngine::new();
    for file in &files {
        let source = std::fs::read_to_string(file)
            .map_err(|error| format!("failed reading '{}': {error}", file.display()))?;

        engine
            .add_policy(file.to_string_lossy().into_owned(), source)
            .map_err(|error| format!("failed compiling '{}': {error}", file.display()))?;
    }

    Ok(RegoPolicy {
        engine,
        module_count: files.len(),
    })
}

fn collect_rego_files(dir: &Path, out: &mut Vec<PathBuf>) -> Result<(), std::io::Error> {
    for entry in std::fs::read_dir(dir)? {
        let entry = entry?;
        let path = entry.path();

        if path.is_dir() {
            collect_rego_files(&path, out)?;
            continue;
        }

        if path.extension().and_then(|ext| ext.to_str()) == Some("rego") {
            out.push(path);
        }
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;

    use super::*;
    use tempfile::tempdir;

    fn write_rego_bundle(dir: &Path, command: &str) {
        std::fs::write(
            dir.join("main.rego"),
            r#"package sandbox.main

default allow = false

allow if {
  data.sandbox[input.command].allow
}
"#,
        )
        .expect("write main rego");

        std::fs::write(
            dir.join("command.rego"),
            format!(
                "package sandbox.{}\n\ndefault allow = false\n\nallow if {{\n  count(input.args) == 0\n  count(object.keys(input.env)) == 0\n  startswith(input.path, \"/\")\n}}\n",
                command
            ),
        )
        .expect("write command rego");
    }

    #[test]
    fn rego_mode_selected_when_policy_dir_is_set() {
        let dir = tempdir().expect("temp rego dir");
        write_rego_bundle(dir.path(), "echo");

        let engine = PolicyEngine::from_sources(Some(dir.path().to_path_buf()));
        assert_eq!(engine.mode(), PolicyMode::Rego);
    }

    #[test]
    fn invalid_startup_policy_is_deny_all() {
        let dir = tempdir().expect("temp rego dir");
        std::fs::write(dir.path().join("bad.rego"), "package sandbox.main\nallow if")
            .expect("write bad rego");

        let engine = PolicyEngine::from_sources(Some(dir.path().to_path_buf()));
        assert_eq!(engine.mode(), PolicyMode::DenyAll);
        let err = engine
            .validate_invocation(
                "echo",
                "/usr/bin/echo",
                "0000000000000000000000000000000000000000000000000000000000000000",
                &[],
                &BTreeMap::new(),
            )
            .expect_err("deny-all expected");
        assert!(matches!(err, ValidationError::PolicyUnavailable { .. }));
    }

    #[test]
    fn rego_input_contains_command_path_args_env_hash() {
        let modules = [
            (
                "main.rego",
                r#"package sandbox.main

default allow = false

allow if {
  data.sandbox[input.command].allow
}
"#,
            ),
            (
                "echo.rego",
                r#"package sandbox.echo

default allow = false

allow if {
  input.command == "echo"
  input.args[0] == "ok"
  input.env.FLAG == "1"
  input.hash == "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef"
  startswith(input.path, "/")
}
"#,
            ),
        ];

        let engine = PolicyEngine::from_rego_for_tests(&modules);
        let env = BTreeMap::from([(String::from("FLAG"), String::from("1"))]);
        let args = vec!["ok".to_string()];
        assert!(
            engine
                .validate_invocation(
                    "echo",
                    "/usr/bin/echo",
                    "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef",
                    &args,
                    &env,
                )
                .is_ok()
        );

        let err = engine
            .validate_invocation(
                "/usr/bin/echo",
                "/usr/bin/echo",
                "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef",
                &args,
                &env,
            )
            .expect_err("command token should not match when full path is sent");
        assert!(err.to_string().contains("Command not allowed"));
    }

    #[test]
    fn reload_transitions_invalid_to_deny_all_and_recovers() {
        let dir = tempdir().expect("temp rego dir");
        write_rego_bundle(dir.path(), "echo");

        let engine = PolicyEngine::from_sources(Some(dir.path().to_path_buf()));
        assert_eq!(engine.mode(), PolicyMode::Rego);
        assert!(engine
            .validate_invocation(
                "echo",
                "/usr/bin/echo",
                "0000000000000000000000000000000000000000000000000000000000000000",
                &[],
                &BTreeMap::new(),
            )
            .is_ok());

        std::fs::write(
            dir.path().join("command.rego"),
            "package sandbox.echo\n\ndefault allow = false\nallow if",
        )
        .expect("write invalid rego");

        engine.reload();
        assert_eq!(engine.mode(), PolicyMode::DenyAll);
        assert!(matches!(
            engine
                .validate_invocation(
                    "echo",
                    "/usr/bin/echo",
                    "0000000000000000000000000000000000000000000000000000000000000000",
                    &[],
                    &BTreeMap::new(),
                )
                .expect_err("deny-all expected"),
            ValidationError::PolicyUnavailable { .. }
        ));

        write_rego_bundle(dir.path(), "echo");
        engine.reload();
        assert_eq!(engine.mode(), PolicyMode::Rego);
        assert!(engine
            .validate_invocation(
                "echo",
                "/usr/bin/echo",
                "0000000000000000000000000000000000000000000000000000000000000000",
                &[],
                &BTreeMap::new(),
            )
            .is_ok());
    }

    #[test]
    fn missing_policy_dir_is_deny_all() {
        let engine = PolicyEngine::from_sources(None);
        assert_eq!(engine.mode(), PolicyMode::DenyAll);
    }
}
