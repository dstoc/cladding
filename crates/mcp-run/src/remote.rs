use std::collections::{BTreeMap, HashSet};
use std::io::Write;

use base64::Engine as _;
use futures_util::StreamExt;
use reqwest::{StatusCode, Url};
use thiserror::Error;

use crate::executor::RunNetworkToolInput;
use crate::raw::{RawErrorBody, RawStreamEvent};

pub const LOCAL_FAILURE_EXIT_CODE: i32 = 125;
const REMOTE_EXIT_CODE_UNAVAILABLE: i32 = 1;

#[derive(Debug, Error)]
pub enum RemoteClientError {
    #[error("RUN_REMOTE_SERVER must be set")]
    MissingServerUrl,
    #[error("RUN_REMOTE_SERVER must be a full URL (example: http://127.0.0.1:8000/raw)")]
    InvalidServerUrl,
    #[error("missing required `--` delimiter before remote executable")]
    MissingDelimiter,
    #[error("missing remote executable after `--`")]
    MissingExecutable,
    #[error("unknown option: {0}")]
    UnknownOption(String),
    #[error("missing value for --keep-env")]
    MissingKeepEnvValue,
    #[error("local environment variable(s) are not set: {0}")]
    MissingLocalEnv(String),
    #[error("failed to determine current working directory: {0}")]
    CurrentDir(#[source] std::io::Error),
    #[error("request failed: {0}")]
    Request(#[source] reqwest::Error),
    #[error("server rejected request ({status}): {message}")]
    ServerRejected { status: StatusCode, message: String },
    #[error("stream protocol error: {0}")]
    Protocol(String),
    #[error("failed to write output: {0}")]
    OutputWrite(#[source] std::io::Error),
    #[error("remote runtime error: {0}")]
    RemoteRuntime(String),
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct ParsedArgs {
    keep_env: Vec<String>,
    executable: String,
    args: Vec<String>,
}

pub async fn run_remote_from_env(args: Vec<String>) -> Result<i32, RemoteClientError> {
    let mut stdout = std::io::stdout().lock();
    let mut stderr = std::io::stderr().lock();
    run_remote_from_env_with_io(args, &mut stdout, &mut stderr).await
}

async fn run_remote_from_env_with_io<WOut: Write, WErr: Write>(
    args: Vec<String>,
    stdout: &mut WOut,
    stderr: &mut WErr,
) -> Result<i32, RemoteClientError> {
    let parsed = parse_args(&args)?;
    let server_url = resolve_server_url(std::env::var("RUN_REMOTE_SERVER").ok())?;
    let env = collect_forwarded_env(&parsed.keep_env, |name| std::env::var(name).ok())?;
    let cwd = std::env::current_dir().map_err(RemoteClientError::CurrentDir)?;

    let payload = RunNetworkToolInput {
        executable: parsed.executable,
        args: parsed.args,
        cwd: Some(cwd.to_string_lossy().to_string()),
        env: Some(env),
    };

    run_remote_request(&server_url, payload, stdout, stderr).await
}

pub async fn run_remote_request<WOut: Write, WErr: Write>(
    server_url: &str,
    payload: RunNetworkToolInput,
    stdout: &mut WOut,
    stderr: &mut WErr,
) -> Result<i32, RemoteClientError> {
    let client = reqwest::Client::new();
    let response = client
        .post(server_url)
        .json(&payload)
        .send()
        .await
        .map_err(RemoteClientError::Request)?;

    if !response.status().is_success() {
        let status = response.status();
        let body = response.text().await.map_err(RemoteClientError::Request)?;
        let message = serde_json::from_str::<RawErrorBody>(&body)
            .map(|decoded| decoded.error)
            .unwrap_or_else(|_| body.trim().to_string());
        return Err(RemoteClientError::ServerRejected { status, message });
    }

    process_stream(response, stdout, stderr).await
}

async fn process_stream<WOut: Write, WErr: Write>(
    response: reqwest::Response,
    stdout: &mut WOut,
    stderr: &mut WErr,
) -> Result<i32, RemoteClientError> {
    let mut buffer = Vec::new();
    let mut stream = response.bytes_stream();
    let mut saw_start = false;
    let mut exit_code: Option<i32> = None;

    while let Some(next_chunk) = stream.next().await {
        let chunk = next_chunk.map_err(RemoteClientError::Request)?;
        buffer.extend_from_slice(&chunk);

        while let Some(newline_index) = buffer.iter().position(|byte| *byte == b'\n') {
            let line = buffer.drain(..=newline_index).collect::<Vec<u8>>();
            let line = &line[..line.len().saturating_sub(1)];
            if line.is_empty() {
                continue;
            }

            handle_event_line(line, stdout, stderr, &mut saw_start, &mut exit_code)?;
            if let Some(code) = exit_code {
                return Ok(code);
            }
        }
    }

    if !buffer.is_empty() {
        handle_event_line(&buffer, stdout, stderr, &mut saw_start, &mut exit_code)?;
    }

    match exit_code {
        Some(code) => Ok(code),
        None => Err(RemoteClientError::Protocol(
            "stream ended before exit event".to_string(),
        )),
    }
}

fn handle_event_line<WOut: Write, WErr: Write>(
    line: &[u8],
    stdout: &mut WOut,
    stderr: &mut WErr,
    saw_start: &mut bool,
    exit_code: &mut Option<i32>,
) -> Result<(), RemoteClientError> {
    let event: RawStreamEvent = serde_json::from_slice(line)
        .map_err(|error| RemoteClientError::Protocol(format!("invalid event JSON: {error}")))?;

    match event {
        RawStreamEvent::Start {} => {
            *saw_start = true;
            Ok(())
        }
        RawStreamEvent::Stdout { data_b64 } => {
            let bytes = base64::engine::general_purpose::STANDARD
                .decode(data_b64)
                .map_err(|error| {
                    RemoteClientError::Protocol(format!("invalid stdout base64 payload: {error}"))
                })?;
            stdout
                .write_all(&bytes)
                .and_then(|_| stdout.flush())
                .map_err(RemoteClientError::OutputWrite)
        }
        RawStreamEvent::Stderr { data_b64 } => {
            let bytes = base64::engine::general_purpose::STANDARD
                .decode(data_b64)
                .map_err(|error| {
                    RemoteClientError::Protocol(format!("invalid stderr base64 payload: {error}"))
                })?;
            stderr
                .write_all(&bytes)
                .and_then(|_| stderr.flush())
                .map_err(RemoteClientError::OutputWrite)
        }
        RawStreamEvent::Exit { exit_code: remote } => {
            if !*saw_start {
                return Err(RemoteClientError::Protocol(
                    "received exit event before start event".to_string(),
                ));
            }
            *exit_code = Some(remote.unwrap_or(REMOTE_EXIT_CODE_UNAVAILABLE));
            Ok(())
        }
        RawStreamEvent::Error { message } => Err(RemoteClientError::RemoteRuntime(message)),
    }
}

fn parse_args(args: &[String]) -> Result<ParsedArgs, RemoteClientError> {
    let delimiter = args
        .iter()
        .position(|arg| arg == "--")
        .ok_or(RemoteClientError::MissingDelimiter)?;

    let mut keep_env = Vec::new();
    let mut seen = HashSet::new();

    let mut index = 0;
    while index < delimiter {
        let arg = &args[index];
        if let Some(value) = arg.strip_prefix("--keep-env=") {
            append_keep_env(value, &mut keep_env, &mut seen);
            index += 1;
            continue;
        }
        if arg == "--keep-env" {
            let value = args
                .get(index + 1)
                .ok_or(RemoteClientError::MissingKeepEnvValue)?;
            if index + 1 >= delimiter {
                return Err(RemoteClientError::MissingKeepEnvValue);
            }
            append_keep_env(value, &mut keep_env, &mut seen);
            index += 2;
            continue;
        }
        return Err(RemoteClientError::UnknownOption(arg.clone()));
    }

    let command = &args[(delimiter + 1)..];
    let executable = command
        .first()
        .cloned()
        .ok_or(RemoteClientError::MissingExecutable)?;

    Ok(ParsedArgs {
        keep_env,
        executable,
        args: command[1..].to_vec(),
    })
}

fn append_keep_env(value: &str, keep_env: &mut Vec<String>, seen: &mut HashSet<String>) {
    for name in value.split(',') {
        let trimmed = name.trim();
        if trimmed.is_empty() {
            continue;
        }
        if seen.insert(trimmed.to_string()) {
            keep_env.push(trimmed.to_string());
        }
    }
}

fn collect_forwarded_env<F>(
    keep_env: &[String],
    mut lookup: F,
) -> Result<BTreeMap<String, String>, RemoteClientError>
where
    F: FnMut(&str) -> Option<String>,
{
    let mut env = BTreeMap::new();
    let mut missing = Vec::new();

    for name in keep_env {
        match lookup(name) {
            Some(value) => {
                env.insert(name.clone(), value);
            }
            None => {
                missing.push(name.clone());
            }
        }
    }

    if missing.is_empty() {
        return Ok(env);
    }

    missing.sort();
    missing.dedup();
    Err(RemoteClientError::MissingLocalEnv(missing.join(", ")))
}

fn resolve_server_url(raw: Option<String>) -> Result<String, RemoteClientError> {
    let url = raw
        .map(|value| value.trim().to_string())
        .filter(|value| !value.is_empty())
        .ok_or(RemoteClientError::MissingServerUrl)?;

    let parsed = Url::parse(&url).map_err(|_| RemoteClientError::InvalidServerUrl)?;
    let scheme = parsed.scheme();
    if !parsed.has_host() || (scheme != "http" && scheme != "https") {
        return Err(RemoteClientError::InvalidServerUrl);
    }

    Ok(url)
}

#[cfg(test)]
mod tests {
    use std::convert::Infallible;

    use axum::Router;
    use axum::body::{Body, Bytes};
    use axum::extract::State;
    use axum::http::{HeaderValue, StatusCode, header};
    use axum::response::{IntoResponse, Response};
    use axum::routing::post;

    use super::*;

    #[test]
    fn parse_requires_delimiter() {
        let args = vec!["echo".to_string(), "hello".to_string()];
        let err = parse_args(&args).expect_err("missing delimiter should fail");
        assert!(matches!(err, RemoteClientError::MissingDelimiter));
    }

    #[test]
    fn resolve_server_url_requires_full_url() {
        let err = resolve_server_url(Some("127.0.0.1:8000".to_string()))
            .expect_err("host:port shorthand should fail");
        assert!(matches!(err, RemoteClientError::InvalidServerUrl));
    }

    #[test]
    fn keep_env_fails_for_missing_local_variables() {
        let names = vec!["ONE".to_string(), "MISSING".to_string()];
        let err = collect_forwarded_env(&names, |name| {
            if name == "ONE" {
                Some("1".to_string())
            } else {
                None
            }
        })
        .expect_err("missing vars should fail");
        assert!(matches!(err, RemoteClientError::MissingLocalEnv(_)));
        assert!(err.to_string().contains("MISSING"));
    }

    async fn start_server(router: Router) -> (String, tokio::task::JoinHandle<()>) {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind");
        let addr = listener.local_addr().expect("local addr");
        let task = tokio::spawn(async move {
            let _ = axum::serve(listener, router).await;
        });
        (format!("http://{addr}/raw"), task)
    }

    fn event_line(event: RawStreamEvent) -> Vec<u8> {
        let mut line = serde_json::to_vec(&event).expect("serialize event");
        line.push(b'\n');
        line
    }

    #[tokio::test]
    async fn parses_and_replays_stdout_stderr_and_exit_code() {
        let lines = [
            event_line(RawStreamEvent::Start {}),
            event_line(RawStreamEvent::Stdout {
                data_b64: base64::engine::general_purpose::STANDARD.encode(b"hello"),
            }),
            event_line(RawStreamEvent::Stderr {
                data_b64: base64::engine::general_purpose::STANDARD.encode([255u8, 0u8]),
            }),
            event_line(RawStreamEvent::Exit { exit_code: Some(7) }),
        ]
        .concat();

        let split = lines.len() / 2;
        let first = Bytes::copy_from_slice(&lines[..split]);
        let second = Bytes::copy_from_slice(&lines[split..]);

        async fn handler(State(chunks): State<Vec<Bytes>>) -> Response {
            let stream = futures_util::stream::iter(
                chunks
                    .into_iter()
                    .map(|chunk| Ok::<Bytes, Infallible>(chunk)),
            );
            let mut response = Response::new(Body::from_stream(stream));
            *response.status_mut() = StatusCode::OK;
            response.headers_mut().insert(
                header::CONTENT_TYPE,
                HeaderValue::from_static("application/x-ndjson"),
            );
            response
        }

        let router = Router::new()
            .route("/raw", post(handler))
            .with_state(vec![first, second]);
        let (url, server_task) = start_server(router).await;

        let payload = RunNetworkToolInput {
            executable: "cmd".to_string(),
            args: vec![],
            cwd: None,
            env: Some(BTreeMap::new()),
        };

        let mut stdout = Vec::new();
        let mut stderr = Vec::new();
        let code = run_remote_request(&url, payload, &mut stdout, &mut stderr)
            .await
            .expect("request should succeed");

        assert_eq!(code, 7);
        assert_eq!(stdout, b"hello");
        assert_eq!(stderr, vec![255, 0]);

        server_task.abort();
    }

    #[tokio::test]
    async fn non_200_json_errors_are_reported_cleanly() {
        async fn handler() -> Response {
            (
                StatusCode::FORBIDDEN,
                axum::Json(RawErrorBody {
                    error: "blocked".to_string(),
                }),
            )
                .into_response()
        }

        let router = Router::new().route("/raw", post(handler));
        let (url, server_task) = start_server(router).await;

        let payload = RunNetworkToolInput {
            executable: "cmd".to_string(),
            args: vec![],
            cwd: None,
            env: Some(BTreeMap::new()),
        };
        let mut stdout = Vec::new();
        let mut stderr = Vec::new();
        let err = run_remote_request(&url, payload, &mut stdout, &mut stderr)
            .await
            .expect_err("request should fail");

        assert!(matches!(
            err,
            RemoteClientError::ServerRejected {
                status: StatusCode::FORBIDDEN,
                ..
            }
        ));
        assert!(err.to_string().contains("blocked"));

        server_task.abort();
    }
}
