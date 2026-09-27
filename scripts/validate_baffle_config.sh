#!/bin/sh
set -eu

if [ "$#" -ne 2 ]; then
  echo "usage: $0 <cladding-binary> <baffle-binary>" >&2
  exit 2
fi

cladding_bin=$1
baffle_bin=$2
temp_root=$(mktemp -d)
container_name="cladding-baffle-config-$$"
trap 'podman rm -f "$container_name" >/dev/null 2>&1 || true; rm -rf "$temp_root"' EXIT

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
socket_dir="$project_root/runtime/sockets/proxy"
control_dir="$temp_root/control"
mkdir -p "$socket_dir" "$control_dir"
chmod 700 "$control_dir"

podman run --pull=always --detach --name "$container_name" \
  --volume "$baffle_bin:/usr/local/bin/baffle:ro" \
  --volume "$project_root/config:/opt/config:ro" \
  --volume "$project_root/credentials/baffle:/opt/credentials/baffle:ro" \
  --volume "$socket_dir:/run/cladding/proxy:rw" \
  --volume "$control_dir:/run/baffle:rw" \
  --entrypoint /bin/sh \
  docker.io/library/ubuntu:24.04 \
  -ec 'apt-get update; DEBIAN_FRONTEND=noninteractive apt-get install -y --no-install-recommends ca-certificates libssl3t64; rm -rf /var/lib/apt/lists/*; exec baffle daemon --config /opt/config/proxy/daemon.toml' >/dev/null

ready=false
for _ in $(seq 1 60); do
  if podman exec "$container_name" baffle list >/dev/null 2>&1; then
    ready=true
    break
  fi
  if [ "$(podman inspect --format '{{.State.Running}}' "$container_name")" != true ]; then
    podman logs "$container_name" >&2
    exit 1
  fi
  sleep 1
done
if [ "$ready" != true ]; then
  podman logs "$container_name" >&2
  echo "Baffle daemon did not accept control commands" >&2
  exit 1
fi

podman exec "$container_name" baffle create agent.toml
podman exec "$container_name" baffle create nw-sandbox.toml
test -S "$socket_dir/agent/proxy.sock"
test -S "$socket_dir/nw-sandbox/proxy.sock"
echo "Baffle file-only configuration validation passed under rootless Podman"
