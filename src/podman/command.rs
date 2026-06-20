use crate::error::{Error, Result};
use std::ffi::OsStr;
use std::process::{Command, ExitStatus, Output};

#[derive(Debug, Clone, Copy)]
pub(super) struct PodmanRuntimeOptions {
    use_runsc: bool,
    network_none: bool,
}

impl PodmanRuntimeOptions {
    pub(super) fn new(use_runsc: bool) -> Self {
        Self {
            use_runsc,
            network_none: false,
        }
    }

    pub(super) fn with_network_none(mut self, network_none: bool) -> Self {
        self.network_none = network_none;
        self
    }
}

fn append_runtime_args_with_options(cmd: &mut Command, options: PodmanRuntimeOptions) {
    if !options.use_runsc {
        return;
    }

    cmd.arg("--runtime");
    cmd.arg("runsc");
    cmd.arg("--runtime-flag");
    cmd.arg("ignore-cgroups");
    cmd.arg("--runtime-flag");
    cmd.arg("host-uds=all");
    if options.network_none {
        cmd.arg("--runtime-flag");
        cmd.arg("network=none");
    }
}

pub(super) fn podman_command_with_options(options: PodmanRuntimeOptions) -> Command {
    let mut cmd = Command::new("podman");
    append_runtime_args_with_options(&mut cmd, options);
    cmd
}

pub fn trace_command(cmd: &Command, verbose: bool) {
    if verbose {
        eprintln!("+ {}", format_command(cmd));
    }
}

fn format_command(cmd: &Command) -> String {
    std::iter::once(cmd.get_program())
        .chain(cmd.get_args())
        .map(shell_quote)
        .collect::<Vec<_>>()
        .join(" ")
}

fn shell_quote(value: &OsStr) -> String {
    let value = value.to_string_lossy();
    if value.is_empty() {
        return "''".to_string();
    }
    if value.chars().all(|ch| {
        ch.is_ascii_alphanumeric() || matches!(ch, '/' | '.' | '_' | '-' | ':' | '=' | ',')
    }) {
        return value.into_owned();
    }

    format!("'{}'", value.replace('\'', r#"'\''"#))
}

pub fn ensure_success(status: ExitStatus, context: &'static str) -> Result<()> {
    if status.success() {
        return Ok(());
    }

    let code = status.code().unwrap_or(1);
    eprintln!("error: {context} failed (exit code {code})");
    Err(Error::CommandFailed { context, code })
}

pub fn ensure_success_output(output: &Output, context: &'static str) -> Result<()> {
    if output.status.success() {
        return Ok(());
    }

    let code = output.status.code().unwrap_or(1);
    eprintln!("error: {context} failed (exit code {code})");
    let stderr = String::from_utf8_lossy(&output.stderr);
    if !stderr.trim().is_empty() {
        eprintln!("{stderr}");
    }
    Err(Error::CommandFailed { context, code })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn command_args(cmd: &Command) -> Vec<String> {
        cmd.get_args()
            .map(|arg| arg.to_string_lossy().into_owned())
            .collect()
    }

    #[test]
    fn format_command_quotes_shell_sensitive_args() {
        let mut cmd = Command::new("podman");
        cmd.args([
            "run",
            "--name",
            "demo agent",
            "image:latest",
            "echo",
            "it's ok",
        ]);

        assert_eq!(
            format_command(&cmd),
            "podman run --name 'demo agent' image:latest echo 'it'\\''s ok'"
        );
    }

    #[test]
    fn podman_command_with_options_adds_runsc_flags_when_enabled() {
        let cmd = podman_command_with_options(PodmanRuntimeOptions::new(true));
        assert_eq!(
            command_args(&cmd),
            vec![
                "--runtime",
                "runsc",
                "--runtime-flag",
                "ignore-cgroups",
                "--runtime-flag",
                "host-uds=all",
            ]
        );
    }

    #[test]
    fn podman_command_with_options_adds_network_none_flag_when_enabled() {
        let cmd =
            podman_command_with_options(PodmanRuntimeOptions::new(true).with_network_none(true));
        assert_eq!(
            command_args(&cmd),
            vec![
                "--runtime",
                "runsc",
                "--runtime-flag",
                "ignore-cgroups",
                "--runtime-flag",
                "host-uds=all",
                "--runtime-flag",
                "network=none",
            ]
        );
    }
}
