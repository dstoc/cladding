use anyhow::Context as _;
use cladding::assets::{
    materialize_config, materialize_runtime_scripts, scripts_files, tool_files,
    write_embedded_tools,
};
use cladding::config::{
    ExecutionConfig, MountTarget, load_cladding_config_v2, write_default_cladding_config,
};
use cladding::error::{Error, Result};
use cladding::fs_utils::{canonicalize_path, is_broken_symlink, is_executable, path_is_symlink};
use cladding::podman::{
    list_running_projects, podman_build_image, podman_container_exists, podman_required,
    runsc_available, runtime_cleanup, runtime_create,
};
use cladding::runtime::RuntimeSpec;
use clap::{ArgAction, Args, Parser, Subcommand, ValueEnum};
use signal_hook::consts::signal::{SIGINT, SIGTERM};
use signal_hook::iterator::Signals;
use std::collections::HashSet;
use std::env;
use std::fs;
use std::io::{self, IsTerminal};
use std::path::{Path, PathBuf};
use std::process::{Child, Command};
use std::sync::{
    Arc,
    atomic::{AtomicBool, Ordering},
};
use std::thread;
use std::time::Duration;

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
    Init { name: Option<String> },
    /// Check requirements
    Check,
    /// Start the system
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
    /// Show logs for a cladding container
    Logs {
        #[arg(value_enum)]
        target: LogsTarget,
        #[arg(trailing_var_arg = true, allow_hyphen_values = true)]
        args: Vec<String>,
    },
    /// Reload the squid proxy configuration
    ReloadProxy,
    /// Show running cladding projects
    Ps,
    /// Publish an agent TCP port to the host
    Expose(ExposeArgs),
    /// Connect agent localhost to a host-reachable TCP endpoint
    Inject(InjectArgs),
}

#[derive(Debug, Args)]
#[command(arg_required_else_help = true)]
struct ExposeArgs {
    #[arg(value_name = "CONTAINERPORT", value_parser = clap::value_parser!(u16).range(1..=65535))]
    container_port: u16,
    #[arg(value_name = "HOSTPORT", value_parser = clap::value_parser!(u16).range(1..=65535))]
    host_port: Option<u16>,
}

#[derive(Debug, Args)]
#[command(arg_required_else_help = true)]
struct InjectArgs {
    #[arg(value_name = "HOST-ENDPOINT", value_parser = parse_inject_host_endpoint)]
    host_endpoint: InjectHostEndpoint,
    #[arg(value_name = "CONTAINERPORT", value_parser = clap::value_parser!(u16).range(1..=65535))]
    container_port: Option<u16>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct InjectHostEndpoint {
    host: String,
    port: u16,
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

#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum)]
#[value(rename_all = "kebab-case")]
enum LogsTarget {
    Agent,
    Proxy,
    NwSandbox,
    FsSandbox,
}

impl LogsTarget {
    fn as_str(self) -> &'static str {
        match self {
            Self::Agent => "agent",
            Self::Proxy => "proxy",
            Self::NwSandbox => "nw-sandbox",
            Self::FsSandbox => "fs-sandbox",
        }
    }

    fn config_key(self) -> Option<&'static str> {
        match self {
            Self::Agent | Self::Proxy => None,
            Self::NwSandbox => Some("nw_sandbox"),
            Self::FsSandbox => Some("fs_sandbox"),
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
        CommandSpec::Init { name } => cmd_init(&context, name.as_deref()),
        CommandSpec::Check => cmd_check(&context),
        CommandSpec::Up { verbose } => cmd_up(&context, verbose),
        CommandSpec::Down { verbose } => cmd_down(&context, verbose),
        CommandSpec::Destroy => cmd_destroy(&context),
        CommandSpec::Run { env, args } => cmd_run(&context, &env, &args),
        CommandSpec::RunWithScissors { target, env, args } => {
            cmd_run_with_scissors(&context, target, &env, &args)
        }
        CommandSpec::Logs { target, args } => cmd_logs(&context, target, &args),
        CommandSpec::ReloadProxy => cmd_reload_proxy(&context),
        CommandSpec::Ps => cmd_ps(&context),
        CommandSpec::Expose(args) => cmd_expose(&context, &args),
        CommandSpec::Inject(args) => cmd_inject(&context, &args),
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

fn cmd_init(context: &Context, name_override: Option<&str>) -> Result<()> {
    let project_root = &context.project_root;
    let config_dir = project_root.join("config");
    let home_dir = project_root.join("home");
    let tools_dir = project_root.join("tools");
    let runtime_dir = project_root.join("runtime");
    let empty_mask_dir = runtime_dir.join("empty-mask");
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

    if runtime_dir.exists() || path_is_symlink(&runtime_dir) {
        println!("runtime already exists: {}", runtime_dir.display());
    } else {
        fs::create_dir_all(&runtime_dir)
            .with_context(|| format!("failed to create {}", runtime_dir.display()))?;
        println!("initialized: {}", runtime_dir.display());
    }

    if empty_mask_dir.exists() || path_is_symlink(&empty_mask_dir) {
        println!("empty-mask already exists: {}", empty_mask_dir.display());
    } else {
        fs::create_dir_all(&empty_mask_dir)
            .with_context(|| format!("failed to create {}", empty_mask_dir.display()))?;
        println!("initialized: {}", empty_mask_dir.display());
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

    warn_obsolete_generated_paths(context);
    check_required_binaries(context, &config)?;
    check_runsc_runtime(&config, false)?;
    check_required_config_files(context, &config)?;
    check_required_images(&config, false)?;
    let spec = RuntimeSpec::build(&context.project_root, &config);
    check_required_host_paths(&spec)?;
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
            continue;
        }

        let Some((_, embedded)) = tool_files()
            .into_iter()
            .find(|(embedded_name, _)| *embedded_name == name)
        else {
            continue;
        };

        match fs::read(&path) {
            Ok(existing) if existing == embedded => {}
            Ok(_) => {
                eprintln!("outdated: tools/bin/{name} ({})", path.display());
                eprintln!("hint: run cladding build");
                missing = true;
            }
            Err(err) => {
                eprintln!("error: failed to read tools/bin/{name} ({err})");
                eprintln!("hint: run cladding build");
                missing = true;
            }
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
    let mut entries = vec!["agent/domains.lst", "agent/host_ports.lst"];
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
    ]
}

fn warn_obsolete_generated_paths(context: &Context) {
    for rel_path in ["scripts", "config/proxy/squid.conf", "config/squid.conf"] {
        let path = context.project_root.join(rel_path);
        if path.exists() || path_is_symlink(&path) {
            eprintln!(
                "warning: {rel_path} is no longer used and can be removed ({})",
                path.display()
            );
        }
    }
}

fn report_runtime_script_mismatch(context: &Context, level: &str) -> Result<bool> {
    let dst = context.project_root.join("runtime/scripts");
    let mut mismatched = false;

    for (rel_path, contents) in scripts_files() {
        let target = dst.join(&rel_path);
        match fs::read(&target) {
            Ok(existing) => {
                if existing != contents {
                    eprintln!(
                        "{level}: runtime/scripts/{} differs from embedded version",
                        rel_path.display()
                    );
                    mismatched = true;
                }
            }
            Err(_) => {
                eprintln!("{level}: runtime/scripts/{} is missing", rel_path.display());
                mismatched = true;
            }
        }
    }

    if mismatched {
        eprintln!("hint: run cladding up to regenerate runtime scripts");
    }

    Ok(mismatched)
}

fn check_required_host_paths(spec: &RuntimeSpec) -> Result<()> {
    let mut missing = false;
    let mut seen = HashSet::new();
    for path in spec.required_host_paths() {
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

fn check_required_images(config: &ExecutionConfig, verbose: bool) -> Result<()> {
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
        let mut cmd = Command::new("podman");
        cmd.args(["image", "exists", image]);
        cladding::podman::trace_command(&cmd, verbose);
        let status = cmd.status();

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

fn check_runsc_runtime(config: &ExecutionConfig, verbose: bool) -> Result<()> {
    if !config.use_runsc {
        return Ok(());
    }

    match runsc_available(verbose) {
        Ok(true) => Ok(()),
        Ok(false) => {
            eprintln!(
                "missing: runsc (not found on PATH and Podman does not report a runtime named 'runsc')"
            );
            eprintln!("hint: install runsc or configure Podman to expose a runtime named 'runsc'");
            Err(Error::message("missing runsc runtime"))
        }
        Err(err) => {
            eprintln!("error: failed to check runsc availability: {err}");
            eprintln!(
                "hint: install runsc or verify 'podman info' can inspect configured runtimes"
            );
            Err(Error::message("failed to check runsc availability"))
        }
    }
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
    verbose: bool,
) -> Result<ProjectRuntimeStatus> {
    let current_project_root = current_project_root(context)?;

    let mut conflicting_roots = Vec::new();
    let mut already_running = false;
    for project in list_running_projects(verbose)? {
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

fn cmd_up(context: &Context, verbose: bool) -> Result<()> {
    let config = load_cladding_config_v2(&context.project_root)?;
    materialize_runtime_scripts(&context.project_root)?;
    let status = project_runtime_status(context, &config, verbose)?;

    if status.already_running {
        println!(
            "already running: {} ({})",
            config.name, status.current_project_root
        );
        return Ok(());
    }

    check_required_binaries(context, &config)?;
    check_runsc_runtime(&config, verbose)?;
    check_required_images(&config, verbose)?;
    check_required_config_files(context, &config)?;
    warn_obsolete_generated_paths(context);
    let _ = report_runtime_script_mismatch(context, "warning")?;
    let spec = RuntimeSpec::build(&context.project_root, &config);
    fs::create_dir_all(context.project_root.join("runtime/empty-mask"))
        .with_context(|| "failed to create runtime empty-mask directory")?;
    check_required_host_paths(&spec)?;
    runtime_create(&spec, verbose)
}

fn cmd_down(context: &Context, verbose: bool) -> Result<()> {
    let config = load_cladding_config_v2(&context.project_root)?;
    let spec = RuntimeSpec::build(&context.project_root, &config);
    let mut cleanup_error = None;
    record_cleanup_result(&mut cleanup_error, runtime_cleanup(&spec, verbose));

    match cleanup_error {
        Some(err) => Err(err),
        None => Ok(()),
    }
}

fn cmd_destroy(context: &Context) -> Result<()> {
    let config = load_cladding_config_v2(&context.project_root)?;
    let spec = RuntimeSpec::build(&context.project_root, &config);
    let mut cleanup_error = None;
    record_cleanup_result(&mut cleanup_error, runtime_cleanup(&spec, false));

    match cleanup_error {
        Some(err) => Err(err),
        None => Ok(()),
    }
}

fn cmd_ps(_context: &Context) -> Result<()> {
    podman_required("podman (required for cladding ps)")?;
    let projects = list_running_projects(false)?;
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
    podman_required("podman (required for cladding expose)")?;
    socat_required("expose")?;

    let config = load_cladding_config_v2(&context.project_root)?;
    let status = project_runtime_status(context, &config, false)?;
    if !status.already_running {
        eprintln!("error: cladding project '{}' is not running", config.name);
        eprintln!("hint: run 'cladding up'");
        return Err(Error::message("project is not running"));
    }

    let (_, agent_container_name) = agent_runtime_names(&config.name);
    if !podman_container_exists(&agent_container_name)? {
        eprintln!(
            "error: target container '{}' is missing for project '{}'",
            agent_container_name, config.name
        );
        eprintln!("hint: run 'cladding up'");
        return Err(Error::message("missing agent container"));
    }

    let host_port = args.host_port.unwrap_or(args.container_port);
    let current_exe =
        env::current_exe().with_context(|| "failed to determine current executable")?;
    let status = build_blocking_expose_command(&current_exe, args.container_port, host_port)
        .status()
        .with_context(|| "failed to run socat")?;

    cladding::podman::ensure_success(status, "socat")
}

fn cmd_inject(context: &Context, args: &InjectArgs) -> Result<()> {
    podman_required("podman (required for cladding inject)")?;
    socat_required("inject")?;

    let config = load_cladding_config_v2(&context.project_root)?;
    let status = project_runtime_status(context, &config, false)?;
    if !status.already_running {
        eprintln!("error: cladding project '{}' is not running", config.name);
        eprintln!("hint: run 'cladding up'");
        return Err(Error::message("project is not running"));
    }

    let (_, agent_container_name) = agent_runtime_names(&config.name);
    if !podman_container_exists(&agent_container_name)? {
        eprintln!(
            "error: target container '{}' is missing for project '{}'",
            agent_container_name, config.name
        );
        eprintln!("hint: run 'cladding up'");
        return Err(Error::message("missing agent container"));
    }

    if !agent_inject_mount_available(&agent_container_name)? {
        eprintln!(
            "error: target container '{}' is missing /run/cladding/agent/inject for project '{}'",
            agent_container_name, config.name
        );
        eprintln!("hint: run 'cladding down' then 'cladding up'");
        return Err(Error::message("missing inject mount"));
    }

    let container_port = args.container_port.unwrap_or(args.host_endpoint.port);
    let host_socket_dir = inject_host_socket_dir(&context.project_root, container_port);
    let host_socket = host_socket_path(&context.project_root, container_port);
    ensure_inject_socket_path_available(&host_socket)?;
    fs::create_dir_all(&host_socket_dir).with_context(|| {
        format!(
            "failed to create inject socket directory {}",
            host_socket_dir.display()
        )
    })?;

    let agent_socket = inject_agent_socket_path(container_port);
    let mut host_cmd = build_inject_host_bridge_command(&host_socket, &args.host_endpoint);
    let mut host_child = match host_cmd.spawn() {
        Ok(child) => child,
        Err(err) => {
            cleanup_inject_socket_artifacts(&host_socket_dir, &host_socket);
            return Err(anyhow::Error::new(err)
                .context("failed to run host socat for cladding inject")
                .into());
        }
    };

    let agent_listener_args = build_inject_agent_listener_args(&agent_socket, container_port);
    let mut agent_cmd =
        build_inject_agent_exec_command(&agent_container_name, &agent_listener_args);
    let mut agent_child = match agent_cmd.spawn() {
        Ok(child) => child,
        Err(err) => {
            let _ = host_child.kill();
            let _ = host_child.wait();
            cleanup_inject_socket_artifacts(&host_socket_dir, &host_socket);
            return Err(anyhow::Error::new(err)
                .context("failed to run podman exec for cladding inject")
                .into());
        }
    };

    let result = supervise_inject_children(
        &mut host_child,
        &mut agent_child,
        &agent_container_name,
        container_port,
    );

    cleanup_inject_socket_artifacts(&host_socket_dir, &host_socket);
    result
}

fn socat_required(command_name: &str) -> Result<()> {
    if command_exists_on_path("socat") {
        Ok(())
    } else {
        eprintln!("missing: socat (required for cladding {command_name})");
        Err(Error::message("missing socat"))
    }
}

fn agent_inject_mount_available(agent_container_name: &str) -> Result<bool> {
    let status = Command::new("podman")
        .args([
            "exec",
            agent_container_name,
            "test",
            "-d",
            "/run/cladding/agent/inject",
        ])
        .status()
        .with_context(|| "failed to check inject mount availability")?;

    Ok(status.success())
}

fn inject_host_socket_dir(project_root: &Path, container_port: u16) -> PathBuf {
    project_root
        .join("runtime/sockets/agent/inject")
        .join(container_port.to_string())
}

fn host_socket_path(project_root: &Path, container_port: u16) -> PathBuf {
    inject_host_socket_dir(project_root, container_port).join("host.sock")
}

fn inject_agent_socket_path(container_port: u16) -> String {
    format!("/run/cladding/agent/inject/{container_port}/host.sock")
}

fn ensure_inject_socket_path_available(host_socket: &Path) -> Result<()> {
    if host_socket.exists() {
        eprintln!(
            "error: inject socket already exists at {}",
            host_socket.display()
        );
        return Err(Error::message("inject socket already exists"));
    }
    Ok(())
}

fn build_inject_host_bridge_command(host_socket: &Path, endpoint: &InjectHostEndpoint) -> Command {
    let mut cmd = Command::new("socat");
    cmd.arg(format!(
        "UNIX-LISTEN:{},fork,reuseaddr",
        host_socket.display()
    ));
    cmd.arg(format!("TCP:{}:{}", endpoint.host, endpoint.port));
    cmd
}

fn build_inject_agent_listener_args(agent_socket: &str, container_port: u16) -> Vec<String> {
    vec![
        "socat".to_string(),
        format!("TCP-LISTEN:{container_port},bind=127.0.0.1,fork,reuseaddr"),
        format!("UNIX-CONNECT:{agent_socket}"),
    ]
}

fn build_inject_agent_exec_command(
    agent_container_name: &str,
    listener_args: &[String],
) -> Command {
    let mut cmd = Command::new("podman");
    cmd.args(["exec", "-i", agent_container_name]);
    for arg in listener_args {
        cmd.arg(arg);
    }
    cmd
}

fn build_inject_agent_cleanup_command(
    agent_container_name: &str,
    listener_pattern: &str,
) -> Command {
    let mut cmd = Command::new("podman");
    cmd.args([
        "exec",
        agent_container_name,
        "pkill",
        "-f",
        listener_pattern,
    ]);
    cmd
}

fn supervise_inject_children(
    host_child: &mut Child,
    agent_child: &mut Child,
    agent_container_name: &str,
    container_port: u16,
) -> Result<()> {
    let interrupted = Arc::new(AtomicBool::new(false));
    let mut signals =
        Signals::new([SIGINT, SIGTERM]).with_context(|| "failed to install signal handlers")?;
    let signal_handle = signals.handle();
    let signal_flag = Arc::clone(&interrupted);
    let signal_thread = thread::spawn(move || {
        if signals.forever().next().is_some() {
            signal_flag.store(true, Ordering::Relaxed);
        }
    });

    enum ExitCause {
        Interrupted,
        Host(std::process::ExitStatus),
        Agent(std::process::ExitStatus),
    }

    let exit_cause = loop {
        if interrupted.load(Ordering::Relaxed) {
            break ExitCause::Interrupted;
        }

        if let Some(status) = host_child
            .try_wait()
            .with_context(|| "failed to wait on host socat for cladding inject")?
        {
            break ExitCause::Host(status);
        }

        if let Some(status) = agent_child
            .try_wait()
            .with_context(|| "failed to wait on podman exec for cladding inject")?
        {
            break ExitCause::Agent(status);
        }

        thread::sleep(Duration::from_millis(100));
    };

    let _ = host_child.kill();
    let _ = agent_child.kill();
    let host_status = host_child.wait();
    let agent_status = agent_child.wait();

    signal_handle.close();
    let _ = signal_thread.join();

    let listener_args =
        build_inject_agent_listener_args(&inject_agent_socket_path(container_port), container_port);
    let listener_pattern = listener_args.join(" ");
    let _cleanup_status =
        build_inject_agent_cleanup_command(agent_container_name, &listener_pattern)
            .status()
            .ok();

    let _ = host_status;
    let _ = agent_status;

    match exit_cause {
        ExitCause::Interrupted => Ok(()),
        ExitCause::Host(status) => {
            if status.success() {
                Err(Error::message("inject host bridge exited unexpectedly"))
            } else {
                Err(Error::CommandFailed {
                    context: "inject host socat",
                    code: status.code().unwrap_or(1),
                })
            }
        }
        ExitCause::Agent(status) => {
            if status.success() {
                Err(Error::message("inject agent listener exited unexpectedly"))
            } else {
                Err(Error::CommandFailed {
                    context: "podman exec",
                    code: status.code().unwrap_or(1),
                })
            }
        }
    }
}

fn cleanup_inject_socket_artifacts(host_socket_dir: &Path, host_socket: &Path) {
    let _ = fs::remove_file(host_socket);
    let _ = fs::remove_dir_all(host_socket_dir);
}

fn command_exists_on_path(command: &str) -> bool {
    env::var_os("PATH")
        .as_deref()
        .is_some_and(|paths| env::split_paths(paths).any(|path| is_executable(&path.join(command))))
}

fn build_blocking_expose_command(
    current_exe: &Path,
    container_port: u16,
    host_port: u16,
) -> Command {
    let mut cmd = Command::new("socat");
    cmd.arg(format!(
        "TCP-LISTEN:{host_port},bind=127.0.0.1,reuseaddr,fork"
    ));
    let current_exe = shell_single_quote_path(current_exe);
    cmd.arg(format!(
        "EXEC:{current_exe} run socat STDIO TCP\\:127.0.0.1\\:{container_port}"
    ));
    cmd
}

fn shell_single_quote_path(path: &Path) -> String {
    let path = path.to_string_lossy();
    let escaped = path.replace('\'', "'\\''");
    format!("'{escaped}'")
}

fn cmd_run(context: &Context, env_vars: &[String], args: &[String]) -> Result<()> {
    let config = load_cladding_config_v2(&context.project_root)?;
    let container_name = runtime_container_name(&project_component_name(&config.name, "agent"));
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
    let (container_name, mount_target) = match target {
        RunWithScissorsTarget::NwSandbox => {
            if !target.enabled(&config) {
                return run_with_scissors_target_disabled(&config, target);
            }
            (
                runtime_container_name(&project_component_name(&config.name, "nw-sandbox")),
                MountTarget::NwSandbox,
            )
        }
        RunWithScissorsTarget::FsSandbox => {
            if !target.enabled(&config) {
                return run_with_scissors_target_disabled(&config, target);
            }
            (
                runtime_container_name(&project_component_name(&config.name, "fs-sandbox")),
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

fn cmd_logs(context: &Context, target: LogsTarget, args: &[String]) -> Result<()> {
    podman_required("podman (required for cladding logs)")?;

    let config = load_cladding_config_v2(&context.project_root)?;
    let pod_name = match target {
        LogsTarget::Agent => project_component_name(&config.name, "agent"),
        LogsTarget::Proxy => project_component_name(&config.name, "proxy"),
        LogsTarget::NwSandbox if config.nw_sandbox_enabled() => {
            project_component_name(&config.name, "nw-sandbox")
        }
        LogsTarget::FsSandbox if config.fs_sandbox_enabled() => {
            project_component_name(&config.name, "fs-sandbox")
        }
        _ => return logs_target_disabled(&config, target),
    };
    let container_name = runtime_container_name(&pod_name);

    let mut cmd = Command::new("podman");
    cmd.arg("logs");
    for arg in args {
        cmd.arg(arg);
    }
    cmd.arg(container_name);

    let status = cmd.status().with_context(|| "failed to run podman logs")?;
    cladding::podman::ensure_success(status, "podman logs")
}

fn logs_target_disabled(config: &ExecutionConfig, target: LogsTarget) -> Result<()> {
    let target_name = target.as_str();
    eprintln!(
        "error: target '{target_name}' is disabled for project '{}'",
        config.name
    );
    if let Some(key) = target.config_key() {
        eprintln!("hint: enable '{key}.enabled' or choose a different target");
    }
    Err(Error::message("selected logs target is disabled"))
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

    let status = project_runtime_status(context, config, false)?;
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

    let status = Command::new("podman")
        .args([
            "exec",
            &runtime_container_name(&project_component_name(&config.name, "proxy")),
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

fn record_cleanup_result(target: &mut Option<Error>, result: Result<()>) {
    if let Err(err) = result
        && target.is_none()
    {
        *target = Some(err);
    }
}

fn runtime_container_name(pod_name: &str) -> String {
    format!("{pod_name}-instance")
}

fn project_component_name(project_name: &str, component: &str) -> String {
    format!("{project_name}-{component}")
}

fn agent_runtime_names(project_name: &str) -> (String, String) {
    let agent_pod_name = project_component_name(project_name, "agent");
    let agent_container_name = runtime_container_name(&agent_pod_name);
    (agent_pod_name, agent_container_name)
}

fn image_is_buildable_by_cladding(image: &str) -> bool {
    image == DEFAULT_CLADDING_BUILD_IMAGE
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

    fn command_args(cmd: &Command) -> Vec<String> {
        cmd.get_args()
            .map(|arg| arg.to_string_lossy().into_owned())
            .collect()
    }

    #[test]
    fn expose_container_port_parses() {
        let cli = Cli::try_parse_from(["cladding", "expose", "3000"]).expect("cli parse");
        match cli.command.expect("command") {
            CommandSpec::Expose(args) => {
                assert_eq!(args.container_port, 3000);
                assert_eq!(args.host_port, None);
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
            }
            other => panic!("unexpected command: {other:?}"),
        }
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
    fn build_blocking_expose_command_places_ports_and_bind() {
        let cmd = build_blocking_expose_command(
            Path::new("/tmp/cladding test/bin's/cladding"),
            5432,
            15432,
        );
        let args = command_args(&cmd);

        assert_eq!(cmd.get_program().to_string_lossy(), "socat");
        assert_eq!(args.len(), 2);
        assert_eq!(args[0], "TCP-LISTEN:15432,bind=127.0.0.1,reuseaddr,fork");
        assert_eq!(
            args[1],
            "EXEC:'/tmp/cladding test/bin'\\''s/cladding' run socat STDIO TCP\\:127.0.0.1\\:5432"
        );
        assert!(!args.iter().any(|arg| arg.starts_with("--")));
    }

    #[test]
    fn inject_socket_helpers_build_expected_paths() {
        let project_root = Path::new("/tmp/project/.cladding");
        assert_eq!(
            inject_host_socket_dir(project_root, 15432),
            PathBuf::from("/tmp/project/.cladding/runtime/sockets/agent/inject/15432")
        );
        assert_eq!(
            host_socket_path(project_root, 15432),
            PathBuf::from("/tmp/project/.cladding/runtime/sockets/agent/inject/15432/host.sock")
        );
        assert_eq!(
            inject_agent_socket_path(15432),
            "/run/cladding/agent/inject/15432/host.sock"
        );
    }

    #[test]
    fn ensure_inject_socket_path_available_detects_conflict() {
        let temp = create_temp_dir("inject-socket-conflict");
        let socket = host_socket_path(&temp, 15432);
        if let Some(parent) = socket.parent() {
            fs::create_dir_all(parent).expect("create socket parent");
        }
        fs::write(&socket, b"busy").expect("create conflicting socket path");

        assert!(ensure_inject_socket_path_available(&socket).is_err());
    }

    #[test]
    fn build_inject_host_bridge_command_uses_requested_endpoint() {
        let cmd = build_inject_host_bridge_command(
            Path::new("/tmp/project/.cladding/runtime/sockets/agent/inject/15432/host.sock"),
            &InjectHostEndpoint {
                host: "db.internal".to_string(),
                port: 5432,
            },
        );
        let args = command_args(&cmd);

        assert_eq!(cmd.get_program().to_string_lossy(), "socat");
        assert_eq!(
            args,
            vec![
                "UNIX-LISTEN:/tmp/project/.cladding/runtime/sockets/agent/inject/15432/host.sock,fork,reuseaddr",
                "TCP:db.internal:5432"
            ]
        );
    }

    #[test]
    fn build_inject_agent_listener_args_match_prd_shape() {
        let args =
            build_inject_agent_listener_args("/run/cladding/agent/inject/15432/host.sock", 15432);
        assert_eq!(
            args,
            vec![
                "socat".to_string(),
                "TCP-LISTEN:15432,bind=127.0.0.1,fork,reuseaddr".to_string(),
                "UNIX-CONNECT:/run/cladding/agent/inject/15432/host.sock".to_string(),
            ]
        );
    }

    #[test]
    fn build_inject_agent_exec_command_has_no_runtime_flags() {
        let cmd = build_inject_agent_exec_command(
            "demo-agent-instance",
            &build_inject_agent_listener_args("/run/cladding/agent/inject/15432/host.sock", 15432),
        );
        let args = command_args(&cmd);

        assert_eq!(
            args,
            vec![
                "exec".to_string(),
                "-i".to_string(),
                "demo-agent-instance".to_string(),
                "socat".to_string(),
                "TCP-LISTEN:15432,bind=127.0.0.1,fork,reuseaddr".to_string(),
                "UNIX-CONNECT:/run/cladding/agent/inject/15432/host.sock".to_string(),
            ]
        );
        assert!(
            !args
                .iter()
                .any(|arg| arg == "--runtime" || arg == "--runtime-flag")
        );
    }

    #[test]
    fn build_inject_agent_cleanup_command_targets_exact_pattern() {
        let cmd = build_inject_agent_cleanup_command(
            "demo-agent-instance",
            "socat TCP-LISTEN:15432,bind=127.0.0.1,fork,reuseaddr UNIX-CONNECT:/run/cladding/agent/inject/15432/host.sock",
        );
        let args = command_args(&cmd);

        assert_eq!(cmd.get_program().to_string_lossy(), "podman");
        assert_eq!(
            args,
            vec![
                "exec".to_string(),
                "demo-agent-instance".to_string(),
                "pkill".to_string(),
                "-f".to_string(),
                "socat TCP-LISTEN:15432,bind=127.0.0.1,fork,reuseaddr UNIX-CONNECT:/run/cladding/agent/inject/15432/host.sock".to_string(),
            ]
        );
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

    #[test]
    fn required_config_entries_use_normalized_layout() {
        let config = execution_config(false, false, Vec::new());
        assert_eq!(
            required_config_entries(&config),
            vec!["agent/domains.lst", "agent/host_ports.lst"]
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
                "fs_sandbox",
                "fs_sandbox/main.rego",
            ]
        );
    }

    #[test]
    fn runtime_container_name_matches_direct_runtime_names() {
        assert_eq!(
            runtime_container_name("demo-fs-sandbox"),
            "demo-fs-sandbox-instance"
        );
        assert_eq!(runtime_container_name("demo-agent"), "demo-agent-instance");
    }

    #[test]
    fn agent_runtime_names_cover_pod_and_container_names() {
        assert_eq!(
            agent_runtime_names("demo"),
            ("demo-agent".to_string(), "demo-agent-instance".to_string())
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
            ]
        );
    }

    #[test]
    fn check_legacy_config_entries_detects_old_layout_paths() {
        let temp = create_temp_dir("legacy-config-paths");
        let config_dir = temp.join("config");
        fs::create_dir_all(&config_dir).expect("create config dir");
        fs::write(config_dir.join("cli_domains.lst"), "legacy").expect("write legacy config");

        let context = Context { project_root: temp };

        assert!(check_legacy_config_entries(&context));
    }

    #[test]
    fn report_runtime_script_mismatch_detects_drift() {
        let temp = create_temp_dir("runtime-script-mismatch");
        let scripts_dir = temp.join("runtime/scripts");
        fs::create_dir_all(&scripts_dir).expect("create scripts dir");
        for (rel_path, contents) in scripts_files() {
            let path = scripts_dir.join(rel_path);
            if let Some(parent) = path.parent() {
                fs::create_dir_all(parent).expect("create script parent");
            }
            fs::write(path, contents).expect("write script");
        }

        let context = Context { project_root: temp };
        assert!(!report_runtime_script_mismatch(&context, "error").expect("check scripts"));

        fs::write(
            context
                .project_root
                .join("runtime/scripts/proxy_startup.sh"),
            b"#!/bin/sh\nexit 0\n",
        )
        .expect("modify script");

        assert!(report_runtime_script_mismatch(&context, "error").expect("check scripts"));
    }

    #[test]
    fn check_required_binaries_detects_outdated_tools() {
        let temp = create_temp_dir("outdated-tools");
        let bin_dir = temp.join("tools/bin");
        fs::create_dir_all(&bin_dir).expect("create bin dir");
        write_embedded_tools(&bin_dir).expect("write tools");

        let context = Context {
            project_root: temp.clone(),
        };
        let config = execution_config(true, true, Vec::new());

        assert!(check_required_binaries(&context, &config).is_ok());

        fs::write(bin_dir.join("mcp-run"), b"stale").expect("stale tool");
        assert!(check_required_binaries(&context, &config).is_err());
    }

    #[test]
    fn resolve_container_workdir_uses_default_project_mapping() {
        let temp = create_temp_dir("default-workdir");
        let project_dir = temp.join("workspace");
        let nested_dir = project_dir.join("src/module");
        fs::create_dir_all(&nested_dir).expect("create nested dir");

        let config = ExecutionConfig {
            name: "demo".to_string(),
            use_runsc: false,
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
            use_runsc: false,
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
            use_runsc: false,
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
