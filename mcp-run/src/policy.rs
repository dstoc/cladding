use std::collections::{BTreeMap, HashSet};
use std::path::Path;

use regex::Regex;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use thiserror::Error;

pub type Policy = Vec<CommandRule>;

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CommandRule {
    pub command: String,
    #[serde(default)]
    pub args: Vec<ArgCheck>,
    #[serde(default)]
    pub env: Vec<String>,
    #[serde(default)]
    pub description: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "lowercase", deny_unknown_fields)]
pub enum ArgCheck {
    Exact {
        value: String,
        #[serde(default)]
        position: Option<usize>,
        #[serde(default)]
        required: Option<bool>,
    },
    Regex {
        pattern: String,
        #[serde(default)]
        position: Option<usize>,
        #[serde(default)]
        required: Option<bool>,
    },
    Hash {
        value: String,
        #[serde(default)]
        algorithm: Option<HashAlgorithm>,
        #[serde(default)]
        position: Option<usize>,
        #[serde(default)]
        required: Option<bool>,
    },
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum HashAlgorithm {
    Sha256,
}

impl ArgCheck {
    fn position(&self) -> Option<usize> {
        match self {
            ArgCheck::Exact { position, .. } => *position,
            ArgCheck::Regex { position, .. } => *position,
            ArgCheck::Hash { position, .. } => *position,
        }
    }

    fn required(&self) -> bool {
        match self {
            ArgCheck::Exact { required, .. } => required.unwrap_or(false),
            ArgCheck::Regex { required, .. } => required.unwrap_or(false),
            ArgCheck::Hash { required, .. } => required.unwrap_or(false),
        }
    }

    fn expected_description(&self) -> String {
        match self {
            ArgCheck::Exact { value, .. } => value.clone(),
            ArgCheck::Regex { .. } => "regex".to_string(),
            ArgCheck::Hash { .. } => "hash".to_string(),
        }
    }
}

#[derive(Debug, Error)]
pub enum PolicyLoadError {
    #[error("policy file not found")]
    NotFound,
    #[error("unable to read policy file: {source}")]
    Read { source: std::io::Error },
    #[error("invalid JSON in policy file: {source}")]
    InvalidJson { source: serde_json::Error },
    #[error("legacy field 'allowedHosts' is not supported; remove it from the policy")]
    LegacyAllowedHosts,
    #[error("policy schema validation failed: {source}")]
    InvalidSchema { source: serde_json::Error },
    #[error("invalid regex '{pattern}' in policy: {source}")]
    InvalidRegex {
        pattern: String,
        source: regex::Error,
    },
}

pub fn load_policy(policy_path: &Path) -> Result<Policy, PolicyLoadError> {
    let raw = std::fs::read_to_string(policy_path).map_err(|source| {
        if source.kind() == std::io::ErrorKind::NotFound {
            PolicyLoadError::NotFound
        } else {
            PolicyLoadError::Read { source }
        }
    })?;

    let value: serde_json::Value =
        serde_json::from_str(&raw).map_err(|source| PolicyLoadError::InvalidJson { source })?;

    if value.get("allowedHosts").is_some() {
        return Err(PolicyLoadError::LegacyAllowedHosts);
    }

    let policy: Policy = serde_json::from_value(value)
        .map_err(|source| PolicyLoadError::InvalidSchema { source })?;

    for rule in &policy {
        for check in &rule.args {
            if let ArgCheck::Regex { pattern, .. } = check {
                Regex::new(pattern).map_err(|source| PolicyLoadError::InvalidRegex {
                    pattern: pattern.clone(),
                    source,
                })?;
            }
        }
    }

    Ok(policy)
}

#[derive(Debug, Error)]
pub enum ValidationError {
    #[error("Command not allowed: {0}")]
    CommandNotAllowed(String),
    #[error("Command validation failed for '{command}'. Tried {rule_count} rule(s):\n- {details}")]
    RuleValidationFailed {
        command: String,
        rule_count: usize,
        details: String,
    },
}

pub fn validate_invocation(
    policy: &Policy,
    command: &str,
    args: &[String],
    env: &BTreeMap<String, String>,
) -> Result<(), ValidationError> {
    let rules: Vec<&CommandRule> = policy
        .iter()
        .filter(|rule| rule.command == command)
        .collect();

    if rules.is_empty() {
        return Err(ValidationError::CommandNotAllowed(command.to_string()));
    }

    let mut errors = Vec::with_capacity(rules.len());
    for rule in &rules {
        match validate_rule(args, &rule.args).and_then(|_| validate_env(env, &rule.env)) {
            Ok(()) => return Ok(()),
            Err(error) => errors.push(error),
        }
    }

    Err(ValidationError::RuleValidationFailed {
        command: command.to_string(),
        rule_count: rules.len(),
        details: errors.join("\n- "),
    })
}

fn validate_env(env: &BTreeMap<String, String>, allowed_env: &[String]) -> Result<(), String> {
    let allowed: HashSet<&str> = allowed_env.iter().map(String::as_str).collect();
    for key in env.keys() {
        if !allowed.contains(key.as_str()) {
            return Err(format!("Environment variable not allowed: {key}"));
        }
    }
    Ok(())
}

fn validate_rule(args: &[String], checks: &[ArgCheck]) -> Result<(), String> {
    if checks.is_empty() {
        if args.is_empty() {
            return Ok(());
        }
        return Err("Command does not allow arguments.".to_string());
    }

    for (index, arg) in args.iter().enumerate() {
        let mut matched = false;
        for check in checks {
            if let Some(position) = check.position() {
                if position != index {
                    continue;
                }
            }

            if check_arg(arg, check) {
                matched = true;
                break;
            }
        }

        if !matched {
            return Err(format!("Argument not allowed at position {index}: {arg}"));
        }
    }

    for check in checks {
        if !check.required() {
            continue;
        }

        let satisfied = if let Some(position) = check.position() {
            args.get(position)
                .is_some_and(|value| check_arg(value, check))
        } else {
            args.iter().any(|value| check_arg(value, check))
        };

        if !satisfied {
            if let Some(position) = check.position() {
                return Err(format!("Missing required argument at position {position}"));
            }
            return Err(format!(
                "Missing required argument matching: {}",
                check.expected_description()
            ));
        }
    }

    Ok(())
}

fn check_arg(arg: &str, check: &ArgCheck) -> bool {
    match check {
        ArgCheck::Exact { value, .. } => arg == value,
        ArgCheck::Regex { pattern, .. } => {
            Regex::new(pattern).is_ok_and(|regex| regex.is_match(arg))
        }
        ArgCheck::Hash {
            value, algorithm, ..
        } => {
            let algorithm = algorithm.unwrap_or(HashAlgorithm::Sha256);
            match algorithm {
                HashAlgorithm::Sha256 => check_file_sha256(arg, value),
            }
        }
    }
}

fn check_file_sha256(file_path: &str, expected_hash: &str) -> bool {
    let bytes = match std::fs::read(file_path) {
        Ok(bytes) => bytes,
        Err(_) => return false,
    };
    let hash = Sha256::digest(bytes);
    format!("{hash:x}") == expected_hash
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;

    use super::*;
    use sha2::{Digest, Sha256};
    use tempfile::NamedTempFile;

    fn write_policy_file(policy: serde_json::Value) -> NamedTempFile {
        let file = NamedTempFile::new().expect("temp policy file");
        std::fs::write(
            file.path(),
            serde_json::to_vec_pretty(&policy).expect("serialize policy"),
        )
        .expect("write policy");
        file
    }

    fn sha256_hex(bytes: &[u8]) -> String {
        let digest = Sha256::digest(bytes);
        format!("{digest:x}")
    }

    #[test]
    fn policy_parses_exact_regex_hash_and_env() {
        let hashed_file = NamedTempFile::new().expect("temp file");
        std::fs::write(hashed_file.path(), b"hello-hash").expect("write hash file");
        let expected_hash = sha256_hex(b"hello-hash");

        let policy: Policy = vec![CommandRule {
            command: "cmd".to_string(),
            args: vec![
                ArgCheck::Exact {
                    value: "install".to_string(),
                    position: Some(0),
                    required: Some(true),
                },
                ArgCheck::Regex {
                    pattern: "^pkg-[a-z]+$".to_string(),
                    position: Some(1),
                    required: Some(true),
                },
                ArgCheck::Hash {
                    value: expected_hash,
                    algorithm: Some(HashAlgorithm::Sha256),
                    position: Some(2),
                    required: Some(true),
                },
            ],
            env: vec!["TOKEN".to_string()],
            description: None,
        }];

        let args = vec![
            "install".to_string(),
            "pkg-core".to_string(),
            hashed_file.path().to_string_lossy().to_string(),
        ];
        let env = BTreeMap::from([(String::from("TOKEN"), String::from("abc"))]);

        assert!(validate_invocation(&policy, "cmd", &args, &env).is_ok());
    }

    #[test]
    fn policy_enforces_position_and_required_rules() {
        let policy: Policy = vec![CommandRule {
            command: "git".to_string(),
            args: vec![
                ArgCheck::Exact {
                    value: "commit".to_string(),
                    position: Some(0),
                    required: Some(true),
                },
                ArgCheck::Exact {
                    value: "-m".to_string(),
                    position: None,
                    required: Some(true),
                },
                ArgCheck::Regex {
                    pattern: ".*".to_string(),
                    position: None,
                    required: Some(false),
                },
            ],
            env: vec![],
            description: None,
        }];

        let missing_required = vec!["commit".to_string(), "message".to_string()];
        let err = validate_invocation(&policy, "git", &missing_required, &BTreeMap::new())
            .expect_err("missing -m should fail");
        assert!(
            err.to_string()
                .contains("Missing required argument matching: -m")
        );

        let good = vec![
            "commit".to_string(),
            "-m".to_string(),
            "message".to_string(),
        ];
        assert!(validate_invocation(&policy, "git", &good, &BTreeMap::new()).is_ok());
    }

    #[test]
    fn policy_enforces_env_allowlist() {
        let policy: Policy = vec![CommandRule {
            command: "npm".to_string(),
            args: vec![ArgCheck::Regex {
                pattern: ".*".to_string(),
                position: None,
                required: Some(false),
            }],
            env: vec!["TOKEN".to_string()],
            description: None,
        }];

        let env = BTreeMap::from([(String::from("UNSAFE"), String::from("1"))]);
        let err = validate_invocation(&policy, "npm", &["test".into()], &env)
            .expect_err("disallowed env key should fail");
        assert!(
            err.to_string()
                .contains("Environment variable not allowed: UNSAFE")
        );
    }

    #[test]
    fn policy_applies_or_across_multiple_rules() {
        let policy: Policy = vec![
            CommandRule {
                command: "npm".to_string(),
                args: vec![ArgCheck::Exact {
                    value: "install".to_string(),
                    position: Some(0),
                    required: Some(true),
                }],
                env: vec![],
                description: None,
            },
            CommandRule {
                command: "npm".to_string(),
                args: vec![ArgCheck::Exact {
                    value: "test".to_string(),
                    position: Some(0),
                    required: Some(true),
                }],
                env: vec![],
                description: None,
            },
        ];

        assert!(
            validate_invocation(&policy, "npm", &["test".to_string()], &BTreeMap::new()).is_ok()
        );

        let err = validate_invocation(&policy, "npm", &["publish".to_string()], &BTreeMap::new())
            .expect_err("unknown mode should fail");
        assert!(err.to_string().contains("Tried 2 rule(s)"));
    }

    #[test]
    fn policy_rejects_allowed_hosts_field() {
        let file = write_policy_file(serde_json::json!({
            "allowedHosts": ["example.com"]
        }));
        let err = load_policy(file.path()).expect_err("allowedHosts should be rejected");
        assert!(err.to_string().contains("allowedHosts"));
    }

    #[test]
    fn policy_rejects_commands_wrapper_object() {
        let file = write_policy_file(serde_json::json!({
            "commands": []
        }));
        let err = load_policy(file.path()).expect_err("commands wrapper should be rejected");
        assert!(matches!(err, PolicyLoadError::InvalidSchema { .. }));
    }
}
