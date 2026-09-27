#!/bin/sh
set -eu

BAFFLE_BIN=${BAFFLE_BIN:-/opt/tools/bin/baffle}
CONFIG_DIR=${BAFFLE_CONFIG_DIR:-/opt/config/proxy}
CREDENTIALS_DIR=${BAFFLE_CREDENTIALS_DIR:-/opt/credentials/baffle}
CONTROL_SOCKET=${BAFFLE_CONTROL_SOCKET:-/run/baffle/control.sock}
DEFAULT_CONTROL_SOCKET=/run/baffle/control.sock
SOCKET_DIR=${BAFFLE_SOCKET_DIR:-/run/cladding/proxy}
NW_SANDBOX_ENABLED=${CLADDING_NW_SANDBOX_ENABLED:-false}
SOCKET_RELAY=${CLADDING_BAFFLE_SOCKET_RELAY:-false}
daemon_pid=
relay_pids=
control_socket_dir=
private_control_dir=
private_data_dir=
runtime_control_socket=
runtime_config=

log() {
    printf '[baffle-startup] %s\n' "$*" >&2
}

ensure_mode() {
    mode_path=$1
    expected_mode=$2
    description=$3
    observed_mode=$(stat -c '%a' "$mode_path" 2>/dev/null || printf 'unavailable')
    if [ "$observed_mode" != "$expected_mode" ]; then
        if ! chmod "$expected_mode" "$mode_path"; then
            fail "$description: failed to set path=$mode_path expected_mode=0$expected_mode observed_mode=$observed_mode"
        fi
        observed_mode=$(stat -c '%a' "$mode_path" 2>/dev/null || printf 'unavailable')
    fi
    if [ "$observed_mode" != "$expected_mode" ]; then
        fail "$description: mode mismatch path=$mode_path expected_mode=0$expected_mode observed_mode=$observed_mode"
    fi
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

stop_socket_relays() {
    for relay_pid in $relay_pids; do
        kill -TERM "$relay_pid" 2>/dev/null || true
    done
    for relay_pid in $relay_pids; do
        wait "$relay_pid" 2>/dev/null || true
    done
    relay_pids=
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
    stop_socket_relays
    remove_control_socket_alias "$CONTROL_SOCKET"
    remove_control_socket_alias "$DEFAULT_CONTROL_SOCKET"
    if [ "$SOCKET_RELAY" = true ]; then
        for relay_component in agent nw-sandbox; do
            relay_socket="$SOCKET_DIR/$relay_component/proxy.sock"
            if [ -S "$relay_socket" ]; then
                rm -f "$relay_socket" || true
            fi
        done
    fi
    if [ -n "$private_control_dir" ]; then
        rm -f "$private_control_dir/daemon.toml" "$private_control_dir/control.sock" || true
        rmdir "$private_control_dir" 2>/dev/null || true
    fi
    if [ -n "$private_data_dir" ]; then
        rm -f "$private_data_dir/agent/proxy.sock" \
            "$private_data_dir/nw-sandbox/proxy.sock" || true
        rmdir "$private_data_dir/agent" "$private_data_dir/nw-sandbox" \
            "$private_data_dir" 2>/dev/null || true
    fi
    exit "$status"
}

shutdown() {
    signal=$1
    trap - HUP INT TERM
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
case "$SOCKET_RELAY" in
    true) command -v socat >/dev/null 2>&1 || fail "socat is required for Baffle socket relay mode" ;;
    false) ;;
    *) fail "CLADDING_BAFFLE_SOCKET_RELAY must be true or false" ;;
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

baffle_socket_dir="$SOCKET_DIR"
if [ "$SOCKET_RELAY" = true ]; then
    private_data_dir=$(mktemp -d /tmp/cladding-baffle-data.XXXXXX) \
        || fail "failed to create private Baffle data-socket directory"
    if ! chmod 0700 "$private_data_dir"; then
        fail "failed to secure private Baffle data-socket directory"
    fi
    if ! mkdir -p "$private_data_dir/agent"; then
        fail "failed to create private Baffle agent socket directory"
    fi
    if [ "$NW_SANDBOX_ENABLED" = true ] && ! mkdir -p "$private_data_dir/nw-sandbox"; then
        fail "failed to create private Baffle network-sandbox socket directory"
    fi
    if ! chmod 0700 "$private_data_dir" "$private_data_dir/agent"; then
        fail "failed to secure private Baffle agent socket directory"
    fi
    if [ "$NW_SANDBOX_ENABLED" = true ] \
        && ! chmod 0700 "$private_data_dir/nw-sandbox"; then
        fail "failed to secure private Baffle network-sandbox socket directory"
    fi
    baffle_socket_dir=$private_data_dir
fi

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
    -e "s|^socket_dir = .*|socket_dir = \"$baffle_socket_dir\"|" \
    "$CONFIG_DIR/daemon.toml" > "$runtime_config"; then
    fail "failed to write Baffle runtime configuration"
fi
if ! grep -q "^trusted_operator_uid = $proxy_uid$" "$runtime_config"; then
    fail "Baffle runtime configuration does not trust the proxy process UID"
fi
if ! grep -q "^control_socket = \"$runtime_control_socket\"$" "$runtime_config"; then
    fail "Baffle runtime configuration does not use the private control socket"
fi
if ! grep -q "^socket_dir = \"$baffle_socket_dir\"$" "$runtime_config"; then
    fail "Baffle runtime configuration does not use the configured data socket directory"
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
daemon_status_file="/proc/$daemon_pid/status"
if [ -r "$daemon_status_file" ]; then
    awk '/^(Name|Uid|Gid|Groups|NSpid):/ { print "[baffle-startup] daemon_process_identity: " $0 }' \
        "$daemon_status_file" >&2 || true
else
    log "Baffle daemon process status is not readable: $daemon_status_file"
fi
if ! cd "$CONFIG_DIR/sessions"; then
    fail "cannot read Baffle session configuration directory: $CONFIG_DIR/sessions"
fi

start_socket_relay() {
    component=$1
    private_socket="$private_data_dir/$component/proxy.sock"
    relay_dir="$SOCKET_DIR/$component"
    public_socket="$relay_dir/proxy.sock"
    attempt=0
    while [ "$attempt" -lt 300 ] && [ ! -S "$private_socket" ]; do
        if [ ! -d "/proc/$daemon_pid" ]; then
            fail "Baffle daemon exited before creating its $component data socket"
        fi
        attempt=$((attempt + 1))
        sleep 0.1
    done
    if [ ! -S "$private_socket" ]; then
        fail "timed out waiting for Baffle $component data socket: $private_socket"
    fi

    ensure_mode "$relay_dir" 700 \
        "Baffle $component relay directory mode"
    if [ -S "$public_socket" ]; then
        rm -f "$public_socket" || fail "failed to remove stale proxy relay socket: $public_socket"
    elif [ -e "$public_socket" ]; then
        fail "proxy relay socket path exists and is not a socket: $public_socket"
    fi

    # The umask creates a mode-0600 socket without a post-bind chmod on shared filesystems.
    (
        umask 0177
        exec socat "UNIX-LISTEN:$public_socket,fork,unlink-close" \
            "UNIX-CONNECT:$private_socket"
    ) &
    relay_pid=$!
    relay_pids="$relay_pids $relay_pid"

    attempt=0
    while [ "$attempt" -lt 50 ] && [ ! -S "$public_socket" ]; do
        if ! kill -0 "$relay_pid" 2>/dev/null; then
            fail "socat relay exited before creating the $component socket"
        fi
        attempt=$((attempt + 1))
        sleep 0.1
    done
    if [ ! -S "$public_socket" ]; then
        fail "timed out waiting for the $component proxy relay socket: $public_socket"
    fi
    ensure_mode "$public_socket" 600 "Baffle $component relay socket mode"
    log "started trusted $component data-socket relay"
}

create_session() {
    component=$1
    session_file=$2

    if [ "$SOCKET_RELAY" = true ]; then
        session_socket_dir="$private_data_dir/$component"
    else
        session_socket_dir="$SOCKET_DIR/$component"
    fi

    probe_socket_bind() {
        probe_component=$1
        probe_directory=$2
        probe_name=".cladding-bind-probe-$probe_component-$$.sock"
        probe_path="$probe_directory/$probe_name"
        probe_bind_path="/proc/self/fd/3/$probe_name"
        probe_output="${TMPDIR:-/tmp}/cladding-bind-probe-$probe_component-$$.log"
        canonical_directory=$(readlink -f "$probe_directory" 2>/dev/null || printf '%s' "$probe_directory")

        log "Unix socket bind probe: component=$probe_component uid=$(id -u) directory=$probe_directory canonical_directory=$canonical_directory bind_path=$probe_bind_path canonical_target=$(readlink -f "$probe_path" 2>/dev/null || printf '%s' "$probe_path")"
        id >&2 || true
        stat -c 'directory_metadata: mode=%a uid=%u gid=%g path=%n' "$probe_directory" >&2 || true
        stat -f -c 'filesystem_type=%T path=%n' "$probe_directory" >&2 || true
        awk -v target="$canonical_directory" '
            {
                mount = $5
                if ((mount == "/" || target == mount || index(target, mount "/") == 1) \
                    && length(mount) >= longest) {
                    mount_record = $0
                    longest = length(mount)
                }
            }
            END {
                if (mount_record != "") print "mountinfo: " mount_record
                else print "mountinfo: no matching mount found"
            }
        ' /proc/self/mountinfo >&2 || true

        if [ -e "$probe_path" ] || [ -L "$probe_path" ]; then
            log "Unix socket bind probe path already exists: $probe_path"
            return 1
        fi

        (
            exec 3< "$probe_directory"
            umask 0077
            exec socat "UNIX-LISTEN:$probe_bind_path,unlink-early" /dev/null
        ) >"$probe_output" 2>&1 &
        probe_pid=$!
        probe_attempt=0
        while [ "$probe_attempt" -lt 50 ] && [ ! -S "$probe_path" ]; do
            if ! kill -0 "$probe_pid" 2>/dev/null; then
                break
            fi
            probe_attempt=$((probe_attempt + 1))
            sleep 0.1
        done

        if [ -S "$probe_path" ]; then
            if ! chmod 0600 "$probe_path"; then
                log "Unix socket permission probe failed: component=$probe_component path=$probe_path"
                stat -c 'probe_socket_metadata: mode=%a uid=%u gid=%g path=%n' "$probe_path" >&2 || true
                kill -TERM "$probe_pid" 2>/dev/null || true
                wait "$probe_pid" 2>/dev/null || true
                rm -f "$probe_path" || true
                rm -f "$probe_output"
                return 1
            fi
            stat -c 'probe_socket_metadata: mode=%a uid=%u gid=%g path=%n' "$probe_path" >&2 || true
            kill -TERM "$probe_pid" 2>/dev/null || true
            wait "$probe_pid" 2>/dev/null || true
            rm -f "$probe_path"
            rm -f "$probe_output"
            log "Unix socket bind probe passed: component=$probe_component"
            return 0
        fi

        if kill -0 "$probe_pid" 2>/dev/null; then
            kill -TERM "$probe_pid" 2>/dev/null || true
        fi
        probe_status=0
        wait "$probe_pid" || probe_status=$?
        log "Unix socket bind probe failed: component=$probe_component exit_code=$probe_status path=$probe_path"
        cat "$probe_output" >&2 || true
        if [ -e "$probe_path" ] || [ -S "$probe_path" ]; then
            ls -ld "$probe_path" >&2 || true
            rm -f "$probe_path" || true
        fi
        rm -f "$probe_output"
        return 1
    }

    if [ "${CLADDING_BAFFLE_BIND_PROBE:-false}" = true ]; then
        if ! probe_socket_bind "$component" "$session_socket_dir"; then
            fail "minimal Unix socket bind failed for $component session directory: $session_socket_dir"
        fi
    fi

    if [ "${CLADDING_BAFFLE_EXACT_BIND_PROBE:-false}" = true ]; then
        exact_probe=/opt/tools/bin/baffle-socket-probe
        if [ ! -x "$exact_probe" ]; then
            fail "Baffle socket operation probe is missing or not executable: $exact_probe"
        fi
        log "running Baffle-compatible socket operation probe for $component"
        if ! "$exact_probe" "$session_socket_dir/proxy.sock"; then
            fail "Baffle-compatible socket operation probe failed for $component session directory: $session_socket_dir"
        fi
    fi

    log "creating persistent session from $session_file"
    if "$BAFFLE_BIN" create "$session_file"; then
        log "created persistent session from $session_file"
    else
        status=$?
        log "session socket bind context: uid=$proxy_uid socket_dir=$baffle_socket_dir"
        grep '^socket_dir = ' "$runtime_config" >&2 || true
        for socket_path in "$private_control_dir" "$baffle_socket_dir" \
            "$baffle_socket_dir/$component" "$baffle_socket_dir/$component/proxy.sock"; do
            if [ -e "$socket_path" ]; then
                ls -ld "$socket_path" >&2 || true
            else
                log "session socket bind path is missing: $socket_path"
            fi
        done
        fail "failed to create session from $session_file (exit code $status)"
    fi
    if [ "$SOCKET_RELAY" = true ]; then
        start_socket_relay "$component"
    fi
}

create_session agent agent.toml
if [ "$NW_SANDBOX_ENABLED" = true ]; then
    create_session nw-sandbox nw-sandbox.toml
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
