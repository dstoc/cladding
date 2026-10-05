use cladding::config::ExecutionConfig;
use clap::{ArgAction, Args, Parser, Subcommand, ValueEnum};
use std::net::IpAddr;
#[cfg(test)]
use std::path::Path;
use std::path::PathBuf;

const VERSION: &str = jj_version::jj_version!(fallback = env!("CARGO_PKG_VERSION"),);

#[derive(Parser)]
#[command(name = "cladding", version = VERSION, arg_required_else_help = true)]
pub(super) struct Cli {
    /// Select the `.cladding` directory used for runtime state and default config
    #[arg(
        long,
        global = true,
        value_name = "PATH",
        conflicts_with = "project_root"
    )]
    pub(super) cladding_dir: Option<PathBuf>,
    /// Load config from FILE, or from stdin when FILE is `-`
    #[arg(long, global = true, value_name = "FILE")]
    pub(super) config: Option<PathBuf>,
    #[arg(long, global = true, hide = true)]
    pub(super) project_root: Option<PathBuf>,
    #[command(subcommand)]
    pub(super) command: Option<CommandSpec>,
}

#[derive(Debug, Subcommand)]
pub(super) enum CommandSpec {
    /// Build local images, refresh embedded tools, and initialize the project Baffle CA
    Build,
    /// Create project config and runtime layout
    Init { name: Option<String> },
    /// Check project requirements, including the Baffle CA
    Check,
    /// Validate requirements and start the proxy and enabled execution containers
    Up {
        /// Show Podman commands before executing them
        #[arg(short, long)]
        verbose: bool,
    },
    /// Stop the system
    Down {
        /// Show Podman commands before executing them
        #[arg(short, long)]
        verbose: bool,
    },
    /// Force-remove running containers
    Destroy,
    /// Run a command in a temporary runtime, then remove the runtime
    Run {
        /// Show startup and teardown diagnostics
        #[arg(short, long)]
        verbose: bool,
        #[arg(
            long = "env",
            value_name = "KEY[=VALUE]",
            action = ArgAction::Append,
            help = "Set an environment variable for the user command. Repeat to set multiple values"
        )]
        env: Vec<String>,
        #[arg(
            value_name = "COMMAND",
            trailing_var_arg = true,
            allow_hyphen_values = true,
            required = true
        )]
        args: Vec<String>,
    },
    /// Execute a command in an already-running execution container
    Exec {
        /// Execution target. Sandbox targets run directly from the host and bypass agent-side delegation and policy checks
        #[arg(long, value_enum, default_value_t = ExecTarget::Agent)]
        target: ExecTarget,
        #[arg(
            long = "env",
            value_name = "KEY[=VALUE]",
            action = ArgAction::Append,
            help = "Set an environment variable for the user command. Repeat to set multiple values"
        )]
        env: Vec<String>,
        #[arg(
            value_name = "COMMAND",
            trailing_var_arg = true,
            allow_hyphen_values = true
        )]
        args: Vec<String>,
    },
    /// Show logs for a cladding container
    Logs {
        #[arg(value_enum)]
        target: LogsTarget,
        #[arg(trailing_var_arg = true, allow_hyphen_values = true)]
        args: Vec<String>,
    },
    /// Reload active file-backed Baffle sessions from their native TOML files
    ReloadProxy,
    /// Show running cladding projects
    Ps,
    /// Publish an agent TCP port to the host
    Expose(ExposeArgs),
    /// Connect agent localhost to a host-reachable TCP endpoint
    Inject(InjectArgs),
}

impl CommandSpec {
    pub(super) fn uses_config(&self) -> bool {
        !matches!(self, Self::Init { .. } | Self::Ps)
    }
}

#[derive(Debug, Args)]
#[command(arg_required_else_help = true)]
pub(super) struct ExposeArgs {
    #[arg(value_name = "CONTAINERPORT", value_parser = clap::value_parser!(u16).range(1..=65535))]
    pub(super) container_port: u16,
    #[arg(value_name = "HOSTPORT", value_parser = clap::value_parser!(u16).range(1..=65535))]
    pub(super) host_port: Option<u16>,
    /// Host IP address on which to listen
    #[arg(long, value_name = "ADDRESS", default_value = "127.0.0.1")]
    pub(super) bind_address: IpAddr,
}

#[derive(Debug, Args)]
#[command(arg_required_else_help = true)]
pub(super) struct InjectArgs {
    #[arg(value_name = "HOST-ENDPOINT", value_parser = parse_inject_host_endpoint)]
    pub(super) host_endpoint: InjectHostEndpoint,
    #[arg(value_name = "CONTAINERPORT", value_parser = clap::value_parser!(u16).range(1..=65535))]
    pub(super) container_port: Option<u16>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct InjectHostEndpoint {
    pub(super) host: String,
    pub(super) port: u16,
}

fn parse_inject_host_endpoint(raw: &str) -> std::result::Result<InjectHostEndpoint, String> {
    if raw.is_empty() {
        return Err("inject host endpoint cannot be empty".to_string());
    }
    if raw.chars().any(|ch| ch.is_whitespace() || ch == ',') {
        return Err(format!("invalid inject host endpoint '{raw}'"));
    }

    if let Some((host, port)) = raw.split_once(':') {
        if port.contains(':') {
            return Err(format!("invalid inject host endpoint '{raw}'"));
        }
        let host = parse_inject_host(host, raw)?;
        let port = parse_inject_port(port, raw)?;
        return Ok(InjectHostEndpoint { host, port });
    }

    let port = parse_inject_port(raw, raw)?;
    Ok(InjectHostEndpoint {
        host: "127.0.0.1".to_string(),
        port,
    })
}

fn parse_inject_host(host: &str, raw: &str) -> std::result::Result<String, String> {
    if host.is_empty() {
        return Err(format!("invalid inject host endpoint '{raw}'"));
    }
    if !host
        .chars()
        .all(|ch| ch.is_ascii_alphanumeric() || matches!(ch, '.' | '_' | '-'))
    {
        return Err(format!("invalid inject host endpoint '{raw}'"));
    }
    Ok(host.to_string())
}

fn parse_inject_port(port: &str, raw: &str) -> std::result::Result<u16, String> {
    let port = port
        .parse::<u16>()
        .ok()
        .filter(|port| *port >= 1)
        .ok_or_else(|| format!("invalid inject host endpoint '{raw}'"))?;
    Ok(port)
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum)]
#[value(rename_all = "kebab-case")]
pub(super) enum ExecTarget {
    Agent,
    NwSandbox,
    FsSandbox,
}

impl ExecTarget {
    pub(super) fn as_str(self) -> &'static str {
        match self {
            Self::Agent => "agent",
            Self::NwSandbox => "nw-sandbox",
            Self::FsSandbox => "fs-sandbox",
        }
    }

    pub(super) fn enabled(self, config: &ExecutionConfig) -> bool {
        match self {
            Self::Agent => true,
            Self::NwSandbox => config.nw_sandbox_enabled(),
            Self::FsSandbox => config.fs_sandbox_enabled(),
        }
    }

    pub(super) fn mount_target(self) -> cladding::config::MountTarget {
        match self {
            Self::Agent => cladding::config::MountTarget::Agent,
            Self::NwSandbox => cladding::config::MountTarget::NwSandbox,
            Self::FsSandbox => cladding::config::MountTarget::FsSandbox,
        }
    }

    pub(super) fn other_sandbox(self) -> Option<Self> {
        match self {
            Self::Agent => None,
            Self::NwSandbox => Some(Self::FsSandbox),
            Self::FsSandbox => Some(Self::NwSandbox),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum)]
#[value(rename_all = "kebab-case")]
pub(super) enum LogsTarget {
    Agent,
    Proxy,
    NwSandbox,
    FsSandbox,
}

impl LogsTarget {
    pub(super) fn as_str(self) -> &'static str {
        match self {
            Self::Agent => "agent",
            Self::Proxy => "proxy",
            Self::NwSandbox => "nw-sandbox",
            Self::FsSandbox => "fs-sandbox",
        }
    }

    pub(super) fn config_key(self) -> Option<&'static str> {
        match self {
            Self::Agent | Self::Proxy => None,
            Self::NwSandbox => Some("nw_sandbox"),
            Self::FsSandbox => Some("fs_sandbox"),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use clap::CommandFactory;

    #[test]
    fn version_flag_displays_version() {
        let err = match Cli::try_parse_from(["cladding", "--version"]) {
            Ok(_) => panic!("version should exit"),
            Err(err) => err,
        };
        assert_eq!(err.kind(), clap::error::ErrorKind::DisplayVersion);
    }

    #[test]
    fn help_summaries_describe_the_baffle_workflow() {
        let mut command = Cli::command();
        for (name, expected) in [
            ("run", "temporary runtime"),
            ("exec", "already-running"),
            ("init", "runtime layout"),
            ("build", "Baffle CA"),
            ("check", "Baffle CA"),
            ("up", "Validate requirements"),
            ("reload-proxy", "Baffle sessions"),
        ] {
            let summary = command
                .find_subcommand_mut(name)
                .and_then(|subcommand| subcommand.get_about())
                .unwrap_or_else(|| panic!("missing help summary for {name}"));
            assert!(
                summary.to_string().contains(expected),
                "{name} help summary does not mention {expected:?}: {summary}"
            );
        }
    }

    #[test]
    fn run_and_exec_help_describe_environment_overrides_consistently() {
        for name in ["run", "exec"] {
            let help = Cli::command()
                .find_subcommand_mut(name)
                .expect("command should exist")
                .render_help()
                .to_string();

            assert!(help.contains("--env <KEY[=VALUE]>"), "{name} help: {help}");
            assert!(
                help.contains("Repeat to set multiple values"),
                "{name} help: {help}"
            );
        }
    }

    #[test]
    fn expose_container_port_parses() {
        let cli = Cli::try_parse_from(["cladding", "expose", "3000"]).expect("cli parse");
        match cli.command.expect("command") {
            CommandSpec::Expose(args) => {
                assert_eq!(args.container_port, 3000);
                assert_eq!(args.host_port, None);
                assert_eq!(args.bind_address, "127.0.0.1".parse::<IpAddr>().unwrap());
            }
            other => panic!("unexpected command: {other:?}"),
        }
    }

    #[test]
    fn expose_container_and_host_ports_parse() {
        let cli = Cli::try_parse_from(["cladding", "expose", "3000", "9000"]).expect("cli parse");
        match cli.command.expect("command") {
            CommandSpec::Expose(args) => {
                assert_eq!(args.container_port, 3000);
                assert_eq!(args.host_port, Some(9000));
                assert_eq!(args.bind_address, "127.0.0.1".parse::<IpAddr>().unwrap());
            }
            other => panic!("unexpected command: {other:?}"),
        }
    }

    #[test]
    fn expose_bind_address_parses_ipv4_and_ipv6() {
        let ipv4 = Cli::try_parse_from([
            "cladding",
            "expose",
            "3000",
            "--bind-address",
            "192.168.1.20",
        ])
        .expect("ipv4 bind address should parse");
        match ipv4.command.expect("command") {
            CommandSpec::Expose(args) => {
                assert_eq!(args.bind_address, "192.168.1.20".parse::<IpAddr>().unwrap());
            }
            other => panic!("unexpected command: {other:?}"),
        }

        let ipv6 = Cli::try_parse_from(["cladding", "expose", "3000", "--bind-address", "::1"])
            .expect("ipv6 bind address should parse");
        match ipv6.command.expect("command") {
            CommandSpec::Expose(args) => {
                assert_eq!(args.bind_address, "::1".parse::<IpAddr>().unwrap());
            }
            other => panic!("unexpected command: {other:?}"),
        }
    }

    #[test]
    fn expose_bind_address_rejects_interface_names() {
        assert!(
            Cli::try_parse_from(["cladding", "expose", "3000", "--bind-address", "eth0",]).is_err()
        );
    }

    #[test]
    fn up_and_down_verbose_flags_parse() {
        let up = Cli::try_parse_from(["cladding", "up", "-v"]).expect("cli parse");
        match up.command.expect("command") {
            CommandSpec::Up { verbose } => assert!(verbose),
            other => panic!("unexpected command: {other:?}"),
        }

        let down = Cli::try_parse_from(["cladding", "down", "--verbose"]).expect("cli parse");
        match down.command.expect("command") {
            CommandSpec::Down { verbose } => assert!(verbose),
            other => panic!("unexpected command: {other:?}"),
        }
    }

    #[test]
    fn shared_directory_and_config_options_parse_before_and_after_command() {
        let before = Cli::try_parse_from([
            "cladding",
            "--cladding-dir",
            "/tmp/app/.cladding",
            "--config",
            "./custom.json",
            "up",
        ])
        .expect("global options before command should parse");
        assert_eq!(
            before.cladding_dir.as_deref(),
            Some(Path::new("/tmp/app/.cladding"))
        );
        assert_eq!(before.config.as_deref(), Some(Path::new("./custom.json")));
        assert!(matches!(before.command, Some(CommandSpec::Up { .. })));

        let after = Cli::try_parse_from([
            "cladding",
            "up",
            "--cladding-dir",
            "/tmp/app/.cladding",
            "--config",
            "-",
        ])
        .expect("global options after command should parse");
        assert_eq!(after.config.as_deref(), Some(Path::new("-")));
        assert_eq!(
            after.cladding_dir.as_deref(),
            Some(Path::new("/tmp/app/.cladding"))
        );
    }

    #[test]
    fn exec_arguments_remain_positional_after_global_options() {
        let cli = Cli::try_parse_from([
            "cladding",
            "--config",
            "custom.json",
            "exec",
            "echo",
            "--config",
        ])
        .expect("exec command arguments should remain positional");
        assert_eq!(cli.config.as_deref(), Some(Path::new("custom.json")));
        match cli.command.expect("command") {
            CommandSpec::Exec { args, .. } => assert_eq!(args, ["echo", "--config"]),
            other => panic!("unexpected command: {other:?}"),
        }
    }

    #[test]
    fn run_takes_a_command_and_its_arguments() {
        for argv in [
            vec!["cladding", "run", "codex", "exec", "--full-auto"],
            vec!["cladding", "run", "--", "codex", "exec", "--full-auto"],
        ] {
            let cli = Cli::try_parse_from(argv).expect("run command should parse");
            match cli.command.expect("command") {
                CommandSpec::Run { args, env, verbose } => {
                    assert_eq!(args, ["codex", "exec", "--full-auto"]);
                    assert!(!verbose);
                    assert!(env.is_empty());
                }
                other => panic!("unexpected command: {other:?}"),
            }
        }

        assert!(Cli::try_parse_from(["cladding", "run"]).is_err());
        assert!(Cli::try_parse_from(["cladding", "once", "--", "codex"]).is_err());
    }

    #[test]
    fn run_accepts_verbose_before_the_command_delimiter() {
        for flag in ["-v", "--verbose"] {
            let cli = Cli::try_parse_from(["cladding", "run", flag, "--", "echo", "hello"])
                .expect("run verbose flag should parse");
            match cli.command.expect("command") {
                CommandSpec::Run { args, env, verbose } => {
                    assert!(verbose);
                    assert_eq!(args, ["echo", "hello"]);
                    assert!(env.is_empty());
                }
                other => panic!("unexpected command: {other:?}"),
            }
        }
    }

    #[test]
    fn run_and_exec_accept_repeated_environment_overrides() {
        for name in ["run", "exec"] {
            let cli = Cli::try_parse_from([
                "cladding", name, "--env", "FOO=bar", "--env", "BAZ", "--", "env",
            ])
            .expect("repeated environment options should parse");

            let (env, args) = match cli.command.expect("command") {
                CommandSpec::Run { env, args, .. } | CommandSpec::Exec { env, args, .. } => {
                    (env, args)
                }
                other => panic!("unexpected command: {other:?}"),
            };
            assert_eq!(env, ["FOO=bar", "BAZ"]);
            assert_eq!(args, ["env"]);
        }
    }

    #[test]
    fn run_and_exec_reject_environment_options_without_a_value() {
        for name in ["run", "exec"] {
            assert!(
                Cli::try_parse_from(["cladding", name, "--env", "--", "env"]).is_err(),
                "{name} should reject --env without a value"
            );
        }
    }

    #[test]
    fn expose_without_ports_fails() {
        assert!(Cli::try_parse_from(["cladding", "expose"]).is_err());
    }

    #[test]
    fn expose_list_fails_to_parse() {
        assert!(Cli::try_parse_from(["cladding", "expose", "list"]).is_err());
    }

    #[test]
    fn expose_stop_fails_to_parse() {
        assert!(Cli::try_parse_from(["cladding", "expose", "stop", "9000"]).is_err());
    }

    #[test]
    fn inject_bare_port_parses_with_default_host() {
        let cli = Cli::try_parse_from(["cladding", "inject", "11434"]).expect("cli parse");
        match cli.command.expect("command") {
            CommandSpec::Inject(args) => {
                assert_eq!(
                    args.host_endpoint,
                    InjectHostEndpoint {
                        host: "127.0.0.1".to_string(),
                        port: 11434,
                    }
                );
                assert_eq!(args.container_port, None);
            }
            other => panic!("unexpected command: {other:?}"),
        }
    }

    #[test]
    fn inject_host_and_container_ports_parse() {
        let cli = Cli::try_parse_from(["cladding", "inject", "db.internal:5432", "15432"])
            .expect("cli parse");
        match cli.command.expect("command") {
            CommandSpec::Inject(args) => {
                assert_eq!(
                    args.host_endpoint,
                    InjectHostEndpoint {
                        host: "db.internal".to_string(),
                        port: 5432,
                    }
                );
                assert_eq!(args.container_port, Some(15432));
            }
            other => panic!("unexpected command: {other:?}"),
        }
    }

    #[test]
    fn inject_default_container_port_matches_host_port() {
        let cli =
            Cli::try_parse_from(["cladding", "inject", "db.internal:5432"]).expect("cli parse");
        match cli.command.expect("command") {
            CommandSpec::Inject(args) => {
                assert_eq!(args.container_port.unwrap_or(args.host_endpoint.port), 5432);
            }
            other => panic!("unexpected command: {other:?}"),
        }
    }

    #[test]
    fn inject_list_and_stop_fails_to_parse() {
        assert!(Cli::try_parse_from(["cladding", "inject", "list"]).is_err());
        assert!(Cli::try_parse_from(["cladding", "inject", "stop", "9000"]).is_err());
    }

    #[test]
    fn inject_invalid_endpoints_fail_to_parse() {
        for argv in [
            ["cladding", "inject", "db.internal:5432:1"],
            ["cladding", "inject", "db,internal:5432"],
            ["cladding", "inject", "db internal:5432"],
            ["cladding", "inject", ":5432"],
            ["cladding", "inject", "[::1]:5432"],
            ["cladding", "inject", "db.internal:0"],
        ] {
            assert!(Cli::try_parse_from(argv).is_err(), "argv={argv:?}");
        }
    }

    #[test]
    fn init_update_scripts_fails_to_parse() {
        assert!(Cli::try_parse_from(["cladding", "init", "--update-scripts"]).is_err());
    }

    #[test]
    fn exec_defaults_to_agent_and_accepts_each_closed_target() {
        let default = Cli::try_parse_from(["cladding", "exec", "echo", "hello"])
            .expect("default exec target should parse");
        match default.command.expect("command") {
            CommandSpec::Exec { target, args, .. } => {
                assert_eq!(target, ExecTarget::Agent);
                assert_eq!(args, ["echo", "hello"]);
            }
            other => panic!("unexpected command: {other:?}"),
        }

        for (value, expected) in [
            ("agent", ExecTarget::Agent),
            ("nw-sandbox", ExecTarget::NwSandbox),
            ("fs-sandbox", ExecTarget::FsSandbox),
        ] {
            let cli =
                Cli::try_parse_from(["cladding", "exec", "--target", value, "echo", "--flag"])
                    .expect("valid exec target should parse");

            match cli.command.expect("command") {
                CommandSpec::Exec { target, args, .. } => {
                    assert_eq!(target, expected);
                    assert_eq!(args, ["echo", "--flag"]);
                }
                other => panic!("unexpected command: {other:?}"),
            }
        }

        assert!(Cli::try_parse_from(["cladding", "exec", "--target", "proxy", "true"]).is_err());
        assert!(Cli::try_parse_from(["cladding", "exec", "--target", "unknown", "true"]).is_err());
    }

    #[test]
    fn logs_target_and_passthrough_args_parse() {
        let cli = Cli::try_parse_from(["cladding", "logs", "fs-sandbox", "-f", "--since", "10m"])
            .expect("cli parse");

        match cli.command.expect("command") {
            CommandSpec::Logs { target, args } => {
                assert_eq!(target, LogsTarget::FsSandbox);
                assert_eq!(args, vec!["-f", "--since", "10m"]);
            }
            other => panic!("unexpected command: {other:?}"),
        }
    }
}
