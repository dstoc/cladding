#!/bin/sh
set -eu

if [ "$#" -ne 1 ]; then
  echo "usage: $0 <verified-gantry-release-directory>" >&2
  exit 2
fi

gantry_dir=$1
script_dir=$(CDPATH= cd "$(dirname "$0")" && pwd)
repo_root=$(CDPATH= cd "$script_dir/.." && pwd)
test "$(cat "$gantry_dir/VERSION")" = 0.4.8
for name in gantry run-remote; do
  test -f "$gantry_dir/$name"
  test -x "$gantry_dir/$name"
done

gantry_help=$("$gantry_dir/gantry" --help)
printf '%s\n' "$gantry_help" | grep -F -- '--backend bubblewrap|direct' >/dev/null

temp_root=$(mktemp -d "${TMPDIR:-/tmp}/gantry-release-check.XXXXXX")
chmod 0700 "$temp_root"
policy_dir="$temp_root/policy"
socket_path="$temp_root/gantry.sock"
server_log="$temp_root/gantry.log"
server_pid=

cleanup() {
  status=$?
  trap - EXIT INT TERM
  if [ "$status" -ne 0 ] && [ -s "$server_log" ]; then
    echo "Gantry release smoke-test server output:" >&2
    cat "$server_log" >&2
  fi
  if [ -n "$server_pid" ]; then
    kill -TERM "$server_pid" 2>/dev/null || true
    wait "$server_pid" 2>/dev/null || true
  fi
  rm -rf "$temp_root"
  exit "$status"
}
trap cleanup EXIT
trap 'exit 130' INT
trap 'exit 143' TERM

mkdir -m 0700 "$policy_dir"
cp "$repo_root/config-template/nw_sandbox/decision.rego" "$policy_dir/decision.rego"
cat > "$policy_dir/main.rego" <<'EOF'
package sandbox.main

default allow = false

allow if {
  input.command == "/bin/true"
}
EOF
unset RUN_REMOTE_SERVER
umask 077
"$gantry_dir/gantry" \
  --backend direct \
  --bind-uds "$socket_path" \
  --policy-dir "$policy_dir" \
  > "$server_log" 2>&1 &
server_pid=$!

attempt=0
while [ "$attempt" -lt 50 ] && [ ! -S "$socket_path" ]; do
  if ! kill -0 "$server_pid" 2>/dev/null; then
    echo "Gantry release server exited before binding its UDS" >&2
    exit 1
  fi
  attempt=$((attempt + 1))
  sleep 0.1
done
if [ ! -S "$socket_path" ]; then
  echo "Gantry release server did not create its UDS" >&2
  exit 1
fi

allowed_check=$(RUN_REMOTE_SOCKET="$socket_path" \
  "$gantry_dir/run-remote" --check -- /bin/true)
printf '%s\n' "$allowed_check" | python3 -c '
import json
import sys

decision = json.load(sys.stdin)
assert decision["allowed"] is True
assert decision["backend"] == "direct"
assert decision["filesystem"] is None
assert decision["baffle_session"] is None
'
RUN_REMOTE_SOCKET="$socket_path" "$gantry_dir/run-remote" -- /bin/true

set +e
denied_check=$(RUN_REMOTE_SOCKET="$socket_path" \
  "$gantry_dir/run-remote" --check -- /bin/false)
denied_status=$?
set -e
test "$denied_status" -eq 1
printf '%s\n' "$denied_check" | python3 -c '
import json
import sys

decision = json.load(sys.stdin)
assert decision["allowed"] is False
assert decision["backend"] == "direct"
assert decision["filesystem"] is None
assert decision["baffle_session"] is None
'
