#!/bin/sh
set -eu

if [ "$#" -ne 2 ]; then
  echo "usage: $0 <cladding-binary> <baffle-binary>" >&2
  exit 2
fi

cladding_bin=$1
baffle_bin=$2
command -v python3 >/dev/null 2>&1 || {
  echo "required validation tool is missing: python3" >&2
  exit 2
}
script_dir=$(CDPATH= cd "$(dirname "$0")" && pwd)
control_check_script="$script_dir/validate_baffle_control_socket.sh"
socket_probe_bin=${BAFFLE_SOCKET_PROBE_BIN:-}
temp_parent=${BAFFLE_VALIDATION_TMP_DIR:-${TMPDIR:-/tmp}}
temp_root=$(mktemp -d "$temp_parent/cladding-baffle-validation.XXXXXX")
container_prefix="cladding-baffle-startup-$$"
disabled_container="${container_prefix}-disabled"
enabled_container="${container_prefix}-enabled"
socket_test_image="localhost/cladding-baffle-socket-test:latest"
runner_uid=$(id -u)
runner_gid=$(id -g)
container_uid=${BAFFLE_VALIDATION_CONTAINER_UID:-$runner_uid}
container_gid=${BAFFLE_VALIDATION_CONTAINER_GID:-$runner_gid}
socket_volume_names=
socket_volume_agent=
socket_volume_nw_sandbox=
proxy_name=
current_phase="initialize validation"
run_pid=

cleanup() {
  status=$?
  trap - EXIT
  if [ -n "$run_pid" ]; then
    kill "$run_pid" 2>/dev/null || true
    wait "$run_pid" 2>/dev/null || true
  fi
  if [ "$status" -ne 0 ]; then
    echo "::error title=Baffle socket validation failed::phase=$current_phase; validator exited with code $status"
    for name in "$disabled_container" "$enabled_container"; do
      if podman inspect "$name" >/dev/null 2>&1; then
        echo "--- $name state ---" >&2
        podman inspect --format '{{.State.Status}} (exit {{.State.ExitCode}})' "$name" >&2 || true
        echo "--- $name logs ---" >&2
        podman logs "$name" >&2 || true
      fi
    done
  fi
  podman rm -f "$disabled_container" "$enabled_container" >/dev/null 2>&1 || true
  for volume_name in $socket_volume_names; do
    podman volume rm "$volume_name" >/dev/null 2>&1 || true
  done
  rm -rf "$temp_root"
  exit "$status"
}
trap cleanup EXIT

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

container_stat_mode() {
  podman exec "$1" stat -c '%a' "$2" 2>/dev/null
}

container_stat_uid() {
  podman exec "$1" stat -c '%u' "$2" 2>/dev/null
}

redact_run_secret_log() {
  CLADDING_REDACT_SECRET="$run_secret_value" python3 -c '
import os
import sys

secret = os.environb.get(b"CLADDING_REDACT_SECRET", b"")
output = sys.stdin.buffer.read()
sys.stdout.buffer.write(output.replace(secret, b"[REDACTED]") if secret else output)
'
}

require_container_mode() {
  mode_container=$1
  mode_path=$2
  expected_mode=$3
  description=$4
  observed_mode=$(container_stat_mode "$mode_container" "$mode_path" 2>/dev/null || printf 'unavailable')
  if [ "$observed_mode" != "$expected_mode" ]; then
    message="$description path=$mode_path expected_mode=0$expected_mode observed_mode=$observed_mode"
    echo "$message" >&2
    annotation_message=$(printf '%s' "$message" | sed 's/%/%25/g; s/\r/%0D/g; s/\n/%0A/g')
    echo "::error title=Baffle socket permissions::$annotation_message"
    exit 1
  fi
}

proxy_socket_exists() {
  podman exec "$1" test -S "/run/cladding/proxy/$2/proxy.sock" >/dev/null 2>&1
}

require_mode() {
  mode_path=$1
  expected_mode=$2
  description=$3
  observed_mode=$(stat_mode "$mode_path" 2>/dev/null || printf 'unavailable')
  if [ "$observed_mode" != "$expected_mode" ]; then
    message="$description path=$mode_path expected_mode=0$expected_mode observed_mode=$observed_mode"
    echo "$message" >&2
    annotation_message=$(printf '%s' "$message" | sed 's/%/%25/g; s/\r/%0D/g; s/\n/%0A/g')
    echo "::error title=Baffle socket permissions::$annotation_message"
    exit 1
  fi
}

report_failure_output() {
  title=$1
  message=$2
  output_file=$3
  details=$(tail -n 20 "$output_file" | tr '\n' ' ' | sed 's/%/%25/g; s/\r/%0D/g')
  echo "::error title=$title::$message output=$details"
}

install_socket_test_rule() {
  rule_component=$1
  cat > "$project_root/config/proxy/sessions/$rule_component.toml" <<EOF
version = 2
persistent = true
socket_name = "$rule_component/proxy.sock"
unmatched = "deny"

[rules."example.com"]
EOF
  podman exec "$proxy_name" /opt/tools/bin/baffle reload --all
}

rootless=$(podman info --format '{{.Host.Security.Rootless}}')
if [ "$rootless" != true ]; then
  echo "Baffle config validation requires rootless Podman" >&2
  exit 1
fi

current_phase="build socket test image"
podman build --quiet --tag "$socket_test_image" \
  --file scripts/Containerfile.baffle-socket-test scripts

mkdir "$temp_root/workspace"
current_phase="initialize Baffle fixture"
(
  cd "$temp_root/workspace"
  "$cladding_bin" init bafflevalidation >/dev/null
)
project_root="$temp_root/workspace/.cladding"
current_phase="configure Baffle fixture"
jq '.agent.image = "docker.io/library/debian:trixie-slim" | .nw_sandbox.image = "docker.io/library/debian:trixie-slim"' \
  "$project_root/cladding.json" > "$project_root/cladding.json.tmp"
mv "$project_root/cladding.json.tmp" "$project_root/cladding.json"
for session_file in agent.toml nw-sandbox.toml; do
  session_path="$project_root/config/proxy/sessions/$session_file"
  if ! grep -Fqx 'unmatched = "deny"' "$session_path"; then
    echo "generated Baffle session does not deny unmatched hosts: $session_path" >&2
    exit 1
  fi
  if grep -Eq '^\[\[?rules\.' "$session_path"; then
    echo "generated Baffle session contains an active allow rule: $session_path" >&2
    exit 1
  fi
done
current_phase="build Cladding proxy image"
"$cladding_bin" --cladding-dir "$project_root" build
current_phase="verify embedded Baffle binary"
cmp "$project_root/tools/bin/baffle" "$baffle_bin"
if [ -n "$socket_probe_bin" ] && [ ! -x "$socket_probe_bin" ]; then
  echo "Baffle socket operation probe is missing or not executable: $socket_probe_bin" >&2
  exit 1
fi

run_baffle_ca_init() {
  podman run --rm --network none --userns keep-id \
    --volume "$project_root/config:/opt/config:ro" \
    --volume "$project_root/credentials/baffle:/opt/credentials/baffle:rw" \
    --volume "$project_root/tools/bin/baffle:/opt/tools/bin/baffle:ro" \
    --entrypoint /opt/tools/bin/baffle \
    localhost/cladding-proxy:latest \
    ca init --config /opt/config/proxy/daemon.toml
}

current_phase="validate CA initialized by cladding build"
test -f "$project_root/credentials/baffle/ca.crt"
test -f "$project_root/credentials/baffle/ca-key.pem"
test "$(stat_mode "$project_root/credentials/baffle/ca.crt")" = 644
test "$(stat_mode "$project_root/credentials/baffle/ca-key.pem")" = 600
if [ "$(stat_uid "$project_root/credentials/baffle/ca.crt")" != "$runner_uid" ]; then
  echo "Baffle CA certificate is not owned by the invoking host user" >&2
  exit 1
fi
if [ "$(stat_uid "$project_root/credentials/baffle/ca-key.pem")" != "$runner_uid" ]; then
  echo "Baffle CA private key is not owned by the invoking host user" >&2
  exit 1
fi
ca_init_cert_before=$(sha256sum "$project_root/credentials/baffle/ca.crt" | cut -d ' ' -f 1)
ca_init_key_before=$(sha256sum "$project_root/credentials/baffle/ca-key.pem" | cut -d ' ' -f 1)

current_phase="verify Baffle refuses to overwrite the initialized CA"
ca_init_output="$temp_root/ca-init.log"
if run_baffle_ca_init >"$ca_init_output" 2>&1; then
  cat "$ca_init_output" >&2
  echo "Baffle CA initialization unexpectedly replaced existing CA material" >&2
  exit 1
else
  if ! grep -F "refusing to overwrite existing" "$ca_init_output" >/dev/null; then
    cat "$ca_init_output" >&2
    echo "Baffle CA initialization did not report the existing-file conflict" >&2
    exit 1
  fi
fi
test "$(sha256sum "$project_root/credentials/baffle/ca.crt" | cut -d ' ' -f 1)" = "$ca_init_cert_before"
test "$(sha256sum "$project_root/credentials/baffle/ca-key.pem" | cut -d ' ' -f 1)" = "$ca_init_key_before"

verify_scoped_socket_access() {
  component=$1
  socket_runtime=${2:-default}
  current_phase="verify $component socket permissions ($socket_runtime runtime)"

  require_container_mode "$proxy_name" /run/cladding/proxy 700 \
    "Baffle socket root directory mode mismatch"
  if [ "$(container_stat_uid "$proxy_name" /run/cladding/proxy)" != "$container_uid" ]; then
    echo "Baffle socket root directory is not owned by the proxy user" >&2
    exit 1
  fi
  require_container_mode "$proxy_name" "/run/cladding/proxy/$component" 700 \
    "Baffle $component socket directory mode mismatch"
  require_container_mode "$proxy_name" "/run/cladding/proxy/$component/proxy.sock" 600 \
    "Baffle $component socket mode mismatch"
  if [ "$(container_stat_uid "$proxy_name" "/run/cladding/proxy/$component")" != "$container_uid" ]; then
    echo "Baffle $component socket directory is not owned by the proxy user" >&2
    exit 1
  fi
  if [ "$(container_stat_uid "$proxy_name" "/run/cladding/proxy/$component/proxy.sock")" != "$container_uid" ]; then
    echo "Baffle $component socket is not owned by the proxy user" >&2
    exit 1
  fi
  if [ "$component" = agent ]; then
    component_volume=$socket_volume_agent
  else
    component_volume=$socket_volume_nw_sandbox
  fi
  component_mount="$component_volume:/run/cladding/proxy/$component:rw"

  echo "Testing rootless keep-id socket access for $component ($socket_runtime runtime)"
  current_phase="test $component socket access ($socket_runtime runtime)"
  if [ "$socket_runtime" = runsc ]; then
    set -- podman --runtime runsc \
      --runtime-flag ignore-cgroups \
      --runtime-flag host-uds=all \
      --runtime-flag network=none \
      run
  else
    set -- podman run
  fi
  output_file="$temp_root/socket-test-$component-$socket_runtime.log"
  if [ "$component" = agent ]; then
    test_rule_enabled=$socket_test_rule_agent
  else
    test_rule_enabled=$socket_test_rule_nw_sandbox
  fi
  if [ "$test_rule_enabled" != true ]; then
    denial_output_file="$temp_root/socket-denial-$component-$socket_runtime.log"
    if "$@" --rm --network none --userns keep-id \
      --user "$container_uid:$container_gid" \
      --volume "$component_mount" \
      --entrypoint /bin/sh "$socket_test_image" -ec '
        socket_path=$1
        test -S "$socket_path"
        socat TCP-LISTEN:3128,bind=127.0.0.1,fork,reuseaddr \
          UNIX-CONNECT:"$socket_path" &
        bridge_pid=$!
        trap '\''kill "$bridge_pid" 2>/dev/null || true; wait "$bridge_pid" 2>/dev/null || true'\'' EXIT
        sleep 1
        if curl --fail --silent --show-error --connect-timeout 10 --max-time 30 \
          --proxy http://127.0.0.1:3128 --output /dev/null https://example.com/; then
          echo "generated Baffle policy unexpectedly allowed example.com" >&2
          exit 88
        fi
      ' socket-denial "/run/cladding/proxy/$component/proxy.sock" \
      >"$denial_output_file" 2>&1; then
      :
    else
      status=$?
      cat "$denial_output_file" >&2
      if [ "$status" -eq 88 ]; then
        echo "generated Baffle policy unexpectedly allowed example.com for $component" >&2
      else
        report_failure_output "Baffle default-deny socket check failed" \
          "component=$component runtime=$socket_runtime exit=$status" "$denial_output_file"
      fi
      exit "$status"
    fi

    install_socket_test_rule "$component"
    if [ "$component" = agent ]; then
      socket_test_rule_agent=true
    else
      socket_test_rule_nw_sandbox=true
    fi
  fi
  if "$@" --rm --network none --userns keep-id \
    --user "$container_uid:$container_gid" \
    --volume "$component_mount" \
    --entrypoint /bin/sh "$socket_test_image" -ec '
      socket_path=$1
      test -S "$socket_path"
      socat TCP-LISTEN:3128,bind=127.0.0.1,fork,reuseaddr \
        UNIX-CONNECT:"$socket_path" &
      bridge_pid=$!
      trap '\''kill "$bridge_pid" 2>/dev/null || true; wait "$bridge_pid" 2>/dev/null || true'\'' EXIT
      sleep 1
      curl --fail --silent --show-error --connect-timeout 10 --max-time 30 \
        --proxy http://127.0.0.1:3128 --output /dev/null https://example.com/
    ' socket-test "/run/cladding/proxy/$component/proxy.sock" >"$output_file" 2>&1; then
    cat "$output_file"
  else
    status=$?
    cat "$output_file" >&2
    report_failure_output "Baffle socket access failed" \
      "component=$component runtime=$socket_runtime exit=$status" "$output_file"
    exit "$status"
  fi
  echo "Rootless keep-id socket access passed for the $component proxy endpoint ($socket_runtime runtime)"
}

run_proxy_startup() {
  name=$1
  sandbox_enabled=$2
  sandbox_state=$3
  cp "$script_dir/../config-template/proxy/sessions/agent.toml" \
    "$project_root/config/proxy/sessions/agent.toml"
  cp "$script_dir/../config-template/proxy/sessions/nw-sandbox.toml" \
    "$project_root/config/proxy/sessions/nw-sandbox.toml"
  socket_test_rule_agent=false
  socket_test_rule_nw_sandbox=false
  proxy_name=$name
  current_phase="start proxy ($name)"
  socket_volume_agent="$container_prefix-$name-agent"
  podman volume create --opt nocopy "$socket_volume_agent" >/dev/null
  socket_volume_names="$socket_volume_names $socket_volume_agent"
  if [ "$sandbox_enabled" = true ]; then
    socket_volume_nw_sandbox="$container_prefix-$name-nw-sandbox"
    podman volume create --opt nocopy "$socket_volume_nw_sandbox" >/dev/null
    socket_volume_names="$socket_volume_names $socket_volume_nw_sandbox"
  else
    socket_volume_nw_sandbox=
  fi

  socket_dir="$temp_root/sockets-$name"
  mkdir -p "$socket_dir/agent"
  chmod 700 "$socket_dir" "$socket_dir/agent"
  if [ "$sandbox_enabled" = true ]; then
    mkdir -p "$socket_dir/nw-sandbox"
    chmod 700 "$socket_dir/nw-sandbox"
  fi

  start_output_file="$temp_root/proxy-start-$name.log"
  current_phase="create proxy container ($name)"
  set -- podman run --detach --init --name "$name" \
    --userns keep-id \
    --env "CLADDING_NW_SANDBOX_ENABLED=$sandbox_enabled" \
    --env CLADDING_BAFFLE_BIND_PROBE=true \
    --env RUST_LOG=debug \
    --env RUST_BACKTRACE=1 \
    --volume "$project_root/tools/bin/baffle:/opt/tools/bin/baffle:ro" \
    --volume "$project_root/runtime/scripts/proxy_startup.sh:/opt/scripts/proxy_startup.sh:ro" \
    --volume "$project_root/config:/opt/config:ro" \
    --volume "$project_root/credentials/baffle:/opt/credentials/baffle:ro" \
    --volume "$socket_dir:/run/cladding/proxy:rw" \
    --volume "$socket_volume_agent:/run/cladding/proxy/agent:U"
  if [ "$sandbox_enabled" = true ]; then
    set -- "$@" \
      --volume "$socket_volume_nw_sandbox:/run/cladding/proxy/nw-sandbox:U"
  fi
  if [ -n "$socket_probe_bin" ]; then
    set -- "$@" \
      --volume "$socket_probe_bin:/opt/tools/bin/baffle-socket-probe:ro" \
      --env CLADDING_BAFFLE_EXACT_BIND_PROBE=true
  fi
  if "$@" --entrypoint /bin/sh \
    localhost/cladding-proxy:latest /opt/scripts/proxy_startup.sh >"$start_output_file" 2>&1; then
    :
  else
    status=$?
    cat "$start_output_file" >&2
    report_failure_output "Baffle proxy container failed to start" \
      "container=$name exit=$status" "$start_output_file"
    exit "$status"
  fi

  ready=false
  attempt=0
  while [ "$attempt" -lt 60 ]; do
    if proxy_socket_exists "$name" agent; then
      if [ "$sandbox_enabled" = false ] || proxy_socket_exists "$name" nw-sandbox; then
        ready=true
        break
      fi
    fi
    if [ "$(podman inspect --format '{{.State.Running}}' "$name")" != true ]; then
      if podman logs "$name" >"$start_output_file" 2>&1; then
        cat "$start_output_file" >&2
        report_failure_output "Baffle proxy startup exited" \
          "container=$name state=not-running" "$start_output_file"
      fi
      exit 1
    fi
    attempt=$((attempt + 1))
    sleep 1
  done
  if [ "$ready" != true ]; then
    if podman logs "$name" >"$start_output_file" 2>&1; then
      cat "$start_output_file" >&2
      report_failure_output "Baffle proxy startup timed out" \
        "container=$name expected=$sandbox_state" "$start_output_file"
    fi
    echo "Baffle startup did not create the expected $sandbox_state session socket" >&2
    exit 1
  fi

  current_phase="verify Podman init helper mount ($name)"
  if ! podman exec "$name" test -x /run/podman-init; then
    echo "Podman did not mount its configured init helper at /run/podman-init" >&2
    exit 1
  fi

  current_phase="verify private proxy control socket ($name)"
  control_output_file="$temp_root/control-check-$name.log"
  if podman exec -i "$name" /bin/sh -ex \
    < "$control_check_script" >"$control_output_file" 2>&1; then
    echo "Private Baffle control socket and runtime configuration verified for $name"
  else
    status=$?
    cat "$control_output_file" >&2
    report_failure_output "Baffle private control socket validation failed" \
      "container=$name exit=$status" "$control_output_file"
    podman logs "$name" >&2
    exit "$status"
  fi

  if [ "$sandbox_enabled" = false ] && proxy_socket_exists "$name" nw-sandbox; then
    echo "Baffle startup created a network-sandbox session while it was disabled" >&2
    exit 1
  fi

  verify_scoped_socket_access agent
  if [ "$sandbox_enabled" = true ]; then
    verify_scoped_socket_access nw-sandbox
  fi
  if [ "${CLADDING_TEST_RUNSC:-false}" = true ]; then
    verify_scoped_socket_access agent runsc
    if [ "$sandbox_enabled" = true ]; then
      verify_scoped_socket_access nw-sandbox runsc
    fi
  fi

  podman stop --time 5 "$name" >/dev/null
  if [ "$(podman inspect --format '{{.State.Status}}' "$name")" != exited ]; then
    echo "Baffle proxy container did not stop cleanly" >&2
    podman logs "$name" >&2
    exit 1
  fi
  for component in agent nw-sandbox; do
    if [ "$component" = nw-sandbox ] && [ "$sandbox_enabled" = false ]; then
      continue
    fi
    if [ "$component" = agent ]; then
      component_volume=$socket_volume_agent
    else
      component_volume=$socket_volume_nw_sandbox
    fi
    if podman run --rm --network none --userns keep-id \
      --user "$container_uid:$container_gid" \
      --volume "$component_volume:/run/cladding/proxy/$component:ro" \
      --entrypoint /bin/sh "$socket_test_image" -ec \
      "test ! -e /run/cladding/proxy/$component/proxy.sock"; then
      :
    else
      echo "Baffle left a $component session socket after proxy shutdown" >&2
      exit 1
    fi
  done
}

run_proxy_startup "$disabled_container" false "agent"
run_proxy_startup "$enabled_container" true "agent and network-sandbox"

current_phase="verify private per-run Baffle secrets overlay"
run_secret_name="mount-probe"
run_secret_value="cladding-test-run-secret"
run_project_secret_name="mount-project-probe"
redacted_log=$(printf 'before:%s:after\n' "$run_secret_value" | redact_run_secret_log)
if [ "$redacted_log" != 'before:[REDACTED]:after' ]; then
  echo "Baffle validation log redaction did not remove the run secret value" >&2
  exit 1
fi
printf '%s' "cladding-test-persistent-secret" \
  > "$project_root/credentials/baffle/secrets/$run_secret_name"
chmod 0600 "$project_root/credentials/baffle/secrets/$run_secret_name"
printf '%s' "cladding-test-other-persistent-secret" \
  > "$project_root/credentials/baffle/secrets/$run_project_secret_name"
chmod 0600 "$project_root/credentials/baffle/secrets/$run_project_secret_name"
run_persistent_secret_before=$(sha256sum \
  "$project_root/credentials/baffle/secrets/$run_secret_name" | cut -d ' ' -f 1)
run_project_secret_before=$(sha256sum \
  "$project_root/credentials/baffle/secrets/$run_project_secret_name" | cut -d ' ' -f 1)
jq '.agent.image = "localhost/cladding-proxy:latest" | .nw_sandbox.enabled = false' \
  "$project_root/cladding.json" > "$project_root/cladding.json.tmp"
mv "$project_root/cladding.json.tmp" "$project_root/cladding.json"
mkdir -m 0700 "$temp_root/run-tmp"
(
  cd "$temp_root/workspace"
  export CLADDING_RUN_SECRET_PROBE="$run_secret_value"
  if TMPDIR="$temp_root/run-tmp" "$cladding_bin" --cladding-dir "$project_root" run -v \
    --secret "$run_secret_name=env:CLADDING_RUN_SECRET_PROBE" -- \
    /bin/sh -ec 'test -z "${CLADDING_RUN_SECRET_PROBE+x}"
      while [ ! -f /home/user/workspace/.run-secret-mount-finish ]; do sleep 1; done'; then
    run_status=0
  else
    run_status=$?
  fi
  printf '%s\n' "$run_status" > "$temp_root/run-secret-mount.status"
  exit "$run_status"
) > "$temp_root/run-secret-mount.log" 2>&1 &
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
  if [ -f "$temp_root/run-secret-mount.status" ]; then
    redact_run_secret_log < "$temp_root/run-secret-mount.log" >&2
    echo "one-off secret mount runtime exited before creating its private runtime root" >&2
    exit 1
  fi
  attempt=$((attempt + 1))
  sleep 1
done
if [ -z "$run_root" ]; then
  echo "one-off secret mount runtime did not create its private runtime root" >&2
  exit 1
fi
run_name=$(sed -n 's/^starting one-off instance: //p' \
  "$temp_root/run-secret-mount.log" | head -n 1)
run_proxy="$run_name-proxy-instance"
run_agent="$run_name-agent-instance"
attempt=0
while ! podman container exists "$run_proxy" >/dev/null 2>&1 \
  || ! podman container exists "$run_agent" >/dev/null 2>&1; do
  if [ "$attempt" -ge 60 ] || [ -f "$temp_root/run-secret-mount.status" ]; then
    redact_run_secret_log < "$temp_root/run-secret-mount.log" >&2
    echo "one-off secret mount runtime did not start proxy and agent containers" >&2
    exit 1
  fi
  attempt=$((attempt + 1))
  sleep 1
done
run_secret_directory="$run_root/runtime/secrets"
run_secret_destination="/opt/credentials/baffle/secrets/$run_secret_name"
require_mode "$run_secret_directory" 700 "Run secret directory mode mismatch"
require_mode "$run_secret_directory/$run_secret_name" 600 "Run secret file mode mismatch"
if ! podman inspect "$run_proxy" | jq -e \
  --arg source "$run_secret_directory" \
  --arg destination "/opt/credentials/baffle/secrets" \
  '.[0].Mounts | any(.Source == $source and .Destination == $destination and .RW == false)' \
  >/dev/null; then
  echo "one-off proxy did not mount its run secret directory read-only over the Baffle secrets path" >&2
  exit 1
fi
if ! podman inspect "$run_proxy" | jq -e \
  --arg source "$project_root/credentials/baffle/secrets/$run_project_secret_name" \
  --arg destination "/opt/credentials/baffle/secrets/$run_project_secret_name" \
  '.[0].Mounts | any(.Source == $source and .Destination == $destination and .RW == false)' \
  >/dev/null; then
  echo "one-off proxy did not preserve the non-overridden project secret as a read-only mount" >&2
  exit 1
fi
if podman inspect "$run_agent" | jq -e \
  --arg source "$run_secret_directory" \
  --arg destination "/opt/credentials/baffle/secrets" \
  '.[0].Mounts | any(.Source == $source or .Destination == $destination or (.Destination | startswith($destination + "/")))' \
  >/dev/null; then
  echo "one-off agent received the run secret override" >&2
  exit 1
fi
podman exec "$run_proxy" test -s /opt/credentials/baffle/ca.crt
podman exec "$run_proxy" test -s "/opt/credentials/baffle/secrets/$run_secret_name"
podman exec "$run_proxy" test -s "/opt/credentials/baffle/secrets/$run_project_secret_name"
expected_run_secret_hash=$(printf '%s' "$run_secret_value" | sha256sum | cut -d ' ' -f 1)
mounted_run_secret_hash=$(podman exec "$run_proxy" sha256sum "$run_secret_destination" | cut -d ' ' -f 1)
if [ "$expected_run_secret_hash" != "$mounted_run_secret_hash" ]; then
  echo "one-off proxy did not read the host environment secret value" >&2
  exit 1
fi
expected_project_secret_hash=$(sha256sum \
  "$project_root/credentials/baffle/secrets/$run_project_secret_name" | cut -d ' ' -f 1)
mounted_project_secret_hash=$(podman exec "$run_proxy" sha256sum \
  "/opt/credentials/baffle/secrets/$run_project_secret_name" | cut -d ' ' -f 1)
if [ "$expected_project_secret_hash" != "$mounted_project_secret_hash" ]; then
  echo "one-off proxy could not read the non-overridden project secret" >&2
  exit 1
fi
if grep -F "$run_secret_value" "$temp_root/run-secret-mount.log" >/dev/null; then
  echo "one-off verbose log contains the run secret value" >&2
  exit 1
fi
touch "$temp_root/workspace/.run-secret-mount-finish"
wait "$run_pid"
run_pid=
if [ -e "$run_root" ]; then
  echo "one-off secret mount runtime directory remains after cleanup" >&2
  exit 1
fi
if [ "$(sha256sum "$project_root/credentials/baffle/secrets/$run_secret_name" | cut -d ' ' -f 1)" \
  != "$run_persistent_secret_before" ]; then
  echo "one-off secret override changed the persistent Baffle secret file" >&2
  exit 1
fi
if [ "$(sha256sum "$project_root/credentials/baffle/secrets/$run_project_secret_name" | cut -d ' ' -f 1)" \
  != "$run_project_secret_before" ]; then
  echo "one-off secret overlay changed the non-overridden project secret file" >&2
  exit 1
fi

echo "Baffle proxy startup, run-secret overlay, project-secret passthrough, and persistent-session shutdown passed under rootless Podman"
