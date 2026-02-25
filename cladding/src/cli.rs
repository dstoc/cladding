use cladding::assets::{
    config_top_level_entries, materialize_config, materialize_scripts, render_pods_yaml,
    scripts_top_level_entries, write_embedded_tools,
};
use cladding::config::{load_cladding_config, write_default_cladding_config, Config};
use cladding::error::{Error, Result};
use cladding::fs_utils::{canonicalize_path, is_broken_symlink, is_executable, path_is_symlink};
use cladding::network::resolve_network_settings;
use cladding::podman::{ensure_network_settings, podman_build_image, podman_play_kube};
use anyhow::Context as _;
use clap::{ArgAction, Parser, Subcommand};
use std::env;
use std::fs;
use std::io::{self, IsTerminal};
use std::path::{Path, PathBuf};
use std::process::Command;

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

#[derive(Subcommand)]
enum CommandSpec {
    /// Build local container images
    Build,
    /// Create config and default mount directories
    Init { name: Option<String> },
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
    /// Reload the squid proxy configuration
    ReloadProxy,
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
        CommandSpec::Up => cmd_up(&context),
        CommandSpec::Down => cmd_down(&context),
        CommandSpec::Destroy => cmd_destroy(&context),
        CommandSpec::Run { env, args } => cmd_run(&context, &env, &args),
        CommandSpec::ReloadProxy => cmd_reload_proxy(&context),
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
    fs::create_dir_all(&tools_bin_dir)
        .with_context(|| "failed to create tools directory")?;

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

fn cmd_init(context: &Context, name_override: Option<&str>) -> Result<()> {
    let project_root = &context.project_root;
    let config_dir = project_root.join("config");
    let scripts_dir = project_root.join("scripts");
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

    materialize_scripts(&scripts_dir)?;

    if cladding_config.exists() {
        println!("cladding config already exists: {}", cladding_config.display());
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

    let config = load_cladding_config(&context.project_root)?;
    let network_settings = resolve_network_settings(&config.name, &config.subnet)?;
    ensure_network_settings(&network_settings)?;

    Ok(())
}

fn cmd_check(context: &Context) -> Result<()> {
    check_required_paths(context)?;
    check_required_binaries(context)?;
    let config = load_cladding_config(&context.project_root)?;
    resolve_network_settings(&config.name, &config.subnet)?;
    check_required_images(&config)?;
    println!("check: ok");
    Ok(())
}

fn check_required_paths(context: &Context) -> Result<()> {
    let mut missing = false;
    for name in ["config", "home", "scripts", "tools"] {
        let path = context.project_root.join(name);

        if is_broken_symlink(&path)? {
            eprintln!("missing: {name} (broken symlink at {})", path.display());
            if name == "config" {
                eprintln!("hint: run cladding init");
            } else if name == "scripts" {
                eprintln!("hint: run cladding init");
            } else {
                eprintln!("hint: create or relink {}", path.display());
            }
            missing = true;
            continue;
        }

        if !path.exists() {
            eprintln!("missing: {name} ({})", path.display());
            if name == "config" {
                eprintln!("hint: run cladding init");
            } else if name == "scripts" {
                eprintln!("hint: run cladding init");
            } else {
                eprintln!("hint: mkdir -p {} (or symlink it)", path.display());
            }
            missing = true;
        }
    }

    if missing {
        return Err(Error::message("missing required paths"));
    }

    check_required_config_files(context)?;
    check_required_scripts_files(context)
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
                    eprintln!("hint: pull/tag image '{image}', or set cladding.json image to a supported build target and run cladding build");
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

fn cmd_up(context: &Context) -> Result<()> {
    check_required_paths(context)?;
    check_required_binaries(context)?;

    let config = load_cladding_config(&context.project_root)?;
    let network_settings = resolve_network_settings(&config.name, &config.subnet)?;
    check_required_images(&config)?;
    ensure_network_settings(&network_settings)?;

    let rendered = render_pods_yaml(
        &context.project_root,
        &config.sandbox_image,
        &config.cli_image,
        &network_settings.proxy_pod_name,
        &network_settings.sandbox_pod_name,
        &network_settings.cli_pod_name,
        &network_settings.proxy_ip,
        &network_settings.sandbox_ip,
        &network_settings.cli_ip,
    );
    podman_play_kube(&rendered, &network_settings, false)
}

fn cmd_down(context: &Context) -> Result<()> {
    let config = load_cladding_config(&context.project_root)?;
    let network_settings = resolve_network_settings(&config.name, &config.subnet)?;
    let rendered = render_pods_yaml(
        &context.project_root,
        &config.sandbox_image,
        &config.cli_image,
        &network_settings.proxy_pod_name,
        &network_settings.sandbox_pod_name,
        &network_settings.cli_pod_name,
        &network_settings.proxy_ip,
        &network_settings.sandbox_ip,
        &network_settings.cli_ip,
    );
    podman_play_kube(&rendered, &network_settings, true)
}

fn cmd_destroy(context: &Context) -> Result<()> {
    let config = load_cladding_config(&context.project_root)?;
    let network_settings = resolve_network_settings(&config.name, &config.subnet)?;

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

    cladding::podman::ensure_success(status, "podman rm")
}

fn cmd_run(context: &Context, env_vars: &[String], args: &[String]) -> Result<()> {
    if args.is_empty() {
        eprintln!("usage: cladding run [--env KEY[=VALUE] ...] <command> [args...]");
        return Err(Error::message("missing run command"));
    }

    let config = load_cladding_config(&context.project_root)?;
    let network_settings = resolve_network_settings(&config.name, &config.subnet)?;

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
        eprintln!("hint: run cladding from {} or one of its subdirectories", project_dir.display());
        Error::message("invalid working directory")
    })?;

    let mut container_workdir = PathBuf::from("/home/user/workspace");
    if !workdir_rel.as_os_str().is_empty() {
        container_workdir = container_workdir.join(workdir_rel);
    }

    let interactive = io::stdin().is_terminal() && io::stdout().is_terminal();
    let container_name = format!("{}-cli-app", network_settings.cli_pod_name);

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

    cmd.arg(&container_name);

    for arg in args {
        cmd.arg(arg);
    }

    let status = cmd.status().with_context(|| "failed to run podman exec")?;

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
    let network_settings = resolve_network_settings(&config.name, &config.subnet)?;

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

fn image_is_buildable_by_cladding(image: &str) -> bool {
    image == DEFAULT_CLADDING_BUILD_IMAGE
}
