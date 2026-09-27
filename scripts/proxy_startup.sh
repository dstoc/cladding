#!/bin/sh
set -eu

BAFFLE_BIN=${BAFFLE_BIN:-/opt/tools/bin/baffle}
CONFIG_DIR=${BAFFLE_CONFIG_DIR:-/opt/config/proxy}
CREDENTIALS_DIR=${BAFFLE_CREDENTIALS_DIR:-/opt/credentials/baffle}
CONTROL_SOCKET=${BAFFLE_CONTROL_SOCKET:-/run/baffle/control.sock}
DEFAULT_CONTROL_SOCKET=/run/baffle/control.sock
SOCKET_DIR=${BAFFLE_SOCKET_DIR:-/run/cladding/proxy}
NW_SANDBOX_ENABLED=${CLADDING_NW_SANDBOX_ENABLED:-false}
daemon_pid=
control_socket_dir=
private_control_dir=
runtime_control_socket=
runtime_config=

log() {
    printf '[baffle-startup] %s\n' "$*" >&2
}

fail() {
    log "error: $*"
    exit 1
}

require_file() {
    if [ ! -f "$1" ] || [ ! -r "$1" ]; then
        fail "required file is missing or unreadable: $1"
    fi
}

require_directory() {
    if [ ! -d "$1" ] || [ ! -r "$1" ] || [ ! -x "$1" ]; then
        fail "required directory is missing or unreadable: $1"
    fi
}

stop_daemon() {
    if [ -n "$daemon_pid" ]; then
        # Baffle handles SIGINT with an orderly shutdown that removes session sockets.
        kill -INT "$daemon_pid" 2>/dev/null || true
        wait "$daemon_pid" 2>/dev/null || true
        daemon_pid=
    fi
}

remove_control_socket_alias() {
    alias_path=$1
    if [ -n "$runtime_control_socket" ] \
        && [ -L "$alias_path" ] \
        && [ "$(readlink "$alias_path")" = "$runtime_control_socket" ]; then
        rm -f "$alias_path" || true
    fi
}

cleanup() {
    status=$?
    trap - EXIT
    stop_daemon
    remove_control_socket_alias "$CONTROL_SOCKET"
    remove_control_socket_alias "$DEFAULT_CONTROL_SOCKET"
    if [ -n "$private_control_dir" ]; then
        rm -f "$private_control_dir/daemon.toml" "$private_control_dir/control.sock" || true
        rmdir "$private_control_dir" 2>/dev/null || true
    fi
    exit "$status"
}

shutdown() {
    signal=$1
    trap - HUP INT TERM EXIT
    log "received $signal; stopping Baffle daemon"
    stop_daemon
    exit 0
}

trap cleanup EXIT
trap 'shutdown HUP' HUP
trap 'shutdown INT' INT
trap 'shutdown TERM' TERM

case "$NW_SANDBOX_ENABLED" in
    true|false) ;;
    *) fail "CLADDING_NW_SANDBOX_ENABLED must be true or false" ;;
esac

if [ ! -x "$BAFFLE_BIN" ]; then
    fail "Baffle executable is missing or not executable: $BAFFLE_BIN"
fi

require_file "$CONFIG_DIR/daemon.toml"
require_file "$CONFIG_DIR/sessions/agent.toml"
require_file "$CREDENTIALS_DIR/ca.crt"
require_file "$CREDENTIALS_DIR/ca-key.pem"
require_directory "$CREDENTIALS_DIR/secrets"
require_directory "$SOCKET_DIR/agent"
if [ ! -w "$SOCKET_DIR/agent" ]; then
    fail "agent session socket directory is not writable: $SOCKET_DIR/agent"
fi

if [ "$NW_SANDBOX_ENABLED" = true ]; then
    require_file "$CONFIG_DIR/sessions/nw-sandbox.toml"
    require_directory "$SOCKET_DIR/nw-sandbox"
    if [ ! -w "$SOCKET_DIR/nw-sandbox" ]; then
        fail "network-sandbox session socket directory is not writable: $SOCKET_DIR/nw-sandbox"
    fi
fi

umask 077
proxy_uid=$(id -u)
control_socket_dir=$(dirname "$CONTROL_SOCKET")
if ! mkdir -p "$control_socket_dir"; then
    fail "failed to create private Baffle control-socket directory"
fi
if [ ! -w "$control_socket_dir" ]; then
    fail "Baffle control-socket parent directory is not writable: $control_socket_dir"
fi
private_control_dir="$control_socket_dir/$proxy_uid"
if ! mkdir -p "$private_control_dir"; then
    fail "failed to create private Baffle control directory"
fi
if ! chmod 0700 "$private_control_dir"; then
    fail "failed to secure private Baffle control directory"
fi
runtime_control_socket="$private_control_dir/control.sock"
rm -f "$private_control_dir/daemon.toml" "$runtime_control_socket"

if [ -e "$CONTROL_SOCKET" ] || [ -L "$CONTROL_SOCKET" ]; then
    if ! rm -f "$CONTROL_SOCKET"; then
        fail "failed to remove stale Baffle control-socket path: $CONTROL_SOCKET"
    fi
fi
if ! ln -s "$runtime_control_socket" "$CONTROL_SOCKET"; then
    fail "failed to publish private Baffle control socket at $CONTROL_SOCKET"
fi
if [ "$CONTROL_SOCKET" != "$DEFAULT_CONTROL_SOCKET" ]; then
    if [ -e "$DEFAULT_CONTROL_SOCKET" ] || [ -L "$DEFAULT_CONTROL_SOCKET" ]; then
        if ! rm -f "$DEFAULT_CONTROL_SOCKET"; then
            fail "failed to remove stale default Baffle control-socket path"
        fi
    fi
    if ! ln -s "$runtime_control_socket" "$DEFAULT_CONTROL_SOCKET"; then
        fail "failed to publish private Baffle control socket at $DEFAULT_CONTROL_SOCKET"
    fi
fi

runtime_config="$private_control_dir/daemon.toml"
if ! sed \
    -e "s|^trusted_operator_uid = .*|trusted_operator_uid = $proxy_uid|" \
    -e "s|^control_socket = .*|control_socket = \"$runtime_control_socket\"|" \
    "$CONFIG_DIR/daemon.toml" > "$runtime_config"; then
    fail "failed to write Baffle runtime configuration"
fi
if ! grep -q "^trusted_operator_uid = $proxy_uid$" "$runtime_config"; then
    fail "Baffle runtime configuration does not trust the proxy process UID"
fi
if ! grep -q "^control_socket = \"$runtime_control_socket\"$" "$runtime_config"; then
    fail "Baffle runtime configuration does not use the private control socket"
fi
if ! chmod 0600 "$runtime_config"; then
    fail "failed to secure Baffle runtime configuration"
fi

log "starting Baffle daemon"
"$BAFFLE_BIN" daemon --config "$runtime_config" &
daemon_pid=$!

attempt=0
status_ifs=$(printf ':\t ')
while :; do
    if [ -S "$CONTROL_SOCKET" ] && "$BAFFLE_BIN" list >/dev/null 2>&1; then
        break
    fi

    status_file="/proc/$daemon_pid/status"
    if [ ! -d "/proc/$daemon_pid" ]; then
        if wait "$daemon_pid"; then
            daemon_status=0
        else
            daemon_status=$?
        fi
        daemon_pid=
        fail "Baffle daemon exited before creating its control socket (exit code $daemon_status)"
    fi

    daemon_state=
    if [ -r "$status_file" ]; then
        while IFS="$status_ifs" read -r key value rest; do
            if [ "$key" = State ]; then
                daemon_state=$value
                break
            fi
        done < "$status_file"
    fi
    case "$daemon_state" in
        Z|X)
            if wait "$daemon_pid"; then
                daemon_status=0
            else
                daemon_status=$?
            fi
            daemon_pid=
            fail "Baffle daemon exited before creating its control socket (exit code $daemon_status)"
            ;;
    esac

    if [ "$attempt" -ge 300 ]; then
        if [ -S "$CONTROL_SOCKET" ]; then
            "$BAFFLE_BIN" list >/dev/null || true
            fail "timed out after 30 seconds waiting for Baffle control socket to accept commands: $CONTROL_SOCKET"
        fi
        fail "timed out after 30 seconds waiting for Baffle control socket $CONTROL_SOCKET"
    fi
    attempt=$((attempt + 1))
    sleep 0.1
done

log "Baffle control socket accepts commands"
if ! cd "$CONFIG_DIR/sessions"; then
    fail "cannot read Baffle session configuration directory: $CONFIG_DIR/sessions"
fi

create_session() {
    session_file=$1
    log "creating persistent session from $session_file"
    if "$BAFFLE_BIN" create "$session_file"; then
        log "created persistent session from $session_file"
    else
        status=$?
        fail "failed to create session from $session_file (exit code $status)"
    fi
}

create_session agent.toml
if [ "$NW_SANDBOX_ENABLED" = true ]; then
    create_session nw-sandbox.toml
fi

log "Baffle daemon is supervising persistent sessions"
if wait "$daemon_pid"; then
    daemon_status=0
else
    daemon_status=$?
fi
daemon_pid=

if [ "$daemon_status" -eq 0 ]; then
    daemon_status=1
fi
log "error: Baffle daemon exited unexpectedly (exit code $daemon_status)"
exit "$daemon_status"
