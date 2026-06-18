pub(super) fn build_idle_supervisor_command(
    bridge_command: &str,
    primary_command: &str,
) -> Vec<String> {
    vec![
        "/bin/sh".to_string(),
        "-ec".to_string(),
        format!(
            r#"
{bridge_command} &
bridge_pid=$!
(
  while kill -0 "$bridge_pid" 2>/dev/null; do
    sleep 1
  done
  wait "$bridge_pid" 2>/dev/null || true
  kill -TERM "$PPID" 2>/dev/null || true
) &
watcher_pid=$!
trap 'kill "$bridge_pid" "$watcher_pid" 2>/dev/null || true' INT TERM
exec {primary_command}
"#
        ),
    ]
}

pub(super) fn build_mcp_run_supervisor_command(
    bridge_command: &str,
    primary_command: &str,
) -> Vec<String> {
    vec![
        "/bin/sh".to_string(),
        "-ec".to_string(),
        format!(
            r#"
{bridge_command} &
bridge_pid=$!
(
  while kill -0 "$bridge_pid" 2>/dev/null; do
    sleep 1
  done
  wait "$bridge_pid" 2>/dev/null || true
  kill -TERM "$PPID" 2>/dev/null || true
) &
watcher_pid=$!
trap 'kill "$bridge_pid" "$watcher_pid" 2>/dev/null || true' INT TERM
exec {primary_command}
"#
        ),
    ]
}
