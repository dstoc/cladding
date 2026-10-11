#!/bin/sh
set -eu

if [ "$#" -ne 5 ]; then
  echo "usage: $0 <image> <host-workspace> <vm-workspace> <host-shared-root> <machine-ssh>" >&2
  exit 2
fi

image=$1
host_workspace=$2
vm_workspace=$3
host_shared_root=$4
machine_ssh=$5
report_root=${RUNNER_TEMP:-${TMPDIR:-/tmp}}/cladding-machine-bind-repro.$$
report="$report_root.log"

cleanup() {
  rm -f "$report" "$report_root".*
}
trap cleanup EXIT
trap 'exit 129' HUP
trap 'exit 130' INT
trap 'exit 143' TERM

mkdir -p "$host_workspace"

run_probe() {
  name=$1
  userns=$2
  source_path=$3
  output="$report_root.$name"

  if [ "$userns" = keep-id ]; then
    if podman run --rm --network none --userns keep-id \
      --volume "$source_path:/home/user/workspace" \
      --workdir /home/user/workspace "$image" \
      /bin/sh -ec 'id; stat -c "%u:%g %a %n" /home/user /home/user/workspace; touch /home/user/workspace/.cladding-bind-probe; rm /home/user/workspace/.cladding-bind-probe' \
      > "$output" 2>&1; then
      status=0
    else
      status=$?
    fi
  else
    if podman run --rm --network none \
      --volume "$source_path:/home/user/workspace" \
      --workdir /home/user/workspace "$image" \
      /bin/sh -ec 'id; stat -c "%u:%g %a %n" /home/user /home/user/workspace; touch /home/user/workspace/.cladding-bind-probe; rm /home/user/workspace/.cladding-bind-probe' \
      > "$output" 2>&1; then
      status=0
    else
      status=$?
    fi
  fi

  printf 'Probe %s (userns=%s, source=%s): exit=%s\n' \
    "$name" "$userns" "$source_path" "$status"
  cat "$output"
  rm -f "$output"
}

{
  echo "Podman Machine bind mount reproduction"
  echo "Image: $image"
  podman image inspect --format 'Image user={{.Config.User}} workdir={{.Config.WorkingDir}}' "$image"
  echo
  echo "GitHub host identity and path ownership:"
  id
  stat -c '%u:%g %a %n' "$host_shared_root" "$host_workspace"
  echo
  echo "Podman Machine guest identity and path ownership:"
  "$machine_ssh" sh -lc \
    "id; stat -c '%u:%g %a %n' '$host_shared_root' '$host_workspace' '$vm_workspace' 2>&1 || true"
  echo
  echo "Four mount probes:"
  run_probe A keep-id "$host_workspace"
  run_probe B default "$host_workspace"
  run_probe C keep-id "$vm_workspace"
  run_probe D default "$vm_workspace"
} > "$report" 2>&1

cat "$report"
if [ -n "${GITHUB_ACTIONS:-}" ]; then
  annotation_report=$(tr '\r\n' '  ' < "$report" | sed 's/%/%25/g')
  printf '::notice title=Podman Machine bind diagnostics::%s\n' "$annotation_report"
fi
if [ -n "${GITHUB_STEP_SUMMARY:-}" ]; then
  {
    echo "### Podman Machine bind mount reproduction"
    echo
    echo '```text'
    cat "$report"
    echo '```'
  } >> "$GITHUB_STEP_SUMMARY"
fi
