use serde_json::json;
use std::fs::{self, OpenOptions};
use std::os::unix::fs::{MetadataExt as _, OpenOptionsExt as _, PermissionsExt as _};
use std::path::PathBuf;
use std::process::Command;
use std::time::{SystemTime, UNIX_EPOCH};

#[test]
fn destroy_rejects_run_root_replaced_during_post_lock_inventory() {
    let fixture = unique_temp_dir();
    let uuid = "abc12345-1111-4222-8333-123456789abc";
    let name = format!("cladding-run-{uuid}");
    let project = fixture.join("custom-state");
    let runtime_root = project.join("runtime/runs").join(&name);
    fs::create_dir_all(&runtime_root).unwrap();
    fs::set_permissions(
        runtime_root.parent().unwrap(),
        fs::Permissions::from_mode(0o700),
    )
    .unwrap();
    fs::set_permissions(&runtime_root, fs::Permissions::from_mode(0o700)).unwrap();

    let lock_path = runtime_root.join("lifetime.lock");
    OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(&lock_path)
        .unwrap();
    let root_metadata = fs::symlink_metadata(&runtime_root).unwrap();
    let lock_metadata = fs::symlink_metadata(&lock_path).unwrap();
    let private_identity = format!(
        "v1:{}:{}:{}:{}",
        root_metadata.dev(),
        root_metadata.ino(),
        lock_metadata.dev(),
        lock_metadata.ino()
    );
    let control_socket = PathBuf::from(format!(
        "/tmp/cladding-control-{}/{}.sock",
        // SAFETY: getuid takes no pointers and returns the invoking uid.
        unsafe { libc::getuid() },
        uuid
    ));
    let labels = json!({
        "cladding": name.clone(),
        "project_root": project,
        "app": "agent",
        "cladding_run_metadata_version": "2",
        "cladding_run_control_socket": control_socket,
        "cladding_run_private_identity": private_identity,
        "cladding_run_expected_roles": "agent,proxy",
        "cladding_run_cwd_v1": "[]"
    });
    let inventory = json!([{
        "Id": "review-stopped-agent-id",
        "Names": [format!("{name}-agent-instance")],
        "State": "exited",
        "Labels": labels
    }]);
    let inventory_path = fixture.join("containers.json");
    fs::write(&inventory_path, serde_json::to_vec(&inventory).unwrap()).unwrap();

    let bin = fixture.join("bin");
    fs::create_dir(&bin).unwrap();
    let fake_podman = bin.join("podman");
    fs::write(
        &fake_podman,
        r##"#!/bin/sh
printf '%s\n' "$*" >> "$CALLS"
if [ "$1" = container ] && [ "$2" = ps ]; then
  count=0
  if [ -f "$COUNT" ]; then count=$(cat "$COUNT"); fi
  count=$((count + 1))
  printf '%s' "$count" > "$COUNT"
  if [ "$count" -eq 2 ]; then
    mv "$RUNROOT" "$RUNROOT.original"
    mkdir -m 700 "$RUNROOT"
    printf 'unrelated replacement data' > "$RUNROOT/unrelated-keep.txt"
  fi
  cat "$INVENTORY"
elif [ "$1" = volume ] && [ "$2" = ls ]; then
  printf '[]\n'
else
  printf 'unexpected Podman call: %s\n' "$*" >&2
  exit 1
fi
"##,
    )
    .unwrap();
    fs::set_permissions(&fake_podman, fs::Permissions::from_mode(0o755)).unwrap();

    let output = Command::new(env!("CARGO_BIN_EXE_cladding"))
        .current_dir(&fixture)
        .args(["destroy", "--run", uuid])
        .env(
            "PATH",
            format!(
                "{}:{}",
                bin.display(),
                std::env::var("PATH").unwrap_or_default()
            ),
        )
        .env("RUNROOT", &runtime_root)
        .env("INVENTORY", &inventory_path)
        .env("COUNT", fixture.join("inventory-count"))
        .env("CALLS", fixture.join("calls"))
        .output()
        .unwrap();

    assert!(!output.status.success(), "destroy unexpectedly succeeded");
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("verified run path or parent changed"),
        "destroy should report changed run ownership, got: {stderr}"
    );
    assert_eq!(
        fs::read_to_string(runtime_root.join("unrelated-keep.txt")).unwrap(),
        "unrelated replacement data"
    );
    assert!(runtime_root.with_extension("original").is_dir());
    let calls = fs::read_to_string(fixture.join("calls")).unwrap();
    assert!(
        !calls.contains("container rm"),
        "destroy removed a container: {calls}"
    );
    fs::remove_dir_all(fixture).unwrap();
}

fn unique_temp_dir() -> PathBuf {
    let nonce = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    let path =
        std::env::temp_dir().join(format!("cladding-recovery-{}-{nonce}", std::process::id()));
    fs::create_dir(&path).unwrap();
    path
}
