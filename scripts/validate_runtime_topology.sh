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

for tool in podman jq python3; do
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
  "$cladding_bin" --cladding-dir "$project_root" down >/dev/null 2>&1 || true
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
cat >> "$project_root/config/nw_sandbox/main.rego" <<'EOF'

allow if {
  input.command == "/bin/echo"
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
  'cd /home/user && run-in-nw-sandbox -- /bin/echo nw-sandbox-uds-ok')
test "$nw_output" = nw-sandbox-uds-ok
phase="verify shared managed mount communication"
podman exec "$agent" sh -ec 'printf "agent-to-sandbox-ok\n" > /shared/topology-marker'
test "$(podman exec "$nw_sandbox" cat /shared/topology-marker)" = agent-to-sandbox-ok

# The inject lifecycle check stops and restarts the agent. Run it after the
# topology checks that validate mounts shared with the agent.
phase="validate cladding inject transport and lifecycle"
python3 "$script_dir/validate_inject.py" \
  --cladding-bin "$cladding_bin" \
  --cladding-dir "$project_root" \
  --agent "$agent"

phase="stop Cladding runtime and clean owned resources"
(cd "$workspace" && "$cladding_bin" --cladding-dir "$project_root" down)
require_no_project_resources
test ! -S "$project_root/runtime/sockets/proxy/agent/proxy.sock"
test ! -S "$project_root/runtime/sockets/proxy/nw-sandbox/proxy.sock"

phase="configure and verify an explicit host workspace mount"
jq '.mounts += [{"mount":"/host-project", "hostPath":"..", "targets":["agent", "nw-sandbox"]}]' \
  "$project_root/cladding.json" > "$project_root/cladding.json.tmp"
mv "$project_root/cladding.json.tmp" "$project_root/cladding.json"
if (cd "$workspace" && "$cladding_bin" --cladding-dir "$project_root" up --verbose) \
  > "$temp_root/startup.log" 2>&1; then
  cat "$temp_root/startup.log"
else
  status=$?
  cat "$temp_root/startup.log" >&2
  exit "$status"
fi
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
  (cd "$workspace" && "$cladding_bin" --cladding-dir "$project_root" \
    exec --target "$target" sh -ec 'test "$PWD" = /host-project')
done
(cd "$workspace" && "$cladding_bin" --cladding-dir "$project_root" down)
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
  "$run_root/runtime/empty-mask" \
  "$run_root/runtime/sockets/proxy"; do
  if ! grep -F "$source" "$run_podman_log" >/dev/null; then
    echo "cladding run did not pass the generated bind source to Podman: $source" >&2
    exit 1
  fi
done
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

phase="fail after partial startup and clean owned resources"
# The network sandbox starts after the proxy and agent, so this runtime option
# fails after Cladding has created several owned resources.
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
