#![cfg(unix)]

use std::fs;
use std::os::unix::fs::PermissionsExt as _;
use std::path::PathBuf;
use std::process::{Command, Output};
use std::sync::atomic::{AtomicUsize, Ordering};

static FIXTURE_COUNTER: AtomicUsize = AtomicUsize::new(0);

struct Fixture {
    root: PathBuf,
    bin: PathBuf,
}

impl Fixture {
    fn new() -> Self {
        let id = FIXTURE_COUNTER.fetch_add(1, Ordering::Relaxed);
        let root =
            std::env::temp_dir().join(format!("cladding-cleanup-cli-{}-{id}", std::process::id()));
        let bin = root.join("bin");
        fs::create_dir_all(&bin).expect("fixture directories should be created");
        let podman = bin.join("podman");
        fs::write(
            &podman,
            "#!/bin/sh\ncase \"$1 $2\" in\n  'container ls') printf '%s\\n' '[]' ;;\n  'volume ls') printf '%s\\n' '[]' ;;\n  *) echo \"unexpected podman call: $*\" >&2; exit 99 ;;\nesac\n",
        )
        .expect("mock Podman should be written");
        fs::set_permissions(&podman, fs::Permissions::from_mode(0o755))
            .expect("mock Podman should be executable");
        Self { root, bin }
    }

    fn run_cleanup(&self) -> Output {
        Command::new(env!("CARGO_BIN_EXE_cladding"))
            .arg("cleanup")
            .current_dir(&self.root)
            .env("PATH", &self.bin)
            .output()
            .expect("cladding cleanup should execute")
    }

    fn run_cleanup_with_run_fixture(&self, runtime_root: &std::path::Path) -> Output {
        let owner = "cladding-run-12345678-1234-4234-8234-123456789abc";
        let container_name = format!("{owner}-agent-instance");
        let volume_name = "run-socket";
        let project_root = "/gone/project/.cladding";
        let container_labels = serde_json::json!({
            "app": "agent",
            "cladding": owner,
            "cladding_resource": "runtime-container",
            "cladding_runtime_root": runtime_root,
            "project_root": project_root,
        });
        let volume_labels = serde_json::json!({
            "cladding": owner,
            "cladding_resource": "inter-container-socket",
            "cladding_runtime_root": runtime_root,
            "cladding_socket_volume": volume_name,
            "project_root": project_root,
        });
        let containers_path = self.root.join("containers.json");
        let volumes_path = self.root.join("volumes.json");
        let commands_path = self.root.join("podman-commands.txt");
        fs::write(
            &containers_path,
            format!(
                "{}\n",
                serde_json::json!([{
                    "Names": [container_name],
                    "State": "exited",
                    "Labels": container_labels,
                }])
            ),
        )
        .unwrap();
        fs::write(
            &volumes_path,
            format!(
                "{}\n",
                serde_json::json!([{
                    "Name": volume_name,
                    "Labels": volume_labels,
                }])
            ),
        )
        .unwrap();
        fs::write(&commands_path, "").unwrap();

        let script = r#"#!/bin/sh
printf '%s\n' "$*" >> "$CLADDING_TEST_COMMANDS"
case "$1:$2" in
  container:ls)
    while IFS= read -r line; do printf '%s\n' "$line"; done < "$CLADDING_TEST_CONTAINERS"
    ;;
  volume:ls)
    while IFS= read -r line; do printf '%s\n' "$line"; done < "$CLADDING_TEST_VOLUMES"
    ;;
  container:exists|volume:exists) exit 0 ;;
  container:inspect)
    if [ "$4" = "{{json .Config.Labels}}" ]; then
      printf '%s\n' "$CLADDING_TEST_CONTAINER_LABELS"
    else
      printf 'exited\n'
    fi
    ;;
  volume:inspect) printf '%s\n' "$CLADDING_TEST_VOLUME_LABELS" ;;
  container:rm) printf '[]\n' > "$CLADDING_TEST_CONTAINERS" ;;
  volume:rm) printf '[]\n' > "$CLADDING_TEST_VOLUMES" ;;
  *) echo "unexpected podman call: $*" >&2; exit 99 ;;
esac
"#;
        let podman_path = self.bin.join("podman");
        fs::write(&podman_path, script).unwrap();
        fs::set_permissions(&podman_path, fs::Permissions::from_mode(0o755)).unwrap();

        Command::new(env!("CARGO_BIN_EXE_cladding"))
            .arg("cleanup")
            .current_dir(&self.root)
            .env("PATH", &self.bin)
            .env("TMPDIR", runtime_root.parent().unwrap())
            .env("CLADDING_TEST_CONTAINERS", containers_path)
            .env("CLADDING_TEST_VOLUMES", volumes_path)
            .env("CLADDING_TEST_COMMANDS", &commands_path)
            .env(
                "CLADDING_TEST_CONTAINER_LABELS",
                container_labels.to_string(),
            )
            .env("CLADDING_TEST_VOLUME_LABELS", volume_labels.to_string())
            .output()
            .expect("cladding cleanup should execute")
    }
}

impl Drop for Fixture {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.root);
    }
}

#[test]
fn cleanup_runs_without_a_project_directory_or_config() {
    let fixture = Fixture::new();
    let output = fixture.run_cleanup();

    assert!(
        output.status.success(),
        "cleanup failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(stdout.contains("0 cleaned"), "cleanup summary: {stdout}");
    assert!(!fixture.root.join(".cladding").exists());
}

#[test]
fn cleanup_removes_stopped_containers_volumes_and_the_private_root() {
    let fixture = Fixture::new();
    let owner = "cladding-run-12345678-1234-4234-8234-123456789abc";
    let temp_base = fixture.root.join("tmp");
    fs::create_dir_all(&temp_base).unwrap();
    let runtime_root = temp_base.join(owner);
    fs::create_dir(&runtime_root).unwrap();
    fs::set_permissions(&runtime_root, fs::Permissions::from_mode(0o700)).unwrap();
    fs::write(runtime_root.join("secret-override"), "fixture-secret").unwrap();

    let output = fixture.run_cleanup_with_run_fixture(&runtime_root);

    assert!(
        output.status.success(),
        "cleanup failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(
        stdout
            .contains("container cladding-run-12345678-1234-4234-8234-123456789abc-agent-instance")
    );
    assert!(stdout.contains("volume run-socket"));
    assert!(stdout.contains("private runtime root removed"));
    assert!(!runtime_root.exists());

    let commands = fs::read_to_string(fixture.root.join("podman-commands.txt")).unwrap();
    assert!(commands.lines().any(|line| {
        line == "container rm cladding-run-12345678-1234-4234-8234-123456789abc-agent-instance"
    }));
    assert!(commands.lines().any(|line| line == "volume rm run-socket"));
}
