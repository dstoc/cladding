#!/bin/sh
set -eu

if [ "$#" -ne 2 ]; then
  echo "usage: $0 <cladding-binary> <baffle-binary>" >&2
  exit 2
fi

cladding_bin=$1
baffle_bin=$2
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
socket_relay=false
if [ "$(uname -s)" = Darwin ]; then
  socket_relay=true
fi
current_phase="initialize validation"

cleanup() {
  status=$?
  trap - EXIT
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

report_failure_output() {
  title=$1
  message=$2
  output_file=$3
  details=$(tail -n 8 "$output_file" | tr '\n' ' ' | sed 's/%/%25/g; s/\r/%0D/g')
  echo "::error title=$title::$message output=$details"
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
current_phase="build Cladding proxy image"
"$cladding_bin" --cladding-dir "$project_root" build
current_phase="verify embedded Baffle binary"
cmp "$project_root/tools/bin/baffle" "$baffle_bin"

verify_scoped_socket_access() {
  component=$1
  socket_dir=$2
  socket_runtime=${3:-default}
  component_dir="$socket_dir/$component"
  socket_path="$component_dir/proxy.sock"
  current_phase="verify $component socket permissions ($socket_runtime runtime)"

  if [ "$(stat_mode "$component_dir")" != 700 ]; then
    echo "Baffle $component socket directory is not mode 0700" >&2
    exit 1
  fi
  if [ "$(stat_mode "$socket_path")" != 600 ]; then
    echo "Baffle $component socket is not mode 0600" >&2
    exit 1
  fi
  if [ "${BAFFLE_VALIDATION_CHECK_HOST_UID:-true}" = true ] \
    && [ "$(stat_uid "$socket_path")" != "$runner_uid" ]; then
    echo "Baffle $component socket is not owned by the invoking host user" >&2
    exit 1
  fi

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
  if "$@" --rm --network none --userns keep-id \
    --user "$container_uid:$container_gid" \
    --volume "$component_dir:/run/cladding/proxy/$component:rw" \
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
  socket_dir="$temp_root/sockets-$name"
  current_phase="start proxy ($name)"
  mkdir -p "$socket_dir/agent"
  if [ "$sandbox_enabled" = true ]; then
    mkdir -p "$socket_dir/nw-sandbox"
  fi
  chmod 700 "$socket_dir" "$socket_dir/agent"
  if [ "$sandbox_enabled" = true ]; then
    chmod 700 "$socket_dir/nw-sandbox"
  fi

  start_output_file="$temp_root/proxy-start-$name.log"
  current_phase="create proxy container ($name)"
  if podman run --detach --name "$name" \
    --userns keep-id \
    --env "CLADDING_NW_SANDBOX_ENABLED=$sandbox_enabled" \
    --env "CLADDING_BAFFLE_SOCKET_RELAY=$socket_relay" \
    --volume "$project_root/tools/bin/baffle:/opt/tools/bin/baffle:ro" \
    --volume "$project_root/runtime/scripts/proxy_startup.sh:/opt/scripts/proxy_startup.sh:ro" \
    --volume "$project_root/config:/opt/config:ro" \
    --volume "$project_root/credentials/baffle:/opt/credentials/baffle:ro" \
    --volume "$socket_dir:/run/cladding/proxy:rw" \
    --entrypoint /bin/sh \
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
    if [ -S "$socket_dir/agent/proxy.sock" ]; then
      if [ "$sandbox_enabled" = false ] || [ -S "$socket_dir/nw-sandbox/proxy.sock" ]; then
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

  current_phase="verify private proxy control socket ($name)"
  if ! podman exec "$name" /bin/sh -ec '
    expected_uid=$(id -u)
    private_dir="/run/baffle/$expected_uid"
    test "$(stat -c %a /run/baffle)" = 1733
    test -d "$private_dir"
    test "$(stat -c %a "$private_dir")" = 700
    test "$(stat -c %u "$private_dir")" = "$expected_uid"
    test -f "$private_dir/daemon.toml"
    test "$(stat -c %a "$private_dir/daemon.toml")" = 600
    test "$(stat -c %u "$private_dir/daemon.toml")" = "$expected_uid"
    test -S "$private_dir/control.sock"
    test "$(stat -c %a "$private_dir/control.sock")" = 600
    test "$(stat -c %u "$private_dir/control.sock")" = "$expected_uid"
    test -L /run/baffle/control.sock
    test "$(readlink /run/baffle/control.sock)" = "$private_dir/control.sock"
    grep -q "^trusted_operator_uid = $expected_uid$" "$private_dir/daemon.toml"
    grep -q "^control_socket = \"$private_dir/control.sock\"$" "$private_dir/daemon.toml"
    if [ "${CLADDING_BAFFLE_SOCKET_RELAY:-false}" = true ]; then
      test "$(stat -c %a "$private_dir/data")" = 700
      test "$(stat -c %u "$private_dir/data")" = "$expected_uid"
      test "$(stat -c %a "$private_dir/data/agent")" = 700
      test "$(stat -c %u "$private_dir/data/agent")" = "$expected_uid"
      test -S "$private_dir/data/agent/proxy.sock"
      test "$(stat -c %a "$private_dir/data/agent/proxy.sock")" = 600
      test "$(stat -c %u "$private_dir/data/agent/proxy.sock")" = "$expected_uid"
      if [ "${CLADDING_NW_SANDBOX_ENABLED:-false}" = true ]; then
        test "$(stat -c %a "$private_dir/data/nw-sandbox")" = 700
        test "$(stat -c %u "$private_dir/data/nw-sandbox")" = "$expected_uid"
        test -S "$private_dir/data/nw-sandbox/proxy.sock"
        test "$(stat -c %a "$private_dir/data/nw-sandbox/proxy.sock")" = 600
        test "$(stat -c %u "$private_dir/data/nw-sandbox/proxy.sock")" = "$expected_uid"
      else
        test ! -e "$private_dir/data/nw-sandbox"
      fi
    fi
  '; then
    echo "Baffle private control socket or runtime configuration permissions are invalid" >&2
    podman logs "$name" >&2
    exit 1
  fi

  if [ "$sandbox_enabled" = false ] && [ -S "$socket_dir/nw-sandbox/proxy.sock" ]; then
    echo "Baffle startup created a network-sandbox session while it was disabled" >&2
    exit 1
  fi

  verify_scoped_socket_access agent "$socket_dir"
  if [ "$sandbox_enabled" = true ]; then
    verify_scoped_socket_access nw-sandbox "$socket_dir"
  fi
  if [ "${CLADDING_TEST_RUNSC:-false}" = true ]; then
    verify_scoped_socket_access agent "$socket_dir" runsc
    if [ "$sandbox_enabled" = true ]; then
      verify_scoped_socket_access nw-sandbox "$socket_dir" runsc
    fi
  fi

  podman stop --time 5 "$name" >/dev/null
  if [ "$(podman inspect --format '{{.State.Status}}' "$name")" != exited ]; then
    echo "Baffle proxy container did not stop cleanly" >&2
    podman logs "$name" >&2
    exit 1
  fi
  if [ -S "$socket_dir/agent/proxy.sock" ]; then
    echo "Baffle left the agent session socket after proxy shutdown" >&2
    exit 1
  fi
  if [ -S "$socket_dir/nw-sandbox/proxy.sock" ]; then
    echo "Baffle left a network-sandbox session socket after proxy shutdown" >&2
    exit 1
  fi
}

run_proxy_startup "$disabled_container" false "agent"
run_proxy_startup "$enabled_container" true "agent and network-sandbox"
echo "Baffle proxy startup and persistent-session shutdown passed under rootless Podman"
