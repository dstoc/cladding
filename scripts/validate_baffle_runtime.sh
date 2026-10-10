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

for tool in podman jq openssl git python3; do
  command -v "$tool" >/dev/null 2>&1 || {
    echo "required integration-test tool is missing: $tool" >&2
    exit 2
  }
done
script_dir=$(CDPATH= cd "$(dirname "$0")" && pwd)
repo_root=$(CDPATH= cd "$script_dir/.." && pwd)
cladding_bin="$repo_root/target/debug/cladding"
if printenv CLADDING_BIN >/dev/null 2>&1; then
  cladding_bin=$(printenv CLADDING_BIN)
fi
rootless=$(podman info --format '{{.Host.Security.Rootless}}')
if [ "$rootless" != true ]; then
  echo "Baffle runtime integration requires rootless Podman" >&2
  exit 2
fi

stat_mode() {
  if [ "$(uname -s)" = Darwin ]; then
    stat -f '%Lp' "$1"
  else
    stat -c '%a' "$1"
  fi
}

stat_uid() {
  if [ "$(uname -s)" = Darwin ]; then
    stat -f '%u' "$1"
  else
    stat -c '%u' "$1"
  fi
}

sha256_file() {
  if command -v sha256sum >/dev/null 2>&1; then
    sha256sum "$1" | cut -d ' ' -f 1
  else
    shasum -a 256 "$1" | cut -d ' ' -f 1
  fi
}

monotonic_ns() {
  python3 -c 'import time; print(time.monotonic_ns())'
}

managed_socket_volume_names() {
  podman volume ls \
    --filter "label=cladding_resource=inter-container-socket" \
    --filter "label=project_root=$project_root" \
    --format '{{.Name}}' | sort
}

require_socket_volume_count() {
  expected=$1
  names=$(managed_socket_volume_names)
  if [ -n "$names" ]; then
    observed=$(printf '%s\n' "$names" | wc -l | tr -d ' ')
  else
    observed=0
  fi
  if [ "$observed" -ne "$expected" ]; then
    echo "managed socket volume count mismatch: expected=$expected observed=$observed names=$names" >&2
    exit 1
  fi
}

temp_parent=${CLADDING_BAFFLE_RUNTIME_TMP_DIR:-${TMPDIR:-/tmp}}
temp_root=$(mktemp -d "$temp_parent/cladding-baffle-runtime.XXXXXX")
project_name="baffleintegration$$"
project_root="$temp_root/workspace/.cladding"
origin_name="$project_name-origin"
proxy="$project_name-proxy-instance"
proxy_image="localhost/$project_name-proxy:latest"
client_image="localhost/$project_name-client:latest"
origin_image="localhost/$project_name-origin:latest"
run_origin_name=
run_pid=
run_secret_value=
phase=initialize

redact_run_log() {
  CLADDING_REDACT_SECRET="$run_secret_value" python3 -c '
import os
import sys

secret = os.environb.get(b"CLADDING_REDACT_SECRET", b"")
output = sys.stdin.buffer.read()
sys.stdout.buffer.write(output.replace(secret, b"[REDACTED]") if secret else output)
'
}

cleanup() {
  status=$?
  trap - EXIT
  if [ -n "$run_origin_name" ]; then
    podman rm -f "$run_origin_name" >/dev/null 2>&1 || true
  fi
  if [ -n "$run_pid" ]; then
    kill "$run_pid" 2>/dev/null || true
    wait "$run_pid" 2>/dev/null || true
  fi
  if [ "$status" -ne 0 ]; then
    echo "Baffle runtime integration failed during: $phase" >&2
    printf '::error title=Baffle runtime integration phase::%s (exit code %s)\n' \
      "$phase" "$status"
    if [ -s "$temp_root/run.log" ]; then
      echo "Saved cladding run output (secret values redacted):" >&2
      if ! redact_run_log < "$temp_root/run.log" >&2; then
        echo "Could not safely redact the saved cladding run output; log omitted." >&2
      fi
    fi
    podman logs "$project_name-proxy-instance" >&2 2>/dev/null || true
    podman logs "$origin_name" >&2 2>/dev/null || true
  fi
  podman rm -f "$origin_name" >/dev/null 2>&1 || true
  "$cladding_bin" --cladding-dir "$project_root" down >/dev/null 2>&1 || true
  podman image rm "$proxy_image" "$client_image" "$origin_image" >/dev/null 2>&1 || true
  rm -rf "$temp_root"
  exit "$status"
}
trap cleanup EXIT

mkdir -p "$temp_root/workspace" "$temp_root/origin/www/authorized"
phase="initialize fixture"
(
  cd "$temp_root/workspace"
  "$cladding_bin" init "$project_name" >/dev/null
)
rmdir "$project_root/tools"
mkdir -p "$temp_root/workspace/tools"
ln -s ../tools "$project_root/tools"
cmp "$script_dir/../config-template/nw_sandbox/main.rego" \
  "$project_root/config/nw_sandbox/main.rego"
cmp "$script_dir/../config-template/nw_sandbox/curl.rego" \
  "$project_root/config/nw_sandbox/curl.rego"
test -z "$(find "$project_root/config" -iname '*squid*' -print)"
test "$(stat_mode "$project_root/config/proxy/sessions"):$(stat_uid "$project_root/config/proxy/sessions")" = "755:$(id -u)"
for session_file in agent.toml nw-sandbox.toml; do
  test "$(stat_mode "$project_root/config/proxy/sessions/$session_file"):$(stat_uid "$project_root/config/proxy/sessions/$session_file")" = "644:$(id -u)"
done
agent_session_config=custom/agent-policy.toml
nw_sandbox_session_config=restricted/network-policy.toml
mkdir -p \
  "$project_root/config/proxy/sessions/custom" \
  "$project_root/config/proxy/sessions/restricted"
cp "$project_root/config/proxy/sessions/agent.toml" \
  "$project_root/config/proxy/sessions/$agent_session_config"
cp "$project_root/config/proxy/sessions/nw-sandbox.toml" \
  "$project_root/config/proxy/sessions/$nw_sandbox_session_config"
test "$(stat_mode "$project_root/credentials/baffle"):$(stat_uid "$project_root/credentials/baffle")" = "700:$(id -u)"
test "$(stat_mode "$project_root/credentials/baffle/secrets"):$(stat_uid "$project_root/credentials/baffle/secrets")" = "700:$(id -u)"
jq --arg image "$client_image" --arg runtime "$runtime" \
  --arg agent_session_config "$agent_session_config" \
  --arg nw_sandbox_session_config "$nw_sandbox_session_config" \
  '.agent.image = $image
   | .nw_sandbox.image = $image
   | .fs_sandbox.enabled = true
   | .fs_sandbox.image = $image
   | .use_runsc = ($runtime == "runsc")
   | .proxy.agent.session_config = $agent_session_config
   | .proxy.nw_sandbox.session_config = $nw_sandbox_session_config' \
  "$project_root/cladding.json" > "$project_root/cladding.json.tmp"
mv "$project_root/cladding.json.tmp" "$project_root/cladding.json"
cat >> "$project_root/config/nw_sandbox/main.rego" <<'EOF'

allow if {
  input.command == "/bin/echo"
}
EOF
cat >> "$project_root/config/fs_sandbox/main.rego" <<'EOF'

allow if {
  input.command == "/bin/echo"
}
EOF

phase="create local TLS origin"
openssl req -x509 -newkey rsa:2048 -nodes -days 2 \
  -keyout "$temp_root/origin/origin-ca.key" \
  -out "$temp_root/origin/origin-ca.crt" \
  -subj "/CN=Cladding Baffle Integration Test CA" \
  -addext "basicConstraints=critical,CA:TRUE" \
  -addext "keyUsage=critical,keyCertSign,cRLSign" >/dev/null 2>&1
openssl req -newkey rsa:2048 -nodes \
  -keyout "$temp_root/origin/server.key" \
  -out "$temp_root/origin/server.csr" \
  -subj "/CN=localhost" >/dev/null 2>&1
cat > "$temp_root/origin/server.ext" <<'EOF'
basicConstraints=critical,CA:FALSE
keyUsage=critical,digitalSignature,keyEncipherment
subjectAltName=DNS:localhost
extendedKeyUsage=serverAuth
EOF
openssl x509 -req -days 2 -in "$temp_root/origin/server.csr" \
  -CA "$temp_root/origin/origin-ca.crt" \
  -CAkey "$temp_root/origin/origin-ca.key" -CAcreateserial \
  -extfile "$temp_root/origin/server.ext" \
  -out "$temp_root/origin/server.crt" >/dev/null 2>&1
# The origin runs as an unprivileged user. This per-run fixture key has no production secret.
chmod 0444 "$temp_root/origin/server.key"

git init --bare --initial-branch=main "$temp_root/origin/www/authorized/repo.git" >/dev/null
git init --initial-branch=main "$temp_root/repo" >/dev/null
git -C "$temp_root/repo" config user.name "Cladding Integration"
git -C "$temp_root/repo" config user.email "integration@example.invalid"
printf '%s\n' "local Baffle integration fixture" > "$temp_root/repo/README.md"
git -C "$temp_root/repo" add README.md
git -C "$temp_root/repo" commit -m "Add local test fixture" >/dev/null
git -C "$temp_root/repo" remote add origin "$temp_root/origin/www/authorized/repo.git"
git -C "$temp_root/repo" push origin main >/dev/null 2>&1
git --git-dir="$temp_root/origin/www/authorized/repo.git" update-server-info

phase="build integration client image"
podman build --quiet -t "$client_image" \
  -f "$script_dir/Containerfile.baffle-integration-client" "$script_dir"

phase="stage local TLS origin build context"
cp "$script_dir/baffle_test_origin.py" "$temp_root/origin/baffle_test_origin.py"

phase="build local TLS origin image"
podman build --pull=never -t "$origin_image" \
  --build-arg BASE_IMAGE="$client_image" \
  -f "$script_dir/Containerfile.baffle-integration-origin" "$temp_root/origin"

phase="build Cladding proxy image"
"$cladding_bin" --cladding-dir "$project_root" build
printf '%s\n' "run tools symlink marker" > "$temp_root/workspace/tools/run-symlink-marker"

phase="copy TLS origin CA into proxy build context"
cp "$temp_root/origin/origin-ca.crt" "$temp_root/origin-ca.crt"

phase="build proxy image with local origin trust"
podman build --quiet -t "$proxy_image" \
  --build-arg BASE_IMAGE=localhost/cladding-proxy:latest \
  -f "$script_dir/Containerfile.baffle-integration-proxy" "$temp_root"

phase="configure Baffle policy and fake credentials"
cat > "$project_root/config/proxy/daemon.toml" <<'EOF'
[daemon]
control_socket = "/run/baffle/control.sock"
socket_dir = "/run/cladding/proxy"
trusted_operator_uid = 0
create_mode = "file_only"
session_config_dir = "/opt/config/proxy/sessions"

[ca]
certificate = "/opt/credentials/baffle/ca.crt"
private_key = "/opt/credentials/baffle/ca-key.pem"

[secrets]
directory = "/opt/credentials/baffle/secrets"
allowed = ["test-token-old", "test-token-new", "missing-token", "run-token"]
EOF
write_agent_policy() {
  token=$1
  path=$2
  cat > "$project_root/config/proxy/sessions/$agent_session_config" <<EOF
version = 2
persistent = true
socket_name = "agent/proxy.sock"
unmatched = "deny"

[rules."localhost"]
ports = [8443]
paths = ["$path"]

[[rules."localhost".inject]]
header = "Authorization"
secret = "$token"
format = "bearer"
EOF
}
cat > "$project_root/config/proxy/sessions/$nw_sandbox_session_config" <<'EOF'
version = 2
persistent = true
socket_name = "nw-sandbox/proxy.sock"
unmatched = "deny"

[rules."localhost"]
ports = [9443]
paths = ["/sandbox/**"]
EOF
write_agent_policy test-token-old "/authorized/**"
printf '%s' "cladding-test-old-value" > "$project_root/credentials/baffle/secrets/test-token-old"
printf '%s' "cladding-test-new-value" > "$project_root/credentials/baffle/secrets/test-token-new"
chmod 0644 "$project_root/credentials/baffle/secrets/test-token-old"
chmod 0600 "$project_root/credentials/baffle/secrets/test-token-new"
jq --arg image "$proxy_image" '.proxy.image = $image' \
  "$project_root/cladding.json" > "$project_root/cladding.json.tmp"
mv "$project_root/cladding.json.tmp" "$project_root/cladding.json"

phase="start Cladding runtime"
"$cladding_bin" --cladding-dir "$project_root" up
require_socket_volume_count 4
test "$(stat_mode "$project_root/credentials/baffle/ca.crt"):$(stat_uid "$project_root/credentials/baffle/ca.crt")" = "644:$(id -u)"
test "$(stat_mode "$project_root/credentials/baffle/ca-key.pem"):$(stat_uid "$project_root/credentials/baffle/ca-key.pem")" = "600:$(id -u)"
ca_before=$(sha256_file "$project_root/credentials/baffle/ca.crt")
phase="verify injected secret permissions"
secret_mode=$(stat_mode "$project_root/credentials/baffle/secrets/test-token-old")
if [ "$secret_mode" != 600 ]; then
  echo "expected test-token-old mode 600 after startup, got $secret_mode" >&2
  exit 1
fi
phase="start local TLS origin container"
podman run --detach --name "$origin_name" --network "container:$proxy" "$origin_image" >/dev/null
agent="$project_name-agent-instance"
sandbox="$project_name-nw-sandbox-instance"
filesystem_sandbox="$project_name-fs-sandbox-instance"
ready=false
attempt=0
phase="wait for Baffle daemon readiness"
while [ "$attempt" -lt 60 ]; do
  if podman exec "$proxy" /opt/tools/bin/baffle list >/dev/null 2>&1; then
    ready=true
    break
  fi
  attempt=$((attempt + 1))
  sleep 1
done
if [ "$ready" != true ]; then
  echo "Baffle daemon did not become ready" >&2
  exit 1
fi
origin_ready=false
attempt=0
phase="wait for local TLS origin readiness"
while [ "$attempt" -lt 60 ]; do
  if podman exec "$origin_name" python3 -c 'import socket; socket.create_connection(("127.0.0.1", 8443), 1).close()' >/dev/null 2>&1; then
    origin_ready=true
    break
  fi
  if [ "$(podman inspect --format '{{.State.Running}}' "$origin_name" 2>/dev/null || true)" != true ]; then
    break
  fi
  attempt=$((attempt + 1))
  sleep 1
done
if [ "$origin_ready" != true ]; then
  echo "local TLS origin did not become ready" >&2
  origin_state=$(podman inspect --format '{{.State.Status}} exit={{.State.ExitCode}} error={{.State.Error}}' "$origin_name" 2>&1 || true)
  origin_probe=$(podman exec "$origin_name" python3 -c 'import socket; socket.create_connection(("127.0.0.1", 8443), 1).close()' 2>&1 || true)
  origin_logs=$(podman logs "$origin_name" 2>&1 | tail -n 20 || true)
  printf 'TLS origin state: %s\nTLS origin probe: %s\nTLS origin logs:\n%s\n' \
    "$origin_state" "$origin_probe" "$origin_logs" >&2
  diagnostic=$(printf 'state=%s; probe=%s; logs=%s' \
    "$origin_state" "$origin_probe" "$origin_logs" \
    | tr '\r\n' '  ' | sed 's/%/%25/g' | cut -c 1-3000)
  printf '::error title=TLS origin readiness diagnostics::%s\n' "$diagnostic"
  exit 1
fi

agent_curl() {
  podman exec --env no_proxy= --env NO_PROXY= "$agent" curl --silent --show-error \
    --proxy http://127.0.0.1:3128 --noproxy '' --connect-timeout 10 --max-time 30 "$@"
}
sandbox_curl() {
  podman exec --env no_proxy= --env NO_PROXY= "$sandbox" curl --silent --show-error \
    --proxy http://127.0.0.1:3128 --noproxy '' --connect-timeout 10 --max-time 30 "$@"
}
expect_curl_denied() {
  container=$1
  shift
  if podman exec --env no_proxy= --env NO_PROXY= "$container" curl --silent \
    --proxy http://127.0.0.1:3128 --noproxy '' --fail --connect-timeout 10 --max-time 30 \
    "$@" >/dev/null 2>&1; then
    echo "unexpectedly authorized request: $*" >&2
    exit 1
  fi
}

report_client_failure() {
  container=$1
  output_file=$2
  client_state=$(podman inspect --format '{{.State.Status}} exit={{.State.ExitCode}} error={{.State.Error}}' \
    "$container" 2>&1 || true)
  command_output=$(tail -n 15 "$output_file" 2>/dev/null || true)
  client_logs=$(podman logs "$container" 2>&1 | tail -n 15 || true)
  printf 'Client container state: %s\nCommand output:\n%s\nContainer logs:\n%s\n' \
    "$client_state" "$command_output" "$client_logs" >&2
  diagnostic=$(printf 'container=%s; state=%s; command_output=%s; logs=%s' \
    "$container" "$client_state" "$command_output" "$client_logs" \
    | tr '\r\n' '  ' | sed 's/%/%25/g' | cut -c 1-3000)
  printf '::error title=Baffle client workload diagnostics::%s\n' "$diagnostic"
}

verify_orphan_reaping() {
  container=$1
  orphan_pid=$(podman exec "$container" python3 -c '
import os, time

supervisor = os.fork()
if supervisor == 0:
    orphan = os.fork()
    if orphan == 0:
        print(os.getpid(), flush=True)
        os.close(1)
        time.sleep(1)
        os._exit(0)
    os._exit(0)
os.waitpid(supervisor, 0)
')
  case "$orphan_pid" in
    ''|*[!0-9]*)
      echo "failed to get orphan process ID from $container: $orphan_pid" >&2
      return 1
      ;;
  esac

  sleep 2
  if podman exec "$container" test -e "/proc/$orphan_pid"; then
    orphan_stat=$(podman exec "$container" cat "/proc/$orphan_pid/stat" 2>&1 || true)
    echo "orphaned process $orphan_pid remains in $container after exit: $orphan_stat" >&2
    return 1
  fi
}

phase="check execution-container isolation and installed trust"
for component in agent nw-sandbox; do
  if [ "$component" = agent ]; then
    container=$agent
    other_component=nw-sandbox
  else
    container=$sandbox
    other_component=agent
  fi
  podman exec "$container" sh -ec '
    test -r /opt/config/proxy/sessions/custom/agent-policy.toml
    test -r /opt/config/nw_sandbox/main.rego
    test -r /opt/config/nw_sandbox/curl.rego
    test -r /run/cladding/ca/baffle.crt
    test -x /run/podman-init
    test ! -e /opt/credentials/baffle/ca-key.pem
    test ! -e /opt/credentials/baffle/secrets/test-token-old
    test ! -e /run/baffle/control.sock
    test -S "/run/cladding/proxy/$1/proxy.sock"
    test ! -e "/run/cladding/proxy/$2/proxy.sock"
  ' sh "$component" "$other_component"
  podman exec --user 0 "$container" cmp \
    /run/cladding/ca/baffle.crt /usr/local/share/ca-certificates/baffle.crt
done
podman exec "$proxy" test -x /run/podman-init
phase="wait for sandbox UDS endpoints"
run_sockets_ready=false
attempt=0
while [ "$attempt" -lt 60 ]; do
  if podman exec "$agent" sh -ec \
    'test -S /run/cladding/run/nw-sandbox/run.sock && test -S /run/cladding/run/fs-sandbox/run.sock'; then
    run_sockets_ready=true
    break
  fi
  attempt=$((attempt + 1))
  sleep 1
done
if [ "$run_sockets_ready" != true ]; then
  echo "agent-to-sandbox UDS endpoints did not become ready" >&2
  exit 1
fi
for component in agent nw-sandbox; do
  podman exec "$proxy" sh -ec '
    test "$(stat -c %a "/run/cladding/proxy/$1")" = 700
    test "$(stat -c %a "/run/cladding/proxy/$1/proxy.sock")" = 600
    test "$(stat -c %u "/run/cladding/proxy/$1/proxy.sock")" = "$(id -u)"
  ' sh "$component"
done
podman exec "$agent" sh -ec '
  test "$(stat -c %a /run/cladding/run/nw-sandbox)" = 700
  test "$(stat -c %a /run/cladding/run/fs-sandbox)" = 700
  test "$(stat -c %a /run/cladding/run/nw-sandbox/run.sock)" = 700
  test "$(stat -c %a /run/cladding/run/fs-sandbox/run.sock)" = 700
  test "$(stat -c %u /run/cladding/run/nw-sandbox/run.sock)" = "$(id -u)"
  test "$(stat -c %u /run/cladding/run/fs-sandbox/run.sock)" = "$(id -u)"
'
podman exec "$sandbox" sh -ec '
  test "$(stat -c %a /run/cladding/run/nw-sandbox)" = 700
  test "$(stat -c %a /run/cladding/run/nw-sandbox/run.sock)" = 700
'
podman exec "$filesystem_sandbox" sh -ec '
  test "$(stat -c %a /run/cladding/run/fs-sandbox)" = 700
  test "$(stat -c %a /run/cladding/run/fs-sandbox/run.sock)" = 700
'
test "$(stat_mode "$project_root/credentials/baffle")" = 700
test "$(stat_mode "$project_root/credentials/baffle/ca.crt")" = 644
test "$(stat_mode "$project_root/credentials/baffle/ca-key.pem")" = 600
test "$(stat_uid "$project_root/credentials/baffle/ca-key.pem")" = "$(id -u)"

phase="verify orphaned child processes are reaped"
verify_orphan_reaping "$agent"
verify_orphan_reaping "$sandbox"

phase="verify agent-to-sandbox UDS communication"
nw_socket_output=$(podman exec "$agent" sh -ec \
  'cd /home/user && run-in-nw-sandbox -- /bin/echo nw-sandbox-uds-ok')
test "$nw_socket_output" = "nw-sandbox-uds-ok"
fs_socket_output=$(podman exec "$agent" sh -ec \
  'cd /home/user && run-in-fs-sandbox -- /bin/echo fs-sandbox-uds-ok')
test "$fs_socket_output" = "fs-sandbox-uds-ok"

phase="authorize agent HTTPS request through scoped proxy"
body=$(agent_curl --fail https://localhost:8443/authorized/curl \
  -H "Authorization: Bearer client-supplied-test-value")
printf '%s' "$body" | jq -e '.authorization == "old"' >/dev/null
phase="authorize network-sandbox HTTPS request through scoped proxy"
body=$(sandbox_curl --fail https://localhost:9443/sandbox/ordinary)
printf '%s' "$body" | jq -e '.authorization == "none"' >/dev/null
phase="deny agent request to an unapproved host"
expect_curl_denied "$agent" --insecure https://127.0.0.1:8443/authorized/wrong-host
phase="deny agent request to an unapproved destination port"
expect_curl_denied "$agent" https://localhost:9443/authorized/wrong-port
phase="deny agent request to an unapproved path"
expect_curl_denied "$agent" https://localhost:8443/unauthorized/path
phase="deny network-sandbox request to the agent port"
expect_curl_denied "$sandbox" https://localhost:8443/sandbox/wrong-component
phase="deny network-sandbox request to an unapproved path"
expect_curl_denied "$sandbox" https://localhost:9443/unauthorized/path
phase="deny plaintext HTTP request"
expect_curl_denied "$agent" http://localhost:8080/authorized/plaintext

phase="clone Git fixture through intercepted TLS"
git_output="$temp_root/git-clone.log"
if podman exec --env no_proxy= --env NO_PROXY= "$agent" git clone \
  https://localhost:8443/authorized/repo.git /tmp/baffle-git-clone >"$git_output" 2>&1; then
  :
else
  status=$?
  report_client_failure "$agent" "$git_output"
  exit "$status"
fi
phase="verify Git fixture clone content"
podman exec "$agent" test -s /tmp/baffle-git-clone/README.md
phase="verify Node.js integration test is present"
podman exec "$agent" test -r /tmp/baffle_node_integration.js
phase="Node.js HTTPS request through intercepted TLS"
node_output="$temp_root/node-integration.log"
if podman exec --env no_proxy= --env NO_PROXY= --env NODE_USE_ENV_PROXY=1 \
  "$agent" node --use-env-proxy /tmp/baffle_node_integration.js >"$node_output" 2>&1; then
  :
else
  status=$?
  report_client_failure "$agent" "$node_output"
  exit "$status"
fi

phase="check Baffle rejects missing secret material"
cat > "$project_root/config/proxy/sessions/missing-secret.toml" <<'EOF'
version = 2
persistent = false
socket_name = "agent/missing-secret.sock"
unmatched = "deny"

[rules."localhost"]
ports = [8443]
paths = ["/missing/**"]

[[rules."localhost".inject]]
header = "Authorization"
secret = "missing-token"
format = "bearer"
EOF
chmod 0644 "$project_root/config/proxy/sessions/missing-secret.toml"
if podman exec "$proxy" /opt/tools/bin/baffle create missing-secret.toml >/dev/null 2>&1; then
  echo "Baffle accepted a session whose allowed secret file is missing" >&2
  exit 1
fi

phase="check Baffle reload snapshots and invalid reload handling"
podman exec "$agent" test -r /tmp/baffle_reload_connection.py
podman exec --env no_proxy= --env NO_PROXY= "$agent" \
  python3 /tmp/baffle_reload_connection.py >/dev/null 2>&1 &
reload_client_pid=$!
attempt=0
while [ "$attempt" -lt 60 ]; do
  if podman exec "$agent" test -e /tmp/baffle-reload-ready; then
    break
  fi
  if ! kill -0 "$reload_client_pid" 2>/dev/null; then
    wait "$reload_client_pid" || true
    podman exec "$agent" cat /tmp/baffle-reload-client.log >&2 || true
    echo "reload client failed before opening its original connection" >&2
    exit 1
  fi
  attempt=$((attempt + 1))
  sleep 1
done
if [ "$attempt" -ge 60 ]; then
  echo "reload client did not establish its original TLS connection" >&2
  exit 1
fi
write_agent_policy test-token-new "/replacement/**"
reload_output=$("$cladding_bin" --cladding-dir "$project_root" reload-proxy 2>&1)
printf '%s\n' "$reload_output" | grep -E 'agent.*reloaded|reloaded.*agent' >/dev/null
printf '%s\n' "$reload_output" | grep -E 'nw-sandbox.*unchanged|unchanged.*nw-sandbox' >/dev/null
podman exec "$agent" touch /tmp/baffle-reload-continue
attempt=0
while [ "$attempt" -lt 60 ]; do
  if podman exec "$agent" test -e /tmp/baffle-reload-done; then
    break
  fi
  if podman exec "$agent" test -e /tmp/baffle-reload-failed; then
    podman exec "$agent" cat /tmp/baffle-reload-client.log >&2
    exit 1
  fi
  attempt=$((attempt + 1))
  sleep 1
done
if [ "$attempt" -ge 60 ]; then
  echo "reload client did not finish its existing/new connection checks" >&2
  exit 1
fi
wait "$reload_client_pid"

cp "$project_root/config/proxy/sessions/$agent_session_config" "$temp_root/agent.valid.toml"
printf '%s\n' 'invalid = [' > "$project_root/config/proxy/sessions/$agent_session_config"
if "$cladding_bin" --cladding-dir "$project_root" reload-proxy >/dev/null 2>&1; then
  echo "Baffle reload accepted invalid session TOML" >&2
  exit 1
fi
cp "$temp_root/agent.valid.toml" "$project_root/config/proxy/sessions/$agent_session_config"
reload_output=$("$cladding_bin" --cladding-dir "$project_root" reload-proxy 2>&1)
printf '%s\n' "$reload_output" | grep -E 'agent.*unchanged|unchanged.*agent' >/dev/null
printf '%s\n' "$reload_output" | grep -E 'nw-sandbox.*unchanged|unchanged.*nw-sandbox' >/dev/null
body=$(agent_curl --fail https://localhost:8443/replacement/after-invalid-reload)
printf '%s' "$body" | jq -e '.authorization == "new"' >/dev/null

phase="verify request records do not contain fake secret values"
podman exec "$origin_name" cat /tmp/baffle-integration/events.jsonl > "$temp_root/events.jsonl"
if grep -F 'cladding-test-old-value' "$temp_root/events.jsonl" \
  || grep -F 'cladding-test-new-value' "$temp_root/events.jsonl"; then
  echo "origin event log contains a credential value" >&2
  exit 1
fi
if ! jq -s -e 'any(.[]; (.path | startswith("/authorized/repo.git/"))
                      and .authorization == "old")' \
  "$temp_root/events.jsonl" >/dev/null; then
  echo "Git clone did not receive the expected old injected credential" >&2
  exit 1
fi
jq -s -e 'any(.[]; .path == "/authorized/curl" and .authorization == "old")
          and any(.[]; .path == "/authorized/node" and .authorization == "old")
          and any(.[]; .path == "/replacement/new-policy" and .authorization == "new")
          and all(.[]; .path != "/authorized/wrong-host"
                     and .path != "/authorized/wrong-port"
                     and .path != "/unauthorized/path"
                     and .path != "/sandbox/wrong-component"
                     and .path != "/authorized/plaintext")' \
  "$temp_root/events.jsonl" >/dev/null

phase="verify persistent CA reuse and normal shutdown"
podman rm -f "$origin_name" >/dev/null
shutdown_started_ns=$(monotonic_ns)
"$cladding_bin" --cladding-dir "$project_root" down
require_socket_volume_count 0
shutdown_finished_ns=$(monotonic_ns)
shutdown_elapsed_ms=$(((shutdown_finished_ns - shutdown_started_ns) / 1000000))
if [ "$shutdown_elapsed_ms" -ge 8000 ]; then
  echo "cladding down took ${shutdown_elapsed_ms} ms; expected less than 8000 ms" >&2
  exit 1
fi
ca_after=$(sha256_file "$project_root/credentials/baffle/ca.crt")
test "$ca_before" = "$ca_after"
"$cladding_bin" --cladding-dir "$project_root" up
require_socket_volume_count 2
"$cladding_bin" --cladding-dir "$project_root" down
require_socket_volume_count 0
test ! -S "$project_root/runtime/sockets/proxy/agent/proxy.sock"
test ! -S "$project_root/runtime/sockets/proxy/nw-sandbox/proxy.sock"

phase="verify disabled network-sandbox lifecycle"
jq '.nw_sandbox.enabled = false' \
  "$project_root/cladding.json" > "$project_root/cladding.json.tmp"
mv "$project_root/cladding.json.tmp" "$project_root/cladding.json"
"$cladding_bin" --cladding-dir "$project_root" up
if podman inspect "$sandbox" >/dev/null 2>&1; then
  echo "Cladding started a network-sandbox container while it was disabled" >&2
  exit 1
fi
sessions=$(podman exec "$proxy" /opt/tools/bin/baffle list)
printf '%s\n' "$sessions" | grep -F 'agent/proxy.sock' >/dev/null
if printf '%s\n' "$sessions" | grep -F 'nw-sandbox/proxy.sock' >/dev/null; then
  echo "Baffle created a network-sandbox session while it was disabled" >&2
  exit 1
fi
"$cladding_bin" --cladding-dir "$project_root" down
test ! -S "$project_root/runtime/sockets/proxy/agent/proxy.sock"
test ! -S "$project_root/runtime/sockets/proxy/nw-sandbox/proxy.sock"

phase="verify one-off shared project state and isolated runtime cleanup"
mkdir -m 0700 "$temp_root/run-tmp"
run_secret_value="cladding-test-old-value"
run_secret_name="run-token"
run_persistent_secret_name="test-token-new"
phase="verify integration log secret redaction"
redacted_log=$(printf 'before:%s:after\n' "$run_secret_value" | redact_run_log)
if [ "$redacted_log" != 'before:[REDACTED]:after' ]; then
  echo "integration log redaction did not remove the run secret value" >&2
  exit 1
fi
phase="verify one-off shared project state and isolated runtime cleanup"
run_persistent_secret_before=$(sha256_file \
  "$project_root/credentials/baffle/secrets/$run_persistent_secret_name")
write_agent_policy "$run_secret_name" "/replacement/**"
run_session_before=$(sha256_file \
  "$project_root/config/proxy/sessions/$agent_session_config")
test ! -e "$project_root/credentials/baffle/secrets/$run_secret_name"
(
  cd "$temp_root/workspace"
  export CLADDING_RUN_SECRET_OVERRIDE="$run_secret_value"
  TMPDIR="$temp_root/run-tmp" "$cladding_bin" --cladding-dir "$project_root" run -v \
    --secret "$run_secret_name=env:CLADDING_RUN_SECRET_OVERRIDE" -- \
    /bin/sh -ec 'test -z "${CLADDING_RUN_SECRET_OVERRIDE+x}"
      while [ ! -f /home/user/workspace/.run-origin-ready ]; do sleep 1; done
      curl --fail --silent --show-error --proxy http://127.0.0.1:3128 --noproxy "" \
        https://localhost:8443/replacement/run-secret \
        > /home/user/workspace/.run-secret-response
      touch /home/user/workspace/.run-request-done
      while [ ! -f /home/user/workspace/.run-finish ]; do sleep 1; done
      exit 7' \
    > "$temp_root/run.log" 2>&1 &
  run_child=$!
  trap 'kill "$run_child" 2>/dev/null || true; wait "$run_child" 2>/dev/null || true' HUP INT TERM
  if wait "$run_child"; then
    run_status=0
  else
    run_status=$?
  fi
  trap - HUP INT TERM
  printf '%s\n' "$run_status" > "$temp_root/run.status"
) &
run_pid=$!
run_root=
attempt=0
while [ "$attempt" -lt 60 ]; do
  run_root=$(find "$temp_root/run-tmp" -mindepth 2 -maxdepth 4 \
    -path '*/runtime/scripts/proxy_startup.sh' -print -quit \
    | sed 's|/runtime/scripts/proxy_startup.sh$||')
  if [ -n "$run_root" ]; then
    break
  fi
  if [ -f "$temp_root/run.status" ]; then
    echo "one-off runtime exited before creating its private runtime root" >&2
    exit 1
  fi
  attempt=$((attempt + 1))
  sleep 1
done
if [ -z "$run_root" ]; then
  echo "one-off runtime did not create its private startup script" >&2
  exit 1
fi
test -d "$run_root/runtime/empty-mask"
test -d "$run_root/runtime/sockets/proxy"
test "$(stat_mode "$run_root/runtime/secrets")" = 700
test "$(stat_mode "$run_root/runtime/secrets/$run_secret_name")" = 600
for project_state in config tools credentials home; do
  if [ -e "$run_root/$project_state" ]; then
    echo "one-off runtime copied project state into $run_root/$project_state" >&2
    exit 1
  fi
done
run_name=$(sed -n 's/^starting one-off instance: //p' "$temp_root/run.log" | head -n 1)
if [ -z "$run_name" ]; then
  echo "one-off command did not report its runtime name" >&2
  exit 1
fi
attempt=0
while ! podman container exists "$run_name-proxy-instance" >/dev/null 2>&1 \
  || ! podman container exists "$run_name-agent-instance" >/dev/null 2>&1; do
  if [ "$attempt" -ge 60 ] || [ -f "$temp_root/run.status" ]; then
    echo "one-off runtime did not start its proxy and agent containers" >&2
    exit 1
  fi
  attempt=$((attempt + 1))
  sleep 1
done
run_proxy="$run_name-proxy-instance"
run_agent="$run_name-agent-instance"
require_socket_volume_count 2
run_secret_directory="$run_root/runtime/secrets"
assert_mount() {
  container=$1
  source=$2
  destination=$3
  if ! podman inspect "$container" | jq -e \
    --arg source "$source" --arg destination "$destination" \
    '.[0].Mounts | any(.Source == $source and .Destination == $destination)' \
    >/dev/null; then
    echo "$container does not mount $source at $destination" >&2
    exit 1
  fi
}
phase="verify one-off mounts use shared project files"
assert_mount "$run_proxy" "$project_root/config" /opt/config
assert_mount "$run_proxy" "$project_root/credentials/baffle" /opt/credentials/baffle
if ! podman inspect "$run_proxy" | jq -e \
  --arg source "$run_secret_directory" --arg destination "/opt/credentials/baffle/secrets" \
  '.[0].Mounts | any(.Source == $source and .Destination == $destination and .RW == false)' \
  >/dev/null; then
  echo "one-off proxy did not mount its run secret directory read-only over the Baffle secrets path" >&2
  exit 1
fi
for persistent_secret_name in test-token-old test-token-new; do
  if ! podman inspect "$run_proxy" | jq -e \
    --arg source "$project_root/credentials/baffle/secrets/$persistent_secret_name" \
    --arg destination "/opt/credentials/baffle/secrets/$persistent_secret_name" \
    '.[0].Mounts | any(.Source == $source and .Destination == $destination and .RW == false)' \
    >/dev/null; then
    echo "one-off proxy did not preserve project Baffle secret $persistent_secret_name as a read-only mount" >&2
    exit 1
  fi
done
assert_mount "$run_proxy" "$run_root/runtime/scripts/proxy_startup.sh" /opt/scripts/proxy_startup.sh
assert_mount "$run_proxy" "$run_root/runtime/sockets/proxy" /run/cladding/proxy
assert_mount "$run_agent" "$project_root/config" /opt/config
assert_mount "$run_agent" "$project_root/home" /home/user
assert_mount "$run_agent" "$temp_root/workspace" /home/user/workspace
assert_mount "$run_agent" "$run_root/runtime/empty-mask" /home/user/workspace/.cladding
if podman inspect "$run_agent" | jq -e \
  --arg source "$run_secret_directory" \
  --arg destination "/opt/credentials/baffle/secrets" \
  '.[0].Mounts | any(.Source == $source or .Destination == $destination or (.Destination | startswith($destination + "/")))' \
  >/dev/null; then
  echo "one-off agent received the run secret override" >&2
  exit 1
fi
podman exec "$run_agent" /bin/sh -ec \
  'test -x /opt/tools/bin/baffle && test -f /opt/tools/run-symlink-marker'
podman exec "$run_agent" /bin/sh -ec 'test -z "${CLADDING_RUN_SECRET_OVERRIDE+x}"'
podman exec "$run_proxy" /bin/sh -ec \
  'test -s /opt/credentials/baffle/ca.crt
   test -f /opt/credentials/baffle/secrets/test-token-old
   test -f /opt/credentials/baffle/secrets/test-token-new'
podman inspect "$run_proxy" | jq -e \
  --arg expected "CLADDING_AGENT_SESSION_CONFIG=$agent_session_config" \
  '.[0].Config.Env | any(. == $expected)' >/dev/null
run_sessions=$(podman exec "$run_proxy" /opt/tools/bin/baffle list)
printf '%s\n' "$run_sessions" | grep -F 'agent/proxy.sock' >/dev/null
if printf '%s\n' "$run_sessions" | grep -F 'nw-sandbox/proxy.sock' >/dev/null; then
  echo "one-off Baffle runtime created a network-sandbox session while disabled" >&2
  exit 1
fi
run_ca=$(podman exec "$run_proxy" sha256sum /opt/credentials/baffle/ca.crt | cut -d ' ' -f 1)
if [ "$run_ca" != "$ca_before" ]; then
  echo "one-off proxy is not using the persistent project CA" >&2
  exit 1
fi
run_origin_name="$project_name-run-origin"
phase="start TLS origin in the one-off proxy network"
podman run --detach --name "$run_origin_name" --network "container:$run_proxy" "$origin_image" >/dev/null
attempt=0
while ! podman exec "$run_origin_name" curl --insecure --fail --silent \
  https://localhost:8443/health >/dev/null 2>&1; do
  if [ "$attempt" -ge 60 ]; then
    echo "one-off TLS origin did not become ready" >&2
    exit 1
  fi
  attempt=$((attempt + 1))
  sleep 1
done
touch "$temp_root/workspace/.run-origin-ready"
phase="verify Baffle resolves the run-only secret override"
attempt=0
while [ ! -f "$temp_root/workspace/.run-request-done" ]; do
  if [ "$attempt" -ge 60 ] || [ -f "$temp_root/run.status" ]; then
    echo "one-off command did not complete its Baffle secret request" >&2
    exit 1
  fi
  attempt=$((attempt + 1))
  sleep 1
done
jq -e '.authorization == "old"' "$temp_root/workspace/.run-secret-response" >/dev/null
if [ "$(sha256_file "$project_root/config/proxy/sessions/$agent_session_config")" \
  != "$run_session_before" ]; then
  echo "one-off run changed the persistent Baffle session configuration" >&2
  exit 1
fi
if grep -F "$run_secret_value" "$temp_root/run.log" >/dev/null; then
  echo "one-off verbose log contains the run secret value" >&2
  exit 1
fi
phase="stop one-off TLS origin before runtime cleanup"
podman rm -f "$run_origin_name" >/dev/null
run_origin_name=
phase="verify one-off command preserves its nonzero exit status"
touch "$temp_root/workspace/.run-finish"
set +e
wait "$run_pid"
set -e
run_status=$(cat "$temp_root/run.status")
run_pid=
if [ "$run_status" -ne 7 ]; then
  echo "one-off command exited with status $run_status; expected 7" >&2
  exit 1
fi
require_socket_volume_count 0
phase="verify one-off runtime root removal"
if [ -e "$run_root" ]; then
  echo "one-off runtime root remains after the command exits: $run_root" >&2
  exit 1
fi
phase="verify run-only secret was not persisted"
if [ -e "$project_root/credentials/baffle/secrets/$run_secret_name" ]; then
  echo "one-off run created a persistent Baffle secret file" >&2
  exit 1
fi
phase="verify other persistent Baffle secret is unchanged"
if [ "$(sha256_file "$project_root/credentials/baffle/secrets/$run_persistent_secret_name")" \
  != "$run_persistent_secret_before" ]; then
  echo "one-off run changed a different persistent Baffle secret file" >&2
  exit 1
fi
phase="verify persistent project CA is unchanged"
ca_after_run=$(sha256_file "$project_root/credentials/baffle/ca.crt")
if [ "$ca_after_run" != "$ca_before" ]; then
  echo "one-off run changed the persistent project CA" >&2
  exit 1
fi
phase="verify one-off Podman resource cleanup"
if podman ps -a --format '{{.Names}}' | grep -E "^$run_name-(proxy|agent|nw-sandbox)(-instance)?$" >/dev/null; then
  echo "one-off runtime left a Podman resource behind" >&2
  exit 1
fi

echo "Baffle policy, HTTPS clients, reload, trust, project-state reuse, CA reuse, and run cleanup passed ($runtime runtime)"
