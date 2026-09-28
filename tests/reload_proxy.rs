#![cfg(unix)]

use std::fs;
use std::os::unix::fs::PermissionsExt as _;
use std::path::PathBuf;
use std::process::{Command, Output};
use std::sync::atomic::{AtomicUsize, Ordering};

static TEMP_DIR_COUNTER: AtomicUsize = AtomicUsize::new(0);

struct Fixture {
    root: PathBuf,
    cladding_dir: PathBuf,
    config_path: PathBuf,
    bin_dir: PathBuf,
    args_path: PathBuf,
    session_config_path: PathBuf,
}

impl Fixture {
    fn new(results: &[&str], exit_code: i32) -> Self {
        let unique = TEMP_DIR_COUNTER.fetch_add(1, Ordering::Relaxed);
        let root = std::env::temp_dir().join(format!(
            "cladding-reload-proxy-{}-{unique}",
            std::process::id()
        ));
        fs::create_dir_all(&root).expect("fixture root should be created");

        let cladding_dir = root.join(".cladding");
        let session_dir = cladding_dir.join("config/proxy/sessions");
        fs::create_dir_all(&session_dir).expect("session config directory should be created");
        let session_config_path = session_dir.join("agent.toml");
        fs::write(&session_config_path, "# user-edited native session\n")
            .expect("session config should be written");

        let config_path = root.join("cladding.json");
        fs::write(
            &config_path,
            r#"{"name":"demo","agent":{"image":"agent:test"}}"#,
        )
        .expect("Cladding config should be written");

        let bin_dir = root.join("bin");
        fs::create_dir_all(&bin_dir).expect("mock binary directory should be created");
        let args_path = root.join("podman-args.txt");
        let podman_path = bin_dir.join("podman");
        let mut script =
            String::from("#!/bin/sh\nprintf '%s\\n' \"$@\" > \"$CLADDING_TEST_ARGS\"\n");
        for result in results {
            script.push_str("printf '%s\\n' ");
            script.push_str(&shell_quote(result));
            script.push('\n');
        }
        script.push_str(&format!("exit {exit_code}\n"));
        fs::write(&podman_path, script).expect("mock podman should be written");
        fs::set_permissions(&podman_path, fs::Permissions::from_mode(0o755))
            .expect("mock podman should be executable");

        Self {
            root,
            cladding_dir,
            config_path,
            bin_dir,
            args_path,
            session_config_path,
        }
    }

    fn run_reload_proxy(&self) -> Output {
        Command::new(env!("CARGO_BIN_EXE_cladding"))
            .arg("--cladding-dir")
            .arg(&self.cladding_dir)
            .arg("--config")
            .arg(&self.config_path)
            .arg("reload-proxy")
            .env("PATH", &self.bin_dir)
            .env("CLADDING_TEST_ARGS", &self.args_path)
            .output()
            .expect("Cladding reload-proxy should run")
    }
}

impl Drop for Fixture {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.root);
    }
}

fn shell_quote(value: &str) -> String {
    format!("'{}'", value.replace('\'', "'\\''"))
}

#[test]
fn reload_proxy_displays_each_result_and_executes_baffle_reload_all() {
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
        "exec\ndemo-proxy-instance\n/opt/tools/bin/baffle\nreload\n--all\n"
    );
    assert_eq!(
        fs::read_to_string(&fixture.session_config_path)
            .expect("native session config should remain present"),
        "# user-edited native session\n"
    );
}

#[test]
fn reload_proxy_displays_other_results_and_fails_when_a_session_fails() {
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
