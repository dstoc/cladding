use std::net::{AddrParseError, SocketAddr};
use std::path::PathBuf;
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

use crate::executor::{RunNetworkToolInput, RunNetworkToolOutput, run_network_tool_impl};
use crate::policy::{PolicyEngine, PolicyMode};
use crate::raw::{RawEndpointState, raw_handler};

pub const DEFAULT_BIND_ADDR: &str = "127.0.0.1:8000";

#[derive(Debug, Clone)]
pub struct AppConfig {
    pub bind_addr: SocketAddr,
    pub policy_dir: Option<PathBuf>,
    pub policy_file: Option<PathBuf>,
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
        let policy_dir = std::env::var("POLICY_DIR")
            .ok()
            .map(|value| value.trim().to_string())
            .filter(|value| !value.is_empty())
            .map(PathBuf::from);
        let policy_file = std::env::var("POLICY_FILE")
            .ok()
            .map(|value| value.trim().to_string())
            .filter(|value| !value.is_empty())
            .map(PathBuf::from);
        let default_cwd =
            std::env::current_dir().map_err(|source| ConfigError::CurrentDir { source })?;

        Ok(Self {
            bind_addr,
            policy_dir,
            policy_file,
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
    #[error("failed to get current working directory: {source}")]
    CurrentDir { source: std::io::Error },
}

#[derive(Debug, Error)]
pub enum AppError {
    #[error(transparent)]
    Config(#[from] ConfigError),
    #[error("server I/O failure: {0}")]
    Io(#[from] std::io::Error),
}

#[derive(Clone)]
pub struct NetworkMcpServer {
    policy_engine: Arc<PolicyEngine>,
    default_cwd: PathBuf,
    tool_router: ToolRouter<Self>,
}

#[tool_router]
impl NetworkMcpServer {
    pub fn new(policy_engine: Arc<PolicyEngine>, default_cwd: PathBuf) -> Self {
        Self {
            policy_engine,
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
        run_network_tool_impl(&self.policy_engine, &self.default_cwd, input)
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
                "Use run_network_tool with executable/args/cwd/env. Requests are validated against POLICY_DIR (Rego) or POLICY_FILE (legacy JSON)."
                    .to_string(),
            ),
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
            Ok(NetworkMcpServer::new(
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
        .with_state(raw_state)
}

pub async fn serve(config: AppConfig) -> Result<(), AppError> {
    let policy_engine = Arc::new(PolicyEngine::from_sources(
        config.policy_dir.clone(),
        config.policy_file.clone(),
    ));
    policy_engine.start_watcher();

    tracing::info!(
        bind_addr = %config.bind_addr,
        policy_mode = match policy_engine.mode() {
            PolicyMode::Rego => "rego",
            PolicyMode::LegacyJson => "legacy-json",
            PolicyMode::DenyAll => "deny-all",
        },
        policy_dir = ?config.policy_dir.as_ref().map(|path| path.display().to_string()),
        policy_file = ?config.policy_file.as_ref().map(|path| path.display().to_string()),
        "starting network MCP server",
    );

    let app = build_app(policy_engine, config.default_cwd.clone());
    let listener = tokio::net::TcpListener::bind(config.bind_addr).await?;
    axum::serve(listener, app).await?;
    Ok(())
}

pub fn tool_error_result(message: impl Into<String>) -> CallToolResult {
    CallToolResult::structured_error(serde_json::json!({ "error": message.into() }))
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use super::*;
    use crate::executor::{MAX_OUTPUT_BYTES, RunNetworkToolOutput, TRUNCATION_MARKER};
    use crate::policy::{ArgCheck, CommandRule, Policy, PolicyEngine};
    use rmcp::ServiceExt;
    use rmcp::model::CallToolRequestParams;
    use rmcp::transport::StreamableHttpClientTransport;

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

        let policy_engine = PolicyEngine::from_legacy_policy_for_tests(policy);
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

    #[tokio::test]
    async fn mcp_tool_output_still_truncates_at_one_mb() {
        let head_path = match find_executable("head") {
            Some(path) => path,
            None => return,
        };

        let requested = MAX_OUTPUT_BYTES + 5;
        let policy: Policy = vec![CommandRule {
            command: head_path.clone(),
            args: vec![
                ArgCheck::Exact {
                    value: "-c".to_string(),
                    position: Some(0),
                    required: Some(true),
                },
                ArgCheck::Exact {
                    value: requested.to_string(),
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

        let policy_engine = PolicyEngine::from_legacy_policy_for_tests(policy);
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
                name: "run_network_tool".to_string().into(),
                arguments,
                task: None,
            })
            .await
            .expect("invoke run_network_tool");

        let typed: RunNetworkToolOutput = call_result.into_typed().expect("typed response");
        assert!(typed.stdout.ends_with(TRUNCATION_MARKER));
        assert_eq!(typed.exit_code, Some(0));

        client.cancel().await.expect("cancel client");
        server_task.abort();
    }
}
