use std::collections::{BTreeMap, HashSet};
use std::net::{AddrParseError, SocketAddr};
use std::path::{Path, PathBuf};
use std::sync::Arc;

use axum::Router;
use axum::routing::any_service;
use regex::Regex;
use rmcp::handler::server::tool::ToolRouter;
use rmcp::handler::server::wrapper::Parameters;
use rmcp::model::{CallToolResult, Implementation, ServerCapabilities, ServerInfo};
use rmcp::transport::streamable_http_server::session::local::LocalSessionManager;
use rmcp::transport::{StreamableHttpServerConfig, StreamableHttpService};
use rmcp::{Json, ServerHandler, tool, tool_handler, tool_router};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::process::Stdio;
use thiserror::Error;
use tokio::io::AsyncReadExt;
use tokio::process::Command;

pub const DEFAULT_BIND_ADDR: &str = "127.0.0.1:8000";
pub const MAX_OUTPUT_BYTES: usize = 1024 * 1024;
pub const TRUNCATION_MARKER: &str = "\n...truncated...";

#[derive(Debug, Clone)]
pub struct AppConfig {
    pub bind_addr: SocketAddr,
    pub policy_file: PathBuf,
    pub default_cwd: PathBuf,
}

impl AppConfig {
    pub fn from_env() -> Result<Self, ConfigError> {
        let bind_raw = std::env::var("MCP_BIND_ADDR").unwrap_or_else(|_| DEFAULT_BIND_ADDR.into());
        let bind_addr =
            bind_raw
                .parse::<SocketAddr>()
                .map_err(|source| ConfigError::InvalidBindAddr {
                    value: bind_raw,
                    source,
                })?;
        let policy_file =
            std::env::var("POLICY_FILE").map_err(|_| ConfigError::MissingPolicyFile)?;
        if policy_file.trim().is_empty() {
            return Err(ConfigError::EmptyPolicyFile);
        }
        let default_cwd =
            std::env::current_dir().map_err(|source| ConfigError::CurrentDir { source })?;

        Ok(Self {
            bind_addr,
            policy_file: PathBuf::from(policy_file),
            default_cwd,
        })
    }
}

#[derive(Debug, Error)]
pub enum ConfigError {
    #[error("invalid MCP_BIND_ADDR '{value}': {source}")]
    InvalidBindAddr {
        value: String,
        source: AddrParseError,
    },
    #[error("POLICY_FILE must be set")]
    MissingPolicyFile,
    #[error("POLICY_FILE must not be empty")]
    EmptyPolicyFile,
    #[error("failed to get current working directory: {source}")]
    CurrentDir { source: std::io::Error },
}

#[derive(Debug, Error)]
pub enum AppError {
    #[error(transparent)]
    Config(#[from] ConfigError),
    #[error("failed to load policy file '{path}': {source}")]
    PolicyLoad {
        path: PathBuf,
        source: PolicyLoadError,
    },
    #[error("server I/O failure: {0}")]
    Io(#[from] std::io::Error),
}

pub type Policy = Vec<CommandRule>;

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

    let policy: Policy = serde_json::from_value(value)
        .map_err(|source| PolicyLoadError::InvalidSchema { source })?;

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
    #[error("Command not allowed: {0}")]
    CommandNotAllowed(String),
    #[error("Command validation failed for '{command}'. Tried {rule_count} rule(s):\n- {details}")]
    RuleValidationFailed {
        command: String,
        rule_count: usize,
        details: String,
    },
}

pub fn validate_invocation(
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
            if let Some(position) = check.position() {
                if position != index {
                    continue;
                }
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
            args.get(position)
                .is_some_and(|value| check_arg(value, check))
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
        ArgCheck::Regex { pattern, .. } => {
            Regex::new(pattern).is_ok_and(|regex| regex.is_match(arg))
        }
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

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct RunNetworkToolInput {
    pub executable: String,
    #[serde(default)]
    pub args: Vec<String>,
    #[serde(default)]
    pub cwd: Option<String>,
    #[serde(default)]
    pub env: Option<BTreeMap<String, String>>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct RunNetworkToolOutput {
    pub stdout: String,
    pub stderr: String,
    #[serde(rename = "exitCode")]
    pub exit_code: Option<i32>,
}

#[derive(Debug, Error)]
pub enum ToolError {
    #[error(transparent)]
    Validation(#[from] ValidationError),
    #[error("Failed to start subprocess: {source}")]
    Spawn { source: std::io::Error },
    #[error("Failed to wait for subprocess: {source}")]
    Wait { source: std::io::Error },
    #[error("Failed to read stdout: {source}")]
    StdoutRead { source: std::io::Error },
    #[error("Failed to read stderr: {source}")]
    StderrRead { source: std::io::Error },
    #[error("Failed to join stdout reader: {source}")]
    StdoutJoin { source: tokio::task::JoinError },
    #[error("Failed to join stderr reader: {source}")]
    StderrJoin { source: tokio::task::JoinError },
}

pub async fn run_network_tool_impl(
    policy: &Policy,
    default_cwd: &Path,
    input: RunNetworkToolInput,
) -> Result<RunNetworkToolOutput, ToolError> {
    let user_env = input.env.unwrap_or_default();
    validate_invocation(policy, &input.executable, &input.args, &user_env)?;

    let mut command = Command::new(&input.executable);
    command
        .args(&input.args)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());

    if let Some(cwd) = input.cwd.as_deref() {
        command.current_dir(cwd);
    } else {
        command.current_dir(default_cwd);
    }

    let command_env = build_command_env(&user_env);
    command.env_clear();
    command.envs(
        command_env
            .iter()
            .map(|(key, value)| (key.as_str(), value.as_str())),
    );

    let mut child = command
        .spawn()
        .map_err(|source| ToolError::Spawn { source })?;

    let stdout = child.stdout.take().ok_or_else(|| ToolError::StdoutRead {
        source: std::io::Error::other("stdout pipe missing"),
    })?;
    let stderr = child.stderr.take().ok_or_else(|| ToolError::StderrRead {
        source: std::io::Error::other("stderr pipe missing"),
    })?;

    let stdout_task = tokio::spawn(read_limited(stdout));
    let stderr_task = tokio::spawn(read_limited(stderr));

    let status = child
        .wait()
        .await
        .map_err(|source| ToolError::Wait { source })?;

    let stdout_capture = stdout_task
        .await
        .map_err(|source| ToolError::StdoutJoin { source })?;
    let stderr_capture = stderr_task
        .await
        .map_err(|source| ToolError::StderrJoin { source })?;

    let (stdout_bytes, stdout_truncated) =
        stdout_capture.map_err(|source| ToolError::StdoutRead { source })?;
    let (stderr_bytes, stderr_truncated) =
        stderr_capture.map_err(|source| ToolError::StderrRead { source })?;

    Ok(RunNetworkToolOutput {
        stdout: finalize_capture(stdout_bytes, stdout_truncated),
        stderr: finalize_capture(stderr_bytes, stderr_truncated),
        exit_code: status.code(),
    })
}

fn build_command_env(user_env: &BTreeMap<String, String>) -> BTreeMap<String, String> {
    let mut command_env = BTreeMap::new();

    for key in ["HOME", "LANG"] {
        if let Ok(value) = std::env::var(key) {
            command_env.insert(key.to_string(), value);
        }
    }

    command_env.extend(
        user_env
            .iter()
            .map(|(key, value)| (key.clone(), value.clone())),
    );

    for key in [
        "PATH",
        "http_proxy",
        "https_proxy",
        "no_proxy",
        "HTTP_PROXY",
        "HTTPS_PROXY",
        "NO_PROXY",
    ] {
        command_env.remove(key);
    }

    if let Ok(path) = std::env::var("PATH") {
        command_env.insert("PATH".to_string(), path);
    }

    let http_proxy = std::env::var("http_proxy").ok();
    let https_proxy = std::env::var("https_proxy").ok();
    let no_proxy = std::env::var("no_proxy").ok();

    if let Some(value) = http_proxy.clone() {
        command_env.insert("http_proxy".to_string(), value);
    }
    if let Some(value) = https_proxy.clone() {
        command_env.insert("https_proxy".to_string(), value);
    }
    if let Some(value) = no_proxy.clone() {
        command_env.insert("no_proxy".to_string(), value);
    }

    if let Some(value) = http_proxy {
        command_env.insert("HTTP_PROXY".to_string(), value);
    }
    if let Some(value) = https_proxy {
        command_env.insert("HTTPS_PROXY".to_string(), value);
    }
    if let Some(value) = no_proxy {
        command_env.insert("NO_PROXY".to_string(), value);
    }

    command_env
}

async fn read_limited<R: tokio::io::AsyncRead + Unpin>(
    mut reader: R,
) -> Result<(Vec<u8>, bool), std::io::Error> {
    let mut output = Vec::new();
    let mut buffer = [0u8; 8192];
    let mut truncated = false;

    loop {
        let bytes_read = reader.read(&mut buffer).await?;
        if bytes_read == 0 {
            break;
        }

        if truncated {
            continue;
        }

        let remaining = MAX_OUTPUT_BYTES.saturating_sub(output.len());
        if bytes_read <= remaining {
            output.extend_from_slice(&buffer[..bytes_read]);
        } else {
            if remaining > 0 {
                output.extend_from_slice(&buffer[..remaining]);
            }
            truncated = true;
        }
    }

    Ok((output, truncated))
}

fn finalize_capture(bytes: Vec<u8>, truncated: bool) -> String {
    let mut value = String::from_utf8_lossy(&bytes).into_owned();
    if truncated {
        value.push_str(TRUNCATION_MARKER);
    }
    value
}

#[derive(Clone)]
pub struct NetworkMcpServer {
    policy: Arc<Policy>,
    default_cwd: PathBuf,
    tool_router: ToolRouter<Self>,
}

#[tool_router]
impl NetworkMcpServer {
    pub fn new(policy: Arc<Policy>, default_cwd: PathBuf) -> Self {
        Self {
            policy,
            default_cwd,
            tool_router: Self::tool_router(),
        }
    }

    #[tool(
        name = "run_network_tool",
        description = "Execute a policy-allowlisted command without shell wrappers."
    )]
    async fn run_network_tool(
        &self,
        Parameters(input): Parameters<RunNetworkToolInput>,
    ) -> Result<Json<RunNetworkToolOutput>, String> {
        run_network_tool_impl(&self.policy, &self.default_cwd, input)
            .await
            .map(Json)
            .map_err(|error| error.to_string())
    }
}

#[tool_handler]
impl ServerHandler for NetworkMcpServer {
    fn get_info(&self) -> ServerInfo {
        ServerInfo {
            capabilities: ServerCapabilities::builder().enable_tools().build(),
            server_info: Implementation {
                name: "network-mcp-rust".to_string(),
                title: Some("Network MCP Rust Reimplementation".to_string()),
                version: env!("CARGO_PKG_VERSION").to_string(),
                description: Some(
                    "Policy-enforced network-capable command runner with no shell wrapping."
                        .to_string(),
                ),
                icons: None,
                website_url: None,
            },
            instructions: Some(
                "Use run_network_tool with executable/args/cwd/env. Requests are validated against POLICY_FILE."
                    .to_string(),
            ),
            ..Default::default()
        }
    }
}

pub fn build_app(policy: Arc<Policy>, default_cwd: PathBuf) -> Router {
    let session_manager = Arc::new(LocalSessionManager::default());
    let policy_for_factory = policy.clone();
    let cwd_for_factory = default_cwd.clone();

    let mcp_service = StreamableHttpService::new(
        move || {
            Ok(NetworkMcpServer::new(
                policy_for_factory.clone(),
                cwd_for_factory.clone(),
            ))
        },
        session_manager,
        StreamableHttpServerConfig::default(),
    );

    Router::new().route_service("/mcp", any_service(mcp_service))
}

pub async fn serve(config: AppConfig) -> Result<(), AppError> {
    let policy = load_policy(&config.policy_file).map_err(|source| AppError::PolicyLoad {
        path: config.policy_file.clone(),
        source,
    })?;

    tracing::info!(
        bind_addr = %config.bind_addr,
        policy_file = %config.policy_file.display(),
        commands = policy.len(),
        "starting network MCP server",
    );

    let app = build_app(Arc::new(policy), config.default_cwd.clone());
    let listener = tokio::net::TcpListener::bind(config.bind_addr).await?;
    axum::serve(listener, app).await?;
    Ok(())
}

pub fn tool_error_result(message: impl Into<String>) -> CallToolResult {
    CallToolResult::structured_error(serde_json::json!({ "error": message.into() }))
}

#[cfg(test)]
mod tests {
    use super::*;
    use rmcp::ServiceExt;
    use rmcp::model::CallToolRequestParams;
    use rmcp::transport::StreamableHttpClientTransport;
    use tempfile::NamedTempFile;

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

    fn parse_env_output(output: &str) -> BTreeMap<String, String> {
        output
            .lines()
            .filter_map(|line| {
                line.split_once('=')
                    .map(|(key, value)| (key.to_string(), value.to_string()))
            })
            .collect()
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

        assert!(
            validate_invocation(&policy, "npm", &["test".to_string()], &BTreeMap::new()).is_ok()
        );

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
    fn build_command_env_merges_user_and_enforces_critical_values() {
        let user_env = BTreeMap::from([
            ("CUSTOM_USER_ENV".to_string(), "allowed".to_string()),
            ("HOME".to_string(), "user-home".to_string()),
            ("LANG".to_string(), "user-lang".to_string()),
            ("PATH".to_string(), "user-path".to_string()),
            ("http_proxy".to_string(), "user-http".to_string()),
            ("https_proxy".to_string(), "user-https".to_string()),
            ("no_proxy".to_string(), "user-no".to_string()),
            ("HTTP_PROXY".to_string(), "user-http-upper".to_string()),
            ("HTTPS_PROXY".to_string(), "user-https-upper".to_string()),
            ("NO_PROXY".to_string(), "user-no-upper".to_string()),
        ]);

        let merged = build_command_env(&user_env);

        assert_eq!(
            merged.get("CUSTOM_USER_ENV").map(String::as_str),
            Some("allowed")
        );
        assert_eq!(merged.get("HOME").map(String::as_str), Some("user-home"));
        assert_eq!(merged.get("LANG").map(String::as_str), Some("user-lang"));

        match std::env::var("PATH") {
            Ok(path) => assert_eq!(merged.get("PATH"), Some(&path)),
            Err(_) => assert!(!merged.contains_key("PATH")),
        }

        match std::env::var("http_proxy").ok() {
            Some(value) => {
                assert_eq!(merged.get("http_proxy"), Some(&value));
                assert_eq!(merged.get("HTTP_PROXY"), Some(&value));
            }
            None => {
                assert!(!merged.contains_key("http_proxy"));
                assert!(!merged.contains_key("HTTP_PROXY"));
            }
        }

        match std::env::var("https_proxy").ok() {
            Some(value) => {
                assert_eq!(merged.get("https_proxy"), Some(&value));
                assert_eq!(merged.get("HTTPS_PROXY"), Some(&value));
            }
            None => {
                assert!(!merged.contains_key("https_proxy"));
                assert!(!merged.contains_key("HTTPS_PROXY"));
            }
        }

        match std::env::var("no_proxy").ok() {
            Some(value) => {
                assert_eq!(merged.get("no_proxy"), Some(&value));
                assert_eq!(merged.get("NO_PROXY"), Some(&value));
            }
            None => {
                assert!(!merged.contains_key("no_proxy"));
                assert!(!merged.contains_key("NO_PROXY"));
            }
        }
    }

    #[tokio::test]
    async fn executes_allowed_command_successfully() {
        let env_path = match find_executable("env") {
            Some(path) => path,
            None => return,
        };

        let policy: Policy = vec![CommandRule {
            command: env_path.clone(),
            args: vec![
                ArgCheck::Exact {
                    value: "printf".to_string(),
                    position: Some(0),
                    required: Some(true),
                },
                ArgCheck::Exact {
                    value: "ok".to_string(),
                    position: Some(1),
                    required: Some(true),
                },
            ],
            env: vec![],
            description: None,
        }];

        let output = run_network_tool_impl(
            &policy,
            Path::new("."),
            RunNetworkToolInput {
                executable: env_path,
                args: vec!["printf".to_string(), "ok".to_string()],
                cwd: None,
                env: None,
            },
        )
        .await
        .expect("command should run");

        assert_eq!(output.exit_code, Some(0));
        assert_eq!(output.stdout, "ok");
        assert_eq!(output.stderr, "");
    }

    #[tokio::test]
    async fn command_runs_with_sanitized_environment() {
        let env_path = match find_executable("env") {
            Some(path) => path,
            None => return,
        };

        let policy: Policy = vec![CommandRule {
            command: env_path.clone(),
            args: vec![],
            env: vec![
                "CUSTOM_USER_ENV".to_string(),
                "HOME".to_string(),
                "LANG".to_string(),
                "PATH".to_string(),
                "http_proxy".to_string(),
                "https_proxy".to_string(),
                "no_proxy".to_string(),
                "HTTP_PROXY".to_string(),
                "HTTPS_PROXY".to_string(),
                "NO_PROXY".to_string(),
            ],
            description: None,
        }];

        let output = run_network_tool_impl(
            &policy,
            Path::new("."),
            RunNetworkToolInput {
                executable: env_path,
                args: vec![],
                cwd: None,
                env: Some(BTreeMap::from([
                    ("CUSTOM_USER_ENV".to_string(), "allowed".to_string()),
                    ("HOME".to_string(), "user-home".to_string()),
                    ("LANG".to_string(), "user-lang".to_string()),
                    ("PATH".to_string(), "user-path".to_string()),
                    ("http_proxy".to_string(), "user-http".to_string()),
                    ("https_proxy".to_string(), "user-https".to_string()),
                    ("no_proxy".to_string(), "user-no".to_string()),
                    ("HTTP_PROXY".to_string(), "user-http-upper".to_string()),
                    ("HTTPS_PROXY".to_string(), "user-https-upper".to_string()),
                    ("NO_PROXY".to_string(), "user-no-upper".to_string()),
                ])),
            },
        )
        .await
        .expect("env should run");

        let command_env = parse_env_output(&output.stdout);

        assert_eq!(
            command_env.get("CUSTOM_USER_ENV").map(String::as_str),
            Some("allowed")
        );
        assert_eq!(
            command_env.get("HOME").map(String::as_str),
            Some("user-home")
        );
        assert_eq!(
            command_env.get("LANG").map(String::as_str),
            Some("user-lang")
        );
        assert!(!command_env.contains_key("PWD"));

        match std::env::var("PATH") {
            Ok(path) => assert_eq!(command_env.get("PATH"), Some(&path)),
            Err(_) => assert!(!command_env.contains_key("PATH")),
        }

        match std::env::var("http_proxy").ok() {
            Some(value) => {
                assert_eq!(command_env.get("http_proxy"), Some(&value));
                assert_eq!(command_env.get("HTTP_PROXY"), Some(&value));
            }
            None => {
                assert!(!command_env.contains_key("http_proxy"));
                assert!(!command_env.contains_key("HTTP_PROXY"));
            }
        }

        match std::env::var("https_proxy").ok() {
            Some(value) => {
                assert_eq!(command_env.get("https_proxy"), Some(&value));
                assert_eq!(command_env.get("HTTPS_PROXY"), Some(&value));
            }
            None => {
                assert!(!command_env.contains_key("https_proxy"));
                assert!(!command_env.contains_key("HTTPS_PROXY"));
            }
        }

        match std::env::var("no_proxy").ok() {
            Some(value) => {
                assert_eq!(command_env.get("no_proxy"), Some(&value));
                assert_eq!(command_env.get("NO_PROXY"), Some(&value));
            }
            None => {
                assert!(!command_env.contains_key("no_proxy"));
                assert!(!command_env.contains_key("NO_PROXY"));
            }
        }
    }

    #[tokio::test]
    async fn blocks_disallowed_command_execution() {
        let env_path = match find_executable("env") {
            Some(path) => path,
            None => return,
        };

        let policy: Policy = vec![CommandRule {
            command: env_path,
            args: vec![],
            env: vec![],
            description: None,
        }];

        let error = run_network_tool_impl(
            &policy,
            Path::new("."),
            RunNetworkToolInput {
                executable: "echo".to_string(),
                args: vec!["blocked".to_string()],
                cwd: None,
                env: None,
            },
        )
        .await
        .expect_err("disallowed command should fail");

        assert!(error.to_string().contains("Command not allowed"));
    }

    #[tokio::test]
    async fn truncates_stdout_at_one_mb() {
        let head_path = match find_executable("head") {
            Some(path) => path,
            None => return,
        };

        let policy: Policy = vec![CommandRule {
            command: head_path.clone(),
            args: vec![
                ArgCheck::Exact {
                    value: "-c".to_string(),
                    position: Some(0),
                    required: Some(true),
                },
                ArgCheck::Exact {
                    value: (MAX_OUTPUT_BYTES + 5).to_string(),
                    position: Some(1),
                    required: Some(true),
                },
                ArgCheck::Exact {
                    value: "/dev/zero".to_string(),
                    position: Some(2),
                    required: Some(true),
                },
            ],
            env: vec![],
            description: None,
        }];

        let output = run_network_tool_impl(
            &policy,
            Path::new("."),
            RunNetworkToolInput {
                executable: head_path,
                args: vec![
                    "-c".to_string(),
                    (MAX_OUTPUT_BYTES + 5).to_string(),
                    "/dev/zero".to_string(),
                ],
                cwd: None,
                env: None,
            },
        )
        .await
        .expect("head should run");

        assert_eq!(output.exit_code, Some(0));
        assert!(output.stdout.ends_with(TRUNCATION_MARKER));
    }

    #[tokio::test]
    async fn mcp_http_sse_smoke_tool_invocation() {
        let env_path = match find_executable("env") {
            Some(path) => path,
            None => return,
        };

        let policy: Policy = vec![CommandRule {
            command: env_path.clone(),
            args: vec![
                ArgCheck::Exact {
                    value: "printf".to_string(),
                    position: Some(0),
                    required: Some(true),
                },
                ArgCheck::Exact {
                    value: "smoke".to_string(),
                    position: Some(1),
                    required: Some(true),
                },
            ],
            env: vec![],
            description: None,
        }];

        let app = build_app(
            Arc::new(policy),
            std::env::current_dir().expect("current dir"),
        );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind test listener");
        let addr = listener.local_addr().expect("listener addr");

        let server_task = tokio::spawn(async move {
            let _ = axum::serve(listener, app).await;
        });

        let url = format!("http://{addr}/mcp");
        let client =
            ().serve(StreamableHttpClientTransport::from_uri(url))
                .await
                .expect("connect MCP client");

        let tools = client.list_tools(None).await.expect("list tools");
        assert!(
            tools
                .tools
                .iter()
                .any(|tool| tool.name == "run_network_tool")
        );

        let arguments = serde_json::json!({
            "executable": env_path,
            "args": ["printf", "smoke"]
        })
        .as_object()
        .cloned();

        let call_result = client
            .call_tool(CallToolRequestParams {
                meta: None,
                name: "run_network_tool".to_string().into(),
                arguments,
                task: None,
            })
            .await
            .expect("invoke run_network_tool");

        let typed: RunNetworkToolOutput = call_result.into_typed().expect("typed response");
        assert_eq!(typed.stdout, "smoke");
        assert_eq!(typed.exit_code, Some(0));

        client.cancel().await.expect("cancel client");
        server_task.abort();
    }
}
