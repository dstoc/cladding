use super::context::Context;
use super::{exec, lifecycle};
use anyhow::Context as _;
use cladding::error::{Error, Result};
use signal_hook::consts::signal::{SIGINT, SIGTERM};
use signal_hook::iterator::{Handle as SignalHandle, Signals};
use std::fs::{self, File};
use std::io::Read as _;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicI32, Ordering};
use std::thread::{self, JoinHandle};

pub(super) fn cmd_once(
    source_context: &Context,
    args: &[String],
    config_uses_stdin: bool,
) -> Result<()> {
    if args.is_empty() {
        return Err(Error::message("missing once command"));
    }

    let instance_name = format!("cladding-once-{}", generate_uuid_v4()?);
    println!("starting one-off instance: {instance_name}");

    let mut config = source_context.load_config().map_err(|err| {
        Error::message(format!(
            "one-off instance '{instance_name}' failed to load config: {err}"
        ))
    })?;
    config.name = instance_name.clone();

    cladding::podman::podman_required("podman (required for cladding once)").map_err(|err| {
        Error::message(format!(
            "one-off instance '{instance_name}' cannot start: {err}"
        ))
    })?;

    let mut signals = SignalWatcher::new().map_err(|err| {
        Error::message(format!(
            "one-off instance '{instance_name}' failed to install signal handlers: {err}"
        ))
    })?;

    let runtime_root = match create_private_runtime_root(
        &instance_name,
        &source_context.workspace_root,
    ) {
        Ok(root) => root,
        Err(err) => {
            let _ = signals.finish();
            return Err(Error::message(format!(
                "one-off instance '{instance_name}' failed to create its private runtime root: {err}"
            )));
        }
    };
    let context = source_context.with_resolved_config(runtime_root.clone(), config);

    if let Err(startup_error) = lifecycle::prepare_once_runtime_root(&runtime_root) {
        let root_cleanup = fs::remove_dir_all(&runtime_root).with_context(|| {
            format!(
                "failed to remove private runtime root {}",
                runtime_root.display()
            )
        });
        let signal = signals.finish();
        if let Err(cleanup_error) = root_cleanup {
            report_cleanup_failure(&instance_name, &runtime_root, &Error::from(cleanup_error));
        }
        if let Some(signal) = signal {
            eprintln!(
                "error: one-off instance '{instance_name}' failed during setup: {startup_error}"
            );
            return Err(signal_status_error(signal));
        }
        return Err(Error::message(format!(
            "one-off instance '{instance_name}' failed during setup: {startup_error}"
        )));
    }

    let (startup_result, runtime_create_attempted) = lifecycle::cmd_up_once(&context, false);

    if let Err(startup_error) = startup_result {
        let cleanup_error = if runtime_create_attempted {
            cleanup_once_runtime(&context, &runtime_root)
        } else {
            remove_private_runtime_root(&runtime_root).map_err(Error::from)
        };
        let signal = signals.finish();
        if let Err(cleanup_error) = cleanup_error {
            report_cleanup_failure(&instance_name, &runtime_root, &cleanup_error);
        }
        if let Some(signal) = signal {
            eprintln!(
                "error: one-off instance '{instance_name}' stopped during startup: {startup_error}"
            );
            return Err(signal_status_error(signal));
        }
        return Err(Error::message(format!(
            "one-off instance '{instance_name}' failed during startup: {startup_error}"
        )));
    }

    let command_result = match signals.received() {
        Some(signal) => Err(signal_status_error(signal)),
        None => exec::cmd_run_once(&context, args, !config_uses_stdin),
    };
    let cleanup_error = cleanup_once_runtime(&context, &runtime_root);
    let signal = signals.finish();

    finish_run(
        &instance_name,
        &runtime_root,
        command_result,
        cleanup_error,
        signal,
    )
}

fn cleanup_once_runtime(context: &Context, runtime_root: &Path) -> Result<()> {
    lifecycle::cmd_down_once(context)?;
    remove_private_runtime_root(runtime_root)?;
    Ok(())
}

fn remove_private_runtime_root(runtime_root: &Path) -> anyhow::Result<()> {
    fs::remove_dir_all(runtime_root).with_context(|| {
        format!(
            "failed to remove private runtime root {}",
            runtime_root.display()
        )
    })
}

fn report_cleanup_failure(instance_name: &str, runtime_root: &Path, err: &Error) {
    eprintln!("error: cleanup failed for one-off instance '{instance_name}': {err}");
    eprintln!(
        "manual cleanup may be required; private runtime root: {}",
        runtime_root.display()
    );
}

fn finish_run(
    instance_name: &str,
    runtime_root: &Path,
    command_result: Result<()>,
    cleanup_result: Result<()>,
    signal: Option<i32>,
) -> Result<()> {
    if let Err(err) = &cleanup_result {
        report_cleanup_failure(instance_name, runtime_root, err);
    }

    match command_result {
        Err(err) => Err(preserve_command_status(err, instance_name)),
        Ok(()) => match signal {
            Some(signal) => Err(signal_status_error(signal)),
            None => match cleanup_result {
                Ok(()) => Ok(()),
                Err(err) => Err(Error::message(format!(
                    "cleanup failed for one-off instance '{instance_name}': {err}"
                ))),
            },
        },
    }
}

fn preserve_command_status(err: Error, instance_name: &str) -> Error {
    match err {
        command_error @ Error::CommandFailed { .. } => command_error,
        other => Error::message(format!(
            "one-off instance '{instance_name}' failed: {other}"
        )),
    }
}

fn signal_status_error(signal: i32) -> Error {
    Error::CommandFailed {
        context: "cladding once",
        code: 128 + signal,
    }
}

fn generate_uuid_v4() -> Result<String> {
    let mut bytes = [0_u8; 16];
    File::open("/dev/urandom")
        .and_then(|mut file| file.read_exact(&mut bytes))
        .with_context(|| "failed to read random bytes for one-off instance name")?;

    bytes[6] = (bytes[6] & 0x0f) | 0x40;
    bytes[8] = (bytes[8] & 0x3f) | 0x80;

    Ok(format!(
        "{:02x}{:02x}{:02x}{:02x}-{:02x}{:02x}-{:02x}{:02x}-{:02x}{:02x}-{:02x}{:02x}{:02x}{:02x}{:02x}{:02x}",
        bytes[0],
        bytes[1],
        bytes[2],
        bytes[3],
        bytes[4],
        bytes[5],
        bytes[6],
        bytes[7],
        bytes[8],
        bytes[9],
        bytes[10],
        bytes[11],
        bytes[12],
        bytes[13],
        bytes[14],
        bytes[15]
    ))
}

fn create_private_runtime_root(instance_name: &str, workspace_root: &Path) -> Result<PathBuf> {
    let workspace_root = workspace_root
        .canonicalize()
        .unwrap_or_else(|_| workspace_root.to_path_buf());
    let mut bases = vec![std::env::temp_dir()];
    bases.extend([PathBuf::from("/var/tmp"), PathBuf::from("/dev/shm")]);

    for base in bases {
        if !base.is_dir() {
            continue;
        }
        let base = base.canonicalize().unwrap_or(base);
        let root = base.join(instance_name);
        if root.starts_with(&workspace_root) {
            continue;
        }

        #[cfg(unix)]
        {
            use std::os::unix::fs::DirBuilderExt as _;
            let mut builder = fs::DirBuilder::new();
            builder.mode(0o700);
            builder.create(&root).with_context(|| {
                format!(
                    "failed to create private runtime root {} (name collisions are not reused)",
                    root.display()
                )
            })?;
        }
        #[cfg(not(unix))]
        {
            fs::create_dir(&root).with_context(|| {
                format!(
                    "failed to create private runtime root {} (name collisions are not reused)",
                    root.display()
                )
            })?;
        }
        return Ok(root);
    }

    Err(anyhow::anyhow!(
        "no temporary directory is available outside workspace {}",
        workspace_root.display()
    )
    .into())
}

struct SignalWatcher {
    handle: SignalHandle,
    received: Arc<AtomicI32>,
    thread: Option<JoinHandle<()>>,
}

impl SignalWatcher {
    fn new() -> Result<Self> {
        let mut signals = Signals::new([SIGINT, SIGTERM])
            .with_context(|| "failed to install one-off signal handlers")?;
        let handle = signals.handle();
        let received = Arc::new(AtomicI32::new(0));
        let thread_received = Arc::clone(&received);
        let thread = thread::spawn(move || {
            if let Some(signal) = signals.forever().next() {
                thread_received.store(signal, Ordering::SeqCst);
            }
        });
        Ok(Self {
            handle,
            received,
            thread: Some(thread),
        })
    }

    fn received(&self) -> Option<i32> {
        match self.received.load(Ordering::SeqCst) {
            0 => None,
            signal => Some(signal),
        }
    }

    fn finish(&mut self) -> Option<i32> {
        self.handle.close();
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
        self.received()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn generated_instance_suffix_is_uuid_v4() {
        let uuid = generate_uuid_v4().unwrap();
        assert_eq!(uuid.len(), 36);
        assert_eq!(&uuid[8..9], "-");
        assert_eq!(&uuid[13..14], "-");
        assert_eq!(&uuid[18..19], "-");
        assert_eq!(&uuid[23..24], "-");
        assert_eq!(&uuid[14..15], "4");
        assert!(matches!(&uuid[19..20], "8" | "9" | "a" | "b"));
        assert!(
            uuid.chars()
                .all(|ch| ch.is_ascii_digit() || ('a'..='f').contains(&ch) || ch == '-')
        );
    }

    #[test]
    fn private_runtime_root_is_outside_the_source_workspace() {
        let name = format!("cladding-once-{}", generate_uuid_v4().unwrap());
        let workspace = std::env::temp_dir()
            .join(format!("cladding-once-workspace-{}", std::process::id()))
            .join(&name);
        fs::create_dir_all(&workspace).unwrap();

        let runtime_root = create_private_runtime_root(&name, &workspace).unwrap();

        assert!(!runtime_root.starts_with(&workspace));
        assert!(runtime_root.is_dir());
        let _ = fs::remove_dir_all(&runtime_root);
        let _ = fs::remove_dir_all(workspace.parent().unwrap());
    }

    #[test]
    fn command_failure_status_wins_over_cleanup_failure() {
        let error = finish_run(
            "cladding-once-test",
            Path::new("/tmp/cladding-once-test"),
            Err(Error::CommandFailed {
                context: "podman exec",
                code: 17,
            }),
            Err(Error::message("podman rm failed")),
            None,
        )
        .unwrap_err();

        assert_eq!(error.exit_code(), 17);
    }

    #[test]
    fn successful_command_returns_failure_when_cleanup_fails() {
        let error = finish_run(
            "cladding-once-test",
            Path::new("/tmp/cladding-once-test"),
            Ok(()),
            Err(Error::message("podman rm failed")),
            None,
        )
        .unwrap_err();

        assert_eq!(error.exit_code(), 1);
        assert!(error.to_string().contains("cladding-once-test"));
    }
}
