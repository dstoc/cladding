use anyhow::{Context as _, bail};
use baffle_client::{Client, default_control_socket};
use cladding_fake_credentials::derive_environment;
use std::collections::BTreeMap;
use std::fs::{self, OpenOptions};
use std::io::{Read as _, Write as _};
use std::os::unix::fs::{MetadataExt as _, OpenOptionsExt as _, PermissionsExt as _};
use std::path::Path;

#[tokio::main(flavor = "current_thread")]
async fn main() {
    if let Err(error) = run().await {
        eprintln!("error: {error:#}");
        std::process::exit(1);
    }
}

async fn run() -> anyhow::Result<()> {
    let mut arguments = std::env::args().skip(1);
    match arguments.next().as_deref() {
        Some("create-agent-session") => {
            let session_name = arguments.next().context("missing Baffle session name")?;
            let seed_path = arguments.next().context("missing private seed path")?;
            if arguments.next().is_some() {
                bail!("unexpected argument");
            }
            create_agent_session(&session_name, Path::new(&seed_path)).await
        }
        Some("derive") => {
            let seed_path = arguments.next().context("missing private seed path")?;
            let mappings = parse_mappings(arguments)?;
            let seed = read_private_seed(Path::new(&seed_path))?;
            let values = derive_environment(&seed, &mappings)
                .map_err(anyhow::Error::msg)
                .context("failed to derive Baffle fake credentials")?;
            fs::remove_file(&seed_path).context("failed to remove the private Baffle seed")?;
            serde_json::to_writer(std::io::stdout().lock(), &values)
                .context("failed to write derived environment values")?;
            Ok(())
        }
        _ => bail!(
            "usage: cladding-fake-credentials create-agent-session NAME SEED_FILE | derive SEED_FILE NAME=CREDENTIAL..."
        ),
    }
}

fn parse_mappings(
    arguments: impl Iterator<Item = String>,
) -> anyhow::Result<BTreeMap<String, String>> {
    let mut mappings = BTreeMap::new();
    for mapping in arguments {
        let Some((environment_name, credential_name)) = mapping.split_once('=') else {
            bail!("fake credential mapping must use NAME=CREDENTIAL");
        };
        if environment_name.is_empty() || credential_name.is_empty() {
            bail!("fake credential mapping must use NAME=CREDENTIAL");
        }
        if mappings
            .insert(environment_name.to_string(), credential_name.to_string())
            .is_some()
        {
            bail!("duplicate fake credential environment mapping");
        }
    }
    if mappings.is_empty() {
        bail!("at least one fake credential mapping is required");
    }
    Ok(mappings)
}

async fn create_agent_session(session_name: &str, seed_path: &Path) -> anyhow::Result<()> {
    prepare_seed_destination(seed_path)?;
    let control_socket = std::env::var_os("BAFFLE_CONTROL_SOCKET")
        .map(std::path::PathBuf::from)
        .unwrap_or_else(default_control_socket);
    let client = Client::new(control_socket);
    let session = client
        .create_from_file(session_name)
        .await
        .context("failed to create Baffle agent session")?;

    let result = (|| {
        if !session.is_persistent() {
            bail!("Baffle agent session must be persistent");
        }
        let seed = session
            .credential_seed_base64url()
            .context("Baffle did not return a session credential seed")?;
        write_private_seed(seed_path, seed.as_bytes())
    })();
    if let Err(error) = result {
        client
            .stop(session.id())
            .await
            .context("failed to stop Baffle session after seed setup failed")?;
        return Err(error);
    }
    Ok(())
}

fn prepare_seed_destination(seed_path: &Path) -> anyhow::Result<()> {
    let parent = seed_path
        .parent()
        .context("private seed path must have a parent directory")?;
    match fs::symlink_metadata(parent) {
        Ok(metadata) if metadata.file_type().is_symlink() || !metadata.is_dir() => {
            bail!("private Baffle seed directory is not a regular directory");
        }
        Ok(_) => {}
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            fs::create_dir_all(parent).context("failed to create private Baffle seed directory")?;
        }
        Err(error) => return Err(error).context("failed to inspect private Baffle seed directory"),
    }
    fs::set_permissions(parent, fs::Permissions::from_mode(0o700))
        .context("failed to secure private Baffle seed directory")?;
    match fs::symlink_metadata(seed_path) {
        Ok(_) => bail!("private Baffle seed path already exists"),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(error).context("failed to inspect private Baffle seed path"),
    }
}

fn write_private_seed(seed_path: &Path, seed: &[u8]) -> anyhow::Result<()> {
    let mut file = OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(seed_path)
        .context("failed to create private Baffle seed file")?;
    file.write_all(seed)
        .context("failed to write private Baffle seed file")?;
    file.sync_all()
        .context("failed to sync private Baffle seed file")?;
    fs::set_permissions(seed_path, fs::Permissions::from_mode(0o600))
        .context("failed to secure private Baffle seed file")?;
    Ok(())
}

fn read_private_seed(seed_path: &Path) -> anyhow::Result<String> {
    let metadata = fs::symlink_metadata(seed_path).context("private Baffle seed is unavailable")?;
    if metadata.file_type().is_symlink() || !metadata.is_file() {
        bail!("private Baffle seed path is not a regular file");
    }
    let mode = metadata.permissions().mode();
    if mode & 0o077 != 0 || metadata.uid() != unsafe { libc::geteuid() } {
        bail!("private Baffle seed file has unsafe ownership or permissions");
    }
    let mut seed = String::new();
    OpenOptions::new()
        .read(true)
        .open(seed_path)
        .context("failed to open private Baffle seed file")?
        .read_to_string(&mut seed)
        .context("failed to read private Baffle seed file")?;
    Ok(seed)
}

#[cfg(test)]
mod tests {
    use super::parse_mappings;

    #[test]
    fn parses_multiple_fake_credential_mappings_without_duplicates() {
        let parsed = parse_mappings(
            ["GH_TOKEN=github-api", "CUSTOM_TOKEN=github-api"]
                .into_iter()
                .map(str::to_string),
        )
        .unwrap();
        assert_eq!(parsed.len(), 2);
        assert_eq!(parsed["GH_TOKEN"], "github-api");
    }

    #[test]
    fn rejects_duplicate_mapping_names() {
        assert!(
            parse_mappings(["TOKEN=one", "TOKEN=two"].into_iter().map(str::to_string)).is_err()
        );
    }
}
