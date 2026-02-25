#!/bin/sh
set -e

resolve_script_path() {
  case "$1" in
    */*) printf '%s\n' "$1" ;;
    *) command -v -- "$1" ;;
  esac
}

SCRIPT_PATH="$(resolve_script_path "$0")"
if command -v readlink >/dev/null 2>&1; then
  SCRIPT_PATH="$(readlink -f "$SCRIPT_PATH" 2>/dev/null || printf '%s' "$SCRIPT_PATH")"
fi
case "$SCRIPT_PATH" in
  /*) ;;
  *) SCRIPT_PATH="$(pwd)/$SCRIPT_PATH" ;;
esac
CLADDING_ROOT="$(CDPATH= cd -- "$(dirname -- "$SCRIPT_PATH")" && pwd)"
DEFAULT_CLADDING_BUILD_IMAGE="localhost/cladding-default:latest"
DEFAULT_CLI_BUILD_IMAGE="$DEFAULT_CLADDING_BUILD_IMAGE"
DEFAULT_SANDBOX_BUILD_IMAGE="$DEFAULT_CLADDING_BUILD_IMAGE"

print_help() {
  cat <<'EOF'
Usage: cladding <command> [args...]

Commands:
  build                Build local container images
  init [name]          Create config and default mount directories
  check                Check requirements
  up                   Start the system
  down                 Stop the system
  destroy              Force-remove running containers
  run                  Run a command in the cli container
  reload-proxy         Reload the squid proxy configuration
  help                 Show this help
EOF
}

find_project_root() {
  search_dir="$PWD"

  while :; do
    if [ -d "$search_dir/.cladding" ]; then
      printf '%s\n' "$search_dir/.cladding"
      return 0
    fi

    if [ "$search_dir" = "/" ]; then
      return 1
    fi

    search_dir="$(dirname -- "$search_dir")"
  done
}

cmd_build() {
  load_cladding_config

  HOST_UID="$(id -u)"
  HOST_GID="$(id -g)"
  TOOLS_DIR="$PROJECT_ROOT/tools"
  TOOLS_BIN_DIR="$TOOLS_DIR/bin"

  if [ -L "$TOOLS_DIR" ] && [ ! -e "$TOOLS_DIR" ]; then
    echo "missing: tools (broken symlink at $TOOLS_DIR)" >&2
    echo "hint: create or relink $TOOLS_DIR" >&2
    return 1
  fi

  mkdir -p "$TOOLS_BIN_DIR"

  podman run --rm \
    -e CARGO_TARGET_DIR=/work/mcp-run/target \
    -v "$CLADDING_ROOT/crates/mcp-run:/work/mcp-run" \
    -w /work/mcp-run \
    docker.io/library/rust:latest \
    cargo build --manifest-path /work/mcp-run/Cargo.toml --release --locked --bin mcp-run --bin run-remote

  install -m 0755 "$CLADDING_ROOT/crates/mcp-run/target/release/mcp-run" "$TOOLS_BIN_DIR/mcp-run"
  install -m 0755 "$CLADDING_ROOT/crates/mcp-run/target/release/run-remote" "$TOOLS_BIN_DIR/run-with-network"

  cli_image_built=0
  if [ "$CLI_IMAGE" = "$DEFAULT_CLI_BUILD_IMAGE" ]; then
    podman build \
      --build-arg UID="$HOST_UID" \
      --build-arg GID="$HOST_GID" \
      -t "$CLI_IMAGE" \
      -f "$CLADDING_ROOT/Containerfile.cladding" \
      "$CLADDING_ROOT"
    cli_image_built=1
  else
    echo "skip: not building cli image (config cli_image is $CLI_IMAGE, build target is $DEFAULT_CLADDING_BUILD_IMAGE)"
  fi

  if [ "$SANDBOX_IMAGE" = "$DEFAULT_SANDBOX_BUILD_IMAGE" ]; then
    if [ "$SANDBOX_IMAGE" = "$CLI_IMAGE" ] && [ "$cli_image_built" -eq 1 ]; then
      echo "skip: sandbox image already built (config cli_image and sandbox_image are both $SANDBOX_IMAGE)"
    else
      podman build \
        --build-arg UID="$HOST_UID" \
        --build-arg GID="$HOST_GID" \
        -t "$SANDBOX_IMAGE" \
        -f "$CLADDING_ROOT/Containerfile.cladding" \
        "$CLADDING_ROOT"
    fi
  else
    echo "skip: not building sandbox image (config sandbox_image is $SANDBOX_IMAGE, build target is $DEFAULT_SANDBOX_BUILD_IMAGE)"
  fi
}

image_is_buildable_by_cladding() {
  case "$1" in
    "$DEFAULT_CLADDING_BUILD_IMAGE")
      return 0
      ;;
    *)
      return 1
      ;;
  esac
}

check_required_paths() {
  missing=0

  for name in config home tools; do
    path="$PROJECT_ROOT/$name"

    if [ -L "$path" ]; then
      if [ ! -e "$path" ]; then
        echo "missing: $name (broken symlink at $path)" >&2
        if [ "$name" = "config" ]; then
          echo "hint: run cladding init" >&2
        else
          echo "hint: create or relink $path" >&2
        fi
        missing=1
      fi
      continue
    fi

    if [ ! -e "$path" ]; then
      echo "missing: $name ($path)" >&2
      if [ "$name" = "config" ]; then
        echo "hint: run cladding init" >&2
      else
        echo "hint: mkdir -p $path (or symlink it)" >&2
      fi
      missing=1
    fi
  done

  if [ "$missing" -ne 0 ]; then
    return 1
  fi

  check_required_config_files
}

check_required_config_files() {
  src="$CLADDING_ROOT/config-template"
  dst="$PROJECT_ROOT/config"
  missing=0

  if [ ! -d "$src" ]; then
    echo "missing: config-template ($src)" >&2
    return 1
  fi

  for rel in $(cd "$src" && find . -mindepth 1 -maxdepth 1 -print | sed 's|^\./||'); do
    if [ ! -e "$dst/$rel" ]; then
      echo "missing: config/$rel ($dst/$rel)" >&2
      missing=1
    fi
  done

  if [ "$missing" -ne 0 ]; then
    echo "hint: run cladding init, or add missing top-level entries from $src into $dst" >&2
    return 1
  fi
}

check_required_binaries() {
  missing=0

  for name in mcp-run run-with-network; do
    path="$PROJECT_ROOT/tools/bin/$name"

    if [ ! -x "$path" ]; then
      echo "missing: tools/bin/$name ($path)" >&2
      echo "hint: run cladding build" >&2
      missing=1
    fi
  done

  if [ "$missing" -ne 0 ]; then
    return 1
  fi
}

check_required_images() {
  missing=0

  for image in "$CLI_IMAGE" "$SANDBOX_IMAGE"; do
    if ! podman image exists "$image"; then
      echo "missing: image $image" >&2
      if image_is_buildable_by_cladding "$image"; then
        echo "hint: run cladding build" >&2
      else
        echo "hint: pull/tag image '$image', or set cladding.json image to a supported build target and run cladding build" >&2
      fi
      missing=1
    fi
  done

  if [ "$missing" -ne 0 ]; then
    return 1
  fi
}

ipv4_to_int() {
  ip="$1"
  old_ifs="$IFS"
  IFS=.
  set -- $ip
  IFS="$old_ifs"

  if [ $# -ne 4 ]; then
    return 1
  fi

  for octet in "$1" "$2" "$3" "$4"; do
    case "$octet" in
      ''|*[!0-9]*)
        return 1
        ;;
    esac
    if [ "$octet" -lt 0 ] || [ "$octet" -gt 255 ]; then
      return 1
    fi
  done

  printf '%s\n' "$((($1 * 16777216) + ($2 * 65536) + ($3 * 256) + $4))"
}

int_to_ipv4() {
  int="$1"
  octet1=$((int / 16777216))
  int=$((int % 16777216))
  octet2=$((int / 65536))
  int=$((int % 65536))
  octet3=$((int / 256))
  octet4=$((int % 256))
  printf '%s.%s.%s.%s\n' "$octet1" "$octet2" "$octet3" "$octet4"
}

load_cladding_config() {
  if [ "${CLADDING_CONFIG_LOADED:-0}" -eq 1 ]; then
    return 0
  fi

  CONFIG_PATH="$PROJECT_ROOT/cladding.json"

  if ! command -v jq >/dev/null 2>&1; then
    echo "missing: jq (required to parse $CONFIG_PATH)" >&2
    return 1
  fi

  if [ ! -f "$CONFIG_PATH" ]; then
    echo "missing: cladding.json ($CONFIG_PATH)" >&2
    echo "hint: run cladding init" >&2
    return 1
  fi

  if ! CLADDING_NAME="$(jq -er '.name | strings' "$CONFIG_PATH" 2>/dev/null)"; then
    echo "error: cladding.json must include string key: name" >&2
    return 1
  fi
  if ! CLADDING_SUBNET="$(jq -er '.subnet | strings' "$CONFIG_PATH" 2>/dev/null)"; then
    echo "error: cladding.json must include string key: subnet" >&2
    return 1
  fi
  if ! SANDBOX_IMAGE="$(jq -er '.sandbox_image | strings' "$CONFIG_PATH" 2>/dev/null)"; then
    echo "error: cladding.json must include string key: sandbox_image" >&2
    return 1
  fi
  if ! CLI_IMAGE="$(jq -er '.cli_image | strings' "$CONFIG_PATH" 2>/dev/null)"; then
    echo "error: cladding.json must include string key: cli_image" >&2
    return 1
  fi

  case "$CLADDING_NAME" in
    ''|*[!a-z0-9]*)
      echo "error: config key 'name' must be lowercase alphanumeric ([a-z0-9]+)" >&2
      return 1
      ;;
  esac

  CLADDING_CONFIG_LOADED=1
}

derive_cladding_name_from_pwd() {
  raw_name="$(basename -- "$PWD")"
  name="$(printf '%s' "$raw_name" | tr '[:upper:]' '[:lower:]' | tr -cd 'a-z0-9')"

  if [ -z "$name" ]; then
    echo "error: could not derive an alphanumeric name from directory: $PWD" >&2
    return 1
  fi

  printf '%s\n' "$name"
}

normalize_cladding_name_arg() {
  name_arg="$1"
  name="$(printf '%s' "$name_arg" | tr '[:upper:]' '[:lower:]')"

  case "$name" in
    ''|*[!a-z0-9]*)
      echo "error: init name must be alphanumeric ([a-zA-Z0-9]+)" >&2
      return 1
      ;;
  esac

  printf '%s\n' "$name"
}

list_podman_ipv4_subnets() {
  if ! network_names="$(podman network ls --format '{{.Name}}' 2>/dev/null)"; then
    return 1
  fi

  for net_name in $network_names; do
    podman network inspect -f '{{range .Subnets}}{{.Subnet}}{{"\n"}}{{end}}' "$net_name" 2>/dev/null || true
  done | awk '/^[0-9]+\.[0-9]+\.[0-9]+\.[0-9]+\/[0-9]+$/'
}

pick_available_subnet() {
  if ! used_subnets="$(list_podman_ipv4_subnets)"; then
    return 1
  fi
  i=0
  while [ "$i" -le 255 ]; do
    candidate="10.90.$i.0/24"
    if ! printf '%s\n' "$used_subnets" | grep -Fxq "$candidate"; then
      printf '%s\n' "$candidate"
      return 0
    fi
    i=$((i + 1))
  done

  return 2
}

write_default_cladding_config() {
  config_path="$1"
  name_override="${2:-}"

  if ! command -v podman >/dev/null 2>&1; then
    echo "missing: podman (required for cladding init to choose name/subnet)" >&2
    return 1
  fi

  if [ -n "$name_override" ]; then
    if ! name="$(normalize_cladding_name_arg "$name_override")"; then
      return 1
    fi
  else
    if ! name="$(derive_cladding_name_from_pwd)"; then
      return 1
    fi
  fi

  network_name="${name}_cladding_net"
  network_exists_rc=0
  podman network exists "$network_name" >/dev/null 2>&1 || network_exists_rc=$?
  case "$network_exists_rc" in
    0)
      echo "error: network already exists for generated name: $network_name" >&2
      echo "hint: run cladding init from a different directory name, or remove the existing network" >&2
      return 1
      ;;
    1)
      ;;
    *)
      echo "error: failed to check existing networks via podman" >&2
      return 1
      ;;
  esac

  subnet_rc=0
  subnet="$(pick_available_subnet)" || subnet_rc=$?
  case "$subnet_rc" in
    0)
      ;;
    1)
      echo "error: failed to inspect existing network subnets via podman" >&2
      return 1
      ;;
    2)
      echo "error: could not find an unused subnet in 10.90.0.0/16 (/24 slices)" >&2
      return 1
      ;;
    *)
      echo "error: unexpected failure while selecting subnet" >&2
      return 1
      ;;
  esac

  cat > "$config_path" <<EOF
{
  "sandbox_image": "$DEFAULT_SANDBOX_BUILD_IMAGE",
  "cli_image": "$DEFAULT_CLI_BUILD_IMAGE",
  "name": "$name",
  "subnet": "$subnet"
}
EOF
}

ensure_network_settings() {
  if ! command -v podman >/dev/null 2>&1; then
    echo "missing: podman" >&2
    return 1
  fi

  if ! podman network exists "$NETWORK"; then
    podman network create --subnet "$NETWORK_SUBNET" "$NETWORK"
  elif ! podman network inspect "$NETWORK" | grep -q "\"subnet\": \"$NETWORK_SUBNET\""; then
    echo "error: network $NETWORK exists but is not on $NETWORK_SUBNET" >&2
    echo "hint: update cladding.json subnet to match, or run 'podman network rm $NETWORK' and retry" >&2
    return 1
  fi
}

cmd_init() {
  if [ "$#" -gt 1 ]; then
    echo "usage: cladding init [name]" >&2
    return 1
  fi

  name_override="${1:-}"
  src="$CLADDING_ROOT/config-template"
  dst="$PROJECT_ROOT/config"
  cladding_config="$PROJECT_ROOT/cladding.json"
  cladding_gitignore="$PROJECT_ROOT/.gitignore"
  generated_config_tmp=""
  project_root_created=0

  if [ -e "$PROJECT_ROOT" ] && [ ! -d "$PROJECT_ROOT" ]; then
    echo "error: .cladding path exists but is not a directory: $PROJECT_ROOT" >&2
    return 1
  fi

  if [ ! -d "$src" ]; then
    echo "missing: config-template ($src)" >&2
    return 1
  fi

  if [ ! -e "$cladding_config" ]; then
    if ! generated_config_tmp="$(mktemp "${TMPDIR:-/tmp}/cladding-config-XXXXXX")"; then
      echo "error: failed to allocate temporary file for generated cladding config" >&2
      return 1
    fi

    if ! write_default_cladding_config "$generated_config_tmp" "$name_override"; then
      rm -f "$generated_config_tmp"
      return 1
    fi
  fi

  if [ ! -e "$PROJECT_ROOT" ]; then
    project_root_created=1
  fi
  mkdir -p "$PROJECT_ROOT"
  if [ "$project_root_created" -eq 1 ]; then
    printf '*\n' > "$cladding_gitignore"
  fi

  if [ -e "$dst" ] || [ -L "$dst" ]; then
    echo "config already exists: $dst"
  else
    cp -a "$src" "$dst"
    echo "initialized: $dst"
  fi

  if [ -e "$cladding_config" ]; then
    echo "cladding config already exists: $cladding_config"
  else
    install -m 0644 "$generated_config_tmp" "$cladding_config"
    rm -f "$generated_config_tmp"
    echo "generated: $cladding_config"
  fi

  resolve_network_settings
  ensure_network_settings
}

cmd_check() {
  check_required_paths
  check_required_binaries
  resolve_network_settings
  check_required_images
  echo "check: ok"
}

resolve_network_settings() {
  load_cladding_config

  subnet_ip="${CLADDING_SUBNET%/*}"
  subnet_prefix="${CLADDING_SUBNET#*/}"

  if [ "$subnet_ip" = "$CLADDING_SUBNET" ] || [ -z "$subnet_ip" ] || [ -z "$subnet_prefix" ]; then
    echo "error: config key 'subnet' must be in CIDR notation (example: 10.90.0.0/24)" >&2
    return 1
  fi
  case "$subnet_prefix" in
    ''|*[!0-9]*)
      echo "error: subnet prefix must be numeric: $CLADDING_SUBNET" >&2
      return 1
      ;;
  esac
  if [ "$subnet_prefix" -lt 0 ] || [ "$subnet_prefix" -gt 32 ]; then
    echo "error: subnet prefix out of range (0-32): $CLADDING_SUBNET" >&2
    return 1
  fi

  if ! subnet_ip_int="$(ipv4_to_int "$subnet_ip")"; then
    echo "error: invalid IPv4 subnet address: $CLADDING_SUBNET" >&2
    return 1
  fi

  if [ "$subnet_prefix" -eq 0 ]; then
    subnet_mask_int=0
  else
    subnet_mask_int=$(((4294967295 << (32 - subnet_prefix)) & 4294967295))
  fi
  subnet_network_int=$((subnet_ip_int & subnet_mask_int))
  subnet_broadcast_int=$((subnet_network_int | (4294967295 ^ subnet_mask_int)))

  # +1 is typically the bridge gateway on Podman networks; skip it.
  proxy_ip_int=$((subnet_network_int + 2))
  sandbox_ip_int=$((subnet_network_int + 3))
  cli_ip_int=$((subnet_network_int + 4))

  if [ "$cli_ip_int" -ge "$subnet_broadcast_int" ]; then
    echo "error: subnet too small, need usable IPs for gateway + 3 pods: $CLADDING_SUBNET" >&2
    return 1
  fi

  NETWORK="${CLADDING_NAME}_cladding_net"
  NETWORK_SUBNET="$(int_to_ipv4 "$subnet_network_int")/$subnet_prefix"
  PROXY_IP="$(int_to_ipv4 "$proxy_ip_int")"
  SANDBOX_IP="$(int_to_ipv4 "$sandbox_ip_int")"
  CLI_IP="$(int_to_ipv4 "$cli_ip_int")"

  PROXY_POD_NAME="${CLADDING_NAME}-proxy-pod"
  SANDBOX_POD_NAME="${CLADDING_NAME}-sandbox-pod"
  CLI_POD_NAME="${CLADDING_NAME}-cli-pod"
}

render_pods_yaml() {
  sed \
    -e "s|PROJECT_ROOT|$PROJECT_ROOT|g" \
    -e "s|CLADDING_ROOT|$CLADDING_ROOT|g" \
    -e "s|REPLACE_PROXY_POD_NAME|$PROXY_POD_NAME|g" \
    -e "s|REPLACE_SANDBOX_POD_NAME|$SANDBOX_POD_NAME|g" \
    -e "s|REPLACE_CLI_POD_NAME|$CLI_POD_NAME|g" \
    -e "s|REPLACE_SANDBOX_IMAGE|$SANDBOX_IMAGE|g" \
    -e "s|REPLACE_CLI_IMAGE|$CLI_IMAGE|g" \
    -e "s|REPLACE_PROXY_IP|$PROXY_IP|g" \
    -e "s|REPLACE_SANDBOX_IP|$SANDBOX_IP|g" \
    -e "s|REPLACE_CLI_IP|$CLI_IP|g" \
    "$CLADDING_ROOT/pods.yaml"
}

cmd_up() {
  check_required_paths
  check_required_binaries

  resolve_network_settings
  check_required_images
  ensure_network_settings

  render_pods_yaml | podman play kube \
      --network "$NETWORK" \
      --ip "$PROXY_IP" \
      --ip "$SANDBOX_IP" \
      --ip "$CLI_IP" \
      -
}

cmd_down() {
  resolve_network_settings
  render_pods_yaml | podman play kube --down -
}

cmd_destroy() {
  resolve_network_settings
  podman rm -f "$CLI_POD_NAME" "$SANDBOX_POD_NAME" "$PROXY_POD_NAME"
}

cwd_relative_to_project_root() {
  project_dir="$(dirname -- "$PROJECT_ROOT")"

  # 1. Try GNU realpath (Linux / Homebrew coreutils)
  if realpath --relative-to=. . >/dev/null 2>&1; then
    realpath --relative-to="$project_dir" "$PWD"
    return 0
  fi

  # 2. Try Python (Standard on macOS)
  if command -v python3 >/dev/null 2>&1; then
    python3 -c "import os.path, sys; print(os.path.relpath(sys.argv[1], sys.argv[2]))" "$PWD" "$project_dir"
    return 0
  fi

  # 3. Pure shell fallback (Simple subdirectory matching only)
  case "$PWD" in
    "$project_dir")
      printf '%s\n' "."
      ;;
    "$project_dir"/*)
      printf '%s\n' "${PWD#$project_dir/}"
      ;;
    *)
      return 1
      ;;
  esac
}

cmd_run() {
  if [ $# -eq 0 ]; then
    echo "usage: cladding run <command> [args...]" >&2
    return 1
  fi

  resolve_network_settings

  if workdir_rel="$(cwd_relative_to_project_root)"; then
    container_workdir="/home/user/workspace"
    if [ "$workdir_rel" != "." ]; then
      container_workdir="$container_workdir/$workdir_rel"
    fi
  else
    project_dir="$(dirname -- "$PROJECT_ROOT")"
    echo "error: could not determine current path relative to project dir ($project_dir): $PWD" >&2
    echo "hint: run cladding from $project_dir or one of its subdirectories" >&2
    return 1
  fi

  if [ -t 0 ] && [ -t 1 ]; then
    exec podman exec -it \
      -w "$container_workdir" \
      --env LANG="C.UTF-8" \
      --env TERM="xterm-256color" \
      --env COLORTERM="${COLORTERM:-truecolor}" \
      --env FORCE_COLOR="${FORCE_COLOR:-3}" \
      "$CLI_POD_NAME-cli-app" \
      "$@"
  fi

  exec podman exec -i \
    -w "$container_workdir" \
    --env LANG="C.UTF-8" \
    "$CLI_POD_NAME-cli-app" \
    "$@"
}

cmd_reload_proxy() {
  resolve_network_settings
  podman exec "$PROXY_POD_NAME-proxy" squid -k reconfigure -f /tmp/squid_generated.conf
}

if [ $# -eq 0 ]; then
  cmd="help"
else
  cmd="$1"
  shift
fi

if PROJECT_ROOT="$(find_project_root)"; then
  :
else
  case "$cmd" in
    help|-h|--help)
      ;;
    init)
      PROJECT_ROOT="$PWD/.cladding"
      ;;
    *)
      echo "error: no .cladding directory found in $PWD or any parent directory" >&2
      echo "hint: run 'cladding init' from the project directory to create one" >&2
      exit 1
      ;;
  esac
fi

case "$cmd" in
  build)
    cmd_build "$@"
    ;;
  init)
    cmd_init "$@"
    ;;
  check)
    cmd_check "$@"
    ;;
  up)
    cmd_up "$@"
    ;;
  down)
    cmd_down "$@"
    ;;
  destroy)
    cmd_destroy "$@"
    ;;
  run)
    cmd_run "$@"
    ;;
  reload-proxy)
    cmd_reload_proxy "$@"
    ;;
  help|-h|--help)
    print_help
    ;;
  *)
    echo "Unknown command: $cmd" >&2
    echo >&2
    print_help >&2
    exit 1
    ;;
esac
