use std::collections::BTreeMap;
use std::fmt::Write as _;
use std::io::Read;
use std::path::{Path, PathBuf};
use std::process::Stdio;

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use thiserror::Error;
use tokio::io::AsyncReadExt;
use tokio::process::{Child, Command};

use crate::policy::{PolicyEngine, ValidationError};

pub const MAX_OUTPUT_BYTES: usize = 1024 * 1024;
pub const TRUNCATION_MARKER: &str = "\n...truncated...";

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct RunCommandInput {
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
pub struct RunCommandOutput {
    pub stdout: String,
    pub stderr: String,
    #[serde(rename = "exitCode")]
    pub exit_code: Option<i32>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct RunCommandCheckOutput {
    pub allowed: bool,
    pub reason: Option<String>,
    pub executable: String,
    pub resolved_path: Option<String>,
    pub hash: Option<String>,
    #[serde(default)]
    pub cwd: Option<String>,
    pub policy_mode: String,
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

pub async fn run_command_impl(
    policy_engine: &PolicyEngine,
    default_cwd: &Path,
    input: RunCommandInput,
) -> Result<RunCommandOutput, ToolError> {
    let mut child = spawn_command_process(policy_engine, default_cwd, input)?;

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

    Ok(RunCommandOutput {
        stdout: finalize_capture(stdout_bytes, stdout_truncated),
        stderr: finalize_capture(stderr_bytes, stderr_truncated),
        exit_code: status.code(),
    })
}

pub fn spawn_command_process(
    policy_engine: &PolicyEngine,
    default_cwd: &Path,
    input: RunCommandInput,
) -> Result<Child, ToolError> {
    let authorization = authorize_command_request(policy_engine, default_cwd, input);
    if !authorization.decision.allowed {
        return Err(ToolError::Validation(
            authorization
                .validation_error
                .expect("authorization denial without validation error"),
        ));
    }

    let user_env = authorization.user_env;
    let resolved_executable = authorization
        .decision
        .resolved_path
        .as_deref()
        .expect("allowed authorization without resolved executable");
    let authorized_cwd = authorization
        .authorized_cwd
        .as_ref()
        .expect("allowed authorization without cwd");

    let mut command = Command::new(resolved_executable);
    command
        .args(&authorization.input.args)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true);

    set_authorized_cwd_for_spawn(&mut command, authorized_cwd);

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

pub fn check_command(
    policy_engine: &PolicyEngine,
    default_cwd: &Path,
    input: RunCommandInput,
) -> RunCommandCheckOutput {
    authorize_command_request(policy_engine, default_cwd, input).decision
}

#[derive(Debug)]
struct CommandAuthorization {
    decision: RunCommandCheckOutput,
    validation_error: Option<ValidationError>,
    user_env: BTreeMap<String, String>,
    input: RunCommandInput,
    authorized_cwd: Option<AuthorizedCwd>,
}

#[derive(Debug)]
struct AuthorizedCwd {
    #[cfg(unix)]
    fd: std::os::fd::OwnedFd,
    effective_input: PathBuf,
    cwd_label: String,
}

fn authorize_command_request(
    policy_engine: &PolicyEngine,
    default_cwd: &Path,
    input: RunCommandInput,
) -> CommandAuthorization {
    let user_env = input.env.clone().unwrap_or_default();
    let policy_mode = match policy_engine.mode() {
        crate::policy::PolicyMode::Rego => "rego",
        crate::policy::PolicyMode::DenyAll => "deny-all",
    }
    .to_string();

    let mut decision = RunCommandCheckOutput {
        allowed: false,
        reason: None,
        executable: input.executable.clone(),
        resolved_path: None,
        hash: None,
        cwd: None,
        policy_mode,
    };

    let authorized_cwd = match resolve_authorized_cwd(input.cwd.as_deref(), default_cwd) {
        Ok(cwd) => {
            decision.cwd = Some(cwd.cwd_label.clone());
            cwd
        }
        Err(validation_error) => {
            decision.reason = Some(validation_error.to_string());
            return CommandAuthorization {
                decision,
                validation_error: Some(validation_error),
                user_env,
                input,
                authorized_cwd: None,
            };
        }
    };

    let resolved_path = match resolve_executable_path(&input.executable) {
        Ok(path) => path,
        Err(details) => {
            let validation_error = ValidationError::PathResolutionFailed {
                command: input.executable.clone(),
                details,
            };
            decision.reason = Some(validation_error.to_string());
            return CommandAuthorization {
                decision,
                validation_error: Some(validation_error),
                user_env,
                input,
                authorized_cwd: Some(authorized_cwd),
            };
        }
    };
    decision.resolved_path = Some(resolved_path.clone());

    let executable_hash = match compute_executable_sha256_hex(&resolved_path) {
        Ok(hash) => hash,
        Err(details) => {
            let validation_error = ValidationError::HashResolutionFailed {
                command: input.executable.clone(),
                details,
            };
            decision.reason = Some(validation_error.to_string());
            return CommandAuthorization {
                decision,
                validation_error: Some(validation_error),
                user_env,
                input,
                authorized_cwd: Some(authorized_cwd),
            };
        }
    };
    decision.hash = Some(executable_hash.clone());

    match policy_engine.validate_invocation(
        &input.executable,
        &resolved_path,
        &executable_hash,
        &input.args,
        &user_env,
        &authorized_cwd.cwd_label,
    ) {
        Ok(()) => {
            tracing::info!(
                command = %input.executable,
                path = %resolved_path,
                cwd = %authorized_cwd.cwd_label,
                effective_cwd = %authorized_cwd.effective_input.display(),
                "command authorization allowed",
            );
            decision.allowed = true;
            CommandAuthorization {
                decision,
                validation_error: None,
                user_env,
                input,
                authorized_cwd: Some(authorized_cwd),
            }
        }
        Err(validation_error) => {
            decision.reason = Some(validation_error.to_string());
            CommandAuthorization {
                decision,
                validation_error: Some(validation_error),
                user_env,
                input,
                authorized_cwd: Some(authorized_cwd),
            }
        }
    }
}

fn resolve_authorized_cwd(
    request_cwd: Option<&str>,
    default_cwd: &Path,
) -> Result<AuthorizedCwd, ValidationError> {
    let effective_input = request_cwd
        .map(PathBuf::from)
        .unwrap_or_else(|| default_cwd.to_path_buf());
    let cwd_for_error = effective_input.to_string_lossy().into_owned();

    if !effective_input.is_absolute() {
        return Err(ValidationError::CwdResolutionFailed {
            cwd: cwd_for_error,
            details: "cwd must be absolute".to_string(),
        });
    }

    resolve_authorized_cwd_absolute(effective_input)
}

#[cfg(target_os = "linux")]
fn resolve_authorized_cwd_absolute(
    effective_input: PathBuf,
) -> Result<AuthorizedCwd, ValidationError> {
    use std::ffi::CString;
    use std::mem::MaybeUninit;
    use std::os::fd::{AsRawFd, FromRawFd, OwnedFd, RawFd};
    use std::os::unix::ffi::OsStrExt;

    fn cwd_error(cwd: &Path, details: impl Into<String>) -> ValidationError {
        ValidationError::CwdResolutionFailed {
            cwd: cwd.to_string_lossy().into_owned(),
            details: details.into(),
        }
    }

    fn open_root() -> Result<OwnedFd, std::io::Error> {
        let root = CString::new("/").expect("static path has no nul");
        let fd = unsafe {
            libc::open(
                root.as_ptr(),
                libc::O_PATH | libc::O_DIRECTORY | libc::O_CLOEXEC,
            )
        };
        if fd < 0 {
            Err(std::io::Error::last_os_error())
        } else {
            Ok(unsafe { OwnedFd::from_raw_fd(fd) })
        }
    }

    fn open_component(parent_fd: RawFd, component: &[u8]) -> Result<OwnedFd, std::io::Error> {
        let component = CString::new(component).map_err(|_| {
            std::io::Error::new(
                std::io::ErrorKind::InvalidInput,
                "cwd component contains a nul byte",
            )
        })?;
        let fd = unsafe {
            libc::openat(
                parent_fd,
                component.as_ptr(),
                libc::O_PATH | libc::O_NOFOLLOW | libc::O_CLOEXEC,
            )
        };
        if fd < 0 {
            return Err(std::io::Error::last_os_error());
        }

        let fd = unsafe { OwnedFd::from_raw_fd(fd) };
        let mut stat = MaybeUninit::<libc::stat>::uninit();
        if unsafe { libc::fstat(fd.as_raw_fd(), stat.as_mut_ptr()) } != 0 {
            return Err(std::io::Error::last_os_error());
        }

        let stat = unsafe { stat.assume_init() };
        match stat.st_mode & libc::S_IFMT {
            libc::S_IFDIR => Ok(fd),
            libc::S_IFLNK => Err(std::io::Error::from_raw_os_error(libc::ELOOP)),
            _ => Err(std::io::Error::from_raw_os_error(libc::ENOTDIR)),
        }
    }

    fn open_error_details(error: &std::io::Error) -> String {
        match error.raw_os_error() {
            Some(libc::ENOENT) => "path does not exist".to_string(),
            Some(libc::ENOTDIR) => "path is not a directory".to_string(),
            Some(libc::ELOOP) => "path contains a symlink or magic link".to_string(),
            Some(libc::EACCES) => "permission denied".to_string(),
            _ => error.to_string(),
        }
    }

    let cwd_bytes = effective_input.as_os_str().as_bytes();
    if cwd_bytes.starts_with(b"//") {
        return Err(cwd_error(&effective_input, "cwd must not start with '//'"));
    }

    let mut current = open_root()
        .map_err(|error| cwd_error(&effective_input, format!("failed opening '/': {error}")))?;
    let mut label_components = Vec::new();

    for component in cwd_bytes.split(|byte| *byte == b'/') {
        if component.is_empty() {
            continue;
        }

        if component == b"." || component == b".." {
            return Err(cwd_error(
                &effective_input,
                "cwd must not contain '.' or '..' components",
            ));
        }

        let label_component = std::str::from_utf8(component)
            .map_err(|_| cwd_error(&effective_input, "cwd contains a non-UTF-8 component"))?;
        let next = open_component(current.as_raw_fd(), component)
            .map_err(|error| cwd_error(&effective_input, open_error_details(&error)))?;
        current = next;
        label_components.push(label_component.to_string());
    }

    let cwd_label = if label_components.is_empty() {
        "/".to_string()
    } else {
        format!("/{}", label_components.join("/"))
    };

    Ok(AuthorizedCwd {
        fd: current,
        effective_input,
        cwd_label,
    })
}

#[cfg(not(target_os = "linux"))]
fn resolve_authorized_cwd_absolute(
    effective_input: PathBuf,
) -> Result<AuthorizedCwd, ValidationError> {
    Err(ValidationError::CwdResolutionFailed {
        cwd: effective_input.to_string_lossy().into_owned(),
        details: "trusted cwd resolution is only implemented on Linux".to_string(),
    })
}

#[cfg(unix)]
fn set_authorized_cwd_for_spawn(command: &mut Command, authorized_cwd: &AuthorizedCwd) {
    use std::os::fd::AsRawFd;

    let cwd_fd = authorized_cwd.fd.as_raw_fd();
    unsafe {
        command.pre_exec(move || {
            if libc::fchdir(cwd_fd) == 0 {
                Ok(())
            } else {
                Err(std::io::Error::last_os_error())
            }
        });
    }
}

#[cfg(not(unix))]
fn set_authorized_cwd_for_spawn(_command: &mut Command, _authorized_cwd: &AuthorizedCwd) {}

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
                format!(
                    "failed reading metadata for '{}': {error}",
                    candidate.display()
                )
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
                format!(
                    "failed reading metadata for '{}': {error}",
                    candidate.display()
                )
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

pub(crate) fn compute_executable_sha256_hex(path: &str) -> Result<String, String> {
    let mut file = std::fs::File::open(path)
        .map_err(|error| format!("failed opening '{path}' for hashing: {error}"))?;
    let mut hasher = Sha256::new();
    let mut buffer = [0u8; 8192];

    loop {
        let bytes_read = file
            .read(&mut buffer)
            .map_err(|error| format!("failed reading '{path}' for hashing: {error}"))?;
        if bytes_read == 0 {
            break;
        }
        hasher.update(&buffer[..bytes_read]);
    }

    let digest = hasher.finalize();
    let mut output = String::with_capacity(digest.len() * 2);
    for byte in digest {
        let _ = write!(&mut output, "{byte:02x}");
    }
    Ok(output)
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
    use std::path::PathBuf;

    use super::*;
    use crate::policy::PolicyEngine;

    fn default_cwd_for_tests() -> PathBuf {
        std::env::current_dir().expect("test cwd should resolve")
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

    fn rego_engine_allow_command_in_cwd(command: &str, cwd: &str) -> PolicyEngine {
        let module = format!(
            "package sandbox.main\n\ndefault allow = false\n\nallow if {{\n  input.command == {}\n  input.cwd == {}\n}}\n",
            serde_json::to_string(command).expect("command string"),
            serde_json::to_string(cwd).expect("cwd string"),
        );
        PolicyEngine::from_rego_for_tests(&[("main.rego", &module)])
    }

    fn write_executable_script(path: &Path, body: &[u8]) {
        std::fs::write(path, body).expect("write script");
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mut perms = std::fs::metadata(path).expect("metadata").permissions();
            perms.set_mode(0o755);
            std::fs::set_permissions(path, perms).expect("set perms");
        }
    }

    #[cfg(unix)]
    #[test]
    fn resolve_executable_path_preserves_symlink_in_path_lookup() {
        use std::os::unix::fs::{PermissionsExt, symlink};

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

        assert_ne!(merged.get("PATH").map(String::as_str), Some("user-path"));
        if std::env::var_os("PATH").is_some() {
            assert!(merged.contains_key("PATH"));
        } else {
            assert!(!merged.contains_key("PATH"));
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

    #[test]
    fn compute_executable_sha256_hex_uses_lowercase_hex() {
        let temp = tempfile::tempdir().expect("tempdir");
        let file_path = temp.path().join("sample.bin");
        std::fs::write(&file_path, b"abc").expect("write file");

        let hash = compute_executable_sha256_hex(file_path.to_string_lossy().as_ref())
            .expect("hash should succeed");
        assert_eq!(
            hash,
            "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"
        );
    }

    #[tokio::test]
    async fn check_allows_command_with_resolved_path_and_hash() {
        let temp = tempfile::tempdir().expect("tempdir");
        let script_path = temp.path().join("allowed-check.sh");
        write_executable_script(&script_path, b"#!/bin/sh\necho allowed\n");

        let script = script_path.to_string_lossy().into_owned();
        let expected_hash = compute_executable_sha256_hex(script_path.to_string_lossy().as_ref())
            .expect("hash should succeed");
        let expected_path = script_path.to_string_lossy().into_owned();
        let policy_engine = rego_engine_allow_commands(&[&script]);
        let decision = check_command(
            &policy_engine,
            default_cwd_for_tests().as_path(),
            RunCommandInput {
                executable: script.clone(),
                args: vec![],
                cwd: None,
                env: None,
            },
        );

        assert!(decision.allowed);
        assert_eq!(decision.reason, None);
        assert_eq!(decision.executable, script);
        assert_eq!(
            decision.resolved_path.as_deref(),
            Some(expected_path.as_str())
        );
        assert_eq!(decision.hash.as_deref(), Some(expected_hash.as_str()));
        assert_eq!(decision.policy_mode, "rego");
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn check_allows_and_denies_based_on_explicit_trusted_cwd() {
        let temp = tempfile::tempdir_in(default_cwd_for_tests()).expect("tempdir");
        let allowed_cwd = temp.path().join("allowed");
        let denied_cwd = temp.path().join("denied");
        std::fs::create_dir(&allowed_cwd).expect("create allowed cwd");
        std::fs::create_dir(&denied_cwd).expect("create denied cwd");
        let script_path = temp.path().join("cwd-policy.sh");
        write_executable_script(&script_path, b"#!/bin/sh\necho ok\n");

        let script = script_path.to_string_lossy().into_owned();
        let allowed_label = allowed_cwd.to_string_lossy().into_owned();
        let denied_label = denied_cwd.to_string_lossy().into_owned();
        let policy_engine = rego_engine_allow_command_in_cwd(&script, &allowed_label);

        let allowed = check_command(
            &policy_engine,
            default_cwd_for_tests().as_path(),
            RunCommandInput {
                executable: script.clone(),
                args: vec![],
                cwd: Some(allowed_label.clone()),
                env: None,
            },
        );
        assert!(allowed.allowed);
        assert_eq!(allowed.cwd.as_deref(), Some(allowed_label.as_str()));

        let denied = check_command(
            &policy_engine,
            default_cwd_for_tests().as_path(),
            RunCommandInput {
                executable: script,
                args: vec![],
                cwd: Some(denied_label.clone()),
                env: None,
            },
        );
        assert!(!denied.allowed);
        assert_eq!(denied.cwd.as_deref(), Some(denied_label.as_str()));
        assert!(
            denied
                .reason
                .as_deref()
                .expect("reason")
                .contains("Command not allowed")
        );
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn check_exposes_only_trusted_cwd_to_rego() {
        let temp = tempfile::tempdir_in(default_cwd_for_tests()).expect("tempdir");
        let cwd = temp.path().join("work");
        std::fs::create_dir(&cwd).expect("create cwd");
        let script_path = temp.path().join("raw-cwd-field.sh");
        write_executable_script(&script_path, b"#!/bin/sh\necho ok\n");

        let script = script_path.to_string_lossy().into_owned();
        let cwd_with_redundant_slashes = format!("{}/", cwd.to_string_lossy());
        let cwd_label = cwd.to_string_lossy().into_owned();
        let module = format!(
            "package sandbox.main\n\ndefault allow = false\n\nexpected_keys := {{\"args\", \"command\", \"cwd\", \"env\", \"hash\", \"path\"}}\n\nallow if {{\n  input.command == {}\n  input.cwd == {}\n  object.keys(input) == expected_keys\n}}\n",
            serde_json::to_string(&script).expect("script string"),
            serde_json::to_string(&cwd_label).expect("cwd string"),
        );
        let policy_engine = PolicyEngine::from_rego_for_tests(&[("main.rego", &module)]);

        let decision = check_command(
            &policy_engine,
            default_cwd_for_tests().as_path(),
            RunCommandInput {
                executable: script,
                args: vec![],
                cwd: Some(cwd_with_redundant_slashes),
                env: None,
            },
        );

        assert!(decision.allowed);
        assert_eq!(decision.cwd.as_deref(), Some(cwd_label.as_str()));
    }

    #[tokio::test]
    async fn check_uses_default_cwd_as_trusted_policy_input() {
        let temp = tempfile::tempdir_in(default_cwd_for_tests()).expect("tempdir");
        let cwd = temp.path().join("work");
        std::fs::create_dir(&cwd).expect("create cwd");
        let script_path = temp.path().join("cwd-check.sh");
        write_executable_script(&script_path, b"#!/bin/sh\necho allowed\n");

        let script = script_path.to_string_lossy().into_owned();
        let cwd_label = cwd.to_string_lossy().into_owned();
        let module = format!(
            "package sandbox.main\n\ndefault allow = false\n\nallow if {{\n  input.command == {}\n  input.cwd == {}\n}}\n",
            serde_json::to_string(&script).expect("script string"),
            serde_json::to_string(&cwd_label).expect("cwd string"),
        );
        let policy_engine = PolicyEngine::from_rego_for_tests(&[("main.rego", &module)]);

        let decision = check_command(
            &policy_engine,
            &cwd,
            RunCommandInput {
                executable: script,
                args: vec![],
                cwd: None,
                env: None,
            },
        );

        assert!(decision.allowed);
        assert_eq!(decision.cwd.as_deref(), Some(cwd_label.as_str()));
    }

    #[tokio::test]
    async fn check_reports_cwd_resolution_failure() {
        let temp = tempfile::tempdir_in(default_cwd_for_tests()).expect("tempdir");
        let missing_cwd = temp.path().join("missing");
        let policy_engine = rego_engine_allow_commands(&["/bin/does-not-matter"]);

        let decision = check_command(
            &policy_engine,
            default_cwd_for_tests().as_path(),
            RunCommandInput {
                executable: "definitely-not-a-real-command-for-mcp-run".to_string(),
                args: vec![],
                cwd: Some(missing_cwd.to_string_lossy().into_owned()),
                env: None,
            },
        );

        assert!(!decision.allowed);
        assert!(decision.cwd.is_none());
        assert!(
            decision
                .reason
                .as_deref()
                .expect("reason")
                .contains("path does not exist")
        );
        assert!(decision.resolved_path.is_none());
        assert!(decision.hash.is_none());
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn check_reports_file_cwd_resolution_failure_before_executable_resolution() {
        let temp = tempfile::tempdir_in(default_cwd_for_tests()).expect("tempdir");
        let file_cwd = temp.path().join("not-a-directory");
        std::fs::write(&file_cwd, b"file").expect("write cwd file");
        let policy_engine = rego_engine_allow_commands(&["/bin/does-not-matter"]);

        let decision = check_command(
            &policy_engine,
            default_cwd_for_tests().as_path(),
            RunCommandInput {
                executable: "definitely-not-a-real-command-for-mcp-run".to_string(),
                args: vec![],
                cwd: Some(file_cwd.to_string_lossy().into_owned()),
                env: None,
            },
        );

        assert!(!decision.allowed);
        assert!(decision.cwd.is_none());
        assert!(
            decision
                .reason
                .as_deref()
                .expect("reason")
                .contains("path is not a directory")
        );
        assert!(decision.resolved_path.is_none());
        assert!(decision.hash.is_none());
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn check_normalizes_root_repeated_and_trailing_slashes() {
        let true_path = match find_executable("true") {
            Some(path) => path,
            None => return,
        };
        let root_policy = rego_engine_allow_command_in_cwd(&true_path, "/");
        let root_decision = check_command(
            &root_policy,
            default_cwd_for_tests().as_path(),
            RunCommandInput {
                executable: true_path.clone(),
                args: vec![],
                cwd: Some("/".to_string()),
                env: None,
            },
        );
        assert!(root_decision.allowed);
        assert_eq!(root_decision.cwd.as_deref(), Some("/"));

        let temp = tempfile::tempdir_in(default_cwd_for_tests()).expect("tempdir");
        let parent = temp.path().join("parent");
        let child = parent.join("child");
        std::fs::create_dir(&parent).expect("create parent");
        std::fs::create_dir(&child).expect("create child");
        let child_label = child.to_string_lossy().into_owned();
        let child_policy = rego_engine_allow_command_in_cwd(&true_path, &child_label);

        let repeated = format!("{}//child", parent.to_string_lossy());
        let repeated_decision = check_command(
            &child_policy,
            default_cwd_for_tests().as_path(),
            RunCommandInput {
                executable: true_path.clone(),
                args: vec![],
                cwd: Some(repeated),
                env: None,
            },
        );
        assert!(repeated_decision.allowed);
        assert_eq!(repeated_decision.cwd.as_deref(), Some(child_label.as_str()));

        let trailing_decision = check_command(
            &child_policy,
            default_cwd_for_tests().as_path(),
            RunCommandInput {
                executable: true_path,
                args: vec![],
                cwd: Some(format!("{child_label}/")),
                env: None,
            },
        );
        assert!(trailing_decision.allowed);
        assert_eq!(trailing_decision.cwd.as_deref(), Some(child_label.as_str()));
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn check_rejects_malformed_relative_and_symlink_cwds() {
        let true_path = match find_executable("true") {
            Some(path) => path,
            None => return,
        };
        let policy_engine = rego_engine_allow_commands(&[&true_path]);

        for cwd in ["//foo", "relative-inside-default"] {
            let decision = check_command(
                &policy_engine,
                default_cwd_for_tests().as_path(),
                RunCommandInput {
                    executable: true_path.clone(),
                    args: vec![],
                    cwd: Some(cwd.to_string()),
                    env: None,
                },
            );

            assert!(!decision.allowed, "cwd {cwd:?} should reject");
            assert!(decision.cwd.is_none(), "cwd {cwd:?} should not resolve");
            assert!(decision.resolved_path.is_none());
            assert!(decision.hash.is_none());
        }

        #[cfg(unix)]
        {
            use std::os::unix::fs::symlink;

            let temp = tempfile::tempdir_in(default_cwd_for_tests()).expect("tempdir");
            let real = temp.path().join("real");
            let link = temp.path().join("link");
            std::fs::create_dir(&real).expect("create real cwd");
            symlink(&real, &link).expect("create cwd symlink");

            let decision = check_command(
                &policy_engine,
                default_cwd_for_tests().as_path(),
                RunCommandInput {
                    executable: true_path,
                    args: vec![],
                    cwd: Some(link.to_string_lossy().into_owned()),
                    env: None,
                },
            );

            assert!(!decision.allowed);
            assert!(decision.cwd.is_none());
            assert!(
                decision
                    .reason
                    .as_deref()
                    .expect("reason")
                    .contains("symlink")
            );
            assert!(decision.resolved_path.is_none());
            assert!(decision.hash.is_none());
        }
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn check_rejects_proc_fd_magic_link_cwd() {
        use std::os::fd::AsRawFd;

        let true_path = match find_executable("true") {
            Some(path) => path,
            None => return,
        };
        let proc_fd = Path::new("/proc")
            .join(std::process::id().to_string())
            .join("fd");
        if !proc_fd.exists() {
            return;
        }

        let temp = tempfile::tempdir_in(default_cwd_for_tests()).expect("tempdir");
        let cwd = std::fs::File::open(temp.path()).expect("open tempdir");
        let magic_link_cwd = proc_fd.join(cwd.as_raw_fd().to_string());
        let policy_engine = rego_engine_allow_commands(&[&true_path]);

        let decision = check_command(
            &policy_engine,
            default_cwd_for_tests().as_path(),
            RunCommandInput {
                executable: true_path,
                args: vec![],
                cwd: Some(magic_link_cwd.to_string_lossy().into_owned()),
                env: None,
            },
        );

        assert!(!decision.allowed);
        assert!(decision.cwd.is_none());
        assert!(
            decision
                .reason
                .as_deref()
                .expect("reason")
                .contains("symlink or magic link")
        );
        assert!(decision.resolved_path.is_none());
        assert!(decision.hash.is_none());
    }

    #[cfg(all(target_os = "linux", unix))]
    #[tokio::test]
    async fn spawn_runs_in_authorized_cwd_not_process_default_cwd() {
        let sh_path = match find_executable("sh") {
            Some(path) => path,
            None => return,
        };
        let temp = tempfile::tempdir_in(default_cwd_for_tests()).expect("tempdir");
        let default_cwd = temp.path().join("default");
        let requested_cwd = temp.path().join("requested");
        std::fs::create_dir(&default_cwd).expect("create default cwd");
        std::fs::create_dir(&requested_cwd).expect("create requested cwd");

        let requested_label = requested_cwd.to_string_lossy().into_owned();
        let policy_engine = rego_engine_allow_command_in_cwd(&sh_path, &requested_label);
        let output = run_command_impl(
            &policy_engine,
            &default_cwd,
            RunCommandInput {
                executable: sh_path,
                args: vec!["-c".to_string(), "pwd".to_string()],
                cwd: Some(requested_label.clone()),
                env: None,
            },
        )
        .await
        .expect("shell should run");

        assert_eq!(output.exit_code, Some(0));
        assert_eq!(output.stdout.trim_end(), requested_label);
    }

    #[tokio::test]
    async fn executes_allowed_command_successfully() {
        let env_path = match find_executable("env") {
            Some(path) => path,
            None => return,
        };

        let policy_engine = rego_engine_allow_commands(&[&env_path]);
        let output = run_command_impl(
            &policy_engine,
            default_cwd_for_tests().as_path(),
            RunCommandInput {
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
        let output = run_command_impl(
            &policy_engine,
            default_cwd_for_tests().as_path(),
            RunCommandInput {
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
        let executable = std::env::current_exe()
            .expect("current test executable")
            .to_string_lossy()
            .into_owned();
        let policy_engine = rego_engine_allow_commands(&["/bin/does-not-match-this-path"]);
        let error = run_command_impl(
            &policy_engine,
            default_cwd_for_tests().as_path(),
            RunCommandInput {
                executable,
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
        let output = run_command_impl(
            &policy_engine,
            default_cwd_for_tests().as_path(),
            RunCommandInput {
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
    async fn check_denied_by_policy_returns_data_without_spawning() {
        let temp = tempfile::tempdir().expect("tempdir");
        let side_effect_file = temp.path().join("side-effect.txt");
        let script_path = temp.path().join("side-effect.sh");
        write_executable_script(
            &script_path,
            format!(
                "#!/bin/sh\nprintf spawned > {}\n",
                side_effect_file.display()
            )
            .as_bytes(),
        );

        let script = script_path.to_string_lossy().into_owned();
        let policy_engine = rego_engine_allow_commands(&["/bin/does-not-match-this-path"]);
        let decision = check_command(
            &policy_engine,
            default_cwd_for_tests().as_path(),
            RunCommandInput {
                executable: script,
                args: vec![],
                cwd: None,
                env: None,
            },
        );

        assert!(!decision.allowed);
        assert!(
            decision
                .reason
                .as_deref()
                .expect("reason")
                .contains("Command not allowed")
        );
        assert!(!side_effect_file.exists());
    }

    #[tokio::test]
    async fn check_reports_deny_all_mode() {
        let temp = tempfile::tempdir().expect("tempdir");
        std::fs::write(
            temp.path().join("bad.rego"),
            "package sandbox.main\nallow if",
        )
        .expect("write bad rego");
        let policy_engine = PolicyEngine::from_sources(Some(temp.path().to_path_buf()));

        let env_path = match find_executable("env") {
            Some(path) => path,
            None => return,
        };

        let decision = check_command(
            &policy_engine,
            default_cwd_for_tests().as_path(),
            RunCommandInput {
                executable: env_path,
                args: vec![],
                cwd: None,
                env: None,
            },
        );

        assert!(!decision.allowed);
        assert!(
            decision
                .reason
                .as_deref()
                .expect("reason")
                .contains("Policy deny-all is active")
        );
        assert_eq!(decision.policy_mode, "deny-all");
    }

    #[tokio::test]
    async fn check_reports_missing_executable() {
        let policy_engine = rego_engine_allow_commands(&["/bin/does-not-matter"]);
        let decision = check_command(
            &policy_engine,
            default_cwd_for_tests().as_path(),
            RunCommandInput {
                executable: "definitely-not-a-real-command-for-mcp-run".to_string(),
                args: vec![],
                cwd: None,
                env: None,
            },
        );

        assert!(!decision.allowed);
        assert!(
            decision
                .reason
                .as_deref()
                .expect("reason")
                .contains("was not found on PATH")
        );
        assert!(decision.resolved_path.is_none());
        assert!(decision.hash.is_none());
    }
}
