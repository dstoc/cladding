#!/bin/sh
set -eu

if [ "$#" -ne 2 ]; then
  echo "usage: $0 <cladding-binary> <baffle-binary>" >&2
  exit 2
fi

cladding_bin=$1
baffle_bin=$2
temp_root=$(mktemp -d)
container_prefix="cladding-baffle-startup-$$"
disabled_container="${container_prefix}-disabled"
enabled_container="${container_prefix}-enabled"
trap 'podman rm -f "$disabled_container" "$enabled_container" >/dev/null 2>&1 || true; rm -rf "$temp_root"' EXIT

rootless=$(podman info --format '{{.Host.Security.Rootless}}')
if [ "$rootless" != true ]; then
  echo "Baffle config validation requires rootless Podman" >&2
  exit 1
fi

mkdir "$temp_root/workspace"
(
  cd "$temp_root/workspace"
  "$cladding_bin" init bafflevalidation >/dev/null
)
project_root="$temp_root/workspace/.cladding"
jq '.agent.image = "docker.io/library/debian:trixie-slim" | .nw_sandbox.image = "docker.io/library/debian:trixie-slim"' \
  "$project_root/cladding.json" > "$project_root/cladding.json.tmp"
mv "$project_root/cladding.json.tmp" "$project_root/cladding.json"
"$cladding_bin" --cladding-dir "$project_root" build
cmp "$project_root/tools/bin/baffle" "$baffle_bin"

run_proxy_startup() {
  name=$1
  sandbox_enabled=$2
  sandbox_state=$3
  socket_dir="$temp_root/sockets-$name"
  mkdir -p "$socket_dir/agent"
  if [ "$sandbox_enabled" = true ]; then
    mkdir -p "$socket_dir/nw-sandbox"
  fi
  chmod 700 "$socket_dir" "$socket_dir/agent"
  if [ "$sandbox_enabled" = true ]; then
    chmod 700 "$socket_dir/nw-sandbox"
  fi

  podman run --detach --name "$name" \
    --env "CLADDING_NW_SANDBOX_ENABLED=$sandbox_enabled" \
    --volume "$project_root/tools/bin/baffle:/opt/tools/bin/baffle:ro" \
    --volume "$project_root/runtime/scripts/proxy_startup.sh:/opt/scripts/proxy_startup.sh:ro" \
    --volume "$project_root/config:/opt/config:ro" \
    --volume "$project_root/credentials/baffle:/opt/credentials/baffle:ro" \
    --volume "$socket_dir:/run/cladding/proxy:rw" \
    --entrypoint /bin/sh \
    localhost/cladding-proxy:latest /opt/scripts/proxy_startup.sh >/dev/null

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
      podman logs "$name" >&2
      exit 1
    fi
    attempt=$((attempt + 1))
    sleep 1
  done
  if [ "$ready" != true ]; then
    podman logs "$name" >&2
    echo "Baffle startup did not create the expected $sandbox_state session socket" >&2
    exit 1
  fi

  if [ "$sandbox_enabled" = false ] && [ -S "$socket_dir/nw-sandbox/proxy.sock" ]; then
    echo "Baffle startup created a network-sandbox session while it was disabled" >&2
    exit 1
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
