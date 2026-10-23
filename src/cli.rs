mod args;
mod check;
mod context;
mod control;
mod errors;
mod exec;
mod expose;
mod inject;
mod lifecycle;
mod run;
mod selection;
mod terminal;

use anyhow::Context as _;
use args::{Cli, CommandSpec};
use clap::Parser;
use context::{
    ConfigSource, Context, resolve_project_root, resolve_workspace_root, stdin_config_base_dir,
};
use selection::{SelectedRun, resolve_run};

use cladding::config::DEFAULT_COMPONENT_IMAGE;
use cladding::error::{Error, Result};
use std::io::Read as _;

pub use errors::print_error_and_exit;

const CONTAINER_HOME_DIR: &str = "/home/user";

pub fn run() -> Result<()> {
    let cli = Cli::parse();
    let command = cli.command.unwrap();
    let _metrics = match &command {
        CommandSpec::Run { verbose: true, .. } => Some(cladding::runtime_metrics::start("run")),
        _ => None,
    };

    let cwd = std::env::current_dir().with_context(|| "failed to determine current directory")?;
    if cli.run.is_some() && !command.selects_run() {
        eprintln!("error: --run applies only to runtime attachment and recovery commands");
        return Err(Error::message("unsupported --run option for command"));
    }
    if command.selects_run() {
        if cli.config.is_some() {
            eprintln!("error: runtime attachment commands do not read configuration");
            return Err(Error::message(
                "unsupported --config option for runtime command",
            ));
        }
        if cli.run.is_some() && cli.cladding_dir.is_some() {
            eprintln!("error: --run cannot be combined with --cladding-dir");
            return Err(Error::message("conflicting run selection options"));
        }
        let project_root = if cli.run.is_none() {
            Some(resolve_project_root(
                &cwd,
                cli.cladding_dir.as_ref(),
                &command,
            )?)
        } else {
            None
        };
        let allow_unreachable = matches!(&command, CommandSpec::Logs { .. } | CommandSpec::Destroy);
        let selected = resolve_run(
            cli.run.as_deref(),
            project_root.as_deref(),
            allow_unreachable,
        )?;
        return dispatch_selected(command, &selected);
    }

    if cli.config.is_some() && !command.uses_config() {
        eprintln!("error: this command does not read configuration");
        eprintln!("hint: --config applies to commands that use the Cladding configuration");
        return Err(Error::message("unsupported --config option for command"));
    }
    if cli.cladding_dir.is_some() && matches!(&command, CommandSpec::Ps) {
        eprintln!("error: 'ps' is not scoped to a project directory");
        eprintln!("hint: omit --cladding-dir when listing all discovered runs");
        return Err(Error::message("unsupported --cladding-dir option for ps"));
    }

    if matches!(command, CommandSpec::Ps) {
        return lifecycle::cmd_ps();
    }

    let selected_root = cli.cladding_dir.as_ref();
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
    let config_uses_stdin = context.config_uses_stdin();

    match command {
        CommandSpec::Build => lifecycle::cmd_build(&context),
        CommandSpec::Init { name } => lifecycle::cmd_init(&context, name.as_deref()),
        CommandSpec::Check => check::cmd_check(&context),
        CommandSpec::Run {
            args,
            env,
            secret,
            verbose,
        } => run::cmd_run(&context, &env, &secret, &args, config_uses_stdin, verbose),
        CommandSpec::Ps => lifecycle::cmd_ps(),
        _ => Err(Error::message(
            "runtime command dispatch was not initialized",
        )),
    }
}

fn dispatch_selected(command: CommandSpec, selected: &SelectedRun) -> Result<()> {
    match command {
        CommandSpec::Destroy => lifecycle::cmd_destroy_selected(selected),
        CommandSpec::Stop => lifecycle::cmd_stop_selected(selected),
        CommandSpec::Exec { target, env, args } => {
            exec::cmd_exec_selected(selected, target, &env, &args)
        }
        CommandSpec::Logs { target, args } => exec::cmd_logs_selected(selected, target, &args),
        CommandSpec::ReloadProxy => exec::cmd_reload_proxy_selected(selected),
        CommandSpec::Expose(args) => expose::cmd_expose_selected(selected, &args),
        CommandSpec::Inject(args) => inject::cmd_inject_selected(selected, &args),
        _ => Err(Error::message("command does not support run selection")),
    }
}
