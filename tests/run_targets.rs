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
}

impl Fixture {
    fn new() -> Self {
        let unique = TEMP_DIR_COUNTER.fetch_add(1, Ordering::Relaxed);
        let root = std::env::temp_dir().join(format!(
            "cladding-run-targets-{}-{unique}",
            std::process::id()
        ));
        fs::create_dir_all(&root).expect("fixture root should be created");

        let cladding_dir = root.join(".cladding");
        fs::create_dir_all(&cladding_dir).expect("Cladding directory should be created");
        let config_path = root.join("cladding.json");
        fs::write(
            &config_path,
            r#"{"name":"demo","agent":{"image":"agent:test"},"nw_sandbox":{"enabled":true,"image":"nw:test"},"fs_sandbox":{"enabled":true,"image":"fs:test"}}"#,
        )
        .expect("Cladding config should be written");

        let bin_dir = root.join("bin");
        fs::create_dir_all(&bin_dir).expect("mock binary directory should be created");
        let args_path = root.join("podman-args.txt");
        let podman_path = bin_dir.join("podman");
        let running_pods = serde_json::json!([{
            "Labels": {
                "cladding": "demo",
                "project_root": cladding_dir.display().to_string(),
            }
        }]);
        let script = format!(
            "#!/bin/sh\nif [ \"$1\" = pod ] && [ \"$2\" = ps ]; then\nprintf '%s\\n' {}\nexit 0\nfi\nprintf '%s\\n' \"$@\" > \"$CLADDING_TEST_ARGS\"\nexit 0\n",
            shell_quote(&running_pods.to_string())
        );
        fs::write(&podman_path, script).expect("mock Podman should be written");
        fs::set_permissions(&podman_path, fs::Permissions::from_mode(0o755))
            .expect("mock Podman should be executable");

        Self {
            root,
            cladding_dir,
            config_path,
            bin_dir,
            args_path,
        }
    }

    fn run(&self, target_args: &[&str], command: &str) -> Output {
        Command::new(env!("CARGO_BIN_EXE_cladding"))
            .current_dir(&self.root)
            .arg("--cladding-dir")
            .arg(&self.cladding_dir)
            .arg("--config")
            .arg(&self.config_path)
            .arg("run")
            .args(target_args)
            .args(["echo", command])
            .env("PATH", &self.bin_dir)
            .env("CLADDING_TEST_ARGS", &self.args_path)
            .output()
            .expect("Cladding run should execute")
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
fn run_executes_directly_in_the_selected_container_with_target_cwd() {
    let fixture = Fixture::new();

    for (target_args, container_name, workdir, command) in [
        (
            &[][..],
            "demo-agent-instance",
            "/home/user/workspace",
            "agent",
        ),
        (
            &["--target", "agent"][..],
            "demo-agent-instance",
            "/home/user/workspace",
            "explicit-agent",
        ),
        (
            &["--target", "nw-sandbox"][..],
            "demo-nw-sandbox-instance",
            "/home/user/workspace",
            "network-sandbox",
        ),
        (
            &["--target", "fs-sandbox"][..],
            "demo-fs-sandbox-instance",
            "/home/user",
            "filesystem-sandbox",
        ),
    ] {
        let output = fixture.run(target_args, command);
        assert!(
            output.status.success(),
            "run failed: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        assert_eq!(
            fs::read_to_string(&fixture.args_path).expect("Podman args should be recorded"),
            format!(
                "exec\n-i\n-w\n{workdir}\n--env\nLANG=C.UTF-8\n{container_name}\necho\n{command}\n"
            )
        );
    }
}
