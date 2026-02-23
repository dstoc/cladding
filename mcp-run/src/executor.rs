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
    let resolved_executable =
        resolve_executable_path(&input.executable).map_err(|details| ToolError::Validation(
            ValidationError::PathResolutionFailed {
                command: input.executable.clone(),
                details,
            },
        ))?;
    policy_engine.validate_invocation(
        &input.executable,
        &resolved_executable,
        &input.args,
        &user_env,
    )?;

    let mut command = Command::new(&resolved_executable);
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

pub(crate) fn resolve_executable_path(command: &str) -> Result<String, String> {
    if command.contains('/') {
        let path = std::path::Path::new(command);
        let candidate = if path.is_absolute() {
            path.to_path_buf()
        } else {
            std::env::current_dir()
                .map_err(|error| format!("failed resolving cwd: {error}"))?
                .join(path)
        };
        if !candidate.is_file() {
            return Err(format!("'{}' is not a file", candidate.display()));
        }

        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let metadata = candidate.metadata().map_err(|error| {
                format!("failed reading metadata for '{}': {error}", candidate.display())
            })?;
            if metadata.permissions().mode() & 0o111 == 0 {
                return Err(format!("'{}' is not executable", candidate.display()));
            }
        }

        return Ok(candidate.to_string_lossy().into_owned());
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

        return Ok(candidate.to_string_lossy().into_owned());
    }

    Err(format!("'{}' was not found on PATH", command))
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
    use crate::policy::PolicyEngine;

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

    #[cfg(unix)]
    #[test]
    fn resolve_executable_path_preserves_symlink_in_path_lookup() {
        use std::os::unix::fs::{symlink, PermissionsExt};

        let temp = tempfile::tempdir().expect("tempdir");
        let real_bin = temp.path().join("real-bin");
        std::fs::write(&real_bin, b"#!/bin/sh\necho ok\n").expect("write real bin");
        let mut perms = std::fs::metadata(&real_bin)
            .expect("metadata")
            .permissions();
        perms.set_mode(0o755);
        std::fs::set_permissions(&real_bin, perms).expect("set perms");

        let link_path = temp.path().join("cargo");
        symlink(&real_bin, &link_path).expect("symlink");

        let original_path = std::env::var_os("PATH");
        unsafe {
            std::env::set_var("PATH", temp.path());
        }
        let resolved = resolve_executable_path("cargo").expect("resolve");
        if let Some(value) = original_path {
            unsafe {
                std::env::set_var("PATH", value);
            }
        } else {
            unsafe {
                std::env::remove_var("PATH");
            }
        }

        assert_eq!(resolved, link_path.to_string_lossy());
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

        let policy_engine = rego_engine_allow_commands(&[&env_path]);
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

        let policy_engine = rego_engine_allow_commands(&[&env_path]);
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

        let policy_engine = rego_engine_allow_commands(&[&env_path]);
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

        let policy_engine = rego_engine_allow_commands(&[&head_path]);
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
