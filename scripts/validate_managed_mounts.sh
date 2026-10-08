#!/bin/sh
set -eu

if [ "$#" -gt 1 ]; then
  echo "usage: $0 [default|runsc]" >&2
  exit 2
fi
runtime=default
if [ "$#" -eq 1 ]; then
  runtime=$1
fi
case "$runtime" in
  default|runsc) ;;
  *) echo "runtime must be default or runsc" >&2; exit 2 ;;
esac

for tool in podman jq; do
  command -v "$tool" >/dev/null 2>&1 || {
    echo "required validation tool is missing: $tool" >&2
    exit 2
  }
done
if [ "$runtime" = runsc ]; then
  command -v runsc >/dev/null 2>&1 || {
    echo "runsc validation requested but runsc is not installed" >&2
    exit 2
  }
fi
rootless=$(podman info --format '{{.Host.Security.Rootless}}')
if [ "$rootless" != true ]; then
  echo "managed mount validation requires rootless Podman" >&2
  exit 2
fi

script_dir=$(CDPATH= cd "$(dirname "$0")" && pwd)
repo_root=$(CDPATH= cd "$script_dir/.." && pwd)
cladding_bin=${CLADDING_BIN:-$repo_root/target/debug/cladding}
tmp_parent=${CLADDING_MANAGED_MOUNT_TMP_DIR:-${TMPDIR:-/tmp}}
temp_root=$(mktemp -d "$tmp_parent/cladding-managed-mounts.XXXXXX")
workspace="$temp_root/workspace"
project_root="$workspace/.cladding"
project_name="managedmounts$$"
run_pid=
phase="initialize fixture"

managed_volume_names() {
  podman volume ls \
    --filter "label=cladding_resource=managed-mount" \
    --filter "label=project_root=$project_root" \
    --format '{{.Name}}' | sort
}

require_volume_count() {
  expected=$1
  names=$(managed_volume_names)
  if [ -n "$names" ]; then
    observed=$(printf '%s\n' "$names" | wc -l | tr -d ' ')
  else
    observed=0
  fi
  if [ "$observed" -ne "$expected" ]; then
    echo "managed volume count mismatch: expected=$expected observed=$observed names=$names" >&2
    exit 1
  fi
}

exec_in() {
  target=$1
  shift
  "$cladding_bin" --cladding-dir "$project_root" exec --target "$target" "$@"
}

cleanup() {
  status=$?
  trap - EXIT
  if [ -n "$run_pid" ]; then
    kill "$run_pid" 2>/dev/null || true
    wait "$run_pid" 2>/dev/null || true
  fi
  if [ "$status" -ne 0 ]; then
    echo "managed mount validation failed during: $phase" >&2
    printf '::error title=Managed mount validation phase::%s (exit code %s)\n' \
      "$phase" "$status"
  fi
  if [ -d "$project_root" ]; then
    (cd "$workspace" && "$cladding_bin" --cladding-dir "$project_root" down) \
      >/dev/null 2>&1 || true
  fi
  rm -rf "$temp_root"
  exit "$status"
}
trap cleanup EXIT
trap 'exit 130' INT
trap 'exit 143' TERM

mkdir -p "$workspace/snapshot-source"
printf 'original snapshot data\n' > "$workspace/snapshot-source/.dotfile"
printf 'permission fixture\n' > "$workspace/snapshot-source/payload"
printf 'private fixture\n' > "$workspace/snapshot-source/private"
chmod 0640 "$workspace/snapshot-source/payload"
chmod 0600 "$workspace/snapshot-source/private"
chmod 0750 "$workspace/snapshot-source"
ln -s .dotfile "$workspace/snapshot-source/link"
(
  cd "$workspace"
  "$cladding_bin" init "$project_name" >/dev/null
)
jq --arg image docker.io/library/debian:trixie-slim \
  --arg runtime "$runtime" \
  '.agent.image = $image
   | .nw_sandbox.enabled = true
   | .nw_sandbox.image = $image
   | .use_runsc = ($runtime == "runsc")
   | .mounts = [
       {"mount":"/snapshot", "hostPath":"../snapshot-source", "type":"copy", "targets":["agent", "nw-sandbox"]},
       {"mount":"/shared", "type":"tmpfs", "size":"32MiB", "targets":["agent", "nw-sandbox"]}
     ]' \
  "$project_root/cladding.json" > "$project_root/cladding.json.tmp"
mv "$project_root/cladding.json.tmp" "$project_root/cladding.json"

phase="build project images and initialize CA"
(cd "$workspace" && "$cladding_bin" --cladding-dir "$project_root" build)

phase="start persistent runtime and create shared volumes"
(cd "$workspace" && "$cladding_bin" --cladding-dir "$project_root" up)
require_volume_count 2
persistent_names=$(managed_volume_names)

phase="verify copy seed, permissions, symlink, source isolation, and shared writes"
exec_in agent sh -ec '
  test "$(cat /snapshot/.dotfile)" = "original snapshot data"
  test -L /snapshot/link
  test "$(cat /snapshot/link)" = "original snapshot data"
  test "$(stat -c %a /snapshot)" = 750
  test "$(stat -c %a /snapshot/payload)" = 640
  test "$(stat -c %a /snapshot/private)" = 600
  test "$(stat -c %u /snapshot/private)" = 1000
  test -r /snapshot/private
  test ! -e /snapshot/host-after-start
  printf "agent write\n" > /snapshot/from-agent
  printf "agent tmpfs write\n" > /shared/from-agent
'
printf 'changed after startup\n' > "$workspace/snapshot-source/.dotfile"
printf 'host change after startup\n' > "$workspace/snapshot-source/host-after-start"
exec_in nw-sandbox sh -ec '
  test "$(cat /snapshot/.dotfile)" = "original snapshot data"
  test ! -e /snapshot/host-after-start
  test "$(cat /snapshot/from-agent)" = "agent write"
  printf "sandbox write\n" > /snapshot/from-sandbox
  test "$(cat /shared/from-agent)" = "agent tmpfs write"
  printf "sandbox tmpfs write\n" > /shared/from-sandbox
'
exec_in agent sh -ec '
  test "$(cat /snapshot/from-sandbox)" = "sandbox write"
  test "$(cat /shared/from-sandbox)" = "sandbox tmpfs write"
'
test "$(cat "$workspace/snapshot-source/.dotfile")" = "changed after startup"
test ! -e "$workspace/snapshot-source/from-agent"
test ! -e "$workspace/snapshot-source/from-sandbox"
test "$(cat "$workspace/snapshot-source/payload")" = "permission fixture"

phase="remove persistent runtime volumes on down"
(cd "$workspace" && "$cladding_bin" --cladding-dir "$project_root" down)
require_volume_count 0

phase="reseed copy snapshot and clear tmpfs on a new runtime"
(cd "$workspace" && "$cladding_bin" --cladding-dir "$project_root" up)
require_volume_count 2
exec_in agent sh -ec '
  test "$(cat /snapshot/.dotfile)" = "changed after startup"
  test -f /snapshot/host-after-start
  test ! -e /shared/from-agent
'
(cd "$workspace" && "$cladding_bin" --cladding-dir "$project_root" down)
require_volume_count 0

phase="create run-scoped volumes and clean them after successful command"
rm -f "$workspace/run-ready" "$workspace/run-exit"
(
  cd "$workspace"
  "$cladding_bin" --cladding-dir "$project_root" run -- sh -ec '
    test -f /snapshot/host-after-start
    test ! -e /shared/from-agent
    printf "one-off write\n" > /snapshot/from-run
    touch run-ready
    while [ ! -f run-exit ]; do sleep 1; done
  '
) > "$temp_root/run-success.log" 2>&1 &
run_pid=$!
attempt=0
while [ ! -f "$workspace/run-ready" ] && [ "$attempt" -lt 90 ]; do
  if ! kill -0 "$run_pid" 2>/dev/null; then
    wait "$run_pid" || true
    run_pid=
    cat "$temp_root/run-success.log" >&2
    echo "one-off command exited before its readiness signal" >&2
    exit 1
  fi
  sleep 1
  attempt=$((attempt + 1))
done
if [ ! -f "$workspace/run-ready" ]; then
  echo "one-off command did not reach its readiness signal" >&2
  exit 1
fi
require_volume_count 2
run_names=$(managed_volume_names)
for name in $run_names; do
  case " $persistent_names " in
    *" $name "*) echo "one-off runtime reused persistent volume name: $name" >&2; exit 1 ;;
  esac
done
touch "$workspace/run-exit"
wait "$run_pid"
run_pid=
require_volume_count 0

phase="remove run-scoped volumes when the command fails"
run_status=0
(
  cd "$workspace"
  "$cladding_bin" --cladding-dir "$project_root" run -- sh -ec '
    printf "failed command write\n" > /snapshot/from-failed-run
    exit 23
  '
) > "$temp_root/run-failure.log" 2>&1 || run_status=$?
if [ "$run_status" -ne 23 ]; then
  cat "$temp_root/run-failure.log" >&2
  echo "one-off command returned unexpected status: $run_status (expected 23)" >&2
  exit 1
fi
require_volume_count 0

phase="complete"
