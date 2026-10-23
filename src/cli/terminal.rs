use std::io;
use std::os::fd::RawFd;
use std::process::Command;

pub(super) struct ForegroundTerminal {
    fd: RawFd,
    original_group: libc::pid_t,
}

impl ForegroundTerminal {
    pub(super) fn capture(fd: RawFd) -> io::Result<Self> {
        // SAFETY: tcgetpgrp only reads the terminal state associated with fd.
        let original_group = unsafe { libc::tcgetpgrp(fd) };
        if original_group < 0 {
            return Err(io::Error::last_os_error());
        }
        Ok(Self { fd, original_group })
    }

    pub(super) fn configure_child(&self, command: &mut Command) {
        use std::os::unix::process::CommandExt as _;

        command.process_group(0);
        let fd = self.fd;
        // SAFETY: the hook uses only process and terminal syscalls before exec.
        unsafe {
            command.pre_exec(move || {
                if libc::setpgid(0, 0) != 0 {
                    return Err(io::Error::last_os_error());
                }
                set_foreground_group(fd, libc::getpgrp())
            });
        }
    }
}

impl Drop for ForegroundTerminal {
    fn drop(&mut self) {
        let _ = set_foreground_group(self.fd, self.original_group);
    }
}

fn set_foreground_group(fd: RawFd, group: libc::pid_t) -> io::Result<()> {
    // A process group in the background receives SIGTTOU when it changes the
    // terminal foreground group. Block it for the syscall, then restore the
    // thread's previous signal mask.
    // SAFETY: these calls use stack-owned signal sets and a valid tty descriptor.
    unsafe {
        let mut blocked = std::mem::zeroed::<libc::sigset_t>();
        let mut previous = std::mem::zeroed::<libc::sigset_t>();
        libc::sigemptyset(&mut blocked);
        libc::sigaddset(&mut blocked, libc::SIGTTOU);
        let mask_result = libc::pthread_sigmask(libc::SIG_BLOCK, &blocked, &mut previous);
        if mask_result != 0 {
            return Err(io::Error::from_raw_os_error(mask_result));
        }

        let foreground_result = libc::tcsetpgrp(fd, group);
        let foreground_error = (foreground_result != 0).then(io::Error::last_os_error);
        let restore_result =
            libc::pthread_sigmask(libc::SIG_SETMASK, &previous, std::ptr::null_mut());
        if let Some(error) = foreground_error {
            return Err(error);
        }
        if restore_result != 0 {
            return Err(io::Error::from_raw_os_error(restore_result));
        }
    }
    Ok(())
}

#[cfg(all(test, target_os = "linux"))]
mod tests {
    use super::ForegroundTerminal;
    use std::fs::File;
    use std::io::{self, Read as _, Write as _};
    use std::os::fd::{AsRawFd as _, FromRawFd as _, RawFd};
    use std::os::unix::process::CommandExt as _;
    use std::process::{Command, Stdio};
    use std::thread;
    use std::time::{Duration, Instant};

    const CHILD_FLAG: &str = "CLADDING_TERMINAL_PTY_CHILD";
    const TEST_NAME: &str =
        "cli::terminal::tests::foreground_terminal_handoff_allows_child_to_read";

    #[test]
    fn foreground_terminal_handoff_allows_child_to_read() {
        if std::env::var_os(CHILD_FLAG).is_some() {
            let terminal = ForegroundTerminal::capture(0).unwrap();
            let mut command = Command::new("/bin/sh");
            command
                .args([
                    "-c",
                    "printf READY; read value; printf 'GOT:%s\\n' \"$value\"",
                ])
                .stdin(Stdio::inherit())
                .stdout(Stdio::inherit())
                .stderr(Stdio::inherit());
            terminal.configure_child(&mut command);
            let status = command.status().unwrap();
            drop(terminal);
            assert!(status.success());
            return;
        }

        // SAFETY: openpty initializes two valid descriptors which this test owns.
        let (master, slave) = unsafe {
            let mut master = -1;
            let mut slave = -1;
            if libc::openpty(
                &mut master,
                &mut slave,
                std::ptr::null_mut(),
                std::ptr::null(),
                std::ptr::null(),
            ) != 0
            {
                let error = io::Error::last_os_error();
                if matches!(
                    error.kind(),
                    io::ErrorKind::PermissionDenied
                        | io::ErrorKind::NotFound
                        | io::ErrorKind::Unsupported
                ) {
                    eprintln!(
                        "skipping PTY regression because this runner cannot allocate a PTY: {error}"
                    );
                    return;
                }
                panic!("failed to allocate PTY: {error}");
            }
            (master, slave)
        };
        // SAFETY: both descriptors were returned by openpty and transfer to File.
        let mut master = unsafe { File::from_raw_fd(master) };
        // SAFETY: the slave descriptor is duplicated for stdio, then this owner closes it.
        let slave = unsafe { File::from_raw_fd(slave) };
        let executable = std::env::current_exe().unwrap();
        let mut child = Command::new(executable)
            .args(["--exact", TEST_NAME, "--nocapture"])
            .env(CHILD_FLAG, "1")
            .stdin(Stdio::from(slave.try_clone().unwrap()))
            .stdout(Stdio::from(slave.try_clone().unwrap()))
            .stderr(Stdio::from(slave.try_clone().unwrap()))
            .spawn_with_controlling_pty(slave.as_raw_fd())
            .unwrap();
        drop(slave);

        let ready = read_until(&mut master, b"READY", Duration::from_secs(5));
        assert!(
            ready.windows(5).any(|window| window == b"READY"),
            "PTY child did not reach its read: {ready:?}"
        );
        master.write_all(b"hello from pty\n").unwrap();
        let output = read_until(&mut master, b"GOT:hello from pty", Duration::from_secs(5));
        assert!(
            output
                .windows(b"GOT:hello from pty".len())
                .any(|window| window == b"GOT:hello from pty"),
            "foreground child did not receive PTY input: {output:?}"
        );
        let deadline = Instant::now() + Duration::from_secs(5);
        loop {
            if let Some(status) = child.try_wait().unwrap() {
                assert!(status.success());
                break;
            }
            if Instant::now() >= deadline {
                // SAFETY: this test process created the isolated child process group.
                unsafe {
                    libc::kill(-(child.id() as libc::pid_t), libc::SIGKILL);
                }
                let _ = child.wait();
                panic!("PTY child process did not exit after reading input");
            }
            thread::sleep(Duration::from_millis(10));
        }
    }

    trait SpawnWithControllingPty {
        fn spawn_with_controlling_pty(
            &mut self,
            slave_fd: RawFd,
        ) -> io::Result<std::process::Child>;
    }

    impl SpawnWithControllingPty for Command {
        fn spawn_with_controlling_pty(
            &mut self,
            _slave_fd: RawFd,
        ) -> io::Result<std::process::Child> {
            // SAFETY: the child becomes a new session leader and takes its redirected PTY as
            // the controlling terminal before the Rust test harness starts.
            unsafe {
                self.pre_exec(|| {
                    if libc::setsid() < 0 {
                        return Err(io::Error::last_os_error());
                    }
                    if libc::ioctl(0, libc::TIOCSCTTY as _, 0) != 0 {
                        return Err(io::Error::last_os_error());
                    }
                    if libc::tcsetpgrp(0, libc::getpgrp()) != 0 {
                        return Err(io::Error::last_os_error());
                    }
                    Ok(())
                });
            }
            self.spawn()
        }
    }

    fn read_until(master: &mut File, needle: &[u8], timeout: Duration) -> Vec<u8> {
        let deadline = Instant::now() + timeout;
        let mut output = Vec::new();
        let mut buffer = [0_u8; 1024];
        while Instant::now() < deadline {
            // SAFETY: poll reads readiness for the valid PTY master descriptor.
            let mut poll = libc::pollfd {
                fd: master.as_raw_fd(),
                events: libc::POLLIN,
                revents: 0,
            };
            let wait_ms = deadline
                .saturating_duration_since(Instant::now())
                .as_millis()
                .min(100) as libc::c_int;
            if unsafe { libc::poll(&mut poll, 1, wait_ms) } <= 0 {
                continue;
            }
            match master.read(&mut buffer) {
                Ok(0) | Err(_) => break,
                Ok(size) => output.extend_from_slice(&buffer[..size]),
            }
            if output.windows(needle.len()).any(|window| window == needle) {
                break;
            }
        }
        output
    }
}
