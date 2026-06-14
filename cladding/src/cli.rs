use anyhow::Context as _;
use cladding::assets::{
    materialize_config, materialize_scripts, materialize_scripts_force, scripts_files,
    scripts_top_level_entries, write_embedded_tools,
};
use cladding::config::{
    ExecutionConfig, MountTarget, load_cladding_config_v2, write_default_cladding_config,
};
use cladding::error::{Error, Result};
use cladding::fs_utils::{canonicalize_path, is_broken_symlink, is_executable, path_is_symlink};
use cladding::network::{parse_cladding_pool_index, resolve_network_settings_for_config};
use cladding::podman::{
    EnsureNetworkOutcome, ensure_pool_network_settings, list_podman_network_subnets,
    list_project_expose_proxies, list_running_project_networks, list_running_projects,
    podman_build_image, podman_container_exists, podman_play_kube, podman_remove_containers,
    podman_required,
};
use cladding::pods::{host_paths_from_rendered, render_pods_yaml_v2};
use clap::{ArgAction, Args, Parser, Subcommand, ValueEnum};
use signal_hook::consts::signal::{SIGINT, SIGTERM};
use signal_hook::iterator::Signals;
use std::collections::{HashMap, HashSet};
use std::env;
use std::fs;
use std::io::{self, IsTerminal};
use std::net::TcpListener;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::thread;
use std::time::{SystemTime, UNIX_EPOCH};

const DEFAULT_CLADDING_BUILD_IMAGE: &str = "localhost/cladding-default:latest";
const DEFAULT_CLI_BUILD_IMAGE: &str = DEFAULT_CLADDING_BUILD_IMAGE;
const DEFAULT_SANDBOX_BUILD_IMAGE: &str = DEFAULT_CLADDING_BUILD_IMAGE;
const CONTAINER_WORKSPACE_DIR: &str = "/home/user/workspace";

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
    /// Run a command in the agent container
    Run {
        #[arg(long = "env", value_name = "KEY[=VALUE]", action = ArgAction::Append)]
        env: Vec<String>,
        #[arg(trailing_var_arg = true, allow_hyphen_values = true)]
        args: Vec<String>,
    },
    /// Run a command in the sandbox container
    RunWithScissors {
        #[arg(long, value_enum, default_value_t = RunWithScissorsTarget::NwSandbox)]
        target: RunWithScissorsTarget,
        #[arg(long = "env", value_name = "KEY[=VALUE]", action = ArgAction::Append)]
        env: Vec<String>,
        #[arg(trailing_var_arg = true, allow_hyphen_values = true)]
        args: Vec<String>,
    },
    /// Reload the squid proxy configuration
    ReloadProxy,
    /// Show running cladding projects
    Ps,
    /// Publish an agent TCP port to the host
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

#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum)]
#[value(rename_all = "kebab-case")]
enum RunWithScissorsTarget {
    NwSandbox,
    FsSandbox,
}

impl RunWithScissorsTarget {
    fn as_str(self) -> &'static str {
        match self {
            Self::NwSandbox => "nw-sandbox",
            Self::FsSandbox => "fs-sandbox",
        }
    }

    fn config_key(self) -> &'static str {
        match self {
            Self::NwSandbox => "nw_sandbox",
            Self::FsSandbox => "fs_sandbox",
        }
    }

    fn enabled(self, config: &ExecutionConfig) -> bool {
        match self {
            Self::NwSandbox => config.nw_sandbox_enabled(),
            Self::FsSandbox => config.fs_sandbox_enabled(),
        }
    }

    fn other(self) -> Self {
        match self {
            Self::NwSandbox => Self::FsSandbox,
            Self::FsSandbox => Self::NwSandbox,
        }
    }
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
        CommandSpec::RunWithScissors { target, env, args } => {
            cmd_run_with_scissors(&context, target, &env, &args)
        }
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
    let config = load_cladding_config_v2(&context.project_root)?;

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

    let mut built_images = HashSet::new();
    build_default_image(
        "agent",
        config.agent_image(),
        host_uid,
        host_gid,
        &mut built_images,
    )?;
    if config.nw_sandbox_enabled() {
        build_default_image(
            "nw sandbox",
            config.nw_sandbox_image(),
            host_uid,
            host_gid,
            &mut built_images,
        )?;
    }
    if config.fs_sandbox_enabled() {
        build_default_image(
            "fs sandbox",
            config.fs_sandbox_image(),
            host_uid,
            host_gid,
            &mut built_images,
        )?;
    }

    Ok(())
}

fn build_default_image(
    label: &str,
    image: &str,
    host_uid: u32,
    host_gid: u32,
    built_images: &mut HashSet<String>,
) -> Result<()> {
    if !built_images.insert(image.to_string()) {
        println!("skip: {label} image already built ({image})");
        return Ok(());
    }

    if image != DEFAULT_CLADDING_BUILD_IMAGE {
        println!(
            "skip: not building {label} image (config image is {}, build target is {})",
            image, DEFAULT_CLADDING_BUILD_IMAGE
        );
        return Ok(());
    }

    podman_build_image(image, host_uid, host_gid)
}

fn cmd_init(context: &Context, name_override: Option<&str>, update_scripts: bool) -> Result<()> {
    let project_root = &context.project_root;
    let config_dir = project_root.join("config");
    let scripts_dir = project_root.join("scripts");
    let home_dir = project_root.join("home");
    let tools_dir = project_root.join("tools");
    let cladding_config = project_root.join("cladding.json");
    let cladding_gitignore = project_root.join(".gitignore");
    let cladding_config_preexisting = cladding_config.exists();

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

    if cladding_config_preexisting {
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
    let legacy_config_entries_present = check_legacy_config_entries(context);
    let config = load_cladding_config_v2(&context.project_root)?;

    check_required_binaries(context, &config)?;
    check_required_config_files(context, &config)?;
    let network_settings = resolve_network_settings_for_config(&config.name, 0, &config)?;
    check_required_host_paths(context, &config, &network_settings)?;
    check_required_scripts_files(context)?;
    check_required_images(&config)?;
    if legacy_config_entries_present {
        return Err(Error::message("legacy config entries"));
    }
    println!("check: ok");
    Ok(())
}

fn check_required_binaries(context: &Context, config: &ExecutionConfig) -> Result<()> {
    let mut missing = false;
    let bin_dir = context.project_root.join("tools/bin");

    let mut required = vec!["mcp-run", "run-remote"];
    if config.nw_sandbox_enabled() {
        required.push("run-in-nw-sandbox");
    }
    if config.fs_sandbox_enabled() {
        required.push("run-in-fs-sandbox");
    }

    for name in required {
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

fn check_required_config_files(context: &Context, config: &ExecutionConfig) -> Result<()> {
    let dst = context.project_root.join("config");
    let mut missing = false;

    for name in required_config_entries(config) {
        let path = dst.join(name);
        if !path.exists() {
            eprintln!("missing: config/{name} ({})", path.display());
            missing = true;
        }
    }

    if missing {
        eprintln!(
            "hint: run cladding init, or add missing config paths under {}",
            dst.display()
        );
        return Err(Error::message("missing config files"));
    }

    Ok(())
}

fn check_legacy_config_entries(context: &Context) -> bool {
    let dst = context.project_root.join("config");
    let mut legacy = false;

    for (legacy_name, replacement) in legacy_config_entries() {
        let path = dst.join(*legacy_name);
        if path.exists() {
            eprintln!(
                "error: legacy config/{legacy_name} exists ({})",
                path.display()
            );
            eprintln!("hint: replace config/{legacy_name} with config/{replacement}");
            legacy = true;
        }
    }

    legacy
}

fn required_config_entries(config: &ExecutionConfig) -> Vec<&'static str> {
    let mut entries = vec![
        "agent/domains.lst",
        "agent/host_ports.lst",
        "proxy/squid.conf",
    ];
    if config.nw_sandbox_enabled() {
        entries.push("nw_sandbox");
        entries.push("nw_sandbox/domains.lst");
    }
    if config.fs_sandbox_enabled() {
        entries.push("fs_sandbox");
        entries.push("fs_sandbox/main.rego");
    }
    entries
}

fn legacy_config_entries() -> &'static [(&'static str, &'static str)] {
    &[
        ("sandbox_commands", "nw_sandbox"),
        ("sandbox_domains.lst", "nw_sandbox/domains.lst"),
        ("cli_domains.lst", "agent/domains.lst"),
        ("cli_host_ports.lst", "agent/host_ports.lst"),
        ("agent_domains.lst", "agent/domains.lst"),
        ("agent_host_ports.lst", "agent/host_ports.lst"),
        ("nw_sandbox_domains.lst", "nw_sandbox/domains.lst"),
        ("squid.conf", "proxy/squid.conf"),
    ]
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
    config: &ExecutionConfig,
    network_settings: &cladding::network::NetworkSettings,
) -> Result<()> {
    let rendered = render_pods_yaml_v2(&context.project_root, config, network_settings);

    let mut missing = false;
    let mut seen = HashSet::new();
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

fn check_required_images(config: &ExecutionConfig) -> Result<()> {
    let mut missing = false;
    let mut images = vec![("agent", config.agent_image())];
    if config.nw_sandbox_enabled() {
        images.push(("nw_sandbox", config.nw_sandbox_image()));
    }
    if config.fs_sandbox_enabled() {
        images.push(("fs_sandbox", config.fs_sandbox_image()));
    }

    let mut seen = HashSet::new();
    for (label, image) in images {
        if !seen.insert(image.to_string()) {
            continue;
        }
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
                        "hint: pull/tag image '{image}', or set cladding.json {label}.image to a supported build target and run cladding build"
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

fn project_runtime_status(
    context: &Context,
    config: &ExecutionConfig,
) -> Result<ProjectRuntimeStatus> {
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
    let config = load_cladding_config_v2(&context.project_root)?;
    let status = project_runtime_status(context, &config)?;

    if status.already_running {
        println!(
            "already running: {} ({})",
            config.name, status.current_project_root
        );
        return Ok(());
    }

    check_required_binaries(context, &config)?;
    let network_settings = select_available_network_settings_for_config(&config)?;
    check_required_images(&config)?;
    check_required_config_files(context, &config)?;
    check_required_host_paths(context, &config, &network_settings)?;
    check_required_scripts_files(context)?;
    warn_on_script_mismatch(context)?;
    let rendered = render_pods_yaml_v2(&context.project_root, &config, &network_settings);
    podman_play_kube(&rendered, &network_settings, false)
}

fn cmd_down(context: &Context) -> Result<()> {
    let config = load_cladding_config_v2(&context.project_root)?;
    let project_root = current_project_root(context)?;
    let network_settings =
        resolve_active_project_network_settings(context, &config, "cladding down")?;
    let rendered = render_pods_yaml_v2(&context.project_root, &config, &network_settings);
    let pod_result = podman_play_kube(&rendered, &network_settings, true);
    let legacy_cleanup_result = remove_legacy_runtime_pods(&config);
    let cleanup_result = remove_project_expose_proxies(&config, &project_root, true);

    pod_result?;
    legacy_cleanup_result?;
    cleanup_result
}

fn cmd_destroy(context: &Context) -> Result<()> {
    let config = load_cladding_config_v2(&context.project_root)?;
    let project_root = current_project_root(context)?;
    let network_settings =
        resolve_active_project_network_settings(context, &config, "cladding destroy")?;

    let mut container_names = vec![
        runtime_container_name(&network_settings.agent_name, "agent"),
        runtime_container_name(&network_settings.proxy_name, "proxy"),
    ];
    if let Some(nw) = &network_settings.nw_sandbox {
        container_names.push(runtime_container_name(&nw.name, "nw-sandbox"));
    }
    if let Some(fs) = &network_settings.fs_sandbox {
        container_names.push(runtime_container_name(&fs.name, "fs-sandbox"));
    }

    let mut rm_args = vec!["rm".to_string(), "-f".to_string()];
    rm_args.extend(container_names);

    let status = Command::new("podman")
        .args(&rm_args)
        .status()
        .with_context(|| "failed to run podman rm")?;

    let destroy_result = cladding::podman::ensure_success(status, "podman rm");
    let legacy_cleanup_result = remove_legacy_runtime_pods(&config);
    let cleanup_result = remove_project_expose_proxies(&config, &project_root, true);

    destroy_result?;
    legacy_cleanup_result?;
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
    let config = load_cladding_config_v2(&context.project_root)?;
    let network_settings =
        resolve_active_project_network_settings(context, &config, "cladding run")?;
    let container_name = runtime_container_name(&network_settings.agent_name, "agent");
    run_podman_exec(
        context,
        &config,
        "run",
        MountTarget::Agent,
        &container_name,
        env_vars,
        args,
    )
}

fn cmd_run_with_scissors(
    context: &Context,
    target: RunWithScissorsTarget,
    env_vars: &[String],
    args: &[String],
) -> Result<()> {
    let config = load_cladding_config_v2(&context.project_root)?;
    let network_settings =
        resolve_active_project_network_settings(context, &config, "cladding run-with-scissors")?;
    let (container_name, mount_target) = match target {
        RunWithScissorsTarget::NwSandbox => {
            let Some(component) = network_settings.nw_sandbox.as_ref() else {
                return run_with_scissors_target_disabled(&config, target);
            };
            (
                runtime_container_name(&component.name, "nw-sandbox"),
                MountTarget::NwSandbox,
            )
        }
        RunWithScissorsTarget::FsSandbox => {
            let Some(component) = network_settings.fs_sandbox.as_ref() else {
                return run_with_scissors_target_disabled(&config, target);
            };
            (
                runtime_container_name(&component.name, "fs-sandbox"),
                MountTarget::FsSandbox,
            )
        }
    };
    run_podman_exec(
        context,
        &config,
        "run-with-scissors",
        mount_target,
        &container_name,
        env_vars,
        args,
    )
}

fn run_with_scissors_target_disabled(
    config: &ExecutionConfig,
    target: RunWithScissorsTarget,
) -> Result<()> {
    let target_name = target.as_str();
    let target_key = target.config_key();
    let other = target.other();
    let hint = match (other.enabled(config), target.enabled(config)) {
        (true, false) => format!("hint: use '--target {}'", other.as_str()),
        (false, false) => "hint: enable 'nw_sandbox.enabled' or 'fs_sandbox.enabled'".to_string(),
        _ => format!("hint: enable '{target_key}.enabled' or choose a different target"),
    };

    eprintln!(
        "error: target '{target_name}' is disabled for project '{}'",
        config.name
    );
    eprintln!("{hint}");
    Err(Error::message(
        "selected run-with-scissors target is disabled",
    ))
}

fn run_podman_exec(
    context: &Context,
    config: &ExecutionConfig,
    command_name: &str,
    mount_target: MountTarget,
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
    let container_workdir = resolve_container_workdir(config, &project_dir, &cwd, mount_target)?;

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
        let mut signals =
            Signals::new([SIGINT, SIGTERM]).with_context(|| "failed to install signal handlers")?;
        signal_handle = Some(signals.handle());
        let container_name = container_name.to_string();
        signal_thread = Some(thread::spawn(move || {
            if signals.forever().next().is_some() && !kill_pattern.is_empty() {
                let _ = Command::new("podman")
                    .args(["exec", &container_name, "pkill", "-f", &kill_pattern])
                    .status();
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
    let config = load_cladding_config_v2(&context.project_root)?;
    let network_settings =
        resolve_active_project_network_settings(context, &config, "cladding reload-proxy")?;

    let status = Command::new("podman")
        .args([
            "exec",
            &runtime_container_name(&network_settings.proxy_name, "proxy"),
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

    let config = load_cladding_config_v2(&context.project_root)?;
    let project_root = current_project_root(context)?;
    let network_settings =
        resolve_active_project_network_settings(context, &config, "cladding expose")?;
    let agent_container_name = runtime_container_name(&network_settings.agent_name, "agent");

    if !podman_container_exists(&agent_container_name)? {
        eprintln!(
            "error: target container '{}' is missing for project '{}'",
            agent_container_name, config.name
        );
        eprintln!("hint: run 'cladding up'");
        return Err(Error::message("missing agent container"));
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
                    agent_container_name
                );
                return Ok(());
            }
            ExposeCreateOutcome::HostPortConflict => continue,
        }
    }

    eprintln!("error: could not allocate a free host port starting at {start_host_port}");
    Err(Error::message("could not allocate free host port"))
}

fn cmd_expose_stop(context: &Context, host_port: u16) -> Result<()> {
    podman_required("podman (required for cladding expose stop)")?;

    let config = load_cladding_config_v2(&context.project_root)?;
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

    let config = load_cladding_config_v2(&context.project_root)?;
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

fn remove_project_expose_proxies(
    config: &ExecutionConfig,
    project_root: &str,
    force: bool,
) -> Result<()> {
    let proxies = list_project_expose_proxies(&config.name, project_root, true)?;
    if proxies.is_empty() {
        return Ok(());
    }

    let ids: Vec<String> = proxies.into_iter().map(|proxy| proxy.id).collect();
    podman_remove_containers(&ids, force, true)
}

fn remove_legacy_runtime_pods(config: &ExecutionConfig) -> Result<()> {
    let legacy_names = [
        format!("{}-cli-pod", config.name),
        format!("{}-sandbox-pod", config.name),
        format!("{}-proxy-pod", config.name),
    ];

    for name in legacy_names {
        let output = Command::new("podman")
            .args(["pod", "rm", "-f", &name])
            .output()
            .with_context(|| "failed to run podman pod rm")?;
        if output.status.success() || podman_pod_rm_output_is_missing(&output) {
            continue;
        }
        return cladding::podman::ensure_success_output(&output, "podman pod rm");
    }

    Ok(())
}

fn podman_pod_rm_output_is_missing(output: &std::process::Output) -> bool {
    let stderr = String::from_utf8_lossy(&output.stderr).to_ascii_lowercase();
    stderr.contains("no such pod")
        || stderr.contains("no pod with name or id")
        || stderr.contains("no pod with id or name")
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ExposeCreateOutcome {
    Started,
    HostPortConflict,
}

fn try_start_expose_proxy(
    config: &ExecutionConfig,
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
        .arg(format!(
            "TCP:{}:{container_port}",
            network_settings.agent_ip
        ));

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
        ("cladding_expose_target", "agent".to_string()),
        ("cladding_expose_container_port", container_port.to_string()),
        ("cladding_expose_host_port", host_port.to_string()),
    ]
}

fn runtime_container_name(pod_name: &str, container_name: &str) -> String {
    // podman play kube prefixes app container names with the pod name.
    format!("{pod_name}-{container_name}")
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

fn select_available_network_settings_for_config(
    config: &ExecutionConfig,
) -> Result<cladding::network::NetworkSettings> {
    let running = list_running_project_networks()?;
    let mut used = HashSet::new();
    for project in running {
        let Some(index) = parse_cladding_pool_index(&project.network) else {
            continue;
        };
        used.insert(index);
    }

    let mut subnet_to_networks: HashMap<String, Vec<String>> = HashMap::new();
    for entry in list_podman_network_subnets()? {
        subnet_to_networks
            .entry(entry.subnet)
            .or_default()
            .push(entry.name);
    }

    let mut mismatched = 0usize;
    let mut conflicts = 0usize;
    for index in 0u16..=255 {
        let index = index as u8;
        if !used.contains(&index) {
            let candidate_subnet = format!("10.90.{index}.0/24");
            let candidate_network = format!("cladding-{index}");
            if let Some(names) = subnet_to_networks.get(&candidate_subnet)
                && names.iter().any(|name| name != &candidate_network)
            {
                conflicts += 1;
                continue;
            }
            let candidate = resolve_network_settings_for_config(&config.name, index, config)?;
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
    } else {
        eprintln!("hint: run 'cladding ps' and stop a running project with 'cladding down'");
    }
    Err(Error::message("no free cladding network slots"))
}

fn resolve_active_project_network_settings(
    context: &Context,
    config: &ExecutionConfig,
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

    resolve_network_settings_for_config(&config.name, index, config)
}

fn resolve_container_workdir(
    config: &ExecutionConfig,
    project_dir: &Path,
    cwd: &Path,
    target: MountTarget,
) -> Result<PathBuf> {
    if let Some(custom_workspace_host_path) = effective_cli_workspace_host_path(config, target)
        .filter(|path| path.starts_with(project_dir))
    {
        let custom_workspace_host_path = canonicalize_path(&custom_workspace_host_path)?;
        let workdir_rel = cwd.strip_prefix(&custom_workspace_host_path).map_err(|_| {
            eprintln!(
                "error: could not determine current path relative to configured workspace hostPath ({}): {}",
                custom_workspace_host_path.display(),
                cwd.display()
            );
            eprintln!(
                "hint: run cladding from {} or one of its subdirectories",
                custom_workspace_host_path.display()
            );
            Error::message("invalid working directory")
        })?;
        return Ok(join_container_workspace(workdir_rel));
    }

    let workdir_rel = cwd.strip_prefix(project_dir).map_err(|_| {
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

    Ok(join_container_workspace(workdir_rel))
}

fn effective_cli_workspace_host_path(
    config: &ExecutionConfig,
    target: MountTarget,
) -> Option<PathBuf> {
    let custom_mount = config.mounts.iter().find(|mount| {
        mount.mount_path == CONTAINER_WORKSPACE_DIR && mount.targets.contains(&target)
    })?;

    if custom_mount.ignore {
        return None;
    }

    custom_mount.host_path.clone()
}

fn join_container_workspace(workdir_rel: &Path) -> PathBuf {
    let mut container_workdir = PathBuf::from(CONTAINER_WORKSPACE_DIR);
    if !workdir_rel.as_os_str().is_empty() {
        container_workdir = container_workdir.join(workdir_rel);
    }
    container_workdir
}

#[cfg(test)]
mod tests {
    use super::*;
    use cladding::config::{ExecutionComponentConfig, ExecutionConfig, ResolvedMountConfig};
    use std::sync::atomic::{AtomicU64, Ordering};

    static TEMP_DIR_COUNTER: AtomicU64 = AtomicU64::new(0);

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
        let cli = Cli::try_parse_from(["cladding", "expose", "stop", "9000"]).expect("cli parse");
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

    #[test]
    fn run_with_scissors_target_parses() {
        let cli = Cli::try_parse_from(["cladding", "run-with-scissors", "--target", "fs-sandbox"])
            .expect("cli parse");

        match cli.command.expect("command") {
            CommandSpec::RunWithScissors { target, .. } => {
                assert_eq!(target, RunWithScissorsTarget::FsSandbox);
            }
            other => panic!("unexpected command: {other:?}"),
        }
    }

    #[test]
    fn required_config_entries_use_normalized_layout() {
        let config = execution_config(false, false, Vec::new());
        assert_eq!(
            required_config_entries(&config),
            vec![
                "agent/domains.lst",
                "agent/host_ports.lst",
                "proxy/squid.conf"
            ]
        );
    }

    #[test]
    fn required_config_entries_include_enabled_fs_sandbox() {
        let config = execution_config(false, true, Vec::new());
        assert_eq!(
            required_config_entries(&config),
            vec![
                "agent/domains.lst",
                "agent/host_ports.lst",
                "proxy/squid.conf",
                "fs_sandbox",
                "fs_sandbox/main.rego",
            ]
        );
    }

    #[test]
    fn runtime_container_name_matches_podman_play_kube_names() {
        assert_eq!(
            runtime_container_name("demo-fs-sandbox", "fs-sandbox"),
            "demo-fs-sandbox-fs-sandbox"
        );
        assert_eq!(
            runtime_container_name("demo-agent", "agent"),
            "demo-agent-agent"
        );
    }

    #[test]
    fn legacy_config_entries_cover_pre_rename_and_flat_layouts() {
        assert_eq!(
            legacy_config_entries(),
            &[
                ("sandbox_commands", "nw_sandbox"),
                ("sandbox_domains.lst", "nw_sandbox/domains.lst"),
                ("cli_domains.lst", "agent/domains.lst"),
                ("cli_host_ports.lst", "agent/host_ports.lst"),
                ("agent_domains.lst", "agent/domains.lst"),
                ("agent_host_ports.lst", "agent/host_ports.lst"),
                ("nw_sandbox_domains.lst", "nw_sandbox/domains.lst"),
                ("squid.conf", "proxy/squid.conf"),
            ]
        );
    }

    #[test]
    fn check_legacy_config_entries_detects_old_layout_paths() {
        let temp = create_temp_dir("legacy-config-paths");
        let config_dir = temp.join("config");
        fs::create_dir_all(&config_dir).expect("create config dir");
        fs::write(config_dir.join("squid.conf"), "legacy").expect("write legacy config");

        let context = Context { project_root: temp };

        assert!(check_legacy_config_entries(&context));
    }

    #[test]
    fn resolve_container_workdir_uses_default_project_mapping() {
        let temp = create_temp_dir("default-workdir");
        let project_dir = temp.join("workspace");
        let nested_dir = project_dir.join("src/module");
        fs::create_dir_all(&nested_dir).expect("create nested dir");

        let config = ExecutionConfig {
            name: "demo".to_string(),
            agent: ExecutionComponentConfig {
                enabled: true,
                image: "agent:image".to_string(),
            },
            nw_sandbox: None,
            fs_sandbox: None,
            mounts: Vec::new(),
        };

        let resolved =
            resolve_container_workdir(&config, &project_dir, &nested_dir, MountTarget::Agent)
                .expect("workdir");
        assert_eq!(
            resolved,
            PathBuf::from(CONTAINER_WORKSPACE_DIR).join("src/module")
        );
    }

    #[test]
    fn resolve_container_workdir_uses_custom_workspace_host_path() {
        let temp = create_temp_dir("custom-workdir");
        let project_dir = temp.join("project");
        let custom_root = project_dir.join("workspace");
        let nested_dir = custom_root.join("src/module");
        fs::create_dir_all(&nested_dir).expect("create nested dir");

        let config = ExecutionConfig {
            name: "demo".to_string(),
            agent: ExecutionComponentConfig {
                enabled: true,
                image: "agent:image".to_string(),
            },
            nw_sandbox: None,
            fs_sandbox: None,
            mounts: vec![ResolvedMountConfig {
                mount_path: CONTAINER_WORKSPACE_DIR.to_string(),
                host_path: Some(custom_root.clone()),
                volume: None,
                read_only: false,
                targets: vec![MountTarget::Agent],
                ignore: false,
            }],
        };

        let resolved =
            resolve_container_workdir(&config, &project_dir, &nested_dir, MountTarget::Agent)
                .expect("workdir");
        assert_eq!(
            resolved,
            PathBuf::from(CONTAINER_WORKSPACE_DIR).join("src/module")
        );
    }

    fn execution_config(
        nw_enabled: bool,
        fs_enabled: bool,
        mounts: Vec<ResolvedMountConfig>,
    ) -> ExecutionConfig {
        ExecutionConfig {
            name: "demo".to_string(),
            agent: ExecutionComponentConfig {
                enabled: true,
                image: "agent:image".to_string(),
            },
            nw_sandbox: nw_enabled.then(|| ExecutionComponentConfig {
                enabled: true,
                image: "sandbox:image".to_string(),
            }),
            fs_sandbox: fs_enabled.then(|| ExecutionComponentConfig {
                enabled: true,
                image: "fs:image".to_string(),
            }),
            mounts,
        }
    }

    fn create_temp_dir(name: &str) -> PathBuf {
        let unique = TEMP_DIR_COUNTER.fetch_add(1, Ordering::Relaxed);
        let path =
            env::temp_dir().join(format!("cladding-{name}-{}-{}", std::process::id(), unique));
        if path.exists() {
            fs::remove_dir_all(&path).expect("cleanup stale temp dir");
        }
        fs::create_dir_all(&path).expect("create temp dir");
        path
    }
}
