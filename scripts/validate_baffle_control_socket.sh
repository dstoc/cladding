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
grep -Fqx 'socket_dir = "/run/cladding/proxy"' "$private_dir/daemon.toml"
for component in agent nw-sandbox; do
  if [ "$component" = nw-sandbox ] \
    && [ "${CLADDING_NW_SANDBOX_ENABLED:-false}" != true ]; then
    test ! -e "/run/cladding/proxy/$component/proxy.sock"
    continue
  fi
  socket_dir="/run/cladding/proxy/$component"
  socket_path="$socket_dir/proxy.sock"
  test "$(stat -c %a "$socket_dir")" = 700
  test "$(stat -c %u "$socket_dir")" = "$expected_uid"
  test -S "$socket_path"
  test "$(stat -c %a "$socket_path")" = 600
  test "$(stat -c %u "$socket_path")" = "$expected_uid"
done
