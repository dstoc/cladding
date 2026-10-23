use std::cell::RefCell;
use std::collections::BTreeMap;
use std::ffi::OsStr;
use std::process::{Command, ExitStatus, Output};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

#[derive(Debug, Clone, Copy)]
enum OperationGroup {
    DiscoveryInspect,
    CreateStartExec,
    Removal,
    Helper,
}

impl OperationGroup {
    const ALL: [Self; 4] = [
        Self::DiscoveryInspect,
        Self::CreateStartExec,
        Self::Removal,
        Self::Helper,
    ];

    fn label(self) -> &'static str {
        match self {
            Self::DiscoveryInspect => "discovery/inspect",
            Self::CreateStartExec => "create/start/exec",
            Self::Removal => "removal",
            Self::Helper => "helpers/other",
        }
    }
}

#[derive(Default)]
struct MetricsData {
    phases: BTreeMap<&'static str, Duration>,
    operations: BTreeMap<&'static str, (usize, Duration)>,
    runtime_spec_builds: usize,
    custom_mount_paths: usize,
}

thread_local! {
    static ACTIVE: RefCell<Option<Arc<Mutex<MetricsData>>>> = const { RefCell::new(None) };
    static PENDING_SUMMARY: RefCell<Option<String>> = const { RefCell::new(None) };
}

/// Collect transient lifecycle timing data for one verbose runtime command.
pub struct RuntimeMetricsGuard {
    label: &'static str,
    started: Instant,
    data: Arc<Mutex<MetricsData>>,
    previous: Option<Arc<Mutex<MetricsData>>>,
}

#[derive(Clone)]
pub struct RuntimeMetricsContext(Arc<Mutex<MetricsData>>);

/// Start collecting runtime metrics. The caller prints the completed summary.
pub fn start(label: &'static str) -> RuntimeMetricsGuard {
    let data = Arc::new(Mutex::new(MetricsData::default()));
    let previous = ACTIVE.with(|active| active.replace(Some(Arc::clone(&data))));
    RuntimeMetricsGuard {
        label,
        started: Instant::now(),
        data,
        previous,
    }
}

/// Time a named phase of the active runtime command.
pub fn phase(name: &'static str) -> PhaseGuard {
    let data = ACTIVE.with(|active| active.borrow().as_ref().map(Arc::clone));
    PhaseGuard {
        name,
        started: Instant::now(),
        data,
    }
}

/// Record one prepared runtime spec and its configured host mount paths.
pub fn record_runtime_spec_build(custom_mount_paths: usize) {
    let Some(data) = ACTIVE.with(|active| active.borrow().as_ref().map(Arc::clone)) else {
        return;
    };
    let mut data = data.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
    data.runtime_spec_builds += 1;
    data.custom_mount_paths += custom_mount_paths;
}

pub struct PhaseGuard {
    name: &'static str,
    started: Instant,
    data: Option<Arc<Mutex<MetricsData>>>,
}

impl Drop for PhaseGuard {
    fn drop(&mut self) {
        let Some(data) = &self.data else {
            return;
        };
        let mut data = data.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
        *data.phases.entry(self.name).or_default() += self.started.elapsed();
    }
}

impl Drop for RuntimeMetricsGuard {
    fn drop(&mut self) {
        let summary = format_summary(self.label, self.started.elapsed(), &self.data);
        ACTIVE.with(|active| {
            active.replace(self.previous.take());
        });
        PENDING_SUMMARY.with(|pending| {
            *pending.borrow_mut() = Some(summary);
        });
    }
}

/// Print the last completed command summary, if one was collected.
pub fn print_pending_summary() {
    PENDING_SUMMARY.with(|pending| {
        if let Some(summary) = pending.borrow_mut().take() {
            eprintln!("{summary}");
        }
    });
}

/// Run a child command and include its wall-clock duration in the active summary.
pub fn output(command: &mut Command) -> std::io::Result<Output> {
    let context = capture_context();
    output_with_context(command, context.as_ref())
}

/// Run a child command with a captured metrics context, including on a worker thread.
pub fn output_with_context(
    command: &mut Command,
    context: Option<&RuntimeMetricsContext>,
) -> std::io::Result<Output> {
    let started = context.map(|_| Instant::now());
    let result = command.output();
    if let (Some(context), Some(started), Ok(_)) = (context, started, &result) {
        record_command_in_context(command, started.elapsed(), context);
    }
    result
}

/// Run a child command and include its wall-clock duration in the active summary.
pub fn status(command: &mut Command) -> std::io::Result<ExitStatus> {
    let context = capture_context();
    status_with_context(command, context.as_ref())
}

/// Run a child command with a captured metrics context, including on a worker thread.
pub fn status_with_context(
    command: &mut Command,
    context: Option<&RuntimeMetricsContext>,
) -> std::io::Result<ExitStatus> {
    let started = context.map(|_| Instant::now());
    let result = command.status();
    if let (Some(context), Some(started), Ok(_)) = (context, started, &result) {
        record_command_in_context(command, started.elapsed(), context);
    }
    result
}

/// Record a command that was spawned separately and measured through its wait.
pub fn record_command_elapsed(command: &Command, elapsed: Duration) {
    let Some(context) = capture_context() else {
        return;
    };
    record_command_in_context(command, elapsed, &context);
}

/// Capture the active summary for a worker thread that may run a helper command.
pub fn capture_context() -> Option<RuntimeMetricsContext> {
    active_metrics().map(RuntimeMetricsContext)
}

fn record_command_in_context(
    command: &Command,
    elapsed: Duration,
    context: &RuntimeMetricsContext,
) {
    let group = classify(command);
    let mut data = context
        .0
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    let entry = data.operations.entry(group.label()).or_default();
    entry.0 += 1;
    entry.1 += elapsed;
}

fn active_metrics() -> Option<Arc<Mutex<MetricsData>>> {
    ACTIVE.with(|active| active.borrow().as_ref().map(Arc::clone))
}

fn classify(command: &Command) -> OperationGroup {
    let executable = command.get_program();
    if path_basename(executable) != "podman" {
        return OperationGroup::Helper;
    }

    let args = command
        .get_args()
        .map(|arg| arg.to_string_lossy())
        .collect::<Vec<_>>();
    let mut index = 0;
    while let Some(argument) = args.get(index).map(|arg| arg.as_ref()) {
        match argument {
            "--runtime" | "--runtime-flag" | "--connection" | "--identity" | "--url"
            | "--log-level" => index += 2,
            value if value.starts_with('-') => index += 1,
            _ => break,
        }
    }

    let command = args.get(index).map(|arg| arg.as_ref()).unwrap_or("");
    let subcommand = args.get(index + 1).map(|arg| arg.as_ref()).unwrap_or("");
    match (command, subcommand) {
        ("container", "ps" | "ls" | "exists" | "inspect")
        | ("volume", "ls" | "exists" | "inspect")
        | ("pod", "ps" | "exists" | "inspect") => OperationGroup::DiscoveryInspect,
        ("volume" | "container" | "pod", "rm" | "stop" | "kill") => OperationGroup::Removal,
        ("volume", "create") => OperationGroup::CreateStartExec,
        ("image", "exists") => OperationGroup::DiscoveryInspect,
        ("info" | "ps" | "inspect" | "exists" | "images", _) => OperationGroup::DiscoveryInspect,
        ("rm" | "stop" | "kill", _) => OperationGroup::Removal,
        ("run" | "create" | "start" | "exec" | "wait" | "attach", _) => {
            OperationGroup::CreateStartExec
        }
        _ => OperationGroup::Helper,
    }
}

fn path_basename(path: &OsStr) -> &str {
    path.to_str()
        .unwrap_or_default()
        .rsplit('/')
        .next()
        .unwrap_or_default()
}

fn format_summary(label: &str, total: Duration, data: &Arc<Mutex<MetricsData>>) -> String {
    let data = data.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
    let phase_labels: &[(&str, &str)] = match label {
        "run" => &[
            ("preparation/preflight", "prep"),
            ("runtime startup", "startup"),
            ("user command", "command"),
            ("cleanup", "cleanup"),
        ],
        _ => &[],
    };

    let mut summary = format!("runtime metrics: {label} total={}", format_duration(total));
    for (phase_name, phase_label) in phase_labels {
        let elapsed = data.phases.get(*phase_name).copied().unwrap_or_default();
        summary.push_str(&format!(" {phase_label}={}", format_duration(elapsed)));
    }
    summary.push_str(&format!(
        " | preparation runtime-specs={} custom-mount-paths-prepared={}",
        data.runtime_spec_builds, data.custom_mount_paths
    ));
    summary.push_str(" | podman");
    for group in OperationGroup::ALL {
        let (count, elapsed) = data
            .operations
            .get(group.label())
            .copied()
            .unwrap_or_default();
        summary.push_str(&format!(
            " {}={count}/{}",
            group.label(),
            format_duration(elapsed)
        ));
    }
    summary
}

fn format_duration(duration: Duration) -> String {
    format!("{:.3}s", duration.as_secs_f64())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use std::os::unix::fs::PermissionsExt as _;

    #[test]
    fn fake_child_calls_are_counted_and_include_wall_clock_duration() {
        let root = std::env::temp_dir().join(format!(
            "cladding-runtime-metrics-{}-{}",
            std::process::id(),
            Instant::now().elapsed().as_nanos()
        ));
        fs::create_dir_all(&root).unwrap();
        let helper = root.join("podman");
        fs::write(&helper, "#!/bin/sh\nsleep 0.06\nexit 0\n").unwrap();
        fs::set_permissions(&helper, fs::Permissions::from_mode(0o755)).unwrap();

        let metrics = start("run");
        {
            let _phase = phase("user command");
            let mut command = Command::new(&helper);
            command.args(["exec", "demo", "echo", "a-secret-value"]);
            output(&mut command).unwrap();
        }
        drop(metrics);

        let summary = PENDING_SUMMARY.with(|pending| pending.borrow_mut().take().unwrap());
        assert!(summary.contains("runtime metrics: run total="));
        assert!(summary.contains("command="));
        assert!(summary.contains("create/start/exec=1/0.0"));
        assert!(!summary.contains("a-secret-value"));
        assert!(!summary.contains("echo"));
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn summary_reports_runtime_spec_and_custom_mount_preparation_counts() {
        let metrics = start("run");
        record_runtime_spec_build(2);
        record_runtime_spec_build(1);
        drop(metrics);

        let summary = PENDING_SUMMARY.with(|pending| pending.borrow_mut().take().unwrap());
        assert!(summary.contains("preparation runtime-specs=2 custom-mount-paths-prepared=3"));
    }

    #[test]
    fn operation_classifier_handles_runtime_flags_and_user_exec_arguments() {
        let mut command = Command::new("podman");
        command.args([
            "--runtime",
            "runsc",
            "--runtime-flag",
            "host-uds=all",
            "exec",
            "container",
            "rm",
        ]);
        assert_eq!(classify(&command).label(), "create/start/exec");

        let mut command = Command::new("podman");
        command.args(["volume", "rm", "volume-name"]);
        assert_eq!(classify(&command).label(), "removal");
    }
}
