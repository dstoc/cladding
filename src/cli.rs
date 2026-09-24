mod args;
mod check;
mod context;
mod errors;
mod exec;
mod expose;
mod inject;
mod lifecycle;

use anyhow::Context as _;
use args::{Cli, CommandSpec};
use clap::Parser;
use context::{
    ConfigSource, Context, resolve_project_root, resolve_workspace_root, stdin_config_base_dir,
};

use cladding::error::{Error, Result};
use std::io::Read as _;

pub use errors::print_error_and_exit;

const DEFAULT_CLADDING_BUILD_IMAGE: &str = "localhost/cladding-default:latest";
const DEFAULT_CLI_BUILD_IMAGE: &str = DEFAULT_CLADDING_BUILD_IMAGE;
const DEFAULT_SANDBOX_BUILD_IMAGE: &str = DEFAULT_CLADDING_BUILD_IMAGE;
const CONTAINER_HOME_DIR: &str = "/home/user";
const CONTAINER_WORKSPACE_DIR: &str = "/home/user/workspace";

pub fn run() -> Result<()> {
    let cli = Cli::parse();
    let command = cli.command.unwrap();

    let cwd = std::env::current_dir().with_context(|| "failed to determine current directory")?;
    if cli.config.is_some() && !command.uses_config() {
        eprintln!("error: this command does not read configuration");
        eprintln!("hint: --config applies to commands that use the Cladding configuration");
        return Err(Error::message("unsupported --config option for command"));
    }
    if cli.cladding_dir.is_some() && matches!(&command, CommandSpec::Ps) {
        eprintln!("error: 'ps' is not scoped to a project directory");
        eprintln!("hint: omit --cladding-dir when listing all running projects");
        return Err(Error::message("unsupported --cladding-dir option for ps"));
    }

    let selected_root = cli.cladding_dir.as_ref().or(cli.project_root.as_ref());
    let project_root = resolve_project_root(&cwd, selected_root, &command)?;
    let workspace_root = resolve_workspace_root(&cwd);

    let config_source = match cli.config {
        None => ConfigSource::Default,
        Some(path) if path == std::path::Path::new("-") => {
            let base_dir = stdin_config_base_dir(&cwd, cli.cladding_dir.as_deref());
            let mut raw = String::new();
            std::io::stdin()
                .read_to_string(&mut raw)
                .with_context(|| "failed to read configuration from stdin")?;
            ConfigSource::Stdin { raw, base_dir }
        }
        Some(path) => ConfigSource::File(path),
    };

    let context = Context::new(project_root, workspace_root, config_source);

    match command {
        CommandSpec::Build => lifecycle::cmd_build(&context),
        CommandSpec::Init { name } => lifecycle::cmd_init(&context, name.as_deref()),
        CommandSpec::Check => check::cmd_check(&context),
        CommandSpec::Up { verbose } => lifecycle::cmd_up(&context, verbose),
        CommandSpec::Down { verbose } => lifecycle::cmd_down(&context, verbose),
        CommandSpec::Destroy => lifecycle::cmd_destroy(&context),
        CommandSpec::Run { env, args } => exec::cmd_run(&context, &env, &args),
        CommandSpec::RunWithScissors { target, env, args } => {
            exec::cmd_run_with_scissors(&context, target, &env, &args)
        }
        CommandSpec::Logs { target, args } => exec::cmd_logs(&context, target, &args),
        CommandSpec::ReloadProxy => exec::cmd_reload_proxy(&context),
        CommandSpec::Ps => lifecycle::cmd_ps(&context),
        CommandSpec::Expose(args) => expose::cmd_expose(&context, &args),
        CommandSpec::Inject(args) => inject::cmd_inject(&context, &args),
    }
}
