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

for tool in podman jq openssl git sha256sum; do
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

temp_root=$(mktemp -d "/tmp/cladding-baffle-runtime.XXXXXX")
project_name="baffleintegration$$"
project_root="$temp_root/workspace/.cladding"
origin_name="$project_name-origin"
proxy_image="localhost/$project_name-proxy:latest"
client_image="localhost/$project_name-client:latest"
origin_image="localhost/$project_name-origin:latest"
run_pid=
phase=initialize

cleanup() {
  status=$?
  trap - EXIT
  if [ -n "$run_pid" ]; then
    kill "$run_pid" 2>/dev/null || true
    wait "$run_pid" 2>/dev/null || true
  fi
  if [ "$status" -ne 0 ]; then
    echo "Baffle runtime integration failed during: $phase" >&2
    printf '::error title=Baffle runtime integration phase::%s (exit code %s)\n' \
      "$phase" "$status"
    podman logs "$project_name-proxy-instance" >&2 2>/dev/null || true
    podman logs "$origin_name" >&2 2>/dev/null || true
  fi
  podman rm -f "$origin_name" >/dev/null 2>&1 || true
  "$cladding_bin" --cladding-dir "$project_root" down >/dev/null 2>&1 || true
  podman pod rm -f "$project_name-proxy" >/dev/null 2>&1 || true
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
cmp "$script_dir/../config-template/nw_sandbox/main.rego" \
  "$project_root/config/nw_sandbox/main.rego"
cmp "$script_dir/../config-template/nw_sandbox/curl.rego" \
  "$project_root/config/nw_sandbox/curl.rego"
test -z "$(find "$project_root/config" -iname '*squid*' -print)"
test "$(stat -c '%a:%u' "$project_root/config/proxy/sessions")" = "755:$(id -u)"
for session_file in agent.toml nw-sandbox.toml; do
  test "$(stat -c '%a:%u' "$project_root/config/proxy/sessions/$session_file")" = "644:$(id -u)"
done
test "$(stat -c '%a:%u' "$project_root/credentials/baffle")" = "700:$(id -u)"
test "$(stat -c '%a:%u' "$project_root/credentials/baffle/secrets")" = "700:$(id -u)"
test "$(stat -c '%a:%u' "$project_root/credentials/baffle/ca.crt")" = "644:$(id -u)"
test "$(stat -c '%a:%u' "$project_root/credentials/baffle/ca-key.pem")" = "600:$(id -u)"
jq --arg image "$client_image" --arg runtime "$runtime" \
  '.agent.image = $image
   | .nw_sandbox.image = $image
   | .use_runsc = ($runtime == "runsc")' \
  "$project_root/cladding.json" > "$project_root/cladding.json.tmp"
mv "$project_root/cladding.json.tmp" "$project_root/cladding.json"

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
allowed = ["test-token-old", "test-token-new", "missing-token"]
EOF
write_agent_policy() {
  token=$1
  path=$2
  cat > "$project_root/config/proxy/sessions/agent.toml" <<EOF
version = 1
operation = "create"

[session]
persistent = true
socket_name = "agent/proxy.sock"

[[rules]]
host = "localhost"
mode = "intercept"
ports = [8443]
paths = ["$path"]

[[rules.inject]]
header = "Authorization"
secret = "$token"
format = "bearer"
EOF
}
cat > "$project_root/config/proxy/sessions/nw-sandbox.toml" <<'EOF'
version = 1
operation = "create"

[session]
persistent = true
socket_name = "nw-sandbox/proxy.sock"

[[rules]]
host = "localhost"
mode = "intercept"
ports = [9443]
paths = ["/sandbox/**"]
EOF
write_agent_policy test-token-old "/authorized/**"
printf '%s' "cladding-test-old-value" > "$project_root/credentials/baffle/secrets/test-token-old"
printf '%s' "cladding-test-new-value" > "$project_root/credentials/baffle/secrets/test-token-new"
chmod 0644 "$project_root/credentials/baffle/secrets/test-token-old"
chmod 0600 "$project_root/credentials/baffle/secrets/test-token-new"
ca_before=$(sha256sum "$project_root/credentials/baffle/ca.crt" | cut -d ' ' -f 1)
jq --arg image "$proxy_image" '.proxy = {image: $image}' \
  "$project_root/cladding.json" > "$project_root/cladding.json.tmp"
mv "$project_root/cladding.json.tmp" "$project_root/cladding.json"

phase="start Cladding runtime"
"$cladding_bin" --cladding-dir "$project_root" up
phase="verify injected secret permissions"
secret_mode=$(stat -c '%a' "$project_root/credentials/baffle/secrets/test-token-old")
if [ "$secret_mode" != 600 ]; then
  echo "expected test-token-old mode 600 after startup, got $secret_mode" >&2
  exit 1
fi
phase="start local TLS origin container"
podman run --detach --name "$origin_name" --pod "$project_name-proxy" "$origin_image" >/dev/null
agent="$project_name-agent-instance"
sandbox="$project_name-nw-sandbox-instance"
proxy="$project_name-proxy-instance"
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
    test -r /opt/config/proxy/sessions/agent.toml
    test -r /opt/config/nw_sandbox/main.rego
    test -r /opt/config/nw_sandbox/curl.rego
    test -r /run/cladding/ca/baffle.crt
    test ! -e /opt/credentials/baffle/ca-key.pem
    test ! -e /opt/credentials/baffle/secrets/test-token-old
    test ! -e /run/baffle/control.sock
    test -S "/run/cladding/proxy/$1/proxy.sock"
    test ! -e "/run/cladding/proxy/$2/proxy.sock"
  ' sh "$component" "$other_component"
  podman exec --user 0 "$container" cmp \
    /run/cladding/ca/baffle.crt /usr/local/share/ca-certificates/baffle.crt
done
test "$(stat -c '%a' "$project_root/credentials/baffle")" = 700
test "$(stat -c '%a' "$project_root/credentials/baffle/ca.crt")" = 644
test "$(stat -c '%a' "$project_root/credentials/baffle/ca-key.pem")" = 600
test "$(stat -c '%u' "$project_root/credentials/baffle/ca-key.pem")" = "$(id -u)"

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
version = 1
operation = "create"

[session]
persistent = false
socket_name = "agent/missing-secret.sock"
[[rules]]
host = "localhost"
mode = "intercept"
ports = [8443]
paths = ["/missing/**"]
[[rules.inject]]
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

cp "$project_root/config/proxy/sessions/agent.toml" "$temp_root/agent.valid.toml"
printf '%s\n' 'invalid = [' > "$project_root/config/proxy/sessions/agent.toml"
if "$cladding_bin" --cladding-dir "$project_root" reload-proxy >/dev/null 2>&1; then
  echo "Baffle reload accepted invalid session TOML" >&2
  exit 1
fi
cp "$temp_root/agent.valid.toml" "$project_root/config/proxy/sessions/agent.toml"
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
"$cladding_bin" --cladding-dir "$project_root" down
ca_after=$(sha256sum "$project_root/credentials/baffle/ca.crt" | cut -d ' ' -f 1)
test "$ca_before" = "$ca_after"
"$cladding_bin" --cladding-dir "$project_root" up
"$cladding_bin" --cladding-dir "$project_root" down
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

phase="verify one-off CA isolation and nonzero-command cleanup"
mkdir -m 0700 "$temp_root/run-tmp"
(
  TMPDIR="$temp_root/run-tmp" "$cladding_bin" --cladding-dir "$project_root" run -- \
    /bin/sh -c 'sleep 3; exit 7' > "$temp_root/run.log" 2>&1 &
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
    -path '*/credentials/baffle/ca.crt' -print -quit | sed 's|/credentials/baffle/ca.crt$||')
  if [ -n "$run_root" ]; then
    break
  fi
  if [ -f "$temp_root/run.status" ]; then
    cat "$temp_root/run.log" >&2
    echo "one-off runtime exited before creating private credentials" >&2
    exit 1
  fi
  attempt=$((attempt + 1))
  sleep 1
done
if [ -z "$run_root" ]; then
  echo "one-off runtime did not expose its private CA while active" >&2
  exit 1
fi
run_ca=$(sha256sum "$run_root/credentials/baffle/ca.crt" | cut -d ' ' -f 1)
test "$run_ca" != "$ca_before"
attempt=0
while [ ! -d "$run_root/credentials/baffle/secrets" ] && [ "$attempt" -lt 30 ]; do
  attempt=$((attempt + 1))
  sleep 1
done
test -d "$run_root/credentials/baffle/secrets"
test -z "$(find "$run_root/credentials/baffle/secrets" -mindepth 1 -print -quit)"
set +e
wait "$run_pid"
set -e
run_status=$(cat "$temp_root/run.status")
run_pid=
test "$run_status" -eq 7
test ! -e "$run_root"
run_name=$(sed -n 's/^starting one-off instance: //p' "$temp_root/run.log" | head -n 1)
test -n "$run_name"
if podman ps -a --format '{{.Names}}' | grep -E "^$run_name-(proxy|agent|nw-sandbox)(-instance)?$" >/dev/null; then
  echo "one-off runtime left a Podman resource behind" >&2
  exit 1
fi

echo "Baffle policy, HTTPS clients, reload, trust, isolation, CA reuse, and run cleanup passed ($runtime runtime)"
