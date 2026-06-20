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
use context::{Context, resolve_project_root};

use cladding::error::Result;

pub use errors::print_error_and_exit;

const DEFAULT_CLADDING_BUILD_IMAGE: &str = "localhost/cladding-default:latest";
const DEFAULT_CLI_BUILD_IMAGE: &str = DEFAULT_CLADDING_BUILD_IMAGE;
const DEFAULT_SANDBOX_BUILD_IMAGE: &str = DEFAULT_CLADDING_BUILD_IMAGE;
const CONTAINER_WORKSPACE_DIR: &str = "/home/user/workspace";

pub fn run() -> Result<()> {
    let cli = Cli::parse();
    let command = cli.command.unwrap();

    let cwd = std::env::current_dir().with_context(|| "failed to determine current directory")?;
    let project_root = resolve_project_root(&cwd, cli.project_root.as_ref(), &command)?;

    let context = Context { project_root };

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
