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

for tool in podman jq python3 socat; do
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
topology_image=localhost/cladding-runtime-topology:latest
proxy=
agent=
nw_sandbox=
run_pid=
run_uuid=
run_name=
unrelated_volume=
foreign_runtime_volume=
cancel_ready="/tmp/${project_name}-cancel-ready"
cancel_complete="/tmp/${project_name}-cancel-complete"
cancel_client_pid="/tmp/${project_name}-cancel-client.pid"
cancel_command="touch $cancel_ready; sleep 3; touch $cancel_complete"
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

start_foreground_run() {
  run_log=$1
  (
    cd "$workspace"
    exec "$cladding_bin" --cladding-dir "$project_root" run -v -- \
      /bin/sh -ec 'exec sleep 3600'
  ) > "$run_log" 2>&1 &
  run_pid=$!
  attempt=0
  while [ "$attempt" -lt 90 ]; do
    run_uuid=$(sed -n 's/^run: //p' "$run_log" | head -n 1)
    if [ -n "$run_uuid" ]; then
      run_name="cladding-run-$run_uuid"
      proxy="$run_name-proxy-instance"
      agent="$run_name-agent-instance"
      nw_sandbox="$run_name-nw-sandbox-instance"
      if ! "$cladding_bin" ps | grep -F "$run_uuid  ready " >/dev/null; then
        echo "cladding ps did not report run $run_uuid ready" >&2
        cat "$run_log" >&2
        return 1
      fi
      return 0
    fi
    if ! kill -0 "$run_pid" 2>/dev/null; then
      wait "$run_pid" || true
      run_pid=
      cat "$run_log" >&2
      echo "Cladding run exited before reporting readiness" >&2
      return 1
    fi
    sleep 1
    attempt=$((attempt + 1))
  done
  cat "$run_log" >&2
  echo "Cladding run did not report readiness within 90 seconds" >&2
  return 1
}

stop_foreground_run() {
  if [ -n "$run_uuid" ]; then
    "$cladding_bin" stop --run "$run_uuid"
  fi
  if [ -n "$run_pid" ]; then
    wait "$run_pid" || true
  fi
  run_pid=
  run_uuid=
  run_name=
  proxy=
  agent=
  nw_sandbox=
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
      [ -n "$container" ] || continue
      if podman inspect "$container" >/dev/null 2>&1; then
        echo "Podman state for $container:" >&2
        podman inspect --format '{{.State.Status}} exit={{.State.ExitCode}} error={{.State.Error}}' \
          "$container" >&2 || true
        podman logs "$container" 2>&1 | tail -n 30 >&2 || true
      fi
    done
    if [ -s "$temp_root/startup.log" ]; then
      startup_details=$(
        {
          grep -Ei 'error|failed|permission|denied|no such|not found|exit code|exec format|operation not permitted' \
            "$temp_root/startup.log" || true
          tail -n 10 "$temp_root/startup.log"
        } | awk '!seen[$0]++' | tail -n 30 | tr '\r\n' '  ' | sed 's/%/%25/g'
      )
      startup_details=$(printf '%s' "$startup_details" | cut -c1-4000)
      printf '::error title=Runtime topology startup details::%s\n' "$startup_details"
    fi
    if [ -n "${GITHUB_STEP_SUMMARY:-}" ]; then
      {
        echo "### Runtime topology failure diagnostics"
        echo
        printf 'Phase: `%s` (exit code %s).\n' "$phase" "$status"
        if [ -s "$temp_root/startup.log" ]; then
          echo
          echo "Cladding startup output (last 200 lines):"
          echo
          echo '```text'
          tail -n 200 "$temp_root/startup.log"
          echo '```'
        fi
        if [ -s "$temp_root/partial-startup.log" ]; then
          echo
          echo "Cladding partial-startup output (last 200 lines):"
          echo
          echo '```text'
          tail -n 200 "$temp_root/partial-startup.log"
          echo '```'
        fi
      } >> "$GITHUB_STEP_SUMMARY"
    fi
  fi
  if [ -n "$run_uuid" ]; then
    "$cladding_bin" stop --run "$run_uuid" >/dev/null 2>&1 || true
  elif [ -n "$run_pid" ]; then
    kill "$run_pid" 2>/dev/null || true
  fi
  if [ -n "$run_pid" ]; then
    wait "$run_pid" 2>/dev/null || true
  fi
  if [ -n "$foreign_runtime_volume" ]; then
    podman volume rm "$foreign_runtime_volume" >/dev/null 2>&1 || true
  fi
  if [ -n "$unrelated_volume" ]; then
    podman volume rm "$unrelated_volume" >/dev/null 2>&1 || true
  fi
  rm -rf "$temp_root"
  exit "$status"
}
trap cleanup EXIT
trap 'exit 130' INT
trap 'exit 143' TERM

mkdir -p "$workspace"
printf 'host workspace only\n' > "$workspace/.host-workspace-marker"
phase="initialize Cladding project"
(
  cd "$workspace"
  "$cladding_bin" init "$project_name" >/dev/null
)

phase="configure sandbox communication and a shared managed mount"
host_uid=$(id -u)
host_gid=$(id -g)
jq --arg image "$topology_image" \
  --arg containerfile "$script_dir/runtime-topology-image/Containerfile" \
  --arg context "$script_dir/runtime-topology-image" \
  --arg uid "$host_uid" \
  --arg gid "$host_gid" \
  --arg runtime "$runtime" \
  '.agent.image = $image
   | .agent.build = {
       containerfile: $containerfile,
       context: $context,
       args: {UID: $uid, GID: $gid}
     }
   | .nw_sandbox.enabled = true
   | .nw_sandbox.image = $image
   | .nw_sandbox.build = {
       containerfile: $containerfile,
       context: $context,
       args: {UID: $uid, GID: $gid}
     }
   | .use_runsc = ($runtime == "runsc")
   | .mounts = [{
       "mount": "/shared",
       "type": "tmpfs",
       "size": "32MiB",
       "targets": ["agent", "nw-sandbox"]
     }]' \
  "$project_root/cladding.json" > "$project_root/cladding.json.tmp"
mv "$project_root/cladding.json.tmp" "$project_root/cladding.json"
cancel_command_rego=$(jq -Rn --arg command "$cancel_command" '$command')
cat > "$project_root/config/nw_sandbox/echo.rego" <<'EOF'
package sandbox.echo

default allow = false
default allow_env = false

allow if {
  input.args == ["nw-sandbox-uds-ok"]
}

allow if {
  count(input.args) == 0
}
EOF
cat > "$project_root/config/nw_sandbox/pwd.rego" <<'EOF'
package sandbox.pwd

default allow = false
default allow_env = false

allow if {
  count(input.args) == 0
}
EOF
cat > "$project_root/config/nw_sandbox/sh.rego" <<EOF
package sandbox.sh

default allow = false
default allow_env = false

allow if {
  input.args == ["-c", "printf stdout; printf stderr >&2; exit 7"]
}

allow if {
  input.args == ["-c", $cancel_command_rego]
}
EOF
cat > "$project_root/config/nw_sandbox/env.rego" <<'EOF'
package sandbox.env

default allow = false
default allow_env = false

allow if {
  count(input.args) == 0
}

allow_env if {
  count(input.args) == 0
}
EOF
phase="build Cladding images and initialize the project CA"
podman_wrapper_dir="$temp_root/podman-wrapper"
mkdir -p "$podman_wrapper_dir"
cp "$script_dir/podman-build-timer" "$podman_wrapper_dir/podman"
chmod 0755 "$podman_wrapper_dir/podman"
real_podman=$(command -v podman)
topology_build_duration_file="$temp_root/topology-image-build-seconds"
(
  cd "$workspace"
  PATH="$podman_wrapper_dir:$PATH" \
    CLADDING_TOPOLOGY_REAL_PODMAN="$real_podman" \
    CLADDING_TOPOLOGY_IMAGE="$topology_image" \
    CLADDING_TOPOLOGY_BUILD_DURATION_FILE="$topology_build_duration_file" \
    "$cladding_bin" --cladding-dir "$project_root" build
)
topology_build_duration=$(cat "$topology_build_duration_file")
echo "Topology runtime image build duration: ${topology_build_duration}s"
if [ -n "${GITHUB_STEP_SUMMARY:-}" ]; then
  {
    echo "### Runtime topology image build"
    echo
    echo "Image: \`$topology_image\`."
    echo
    echo "Podman build duration: ${topology_build_duration}s (excludes the proxy image build and Baffle CA initialization)."
  } >> "$GITHUB_STEP_SUMMARY"
fi

phase="start Cladding topology"
start_foreground_run "$temp_root/startup.log"
cat "$temp_root/startup.log"
require_volume_count inter-container-socket 3
require_volume_count managed-mount 1

phase="verify the proxy uses managed Baffle socket volumes without a host parent bind"
if podman inspect "$proxy" | jq -e \
  '.[0].Mounts | any(.Destination == "/run/cladding/proxy" and .Type == "bind")' \
  >/dev/null; then
  echo "proxy unexpectedly has a host bind at /run/cladding/proxy" >&2
  exit 1
fi
podman inspect "$proxy" | jq -e \
  '.[0].Mounts as $mounts
   | any($mounts[]; .Type == "volume" and .Destination == "/run/cladding/proxy")
     and any($mounts[]; .Type == "volume" and .Destination == "/run/cladding/proxy/nw-sandbox")' \
  >/dev/null
agent_socket_volume=$(podman inspect "$proxy" | jq -r \
  '.[0].Mounts[] | select(.Type == "volume" and .Destination == "/run/cladding/proxy") | .Name')
if [ -z "$agent_socket_volume" ] || [ "$agent_socket_volume" = null ]; then
  echo "proxy Baffle socket root is not backed by the agent socket volume" >&2
  exit 1
fi
podman inspect "$agent" | jq -e --arg socket_volume "$agent_socket_volume" \
  '.[0].Mounts | any(.Type == "volume" and .Name == $socket_volume and .Destination == "/run/cladding/proxy")' \
  >/dev/null
podman inspect "$nw_sandbox" | jq -e --arg socket_volume "$agent_socket_volume" \
  '.[0].Mounts as $mounts
   | any($mounts[]; .Type == "volume" and .Destination == "/run/cladding/proxy/nw-sandbox")
     and all($mounts[]; .Name != $socket_volume)' \
  >/dev/null

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
  test "$(stat -c %a /run/cladding/proxy)" = 700
  test "$(stat -c %u /run/cladding/proxy)" = "$(id -u)"
  for component in agent nw-sandbox; do
    socket="/run/cladding/proxy/$component/proxy.sock"
    test -S "$socket"
    test "$(stat -c %a "$socket")" = 600
  done
'
test ! -e "$project_root/runtime/sockets"

phase="verify the default runtime does not mount the host workspace"
for container in "$agent" "$nw_sandbox"; do
  if podman inspect "$container" | jq -e \
    '.[0].Mounts | any(.Destination == "/home/user/workspace")' >/dev/null; then
    echo "$container unexpectedly mounts /home/user/workspace" >&2
    exit 1
  fi
  workdir=$(podman inspect --format '{{.Config.WorkingDir}}' "$container")
  if [ "$workdir" != /home/user ]; then
    echo "$container has an invalid default workdir: $workdir" >&2
    exit 1
  fi
done
podman exec "$agent" sh -ec \
  'test ! -e /home/user/workspace/.host-workspace-marker'
podman exec "$nw_sandbox" sh -ec \
  'test ! -e /home/user/workspace/.host-workspace-marker'

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
  'cd /home/user && run-in-nw-sandbox -- echo nw-sandbox-uds-ok')
test "$nw_output" = nw-sandbox-uds-ok
phase="verify Gantry Direct check response and policy decisions"
allowed_check=$(podman exec "$agent" sh -ec 'run-in-nw-sandbox --check -- echo')
printf '%s\n' "$allowed_check" | jq -e \
  '.allowed == true and .backend == "direct" and .filesystem == null and .baffle_session == null' \
  >/dev/null
denied_env_check=$(podman exec --env CLADDING_POLICY_TEST=forwarded "$agent" sh -ec '
  set +e
  output=$(run-in-nw-sandbox --check --keep-env=CLADDING_POLICY_TEST -- echo)
  status=$?
  set -e
  test "$status" -eq 1
  printf "%s\n" "$output"
')
printf '%s\n' "$denied_env_check" | jq -e \
  '.allowed == false and .backend == "direct" and .filesystem == null and .baffle_session == null' \
  >/dev/null
allowed_env_check=$(podman exec --env CLADDING_POLICY_TEST=forwarded "$agent" sh -ec \
  'run-in-nw-sandbox --check --keep-env=CLADDING_POLICY_TEST -- env')
printf '%s\n' "$allowed_env_check" | jq -e \
  '.allowed == true and .backend == "direct" and .filesystem == null and .baffle_session == null' \
  >/dev/null
forwarded_env=$(podman exec --env CLADDING_POLICY_TEST=forwarded "$agent" sh -ec \
  'run-in-nw-sandbox --keep-env=CLADDING_POLICY_TEST -- env')
printf '%s\n' "$forwarded_env" | grep -Fx 'CLADDING_POLICY_TEST=forwarded' >/dev/null
phase="verify missing and invalid Gantry decisions fail closed"
decision_file="$project_root/config/nw_sandbox/decision.rego"
mv "$decision_file" "$decision_file.disabled"
if missing_decision=$(podman exec "$agent" sh -ec 'run-in-nw-sandbox --check -- echo'); then
  missing_decision_status=0
else
  missing_decision_status=$?
fi
test "$missing_decision_status" -eq 1
printf '%s\n' "$missing_decision" | jq -e \
  '.allowed == false and .backend == "direct" and .filesystem == null and .baffle_session == null' \
  >/dev/null
mv "$decision_file.disabled" "$decision_file"
cp "$decision_file" "$decision_file.valid"
printf 'package sandbox.main\nthis is invalid rego\n' > "$decision_file"
if invalid_decision=$(podman exec "$agent" sh -ec 'run-in-nw-sandbox --check -- echo'); then
  invalid_decision_status=0
else
  invalid_decision_status=$?
fi
test "$invalid_decision_status" -eq 1
printf '%s\n' "$invalid_decision" | jq -e \
  '.allowed == false and .backend == "direct" and .filesystem == null and .baffle_session == null' \
  >/dev/null
mv "$decision_file.valid" "$decision_file"
phase="verify Gantry preserves working directory and streams command output and status"
cwd_output=$(podman exec "$agent" sh -ec \
  'cd /home/user && run-in-nw-sandbox -- pwd')
test "$cwd_output" = /home/user
command_output="$temp_root/gantry-command.stdout"
command_error="$temp_root/gantry-command.stderr"
set +e
podman exec "$agent" sh -ec \
  'run-in-nw-sandbox -- sh -c "printf stdout; printf stderr >&2; exit 7"' \
  >"$command_output" 2>"$command_error"
command_status=$?
set -e
test "$command_status" -eq 7
test "$(cat "$command_output")" = stdout
grep -Fx stderr "$command_error" >/dev/null
phase="verify denied Gantry command does not start"
if podman exec "$agent" sh -ec 'run-in-nw-sandbox -- touch /shared/denied-command-marker'; then
  echo "Gantry unexpectedly executed a denied command" >&2
  exit 1
fi
podman exec "$agent" sh -ec 'test ! -e /shared/denied-command-marker'
phase="verify Gantry client cancellation cleans up a wrapped workload"
podman exec "$nw_sandbox" rm -f "$cancel_ready" "$cancel_complete"
podman exec "$agent" rm -f "$cancel_client_pid"
podman exec --detach \
  --env "CLADDING_CANCEL_COMMAND=$cancel_command" \
  --env "CLADDING_CANCEL_CLIENT_PID=$cancel_client_pid" \
  "$agent" sh -ec '
    printf "%s\n" "$$" > "$CLADDING_CANCEL_CLIENT_PID"
    exec run-in-nw-sandbox -- sh -c "$CLADDING_CANCEL_COMMAND"
  '
cancel_ready_seen=false
cancel_client_pid_value=
attempt=0
while [ "$attempt" -lt 60 ]; do
  cancel_client_pid_value=$(podman exec "$agent" cat "$cancel_client_pid" 2>/dev/null || true)
  if [ -n "$cancel_client_pid_value" ] \
    && podman exec "$nw_sandbox" test -e "$cancel_ready" 2>/dev/null; then
    cancel_ready_seen=true
    break
  fi
  attempt=$((attempt + 1))
  sleep 0.1
done
if [ "$cancel_ready_seen" != true ]; then
  echo "Gantry cancellation workload did not become ready" >&2
  exit 1
fi
case "$cancel_client_pid_value" in
  ''|*[!0-9]*)
    echo "Gantry run-remote client recorded an invalid PID: $cancel_client_pid_value" >&2
    exit 1
    ;;
esac
podman exec "$agent" sh -ec 'kill -TERM "$1"' _ "$cancel_client_pid_value"
sleep 4
if podman exec "$nw_sandbox" test -e "$cancel_complete"; then
  echo "Gantry kept the remote workload running after its UDS client disconnected" >&2
  exit 1
fi
phase="verify shared managed mount communication"
podman exec "$agent" sh -ec 'printf "agent-to-sandbox-ok\n" > /shared/topology-marker'
test "$(podman exec "$nw_sandbox" cat /shared/topology-marker)" = agent-to-sandbox-ok

# Stopping the foreground run's agent ends the run, so validate expose against a
# fresh run after this inject lifecycle check.
phase="validate cladding inject transport and lifecycle"
python3 "$script_dir/validate_inject.py" \
  --cladding-bin "$cladding_bin" \
  --run "$run_uuid" \
  --agent "$agent"

attempt=0
while kill -0 "$run_pid" 2>/dev/null && [ "$attempt" -lt 120 ]; do
  sleep 0.1
  attempt=$((attempt + 1))
done
if kill -0 "$run_pid" 2>/dev/null; then
  echo "foreground run owner did not exit after its agent stopped" >&2
  exit 1
fi
wait "$run_pid" 2>/dev/null || true
run_pid=
run_uuid=
run_name=
proxy=
agent=
nw_sandbox=
start_foreground_run "$temp_root/post-inject-startup.log"

phase="prepare unrelated and foreign run volumes before selected stop"
unrelated_volume="cladding-unrelated-$project_name"
foreign_runtime_uuid="aaaaaaaa-1111-4222-8333-000000000001"
foreign_runtime_volume="cladding-mount-foreign-$project_name"
podman volume create "$unrelated_volume" >/dev/null
podman volume create --opt nocopy \
  --label "cladding=cladding-run-$foreign_runtime_uuid" \
  --label "project_root=$project_root" \
  --label cladding_resource=managed-mount \
  --label "cladding_runtime_root=$project_root/runtime/runs/cladding-run-$foreign_runtime_uuid" \
  --label "cladding_mount_volume=$foreign_runtime_volume" \
  --label "cladding_run_control_socket=/tmp/cladding-$foreign_runtime_uuid.sock" \
  --label cladding_run_expected_roles=agent,proxy \
  --label cladding_run_metadata_version=1 \
  "$foreign_runtime_volume" >/dev/null

phase="validate expose forwarding, explicit run selection, and shutdown"
python3 "$script_dir/validate_expose.py" \
  --cladding-bin "$cladding_bin" \
  --cladding-dir "$project_root" \
  --run "$run_uuid" \
  --stop-run \
  --agent "$agent"
wait "$run_pid" || true
run_pid=
run_uuid=
run_name=
proxy=
agent=
nw_sandbox=
if ! podman volume exists "$unrelated_volume"; then
  echo "stopping the selected run removed an unrelated user volume" >&2
  exit 1
fi
if ! podman volume exists "$foreign_runtime_volume"; then
  echo "stopping the selected run removed a foreign run volume" >&2
  exit 1
fi
remaining_managed=$(volume_names managed-mount)
if [ "$remaining_managed" != "$foreign_runtime_volume" ]; then
  echo "stopping the selected run removed the wrong managed volumes: $remaining_managed" >&2
  exit 1
fi
podman volume rm "$foreign_runtime_volume" "$unrelated_volume" >/dev/null
foreign_runtime_volume=
unrelated_volume=
require_no_project_resources

test ! -e "$project_root/runtime/sockets"

phase="configure and verify an explicit host workspace mount"
jq '.mounts += [{"mount":"/host-project", "hostPath":"..", "targets":["agent", "nw-sandbox"]}]' \
  "$project_root/cladding.json" > "$project_root/cladding.json.tmp"
mv "$project_root/cladding.json.tmp" "$project_root/cladding.json"
start_foreground_run "$temp_root/explicit-mount-startup.log"
cat "$temp_root/explicit-mount-startup.log"
for container in "$agent" "$nw_sandbox"; do
  if ! podman inspect "$container" | jq -e \
    '.[0].Mounts | any(.Destination == "/host-project")' >/dev/null; then
    echo "$container does not have the configured /host-project mount" >&2
    exit 1
  fi
  if ! podman inspect "$container" | jq -e \
    '.[0].Mounts | any(.Destination == "/host-project/.cladding")' >/dev/null; then
    echo "$container does not mask private .cladding state below /host-project" >&2
    exit 1
  fi
done
for container in "$agent" "$nw_sandbox"; do
  podman exec "$container" sh -ec \
    'test -f /host-project/.host-workspace-marker
     test ! -e /host-project/.cladding/config/proxy/daemon.toml'
done
for target in agent nw-sandbox; do
  config_backup="$temp_root/cladding.json.$target"
  cp "$project_root/cladding.json" "$config_backup"
  printf 'not valid JSON\n' > "$project_root/cladding.json"
  (cd "$workspace" && "$cladding_bin" exec --run "$run_uuid" \
    --target "$target" sh -ec 'test "$PWD" = /host-project')
  mv "$config_backup" "$project_root/cladding.json"
done
phase="stop the explicit-mount run and verify cleanup"
stop_foreground_run
require_no_project_resources

phase="run a one-off command with shared project-state bind sources"
run_podman_wrapper="$temp_root/run-podman-wrapper"
mkdir -p "$run_podman_wrapper"
cat > "$run_podman_wrapper/podman" <<'EOF'
#!/bin/sh
printf '%s\n' "$*" >> "$CLADDING_TOPOLOGY_PODMAN_LOG"
exec "$CLADDING_TOPOLOGY_REAL_PODMAN" "$@"
EOF
chmod 0755 "$run_podman_wrapper/podman"
real_podman=$(command -v podman)
run_log="$temp_root/one-off-run.log"
run_podman_log="$temp_root/one-off-podman.log"
run_status=0
(
  cd "$workspace"
  CLADDING_TOPOLOGY_PODMAN_LOG="$run_podman_log" \
    CLADDING_TOPOLOGY_REAL_PODMAN="$real_podman" \
    PATH="$run_podman_wrapper:$PATH" \
    "$cladding_bin" --cladding-dir "$project_root" run -v -- \
      /bin/sh -ec 'test "$PWD" = /host-project
                   test -f /host-project/.host-workspace-marker
                   test ! -e /host-project/.cladding/config/proxy/daemon.toml
                   echo ok'
) > "$run_log" 2>&1 || run_status=$?
cat "$run_log"
if [ "$run_status" -ne 0 ] || ! grep -Fx ok "$run_log" >/dev/null; then
  echo "cladding run did not complete the explicit-mount workdir check" >&2
  exit 1
fi
run_name=$(sed -n 's/^starting one-off instance: //p' "$run_log" | head -n 1)
if [ -z "$run_name" ]; then
  echo "cladding run did not report its one-off instance name" >&2
  exit 1
fi
run_root="$project_root/runtime/runs/$run_name"
for component in proxy agent nw-sandbox; do
  if ! grep -F "$run_name-$component-instance" "$run_podman_log" >/dev/null; then
    echo "cladding run did not start the $component container" >&2
    exit 1
  fi
done
for source in \
  "$run_root/runtime/scripts/proxy_startup.sh" \
  "$run_root/runtime/empty-mask"; do
  if ! grep -F "$source" "$run_podman_log" >/dev/null; then
    echo "cladding run did not pass the generated bind source to Podman: $source" >&2
    exit 1
  fi
done
if [ -e "$run_root/runtime/sockets" ] \
  || grep -F "$run_root/runtime/sockets" "$run_podman_log" >/dev/null; then
  echo "cladding run created or mounted a host socket directory" >&2
  exit 1
fi
if ! grep -F "$project_root/..:/host-project" "$run_podman_log" >/dev/null; then
  echo "cladding run did not apply the configured host workspace mount" >&2
  exit 1
fi
if grep -F ":/home/user/workspace" "$run_podman_log" >/dev/null; then
  echo "cladding run added an implicit host workspace mount" >&2
  exit 1
fi
for temp_run_root in "/tmp/$run_name" "/var/tmp/$run_name" "/dev/shm/$run_name"; do
  if grep -F "$temp_run_root" "$run_podman_log" >/dev/null; then
    echo "cladding run passed a host temporary path to Podman: $temp_run_root" >&2
    exit 1
  fi
done
if [ -e "$run_root" ]; then
  echo "cladding run left its private runtime root behind: $run_root" >&2
  exit 1
fi
require_no_project_resources

phase="fail a one-off startup after the agent starts and clean owned resources"
jq '.nw_sandbox.security_opts = ["cladding-invalid-security-option"]' \
  "$project_root/cladding.json" > "$project_root/cladding.json.tmp"
mv "$project_root/cladding.json.tmp" "$project_root/cladding.json"
failed_run_log="$temp_root/one-off-startup-failure.log"
failed_run_status=0
(cd "$workspace" && "$cladding_bin" --cladding-dir "$project_root" run -v -- /bin/echo should-not-run) \
  > "$failed_run_log" 2>&1 || failed_run_status=$?
if [ "$failed_run_status" -eq 0 ]; then
  echo "cladding run accepted the deliberate network-sandbox startup failure" >&2
  exit 1
fi
failed_run_name=$(sed -n 's/^starting one-off instance: //p' "$failed_run_log" | head -n 1)
if [ -z "$failed_run_name" ] || ! grep -F "$failed_run_name-nw-sandbox-instance" "$failed_run_log" >/dev/null; then
  echo "cladding run failed before attempting the network-sandbox container" >&2
  cat "$failed_run_log" >&2
  exit 1
fi
if [ -e "$project_root/runtime/runs/$failed_run_name" ]; then
  echo "failed cladding run left its private runtime root behind" >&2
  exit 1
fi
require_no_project_resources

phase="fail after partial run startup and clean owned resources"
startup_status=0
(cd "$workspace" && "$cladding_bin" --cladding-dir "$project_root" run -v -- /bin/echo should-not-run) \
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
