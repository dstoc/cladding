/// Validate the document-level compatibility fields required by Cladding.
///
/// Baffle owns the complete session schema and policy validation when it
/// creates or reloads a session.
pub fn validate_baffle_session_config(input: &str) -> anyhow::Result<()> {
    parse_baffle_session_document(input).map(drop)
}

fn parse_baffle_session_document(input: &str) -> anyhow::Result<toml::Table> {
    let document: toml::Value = toml::from_str(input)
        .map_err(|_| anyhow::anyhow!("invalid Baffle session TOML syntax or document"))?;
    let toml::Value::Table(document) = document else {
        anyhow::bail!("Baffle session configuration must be a TOML table");
    };
    if document.get("version").and_then(toml::Value::as_integer) != Some(2) {
        anyhow::bail!("Baffle session configuration must use version = 2");
    }
    Ok(document)
}

/// Validate Cladding's required session format and component data socket.
///
/// Baffle validates the socket path and all policy fields. Requiring an exact
/// component socket name here prevents a selected file from using another
/// component's mounted socket.
pub fn validate_baffle_session_config_for_socket(
    input: &str,
    expected_socket_name: &str,
) -> anyhow::Result<()> {
    let document = parse_baffle_session_document(input)?;
    match document.get("socket_name").and_then(toml::Value::as_str) {
        Some(socket_name) if socket_name == expected_socket_name => Ok(()),
        Some(socket_name) => anyhow::bail!(
            "Baffle session socket_name must be '{expected_socket_name}' for this component (found '{socket_name}')"
        ),
        None => anyhow::bail!(
            "Baffle session must set socket_name = '{expected_socket_name}' for this component"
        ),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn accepts_a_valid_cladding_session_header() {
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

        validate_baffle_session_config_for_socket(session, "agent/proxy.sock").unwrap();
    }

    #[test]
    fn rejects_malformed_documents_and_unsupported_versions() {
        for session in [
            "version = [\n",
            "- 'not a document table'\n",
            "version = 1\nsocket_name = 'agent/proxy.sock'\n",
            "version = 2\nsocket_name = 'nw-sandbox/proxy.sock'\n",
            "version = 2\nsocket_name = 3\n",
        ] {
            assert!(
                validate_baffle_session_config_for_socket(session, "agent/proxy.sock").is_err(),
                "{session}"
            );
        }
    }
}
