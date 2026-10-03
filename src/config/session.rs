use serde::Deserialize;
use std::collections::{BTreeMap, HashSet};

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct SessionDocument {
    version: u16,
    #[serde(default)]
    persistent: bool,
    socket_name: Option<String>,
    unmatched: Option<UnmatchedPolicy>,
    rules: Option<BTreeMap<String, HostRule>>,
}

#[derive(Deserialize)]
#[serde(rename_all = "lowercase")]
enum UnmatchedPolicy {
    Deny,
    Tunnel,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct HostRule {
    mode: Option<RuleMode>,
    #[serde(default = "default_ports")]
    ports: Vec<u16>,
    paths: Option<Vec<String>>,
    inject: Option<Vec<HeaderInjection>>,
}

#[derive(Clone, Copy, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
enum RuleMode {
    Tunnel,
    Intercept,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct HeaderInjection {
    header: String,
    secret: String,
    format: InjectionFormat,
    username: Option<String>,
}

#[derive(Clone, Copy, Deserialize)]
#[serde(rename_all = "snake_case")]
enum InjectionFormat {
    Raw,
    Bearer,
    BasicPassword,
}

fn default_ports() -> Vec<u16> {
    vec![443]
}

/// Validate the native version 2 Baffle session format used by Cladding.
///
/// Baffle validates the same policy again when it creates the live session.
pub fn validate_baffle_session_config(input: &str) -> anyhow::Result<()> {
    validate_baffle_session_config_inner(input, None)
}

/// Validate a native version 2 Baffle session and require its data socket to
/// match the socket mounted for the selected Cladding component.
pub fn validate_baffle_session_config_for_socket(
    input: &str,
    expected_socket_name: &str,
) -> anyhow::Result<()> {
    validate_baffle_session_config_inner(input, Some(expected_socket_name))
}

fn validate_baffle_session_config_inner(
    input: &str,
    expected_socket_name: Option<&str>,
) -> anyhow::Result<()> {
    let document: SessionDocument = toml::from_str(input)
        .map_err(|_| anyhow::anyhow!("invalid Baffle session TOML syntax or field value"))?;
    if document.version != 2 {
        anyhow::bail!("Baffle session configuration must use version = 2");
    }
    let _ = document.persistent;
    let _ = document.unmatched;

    if let Some(socket_name) = document.socket_name.as_deref() {
        validate_socket_name(socket_name)?;
    }
    if let Some(expected_socket_name) = expected_socket_name {
        match document.socket_name.as_deref() {
            Some(socket_name) if socket_name == expected_socket_name => {}
            Some(socket_name) => anyhow::bail!(
                "Baffle session socket_name must be '{expected_socket_name}' for this component (found '{socket_name}')"
            ),
            None => anyhow::bail!(
                "Baffle session must set socket_name = '{expected_socket_name}' for this component"
            ),
        }
    }

    let mut hosts = HashSet::new();
    for (host, rule) in document.rules.unwrap_or_default() {
        let normalized_host = validate_hostname(&host)?;
        if !hosts.insert(normalized_host) {
            anyhow::bail!("Baffle session contains duplicate host rules");
        }
        validate_rule(rule)?;
    }

    Ok(())
}

fn validate_socket_name(name: &str) -> anyhow::Result<()> {
    if name.is_empty()
        || name.starts_with('/')
        || name.contains(['\\', '\0'])
        || name.len() > 107
        || name
            .split('/')
            .any(|component| component.is_empty() || matches!(component, "." | ".."))
    {
        anyhow::bail!("Baffle session socket_name must be a safe relative socket path");
    }
    Ok(())
}

fn validate_hostname(host: &str) -> anyhow::Result<String> {
    let host = host.to_ascii_lowercase();
    let host = host.strip_suffix('.').unwrap_or(&host);
    if host.is_empty()
        || host.len() > 253
        || !host.is_ascii()
        || host.contains('*')
        || host.parse::<std::net::IpAddr>().is_ok()
        || host.split('.').any(|label| {
            label.is_empty()
                || label.len() > 63
                || label.starts_with('-')
                || label.ends_with('-')
                || !label
                    .bytes()
                    .all(|byte| byte.is_ascii_alphanumeric() || byte == b'-')
        })
    {
        anyhow::bail!("Baffle session rule keys must be exact DNS hostnames");
    }
    Ok(host.to_string())
}

fn validate_rule(rule: HostRule) -> anyhow::Result<()> {
    let mut ports = HashSet::new();
    if rule.ports.is_empty() || rule.ports.contains(&0) {
        anyhow::bail!("Baffle session rule ports must contain values from 1 through 65535");
    }
    if rule.ports.iter().any(|port| !ports.insert(*port)) {
        anyhow::bail!("Baffle session rule ports must be unique");
    }

    let has_intercept_fields = rule.paths.is_some() || rule.inject.is_some();
    if rule.mode == Some(RuleMode::Tunnel) && has_intercept_fields {
        anyhow::bail!("Baffle tunnel rules cannot use paths or header injection");
    }

    let mut paths = Vec::new();
    for path in rule.paths.unwrap_or_default() {
        let path = validate_path(&path)?;
        if paths
            .iter()
            .any(|existing: &ValidatedPath| existing.overlaps(&path))
        {
            anyhow::bail!("Baffle session rule paths must not overlap");
        }
        paths.push(path);
    }

    let mut headers = HashSet::new();
    for injection in rule.inject.unwrap_or_default() {
        validate_injection(injection, &mut headers)?;
    }
    Ok(())
}

fn validate_injection(
    injection: HeaderInjection,
    headers: &mut HashSet<String>,
) -> anyhow::Result<()> {
    const PROHIBITED_HEADERS: &[&str] = &[
        "connection",
        "content-length",
        "forwarded",
        "host",
        "http2-settings",
        "keep-alive",
        "proxy-authenticate",
        "proxy-authorization",
        "proxy-connection",
        "te",
        "trailer",
        "transfer-encoding",
        "upgrade",
        "x-forwarded-for",
        "x-forwarded-host",
        "x-forwarded-port",
        "x-forwarded-proto",
        "x-original-host",
        "x-original-url",
        "x-real-ip",
        "x-rewrite-url",
    ];
    let header = injection.header.to_ascii_lowercase();
    if injection.header.is_empty()
        || !injection
            .header
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || b"!#$%&'*+-.^_`|~".contains(&byte))
        || PROHIBITED_HEADERS.contains(&header.as_str())
        || !headers.insert(header)
    {
        anyhow::bail!("Baffle session contains an invalid or duplicate injected header");
    }

    let secret = injection.secret;
    if secret.is_empty()
        || secret.len() > 64
        || !secret
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'.' | b'_' | b'-'))
        || !secret.as_bytes()[0].is_ascii_alphanumeric()
        || secret.ends_with(['.', '-', '_'])
        || secret.contains("..")
    {
        anyhow::bail!("Baffle session contains an invalid symbolic secret name");
    }

    match (injection.format, injection.username) {
        (InjectionFormat::BasicPassword, Some(username))
            if !username.is_empty()
                && !username.contains(':')
                && username.bytes().all(|byte| (b' '..=b'~').contains(&byte)) =>
        {
            Ok(())
        }
        (InjectionFormat::BasicPassword, _) => {
            anyhow::bail!("basic_password injection requires a valid username")
        }
        (_, None) => Ok(()),
        (_, Some(_)) => anyhow::bail!("username is only valid for basic_password injection"),
    }
}

struct ValidatedPath {
    path: String,
    recursive: bool,
}

impl ValidatedPath {
    fn overlaps(&self, other: &Self) -> bool {
        match (self.recursive, other.recursive) {
            (false, false) => self.path == other.path,
            (true, false) => other.path.starts_with(&self.path),
            (false, true) => self.path.starts_with(&other.path),
            (true, true) => {
                self.path.starts_with(&other.path) || other.path.starts_with(&self.path)
            }
        }
    }
}

fn validate_path(input: &str) -> anyhow::Result<ValidatedPath> {
    if !input.starts_with('/') || input.contains(['?', '#', '\\']) {
        anyhow::bail!(
            "Baffle session paths must be absolute URL paths without queries or fragments"
        );
    }

    let (path, recursive) = if let Some(prefix) = input.strip_suffix("/**") {
        (
            if prefix.is_empty() {
                "/"
            } else {
                &input[..prefix.len() + 1]
            },
            true,
        )
    } else {
        (input, false)
    };
    if path.contains('*') {
        anyhow::bail!("Baffle session paths may use ** only as the final segment");
    }

    let canonical = canonicalize_path(path)?;
    Ok(ValidatedPath {
        path: canonical,
        recursive,
    })
}

fn canonicalize_path(path: &str) -> anyhow::Result<String> {
    let bytes = path.as_bytes();
    let mut canonical = String::with_capacity(path.len());
    let mut index = 0;
    while index < bytes.len() {
        let byte = bytes[index];
        if byte == b'%' {
            if index + 2 >= bytes.len() {
                anyhow::bail!("Baffle session path has malformed percent encoding");
            }
            let high = hex_value(bytes[index + 1]).ok_or_else(|| {
                anyhow::anyhow!("Baffle session path has malformed percent encoding")
            })?;
            let low = hex_value(bytes[index + 2]).ok_or_else(|| {
                anyhow::anyhow!("Baffle session path has malformed percent encoding")
            })?;
            let decoded = high * 16 + low;
            if matches!(decoded, b'/' | b'\\' | b'%') {
                anyhow::bail!("Baffle session path has an encoded separator");
            }
            if decoded.is_ascii_alphanumeric() || matches!(decoded, b'-' | b'.' | b'_' | b'~') {
                canonical.push(char::from(decoded));
            } else {
                canonical.push('%');
                canonical.push(char::from(bytes[index + 1].to_ascii_uppercase()));
                canonical.push(char::from(bytes[index + 2].to_ascii_uppercase()));
            }
            index += 3;
            continue;
        }

        if !is_path_character(byte) {
            anyhow::bail!("Baffle session path contains an invalid URL path character");
        }
        canonical.push(char::from(byte));
        index += 1;
    }

    if canonical.contains("//")
        || canonical
            .split('/')
            .any(|segment| matches!(segment, "." | ".."))
    {
        anyhow::bail!("Baffle session path contains an unsafe path segment");
    }
    Ok(canonical)
}

fn hex_value(byte: u8) -> Option<u8> {
    match byte {
        b'0'..=b'9' => Some(byte - b'0'),
        b'a'..=b'f' => Some(byte - b'a' + 10),
        b'A'..=b'F' => Some(byte - b'A' + 10),
        _ => None,
    }
}

fn is_path_character(byte: u8) -> bool {
    byte.is_ascii_alphanumeric()
        || matches!(
            byte,
            b'/' | b'-'
                | b'.'
                | b'_'
                | b'~'
                | b'!'
                | b'$'
                | b'&'
                | b'\''
                | b'('
                | b')'
                | b'+'
                | b','
                | b';'
                | b'='
                | b':'
                | b'@'
        )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn accepts_generated_session_files_and_interception_rules() {
        let session = r#"
version = 2
persistent = true
socket_name = "agent/proxy.sock"
unmatched = "deny"

[rules."api.example.com"]
paths = ["/v1/**"]

[[rules."api.example.com".inject]]
header = "Authorization"
secret = "api-token"
format = "bearer"
"#;

        validate_baffle_session_config(session).unwrap();
    }

    #[test]
    fn rejects_invalid_versions_fields_and_policy_values() {
        for session in [
            "version = 1\n",
            "version = 2\noperation = 'create'\n",
            "version = 2\nunmatched = 'allow'\n",
            "version = 2\n[rules.'*.example.com']\n",
            "version = 2\n[rules.'api.example.com']\nports = [0]\n",
            "version = 2\n[rules.'api.example.com']\nmode = 'tunnel'\npaths = ['/v1']\n",
            "version = 2\nsocket_name = '../outside.sock'\n",
        ] {
            assert!(
                validate_baffle_session_config(session).is_err(),
                "{session}"
            );
        }
    }
}
