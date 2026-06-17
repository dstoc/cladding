use std::fs;
use std::net::{AddrParseError, SocketAddr};
use std::path::{Path, PathBuf};
use std::sync::Arc;

use axum::Router;
use axum::routing::{any_service, post};
use rmcp::handler::server::tool::ToolRouter;
use rmcp::handler::server::wrapper::Parameters;
use rmcp::model::{CallToolResult, Implementation, ServerCapabilities, ServerInfo};
use rmcp::transport::streamable_http_server::session::local::LocalSessionManager;
use rmcp::transport::{StreamableHttpServerConfig, StreamableHttpService};
use rmcp::{Json, ServerHandler, tool, tool_handler, tool_router};
use thiserror::Error;

use crate::executor::{
    RunCommandCheckOutput, RunCommandInput, RunCommandOutput, check_command as check_command_impl,
    run_command_impl,
};
use crate::policy::{PolicyEngine, PolicyMode};
use crate::raw::{RawEndpointState, check_handler, raw_handler};

pub const DEFAULT_BIND_ADDR: &str = "127.0.0.1:8000";

#[derive(Debug, Clone)]
pub struct AppConfig {
    pub bind_addr: Option<SocketAddr>,
    pub bind_uds: Option<PathBuf>,
    pub policy_dir: Option<PathBuf>,
    pub default_cwd: PathBuf,
}

impl AppConfig {
    pub fn from_env() -> Result<Self, ConfigError> {
        Self::from_env_with(|name| std::env::var(name).ok())
    }

    fn from_env_with<F>(mut lookup: F) -> Result<Self, ConfigError>
    where
        F: FnMut(&str) -> Option<String>,
    {
        let bind_uds_raw = normalize_env_value(lookup("MCP_BIND_UDS"));
        let bind_addr_raw = normalize_env_value(lookup("MCP_BIND_ADDR"));

        if bind_uds_raw.is_some() && bind_addr_raw.is_some() {
            return Err(ConfigError::ConflictingBindTargets);
        }

        let bind_uds = bind_uds_raw
            .map(PathBuf::from)
            .map(validate_bind_uds_config)
            .transpose()?;

        let bind_addr =
            if bind_uds.is_some() {
                None
            } else {
                let bind_raw = bind_addr_raw.unwrap_or_else(|| DEFAULT_BIND_ADDR.into());
                Some(bind_raw.parse::<SocketAddr>().map_err(|source| {
                    ConfigError::InvalidBindAddr {
                        value: bind_raw,
                        source,
                    }
                })?)
            };

        let policy_dir = lookup("POLICY_DIR")
            .map(|value| value.trim().to_string())
            .filter(|value| !value.is_empty())
            .map(PathBuf::from);
        let default_cwd =
            std::env::current_dir().map_err(|source| ConfigError::CurrentDir { source })?;
        validate_default_cwd(&default_cwd)?;

        Ok(Self {
            bind_addr,
            bind_uds,
            policy_dir,
            default_cwd,
        })
    }
}

fn validate_default_cwd(default_cwd: &Path) -> Result<(), ConfigError> {
    if default_cwd.is_absolute() {
        Ok(())
    } else {
        Err(ConfigError::RelativeDefaultCwd {
            cwd: default_cwd.to_path_buf(),
        })
    }
}

fn normalize_env_value(raw: Option<String>) -> Option<String> {
    raw.map(|value| value.trim().to_string())
        .filter(|value| !value.is_empty())
}

fn validate_bind_uds_config(bind_uds: PathBuf) -> Result<PathBuf, ConfigError> {
    if !bind_uds.is_absolute() {
        return Err(ConfigError::RelativeBindUds {
            value: bind_uds.to_string_lossy().into_owned(),
        });
    }

    let parent = bind_uds
        .parent()
        .ok_or_else(|| ConfigError::MissingBindUdsParent {
            value: bind_uds.to_string_lossy().into_owned(),
        })?;

    if !parent.is_dir() {
        return Err(ConfigError::MissingBindUdsParent {
            value: bind_uds.to_string_lossy().into_owned(),
        });
    }

    Ok(bind_uds)
}

fn prepare_bind_uds_path(bind_uds: &Path) -> Result<(), ConfigError> {
    let metadata = match fs::symlink_metadata(bind_uds) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(error) => return Err(ConfigError::BindUdsMetadata { source: error }),
    };

    if !is_unix_socket(&metadata) {
        return Err(ConfigError::BindUdsNotSocket {
            path: bind_uds.to_path_buf(),
        });
    }

    fs::remove_file(bind_uds).map_err(|source| ConfigError::BindUdsRemove { source })?;
    Ok(())
}

#[cfg(unix)]
fn is_unix_socket(metadata: &fs::Metadata) -> bool {
    use std::os::unix::fs::FileTypeExt;

    metadata.file_type().is_socket()
}

#[cfg(not(unix))]
fn is_unix_socket(_metadata: &fs::Metadata) -> bool {
    false
}

#[derive(Debug, Error)]
pub enum ConfigError {
    #[error("MCP_BIND_ADDR and MCP_BIND_UDS cannot both be set")]
    ConflictingBindTargets,
    #[error("invalid MCP_BIND_ADDR '{value}': {source}")]
    InvalidBindAddr {
        value: String,
        source: AddrParseError,
    },
    #[error("MCP_BIND_UDS must be an absolute path: '{value}'")]
    RelativeBindUds { value: String },
    #[error("MCP_BIND_UDS parent directory does not exist: '{value}'")]
    MissingBindUdsParent { value: String },
    #[error("MCP_BIND_UDS path exists but is not a unix socket: {path:?}")]
    BindUdsNotSocket { path: PathBuf },
    #[error("failed to inspect MCP_BIND_UDS path: {source}")]
    BindUdsMetadata { source: std::io::Error },
    #[error("failed to remove stale MCP_BIND_UDS socket: {source}")]
    BindUdsRemove { source: std::io::Error },
    #[error("MCP_BIND_UDS is not supported on this platform")]
    BindUdsUnsupportedPlatform,
    #[error("failed to get current working directory: {source}")]
    CurrentDir { source: std::io::Error },
    #[error("default cwd must be absolute: {cwd:?}")]
    RelativeDefaultCwd { cwd: PathBuf },
}

#[derive(Debug, Error)]
pub enum AppError {
    #[error(transparent)]
    Config(#[from] ConfigError),
    #[error("server I/O failure: {0}")]
    Io(#[from] std::io::Error),
}

#[derive(Clone)]
pub struct McpRunServer {
    policy_engine: Arc<PolicyEngine>,
    default_cwd: PathBuf,
    tool_router: ToolRouter<Self>,
}

#[tool_router]
impl McpRunServer {
    pub fn new(policy_engine: Arc<PolicyEngine>, default_cwd: PathBuf) -> Self {
        Self {
            policy_engine,
            default_cwd,
            tool_router: Self::tool_router(),
        }
    }

    #[tool(
        name = "run_command",
        description = "Execute a policy-allowlisted command without shell wrappers."
    )]
    async fn run_command(
        &self,
        Parameters(input): Parameters<RunCommandInput>,
    ) -> Result<Json<RunCommandOutput>, String> {
        run_command_impl(&self.policy_engine, &self.default_cwd, input)
            .await
            .map(Json)
            .map_err(|error| error.to_string())
    }

    #[tool(
        name = "check_command",
        description = "Check whether a command request would be allowed without executing it."
    )]
    async fn check_command(
        &self,
        Parameters(input): Parameters<RunCommandInput>,
    ) -> Result<Json<RunCommandCheckOutput>, String> {
        Ok(Json(check_command_impl(
            &self.policy_engine,
            &self.default_cwd,
            input,
        )))
    }
}

#[tool_handler]
impl ServerHandler for McpRunServer {
    fn get_info(&self) -> ServerInfo {
        ServerInfo {
            capabilities: ServerCapabilities::builder().enable_tools().build(),
            server_info: Implementation {
                name: "mcp-run".to_string(),
                title: Some("mcp-run".to_string()),
                version: env!("CARGO_PKG_VERSION").to_string(),
                description: Some("Policy-enforced command runner with no shell wrapping.".to_string()),
                icons: None,
                website_url: None,
            },
            instructions: Some("Use run_command to execute allowlisted commands and check_command to inspect policy decisions. Requests are validated against POLICY_DIR Rego policy modules.".to_string()),
            ..Default::default()
        }
    }
}

pub fn build_app(policy_engine: Arc<PolicyEngine>, default_cwd: PathBuf) -> Router {
    let session_manager = Arc::new(LocalSessionManager::default());
    let policy_for_factory = policy_engine.clone();
    let cwd_for_factory = default_cwd.clone();
    let raw_state = RawEndpointState {
        policy_engine,
        default_cwd,
    };

    let mcp_service = StreamableHttpService::new(
        move || {
            Ok(McpRunServer::new(
                policy_for_factory.clone(),
                cwd_for_factory.clone(),
            ))
        },
        session_manager,
        StreamableHttpServerConfig::default(),
    );

    Router::new()
        .route_service("/mcp", any_service(mcp_service))
        .route("/raw", post(raw_handler))
        .route("/check", post(check_handler))
        .with_state(raw_state)
}

pub async fn serve(config: AppConfig) -> Result<(), AppError> {
    validate_default_cwd(&config.default_cwd)?;

    let policy_engine = Arc::new(PolicyEngine::from_sources(config.policy_dir.clone()));
    policy_engine.start_watcher();

    tracing::info!(
        bind_addr = ?config.bind_addr,
        bind_uds = ?config.bind_uds.as_ref().map(|path| path.display().to_string()),
        policy_mode = match policy_engine.mode() {
            PolicyMode::Rego => "rego",
            PolicyMode::DenyAll => "deny-all",
        },
        policy_dir = ?config.policy_dir.as_ref().map(|path| path.display().to_string()),
        "starting mcp-run server",
    );

    let app = build_app(policy_engine, config.default_cwd.clone());
    if let Some(bind_uds) = config.bind_uds.as_deref() {
        #[cfg(not(unix))]
        {
            let _ = bind_uds;
            return Err(ConfigError::BindUdsUnsupportedPlatform.into());
        }

        #[cfg(unix)]
        {
            prepare_bind_uds_path(bind_uds)?;
            let listener = tokio::net::UnixListener::bind(bind_uds)?;
            axum::serve(listener, app).await?;
        }
    } else {
        let bind_addr = config.bind_addr.unwrap_or_else(|| {
            DEFAULT_BIND_ADDR
                .parse()
                .expect("default bind address is valid")
        });
        let listener = tokio::net::TcpListener::bind(bind_addr).await?;
        axum::serve(listener, app).await?;
    }
    Ok(())
}

pub fn tool_error_result(message: impl Into<String>) -> CallToolResult {
    CallToolResult::structured_error(serde_json::json!({ "error": message.into() }))
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use super::*;
    use crate::executor::{
        MAX_OUTPUT_BYTES, RunCommandCheckOutput, RunCommandOutput, TRUNCATION_MARKER,
    };
    use crate::policy::PolicyEngine;
    use rmcp::ServiceExt;
    use rmcp::model::CallToolRequestParams;
    use rmcp::transport::StreamableHttpClientTransport;
    use std::fs;
    use std::time::Duration;

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

    fn rego_engine_allow_commands(commands: &[&str]) -> PolicyEngine {
        let mut allowed_map = String::new();
        for command in commands {
            let escaped = command.replace('\\', "\\\\").replace('\"', "\\\"");
            allowed_map.push_str(&format!("  \"{escaped}\": true,\n"));
        }

        let main = format!(
            "package sandbox.main\n\ndefault allow = false\n\nallowed_commands := {{\n{allowed_map}}}\n\nallow if {{\n  allowed_commands[input.command]\n}}\n"
        );

        PolicyEngine::from_rego_for_tests(&[("main.rego", &main)])
    }

    fn config_from_env(
        bind_addr: Option<&str>,
        bind_uds: Option<&str>,
    ) -> Result<AppConfig, ConfigError> {
        AppConfig::from_env_with(|name| match name {
            "MCP_BIND_ADDR" => bind_addr.map(|value| value.to_string()),
            "MCP_BIND_UDS" => bind_uds.map(|value| value.to_string()),
            _ => None,
        })
    }

    #[tokio::test]
    async fn serve_rejects_relative_default_cwd() {
        let config = AppConfig {
            bind_addr: Some("127.0.0.1:0".parse().expect("bind addr")),
            bind_uds: None,
            policy_dir: None,
            default_cwd: PathBuf::from("."),
        };

        let error = tokio::time::timeout(Duration::from_secs(1), serve(config))
            .await
            .expect("serve validation returned")
            .expect_err("relative default cwd rejected");

        assert!(matches!(
            error,
            AppError::Config(ConfigError::RelativeDefaultCwd { .. })
        ));
    }

    #[test]
    fn from_env_rejects_conflicting_bind_targets() {
        let error = config_from_env(Some("127.0.0.1:0"), Some("/tmp/mcp-run.sock"))
            .expect_err("conflicting bind targets rejected");

        assert!(matches!(error, ConfigError::ConflictingBindTargets));
    }

    #[test]
    fn from_env_ignores_empty_bind_values_before_conflict_check() {
        let tempdir = tempfile::tempdir().expect("tempdir");
        let socket_path = tempdir.path().join("mcp-run.sock");

        let config = AppConfig::from_env_with(|name| match name {
            "MCP_BIND_ADDR" => Some("   ".to_string()),
            "MCP_BIND_UDS" => Some(socket_path.to_string_lossy().into_owned()),
            _ => None,
        })
        .expect("empty addr should not conflict");

        assert!(config.bind_addr.is_none());
        assert_eq!(config.bind_uds, Some(socket_path));
    }

    #[test]
    fn from_env_rejects_relative_uds_path() {
        let error =
            config_from_env(None, Some("relative.sock")).expect_err("relative uds path rejected");

        assert!(matches!(error, ConfigError::RelativeBindUds { .. }));
    }

    #[test]
    fn from_env_rejects_missing_uds_parent() {
        let path = PathBuf::from("/definitely-missing-parent/mcp-run.sock");
        let error = config_from_env(None, Some(path.to_str().expect("path string")))
            .expect_err("missing parent rejected");

        assert!(matches!(error, ConfigError::MissingBindUdsParent { .. }));
    }

    #[tokio::test]
    async fn serve_rejects_existing_non_socket_path() {
        let tempdir = tempfile::tempdir().expect("tempdir");
        let socket_path = tempdir.path().join("mcp-run.sock");
        fs::write(&socket_path, b"not a socket").expect("create stale file");

        let config = AppConfig {
            bind_addr: None,
            bind_uds: Some(socket_path.clone()),
            policy_dir: None,
            default_cwd: std::env::current_dir().expect("current dir"),
        };

        let error = tokio::time::timeout(Duration::from_secs(1), serve(config))
            .await
            .expect("serve validation returned")
            .expect_err("non-socket path rejected");

        assert!(matches!(
            error,
            AppError::Config(ConfigError::BindUdsNotSocket { path }) if path == socket_path
        ));
    }

    #[tokio::test]
    async fn mcp_http_sse_smoke_tool_invocation() {
        let env_path = match find_executable("env") {
            Some(path) => path,
            None => return,
        };

        let policy_engine = rego_engine_allow_commands(&[&env_path]);
        let default_cwd = std::env::current_dir().expect("current dir");
        let expected_cwd = default_cwd.to_string_lossy().into_owned();
        let app = build_app(Arc::new(policy_engine), default_cwd);
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
        assert!(tools.tools.iter().any(|tool| tool.name == "run_command"));
        assert!(tools.tools.iter().any(|tool| tool.name == "check_command"));
        assert!(
            tools
                .tools
                .iter()
                .all(|tool| tool.name != "run_network_tool")
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
                name: "run_command".to_string().into(),
                arguments,
                task: None,
            })
            .await
            .expect("invoke run_command");

        let typed: RunCommandOutput = call_result.into_typed().expect("typed response");
        assert_eq!(typed.stdout, "smoke");
        assert_eq!(typed.exit_code, Some(0));

        let check_arguments = serde_json::json!({
            "executable": env_path,
            "args": ["printf", "check"]
        })
        .as_object()
        .cloned();

        let check_result = client
            .call_tool(CallToolRequestParams {
                meta: None,
                name: "check_command".to_string().into(),
                arguments: check_arguments,
                task: None,
            })
            .await
            .expect("invoke check_command");

        let check_typed: RunCommandCheckOutput =
            check_result.into_typed().expect("typed check response");
        assert!(check_typed.allowed);
        assert_eq!(check_typed.executable, env_path);
        assert!(check_typed.resolved_path.is_some());
        assert!(check_typed.hash.is_some());
        assert_eq!(check_typed.cwd.as_deref(), Some(expected_cwd.as_str()));

        client.cancel().await.expect("cancel client");
        server_task.abort();
    }

    #[tokio::test]
    async fn mcp_tool_output_still_truncates_at_one_mb() {
        let head_path = match find_executable("head") {
            Some(path) => path,
            None => return,
        };

        let requested = MAX_OUTPUT_BYTES + 5;
        let policy_engine = rego_engine_allow_commands(&[&head_path]);
        let app = build_app(
            Arc::new(policy_engine),
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

        let arguments = serde_json::json!({
            "executable": head_path,
            "args": ["-c", requested.to_string(), "/dev/zero"]
        })
        .as_object()
        .cloned();

        let call_result = client
            .call_tool(CallToolRequestParams {
                meta: None,
                name: "run_command".to_string().into(),
                arguments,
                task: None,
            })
            .await
            .expect("invoke run_command");

        let typed: RunCommandOutput = call_result.into_typed().expect("typed response");
        assert!(typed.stdout.ends_with(TRUNCATION_MARKER));
        assert_eq!(typed.exit_code, Some(0));

        client.cancel().await.expect("cancel client");
        server_task.abort();
    }

    #[tokio::test]
    async fn mcp_check_command_returns_structured_denial() {
        let true_path = match find_executable("true") {
            Some(path) => path,
            None => return,
        };

        let policy_engine = rego_engine_allow_commands(&[&true_path]);
        let app = build_app(
            Arc::new(policy_engine),
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

        let arguments = serde_json::json!({
            "executable": "echo",
            "args": ["blocked"]
        })
        .as_object()
        .cloned();

        let call_result = client
            .call_tool(CallToolRequestParams {
                meta: None,
                name: "check_command".to_string().into(),
                arguments,
                task: None,
            })
            .await
            .expect("invoke check_command");

        let typed: RunCommandCheckOutput = call_result.into_typed().expect("typed response");
        assert!(!typed.allowed);
        assert!(
            typed
                .reason
                .as_deref()
                .unwrap_or_default()
                .contains("Command not allowed")
        );
        assert!(typed.resolved_path.is_some());
        assert!(typed.hash.is_some());

        client.cancel().await.expect("cancel client");
        server_task.abort();
    }
}
