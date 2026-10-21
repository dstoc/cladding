//! Trusted helpers for deriving Baffle v2 session credentials.

use baffle_client::{CredentialMode, SessionConfig, validate_secret_name};
use base64::{Engine as _, engine::general_purpose::URL_SAFE_NO_PAD};
use ring::hmac::{self, HMAC_SHA256, Key};
use std::collections::{BTreeMap, BTreeSet};

const SEED_BYTES: usize = 32;
const DOMAIN_SEPARATOR: &[u8] = b"baffle:credential-substitution:v1\0";

/// Return whether a name is a Baffle credential identifier.
pub fn is_credential_identifier(name: &str) -> bool {
    validate_secret_name(name).is_ok()
}

/// Validate environment mappings against the selected Baffle agent policy.
///
/// Each mapped credential must have a real secret file available, be referenced
/// by an agent rule, and use Baffle's `substitute` credential mode.
pub fn validate_mappings(
    session_config_toml: &str,
    mappings: &BTreeMap<String, String>,
    available_credentials: &BTreeSet<String>,
) -> Result<(), String> {
    if mappings.is_empty() {
        return Ok(());
    }

    let session = SessionConfig::from_toml(session_config_toml)
        .map_err(|error| format!("invalid Baffle agent session config: {error}"))?;
    for (environment_name, credential_name) in mappings {
        if !available_credentials.contains(credential_name) {
            return Err(format!(
                "fake credential mapping for '{environment_name}' references unavailable Baffle credential '{credential_name}'"
            ));
        }
        if session.effective_credential_mode(credential_name) != CredentialMode::Substitute {
            return Err(format!(
                "fake credential mapping for '{environment_name}' requires Baffle credential '{credential_name}' to use substitute mode"
            ));
        }
        if !session
            .rules
            .iter()
            .flat_map(|rule| &rule.inject)
            .any(|injection| injection.secret == *credential_name)
        {
            return Err(format!(
                "fake credential mapping for '{environment_name}' references Baffle credential '{credential_name}', which is not used by an agent session rule"
            ));
        }
    }
    Ok(())
}

/// Derive a fake credential from Baffle's unpadded Base64URL session seed.
///
/// This matches Baffle v2.1.0's `baffle:credential-substitution:v1\0` HMAC
/// domain, exact credential identifier bytes, and `baffle_v1_` representation.
pub fn derive_fake_credential(
    seed_base64url: &str,
    credential_name: &str,
) -> Result<String, String> {
    let seed = URL_SAFE_NO_PAD
        .decode(seed_base64url)
        .map_err(|_| "Baffle returned an invalid credential seed".to_string())?;
    if seed.len() != SEED_BYTES {
        return Err("Baffle returned a credential seed with an invalid length".to_string());
    }
    validate_secret_name(credential_name)
        .map_err(|_| "Baffle credential identifier is invalid".to_string())?;

    let key = Key::new(HMAC_SHA256, &seed);
    let mut context = hmac::Context::with_key(&key);
    context.update(DOMAIN_SEPARATOR);
    context.update(credential_name.as_bytes());
    let digest = context.sign();
    Ok(format!(
        "baffle_v1_{}",
        URL_SAFE_NO_PAD.encode(digest.as_ref())
    ))
}

/// Derive each environment value. The variable name does not enter derivation.
pub fn derive_environment(
    seed_base64url: &str,
    mappings: &BTreeMap<String, String>,
) -> Result<BTreeMap<String, String>, String> {
    mappings
        .iter()
        .map(|(environment_name, credential_name)| {
            derive_fake_credential(seed_base64url, credential_name)
                .map(|fake| (environment_name.clone(), fake))
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::{derive_environment, derive_fake_credential, validate_mappings};
    use base64::{Engine as _, engine::general_purpose::URL_SAFE_NO_PAD};
    use std::collections::{BTreeMap, BTreeSet};

    const PUBLISHED_VECTOR: &str = "baffle_v1_Tamc3Pa1DyIVEIq8yK6rlwaPc_L-LdF8S9YHz-17h1I";

    fn seed() -> String {
        URL_SAFE_NO_PAD.encode((0_u8..32).collect::<Vec<_>>())
    }

    fn substituting_policy() -> &'static str {
        r#"
version = 2
persistent = true
socket_name = "agent/proxy.sock"

[credentials]
default_mode = "substitute"

[rules."api.example.com"]
mode = "intercept"
ports = [443]
paths = ["/**"]

[[rules."api.example.com".inject]]
header = "Authorization"
secret = "github-api"
format = "bearer"
"#
    }

    #[test]
    fn matches_the_baffle_v2_1_published_test_vector() {
        assert_eq!(
            derive_fake_credential(&seed(), "github-api").unwrap(),
            PUBLISHED_VECTOR
        );
    }

    #[test]
    fn same_credential_has_the_same_fake_across_environment_names() {
        let mappings = BTreeMap::from([
            ("GH_TOKEN".to_string(), "github-api".to_string()),
            ("GITHUB_TOKEN".to_string(), "github-api".to_string()),
        ]);
        let values = derive_environment(&seed(), &mappings).unwrap();
        assert_eq!(values["GH_TOKEN"], values["GITHUB_TOKEN"]);
    }

    #[test]
    fn another_seed_derives_another_fake() {
        let first = derive_fake_credential(&seed(), "github-api").unwrap();
        let second_seed = URL_SAFE_NO_PAD.encode(vec![0xA5; 32]);
        let second = derive_fake_credential(&second_seed, "github-api").unwrap();
        assert_ne!(first, second);
    }

    #[test]
    fn rejects_wrong_length_or_invalid_seeds_without_echoing_them() {
        for invalid in ["not a seed", "AA"] {
            let error = derive_fake_credential(invalid, "github-api").unwrap_err();
            assert!(!error.contains(invalid));
        }
    }

    #[test]
    fn validates_real_secret_substitution_mode_and_rule_reference() {
        let mappings = BTreeMap::from([("GH_TOKEN".to_string(), "github-api".to_string())]);
        let available = BTreeSet::from(["github-api".to_string()]);
        validate_mappings(substituting_policy(), &mappings, &available).unwrap();

        let inject_policy = substituting_policy().replace("= \"substitute\"", "= \"inject\"");
        assert!(
            validate_mappings(&inject_policy, &mappings, &available)
                .unwrap_err()
                .contains("substitute mode")
        );

        let unreferenced =
            substituting_policy().replace("secret = \"github-api\"", "secret = \"other\"");
        assert!(
            validate_mappings(&unreferenced, &mappings, &available)
                .unwrap_err()
                .contains("not used by an agent session rule")
        );

        assert!(
            validate_mappings(substituting_policy(), &mappings, &BTreeSet::new())
                .unwrap_err()
                .contains("unavailable Baffle credential")
        );
    }
}
