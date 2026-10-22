use super::mounts::parse_mounts_v2;
use super::schema::{
    AgentDocument, CladdingConfigDocument, ComponentDocument, ImageBuildDocument, ProxyDocument,
    SessionConfigDocument,
};
use super::types::{
    DEFAULT_AGENT_SESSION_CONFIG, DEFAULT_COMPONENT_IMAGE, DEFAULT_NW_SANDBOX_SESSION_CONFIG,
    DEFAULT_PROXY_IMAGE, ExecutionComponentConfig, ExecutionConfig, ExecutionProxyConfig,
    ImageBuildConfig,
};
use crate::error::{Error, Result};
use anyhow::Context as _;
use std::fs;
use std::path::{Path, PathBuf};

/// Load `cladding.json` from a selected `.cladding` directory.
pub fn load_cladding_config_v2(project_root: &Path) -> Result<ExecutionConfig> {
    let config_path = project_root.join("cladding.json");

    if !config_path.exists() {
        eprintln!("missing: cladding.json ({})", config_path.display());
        eprintln!("hint: run cladding init");
        return Err(Error::message("missing cladding.json"));
    }

    load_cladding_config_v2_from_path(&config_path)
}

/// Load a JSON config file and resolve its relative paths from its parent directory.
pub fn load_cladding_config_v2_from_path(config_path: &Path) -> Result<ExecutionConfig> {
    if !config_path.is_file() {
        eprintln!(
            "error: configuration file does not exist or is not a file: {}",
            config_path.display()
        );
        eprintln!("hint: pass a JSON config file with --config FILE, or use --config - for stdin");
        return Err(Error::message("invalid configuration file path"));
    }

    let raw = fs::read_to_string(config_path)
        .with_context(|| format!("failed to read {}", config_path.display()))?;
    load_cladding_config_v2_from_str(config_path, &raw)
}

/// Parse JSON and resolve relative paths from the parent of `config_path`.
///
/// For stdin input, pass a synthetic path whose parent is the required base directory.
pub fn load_cladding_config_v2_from_str(config_path: &Path, raw: &str) -> Result<ExecutionConfig> {
    let mut deserializer = serde_json::Deserializer::from_str(raw);
    let document = serde_path_to_error::deserialize::<_, CladdingConfigDocument>(&mut deserializer)
        .map_err(|error| {
            let path = error.path().to_string();
            config_deserialization_error(config_path, Some(&path), error.into_inner())
        })?;
    deserializer
        .end()
        .map_err(|error| config_deserialization_error(config_path, None, error))?;

    let name = document.name;
    if !is_lowercase_alnum(&name) {
        return Err(invalid_config(
            config_path,
            "config key 'name' must be lowercase alphanumeric ([a-z0-9]+)",
        ));
    }

    let fake_credential_env = parse_fake_credential_env(document.fake_credential_env, config_path)?;

    let agent = parse_agent(document.agent, &name, config_path)?;
    let nw_sandbox = document
        .nw_sandbox
        .map(|component| parse_component(component, "nw_sandbox", &name, config_path))
        .transpose()?;
    let fs_sandbox = document
        .fs_sandbox
        .map(|component| parse_component(component, "fs_sandbox", &name, config_path))
        .transpose()?;
    let proxy = parse_proxy(document.proxy, &name, config_path)?;
    let execution_config = ExecutionConfig {
        name,
        use_runsc: document.use_runsc,
        fake_credential_env,
        agent,
        nw_sandbox,
        fs_sandbox,
        proxy,
        mounts: Vec::new(),
    };
    let config_dir = resolve_path(
        Path::new("."),
        config_path.parent().unwrap_or(Path::new(".")),
    );
    let mounts = parse_mounts_v2(
        &config_dir,
        &document.mounts,
        config_path,
        &execution_config,
    )?;
    Ok(ExecutionConfig {
        mounts,
        ..execution_config
    })
}

fn parse_fake_credential_env(
    mappings: std::collections::BTreeMap<String, String>,
    config_path: &Path,
) -> Result<std::collections::BTreeMap<String, String>> {
    for (environment_name, credential_name) in &mappings {
        if !is_environment_variable_name(environment_name) {
            return Err(invalid_config(
                config_path,
                "fake_credential_env keys must be valid environment variable names",
            ));
        }
        if !cladding_fake_credentials::is_credential_identifier(credential_name) {
            return Err(invalid_config(
                config_path,
                "fake_credential_env values must be valid Baffle credential identifiers",
            ));
        }
    }
    Ok(mappings)
}

fn is_environment_variable_name(name: &str) -> bool {
    let mut chars = name.chars();
    matches!(chars.next(), Some(ch) if ch.is_ascii_alphabetic() || ch == '_')
        && chars.all(|ch| ch.is_ascii_alphanumeric() || ch == '_')
}

fn config_deserialization_error(
    config_path: &Path,
    path: Option<&str>,
    error: serde_json::Error,
) -> Error {
    let category = match error.classify() {
        serde_json::error::Category::Syntax => "JSON syntax error",
        serde_json::error::Category::Data => "configuration type or field error",
        serde_json::error::Category::Eof => "unexpected end of JSON",
        serde_json::error::Category::Io => "configuration read error",
    };
    let path = path.filter(|path| !path.is_empty());
    let detail = match path {
        Some(path) => format!("{path}: {error}"),
        None => error.to_string(),
    };
    eprintln!("error: cladding.json {category}: {detail}");
    eprintln!("file: {}", config_path.display());
    Error::message(format!("invalid cladding.json ({category}): {detail}"))
}

fn invalid_config(config_path: &Path, message: &str) -> Error {
    eprintln!("error: cladding.json {message}");
    eprintln!("file: {}", config_path.display());
    Error::message("invalid cladding.json")
}

fn parse_agent(
    document: AgentDocument,
    project_name: &str,
    config_path: &Path,
) -> Result<ExecutionComponentConfig> {
    if document.enabled.is_some() {
        eprintln!("error: cladding.json invalid field 'agent.enabled' (agent cannot be disabled)");
        eprintln!("hint: remove 'agent.enabled'");
        eprintln!("file: {}", config_path.display());
        return Err(Error::message("invalid cladding.json"));
    }

    let build = parse_build_config(document.build, "agent", config_path)?;
    let image = component_image(
        document.image,
        build.is_some(),
        project_name,
        "agent",
        config_path,
    )?;
    let security_opts = parse_security_opts(
        document.security_opts.unwrap_or_default(),
        "agent",
        config_path,
    )?;
    Ok(ExecutionComponentConfig {
        enabled: true,
        init: document.init.unwrap_or(true),
        image,
        build,
        security_opts,
    })
}

fn parse_component(
    document: ComponentDocument,
    key: &str,
    project_name: &str,
    config_path: &Path,
) -> Result<ExecutionComponentConfig> {
    let build = parse_build_config(document.build, key, config_path)?;
    let image = component_image(
        document.image,
        build.is_some(),
        project_name,
        key,
        config_path,
    )?;
    let security_opts =
        parse_security_opts(document.security_opts.unwrap_or_default(), key, config_path)?;
    Ok(ExecutionComponentConfig {
        enabled: document.enabled.unwrap_or(true),
        init: document.init.unwrap_or(true),
        image,
        build,
        security_opts,
    })
}

fn component_image(
    image: Option<String>,
    has_build: bool,
    project_name: &str,
    component_key: &str,
    config_path: &Path,
) -> Result<String> {
    let image = image.unwrap_or_else(|| {
        if has_build {
            generated_image_tag(project_name, component_key)
        } else {
            DEFAULT_COMPONENT_IMAGE.to_string()
        }
    });
    if image.is_empty() {
        return Err(invalid_config(
            config_path,
            &format!("invalid field '{component_key}.image' (must not be empty)"),
        ));
    }
    Ok(image)
}

fn parse_security_opts(
    options: Vec<String>,
    component_key: &str,
    config_path: &Path,
) -> Result<Vec<String>> {
    let field = format!("{component_key}.security_opts");
    for (index, option) in options.iter().enumerate() {
        if option.is_empty() {
            return Err(invalid_config(
                config_path,
                &format!("field '{field}[{index}]' must be a non-empty string"),
            ));
        }
    }
    Ok(options)
}

fn parse_proxy(
    document: Option<ProxyDocument>,
    project_name: &str,
    config_path: &Path,
) -> Result<Option<ExecutionProxyConfig>> {
    let Some(document) = document else {
        return Ok(None);
    };
    let key = "proxy";
    let build = parse_build_config(document.build, key, config_path)?;
    let agent_session_config = parse_proxy_session_config(
        document.agent,
        "proxy.agent",
        DEFAULT_AGENT_SESSION_CONFIG,
        config_path,
    )?;
    let nw_sandbox_session_config = parse_proxy_session_config(
        document.nw_sandbox,
        "proxy.nw_sandbox",
        DEFAULT_NW_SANDBOX_SESSION_CONFIG,
        config_path,
    )?;
    let image = document.image.unwrap_or_else(|| {
        if build.is_some() {
            generated_image_tag(project_name, key)
        } else {
            DEFAULT_PROXY_IMAGE.to_string()
        }
    });
    if image.is_empty() {
        return Err(invalid_config(
            config_path,
            "invalid field 'proxy.image' (must not be empty)",
        ));
    }

    Ok(Some(ExecutionProxyConfig {
        image,
        build,
        agent_session_config,
        nw_sandbox_session_config,
    }))
}

fn parse_proxy_session_config(
    raw: Option<SessionConfigDocument>,
    field: &str,
    default: &str,
    config_path: &Path,
) -> Result<String> {
    let Some(raw) = raw else {
        return Ok(default.to_string());
    };
    let Some(value) = raw.session_config else {
        return Ok(default.to_string());
    };
    if value.is_empty() {
        return Err(invalid_config(
            config_path,
            &format!("field '{field}.session_config' must be a non-empty relative path"),
        ));
    }

    validate_session_config_path(&value).map_err(|reason| {
        invalid_config(
            config_path,
            &format!("field '{field}.session_config' {reason}"),
        )
    })?;
    Ok(value)
}

pub fn validate_session_config_path(value: &str) -> std::result::Result<(), &'static str> {
    use std::path::Component;

    let path = Path::new(value);
    let bytes = value.as_bytes();
    let has_windows_drive_prefix =
        bytes.first().is_some_and(u8::is_ascii_alphabetic) && bytes.get(1) == Some(&b':');
    if value.is_empty()
        || value.starts_with('/')
        || has_windows_drive_prefix
        || value.contains(['\\', '\0'])
        || path.is_absolute()
        || value
            .split('/')
            .any(|component| component.is_empty() || matches!(component, "." | ".."))
        || path
            .components()
            .any(|component| !matches!(component, Component::Normal(_)))
    {
        return Err("must be a relative path without absolute, dot, or parent components");
    }
    Ok(())
}

fn parse_build_config(
    raw: Option<ImageBuildDocument>,
    component_key: &str,
    config_path: &Path,
) -> Result<Option<ImageBuildConfig>> {
    let Some(raw) = raw else {
        return Ok(None);
    };
    let field = format!("{component_key}.build");
    if raw.containerfile.is_empty() {
        return Err(invalid_config(
            config_path,
            &format!("field '{field}.containerfile' must be a non-empty string"),
        ));
    }
    let context = raw.context.as_deref().unwrap_or(".");
    if context.is_empty() {
        return Err(invalid_config(
            config_path,
            &format!("field '{field}.context' must be a non-empty string"),
        ));
    }
    for name in raw.args.keys() {
        if name.is_empty() {
            return Err(invalid_config(
                config_path,
                &format!("field '{field}.args.{name}' must not have an empty name"),
            ));
        }
    }

    let base = config_path.parent().unwrap_or(Path::new("."));
    Ok(Some(ImageBuildConfig {
        containerfile: resolve_path(base, Path::new(&raw.containerfile)),
        context: resolve_path(base, Path::new(context)),
        args: raw.args,
    }))
}

fn generated_image_tag(project_name: &str, component_key: &str) -> String {
    format!("localhost/cladding-{project_name}-{component_key}:latest")
}

fn resolve_path(base: &Path, path: &Path) -> PathBuf {
    if path.is_absolute() {
        normalize_path(path)
    } else {
        let base = if base.is_absolute() {
            base.to_path_buf()
        } else {
            std::env::current_dir()
                .unwrap_or_else(|_| PathBuf::from("."))
                .join(base)
        };
        normalize_path(&base.join(path))
    }
}

fn normalize_path(path: &Path) -> PathBuf {
    let mut normalized = PathBuf::new();
    for component in path.components() {
        match component {
            std::path::Component::CurDir => {}
            std::path::Component::ParentDir => {
                if normalized.file_name().is_some() {
                    normalized.pop();
                } else if !normalized.has_root() {
                    normalized.push("..");
                }
            }
            other => normalized.push(other.as_os_str()),
        }
    }
    normalized
}

fn is_lowercase_alnum(name: &str) -> bool {
    !name.is_empty()
        && name
            .chars()
            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::env;
    use std::path::PathBuf;
    use std::sync::atomic::{AtomicUsize, Ordering};

    static TEMP_DIR_COUNTER: AtomicUsize = AtomicUsize::new(0);

    #[test]
    fn load_cladding_config_parses_component_objects() {
        let temp = create_temp_dir("component-config");
        fs::write(
            temp.join("cladding.json"),
            r#"{
  "name": "demo",
  "agent": {
    "image": "agent:image"
  },
  "nw_sandbox": {
    "image": "sandbox:image"
  },
  "fs_sandbox": {
    "enabled": true,
    "image": "fs:image"
  }
}"#,
        )
        .unwrap();

        let config = load_cladding_config_v2(&temp).unwrap();
        assert!(!config.use_runsc);
        assert_eq!(config.agent_image(), "agent:image");
        assert_eq!(config.nw_sandbox_image(), "sandbox:image");
        assert!(config.nw_sandbox_enabled());
        assert!(config.fs_sandbox_enabled());
        assert_eq!(config.fs_sandbox_image(), "fs:image");
        assert!(config.mounts.is_empty());
        assert!(config.fake_credential_env.is_empty());
        assert_eq!(config.proxy_image(), DEFAULT_PROXY_IMAGE);
        assert_eq!(config.agent_session_config(), DEFAULT_AGENT_SESSION_CONFIG);
        assert_eq!(
            config.nw_sandbox_session_config(),
            DEFAULT_NW_SANDBOX_SESSION_CONFIG
        );
        assert!(config.agent.build.is_none());
        assert!(config.agent.security_opts.is_empty());
        assert!(config.agent.init);
        assert!(config.nw_sandbox.as_ref().unwrap().init);
        assert!(config.fs_sandbox.as_ref().unwrap().init);
    }

    #[test]
    fn load_cladding_config_validates_fake_mapping_syntax_without_baffle_policy_files() {
        let temp = create_temp_dir("fake-credential-env");
        assert!(!temp.join("config/proxy/daemon.toml").exists());
        fs::write(
            temp.join("cladding.json"),
            r#"{
  "name": "demo",
  "agent": {},
  "fake_credential_env": {
    "GITHUB_TOKEN": "github-api",
    "CUSTOM_GIT_TOKEN": "github-api"
  }
}"#,
        )
        .unwrap();

        let config = load_cladding_config_v2(&temp).unwrap();
        assert_eq!(config.fake_credential_env.len(), 2);
        assert_eq!(config.fake_credential_env["GITHUB_TOKEN"], "github-api");
        assert_eq!(config.fake_credential_env["CUSTOM_GIT_TOKEN"], "github-api");

        for mapping in [
            r#""INVALID-NAME":"github-api""#,
            r#""GITHUB_TOKEN":"not..valid""#,
        ] {
            fs::write(
                temp.join("cladding.json"),
                format!(r#"{{"name":"demo","agent":{{}},"fake_credential_env":{{{mapping}}}}}"#),
            )
            .unwrap();
            assert!(
                load_cladding_config_v2(&temp).is_err(),
                "accepted invalid mapping {mapping}"
            );
        }
        fs::remove_dir_all(temp).unwrap();
    }

    #[test]
    fn load_cladding_config_parses_independent_component_init_settings() {
        let temp = create_temp_dir("component-init-settings");
        fs::write(
            temp.join("cladding.json"),
            r#"{
  "name": "demo",
  "agent": { "init": false },
  "nw_sandbox": { "init": true },
  "fs_sandbox": { "init": false }
}"#,
        )
        .unwrap();

        let config = load_cladding_config_v2(&temp).unwrap();

        assert!(!config.agent.init);
        assert!(config.nw_sandbox.as_ref().unwrap().init);
        assert!(!config.fs_sandbox.as_ref().unwrap().init);
        fs::remove_dir_all(temp).unwrap();
    }

    #[test]
    fn load_cladding_config_rejects_non_boolean_component_init() {
        let temp = create_temp_dir("invalid-component-init");
        for init in ["null", "0", "\"false\"", "[]", "{}"] {
            let raw = format!(r#"{{"name":"demo","agent":{{"init":{init}}}}}"#);
            fs::write(temp.join("cladding.json"), raw).unwrap();
            assert!(
                load_cladding_config_v2(&temp).is_err(),
                "accepted invalid init {init}"
            );
        }
        fs::remove_dir_all(temp).unwrap();
    }

    #[test]
    fn load_cladding_config_rejects_unknown_component_keys_and_proxy_init() {
        let temp = create_temp_dir("invalid-init-scope");
        let valid_proxy_config = r#"{"name":"demo","agent":{},"proxy":{}}"#;
        fs::write(temp.join("cladding.json"), valid_proxy_config).unwrap();
        assert!(
            load_cladding_config_v2(&temp).is_ok(),
            "rejected proxy config without init: {valid_proxy_config}"
        );

        for raw in [
            r#"{"name":"demo","agent":{"unknown":true}}"#,
            r#"{"name":"demo","agent":{},"proxy":{"init":false}}"#,
        ] {
            fs::write(temp.join("cladding.json"), raw).unwrap();
            assert!(
                load_cladding_config_v2(&temp).is_err(),
                "accepted invalid component config {raw}"
            );
        }
        fs::remove_dir_all(temp).unwrap();
    }

    #[test]
    fn load_cladding_config_parses_component_security_options() {
        let temp = create_temp_dir("component-security-options");
        fs::write(
            temp.join("cladding.json"),
            r#"{
  "name": "demo",
  "agent": { "security_opts": ["unmask=/proc/*", "label=disable"] },
  "nw_sandbox": { "security_opts": ["no-new-privileges"] },
  "fs_sandbox": {}
}"#,
        )
        .unwrap();

        let config = load_cladding_config_v2(&temp).unwrap();

        assert_eq!(
            config.agent.security_opts,
            ["unmask=/proc/*", "label=disable"]
        );
        assert_eq!(
            config.nw_sandbox.as_ref().unwrap().security_opts,
            ["no-new-privileges"]
        );
        assert!(config.fs_sandbox.as_ref().unwrap().security_opts.is_empty());
        fs::remove_dir_all(temp).unwrap();
    }

    #[test]
    fn load_cladding_config_rejects_invalid_component_security_options() {
        let temp = create_temp_dir("invalid-component-security-options");
        for security_opts in ["null", r#"["valid", 1]"#, r#"[""]"#] {
            let raw = format!(r#"{{"name":"demo","agent":{{"security_opts":{security_opts}}}}}"#);
            fs::write(temp.join("cladding.json"), raw).unwrap();
            assert!(
                load_cladding_config_v2(&temp).is_err(),
                "accepted invalid security_opts {security_opts}"
            );
        }
        fs::remove_dir_all(temp).unwrap();
    }

    #[test]
    fn load_cladding_config_selects_independent_proxy_session_files() {
        let temp = create_temp_dir("proxy-session-selection");
        fs::write(
            temp.join("cladding.json"),
            r#"{
  "name": "demo",
  "agent": { "image": "agent:image" },
  "nw_sandbox": { "enabled": true },
  "proxy": {
    "agent": { "session_config": "policies/agent.toml" },
    "nw_sandbox": { "session_config": "restricted.toml" }
  }
}"#,
        )
        .unwrap();

        let config = load_cladding_config_v2(&temp).unwrap();

        assert_eq!(config.agent_session_config(), "policies/agent.toml");
        assert_eq!(config.nw_sandbox_session_config(), "restricted.toml");
        assert!(config.nw_sandbox_enabled());
        fs::remove_dir_all(temp).unwrap();
    }

    #[test]
    fn load_cladding_config_rejects_unsafe_proxy_session_paths() {
        let temp = create_temp_dir("unsafe-proxy-session-path");
        for session_config in [
            "/tmp/agent.toml",
            "../outside.toml",
            "nested/../../outside.toml",
            "./agent.toml",
            "nested//agent.toml",
            r#"nested\\agent.toml"#,
            "C:/outside.toml",
        ] {
            let raw = format!(
                r#"{{
  "name": "demo",
  "agent": {{ "image": "agent:image" }},
  "proxy": {{ "agent": {{ "session_config": {session_config:?} }} }}
}}"#
            );
            fs::write(temp.join("cladding.json"), raw).unwrap();
            assert!(
                load_cladding_config_v2(&temp).is_err(),
                "accepted unsafe session path {session_config}"
            );
        }
        fs::remove_dir_all(temp).unwrap();
    }

    #[test]
    fn load_cladding_config_resolves_build_paths_and_generated_image_tags() {
        let temp = create_temp_dir("build-config");
        fs::write(
            temp.join("cladding.json"),
            r#"{
  "name": "demo",
  "agent": {
    "build": {
      "containerfile": "containers/agent.Containerfile",
      "context": "..",
      "args": { "FEATURE": "enabled" }
    }
  },
  "nw_sandbox": {
    "enabled": true,
    "build": { "containerfile": "containers/nw.Containerfile" }
  },
  "fs_sandbox": {
    "build": { "containerfile": "containers/fs.Containerfile", "context": "../src" }
  },
  "proxy": {
    "image": "localhost/custom-proxy:latest",
    "build": { "containerfile": "containers/proxy.Containerfile", "context": "../proxy" }
  }
}"#,
        )
        .unwrap();

        let config = load_cladding_config_v2(&temp).unwrap();
        assert_eq!(config.agent.image, "localhost/cladding-demo-agent:latest");
        let agent_build = config.agent.build.as_ref().unwrap();
        assert_eq!(
            agent_build.containerfile,
            temp.join("containers/agent.Containerfile")
        );
        assert_eq!(agent_build.context, temp.parent().unwrap());
        assert_eq!(agent_build.args.get("FEATURE").unwrap(), "enabled");
        assert_eq!(
            config.nw_sandbox.as_ref().unwrap().image,
            "localhost/cladding-demo-nw_sandbox:latest"
        );
        assert_eq!(
            config
                .fs_sandbox
                .as_ref()
                .unwrap()
                .build
                .as_ref()
                .unwrap()
                .context,
            temp.parent().unwrap().join("src")
        );
        let proxy = config.proxy.as_ref().unwrap();
        assert_eq!(proxy.image, "localhost/custom-proxy:latest");
        assert_eq!(
            proxy.build.as_ref().unwrap().containerfile,
            temp.join("containers/proxy.Containerfile")
        );
    }

    #[test]
    fn explicit_config_resolves_build_and_mount_paths_from_its_directory() {
        let temp = create_temp_dir("explicit-config-paths");
        let config_dir = temp.join("external-config");
        fs::create_dir_all(&config_dir).unwrap();
        let config_path = config_dir.join("custom.json");
        fs::write(
            &config_path,
            r#"{
  "name": "demo",
  "agent": { "build": { "containerfile": "containers/agent.Containerfile" } },
  "mounts": [{ "mount": "/data", "hostPath": "assets" }]
}"#,
        )
        .unwrap();

        let config = load_cladding_config_v2_from_path(&config_path).unwrap();

        assert_eq!(
            config.agent.build.unwrap().containerfile,
            config_dir.join("containers/agent.Containerfile")
        );
        assert_eq!(
            config.mounts[0].host_path.as_deref(),
            Some(config_dir.join("assets").as_path())
        );
    }

    #[test]
    fn stdin_config_uses_its_synthetic_file_directory_for_references() {
        let temp = create_temp_dir("stdin-config-paths");
        let raw = r#"{
  "name": "demo",
  "agent": { "build": { "containerfile": "containers/agent.Containerfile" } },
  "mounts": [{ "mount": "/data", "hostPath": "assets" }]
}"#;

        let config = load_cladding_config_v2_from_str(&temp.join("cladding.json"), raw).unwrap();

        assert_eq!(
            config.agent.build.unwrap().containerfile,
            temp.join("containers/agent.Containerfile")
        );
        assert_eq!(
            config.mounts[0].host_path.as_deref(),
            Some(temp.join("assets").as_path())
        );
    }

    #[test]
    fn load_cladding_config_accepts_ordinary_build_argument_names() {
        let temp = create_temp_dir("ordinary-build-arg");
        fs::write(
            temp.join("cladding.json"),
            r#"{
  "name": "demo",
  "agent": {
    "build": {
      "containerfile": "Containerfile",
      "args": { "API_TOKEN": "ordinary-value", "AWS_ACCESS_KEY_ID": "ordinary-value" }
    }
  }
}"#,
        )
        .unwrap();

        let config = load_cladding_config_v2(&temp).unwrap();
        let args = &config.agent.build.as_ref().unwrap().args;
        assert_eq!(args.get("API_TOKEN").unwrap(), "ordinary-value");
        assert_eq!(args.get("AWS_ACCESS_KEY_ID").unwrap(), "ordinary-value");
    }

    #[test]
    fn load_cladding_config_defaults_use_runsc_to_false() {
        let temp = create_temp_dir("default-use-runsc");
        fs::write(
            temp.join("cladding.json"),
            r#"{
  "name": "demo",
  "agent": {
    "image": "agent:image"
  }
}"#,
        )
        .unwrap();

        let config = load_cladding_config_v2(&temp).unwrap();
        assert!(!config.use_runsc);
    }

    #[test]
    fn load_cladding_config_accepts_explicit_use_runsc_values() {
        let temp = create_temp_dir("explicit-use-runsc");
        fs::write(
            temp.join("cladding.json"),
            r#"{
  "name": "demo",
  "use_runsc": true,
  "agent": {
    "image": "agent:image"
  }
}"#,
        )
        .unwrap();

        let config = load_cladding_config_v2(&temp).unwrap();
        assert!(config.use_runsc);

        fs::write(
            temp.join("cladding.json"),
            r#"{
  "name": "demo",
  "use_runsc": false,
  "agent": {
    "image": "agent:image"
  }
}"#,
        )
        .unwrap();

        let config = load_cladding_config_v2(&temp).unwrap();
        assert!(!config.use_runsc);
    }

    #[test]
    fn load_cladding_config_rejects_non_boolean_use_runsc() {
        let temp = create_temp_dir("non-boolean-use-runsc");
        fs::write(
            temp.join("cladding.json"),
            r#"{
  "name": "demo",
  "use_runsc": "yes",
  "agent": {
    "image": "agent:image"
  }
}"#,
        )
        .unwrap();

        assert!(load_cladding_config_v2(&temp).is_err());
    }

    #[test]
    fn load_cladding_config_rejects_legacy_top_level_keys() {
        let temp = create_temp_dir("legacy-top-level");
        fs::write(
            temp.join("cladding.json"),
            r#"{
  "name": "demo",
  "agent_image": "agent:image",
  "nw_sandbox_image": "sandbox:image"
}"#,
        )
        .unwrap();

        assert!(load_cladding_config_v2(&temp).is_err());
    }

    #[test]
    fn load_cladding_config_reports_syntax_type_and_missing_field_errors() {
        let path = Path::new("cladding.json");

        let syntax_error = load_cladding_config_v2_from_str(path, r#"{"name":"demo","agent": }"#)
            .unwrap_err()
            .to_string();
        assert!(syntax_error.contains("JSON syntax error"), "{syntax_error}");
        assert!(
            syntax_error.contains("line") && syntax_error.contains("column"),
            "{syntax_error}"
        );

        let type_error =
            load_cladding_config_v2_from_str(path, r#"{"name":"demo","agent":{"init":"false"}}"#)
                .unwrap_err()
                .to_string();
        assert!(type_error.contains("agent.init"), "{type_error}");
        assert!(type_error.contains("boolean"), "{type_error}");

        let missing_error = load_cladding_config_v2_from_str(path, r#"{"agent":{}}"#)
            .unwrap_err()
            .to_string();
        assert!(
            missing_error.contains("missing field `name`"),
            "{missing_error}"
        );
    }

    #[test]
    fn load_cladding_config_rejects_arrays_where_objects_are_required() {
        let path = Path::new("cladding.json");
        for (location, raw) in [
            ("top level", r#"["demo",false,{}]"#),
            ("agent", r#"{"name":"demo","agent":[]}"#),
            (
                "network sandbox",
                r#"{"name":"demo","agent":{},"nw_sandbox":[]}"#,
            ),
            (
                "filesystem sandbox",
                r#"{"name":"demo","agent":{},"fs_sandbox":[]}"#,
            ),
            ("proxy", r#"{"name":"demo","agent":{},"proxy":[]}"#),
            (
                "proxy agent session selector",
                r#"{"name":"demo","agent":{},"proxy":{"agent":[]}}"#,
            ),
            (
                "proxy network sandbox session selector",
                r#"{"name":"demo","agent":{},"proxy":{"nw_sandbox":[]}}"#,
            ),
            (
                "agent image build",
                r#"{"name":"demo","agent":{"build":[]}}"#,
            ),
            (
                "network sandbox image build",
                r#"{"name":"demo","agent":{},"nw_sandbox":{"build":[]}}"#,
            ),
            (
                "filesystem sandbox image build",
                r#"{"name":"demo","agent":{},"fs_sandbox":{"build":[]}}"#,
            ),
            (
                "proxy image build",
                r#"{"name":"demo","agent":{},"proxy":{"build":[]}}"#,
            ),
            ("mount entry", r#"{"name":"demo","agent":{},"mounts":[[]]}"#),
        ] {
            let error = load_cladding_config_v2_from_str(path, raw)
                .unwrap_err()
                .to_string();
            assert!(
                error.contains("configuration type or field error"),
                "array at {location} did not produce a type error: {error}"
            );
            assert!(
                error.contains("expected a configuration object"),
                "array at {location} did not report the expected object type: {error}"
            );
        }
    }

    #[test]
    fn load_cladding_config_rejects_null_for_optional_fields() {
        let path = Path::new("cladding.json");
        for raw in [
            r#"{"name":"demo","agent":{},"nw_sandbox":null}"#,
            r#"{"name":"demo","agent":{},"proxy":null}"#,
            r#"{"name":"demo","agent":{"image":null}}"#,
            r#"{"name":"demo","agent":{},"mounts":[{"mount":"/data","hostPath":null}]}"#,
        ] {
            assert!(
                load_cladding_config_v2_from_str(path, raw).is_err(),
                "accepted explicit null in {raw}"
            );
        }
    }

    #[test]
    fn load_cladding_config_rejects_agent_enabled() {
        let temp = create_temp_dir("agent-enabled");
        fs::write(
            temp.join("cladding.json"),
            r#"{
  "name": "demo",
  "agent": {
    "enabled": false,
    "image": "agent:image"
  }
}"#,
        )
        .unwrap();

        assert!(load_cladding_config_v2(&temp).is_err());
    }

    fn create_temp_dir(name: &str) -> PathBuf {
        let unique = TEMP_DIR_COUNTER.fetch_add(1, Ordering::Relaxed);
        let path =
            env::temp_dir().join(format!("cladding-{name}-{}-{}", std::process::id(), unique));
        if path.exists() {
            fs::remove_dir_all(&path).unwrap();
        }
        fs::create_dir_all(&path).unwrap();
        path
    }
}
