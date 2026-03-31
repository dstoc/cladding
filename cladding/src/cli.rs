use anyhow::Context as _;
use cladding::assets::{
    config_top_level_entries, materialize_config, materialize_scripts, materialize_scripts_force,
    scripts_files, scripts_top_level_entries, write_embedded_tools,
};
use cladding::config::{Config, load_cladding_config, write_default_cladding_config};
use cladding::error::{Error, Result};
use cladding::fs_utils::{canonicalize_path, is_broken_symlink, is_executable, path_is_symlink};
use cladding::network::{parse_cladding_pool_index, resolve_network_settings};
use cladding::podman::{
    EnsureNetworkOutcome, ensure_pool_network_settings, list_podman_network_subnets,
    list_project_expose_proxies, list_running_project_networks, list_running_projects,
    podman_build_image, podman_container_exists, podman_play_kube, podman_remove_containers,
    podman_required,
};
use cladding::pods::{host_paths_from_rendered, render_pods_yaml};
use clap::{ArgAction, Args, Parser, Subcommand};
use std::env;
use std::fs;
use std::io::{self, IsTerminal};
use std::net::TcpListener;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::thread;
use std::time::{SystemTime, UNIX_EPOCH};
use signal_hook::consts::signal::{SIGINT, SIGTERM};
use signal_hook::iterator::Signals;

const DEFAULT_CLADDING_BUILD_IMAGE: &str = "localhost/cladding-default:latest";
const DEFAULT_CLI_BUILD_IMAGE: &str = DEFAULT_CLADDING_BUILD_IMAGE;
const DEFAULT_SANDBOX_BUILD_IMAGE: &str = DEFAULT_CLADDING_BUILD_IMAGE;

#[derive(Debug, Clone)]
struct Context {
    project_root: PathBuf,
}

#[derive(Parser)]
#[command(name = "cladding", arg_required_else_help = true)]
struct Cli {
    #[arg(long, global = true, hide = true)]
    project_root: Option<PathBuf>,
    #[command(subcommand)]
    command: Option<CommandSpec>,
}

#[derive(Debug, Subcommand)]
enum CommandSpec {
    /// Build local container images
    Build,
    /// Create config and default mount directories
    Init {
        name: Option<String>,
        /// Overwrite scripts with embedded defaults
        #[arg(long)]
        update_scripts: bool,
    },
    /// Check requirements
    Check,
    /// Start the system
    Up,
    /// Stop the system
    Down,
    /// Force-remove running containers
    Destroy,
    /// Run a command in the cli container
    Run {
        #[arg(long = "env", value_name = "KEY[=VALUE]", action = ArgAction::Append)]
        env: Vec<String>,
        #[arg(trailing_var_arg = true, allow_hyphen_values = true)]
        args: Vec<String>,
    },
    /// Run a command in the sandbox container
    RunWithScissors {
        #[arg(long = "env", value_name = "KEY[=VALUE]", action = ArgAction::Append)]
        env: Vec<String>,
        #[arg(trailing_var_arg = true, allow_hyphen_values = true)]
        args: Vec<String>,
    },
    /// Reload the squid proxy configuration
    ReloadProxy,
    /// Show running cladding projects
    Ps,
    /// Publish a cli-app TCP port to the host
    Expose(ExposeArgs),
}

#[derive(Debug, Args)]
#[command(args_conflicts_with_subcommands = true, arg_required_else_help = true)]
struct ExposeArgs {
    #[command(subcommand)]
    command: Option<ExposeSubcommand>,
    #[arg(value_name = "CONTAINERPORT", value_parser = clap::value_parser!(u16).range(1..=65535))]
    container_port: Option<u16>,
    #[arg(
        value_name = "HOSTPORT",
        value_parser = clap::value_parser!(u16).range(1..=65535),
        requires = "container_port"
    )]
    host_port: Option<u16>,
}

#[derive(Debug, Subcommand)]
enum ExposeSubcommand {
    /// Remove a published host port for the current project
    Stop {
        #[arg(value_name = "HOSTPORT", value_parser = clap::value_parser!(u16).range(1..=65535))]
        host_port: u16,
    },
    /// List published host ports for the current project
    List,
}

pub fn run() -> Result<()> {
    let cli = Cli::parse();
    let command = cli.command.unwrap();

    let cwd = env::current_dir().with_context(|| "failed to determine current directory")?;
    let project_root = resolve_project_root(&cwd, cli.project_root.as_ref(), &command)?;

    let context = Context { project_root };

    match command {
        CommandSpec::Build => cmd_build(&context),
        CommandSpec::Init {
            name,
            update_scripts,
        } => cmd_init(&context, name.as_deref(), update_scripts),
        CommandSpec::Check => cmd_check(&context),
        CommandSpec::Up => cmd_up(&context),
        CommandSpec::Down => cmd_down(&context),
        CommandSpec::Destroy => cmd_destroy(&context),
        CommandSpec::Run { env, args } => cmd_run(&context, &env, &args),
        CommandSpec::RunWithScissors { env, args } => cmd_run_with_scissors(&context, &env, &args),
        CommandSpec::ReloadProxy => cmd_reload_proxy(&context),
        CommandSpec::Ps => cmd_ps(&context),
        CommandSpec::Expose(args) => cmd_expose(&context, &args),
    }
}

pub fn print_error_and_exit(err: Error) -> ! {
    eprintln!("{err}");
    std::process::exit(err.exit_code());
}

fn find_project_root(start: &Path) -> Option<PathBuf> {
    let mut current = start;
    loop {
        let candidate = current.join(".cladding");
        if candidate.is_dir() {
            return Some(candidate);
        }
        match current.parent() {
            Some(parent) => current = parent,
            None => return None,
        }
    }
}

fn resolve_project_root(
    cwd: &Path,
    override_root: Option<&PathBuf>,
    command: &CommandSpec,
) -> Result<PathBuf> {
    if let Some(root) = override_root {
        return Ok(root.to_path_buf());
    }

    match find_project_root(cwd) {
        Some(root) => Ok(root),
        None => match command {
            CommandSpec::Init { .. } => Ok(cwd.join(".cladding")),
            CommandSpec::Ps => Ok(cwd.join(".cladding")),
            _ => {
                eprintln!(
                    "error: no .cladding directory found in {} or any parent directory",
                    cwd.display()
                );
                eprintln!("hint: run 'cladding init' from the project directory to create one");
                Err(Error::message("missing .cladding"))
            }
        },
    }
}

fn cmd_build(context: &Context) -> Result<()> {
    let config = load_cladding_config(&context.project_root)?;

    let host_uid = unsafe { libc::getuid() };
    let host_gid = unsafe { libc::getgid() };

    let tools_dir = context.project_root.join("tools");
    if is_broken_symlink(&tools_dir)? {
        eprintln!("missing: tools (broken symlink at {})", tools_dir.display());
        eprintln!("hint: create or relink {}", tools_dir.display());
        return Err(Error::message("missing tools"));
    }

    let tools_bin_dir = tools_dir.join("bin");
    fs::create_dir_all(&tools_bin_dir).with_context(|| "failed to create tools directory")?;

    write_embedded_tools(&tools_bin_dir)?;

    let mut cli_image_built = false;
    if config.cli_image == DEFAULT_CLI_BUILD_IMAGE {
        podman_build_image(&config.cli_image, host_uid, host_gid)?;
        cli_image_built = true;
    } else {
        println!(
            "skip: not building cli image (config cli_image is {}, build target is {})",
            config.cli_image, DEFAULT_CLADDING_BUILD_IMAGE
        );
    }

    if config.sandbox_image == DEFAULT_SANDBOX_BUILD_IMAGE {
        if config.sandbox_image == config.cli_image && cli_image_built {
            println!(
                "skip: sandbox image already built (config cli_image and sandbox_image are both {})",
                config.sandbox_image
            );
        } else {
            podman_build_image(&config.sandbox_image, host_uid, host_gid)?;
        }
    } else {
        println!(
            "skip: not building sandbox image (config sandbox_image is {}, build target is {})",
            config.sandbox_image, DEFAULT_CLADDING_BUILD_IMAGE
        );
    }

    Ok(())
}

fn cmd_init(context: &Context, name_override: Option<&str>, update_scripts: bool) -> Result<()> {
    let project_root = &context.project_root;
    let config_dir = project_root.join("config");
    let scripts_dir = project_root.join("scripts");
    let home_dir = project_root.join("home");
    let tools_dir = project_root.join("tools");
    let cladding_config = project_root.join("cladding.json");
    let cladding_gitignore = project_root.join(".gitignore");

    if project_root.exists() && !project_root.is_dir() {
        eprintln!(
            "error: .cladding path exists but is not a directory: {}",
            project_root.display()
        );
        return Err(Error::message("invalid .cladding path"));
    }

    let project_root_created = !project_root.exists();
    fs::create_dir_all(project_root)
        .with_context(|| format!("failed to create {}", project_root.display()))?;

    if project_root_created {
        fs::write(&cladding_gitignore, "*\n")
            .with_context(|| format!("failed to write {}", cladding_gitignore.display()))?;
    }

    if config_dir.exists() || path_is_symlink(&config_dir) {
        println!("config already exists: {}", config_dir.display());
    } else {
        fs::create_dir_all(&config_dir)
            .with_context(|| format!("failed to create {}", config_dir.display()))?;
        println!("initialized: {}", config_dir.display());
    }

    materialize_config(&config_dir)?;

    if scripts_dir.exists() || path_is_symlink(&scripts_dir) {
        println!("scripts already exists: {}", scripts_dir.display());
    } else {
        fs::create_dir_all(&scripts_dir)
            .with_context(|| format!("failed to create {}", scripts_dir.display()))?;
        println!("initialized: {}", scripts_dir.display());
    }

    if home_dir.exists() || path_is_symlink(&home_dir) {
        println!("home already exists: {}", home_dir.display());
    } else {
        fs::create_dir_all(&home_dir)
            .with_context(|| format!("failed to create {}", home_dir.display()))?;
        println!("initialized: {}", home_dir.display());
    }

    if tools_dir.exists() || path_is_symlink(&tools_dir) {
        println!("tools already exists: {}", tools_dir.display());
    } else {
        fs::create_dir_all(&tools_dir)
            .with_context(|| format!("failed to create {}", tools_dir.display()))?;
        println!("initialized: {}", tools_dir.display());
    }

    if update_scripts {
        materialize_scripts_force(&scripts_dir)?;
    } else {
        materialize_scripts(&scripts_dir)?;
    }

    if cladding_config.exists() {
        println!(
            "cladding config already exists: {}",
            cladding_config.display()
        );
    } else {
        let generated = write_default_cladding_config(
            name_override,
            DEFAULT_SANDBOX_BUILD_IMAGE,
            DEFAULT_CLI_BUILD_IMAGE,
        )?;
        fs::write(&cladding_config, generated)
            .with_context(|| format!("failed to write {}", cladding_config.display()))?;
        println!("generated: {}", cladding_config.display());
    }

    Ok(())
}

fn cmd_check(context: &Context) -> Result<()> {
    check_required_binaries(context)?;
    let config = load_cladding_config(&context.project_root)?;
    let network_settings = resolve_network_settings(&config.name, 0)?;
    check_required_host_paths(context, &config, &network_settings)?;
    check_required_config_files(context)?;
    check_required_scripts_files(context)?;
    check_required_images(&config)?;
    println!("check: ok");
    Ok(())
}

fn check_required_binaries(context: &Context) -> Result<()> {
    let mut missing = false;
    let bin_dir = context.project_root.join("tools/bin");

    for name in ["mcp-run", "run-with-network"] {
        let path = bin_dir.join(name);
        if !is_executable(&path) {
            eprintln!("missing: tools/bin/{name} ({})", path.display());
            eprintln!("hint: run cladding build");
            missing = true;
        }
    }

    if missing {
        return Err(Error::message("missing tools binaries"));
    }

    Ok(())
}

fn check_required_config_files(context: &Context) -> Result<()> {
    let dst = context.project_root.join("config");
    let mut missing = false;

    for name in config_top_level_entries() {
        let path = dst.join(&name);
        if !path.exists() {
            eprintln!("missing: config/{name} ({})", path.display());
            missing = true;
        }
    }

    if missing {
        eprintln!(
            "hint: run cladding init, or add missing top-level entries into {}",
            dst.display()
        );
        return Err(Error::message("missing config files"));
    }

    Ok(())
}

fn check_required_scripts_files(context: &Context) -> Result<()> {
    let dst = context.project_root.join("scripts");
    let mut missing = false;

    for name in scripts_top_level_entries() {
        let path = dst.join(&name);
        if !path.exists() {
            eprintln!("missing: scripts/{name} ({})", path.display());
            missing = true;
        }
    }

    if missing {
        eprintln!(
            "hint: run cladding init, or add missing top-level entries into {}",
            dst.display()
        );
        return Err(Error::message("missing scripts files"));
    }

    Ok(())
}

fn warn_on_script_mismatch(context: &Context) -> Result<()> {
    let dst = context.project_root.join("scripts");
    let mut warned = false;

    for (rel_path, contents) in scripts_files() {
        let target = dst.join(&rel_path);
        match fs::read(&target) {
            Ok(existing) => {
                if existing != contents {
                    eprintln!(
                        "warning: scripts/{} differs from embedded version",
                        rel_path.display()
                    );
                    warned = true;
                }
            }
            Err(_) => {
                eprintln!("warning: scripts/{} is missing", rel_path.display());
                warned = true;
            }
        }
    }

    if warned {
        eprintln!("hint: run cladding init --update-scripts to re-materialize scripts");
    }

    Ok(())
}

fn check_required_host_paths(
    context: &Context,
    config: &Config,
    network_settings: &cladding::network::NetworkSettings,
) -> Result<()> {
    let rendered = render_pods_yaml(&context.project_root, config, network_settings);

    let mut missing = false;
    let mut seen = std::collections::HashSet::new();
    for path in host_paths_from_rendered(&rendered) {
        if !seen.insert(path.clone()) {
            continue;
        }
        let host_path = Path::new(&path);
        if !host_path.exists() {
            eprintln!("missing: hostPath {}", host_path.display());
            eprintln!("hint: create or relink {}", host_path.display());
            missing = true;
        }
    }

    if missing {
        return Err(Error::message("missing host paths"));
    }

    Ok(())
}

fn check_required_images(config: &Config) -> Result<()> {
    let mut missing = false;
    for image in [&config.cli_image, &config.sandbox_image] {
        let status = Command::new("podman")
            .args(["image", "exists", image])
            .status();

        match status {
            Ok(status) if status.success() => {}
            Ok(_) => {
                eprintln!("missing: image {image}");
                if image_is_buildable_by_cladding(image) {
                    eprintln!("hint: run cladding build");
                } else {
                    eprintln!(
                        "hint: pull/tag image '{image}', or set cladding.json image to a supported build target and run cladding build"
                    );
                }
                missing = true;
            }
            Err(err) => {
                eprintln!("error: failed to check image {image}: {err}");
                return Err(Error::message("failed to check image"));
            }
        }
    }

    if missing {
        return Err(Error::message("missing required images"));
    }

    Ok(())
}

struct ProjectRuntimeStatus {
    current_project_root: String,
    already_running: bool,
}

fn current_project_root(context: &Context) -> Result<String> {
    Ok(canonicalize_path(&context.project_root)?
        .display()
        .to_string())
}

fn project_runtime_status(context: &Context, config: &Config) -> Result<ProjectRuntimeStatus> {
    let current_project_root = current_project_root(context)?;

    let mut conflicting_roots = Vec::new();
    let mut already_running = false;
    for project in list_running_projects()? {
        if project.name != config.name {
            continue;
        }

        let normalized_root = canonicalize_path(Path::new(&project.project_root))
            .map(|path| path.display().to_string())
            .unwrap_or_else(|_| project.project_root.clone());

        if normalized_root == current_project_root {
            already_running = true;
        } else {
            conflicting_roots.push(project.project_root);
        }
    }

    if !conflicting_roots.is_empty() {
        eprintln!(
            "error: cladding project '{}' is already running from a different PROJECT_ROOT",
            config.name
        );
        eprintln!("current PROJECT_ROOT: {current_project_root}");
        for root in conflicting_roots {
            eprintln!("running PROJECT_ROOT: {root}");
        }
        return Err(Error::message(
            "project already running from different PROJECT_ROOT",
        ));
    }

    Ok(ProjectRuntimeStatus {
        current_project_root,
        already_running,
    })
}

fn cmd_up(context: &Context) -> Result<()> {
    let config = load_cladding_config(&context.project_root)?;
    let status = project_runtime_status(context, &config)?;

    if status.already_running {
        println!(
            "already running: {} ({})",
            config.name, status.current_project_root
        );
        return Ok(());
    }

    check_required_binaries(context)?;
    let network_settings = select_available_network_settings(&config.name)?;
    check_required_images(&config)?;
    check_required_host_paths(context, &config, &network_settings)?;
    check_required_config_files(context)?;
    check_required_scripts_files(context)?;
    warn_on_script_mismatch(context)?;
    let rendered = render_pods_yaml(&context.project_root, &config, &network_settings);
    podman_play_kube(&rendered, &network_settings, false)
}

fn cmd_down(context: &Context) -> Result<()> {
    let config = load_cladding_config(&context.project_root)?;
    let project_root = current_project_root(context)?;
    let network_settings =
        resolve_active_project_network_settings(context, &config, "cladding down")?;
    let rendered = render_pods_yaml(&context.project_root, &config, &network_settings);
    let pod_result = podman_play_kube(&rendered, &network_settings, true);
    let cleanup_result = remove_project_expose_proxies(&config, &project_root, true);

    pod_result?;
    cleanup_result
}

fn cmd_destroy(context: &Context) -> Result<()> {
    let config = load_cladding_config(&context.project_root)?;
    let project_root = current_project_root(context)?;
    let network_settings =
        resolve_active_project_network_settings(context, &config, "cladding destroy")?;

    let status = Command::new("podman")
        .args([
            "rm",
            "-f",
            &network_settings.cli_pod_name,
            &network_settings.sandbox_pod_name,
            &network_settings.proxy_pod_name,
        ])
        .status()
        .with_context(|| "failed to run podman rm")?;

    let destroy_result = cladding::podman::ensure_success(status, "podman rm");
    let cleanup_result = remove_project_expose_proxies(&config, &project_root, true);

    destroy_result?;
    cleanup_result
}

fn cmd_ps(_context: &Context) -> Result<()> {
    podman_required("podman (required for cladding ps)")?;
    let projects = list_running_projects()?;
    if projects.is_empty() {
        println!("no running cladding projects");
        return Ok(());
    }

    println!("running cladding projects:");
    for project in projects {
        println!(
            "{}  {}  (pods: {})",
            project.name, project.project_root, project.pod_count
        );
    }

    Ok(())
}

fn cmd_expose(context: &Context, args: &ExposeArgs) -> Result<()> {
    match &args.command {
        Some(ExposeSubcommand::Stop { host_port }) => cmd_expose_stop(context, *host_port),
        Some(ExposeSubcommand::List) => cmd_expose_list(context),
        None => {
            let Some(container_port) = args.container_port else {
                return Err(Error::message("missing container port"));
            };
            cmd_expose_create(context, container_port, args.host_port)
        }
    }
}

fn cmd_run(context: &Context, env_vars: &[String], args: &[String]) -> Result<()> {
    let config = load_cladding_config(&context.project_root)?;
    let network_settings =
        resolve_active_project_network_settings(context, &config, "cladding run")?;
    let container_name = format!("{}-cli-app", network_settings.cli_pod_name);
    run_podman_exec(context, &config, "run", &container_name, env_vars, args)
}

fn cmd_run_with_scissors(context: &Context, env_vars: &[String], args: &[String]) -> Result<()> {
    let config = load_cladding_config(&context.project_root)?;
    let network_settings =
        resolve_active_project_network_settings(context, &config, "cladding run-with-scissors")?;
    let container_name = format!("{}-sandbox-app", network_settings.sandbox_pod_name);
    run_podman_exec(
        context,
        &config,
        "run-with-scissors",
        &container_name,
        env_vars,
        args,
    )
}

fn run_podman_exec(
    context: &Context,
    config: &Config,
    command_name: &str,
    container_name: &str,
    env_vars: &[String],
    args: &[String],
) -> Result<()> {
    if args.is_empty() {
        eprintln!("usage: cladding {command_name} [--env KEY[=VALUE] ...] <command> [args...]");
        return Err(Error::message(format!("missing {command_name} command")));
    }

    let status = project_runtime_status(context, config)?;
    if !status.already_running {
        eprintln!("error: cladding project '{}' is not running", config.name);
        eprintln!("hint: run 'cladding up'");
        return Err(Error::message("project is not running"));
    }

    let project_dir = context
        .project_root
        .parent()
        .ok_or_else(|| Error::message("could not resolve project directory"))?
        .to_path_buf();

    let cwd = env::current_dir().with_context(|| "failed to determine current directory")?;

    let project_dir = canonicalize_path(&project_dir)?;
    let cwd = canonicalize_path(&cwd)?;

    let workdir_rel = cwd.strip_prefix(&project_dir).map_err(|_| {
        eprintln!(
            "error: could not determine current path relative to project dir ({}): {}",
            project_dir.display(),
            cwd.display()
        );
        eprintln!(
            "hint: run cladding from {} or one of its subdirectories",
            project_dir.display()
        );
        Error::message("invalid working directory")
    })?;

    let mut container_workdir = PathBuf::from("/home/user/workspace");
    if !workdir_rel.as_os_str().is_empty() {
        container_workdir = container_workdir.join(workdir_rel);
    }

    let interactive = io::stdin().is_terminal() && io::stdout().is_terminal();

    let mut cmd = Command::new("podman");
    if interactive {
        let colorterm = env::var("COLORTERM").unwrap_or_else(|_| "truecolor".to_string());
        let force_color = env::var("FORCE_COLOR").unwrap_or_else(|_| "3".to_string());
        cmd.args([
            "exec",
            "-it",
            "-w",
            &container_workdir.display().to_string(),
            "--env",
            "LANG=C.UTF-8",
            "--env",
            "TERM=xterm-256color",
            "--env",
            &format!("COLORTERM={colorterm}"),
            "--env",
            &format!("FORCE_COLOR={force_color}"),
        ]);
    } else {
        cmd.args([
            "exec",
            "-i",
            "-w",
            &container_workdir.display().to_string(),
            "--env",
            "LANG=C.UTF-8",
        ]);
    }

    for env_var in env_vars {
        cmd.arg("--env").arg(env_var);
    }

    cmd.arg(container_name);

    for arg in args {
        cmd.arg(arg);
    }

    let mut child = cmd
        .spawn()
        .with_context(|| format!("failed to run podman exec for {command_name}"))?;

    let mut signal_handle = None;
    let mut signal_thread = None;
    if !interactive {
        let kill_pattern = args.join(" ");
        let mut signals = Signals::new([SIGINT, SIGTERM])
            .with_context(|| "failed to install signal handlers")?;
        signal_handle = Some(signals.handle());
        let container_name = container_name.to_string();
        signal_thread = Some(thread::spawn(move || {
            if signals.forever().next().is_some() {
                if !kill_pattern.is_empty() {
                    let _ = Command::new("podman")
                        .args([
                            "exec",
                            &container_name,
                            "pkill",
                            "-f",
                            &kill_pattern,
                        ])
                        .status();
                }
            }
        }));
    }

    let status = child
        .wait()
        .with_context(|| format!("failed to run podman exec for {command_name}"))?;

    if let Some(handle) = signal_handle {
        handle.close();
    }
    if let Some(thread) = signal_thread {
        let _ = thread.join();
    }

    if let Some(code) = status.code() {
        if code == 0 {
            Ok(())
        } else {
            Err(Error::CommandFailed {
                context: "podman exec",
                code,
            })
        }
    } else {
        Err(Error::message("podman exec failed"))
    }
}

fn cmd_reload_proxy(context: &Context) -> Result<()> {
    let config = load_cladding_config(&context.project_root)?;
    let network_settings =
        resolve_active_project_network_settings(context, &config, "cladding reload-proxy")?;

    let status = Command::new("podman")
        .args([
            "exec",
            &format!("{}-proxy", network_settings.proxy_pod_name),
            "squid",
            "-k",
            "reconfigure",
            "-f",
            "/tmp/squid_generated.conf",
        ])
        .status()
        .with_context(|| "failed to run podman exec")?;

    cladding::podman::ensure_success(status, "podman exec")
}

fn cmd_expose_create(context: &Context, container_port: u16, host_port: Option<u16>) -> Result<()> {
    podman_required("podman (required for cladding expose)")?;

    let config = load_cladding_config(&context.project_root)?;
    let project_root = current_project_root(context)?;
    let network_settings =
        resolve_active_project_network_settings(context, &config, "cladding expose")?;
    let cli_container_name = format!("{}-cli-app", network_settings.cli_pod_name);

    if !podman_container_exists(&cli_container_name)? {
        eprintln!(
            "error: target container '{}' is missing for project '{}'",
            cli_container_name, config.name
        );
        eprintln!("hint: run 'cladding up'");
        return Err(Error::message("missing cli container"));
    }

    let existing = list_project_expose_proxies(&config.name, &project_root, false)?;
    if let Some(proxy) = existing
        .iter()
        .find(|proxy| proxy.container_port == container_port)
    {
        eprintln!(
            "error: container port {container_port} is already exposed for project '{}' on localhost:{}",
            config.name, proxy.host_port
        );
        return Err(Error::message("container port already exposed"));
    }

    let start_host_port = host_port.unwrap_or(container_port);
    for candidate_host_port in start_host_port..=u16::MAX {
        if !host_port_appears_available(candidate_host_port) {
            continue;
        }

        match try_start_expose_proxy(
            &config,
            &project_root,
            &network_settings,
            container_port,
            candidate_host_port,
        )? {
            ExposeCreateOutcome::Started => {
                println!(
                    "exposed: localhost:{candidate_host_port} -> {}:{container_port}",
                    cli_container_name
                );
                return Ok(());
            }
            ExposeCreateOutcome::HostPortConflict => continue,
        }
    }

    eprintln!(
        "error: could not allocate a free host port starting at {start_host_port}"
    );
    Err(Error::message("could not allocate free host port"))
}

fn cmd_expose_stop(context: &Context, host_port: u16) -> Result<()> {
    podman_required("podman (required for cladding expose stop)")?;

    let config = load_cladding_config(&context.project_root)?;
    let project_root = current_project_root(context)?;
    let proxies = list_project_expose_proxies(&config.name, &project_root, true)?;
    let matched: Vec<_> = proxies
        .into_iter()
        .filter(|proxy| proxy.host_port == host_port)
        .collect();

    if matched.is_empty() {
        eprintln!(
            "error: no expose proxy for project '{}' publishes localhost:{host_port}",
            config.name
        );
        return Err(Error::message("host port not found"));
    }

    let ids: Vec<String> = matched.iter().map(|proxy| proxy.id.clone()).collect();
    podman_remove_containers(&ids, true, true)?;
    println!("stopped: localhost:{host_port}");
    Ok(())
}

fn cmd_expose_list(context: &Context) -> Result<()> {
    podman_required("podman (required for cladding expose list)")?;

    let config = load_cladding_config(&context.project_root)?;
    let project_root = current_project_root(context)?;
    let proxies = list_project_expose_proxies(&config.name, &project_root, false)?;

    if proxies.is_empty() {
        println!("no exposed ports for project '{}'", config.name);
        return Ok(());
    }

    println!("HOST PORT  CONTAINER PORT  STATUS");
    for proxy in proxies {
        println!(
            "{:<9}  {:<14}  {}",
            proxy.host_port, proxy.container_port, proxy.status
        );
    }

    Ok(())
}

fn remove_project_expose_proxies(config: &Config, project_root: &str, force: bool) -> Result<()> {
    let proxies = list_project_expose_proxies(&config.name, project_root, true)?;
    if proxies.is_empty() {
        return Ok(());
    }

    let ids: Vec<String> = proxies.into_iter().map(|proxy| proxy.id).collect();
    podman_remove_containers(&ids, force, true)
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ExposeCreateOutcome {
    Started,
    HostPortConflict,
}

fn try_start_expose_proxy(
    config: &Config,
    project_root: &str,
    network_settings: &cladding::network::NetworkSettings,
    container_port: u16,
    host_port: u16,
) -> Result<ExposeCreateOutcome> {
    let container_name = unique_expose_proxy_name(&config.name, container_port, host_port);
    let mut cmd = Command::new("podman");
    cmd.arg("run")
        .arg("-d")
        .arg("--name")
        .arg(&container_name)
        .arg("--network")
        .arg(&network_settings.network)
        .arg("-p")
        .arg(format!("{host_port}:{container_port}"));

    for (key, value) in expose_proxy_labels(&config.name, project_root, container_port, host_port) {
        cmd.arg("--label").arg(format!("{key}={value}"));
    }

    cmd.arg("alpine/socat")
        .arg(format!("TCP-LISTEN:{container_port},fork,reuseaddr"))
        .arg(format!("TCP:{}:{container_port}", network_settings.cli_ip));

    let output = cmd
        .output()
        .with_context(|| "failed to run podman run for cladding expose")?;

    if output.status.success() {
        return Ok(ExposeCreateOutcome::Started);
    }

    if podman_output_is_bind_conflict(&output) {
        return Ok(ExposeCreateOutcome::HostPortConflict);
    }

    cladding::podman::ensure_success_output(&output, "podman run")?;
    Err(Error::message("podman run failed"))
}

fn expose_proxy_labels(
    project_name: &str,
    project_root: &str,
    container_port: u16,
    host_port: u16,
) -> [(&'static str, String); 6] {
    [
        ("cladding", project_name.to_string()),
        ("project_root", project_root.to_string()),
        ("cladding_expose", "true".to_string()),
        ("cladding_expose_target", "cli-app".to_string()),
        (
            "cladding_expose_container_port",
            container_port.to_string(),
        ),
        ("cladding_expose_host_port", host_port.to_string()),
    ]
}

fn unique_expose_proxy_name(project_name: &str, container_port: u16, host_port: u16) -> String {
    let suffix = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_millis())
        .unwrap_or(0);
    format!("{project_name}-expose-{container_port}-{host_port}-{suffix}")
}

fn host_port_appears_available(port: u16) -> bool {
    TcpListener::bind(("127.0.0.1", port)).is_ok()
}

fn podman_output_is_bind_conflict(output: &std::process::Output) -> bool {
    let stderr = String::from_utf8_lossy(&output.stderr).to_ascii_lowercase();
    stderr.contains("address already in use")
        || stderr.contains("port is already allocated")
        || stderr.contains("bind")
}

fn image_is_buildable_by_cladding(image: &str) -> bool {
    image == DEFAULT_CLADDING_BUILD_IMAGE
}

fn select_available_network_settings(name: &str) -> Result<cladding::network::NetworkSettings> {
    let running = list_running_project_networks()?;
    let mut used = std::collections::HashSet::new();
    for project in running {
        let Some(index) = parse_cladding_pool_index(&project.network) else {
            continue;
        };
        used.insert(index);
    }

    let mut subnet_to_networks: std::collections::HashMap<String, Vec<String>> =
        std::collections::HashMap::new();
    for entry in list_podman_network_subnets()? {
        subnet_to_networks
            .entry(entry.subnet)
            .or_default()
            .push(entry.name);
    }

    let mut mismatched = 0usize;
    let mut attempted = 0usize;
    let mut conflicts = 0usize;
    for index in 0u16..=255 {
        let index = index as u8;
        if !used.contains(&index) {
            let candidate_subnet = format!("10.90.{index}.0/24");
            let candidate_network = format!("cladding-{index}");
            if let Some(names) = subnet_to_networks.get(&candidate_subnet) {
                if names.iter().any(|name| name != &candidate_network) {
                    conflicts += 1;
                    continue;
                }
            }
            let candidate = resolve_network_settings(name, index)?;
            attempted += 1;
            match ensure_pool_network_settings(&candidate)? {
                EnsureNetworkOutcome::Ready => return Ok(candidate),
                EnsureNetworkOutcome::SubnetMismatch => {
                    mismatched += 1;
                    continue;
                }
            }
        }
    }

    eprintln!("error: no free cladding network slots in pool cladding-0..cladding-255");
    if mismatched > 0 {
        eprintln!(
            "hint: {mismatched} cladding-N networks exist with unexpected subnets; remove them with 'podman network rm cladding-N'"
        );
    } else if conflicts > 0 {
        eprintln!(
            "hint: {conflicts} pool subnets are already used by non-cladding networks; free those subnets or remove the conflicting networks"
        );
    } else if attempted == 0 {
        eprintln!("hint: run 'cladding ps' and stop a running project with 'cladding down'");
    } else {
        eprintln!("hint: run 'cladding ps' and stop a running project with 'cladding down'");
    }
    Err(Error::message("no free cladding network slots"))
}

fn resolve_active_project_network_settings(
    context: &Context,
    config: &Config,
    command_name: &str,
) -> Result<cladding::network::NetworkSettings> {
    let current_project_root = canonicalize_path(&context.project_root)?
        .display()
        .to_string();

    let mut matched_network: Option<String> = None;
    for project in list_running_project_networks()? {
        if project.name != config.name {
            continue;
        }

        let normalized_root = canonicalize_path(Path::new(&project.project_root))
            .map(|path| path.display().to_string())
            .unwrap_or_else(|_| project.project_root.clone());

        if normalized_root != current_project_root {
            continue;
        }

        if let Some(existing) = &matched_network {
            if existing != &project.network {
                eprintln!(
                    "error: active project '{}' has inconsistent cladding network assignment",
                    config.name
                );
                eprintln!("project_root: {current_project_root}");
                eprintln!("networks: {existing}, {}", project.network);
                return Err(Error::message("inconsistent active network"));
            }
            continue;
        }

        matched_network = Some(project.network);
    }

    let Some(network_name) = matched_network else {
        eprintln!(
            "error: could not resolve active cladding network for project '{}'",
            config.name
        );
        eprintln!("hint: ensure the project is running, then retry '{command_name}'");
        return Err(Error::message("missing active cladding network"));
    };

    let Some(index) = parse_cladding_pool_index(&network_name) else {
        eprintln!(
            "error: active project '{}' is attached to unexpected network '{}'",
            config.name, network_name
        );
        eprintln!("hint: restart the project with 'cladding down' then 'cladding up'");
        return Err(Error::message("unexpected active network"));
    };

    resolve_network_settings(&config.name, index)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn expose_create_args_parse_without_subcommand() {
        let cli = Cli::try_parse_from(["cladding", "expose", "3000", "9000"]).expect("cli parse");
        match cli.command.expect("command") {
            CommandSpec::Expose(args) => {
                assert!(args.command.is_none());
                assert_eq!(args.container_port, Some(3000));
                assert_eq!(args.host_port, Some(9000));
            }
            other => panic!("unexpected command: {other:?}"),
        }
    }

    #[test]
    fn expose_stop_subcommand_parses() {
        let cli =
            Cli::try_parse_from(["cladding", "expose", "stop", "9000"]).expect("cli parse");
        match cli.command.expect("command") {
            CommandSpec::Expose(ExposeArgs {
                command: Some(ExposeSubcommand::Stop { host_port }),
                ..
            }) => assert_eq!(host_port, 9000),
            other => panic!("unexpected command: {other:?}"),
        }
    }

    #[test]
    fn expose_list_subcommand_parses() {
        let cli = Cli::try_parse_from(["cladding", "expose", "list"]).expect("cli parse");
        match cli.command.expect("command") {
            CommandSpec::Expose(ExposeArgs {
                command: Some(ExposeSubcommand::List),
                ..
            }) => {}
            other => panic!("unexpected command: {other:?}"),
        }
    }

    #[test]
    fn expose_requires_action_or_ports() {
        assert!(Cli::try_parse_from(["cladding", "expose"]).is_err());
    }
}
