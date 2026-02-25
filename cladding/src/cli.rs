use crate::assets::{materialize_embedded_files, render_pods_yaml, CONFIG_TOP_LEVEL, EMBEDDED_CONFIG_FILES, EMBEDDED_SCRIPTS};
use crate::config::{load_cladding_config, write_default_cladding_config, Config};
use crate::error::{Error, Result};
use crate::fs_utils::{canonicalize_path, is_broken_symlink, is_executable, path_is_symlink, set_permissions};
use crate::network::resolve_network_settings;
use crate::podman::{
    build_mcp_run, ensure_network_settings, podman_build_image, podman_play_kube,
};
use anyhow::Context as _;
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

pub fn run() -> Result<()> {
    let mut args = env::args().skip(1);
    let cmd = args.next().unwrap_or_else(|| "help".to_string());
    let remaining: Vec<String> = args.collect();

    if matches!(cmd.as_str(), "help" | "-h" | "--help") {
        print_help();
        return Ok(());
    }

    let cwd = env::current_dir().with_context(|| "failed to determine current directory")?;

    let project_root = match find_project_root(&cwd) {
        Some(root) => root,
        None => {
            if cmd == "init" {
                cwd.join(".cladding")
            } else {
                eprintln!(
                    "error: no .cladding directory found in {} or any parent directory",
                    cwd.display()
                );
                eprintln!("hint: run 'cladding init' from the project directory to create one");
                return Err(Error::message("missing .cladding"));
            }
        }
    };

    let context = Context { project_root };

    match cmd.as_str() {
        "build" => cmd_build(&context, &remaining),
        "init" => cmd_init(&context, &remaining),
        "check" => cmd_check(&context),
        "up" => cmd_up(&context),
        "down" => cmd_down(&context),
        "destroy" => cmd_destroy(&context),
        "run" => cmd_run(&context, &remaining),
        "reload-proxy" => cmd_reload_proxy(&context),
        _ => {
            eprintln!("Unknown command: {cmd}");
            eprintln!();
            print_help_to_stderr();
            Err(Error::message("unknown command"))
        }
    }
}

pub fn print_error_and_exit(err: Error) -> ! {
    eprintln!("{err}");
    std::process::exit(err.exit_code());
}

fn print_help() {
    println!(
        "Usage: cladding <command> [args...]\n\nCommands:\n  build                Build local container images\n  init [name]          Create config and default mount directories\n  check                Check requirements\n  up                   Start the system\n  down                 Stop the system\n  destroy              Force-remove running containers\n  run                  Run a command in the cli container\n  reload-proxy         Reload the squid proxy configuration\n  help                 Show this help"
    );
}

fn print_help_to_stderr() {
    eprintln!(
        "Usage: cladding <command> [args...]\n\nCommands:\n  build                Build local container images\n  init [name]          Create config and default mount directories\n  check                Check requirements\n  up                   Start the system\n  down                 Stop the system\n  destroy              Force-remove running containers\n  run                  Run a command in the cli container\n  reload-proxy         Reload the squid proxy configuration\n  help                 Show this help"
    );
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

fn cmd_build(context: &Context, _args: &[String]) -> Result<()> {
    let config = load_cladding_config(&context.project_root)?;

    let cladding_root = find_repo_root().ok_or_else(|| {
        eprintln!(
            "error: could not locate cladding repo root (missing crates/mcp-run/Cargo.toml + Containerfile.cladding)"
        );
        Error::message("missing repo root")
    })?;

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

    build_mcp_run(&cladding_root)?;

    install_binary(
        &cladding_root
            .join("crates/mcp-run/target/release/mcp-run"),
        &tools_bin_dir.join("mcp-run"),
    )?;
    install_binary(
        &cladding_root
            .join("crates/mcp-run/target/release/run-remote"),
        &tools_bin_dir.join("run-with-network"),
    )?;

    let mut cli_image_built = false;
    if config.cli_image == DEFAULT_CLI_BUILD_IMAGE {
        podman_build_image(&cladding_root, &config.cli_image, host_uid, host_gid)?;
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
            podman_build_image(&cladding_root, &config.sandbox_image, host_uid, host_gid)?;
        }
    } else {
        println!(
            "skip: not building sandbox image (config sandbox_image is {}, build target is {})",
            config.sandbox_image, DEFAULT_CLADDING_BUILD_IMAGE
        );
    }

    Ok(())
}

fn install_binary(src: &Path, dst: &Path) -> Result<()> {
    if !src.exists() {
        eprintln!("missing: built binary ({})", src.display());
        return Err(Error::message("missing built binary"));
    }

    fs::copy(src, dst)
        .with_context(|| format!("failed to install binary {}", dst.display()))?;

    set_permissions(dst, 0o755)?;

    Ok(())
}

fn cmd_init(context: &Context, args: &[String]) -> Result<()> {
    if args.len() > 1 {
        eprintln!("usage: cladding init [name]");
        return Err(Error::message("invalid init args"));
    }

    let name_override = args.get(0).map(String::as_str);
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

    materialize_embedded_files(&config_dir, EMBEDDED_CONFIG_FILES)?;

    if scripts_dir.exists() || path_is_symlink(&scripts_dir) {
        println!("scripts already exists: {}", scripts_dir.display());
    } else {
        fs::create_dir_all(&scripts_dir)
            .with_context(|| format!("failed to create {}", scripts_dir.display()))?;
        println!("initialized: {}", scripts_dir.display());
    }

    materialize_embedded_files(&scripts_dir, EMBEDDED_SCRIPTS)?;

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
    for name in ["config", "home", "tools"] {
        let path = context.project_root.join(name);

        if is_broken_symlink(&path)? {
            eprintln!("missing: {name} (broken symlink at {})", path.display());
            if name == "config" {
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
            } else {
                eprintln!("hint: mkdir -p {} (or symlink it)", path.display());
            }
            missing = true;
        }
    }

    if missing {
        return Err(Error::message("missing required paths"));
    }

    check_required_config_files(context)
}

fn check_required_config_files(context: &Context) -> Result<()> {
    let dst = context.project_root.join("config");
    let mut missing = false;

    for name in CONFIG_TOP_LEVEL {
        let path = dst.join(name);
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

    crate::podman::ensure_success(status, "podman rm")
}

fn cmd_run(context: &Context, args: &[String]) -> Result<()> {
    if args.is_empty() {
        eprintln!("usage: cladding run <command> [args...]");
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
            &format!("{}-cli-app", network_settings.cli_pod_name),
        ]);
    } else {
        cmd.args([
            "exec",
            "-i",
            "-w",
            &container_workdir.display().to_string(),
            "--env",
            "LANG=C.UTF-8",
            &format!("{}-cli-app", network_settings.cli_pod_name),
        ]);
    }

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

    crate::podman::ensure_success(status, "podman exec")
}

fn image_is_buildable_by_cladding(image: &str) -> bool {
    image == DEFAULT_CLADDING_BUILD_IMAGE
}

fn find_repo_root() -> Option<PathBuf> {
    let mut candidates = Vec::new();
    if let Ok(exe) = env::current_exe() {
        if let Some(parent) = exe.parent() {
            candidates.push(parent.to_path_buf());
        }
    }

    if let Ok(cwd) = env::current_dir() {
        candidates.push(cwd);
    }

    for candidate in candidates {
        if let Some(found) = walk_up_for_repo_root(&candidate) {
            return Some(found);
        }
    }

    None
}

fn walk_up_for_repo_root(start: &Path) -> Option<PathBuf> {
    let mut current = start;
    loop {
        let mcp_run = current.join("crates/mcp-run").join("Cargo.toml");
        let containerfile = current.join("Containerfile.cladding");
        if mcp_run.is_file() && containerfile.is_file() {
            return Some(current.to_path_buf());
        }
        match current.parent() {
            Some(parent) => current = parent,
            None => return None,
        }
    }
}
