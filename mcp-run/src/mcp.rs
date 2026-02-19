use std::net::{AddrParseError, SocketAddr};
use std::path::PathBuf;
use std::sync::Arc;

use axum::Router;
use axum::routing::any_service;
use rmcp::handler::server::tool::ToolRouter;
use rmcp::handler::server::wrapper::Parameters;
use rmcp::model::{CallToolResult, Implementation, ServerCapabilities, ServerInfo};
use rmcp::transport::streamable_http_server::session::local::LocalSessionManager;
use rmcp::transport::{StreamableHttpServerConfig, StreamableHttpService};
use rmcp::{Json, ServerHandler, tool, tool_handler, tool_router};
use thiserror::Error;

use crate::executor::{RunNetworkToolInput, RunNetworkToolOutput, run_network_tool_impl};
use crate::policy::{Policy, PolicyLoadError, load_policy};

pub const DEFAULT_BIND_ADDR: &str = "127.0.0.1:8000";

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
    use std::sync::Arc;

    use super::*;
    use crate::executor::RunNetworkToolOutput;
    use crate::policy::{ArgCheck, CommandRule, Policy};
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
