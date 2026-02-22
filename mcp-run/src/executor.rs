use std::collections::BTreeMap;
use std::path::Path;
use std::process::Stdio;

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use thiserror::Error;
use tokio::io::AsyncReadExt;
use tokio::process::{Child, Command};

use crate::policy::{PolicyEngine, ValidationError};

pub const MAX_OUTPUT_BYTES: usize = 1024 * 1024;
pub const TRUNCATION_MARKER: &str = "\n...truncated...";

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
    policy_engine: &PolicyEngine,
    default_cwd: &Path,
    input: RunNetworkToolInput,
) -> Result<RunNetworkToolOutput, ToolError> {
    let mut child = spawn_network_tool_process(policy_engine, default_cwd, input)?;

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

pub fn spawn_network_tool_process(
    policy_engine: &PolicyEngine,
    default_cwd: &Path,
    input: RunNetworkToolInput,
) -> Result<Child, ToolError> {
    let user_env = input.env.unwrap_or_default();
    policy_engine.validate_invocation(&input.executable, &input.args, &user_env)?;

    let mut command = Command::new(&input.executable);
    command
        .args(&input.args)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true);

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

    command
        .spawn()
        .map_err(|source| ToolError::Spawn { source })
}

pub(crate) fn build_command_env(user_env: &BTreeMap<String, String>) -> BTreeMap<String, String> {
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

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;
    use std::path::Path;

    use super::*;
    use crate::policy::{ArgCheck, CommandRule, Policy, PolicyEngine};

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

        let policy_engine = PolicyEngine::from_legacy_policy_for_tests(policy);
        let output = run_network_tool_impl(
            &policy_engine,
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

        let policy_engine = PolicyEngine::from_legacy_policy_for_tests(policy);
        let output = run_network_tool_impl(
            &policy_engine,
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

        let policy_engine = PolicyEngine::from_legacy_policy_for_tests(policy);
        let error = run_network_tool_impl(
            &policy_engine,
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

        let policy_engine = PolicyEngine::from_legacy_policy_for_tests(policy);
        let output = run_network_tool_impl(
            &policy_engine,
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
}
