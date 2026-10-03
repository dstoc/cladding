pub(super) fn build_idle_supervisor_command(
    bridge_command: &str,
    primary_command: &str,
) -> Vec<String> {
    build_supervisor_command(bridge_command, primary_command)
}

pub(super) fn build_mcp_run_supervisor_command(
    bridge_command: &str,
    primary_command: &str,
) -> Vec<String> {
    build_supervisor_command(bridge_command, primary_command)
}

fn build_supervisor_command(bridge_command: &str, primary_command: &str) -> Vec<String> {
    vec![
        "/bin/sh".to_string(),
        "-ec".to_string(),
        format!(
            r#"
supervisor_pid=$$
bridge_pid=
watcher_pid=
primary_pid=

watcher_shutdown() {{
  trap '' INT TERM
  if [ -n "$watcher_sleep_pid" ]; then
    kill -TERM "$watcher_sleep_pid" 2>/dev/null || true
    wait "$watcher_sleep_pid" 2>/dev/null || true
  fi
  exit 0
}}

supervisor_shutdown() {{
  shutdown_signal=$1
  trap '' INT TERM
  for child_pid in "$primary_pid" "$bridge_pid" "$watcher_pid"; do
    if [ -n "$child_pid" ]; then
      kill -"$shutdown_signal" "$child_pid" 2>/dev/null || true
    fi
  done
  for child_pid in "$primary_pid" "$bridge_pid" "$watcher_pid"; do
    if [ -n "$child_pid" ]; then
      wait "$child_pid" 2>/dev/null || true
    fi
  done
  exit 0
}}

trap 'supervisor_shutdown TERM' TERM
trap 'supervisor_shutdown INT' INT

{bridge_command} &
bridge_pid=$!
(
  watcher_sleep_pid=
  trap watcher_shutdown INT TERM
  while kill -0 "$bridge_pid" 2>/dev/null; do
    sleep 1 &
    watcher_sleep_pid=$!
    wait "$watcher_sleep_pid" 2>/dev/null || true
    watcher_sleep_pid=
  done
  kill -TERM "$supervisor_pid" 2>/dev/null || true
) &
watcher_pid=$!

{primary_command} &
primary_pid=$!

if wait "$primary_pid"; then
  primary_status=0
else
  primary_status=$?
fi
primary_pid=

trap '' INT TERM
for child_pid in "$bridge_pid" "$watcher_pid"; do
  if [ -n "$child_pid" ]; then
    kill -TERM "$child_pid" 2>/dev/null || true
  fi
done
for child_pid in "$bridge_pid" "$watcher_pid"; do
  if [ -n "$child_pid" ]; then
    wait "$child_pid" 2>/dev/null || true
  fi
done

exit "$primary_status"
"#
        ),
    ]
}

#[cfg(unix)]
#[cfg(test)]
mod tests {
    use super::{build_idle_supervisor_command, build_mcp_run_supervisor_command};
    use std::fs;
    use std::path::{Path, PathBuf};
    use std::process::{Child, Command, ExitStatus};
    use std::sync::Arc;
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::thread;
    use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

    const CHILD_PID_ENV: &str = "CLADDING_TEST_SUPERVISOR_CHILD_PID";
    const CHILD_SIGNAL_ENV: &str = "CLADDING_TEST_SUPERVISOR_CHILD_SIGNAL";
    const CHILD_STOP_ENV: &str = "CLADDING_TEST_SUPERVISOR_CHILD_STOP";
    const CHILD_EXIT_ENV: &str = "CLADDING_TEST_SUPERVISOR_CHILD_EXIT";

    #[test]
    fn supervisor_forwards_term_and_int_and_reaps_children() {
        for builder in [
            build_idle_supervisor_command,
            build_mcp_run_supervisor_command,
        ] {
            for (signal, signal_name) in [(libc::SIGTERM, "TERM"), (libc::SIGINT, "INT")] {
                let temp = test_temp_dir();
                let bridge_pid_file = temp.join("bridge.pid");
                let bridge_signal_file = temp.join("bridge.signal");
                let primary_pid_file = temp.join("primary.pid");
                let primary_signal_file = temp.join("primary.signal");
                let bridge =
                    signal_child_command(&bridge_pid_file, &bridge_signal_file, None, None);
                let primary =
                    signal_child_command(&primary_pid_file, &primary_signal_file, None, None);
                let mut supervisor = start_supervisor(builder(&bridge, &primary));

                wait_for_file(&bridge_pid_file);
                wait_for_file(&primary_pid_file);
                let bridge_pid = read_pid(&bridge_pid_file);
                let primary_pid = read_pid(&primary_pid_file);
                #[cfg(target_os = "linux")]
                let (watcher_pid, watcher_child_pids) = {
                    let child_pids = wait_for_process_children(supervisor.id(), 3);
                    assert!(child_pids.contains(&bridge_pid));
                    assert!(child_pids.contains(&primary_pid));
                    let watcher_pid = *child_pids
                        .iter()
                        .find(|pid| **pid != bridge_pid && **pid != primary_pid)
                        .expect("supervisor watcher child");
                    let watcher_child_pids = wait_for_process_children(watcher_pid as u32, 1);
                    (watcher_pid, watcher_child_pids)
                };

                let result = unsafe { libc::kill(supervisor.id() as libc::pid_t, signal) };
                assert_eq!(result, 0, "failed to signal supervisor");
                let status = wait_for_exit(&mut supervisor);

                assert!(status.success(), "supervisor exited with {status}");
                assert_eq!(fs::read_to_string(bridge_signal_file).unwrap(), signal_name);
                assert_eq!(
                    fs::read_to_string(primary_signal_file).unwrap(),
                    signal_name
                );
                assert_process_gone(bridge_pid);
                assert_process_gone(primary_pid);
                #[cfg(target_os = "linux")]
                {
                    assert_process_gone(watcher_pid);
                    for pid in watcher_child_pids {
                        assert_process_gone(pid);
                    }
                }
                fs::remove_dir_all(temp).unwrap();
            }
        }
    }

    #[test]
    fn supervisor_stops_primary_when_proxy_bridge_exits() {
        let temp = test_temp_dir();
        let bridge_pid_file = temp.join("bridge.pid");
        let bridge_signal_file = temp.join("bridge.signal");
        let bridge_stop_file = temp.join("bridge.stop");
        let primary_pid_file = temp.join("primary.pid");
        let primary_signal_file = temp.join("primary.signal");
        let bridge = signal_child_command(
            &bridge_pid_file,
            &bridge_signal_file,
            Some(&bridge_stop_file),
            None,
        );
        let primary = signal_child_command(&primary_pid_file, &primary_signal_file, None, None);
        let command = build_mcp_run_supervisor_command(&bridge, &primary);
        let mut supervisor = start_supervisor(command);

        wait_for_file(&bridge_pid_file);
        wait_for_file(&primary_pid_file);
        let bridge_pid = read_pid(&bridge_pid_file);
        let primary_pid = read_pid(&primary_pid_file);
        fs::write(&bridge_stop_file, "stop").unwrap();

        let status = wait_for_exit(&mut supervisor);
        assert!(status.success(), "supervisor exited with {status}");
        assert!(!bridge_signal_file.exists(), "the bridge exited on its own");
        assert_eq!(fs::read_to_string(primary_signal_file).unwrap(), "TERM");
        assert_process_gone(bridge_pid);
        assert_process_gone(primary_pid);
        fs::remove_dir_all(temp).unwrap();
    }

    #[test]
    fn supervisor_reaps_helpers_and_preserves_primary_exit_status() {
        let temp = test_temp_dir();
        let bridge_pid_file = temp.join("bridge.pid");
        let bridge_signal_file = temp.join("bridge.signal");
        let primary_pid_file = temp.join("primary.pid");
        let primary_stop_file = temp.join("primary.stop");
        let bridge = signal_child_command(&bridge_pid_file, &bridge_signal_file, None, None);
        let primary = signal_child_command(
            &primary_pid_file,
            &temp.join("primary.signal"),
            Some(&primary_stop_file),
            Some(23),
        );
        let mut supervisor = start_supervisor(build_idle_supervisor_command(&bridge, &primary));

        wait_for_file(&bridge_pid_file);
        wait_for_file(&primary_pid_file);
        let bridge_pid = read_pid(&bridge_pid_file);
        let primary_pid = read_pid(&primary_pid_file);
        fs::write(primary_stop_file, "stop").unwrap();
        let status = wait_for_exit(&mut supervisor);

        assert_eq!(status.code(), Some(23));
        assert_eq!(fs::read_to_string(bridge_signal_file).unwrap(), "TERM");
        assert_process_gone(bridge_pid);
        assert_process_gone(primary_pid);
        fs::remove_dir_all(temp).unwrap();
    }

    #[test]
    fn supervisor_signal_child_fixture() {
        let Ok(pid_path) = std::env::var(CHILD_PID_ENV) else {
            return;
        };
        let signal_path = PathBuf::from(std::env::var_os(CHILD_SIGNAL_ENV).unwrap());
        let term_received = Arc::new(AtomicBool::new(false));
        let int_received = Arc::new(AtomicBool::new(false));
        let _term_registration =
            signal_hook::flag::register(signal_hook::consts::SIGTERM, Arc::clone(&term_received))
                .unwrap();
        let _int_registration =
            signal_hook::flag::register(signal_hook::consts::SIGINT, Arc::clone(&int_received))
                .unwrap();
        let stop_path = std::env::var_os(CHILD_STOP_ENV).map(PathBuf::from);
        fs::write(pid_path, std::process::id().to_string()).unwrap();

        loop {
            if term_received.load(Ordering::Acquire) {
                fs::write(signal_path, "TERM").unwrap();
                return;
            }
            if int_received.load(Ordering::Acquire) {
                fs::write(signal_path, "INT").unwrap();
                return;
            }
            if stop_path.as_ref().is_some_and(|path| path.exists()) {
                let exit_code = std::env::var(CHILD_EXIT_ENV)
                    .ok()
                    .and_then(|status| status.parse().ok())
                    .unwrap_or(0);
                std::process::exit(exit_code);
            }
            thread::sleep(Duration::from_millis(10));
        }
    }

    fn signal_child_command(
        pid_path: &Path,
        signal_path: &Path,
        stop_path: Option<&Path>,
        exit_code: Option<i32>,
    ) -> String {
        let executable = shell_quote(&std::env::current_exe().unwrap().display().to_string());
        let mut command = format!(
            "{CHILD_PID_ENV}={} {CHILD_SIGNAL_ENV}={} ",
            shell_quote(&pid_path.display().to_string()),
            shell_quote(&signal_path.display().to_string())
        );
        if let Some(stop_path) = stop_path {
            command.push_str(&format!(
                "{CHILD_STOP_ENV}={} ",
                shell_quote(&stop_path.display().to_string())
            ));
        }
        if let Some(exit_code) = exit_code {
            command.push_str(&format!("{CHILD_EXIT_ENV}={exit_code} "));
        }
        command.push_str(&format!(
            "{executable} supervisor_signal_child_fixture --test-threads 1 >/dev/null 2>&1"
        ));
        command
    }

    fn start_supervisor(command: Vec<String>) -> Child {
        Command::new(&command[0])
            .args(&command[1..])
            .spawn()
            .expect("start test supervisor")
    }

    fn shell_quote(value: &str) -> String {
        format!("'{}'", value.replace('\'', "'\\''"))
    }

    fn test_temp_dir() -> PathBuf {
        let suffix = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let path = std::env::temp_dir().join(format!(
            "cladding-supervisor-test-{}-{suffix}",
            std::process::id()
        ));
        fs::create_dir_all(&path).unwrap();
        path
    }

    fn wait_for_file(path: &Path) {
        let deadline = Instant::now() + Duration::from_secs(5);
        while !path.exists() {
            assert!(
                Instant::now() < deadline,
                "timed out waiting for {}",
                path.display()
            );
            thread::sleep(Duration::from_millis(10));
        }
    }

    fn read_pid(path: &Path) -> libc::pid_t {
        fs::read_to_string(path).unwrap().parse().unwrap()
    }

    #[cfg(target_os = "linux")]
    fn wait_for_process_children(pid: u32, minimum_count: usize) -> Vec<libc::pid_t> {
        let children_path = format!("/proc/{pid}/task/{pid}/children");
        let deadline = Instant::now() + Duration::from_secs(5);
        loop {
            let children = fs::read_to_string(&children_path)
                .unwrap_or_default()
                .split_whitespace()
                .map(|pid| pid.parse().unwrap())
                .collect::<Vec<_>>();
            if children.len() >= minimum_count {
                return children;
            }
            assert!(
                Instant::now() < deadline,
                "timed out waiting for {minimum_count} child process(es) of {pid}"
            );
            thread::sleep(Duration::from_millis(10));
        }
    }

    fn wait_for_exit(child: &mut Child) -> ExitStatus {
        let deadline = Instant::now() + Duration::from_secs(5);
        loop {
            if let Some(status) = child.try_wait().expect("check supervisor status") {
                return status;
            }
            if Instant::now() >= deadline {
                let _ = child.kill();
                panic!("supervisor did not exit promptly");
            }
            thread::sleep(Duration::from_millis(10));
        }
    }

    fn assert_process_gone(pid: libc::pid_t) {
        let result = unsafe { libc::kill(pid, 0) };
        assert_eq!(result, -1, "process {pid} remained after supervisor exit");
        assert_eq!(
            std::io::Error::last_os_error().raw_os_error(),
            Some(libc::ESRCH)
        );
    }
}
