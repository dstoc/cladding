use crate::error::{Error, Result};
use anyhow::Context as _;
use std::env;
use std::ffi::OsStr;

pub fn write_default_cladding_config(
    name_override: Option<&str>,
    default_sandbox_image: &str,
    default_cli_image: &str,
) -> Result<String> {
    let name = if let Some(name_override) = name_override {
        normalize_cladding_name_arg(name_override)?
    } else {
        derive_cladding_name_from_pwd()?
    };

    Ok(format!(
        "{{\n  \"name\": \"{}\",\n  \"agent\": {{\n    \"image\": \"{}\"\n  }},\n  \"nw_sandbox\": {{\n    \"enabled\": true,\n    \"image\": \"{}\"\n  }}\n}}\n",
        name, default_cli_image, default_sandbox_image
    ))
}

fn derive_cladding_name_from_pwd() -> Result<String> {
    let cwd = env::current_dir().with_context(|| "failed to determine current directory")?;
    let raw_name = cwd.file_name().and_then(OsStr::to_str).unwrap_or("");
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
        return Err(Error::message("could not derive name"));
    }

    Ok(name)
}

fn normalize_cladding_name_arg(name_arg: &str) -> Result<String> {
    let name = name_arg.to_ascii_lowercase();
    if name.is_empty() || !name.chars().all(|c| c.is_ascii_alphanumeric()) {
        eprintln!("error: init name must be alphanumeric ([a-zA-Z0-9]+)");
        return Err(Error::message("invalid init name"));
    }
    Ok(name)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn normalize_init_name() {
        assert_eq!(
            normalize_cladding_name_arg("MyProject").unwrap(),
            "myproject"
        );
        assert!(normalize_cladding_name_arg("bad-name").is_err());
    }

    #[test]
    fn write_default_cladding_config_uses_component_objects() {
        let generated =
            write_default_cladding_config(Some("Demo"), "sandbox:image", "agent:image").unwrap();
        assert_eq!(
            generated,
            "{\n  \"name\": \"demo\",\n  \"agent\": {\n    \"image\": \"agent:image\"\n  },\n  \"nw_sandbox\": {\n    \"enabled\": true,\n    \"image\": \"sandbox:image\"\n  }\n}\n"
        );
    }
}
