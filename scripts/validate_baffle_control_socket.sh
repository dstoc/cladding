#!/bin/sh
set -eu

expected_uid=$(id -u)
private_dir="/run/baffle/$expected_uid"
printf "control_check_identity: uid=%s gid=%s\n" "$expected_uid" "$(id -g)"
stat -c "control_check_metadata: mode=%a uid=%u gid=%g path=%n" \
  /run/baffle "$private_dir" "$private_dir/daemon.toml" \
  "$private_dir/control.sock" /run/baffle/control.sock || true
printf "control_check_alias_target: %s\n" "$(readlink /run/baffle/control.sock 2>&1 || true)"
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
grep -Fqx "control_socket = \"$private_dir/control.sock\"" "$private_dir/daemon.toml"
if [ "${CLADDING_BAFFLE_SOCKET_RELAY:-false}" = true ]; then
  data_dir=$(sed -n 's/^socket_dir = "\(.*\)"$/\1/p' "$private_dir/daemon.toml")
  case "$data_dir" in
    /tmp/cladding-baffle-data.*) ;;
    *) echo "Baffle relay data directory is outside its private temporary root" >&2; exit 1 ;;
  esac
  test "$(stat -c %a /tmp)" = 1777
  test "$(stat -c %a "$data_dir")" = 700
  test "$(stat -c %u "$data_dir")" = "$expected_uid"
  test "$(stat -c %a "$data_dir/agent")" = 700
  test "$(stat -c %u "$data_dir/agent")" = "$expected_uid"
  test -S "$data_dir/agent/proxy.sock"
  test "$(stat -c %a "$data_dir/agent/proxy.sock")" = 600
  test "$(stat -c %u "$data_dir/agent/proxy.sock")" = "$expected_uid"
  if [ "${CLADDING_NW_SANDBOX_ENABLED:-false}" = true ]; then
    test "$(stat -c %a "$data_dir/nw-sandbox")" = 700
    test "$(stat -c %u "$data_dir/nw-sandbox")" = "$expected_uid"
    test -S "$data_dir/nw-sandbox/proxy.sock"
    test "$(stat -c %a "$data_dir/nw-sandbox/proxy.sock")" = 600
    test "$(stat -c %u "$data_dir/nw-sandbox/proxy.sock")" = "$expected_uid"
  else
    test ! -e "$data_dir/nw-sandbox"
  fi
fi
