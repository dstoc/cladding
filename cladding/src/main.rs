use std::env;
use std::ffi::OsStr;
use std::fs;
use std::io::{self, IsTerminal, Write};
use std::path::{Path, PathBuf};
use std::process::{Command, ExitStatus, Stdio};

#[cfg(unix)]
use std::os::unix::fs::PermissionsExt;

const DEFAULT_CLADDING_BUILD_IMAGE: &str = "localhost/cladding-default:latest";
const DEFAULT_CLI_BUILD_IMAGE: &str = DEFAULT_CLADDING_BUILD_IMAGE;
const DEFAULT_SANDBOX_BUILD_IMAGE: &str = DEFAULT_CLADDING_BUILD_IMAGE;

const CONTAINERFILE_CLADDING: &str = include_str!("../../Containerfile.cladding");
const PODS_YAML: &str = include_str!("../../pods.yaml");

struct EmbeddedFile {
    path: &'static str,
    contents: &'static [u8],
    mode: u32,
}

const EMBEDDED_CONFIG_FILES: &[EmbeddedFile] = &[
    EmbeddedFile {
        path: "cli_domains.lst",
        contents: include_bytes!("../../config-template/cli_domains.lst"),
        mode: 0o644,
    },
    EmbeddedFile {
        path: "cli_host_ports.lst",
        contents: include_bytes!("../../config-template/cli_host_ports.lst"),
        mode: 0o644,
    },
    EmbeddedFile {
        path: "sandbox_domains.lst",
        contents: include_bytes!("../../config-template/sandbox_domains.lst"),
        mode: 0o644,
    },
    EmbeddedFile {
        path: "squid.conf",
        contents: include_bytes!("../../config-template/squid.conf"),
        mode: 0o644,
    },
    EmbeddedFile {
        path: "sandbox_commands/main.rego",
        contents: include_bytes!("../../config-template/sandbox_commands/main.rego"),
        mode: 0o644,
    },
    EmbeddedFile {
        path: "sandbox_commands/curl.rego",
        contents: include_bytes!("../../config-template/sandbox_commands/curl.rego"),
        mode: 0o644,
    },
];

const EMBEDDED_SCRIPTS: &[EmbeddedFile] = &[
    EmbeddedFile {
        path: "jail_cli.sh",
        contents: include_bytes!("../../scripts/jail_cli.sh"),
        mode: 0o755,
    },
    EmbeddedFile {
        path: "jail_sandbox.sh",
        contents: include_bytes!("../../scripts/jail_sandbox.sh"),
        mode: 0o755,
    },
    EmbeddedFile {
        path: "proxy_startup.sh",
        contents: include_bytes!("../../scripts/proxy_startup.sh"),
        mode: 0o755,
    },
];

const CONFIG_TOP_LEVEL: &[&str] = &[
    "cli_domains.lst",
    "cli_host_ports.lst",
    "sandbox_domains.lst",
    "squid.conf",
    "sandbox_commands",
];

#[derive(Debug)]
struct ExitCode(i32);

impl ExitCode {
    fn new(code: i32) -> Self {
        ExitCode(code)
    }
}

#[derive(Debug, Clone)]
struct Config {
    name: String,
    subnet: String,
    sandbox_image: String,
    cli_image: String,
}

#[derive(Debug, Clone)]
struct NetworkSettings {
    network: String,
    network_subnet: String,
    proxy_ip: String,
    sandbox_ip: String,
    cli_ip: String,
    proxy_pod_name: String,
    sandbox_pod_name: String,
    cli_pod_name: String,
}

#[derive(Debug, Clone)]
struct Context {
    project_root: PathBuf,
    cladding_root: Option<PathBuf>,
}

fn main() {
    if let Err(code) = run() {
        std::process::exit(code.0);
    }
}

fn run() -> Result<(), ExitCode> {
    let mut args = env::args().skip(1);
    let cmd = args.next().unwrap_or_else(|| "help".to_string());
    let remaining: Vec<String> = args.collect();

    if matches!(cmd.as_str(), "help" | "-h" | "--help") {
        print_help();
        return Ok(());
    }

    let cwd = env::current_dir().map_err(|err| {
        eprintln!("error: failed to determine current directory: {err}");
        ExitCode::new(1)
    })?;

    let project_root = match find_project_root(&cwd) {
        Some(root) => root,
        None => {
            if cmd == "init" {
                cwd.join(".cladding")
            } else {
                let cwd_display = cwd.display();
                eprintln!(
                    "error: no .cladding directory found in {cwd_display} or any parent directory"
                );
                eprintln!("hint: run 'cladding init' from the project directory to create one");
                return Err(ExitCode::new(1));
            }
        }
    };

    let context = Context {
        project_root,
        cladding_root: find_cladding_root(&cwd),
    };

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
            Err(ExitCode::new(1))
        }
    }
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

fn find_cladding_root(start: &Path) -> Option<PathBuf> {
    let mut candidates = Vec::new();
    if let Ok(exe) = env::current_exe() {
        if let Some(parent) = exe.parent() {
            candidates.push(parent.to_path_buf());
        }
    }
    candidates.push(start.to_path_buf());

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

fn cmd_build(context: &Context, _args: &[String]) -> Result<(), ExitCode> {
    let config = load_cladding_config(context)?;

    let cladding_root = context.cladding_root.clone().ok_or_else(|| {
        eprintln!(
            "error: could not locate cladding repo root (missing crates/mcp-run/Cargo.toml + Containerfile.cladding)"
        );
        ExitCode::new(1)
    })?;

    let host_uid = unsafe { libc::getuid() };
    let host_gid = unsafe { libc::getgid() };

    let tools_dir = context.project_root.join("tools");
    if is_broken_symlink(&tools_dir).unwrap_or(false) {
        eprintln!("missing: tools (broken symlink at {})", tools_dir.display());
        eprintln!("hint: create or relink {}", tools_dir.display());
        return Err(ExitCode::new(1));
    }

    let tools_bin_dir = tools_dir.join("bin");
    fs::create_dir_all(&tools_bin_dir).map_err(|err| {
        eprintln!("error: failed to create tools directory: {err}");
        ExitCode::new(1)
    })?;

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

fn build_mcp_run(cladding_root: &Path) -> Result<(), ExitCode> {
    let status = Command::new("podman")
        .args([
            "run",
            "--rm",
            "-e",
            "CARGO_TARGET_DIR=/work/mcp-run/target",
            "-v",
            &format!(
                "{}:/work/mcp-run",
                cladding_root.join("crates/mcp-run").display()
            ),
            "-w",
            "/work/mcp-run",
            "docker.io/library/rust:latest",
            "cargo",
            "build",
            "--manifest-path",
            "/work/mcp-run/Cargo.toml",
            "--release",
            "--locked",
            "--bin",
            "mcp-run",
            "--bin",
            "run-remote",
        ])
        .status()
        .map_err(|err| {
            eprintln!("error: failed to run podman for build: {err}");
            ExitCode::new(1)
        })?;

    ensure_success(status, "podman run")
}

fn podman_build_image(
    cladding_root: &Path,
    image: &str,
    host_uid: u32,
    host_gid: u32,
) -> Result<(), ExitCode> {
    let mut cmd = Command::new("podman");
    cmd.args([
        "build",
        "--build-arg",
        &format!("UID={host_uid}"),
        "--build-arg",
        &format!("GID={host_gid}"),
        "-t",
        image,
        "-f",
        "-",
        &cladding_root.display().to_string(),
    ])
    .stdin(Stdio::piped());

    let mut child = cmd.spawn().map_err(|err| {
        eprintln!("error: failed to run podman build: {err}");
        ExitCode::new(1)
    })?;

    if let Some(mut stdin) = child.stdin.take() {
        stdin
            .write_all(CONTAINERFILE_CLADDING.as_bytes())
            .and_then(|_| stdin.flush())
            .map_err(|err| {
                eprintln!("error: failed to write Containerfile to podman: {err}");
                ExitCode::new(1)
            })?;
    }

    let status = child.wait().map_err(|err| {
        eprintln!("error: failed to wait on podman build: {err}");
        ExitCode::new(1)
    })?;

    ensure_success(status, "podman build")
}

fn install_binary(src: &Path, dst: &Path) -> Result<(), ExitCode> {
    if !src.exists() {
        eprintln!("missing: built binary ({})", src.display());
        return Err(ExitCode::new(1));
    }

    fs::copy(src, dst).map_err(|err| {
        eprintln!("error: failed to install binary {}: {err}", dst.display());
        ExitCode::new(1)
    })?;

    #[cfg(unix)]
    {
        let perm = fs::Permissions::from_mode(0o755);
        fs::set_permissions(dst, perm).map_err(|err| {
            eprintln!("error: failed to set permissions on {}: {err}", dst.display());
            ExitCode::new(1)
        })?;
    }

    Ok(())
}

fn cmd_init(context: &Context, args: &[String]) -> Result<(), ExitCode> {
    if args.len() > 1 {
        eprintln!("usage: cladding init [name]");
        return Err(ExitCode::new(1));
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
        return Err(ExitCode::new(1));
    }

    let project_root_created = !project_root.exists();
    fs::create_dir_all(project_root).map_err(|err| {
        eprintln!("error: failed to create {}: {err}", project_root.display());
        ExitCode::new(1)
    })?;

    if project_root_created {
        fs::write(&cladding_gitignore, "*\n").map_err(|err| {
            eprintln!("error: failed to write {}: {err}", cladding_gitignore.display());
            ExitCode::new(1)
        })?;
    }

    if config_dir.exists() || path_is_symlink(&config_dir) {
        println!("config already exists: {}", config_dir.display());
    } else {
        fs::create_dir_all(&config_dir).map_err(|err| {
            eprintln!("error: failed to create {}: {err}", config_dir.display());
            ExitCode::new(1)
        })?;
        println!("initialized: {}", config_dir.display());
    }

    materialize_embedded_files(&config_dir, EMBEDDED_CONFIG_FILES)?;

    if scripts_dir.exists() || path_is_symlink(&scripts_dir) {
        println!("scripts already exists: {}", scripts_dir.display());
    } else {
        fs::create_dir_all(&scripts_dir).map_err(|err| {
            eprintln!("error: failed to create {}: {err}", scripts_dir.display());
            ExitCode::new(1)
        })?;
        println!("initialized: {}", scripts_dir.display());
    }

    materialize_embedded_files(&scripts_dir, EMBEDDED_SCRIPTS)?;

    if cladding_config.exists() {
        println!("cladding config already exists: {}", cladding_config.display());
    } else {
        let generated = write_default_cladding_config(name_override)?;
        fs::write(&cladding_config, generated).map_err(|err| {
            eprintln!("error: failed to write {}: {err}", cladding_config.display());
            ExitCode::new(1)
        })?;
        println!("generated: {}", cladding_config.display());
    }

    let config = load_cladding_config(context)?;
    let network_settings = resolve_network_settings(&config)?;
    ensure_network_settings(&network_settings)?;

    Ok(())
}

fn materialize_embedded_files(
    base_dir: &Path,
    files: &[EmbeddedFile],
) -> Result<(), ExitCode> {
    for file in files {
        let target = base_dir.join(file.path);
        if target.exists() {
            continue;
        }
        if let Some(parent) = target.parent() {
            fs::create_dir_all(parent).map_err(|err| {
                eprintln!("error: failed to create {}: {err}", parent.display());
                ExitCode::new(1)
            })?;
        }
        fs::write(&target, file.contents).map_err(|err| {
            eprintln!("error: failed to write {}: {err}", target.display());
            ExitCode::new(1)
        })?;
        #[cfg(unix)]
        {
            let perm = fs::Permissions::from_mode(file.mode);
            fs::set_permissions(&target, perm).map_err(|err| {
                eprintln!("error: failed to set permissions on {}: {err}", target.display());
                ExitCode::new(1)
            })?;
        }
    }

    Ok(())
}

fn cmd_check(context: &Context) -> Result<(), ExitCode> {
    check_required_paths(context)?;
    check_required_binaries(context)?;
    let config = load_cladding_config(context)?;
    resolve_network_settings(&config)?;
    check_required_images(&config)?;
    println!("check: ok");
    Ok(())
}

fn check_required_paths(context: &Context) -> Result<(), ExitCode> {
    let mut missing = false;
    for name in ["config", "home", "tools"] {
        let path = context.project_root.join(name);

        if is_broken_symlink(&path).unwrap_or(false) {
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
        return Err(ExitCode::new(1));
    }

    check_required_config_files(context)
}

fn check_required_config_files(context: &Context) -> Result<(), ExitCode> {
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
        return Err(ExitCode::new(1));
    }

    Ok(())
}

fn check_required_binaries(context: &Context) -> Result<(), ExitCode> {
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
        return Err(ExitCode::new(1));
    }

    Ok(())
}

fn check_required_images(config: &Config) -> Result<(), ExitCode> {
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
                return Err(ExitCode::new(1));
            }
        }
    }

    if missing {
        return Err(ExitCode::new(1));
    }

    Ok(())
}


fn cmd_up(context: &Context) -> Result<(), ExitCode> {
    check_required_paths(context)?;
    check_required_binaries(context)?;

    let config = load_cladding_config(context)?;
    let network_settings = resolve_network_settings(&config)?;
    check_required_images(&config)?;
    ensure_network_settings(&network_settings)?;

    let rendered = render_pods_yaml(context, &config, &network_settings);
    podman_play_kube(&rendered, &network_settings, false)
}

fn cmd_down(context: &Context) -> Result<(), ExitCode> {
    let config = load_cladding_config(context)?;
    let network_settings = resolve_network_settings(&config)?;
    let rendered = render_pods_yaml(context, &config, &network_settings);
    podman_play_kube(&rendered, &network_settings, true)
}

fn cmd_destroy(context: &Context) -> Result<(), ExitCode> {
    let config = load_cladding_config(context)?;
    let network_settings = resolve_network_settings(&config)?;

    let status = Command::new("podman")
        .args([
            "rm",
            "-f",
            &network_settings.cli_pod_name,
            &network_settings.sandbox_pod_name,
            &network_settings.proxy_pod_name,
        ])
        .status()
        .map_err(|err| {
            eprintln!("error: failed to run podman rm: {err}");
            ExitCode::new(1)
        })?;

    ensure_success(status, "podman rm")
}

fn cmd_run(context: &Context, args: &[String]) -> Result<(), ExitCode> {
    if args.is_empty() {
        eprintln!("usage: cladding run <command> [args...]");
        return Err(ExitCode::new(1));
    }

    let config = load_cladding_config(context)?;
    let network_settings = resolve_network_settings(&config)?;

    let project_dir = context
        .project_root
        .parent()
        .ok_or_else(|| {
            eprintln!("error: could not resolve project directory");
            ExitCode::new(1)
        })?
        .to_path_buf();

    let cwd = env::current_dir().map_err(|err| {
        eprintln!("error: failed to determine current directory: {err}");
        ExitCode::new(1)
    })?;

    let project_dir = canonicalize_path(&project_dir)?;
    let cwd = canonicalize_path(&cwd)?;

    let workdir_rel = cwd.strip_prefix(&project_dir).map_err(|_| {
        eprintln!(
            "error: could not determine current path relative to project dir ({}): {}",
            project_dir.display(),
            cwd.display()
        );
        eprintln!("hint: run cladding from {} or one of its subdirectories", project_dir.display());
        ExitCode::new(1)
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

    let status = cmd.status().map_err(|err| {
        eprintln!("error: failed to run podman exec: {err}");
        ExitCode::new(1)
    })?;

    if let Some(code) = status.code() {
        if code == 0 {
            Ok(())
        } else {
            Err(ExitCode::new(code))
        }
    } else {
        Err(ExitCode::new(1))
    }
}

fn cmd_reload_proxy(context: &Context) -> Result<(), ExitCode> {
    let config = load_cladding_config(context)?;
    let network_settings = resolve_network_settings(&config)?;

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
        .map_err(|err| {
            eprintln!("error: failed to run podman exec: {err}");
            ExitCode::new(1)
        })?;

    ensure_success(status, "podman exec")
}

fn load_cladding_config(context: &Context) -> Result<Config, ExitCode> {
    let config_path = context.project_root.join("cladding.json");

    if !config_path.exists() {
        eprintln!("missing: cladding.json ({})", config_path.display());
        eprintln!("hint: run cladding init");
        return Err(ExitCode::new(1));
    }

    let raw = fs::read_to_string(&config_path).map_err(|err| {
        eprintln!("error: failed to read {}: {err}", config_path.display());
        ExitCode::new(1)
    })?;

    let parsed: serde_json::Value = match serde_json::from_str(&raw) {
        Ok(parsed) => parsed,
        Err(_) => {
            eprintln!("error: cladding.json must include string key: name");
            return Err(ExitCode::new(1));
        }
    };

    let name = get_config_string(&parsed, "name")?;
    let subnet = get_config_string(&parsed, "subnet")?;
    let sandbox_image = get_config_string(&parsed, "sandbox_image")?;
    let cli_image = get_config_string(&parsed, "cli_image")?;

    if !is_lowercase_alnum(&name) {
        eprintln!("error: config key 'name' must be lowercase alphanumeric ([a-z0-9]+)");
        return Err(ExitCode::new(1));
    }

    Ok(Config {
        name,
        subnet,
        sandbox_image,
        cli_image,
    })
}

fn get_config_string(parsed: &serde_json::Value, key: &str) -> Result<String, ExitCode> {
    parsed
        .get(key)
        .and_then(|value| value.as_str())
        .map(|value| value.to_string())
        .ok_or_else(|| {
            eprintln!("error: cladding.json must include string key: {key}");
            ExitCode::new(1)
        })
}

fn is_lowercase_alnum(name: &str) -> bool {
    !name.is_empty() && name.chars().all(|c| c.is_ascii_lowercase() || c.is_ascii_digit())
}

fn resolve_network_settings(config: &Config) -> Result<NetworkSettings, ExitCode> {
    let subnet = config.subnet.trim();
    let (subnet_ip, subnet_prefix) = match subnet.split_once('/') {
        Some((ip, prefix)) if !ip.is_empty() && !prefix.is_empty() => (ip, prefix),
        _ => {
            eprintln!(
                "error: config key 'subnet' must be in CIDR notation (example: 10.90.0.0/24)"
            );
            return Err(ExitCode::new(1));
        }
    };

    let subnet_prefix: u8 = subnet_prefix.parse().map_err(|_| {
        eprintln!("error: subnet prefix must be numeric: {}", config.subnet);
        ExitCode::new(1)
    })?;

    if subnet_prefix > 32 {
        eprintln!("error: subnet prefix out of range (0-32): {}", config.subnet);
        return Err(ExitCode::new(1));
    }

    let subnet_ip_int = ipv4_to_int(subnet_ip).ok_or_else(|| {
        eprintln!("error: invalid IPv4 subnet address: {}", config.subnet);
        ExitCode::new(1)
    })?;

    let subnet_mask_int = if subnet_prefix == 0 {
        0
    } else {
        (!0u32) << (32 - subnet_prefix)
    };
    let subnet_network_int = subnet_ip_int & subnet_mask_int;
    let subnet_broadcast_int = subnet_network_int | (!subnet_mask_int);

    let proxy_ip_int = subnet_network_int + 2;
    let sandbox_ip_int = subnet_network_int + 3;
    let cli_ip_int = subnet_network_int + 4;

    if cli_ip_int >= subnet_broadcast_int {
        eprintln!(
            "error: subnet too small, need usable IPs for gateway + 3 pods: {}",
            config.subnet
        );
        return Err(ExitCode::new(1));
    }

    let network = format!("{}_cladding_net", config.name);
    let network_subnet = format!("{}/{}", int_to_ipv4(subnet_network_int), subnet_prefix);
    let proxy_ip = int_to_ipv4(proxy_ip_int);
    let sandbox_ip = int_to_ipv4(sandbox_ip_int);
    let cli_ip = int_to_ipv4(cli_ip_int);

    Ok(NetworkSettings {
        network,
        network_subnet,
        proxy_ip,
        sandbox_ip,
        cli_ip,
        proxy_pod_name: format!("{}-proxy-pod", config.name),
        sandbox_pod_name: format!("{}-sandbox-pod", config.name),
        cli_pod_name: format!("{}-cli-pod", config.name),
    })
}

fn ensure_network_settings(network_settings: &NetworkSettings) -> Result<(), ExitCode> {
    let status = Command::new("podman")
        .args(["network", "exists", &network_settings.network])
        .status()
        .map_err(|err| {
            eprintln!("error: failed to check existing networks via podman: {err}");
            ExitCode::new(1)
        })?;

    match status.code() {
        Some(0) => {
            let output = Command::new("podman")
                .args(["network", "inspect", &network_settings.network])
                .output()
                .map_err(|err| {
                    eprintln!("error: failed to inspect podman network: {err}");
                    ExitCode::new(1)
                })?;

            if !output.status.success() {
                return ensure_success(output.status, "podman network inspect");
            }

            let stdout = String::from_utf8_lossy(&output.stdout);
            if !stdout.contains(&format!("\"subnet\": \"{}\"", network_settings.network_subnet))
            {
                eprintln!(
                    "error: network {} exists but is not on {}",
                    network_settings.network, network_settings.network_subnet
                );
                eprintln!(
                    "hint: update cladding.json subnet to match, or run 'podman network rm {}' and retry",
                    network_settings.network
                );
                return Err(ExitCode::new(1));
            }
        }
        Some(1) => {
            let status = Command::new("podman")
                .args([
                    "network",
                    "create",
                    "--subnet",
                    &network_settings.network_subnet,
                    &network_settings.network,
                ])
                .status()
                .map_err(|err| {
                    eprintln!("error: failed to create podman network: {err}");
                    ExitCode::new(1)
                })?;
            ensure_success(status, "podman network create")?;
        }
        _ => {
            eprintln!("error: failed to check existing networks via podman");
            return Err(ExitCode::new(1));
        }
    }

    Ok(())
}

fn render_pods_yaml(
    context: &Context,
    config: &Config,
    network: &NetworkSettings,
) -> String {
    PODS_YAML
        .replace("PROJECT_ROOT", &context.project_root.display().to_string())
        .replace("CLADDING_ROOT", &context.project_root.display().to_string())
        .replace("REPLACE_PROXY_POD_NAME", &network.proxy_pod_name)
        .replace("REPLACE_SANDBOX_POD_NAME", &network.sandbox_pod_name)
        .replace("REPLACE_CLI_POD_NAME", &network.cli_pod_name)
        .replace("REPLACE_SANDBOX_IMAGE", &config.sandbox_image)
        .replace("REPLACE_CLI_IMAGE", &config.cli_image)
        .replace("REPLACE_PROXY_IP", &network.proxy_ip)
        .replace("REPLACE_SANDBOX_IP", &network.sandbox_ip)
        .replace("REPLACE_CLI_IP", &network.cli_ip)
}

fn podman_play_kube(
    rendered: &str,
    network: &NetworkSettings,
    down: bool,
) -> Result<(), ExitCode> {
    let mut cmd = Command::new("podman");
    cmd.arg("play").arg("kube");
    if down {
        cmd.arg("--down");
    } else {
        cmd.args([
            "--network",
            &network.network,
            "--ip",
            &network.proxy_ip,
            "--ip",
            &network.sandbox_ip,
            "--ip",
            &network.cli_ip,
        ]);
    }
    cmd.arg("-");
    cmd.stdin(Stdio::piped());

    let mut child = cmd.spawn().map_err(|err| {
        eprintln!("error: failed to run podman play kube: {err}");
        ExitCode::new(1)
    })?;

    if let Some(mut stdin) = child.stdin.take() {
        stdin.write_all(rendered.as_bytes()).map_err(|err| {
            eprintln!("error: failed to write pods.yaml to podman: {err}");
            ExitCode::new(1)
        })?;
    }

    let status = child.wait().map_err(|err| {
        eprintln!("error: failed to wait on podman play kube: {err}");
        ExitCode::new(1)
    })?;

    ensure_success(status, "podman play kube")
}

fn write_default_cladding_config(name_override: Option<&str>) -> Result<String, ExitCode> {
    if !command_exists("podman") {
        eprintln!("missing: podman (required for cladding init to choose name/subnet)");
        return Err(ExitCode::new(1));
    }

    let name = if let Some(name_override) = name_override {
        normalize_cladding_name_arg(name_override)?
    } else {
        derive_cladding_name_from_pwd()?
    };

    let network_name = format!("{}_cladding_net", name);
    let status = Command::new("podman")
        .args(["network", "exists", &network_name])
        .status()
        .map_err(|err| {
            eprintln!("error: failed to check existing networks via podman: {err}");
            ExitCode::new(1)
        })?;

    match status.code() {
        Some(0) => {
            eprintln!("error: network already exists for generated name: {network_name}");
            eprintln!(
                "hint: run cladding init from a different directory name, or remove the existing network"
            );
            return Err(ExitCode::new(1));
        }
        Some(1) => {}
        _ => {
            eprintln!("error: failed to check existing networks via podman");
            return Err(ExitCode::new(1));
        }
    }

    let subnet = pick_available_subnet().map_err(|code| {
        match code {
            1 => eprintln!("error: failed to inspect existing network subnets via podman"),
            2 => eprintln!(
                "error: could not find an unused subnet in 10.90.0.0/16 (/24 slices)"
            ),
            _ => eprintln!("error: unexpected failure while selecting subnet"),
        }
        ExitCode::new(1)
    })?;

    Ok(format!(
        "{{\n  \"sandbox_image\": \"{}\",\n  \"cli_image\": \"{}\",\n  \"name\": \"{}\",\n  \"subnet\": \"{}\"\n}}\n",
        DEFAULT_SANDBOX_BUILD_IMAGE, DEFAULT_CLI_BUILD_IMAGE, name, subnet
    ))
}

fn derive_cladding_name_from_pwd() -> Result<String, ExitCode> {
    let cwd = env::current_dir().map_err(|err| {
        eprintln!("error: failed to determine current directory: {err}");
        ExitCode::new(1)
    })?;
    let raw_name = cwd
        .file_name()
        .and_then(OsStr::to_str)
        .unwrap_or("");
    let name = raw_name
        .chars()
        .filter(|c| c.is_ascii_alphanumeric())
        .map(|c| c.to_ascii_lowercase())
        .collect::<String>();

    if name.is_empty() {
        eprintln!(
            "error: could not derive an alphanumeric name from directory: {}",
            cwd.display()
        );
        return Err(ExitCode::new(1));
    }

    Ok(name)
}

fn normalize_cladding_name_arg(name_arg: &str) -> Result<String, ExitCode> {
    let name = name_arg.to_ascii_lowercase();
    if name.is_empty() || !name.chars().all(|c| c.is_ascii_alphanumeric()) {
        eprintln!("error: init name must be alphanumeric ([a-zA-Z0-9]+)");
        return Err(ExitCode::new(1));
    }
    Ok(name)
}

fn list_podman_ipv4_subnets() -> Result<Vec<String>, ExitCode> {
    let output = Command::new("podman")
        .args(["network", "ls", "--format", "{{.Name}}"])
        .output()
        .map_err(|err| {
            eprintln!("error: failed to list podman networks: {err}");
            ExitCode::new(1)
        })?;

    if !output.status.success() {
        return ensure_success(output.status, "podman network ls").map(|_| Vec::new());
    }

    let stdout = String::from_utf8_lossy(&output.stdout);
    let mut subnets = Vec::new();

    for name in stdout.lines().map(str::trim).filter(|s| !s.is_empty()) {
        let output = Command::new("podman")
            .args([
                "network",
                "inspect",
                "-f",
                "{{range .Subnets}}{{.Subnet}}{{\"\\n\"}}{{end}}",
                name,
            ])
            .output();

        let output = match output {
            Ok(output) => output,
            Err(_) => continue,
        };

        if !output.status.success() {
            continue;
        }

        let stdout = String::from_utf8_lossy(&output.stdout);
        for line in stdout.lines().map(str::trim) {
            if is_ipv4_cidr(line) {
                subnets.push(line.to_string());
            }
        }
    }

    Ok(subnets)
}

fn pick_available_subnet() -> Result<String, i32> {
    let used_subnets = list_podman_ipv4_subnets().map_err(|_| 1)?;
    for i in 0..=255 {
        let candidate = format!("10.90.{i}.0/24");
        if !used_subnets.iter().any(|subnet| subnet == &candidate) {
            return Ok(candidate);
        }
    }

    Err(2)
}

fn is_ipv4_cidr(value: &str) -> bool {
    let (ip, prefix) = match value.split_once('/') {
        Some(parts) => parts,
        None => return false,
    };
    if prefix.parse::<u8>().ok().filter(|p| *p <= 32).is_none() {
        return false;
    }
    ipv4_to_int(ip).is_some()
}

fn ipv4_to_int(ip: &str) -> Option<u32> {
    let mut parts = ip.split('.');
    let a = parts.next()?.parse::<u8>().ok()?;
    let b = parts.next()?.parse::<u8>().ok()?;
    let c = parts.next()?.parse::<u8>().ok()?;
    let d = parts.next()?.parse::<u8>().ok()?;
    if parts.next().is_some() {
        return None;
    }

    Some(((a as u32) << 24) | ((b as u32) << 16) | ((c as u32) << 8) | (d as u32))
}

fn int_to_ipv4(value: u32) -> String {
    format!(
        "{}.{}.{}.{}",
        (value >> 24) & 0xff,
        (value >> 16) & 0xff,
        (value >> 8) & 0xff,
        value & 0xff
    )
}

fn command_exists(command: &str) -> bool {
    env::var_os("PATH").map_or(false, |paths| {
        env::split_paths(&paths).any(|path| {
            let candidate = path.join(command);
            candidate.is_file()
        })
    })
}

fn path_is_symlink(path: &Path) -> bool {
    fs::symlink_metadata(path)
        .map(|meta| meta.file_type().is_symlink())
        .unwrap_or(false)
}

fn is_broken_symlink(path: &Path) -> io::Result<bool> {
    let meta = fs::symlink_metadata(path)?;
    if meta.file_type().is_symlink() {
        return Ok(fs::metadata(path).is_err());
    }
    Ok(false)
}

fn is_executable(path: &Path) -> bool {
    if let Ok(meta) = fs::metadata(path) {
        #[cfg(unix)]
        {
            return meta.permissions().mode() & 0o111 != 0;
        }
        #[cfg(not(unix))]
        {
            return meta.is_file();
        }
    }
    false
}

fn ensure_success(status: ExitStatus, context: &str) -> Result<(), ExitCode> {
    if status.success() {
        return Ok(());
    }

    let code = status.code().unwrap_or(1);
    eprintln!("error: {context} failed (exit code {code})");
    Err(ExitCode::new(code))
}

fn canonicalize_path(path: &Path) -> Result<PathBuf, ExitCode> {
    fs::canonicalize(path).map_err(|err| {
        eprintln!("error: failed to resolve {}: {err}", path.display());
        ExitCode::new(1)
    })
}

fn image_is_buildable_by_cladding(image: &str) -> bool {
    image == DEFAULT_CLADDING_BUILD_IMAGE
}
