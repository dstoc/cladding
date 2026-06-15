mod executor;
mod mcp;
mod policy;
mod raw;
mod remote;

pub use executor::{
    MAX_OUTPUT_BYTES, RunCommandCheckOutput, RunCommandInput, RunCommandOutput, TRUNCATION_MARKER,
    ToolError, check_command, run_command_impl, spawn_command_process,
};
pub use mcp::{
    AppConfig, AppError, ConfigError, DEFAULT_BIND_ADDR, McpRunServer, build_app, serve,
    tool_error_result,
};
pub use policy::{PolicyEngine, PolicyMode, ValidationError};
pub use raw::{RawEndpointState, RawErrorBody, RawStreamEvent, raw_handler};
pub use remote::{LOCAL_FAILURE_EXIT_CODE, RemoteClientError, run_remote_from_env};
