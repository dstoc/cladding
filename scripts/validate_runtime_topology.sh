#!/bin/sh
set -eu

if [ "$#" -gt 1 ]; then
  echo "usage: $0 [default|runsc]" >&2
  exit 2
fi
runtime=${1:-default}
case "$runtime" in
  default) ;;
  runsc)
    command -v runsc >/dev/null 2>&1 || {
      echo "runsc topology validation requested but runsc is not installed" >&2
      exit 2
    }
    ;;
  *) echo "runtime must be default or runsc" >&2; exit 2 ;;
esac

for tool in podman jq; do
  command -v "$tool" >/dev/null 2>&1 || {
    echo "required topology-test tool is missing: $tool" >&2
    exit 2
  }
done

script_dir=$(CDPATH= cd "$(dirname "$0")" && pwd)
repo_root=$(CDPATH= cd "$script_dir/.." && pwd)
cladding_bin=${CLADDING_BIN:-$repo_root/target/debug/cladding}
rootless=$(podman info --format '{{.Host.Security.Rootless}}')
if [ "$rootless" != true ]; then
  echo "runtime topology validation requires rootless Podman" >&2
  exit 2
fi

temp_parent=${CLADDING_TOPOLOGY_TMP_DIR:-${TMPDIR:-/tmp}}
temp_root=$(mktemp -d "$temp_parent/cladding-runtime-topology.XXXXXX")
workspace="$temp_root/workspace"
project_root="$workspace/.cladding"
project_name="topology$$"
proxy="$project_name-proxy-instance"
agent="$project_name-agent-instance"
nw_sandbox="$project_name-nw-sandbox-instance"
phase="initialize fixture"

volume_names() {
  resource=$1
  podman volume ls \
    --filter "label=cladding_resource=$resource" \
    --filter "label=project_root=$project_root" \
    --format '{{.Name}}' | sort
}

require_volume_count() {
  resource=$1
  expected=$2
  names=$(volume_names "$resource")
  if [ -n "$names" ]; then
    observed=$(printf '%s\n' "$names" | wc -l | tr -d ' ')
  else
    observed=0
  fi
  if [ "$observed" -ne "$expected" ]; then
    echo "managed volume count mismatch: resource=$resource expected=$expected observed=$observed names=$names" >&2
    exit 1
  fi
}

require_no_project_resources() {
  containers=$(podman ps --all \
    --filter "label=project_root=$project_root" \
    --format '{{.Names}}')
  volumes=$(podman volume ls \
    --filter "label=project_root=$project_root" \
    --format '{{.Name}}')
  if [ -n "$containers" ] || [ -n "$volumes" ]; then
    echo "Cladding left project resources after cleanup: containers=$containers volumes=$volumes" >&2
    exit 1
  fi
}

cleanup() {
  status=$?
  trap - EXIT
  if [ "$status" -ne 0 ]; then
    echo "runtime topology validation failed during: $phase" >&2
    printf '::error title=Runtime topology validation phase::%s (exit code %s)\n' \
      "$phase" "$status"
    if [ -s "$temp_root/startup.log" ]; then
      echo "Cladding startup output:" >&2
      cat "$temp_root/startup.log" >&2
    fi
    if [ -s "$temp_root/partial-startup.log" ]; then
      echo "Cladding partial-startup output:" >&2
      cat "$temp_root/partial-startup.log" >&2
    fi
    for container in "$proxy" "$agent" "$nw_sandbox"; do
      if podman inspect "$container" >/dev/null 2>&1; then
        echo "Podman state for $container:" >&2
        podman inspect --format '{{.State.Status}} exit={{.State.ExitCode}} error={{.State.Error}}' \
          "$container" >&2 || true
        podman logs "$container" 2>&1 | tail -n 30 >&2 || true
      fi
    done
  fi
  "$cladding_bin" --cladding-dir "$project_root" down >/dev/null 2>&1 || true
  rm -rf "$temp_root"
  exit "$status"
}
trap cleanup EXIT
trap 'exit 130' INT
trap 'exit 143' TERM

mkdir -p "$workspace"
phase="initialize Cladding project"
(
  cd "$workspace"
  "$cladding_bin" init "$project_name" >/dev/null
)

phase="configure sandbox communication and a shared managed mount"
jq --arg image localhost/cladding-default:latest --arg runtime "$runtime" \
  '.agent.image = $image
   | .nw_sandbox.enabled = true
   | .nw_sandbox.image = $image
   | .use_runsc = ($runtime == "runsc")
   | .mounts = [{
       "mount": "/shared",
       "type": "tmpfs",
       "size": "32MiB",
       "targets": ["agent", "nw-sandbox"]
     }]' \
  "$project_root/cladding.json" > "$project_root/cladding.json.tmp"
mv "$project_root/cladding.json.tmp" "$project_root/cladding.json"
cat >> "$project_root/config/nw_sandbox/main.rego" <<'EOF'

allow if {
  input.command == "/bin/echo"
}
EOF
phase="build Cladding images and initialize the project CA"
(cd "$workspace" && "$cladding_bin" --cladding-dir "$project_root" build)

phase="start Cladding topology"
if (cd "$workspace" && "$cladding_bin" --cladding-dir "$project_root" up --verbose) \
  > "$temp_root/startup.log" 2>&1; then
  cat "$temp_root/startup.log"
else
  status=$?
  cat "$temp_root/startup.log" >&2
  exit "$status"
fi
require_volume_count inter-container-socket 3
require_volume_count managed-mount 1

phase="wait for Baffle control UDS and session sockets"
ready=false
attempt=0
while [ "$attempt" -lt 60 ]; do
  if sessions=$(podman exec "$proxy" /opt/tools/bin/baffle list 2>/dev/null); then
    if printf '%s\n' "$sessions" | grep -F 'agent/proxy.sock' >/dev/null \
      && printf '%s\n' "$sessions" | grep -F 'nw-sandbox/proxy.sock' >/dev/null; then
      ready=true
      break
    fi
  fi
  attempt=$((attempt + 1))
  sleep 1
done
if [ "$ready" != true ]; then
  echo "Baffle control UDS did not report both session sockets" >&2
  exit 1
fi
podman exec "$proxy" sh -ec '
  for component in agent nw-sandbox; do
    socket="/run/cladding/proxy/$component/proxy.sock"
    test -S "$socket"
    test "$(stat -c %a "$socket")" = 600
  done
'

assert_denied_connect() {
  container=$1
  component=$2
  output="$temp_root/$component-denied-connect.log"
  if podman exec --env no_proxy= --env NO_PROXY= "$container" curl \
    --silent --show-error --verbose \
    --proxy http://127.0.0.1:3128 --noproxy '' \
    --connect-timeout 5 --max-time 10 \
    https://example.com > "$output" 2>&1; then
    echo "$component Baffle data socket unexpectedly allowed CONNECT to example.com" >&2
    cat "$output" >&2
    exit 1
  fi
  if ! grep -F '> CONNECT example.com:443 HTTP/' "$output" >/dev/null \
    || ! grep -E '< HTTP/[0-9.]+ 403([[:space:]]|$)' "$output" >/dev/null; then
    echo "$component Baffle data socket did not return the expected denied CONNECT response" >&2
    cat "$output" >&2
    exit 1
  fi
}

phase="verify agent Baffle data socket denies an unlisted CONNECT"
assert_denied_connect "$agent" agent
phase="verify network-sandbox Baffle data socket denies an unlisted CONNECT"
assert_denied_connect "$nw_sandbox" nw-sandbox

phase="verify agent-to-sandbox UDS communication"
nw_output=$(podman exec "$agent" sh -ec \
  'cd /home/user && run-in-nw-sandbox -- /bin/echo nw-sandbox-uds-ok')
test "$nw_output" = nw-sandbox-uds-ok
phase="verify shared managed mount communication"
podman exec "$agent" sh -ec 'printf "agent-to-sandbox-ok\n" > /shared/topology-marker'
test "$(podman exec "$nw_sandbox" cat /shared/topology-marker)" = agent-to-sandbox-ok

phase="stop Cladding runtime and clean owned resources"
(cd "$workspace" && "$cladding_bin" --cladding-dir "$project_root" down)
require_no_project_resources
test ! -S "$project_root/runtime/sockets/proxy/agent/proxy.sock"
test ! -S "$project_root/runtime/sockets/proxy/nw-sandbox/proxy.sock"

phase="fail after partial startup and clean owned resources"
# The network sandbox starts after the proxy and agent, so this runtime option
# fails after Cladding has created several owned resources.
jq '.nw_sandbox.security_opts = ["cladding-invalid-security-option"]' \
  "$project_root/cladding.json" > "$project_root/cladding.json.tmp"
mv "$project_root/cladding.json.tmp" "$project_root/cladding.json"
startup_status=0
(cd "$workspace" && "$cladding_bin" --cladding-dir "$project_root" up --verbose) \
  > "$temp_root/partial-startup.log" 2>&1 || startup_status=$?
if [ "$startup_status" -eq 0 ]; then
  echo "Cladding accepted the deliberate network-sandbox startup failure" >&2
  exit 1
fi
if ! grep -F "$nw_sandbox" "$temp_root/partial-startup.log" >/dev/null; then
  echo "Cladding failed before attempting the network-sandbox container" >&2
  exit 1
fi
require_no_project_resources

phase="complete"
echo "Cladding rootless runtime topology passed"
