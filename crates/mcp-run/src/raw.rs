use std::convert::Infallible;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Instant;

use axum::Json;
use axum::body::{Body, Bytes};
use axum::extract::{State, rejection::JsonRejection};
use axum::http::{HeaderValue, StatusCode, header};
use axum::response::{IntoResponse, Response};
use base64::Engine as _;
use futures_util::StreamExt;
use serde::{Deserialize, Serialize};
use tokio::io::AsyncReadExt;
use tokio::process::{Child, ChildStderr, ChildStdout};
use tokio::sync::mpsc;
use tokio_stream::wrappers::ReceiverStream;

use crate::executor::{RunNetworkToolInput, ToolError, spawn_network_tool_process};
use crate::policy::PolicyEngine;

#[derive(Debug, Clone)]
pub struct RawEndpointState {
    pub policy_engine: Arc<PolicyEngine>,
    pub default_cwd: PathBuf,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct RawErrorBody {
    pub error: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(tag = "event", rename_all = "lowercase")]
pub enum RawStreamEvent {
    Start {},
    Stdout {
        data_b64: String,
    },
    Stderr {
        data_b64: String,
    },
    Exit {
        #[serde(rename = "exitCode")]
        exit_code: Option<i32>,
    },
    Error {
        message: String,
    },
}

#[derive(Debug, Clone, Copy)]
enum OutputStreamKind {
    Stdout,
    Stderr,
}

impl OutputStreamKind {
    fn as_str(self) -> &'static str {
        match self {
            OutputStreamKind::Stdout => "stdout",
            OutputStreamKind::Stderr => "stderr",
        }
    }
}

#[derive(Debug)]
enum ReaderEvent {
    Chunk {
        stream: OutputStreamKind,
        data: Vec<u8>,
    },
    Done {
        stream: OutputStreamKind,
    },
    ReadError {
        stream: OutputStreamKind,
        message: String,
    },
}

pub async fn raw_handler(
    State(state): State<RawEndpointState>,
    payload: Result<Json<RunNetworkToolInput>, JsonRejection>,
) -> Response {
    let input = match payload {
        Ok(Json(input)) => input,
        Err(error) => {
            tracing::warn!(error = %error, "raw request rejected before validation");
            return error_response(
                StatusCode::BAD_REQUEST,
                format!("Invalid request payload: {error}"),
            );
        }
    };

    let executable = input.executable.clone();
    let args_for_log = input.args.clone();

    let mut child = match spawn_network_tool_process(&state.policy_engine, &state.default_cwd, input) {
        Ok(child) => child,
        Err(ToolError::Validation(error)) => {
            tracing::warn!(command = %executable, args = ?args_for_log, error = %error, "raw request denied by policy");
            return error_response(StatusCode::FORBIDDEN, error.to_string());
        }
        Err(error) => {
            tracing::error!(command = %executable, args = ?args_for_log, error = %error, "raw request failed before stream start");
            return error_response(StatusCode::INTERNAL_SERVER_ERROR, error.to_string());
        }
    };

    let stdout = match child.stdout.take() {
        Some(stdout) => stdout,
        None => {
            terminate_child(&mut child).await;
            tracing::error!(command = %executable, args = ?args_for_log, "stdout pipe missing");
            return error_response(
                StatusCode::INTERNAL_SERVER_ERROR,
                "stdout pipe missing".to_string(),
            );
        }
    };
    let stderr = match child.stderr.take() {
        Some(stderr) => stderr,
        None => {
            terminate_child(&mut child).await;
            tracing::error!(command = %executable, args = ?args_for_log, "stderr pipe missing");
            return error_response(
                StatusCode::INTERNAL_SERVER_ERROR,
                "stderr pipe missing".to_string(),
            );
        }
    };

    tracing::info!(command = %executable, args = ?args_for_log, "raw request accepted");

    let (tx, rx) = mpsc::channel::<Bytes>(64);
    tokio::spawn(stream_process_events(
        child,
        stdout,
        stderr,
        tx,
        executable,
        args_for_log,
    ));

    let body_stream = ReceiverStream::new(rx).map(Ok::<_, Infallible>);
    let mut response = Response::new(Body::from_stream(body_stream));
    *response.status_mut() = StatusCode::OK;
    response.headers_mut().insert(
        header::CONTENT_TYPE,
        HeaderValue::from_static("application/x-ndjson"),
    );
    response
}

async fn stream_process_events(
    mut child: Child,
    stdout: ChildStdout,
    stderr: ChildStderr,
    tx: mpsc::Sender<Bytes>,
    executable: String,
    args: Vec<String>,
) {
    let started = Instant::now();
    if !send_event(&tx, &RawStreamEvent::Start {}).await {
        tracing::info!(command = %executable, args = ?args, "raw client disconnected before start event");
        terminate_child(&mut child).await;
        return;
    }

    let (reader_tx, mut reader_rx) = mpsc::channel::<ReaderEvent>(64);
    tokio::spawn(read_output_stream(
        stdout,
        OutputStreamKind::Stdout,
        reader_tx.clone(),
    ));
    tokio::spawn(read_output_stream(
        stderr,
        OutputStreamKind::Stderr,
        reader_tx,
    ));

    let mut stdout_done = false;
    let mut stderr_done = false;
    let mut exit_code: Option<Option<i32>> = None;

    loop {
        tokio::select! {
            status = child.wait(), if exit_code.is_none() => {
                match status {
                    Ok(status) => {
                        exit_code = Some(status.code());
                    }
                    Err(error) => {
                        tracing::error!(command = %executable, args = ?args, error = %error, "raw runtime wait failure");
                        let _ = send_event(&tx, &RawStreamEvent::Error { message: format!("Runtime wait failure: {error}") }).await;
                        return;
                    }
                }
            }
            maybe_event = reader_rx.recv(), if !(stdout_done && stderr_done) => {
                match maybe_event {
                    Some(ReaderEvent::Chunk { stream, data }) => {
                        let data_b64 = base64::engine::general_purpose::STANDARD.encode(data);
                        let event = match stream {
                            OutputStreamKind::Stdout => RawStreamEvent::Stdout { data_b64 },
                            OutputStreamKind::Stderr => RawStreamEvent::Stderr { data_b64 },
                        };
                        if !send_event(&tx, &event).await {
                            tracing::info!(command = %executable, args = ?args, "raw client disconnected during stream");
                            terminate_child(&mut child).await;
                            return;
                        }
                    }
                    Some(ReaderEvent::Done { stream }) => match stream {
                        OutputStreamKind::Stdout => stdout_done = true,
                        OutputStreamKind::Stderr => stderr_done = true,
                    },
                    Some(ReaderEvent::ReadError { stream, message }) => {
                        tracing::error!(command = %executable, args = ?args, stream = stream.as_str(), error = %message, "raw stream read failure");
                        let _ = send_event(
                            &tx,
                            &RawStreamEvent::Error {
                                message: format!("Failed reading {}: {}", stream.as_str(), message),
                            },
                        )
                        .await;
                        terminate_child(&mut child).await;
                        return;
                    }
                    None => {
                        stdout_done = true;
                        stderr_done = true;
                    }
                }
            }
        }

        if exit_code.is_some() && stdout_done && stderr_done {
            break;
        }
    }

    let final_exit_code = exit_code.unwrap_or(None);
    if !send_event(
        &tx,
        &RawStreamEvent::Exit {
            exit_code: final_exit_code,
        },
    )
    .await
    {
        tracing::info!(command = %executable, args = ?args, "raw client disconnected before exit event");
        terminate_child(&mut child).await;
        return;
    }

    tracing::info!(
        command = %executable,
        args = ?args,
        exit_code = ?final_exit_code,
        duration_ms = started.elapsed().as_millis() as u64,
        "raw request completed",
    );
}

async fn read_output_stream<R>(
    mut reader: R,
    stream: OutputStreamKind,
    tx: mpsc::Sender<ReaderEvent>,
) where
    R: tokio::io::AsyncRead + Unpin,
{
    let mut buffer = [0u8; 8192];
    loop {
        match reader.read(&mut buffer).await {
            Ok(0) => {
                let _ = tx.send(ReaderEvent::Done { stream }).await;
                return;
            }
            Ok(bytes_read) => {
                if tx
                    .send(ReaderEvent::Chunk {
                        stream,
                        data: buffer[..bytes_read].to_vec(),
                    })
                    .await
                    .is_err()
                {
                    return;
                }
            }
            Err(error) => {
                let _ = tx
                    .send(ReaderEvent::ReadError {
                        stream,
                        message: error.to_string(),
                    })
                    .await;
                return;
            }
        }
    }
}

async fn send_event(tx: &mpsc::Sender<Bytes>, event: &RawStreamEvent) -> bool {
    let mut line = match serde_json::to_vec(event) {
        Ok(line) => line,
        Err(error) => {
            tracing::error!(error = %error, "failed serializing raw stream event");
            return false;
        }
    };
    line.push(b'\n');
    tx.send(Bytes::from(line)).await.is_ok()
}

async fn terminate_child(child: &mut Child) {
    let _ = child.start_kill();
    let _ = child.wait().await;
}

fn error_response(status: StatusCode, message: String) -> Response {
    (status, Json(RawErrorBody { error: message })).into_response()
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;
    use std::sync::Arc;

    use super::*;
    use crate::executor::{MAX_OUTPUT_BYTES, RunNetworkToolInput};
    use crate::mcp::build_app;
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

    async fn start_server(policy_engine: PolicyEngine) -> (String, tokio::task::JoinHandle<()>) {
        let app = build_app(Arc::new(policy_engine), PathBuf::from("."));
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind test listener");
        let addr = listener.local_addr().expect("local addr");
        let server_task = tokio::spawn(async move {
            let _ = axum::serve(listener, app).await;
        });
        (format!("http://{addr}"), server_task)
    }

    async fn decode_events(response: reqwest::Response) -> Vec<RawStreamEvent> {
        let payload = response.text().await.expect("raw response text");
        payload
            .lines()
            .filter(|line| !line.trim().is_empty())
            .map(|line| serde_json::from_str::<RawStreamEvent>(line).expect("valid event"))
            .collect()
    }

    fn decode_output(events: &[RawStreamEvent], stream: OutputStreamKind) -> Vec<u8> {
        let mut bytes = Vec::new();
        for event in events {
            match event {
                RawStreamEvent::Stdout { data_b64 }
                    if matches!(stream, OutputStreamKind::Stdout) =>
                {
                    let decoded = base64::engine::general_purpose::STANDARD
                        .decode(data_b64)
                        .expect("decode stdout");
                    bytes.extend_from_slice(&decoded);
                }
                RawStreamEvent::Stderr { data_b64 }
                    if matches!(stream, OutputStreamKind::Stderr) =>
                {
                    let decoded = base64::engine::general_purpose::STANDARD
                        .decode(data_b64)
                        .expect("decode stderr");
                    bytes.extend_from_slice(&decoded);
                }
                _ => {}
            }
        }
        bytes
    }

    fn assert_has_event(events: &[RawStreamEvent], expected: &str) {
        assert!(
            events.iter().any(|event| match (expected, event) {
                ("start", RawStreamEvent::Start {}) => true,
                ("stdout", RawStreamEvent::Stdout { .. }) => true,
                ("stderr", RawStreamEvent::Stderr { .. }) => true,
                ("exit", RawStreamEvent::Exit { .. }) => true,
                ("error", RawStreamEvent::Error { .. }) => true,
                _ => false,
            }),
            "missing expected event: {expected}",
        );
    }

    #[tokio::test]
    async fn raw_streams_start_output_and_exit() {
        let sh_path = match find_executable("sh") {
            Some(path) => path,
            None => return,
        };
        let script = "printf 'hello'; printf 'oops' >&2";
        let (base_url, server_task) = start_server(rego_engine_allow_commands(&[&sh_path])).await;
        let response = reqwest::Client::new()
            .post(format!("{base_url}/raw"))
            .json(&RunNetworkToolInput {
                executable: sh_path,
                args: vec!["-c".to_string(), script.to_string()],
                cwd: None,
                env: None,
            })
            .send()
            .await
            .expect("request");

        assert_eq!(response.status(), StatusCode::OK);
        assert_eq!(
            response
                .headers()
                .get(header::CONTENT_TYPE)
                .expect("content type"),
            "application/x-ndjson"
        );

        let events = decode_events(response).await;
        assert_has_event(&events, "start");
        assert_has_event(&events, "stdout");
        assert_has_event(&events, "stderr");
        assert_has_event(&events, "exit");

        let stdout = decode_output(&events, OutputStreamKind::Stdout);
        let stderr = decode_output(&events, OutputStreamKind::Stderr);
        assert_eq!(stdout, b"hello");
        assert_eq!(stderr, b"oops");
        assert!(matches!(
            events.last(),
            Some(RawStreamEvent::Exit { exit_code: Some(0) })
        ));

        server_task.abort();
    }

    #[tokio::test]
    async fn raw_denies_disallowed_command_with_json_error() {
        let true_path = match find_executable("true") {
            Some(path) => path,
            None => return,
        };
        let (base_url, server_task) =
            start_server(rego_engine_allow_commands(&[&true_path])).await;

        let response = reqwest::Client::new()
            .post(format!("{base_url}/raw"))
            .json(&RunNetworkToolInput {
                executable: "echo".to_string(),
                args: vec!["blocked".to_string()],
                cwd: None,
                env: None,
            })
            .send()
            .await
            .expect("request");

        assert_eq!(response.status(), StatusCode::FORBIDDEN);
        let body = response
            .json::<RawErrorBody>()
            .await
            .expect("json error response");
        assert!(body.error.contains("Command not allowed"));

        server_task.abort();
    }

    #[tokio::test]
    async fn raw_does_not_truncate_output_beyond_one_mb() {
        let head_path = match find_executable("head") {
            Some(path) => path,
            None => return,
        };
        let requested = MAX_OUTPUT_BYTES + 4096;
        let (base_url, server_task) =
            start_server(rego_engine_allow_commands(&[&head_path])).await;

        let response = reqwest::Client::new()
            .post(format!("{base_url}/raw"))
            .json(&RunNetworkToolInput {
                executable: head_path,
                args: vec![
                    "-c".to_string(),
                    requested.to_string(),
                    "/dev/zero".to_string(),
                ],
                cwd: None,
                env: None,
            })
            .send()
            .await
            .expect("request");
        assert_eq!(response.status(), StatusCode::OK);

        let events = decode_events(response).await;
        let stdout = decode_output(&events, OutputStreamKind::Stdout);
        assert_eq!(stdout.len(), requested);
        assert!(matches!(
            events.last(),
            Some(RawStreamEvent::Exit { exit_code: Some(0) })
        ));

        server_task.abort();
    }

    #[tokio::test]
    async fn raw_base64_payload_preserves_exact_bytes() {
        let sh_path = match find_executable("sh") {
            Some(path) => path,
            None => return,
        };
        let script = "printf '\\377\\000A'";
        let (base_url, server_task) = start_server(rego_engine_allow_commands(&[&sh_path])).await;

        let response = reqwest::Client::new()
            .post(format!("{base_url}/raw"))
            .json(&RunNetworkToolInput {
                executable: sh_path,
                args: vec!["-c".to_string(), script.to_string()],
                cwd: None,
                env: None,
            })
            .send()
            .await
            .expect("request");
        assert_eq!(response.status(), StatusCode::OK);

        let events = decode_events(response).await;
        let stdout = decode_output(&events, OutputStreamKind::Stdout);
        assert_eq!(stdout, vec![255, 0, 65]);

        server_task.abort();
    }

    #[tokio::test]
    async fn raw_preserves_per_stream_order() {
        let sh_path = match find_executable("sh") {
            Some(path) => path,
            None => return,
        };
        let script = "(for i in 1 2 3; do printf \"o$i\"; done) & (for i in 1 2 3; do printf \"e$i\" >&2; done) & wait";
        let (base_url, server_task) = start_server(rego_engine_allow_commands(&[&sh_path])).await;

        let response = reqwest::Client::new()
            .post(format!("{base_url}/raw"))
            .json(&RunNetworkToolInput {
                executable: sh_path,
                args: vec!["-c".to_string(), script.to_string()],
                cwd: None,
                env: None,
            })
            .send()
            .await
            .expect("request");
        assert_eq!(response.status(), StatusCode::OK);

        let events = decode_events(response).await;
        let stdout = String::from_utf8(decode_output(&events, OutputStreamKind::Stdout))
            .expect("utf8 stdout");
        let stderr = String::from_utf8(decode_output(&events, OutputStreamKind::Stderr))
            .expect("utf8 stderr");
        assert_eq!(stdout, "o1o2o3");
        assert_eq!(stderr, "e1e2e3");

        server_task.abort();
    }
}
