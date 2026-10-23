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
    cladding_dir: PathBuf,
    outside_dir: PathBuf,
    bin_dir: PathBuf,
    args_path: PathBuf,
    uuid: String,
    stop_server: Arc<AtomicBool>,
    server: Option<JoinHandle<()>>,
}

impl Fixture {
    fn new() -> Self {
        let unique = TEMP_DIR_COUNTER.fetch_add(1, Ordering::Relaxed);
        let root = std::env::temp_dir().join(format!("e{}{unique}", std::process::id()));
        let outside_dir = std::env::temp_dir().join(format!("x{}{unique}", std::process::id()));
        let cladding_dir = root.join(".cladding");
        fs::create_dir_all(&cladding_dir).expect("Cladding directory should be created");
        fs::create_dir_all(&outside_dir).expect("outside directory should be created");
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
        let expected_roles = "agent,proxy,nw-sandbox,fs-sandbox";
        let containers = ["agent", "proxy", "nw-sandbox", "fs-sandbox"]
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
                        "cladding_run_metadata_version": "2",
                        "cladding_run_control_socket": socket_path.display().to_string(),
                        "cladding_run_private_identity": "v1:1:2:3:4",
                        "cladding_run_expected_roles": expected_roles,
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
        let script = format!(
            "#!/bin/sh\nif [ \"$1\" = container ] && [ \"$2\" = ps ]; then\nprintf '%s\\n' {}\nexit 0\nfi\nif [ \"$1\" = volume ] && [ \"$2\" = ls ]; then\nprintf '[]\\n'\nexit 0\nfi\nif [ \"$1\" = inspect ]; then\nprintf '[{{\"Id\":\"%s\",\"Config\":{{\"WorkingDir\":\"/home/user\"}},\"Mounts\":[]}}]\\n' \"$2\"\nexit 0\nfi\nprintf '%s\\n' \"$@\" > \"$CLADDING_TEST_ARGS\"\nexit 0\n",
            shell_quote(&serde_json::Value::Array(containers).to_string())
        );
        fs::write(&podman_path, script).expect("mock Podman should be written");
        fs::set_permissions(&podman_path, fs::Permissions::from_mode(0o755))
            .expect("mock Podman should be executable");

        Self {
            root,
            cladding_dir,
            outside_dir,
            bin_dir,
            args_path,
            uuid,
            stop_server,
            server: Some(server),
        }
    }

    fn exec(&self, target_args: &[&str], command: &str) -> Output {
        Command::new(env!("CARGO_BIN_EXE_cladding"))
            .current_dir(&self.outside_dir)
            .arg("exec")
            .arg("--run")
            .arg(&self.uuid)
            .args(target_args)
            .args(["echo", command])
            .env("PATH", &self.bin_dir)
            .env("CLADDING_TEST_ARGS", &self.args_path)
            .output()
            .expect("Cladding exec should execute")
    }
}

impl Drop for Fixture {
    fn drop(&mut self) {
        self.stop_server.store(true, Ordering::SeqCst);
        if let Some(server) = self.server.take() {
            let _ = server.join();
        }
        let _ = fs::remove_dir_all(&self.root);
        let _ = fs::remove_dir_all(&self.outside_dir);
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
        || parsed.get("version").and_then(serde_json::Value::as_u64) != Some(2)
    {
        return;
    }
    let admission = parsed
        .get("operation")
        .and_then(|operation| operation.get("admit"));
    let mount_identity = admission.map(|request| {
        serde_json::json!({
            "role": request["role"],
            "container_id": request["container_id"],
            "mounts": []
        })
    });
    let response = serde_json::json!({
        "version": 2,
        "uuid": uuid,
        "state": "ready",
        "expected_roles": ["agent", "proxy", "nw-sandbox", "fs-sandbox"],
        "fake_environment": [],
        "mount_identity": mount_identity,
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
fn exec_uses_selected_immutable_container_ids_without_loading_project_config() {
    let fixture = Fixture::new();

    for (target_args, container_id, command) in [
        (&[][..], "immutable-agent-id", "agent"),
        (
            &["--target", "agent"][..],
            "immutable-agent-id",
            "explicit-agent",
        ),
        (
            &["--target", "nw-sandbox"][..],
            "immutable-nw-sandbox-id",
            "network-sandbox",
        ),
        (
            &["--target", "fs-sandbox"][..],
            "immutable-fs-sandbox-id",
            "filesystem-sandbox",
        ),
    ] {
        let output = fixture.exec(target_args, command);
        assert!(
            output.status.success(),
            "exec failed: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        assert_eq!(
            fs::read_to_string(&fixture.args_path).expect("Podman args should be recorded"),
            format!(
                "exec\n-i\n-w\n/home/user\n--env\nLANG=C.UTF-8\n{container_id}\necho\n{command}\n"
            )
        );
    }
    assert_eq!(
        fs::read_to_string(fixture.cladding_dir.join("cladding.json"))
            .expect("decoy project config should remain unreadable"),
        "not valid JSON\n"
    );
}
