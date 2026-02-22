use std::collections::{BTreeMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, RwLock};
use std::time::Duration;

use notify::{RecommendedWatcher, RecursiveMode, Watcher};
use regex::Regex;
use regorus::Engine as RegoEngine;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use thiserror::Error;

pub type Policy = Vec<CommandRule>;

const REGO_ALLOW_QUERY: &str = "data.sandbox.main.allow";
const WATCHER_DEBOUNCE_MS: u64 = 250;

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CommandRule {
    pub command: String,
    #[serde(default)]
    pub args: Vec<ArgCheck>,
    #[serde(default)]
    pub env: Vec<String>,
    #[serde(default)]
    pub description: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "lowercase", deny_unknown_fields)]
pub enum ArgCheck {
    Exact {
        value: String,
        #[serde(default)]
        position: Option<usize>,
        #[serde(default)]
        required: Option<bool>,
    },
    Regex {
        pattern: String,
        #[serde(default)]
        position: Option<usize>,
        #[serde(default)]
        required: Option<bool>,
    },
    Hash {
        value: String,
        #[serde(default)]
        algorithm: Option<HashAlgorithm>,
        #[serde(default)]
        position: Option<usize>,
        #[serde(default)]
        required: Option<bool>,
    },
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum HashAlgorithm {
    Sha256,
}

impl ArgCheck {
    fn position(&self) -> Option<usize> {
        match self {
            ArgCheck::Exact { position, .. } => *position,
            ArgCheck::Regex { position, .. } => *position,
            ArgCheck::Hash { position, .. } => *position,
        }
    }

    fn required(&self) -> bool {
        match self {
            ArgCheck::Exact { required, .. } => required.unwrap_or(false),
            ArgCheck::Regex { required, .. } => required.unwrap_or(false),
            ArgCheck::Hash { required, .. } => required.unwrap_or(false),
        }
    }

    fn expected_description(&self) -> String {
        match self {
            ArgCheck::Exact { value, .. } => value.clone(),
            ArgCheck::Regex { .. } => "regex".to_string(),
            ArgCheck::Hash { .. } => "hash".to_string(),
        }
    }
}

#[derive(Debug, Error)]
pub enum PolicyLoadError {
    #[error("policy file not found")]
    NotFound,
    #[error("unable to read policy file: {source}")]
    Read { source: std::io::Error },
    #[error("invalid JSON in policy file: {source}")]
    InvalidJson { source: serde_json::Error },
    #[error("legacy field 'allowedHosts' is not supported; remove it from the policy")]
    LegacyAllowedHosts,
    #[error("policy schema validation failed: {source}")]
    InvalidSchema { source: serde_json::Error },
    #[error("invalid regex '{pattern}' in policy: {source}")]
    InvalidRegex {
        pattern: String,
        source: regex::Error,
    },
}

pub fn load_policy(policy_path: &Path) -> Result<Policy, PolicyLoadError> {
    let raw = std::fs::read_to_string(policy_path).map_err(|source| {
        if source.kind() == std::io::ErrorKind::NotFound {
            PolicyLoadError::NotFound
        } else {
            PolicyLoadError::Read { source }
        }
    })?;

    let value: serde_json::Value =
        serde_json::from_str(&raw).map_err(|source| PolicyLoadError::InvalidJson { source })?;

    if value.get("allowedHosts").is_some() {
        return Err(PolicyLoadError::LegacyAllowedHosts);
    }

    let policy: Policy =
        serde_json::from_value(value).map_err(|source| PolicyLoadError::InvalidSchema { source })?;

    for rule in &policy {
        for check in &rule.args {
            if let ArgCheck::Regex { pattern, .. } = check {
                Regex::new(pattern).map_err(|source| PolicyLoadError::InvalidRegex {
                    pattern: pattern.clone(),
                    source,
                })?;
            }
        }
    }

    Ok(policy)
}

#[derive(Debug, Error)]
pub enum ValidationError {
    #[error("Policy deny-all is active: {details}")]
    PolicyUnavailable { details: String },
    #[error("Policy evaluation failed for '{command}': {details}")]
    PolicyEvaluationFailed { command: String, details: String },
    #[error("Command not allowed: {0}")]
    CommandNotAllowed(String),
    #[error("Command validation failed for '{command}'. Tried {rule_count} rule(s):\n- {details}")]
    RuleValidationFailed {
        command: String,
        rule_count: usize,
        details: String,
    },
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PolicyMode {
    Rego,
    LegacyJson,
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
    legacy_json: Option<Policy>,
    deny_reason: Option<String>,
}

impl PolicySnapshot {
    fn deny_all(details: impl Into<String>) -> Self {
        Self {
            mode: PolicyMode::DenyAll,
            rego: None,
            legacy_json: None,
            deny_reason: Some(details.into()),
        }
    }

    fn from_rego(policy: RegoPolicy) -> Self {
        Self {
            mode: PolicyMode::Rego,
            rego: Some(policy),
            legacy_json: None,
            deny_reason: None,
        }
    }

    fn from_legacy_json(policy: Policy) -> Self {
        Self {
            mode: PolicyMode::LegacyJson,
            rego: None,
            legacy_json: Some(policy),
            deny_reason: None,
        }
    }
}

#[derive(Debug, Clone)]
struct PolicySources {
    policy_dir: Option<PathBuf>,
    policy_file: Option<PathBuf>,
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
    args: &'a [String],
    env: &'a BTreeMap<String, String>,
}

impl PolicyEngine {
    pub fn from_sources(policy_dir: Option<PathBuf>, policy_file: Option<PathBuf>) -> Self {
        let sources = PolicySources {
            policy_dir,
            policy_file,
        };

        let snapshot = match load_policy_snapshot(&sources) {
            Ok(snapshot) => {
                match snapshot.mode {
                    PolicyMode::Rego => {
                        if let Some(rego) = &snapshot.rego {
                            tracing::info!(
                                mode = "rego",
                                query = REGO_ALLOW_QUERY,
                                modules = rego.module_count,
                                "policy engine initialized",
                            );
                        }
                    }
                    PolicyMode::LegacyJson => {
                        if let Some(legacy_json) = &snapshot.legacy_json {
                            tracing::info!(
                                mode = "legacy-json",
                                rules = legacy_json.len(),
                                "policy engine initialized",
                            );
                        }
                    }
                    PolicyMode::DenyAll => {}
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
    pub fn from_legacy_policy_for_tests(policy: Policy) -> Self {
        Self {
            state: Arc::new(RwLock::new(PolicySnapshot::from_legacy_json(policy))),
            sources: PolicySources {
                policy_dir: None,
                policy_file: None,
            },
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
        args: &[String],
        env: &BTreeMap<String, String>,
    ) -> Result<(), ValidationError> {
        let resolved_path = resolve_executable_path(command).map_err(|error| {
            ValidationError::RuleValidationFailed {
                command: command.to_string(),
                rule_count: 1,
                details: format!("unable to resolve executable path: {error}"),
            }
        })?;

        let snapshot = self
            .state
            .read()
            .expect("policy state read lock poisoned")
            .clone();

        let evaluation_input = PolicyEvaluationInput {
            command,
            path: &resolved_path,
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
            PolicyMode::LegacyJson => {
                let legacy_json = snapshot
                    .legacy_json
                    .ok_or_else(|| ValidationError::PolicyUnavailable {
                        details: "internal policy state mismatch".to_string(),
                    })?;
                validate_legacy_invocation(&legacy_json, command, args, env)
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
                match snapshot.mode {
                    PolicyMode::Rego => {
                        if let Some(rego) = &snapshot.rego {
                            tracing::info!(
                                mode = "rego",
                                query = REGO_ALLOW_QUERY,
                                modules = rego.module_count,
                                "policy reload succeeded",
                            );
                        }
                    }
                    PolicyMode::LegacyJson => {
                        if let Some(legacy_json) = &snapshot.legacy_json {
                            tracing::info!(
                                mode = "legacy-json",
                                rules = legacy_json.len(),
                                "policy reload succeeded",
                            );
                        }
                    }
                    PolicyMode::DenyAll => {}
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
    if let Some(policy_dir) = &sources.policy_dir {
        let rego =
            load_rego_policy_dir(policy_dir).map_err(|error| format!("rego policy load failed: {error}"))?;
        return Ok(PolicySnapshot::from_rego(rego));
    }

    if let Some(policy_file) = &sources.policy_file {
        let legacy_json = load_policy(policy_file)
            .map_err(|error| format!("legacy JSON policy load failed ({})", error))?;
        return Ok(PolicySnapshot::from_legacy_json(legacy_json));
    }

    Err("no policy source configured (set POLICY_DIR or POLICY_FILE)".to_string())
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

fn resolve_executable_path(command: &str) -> Result<String, String> {
    if command.contains('/') {
        let canonical = std::fs::canonicalize(command)
            .map_err(|error| format!("{} ({error})", command))?;
        return Ok(canonical.to_string_lossy().into_owned());
    }

    let path = std::env::var_os("PATH").ok_or_else(|| "PATH is not set".to_string())?;
    for directory in std::env::split_paths(&path) {
        let candidate = directory.join(command);
        if !candidate.is_file() {
            continue;
        }

        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let metadata = candidate.metadata().map_err(|error| {
                format!("failed reading metadata for '{}': {error}", candidate.display())
            })?;
            if metadata.permissions().mode() & 0o111 == 0 {
                continue;
            }
        }

        let canonical = std::fs::canonicalize(&candidate)
            .map_err(|error| format!("failed resolving '{}': {error}", candidate.display()))?;
        return Ok(canonical.to_string_lossy().into_owned());
    }

    Err(format!("'{}' was not found on PATH", command))
}

pub fn validate_legacy_invocation(
    policy: &Policy,
    command: &str,
    args: &[String],
    env: &BTreeMap<String, String>,
) -> Result<(), ValidationError> {
    let rules: Vec<&CommandRule> = policy
        .iter()
        .filter(|rule| rule.command == command)
        .collect();

    if rules.is_empty() {
        return Err(ValidationError::CommandNotAllowed(command.to_string()));
    }

    let mut errors = Vec::with_capacity(rules.len());
    for rule in &rules {
        match validate_rule(args, &rule.args).and_then(|_| validate_env(env, &rule.env)) {
            Ok(()) => return Ok(()),
            Err(error) => errors.push(error),
        }
    }

    Err(ValidationError::RuleValidationFailed {
        command: command.to_string(),
        rule_count: rules.len(),
        details: errors.join("\n- "),
    })
}

pub fn validate_invocation(
    policy: &Policy,
    command: &str,
    args: &[String],
    env: &BTreeMap<String, String>,
) -> Result<(), ValidationError> {
    validate_legacy_invocation(policy, command, args, env)
}

fn validate_env(env: &BTreeMap<String, String>, allowed_env: &[String]) -> Result<(), String> {
    let allowed: HashSet<&str> = allowed_env.iter().map(String::as_str).collect();
    for key in env.keys() {
        if !allowed.contains(key.as_str()) {
            return Err(format!("Environment variable not allowed: {key}"));
        }
    }
    Ok(())
}

fn validate_rule(args: &[String], checks: &[ArgCheck]) -> Result<(), String> {
    if checks.is_empty() {
        if args.is_empty() {
            return Ok(());
        }
        return Err("Command does not allow arguments.".to_string());
    }

    for (index, arg) in args.iter().enumerate() {
        let mut matched = false;
        for check in checks {
            if let Some(position) = check.position()
                && position != index
            {
                continue;
            }

            if check_arg(arg, check) {
                matched = true;
                break;
            }
        }

        if !matched {
            return Err(format!("Argument not allowed at position {index}: {arg}"));
        }
    }

    for check in checks {
        if !check.required() {
            continue;
        }

        let satisfied = if let Some(position) = check.position() {
            args.get(position).is_some_and(|value| check_arg(value, check))
        } else {
            args.iter().any(|value| check_arg(value, check))
        };

        if !satisfied {
            if let Some(position) = check.position() {
                return Err(format!("Missing required argument at position {position}"));
            }
            return Err(format!(
                "Missing required argument matching: {}",
                check.expected_description()
            ));
        }
    }

    Ok(())
}

fn check_arg(arg: &str, check: &ArgCheck) -> bool {
    match check {
        ArgCheck::Exact { value, .. } => arg == value,
        ArgCheck::Regex { pattern, .. } => Regex::new(pattern).is_ok_and(|regex| regex.is_match(arg)),
        ArgCheck::Hash {
            value, algorithm, ..
        } => {
            let algorithm = algorithm.unwrap_or(HashAlgorithm::Sha256);
            match algorithm {
                HashAlgorithm::Sha256 => check_file_sha256(arg, value),
            }
        }
    }
}

fn check_file_sha256(file_path: &str, expected_hash: &str) -> bool {
    let bytes = match std::fs::read(file_path) {
        Ok(bytes) => bytes,
        Err(_) => return false,
    };
    let hash = Sha256::digest(bytes);
    format!("{hash:x}") == expected_hash
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;

    use super::*;
    use sha2::{Digest, Sha256};
    use tempfile::{NamedTempFile, tempdir};

    fn write_policy_file(policy: serde_json::Value) -> NamedTempFile {
        let file = NamedTempFile::new().expect("temp policy file");
        std::fs::write(
            file.path(),
            serde_json::to_vec_pretty(&policy).expect("serialize policy"),
        )
        .expect("write policy");
        file
    }

    fn sha256_hex(bytes: &[u8]) -> String {
        let digest = Sha256::digest(bytes);
        format!("{digest:x}")
    }

    fn find_executable(name: &str) -> Option<String> {
        let path = std::env::var_os("PATH")?;
        for dir in std::env::split_paths(&path) {
            let candidate = dir.join(name);
            if candidate.is_file() {
                return Some(candidate.to_string_lossy().into_owned());
            }
        }
        None
    }

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
                "package sandbox.{}\n\nimport rego.v1\n\ndefault allow = false\n\nallow if {{\n  count(input.args) == 0\n  count(object.keys(input.env)) == 0\n  startswith(input.path, \"/\")\n}}\n",
                command
            ),
        )
        .expect("write command rego");
    }

    #[test]
    fn policy_parses_exact_regex_hash_and_env() {
        let hashed_file = NamedTempFile::new().expect("temp file");
        std::fs::write(hashed_file.path(), b"hello-hash").expect("write hash file");
        let expected_hash = sha256_hex(b"hello-hash");

        let policy: Policy = vec![CommandRule {
            command: "cmd".to_string(),
            args: vec![
                ArgCheck::Exact {
                    value: "install".to_string(),
                    position: Some(0),
                    required: Some(true),
                },
                ArgCheck::Regex {
                    pattern: "^pkg-[a-z]+$".to_string(),
                    position: Some(1),
                    required: Some(true),
                },
                ArgCheck::Hash {
                    value: expected_hash,
                    algorithm: Some(HashAlgorithm::Sha256),
                    position: Some(2),
                    required: Some(true),
                },
            ],
            env: vec!["TOKEN".to_string()],
            description: None,
        }];

        let args = vec![
            "install".to_string(),
            "pkg-core".to_string(),
            hashed_file.path().to_string_lossy().to_string(),
        ];
        let env = BTreeMap::from([(String::from("TOKEN"), String::from("abc"))]);

        assert!(validate_invocation(&policy, "cmd", &args, &env).is_ok());
    }

    #[test]
    fn policy_enforces_position_and_required_rules() {
        let policy: Policy = vec![CommandRule {
            command: "git".to_string(),
            args: vec![
                ArgCheck::Exact {
                    value: "commit".to_string(),
                    position: Some(0),
                    required: Some(true),
                },
                ArgCheck::Exact {
                    value: "-m".to_string(),
                    position: None,
                    required: Some(true),
                },
                ArgCheck::Regex {
                    pattern: ".*".to_string(),
                    position: None,
                    required: Some(false),
                },
            ],
            env: vec![],
            description: None,
        }];

        let missing_required = vec!["commit".to_string(), "message".to_string()];
        let err = validate_invocation(&policy, "git", &missing_required, &BTreeMap::new())
            .expect_err("missing -m should fail");
        assert!(
            err.to_string()
                .contains("Missing required argument matching: -m")
        );

        let good = vec![
            "commit".to_string(),
            "-m".to_string(),
            "message".to_string(),
        ];
        assert!(validate_invocation(&policy, "git", &good, &BTreeMap::new()).is_ok());
    }

    #[test]
    fn policy_enforces_env_allowlist() {
        let policy: Policy = vec![CommandRule {
            command: "npm".to_string(),
            args: vec![ArgCheck::Regex {
                pattern: ".*".to_string(),
                position: None,
                required: Some(false),
            }],
            env: vec!["TOKEN".to_string()],
            description: None,
        }];

        let env = BTreeMap::from([(String::from("UNSAFE"), String::from("1"))]);
        let err = validate_invocation(&policy, "npm", &["test".into()], &env)
            .expect_err("disallowed env key should fail");
        assert!(
            err.to_string()
                .contains("Environment variable not allowed: UNSAFE")
        );
    }

    #[test]
    fn policy_applies_or_across_multiple_rules() {
        let policy: Policy = vec![
            CommandRule {
                command: "npm".to_string(),
                args: vec![ArgCheck::Exact {
                    value: "install".to_string(),
                    position: Some(0),
                    required: Some(true),
                }],
                env: vec![],
                description: None,
            },
            CommandRule {
                command: "npm".to_string(),
                args: vec![ArgCheck::Exact {
                    value: "test".to_string(),
                    position: Some(0),
                    required: Some(true),
                }],
                env: vec![],
                description: None,
            },
        ];

        assert!(validate_invocation(&policy, "npm", &["test".to_string()], &BTreeMap::new())
            .is_ok());

        let err = validate_invocation(&policy, "npm", &["publish".to_string()], &BTreeMap::new())
            .expect_err("unknown mode should fail");
        assert!(err.to_string().contains("Tried 2 rule(s)"));
    }

    #[test]
    fn policy_rejects_allowed_hosts_field() {
        let file = write_policy_file(serde_json::json!({
            "allowedHosts": ["example.com"]
        }));
        let err = load_policy(file.path()).expect_err("allowedHosts should be rejected");
        assert!(err.to_string().contains("allowedHosts"));
    }

    #[test]
    fn policy_rejects_commands_wrapper_object() {
        let file = write_policy_file(serde_json::json!({
            "commands": []
        }));
        let err = load_policy(file.path()).expect_err("commands wrapper should be rejected");
        assert!(matches!(err, PolicyLoadError::InvalidSchema { .. }));
    }

    #[test]
    fn rego_mode_selected_when_policy_dir_is_set() {
        let dir = tempdir().expect("temp rego dir");
        write_rego_bundle(dir.path(), "echo");

        let engine = PolicyEngine::from_sources(Some(dir.path().to_path_buf()), None);
        assert_eq!(engine.mode(), PolicyMode::Rego);
    }

    #[test]
    fn policy_dir_takes_precedence_over_policy_file() {
        let dir = tempdir().expect("temp rego dir");
        std::fs::write(
            dir.path().join("main.rego"),
            "package sandbox.main\n\nimport rego.v1\ndefault allow = false\n",
        )
        .expect("write deny rego");

        let policy_file = write_policy_file(serde_json::json!([
            {"command": "echo", "args": [], "env": []}
        ]));

        let engine = PolicyEngine::from_sources(
            Some(dir.path().to_path_buf()),
            Some(policy_file.path().to_path_buf()),
        );

        assert_eq!(engine.mode(), PolicyMode::Rego);
        let err = engine
            .validate_invocation("echo", &[], &BTreeMap::new())
            .expect_err("rego deny should win");
        assert!(err.to_string().contains("Command not allowed"));
    }

    #[test]
    fn invalid_startup_policy_is_deny_all() {
        let dir = tempdir().expect("temp rego dir");
        std::fs::write(dir.path().join("bad.rego"), "package sandbox.main\nallow if")
            .expect("write bad rego");

        let engine = PolicyEngine::from_sources(Some(dir.path().to_path_buf()), None);
        assert_eq!(engine.mode(), PolicyMode::DenyAll);
        let err = engine
            .validate_invocation("echo", &[], &BTreeMap::new())
            .expect_err("deny-all expected");
        assert!(matches!(err, ValidationError::PolicyUnavailable { .. }));
    }

    #[test]
    fn rego_input_contains_command_path_args_env() {
        let echo = match find_executable("echo") {
            Some(path) => path,
            None => return,
        };

        let dir = tempdir().expect("temp rego dir");
        std::fs::write(
            dir.path().join("main.rego"),
            r#"package sandbox.main


default allow = false

allow if {
  data.sandbox[input.command].allow
}
"#,
        )
        .expect("write main rego");

        std::fs::write(
            dir.path().join("echo.rego"),
            r#"package sandbox.echo


default allow = false

allow if {
  input.command == "echo"
  input.args[0] == "ok"
  input.env.FLAG == "1"
  startswith(input.path, "/")
}
"#,
        )
        .expect("write command rego");

        let engine = PolicyEngine::from_sources(Some(dir.path().to_path_buf()), None);
        let env = BTreeMap::from([(String::from("FLAG"), String::from("1"))]);
        let args = vec!["ok".to_string()];
        assert!(engine.validate_invocation("echo", &args, &env).is_ok());

        let err = engine
            .validate_invocation(&echo, &args, &env)
            .expect_err("command token should not match when full path is sent");
        assert!(err.to_string().contains("Command not allowed"));
    }

    #[test]
    fn reload_transitions_invalid_to_deny_all_and_recovers() {
        let dir = tempdir().expect("temp rego dir");
        write_rego_bundle(dir.path(), "echo");

        let engine = PolicyEngine::from_sources(Some(dir.path().to_path_buf()), None);
        assert_eq!(engine.mode(), PolicyMode::Rego);
        assert!(engine
            .validate_invocation("echo", &[], &BTreeMap::new())
            .is_ok());

        std::fs::write(
            dir.path().join("command.rego"),
            "package sandbox.echo\n\nimport rego.v1\n\ndefault allow = false\nallow if",
        )
        .expect("write invalid rego");

        engine.reload();
        assert_eq!(engine.mode(), PolicyMode::DenyAll);
        assert!(matches!(
            engine
                .validate_invocation("echo", &[], &BTreeMap::new())
                .expect_err("deny-all expected"),
            ValidationError::PolicyUnavailable { .. }
        ));

        write_rego_bundle(dir.path(), "echo");
        engine.reload();
        assert_eq!(engine.mode(), PolicyMode::Rego);
        assert!(engine
            .validate_invocation("echo", &[], &BTreeMap::new())
            .is_ok());
    }

    #[test]
    fn legacy_json_fallback_mode_works_without_policy_dir() {
        let policy_file = write_policy_file(serde_json::json!([
            {
                "command": "echo",
                "args": [{"type": "exact", "value": "ok", "position": 0, "required": true}],
                "env": []
            }
        ]));

        let engine = PolicyEngine::from_sources(None, Some(policy_file.path().to_path_buf()));
        assert_eq!(engine.mode(), PolicyMode::LegacyJson);

        assert!(engine
            .validate_invocation("echo", &["ok".to_string()], &BTreeMap::new())
            .is_ok());
    }
}
