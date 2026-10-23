#![cfg(unix)]

use std::fs;
use std::io::{BufRead, Read as _, Write};
use std::os::unix::fs::PermissionsExt as _;
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::PathBuf;
use std::process::{Command, Output};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::thread::{self, JoinHandle};
use std::time::Duration;

static TEMP_DIR_COUNTER: AtomicUsize = AtomicUsize::new(0);

struct Fixture {
    root: PathBuf,
    outside: PathBuf,
    bin_dir: PathBuf,
    args_path: PathBuf,
    uuid: String,
    stop_server: Arc<AtomicBool>,
    server: Option<JoinHandle<()>>,
}

impl Fixture {
    fn new(results: &[&str], exit_code: i32) -> Self {
        let unique = TEMP_DIR_COUNTER.fetch_add(1, Ordering::Relaxed);
        let root = std::env::temp_dir().join(format!("r{}-{unique}", std::process::id()));
        let cladding_dir = root.join(".cladding");
        let outside = root.join("outside");
        fs::create_dir_all(&cladding_dir).expect("Cladding directory should be created");
        fs::create_dir_all(&outside).expect("outside directory should be created");
        fs::write(cladding_dir.join("cladding.json"), "not valid JSON\n")
            .expect("decoy Cladding config should be written");

        let uuid = "aabbccdd-1111-4222-8333-123456789abc".to_string();
        let run_name = format!("cladding-run-{uuid}");
        let socket_path = cladding_dir
            .join("runtime/runs")
            .join(&run_name)
            .join("control.sock");
        fs::create_dir_all(socket_path.parent().unwrap())
            .expect("private run directory should be created");
        fs::set_permissions(
            socket_path.parent().unwrap(),
            fs::Permissions::from_mode(0o700),
        )
        .expect("private run directory should be secured");
        let containers = ["agent", "proxy"]
            .into_iter()
            .map(|role| {
                serde_json::json!({
                    "Id": format!("immutable-{role}-id"),
                    "Names": [format!("{run_name}-{role}-instance")],
                    "State": "running",
                    "Labels": {
                        "app": role,
                        "cladding": run_name,
                        "project_root": cladding_dir.display().to_string(),
                        "cladding_run_metadata_version": "1",
                        "cladding_run_control_socket": socket_path.display().to_string(),
                        "cladding_run_expected_roles": "agent,proxy",
                        "cladding_run_cwd_v1": "[]"
                    }
                })
            })
            .collect::<Vec<_>>();

        let listener = UnixListener::bind(&socket_path).expect("control socket should bind");
        fs::set_permissions(&socket_path, fs::Permissions::from_mode(0o600))
            .expect("control socket should be private");
        listener
            .set_nonblocking(true)
            .expect("control listener should be nonblocking");
        let stop_server = Arc::new(AtomicBool::new(false));
        let thread_stop = Arc::clone(&stop_server);
        let server_uuid = uuid.clone();
        let server = thread::spawn(move || {
            while !thread_stop.load(Ordering::SeqCst) {
                match listener.accept() {
                    Ok((mut stream, _)) => serve_control_request(&mut stream, &server_uuid),
                    Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                        thread::sleep(Duration::from_millis(10));
                    }
                    Err(_) => break,
                }
            }
        });

        let bin_dir = root.join("bin");
        fs::create_dir_all(&bin_dir).expect("mock binary directory should be created");
        let args_path = root.join("podman-args.txt");
        let podman_path = bin_dir.join("podman");
        let mut script = format!(
            "#!/bin/sh\nif [ \"$1\" = container ] && [ \"$2\" = ps ]; then\nprintf '%s\\n' {}\nexit 0\nfi\nif [ \"$1\" = volume ] && [ \"$2\" = ls ]; then\nprintf '[]\\n'\nexit 0\nfi\nif [ \"$1\" = exec ]; then\nprintf '%s\\n' \"$@\" > \"$CLADDING_TEST_ARGS\"\n",
            shell_quote(&serde_json::Value::Array(containers).to_string())
        );
        for result in results {
            script.push_str("printf '%s\\n' ");
            script.push_str(&shell_quote(result));
            script.push('\n');
        }
        script.push_str(&format!("exit {exit_code}\nfi\nexit 9\n"));
        fs::write(&podman_path, script).expect("mock Podman should be written");
        fs::set_permissions(&podman_path, fs::Permissions::from_mode(0o755))
            .expect("mock Podman should be executable");

        Self {
            root,
            outside,
            bin_dir,
            args_path,
            uuid,
            stop_server,
            server: Some(server),
        }
    }

    fn run_reload_proxy(&self) -> Output {
        Command::new(env!("CARGO_BIN_EXE_cladding"))
            .current_dir(&self.outside)
            .args(["--run", &self.uuid, "reload-proxy"])
            .env("PATH", &self.bin_dir)
            .env("CLADDING_TEST_ARGS", &self.args_path)
            .output()
            .expect("Cladding reload-proxy should run")
    }
}

impl Drop for Fixture {
    fn drop(&mut self) {
        self.stop_server.store(true, Ordering::SeqCst);
        if let Some(server) = self.server.take() {
            let _ = server.join();
        }
        let _ = fs::remove_dir_all(&self.root);
    }
}

fn serve_control_request(stream: &mut UnixStream, uuid: &str) {
    let mut request = Vec::new();
    if std::io::BufReader::new(&mut *stream)
        .take(64 * 1024)
        .read_until(b'\n', &mut request)
        .is_err()
    {
        return;
    }
    let parsed: serde_json::Value =
        match serde_json::from_slice(request.strip_suffix(b"\n").unwrap_or(&request)) {
            Ok(parsed) => parsed,
            Err(_) => return,
        };
    if parsed.get("uuid").and_then(serde_json::Value::as_str) != Some(uuid)
        || parsed.get("version").and_then(serde_json::Value::as_u64) != Some(1)
    {
        return;
    }
    let response = serde_json::json!({
        "version": 1,
        "uuid": uuid,
        "state": "ready",
        "expected_roles": ["agent", "proxy"],
        "fake_environment": [],
        "error": null
    });
    if serde_json::to_writer(&mut *stream, &response).is_ok() {
        let _ = stream.write_all(b"\n");
    }
}

fn shell_quote(value: &str) -> String {
    format!("'{}'", value.replace('\'', "'\\''"))
}

#[test]
fn reload_proxy_uses_selected_immutable_id_and_displays_each_result() {
    let fixture = Fixture::new(
        &[
            "Session agent-session: reloaded (socket /run/baffle/proxies/cladding/agent.sock).",
            "Session sandbox-session: unchanged (socket /run/baffle/proxies/cladding/nw-sandbox.sock).",
        ],
        0,
    );

    let output = fixture.run_reload_proxy();

    assert!(
        output.status.success(),
        "reload-proxy failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(stdout.contains("agent-session: reloaded"), "{stdout}");
    assert!(stdout.contains("sandbox-session: unchanged"), "{stdout}");
    assert_eq!(
        fs::read_to_string(&fixture.args_path).expect("Podman args should be recorded"),
        "exec\nimmutable-proxy-id\n/opt/tools/bin/baffle\nreload\n--all\n"
    );
}

#[test]
fn reload_proxy_preserves_failure_status_and_displays_other_results() {
    let fixture = Fixture::new(
        &[
            "Session agent-session: failed (configuration file is invalid) (socket /run/baffle/proxies/cladding/agent.sock).",
            "Session sandbox-session: reloaded (socket /run/baffle/proxies/cladding/nw-sandbox.sock).",
        ],
        7,
    );

    let output = fixture.run_reload_proxy();

    assert_eq!(output.status.code(), Some(7));
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(stdout.contains("agent-session: failed"), "{stdout}");
    assert!(stdout.contains("sandbox-session: reloaded"), "{stdout}");
    assert!(
        String::from_utf8_lossy(&output.stderr).contains("exit code 7"),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
}
