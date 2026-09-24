use crate::assets::containerfile as embedded_containerfile;
use crate::error::{Error, Result};
use anyhow::Context as _;
use std::collections::BTreeMap;
use std::io::Write;
use std::path::Path;
use std::process::{Command, Stdio};

use super::command::ensure_success;

pub fn podman_build_image(
    image: &str,
    containerfile: Option<&Path>,
    context: &Path,
    build_args: &BTreeMap<String, String>,
    host_ids: Option<(u32, u32)>,
) -> Result<()> {
    if let Some(containerfile) = containerfile
        && !containerfile.is_file()
    {
        eprintln!(
            "error: Containerfile does not exist: {}",
            containerfile.display()
        );
        return Err(Error::message("missing Containerfile"));
    }
    let mut cmd = build_command(image, containerfile, context, build_args, host_ids);

    let mut child = cmd.spawn().with_context(|| "failed to run podman build")?;
    if containerfile.is_none()
        && let Some(mut stdin) = child.stdin.take()
    {
        stdin
            .write_all(embedded_containerfile().as_bytes())
            .and_then(|_| stdin.flush())
            .with_context(|| "failed to write Containerfile to podman")?;
    }

    let status = child
        .wait()
        .with_context(|| "failed to wait on podman build")?;

    ensure_success(status, "podman build")
}

fn build_command(
    image: &str,
    containerfile: Option<&Path>,
    context: &Path,
    build_args: &BTreeMap<String, String>,
    host_ids: Option<(u32, u32)>,
) -> Command {
    let mut cmd = Command::new("podman");
    cmd.arg("build");
    if let Some((host_uid, host_gid)) = host_ids {
        cmd.arg("--build-arg")
            .arg(format!("UID={host_uid}"))
            .arg("--build-arg")
            .arg(format!("GID={host_gid}"));
    }
    for (name, value) in build_args {
        cmd.arg("--build-arg").arg(format!("{name}={value}"));
    }
    cmd.arg("-t").arg(image).arg("-f");
    match containerfile {
        Some(containerfile) => {
            cmd.arg(containerfile);
        }
        None => {
            cmd.arg("-").stdin(Stdio::piped());
        }
    }
    cmd.arg(context);
    cmd
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::BTreeMap;

    #[test]
    fn build_command_passes_custom_file_context_args_and_default_uid_gid() {
        let args = BTreeMap::from([("FEATURE".to_string(), "enabled".to_string())]);
        let command = build_command(
            "localhost/custom:latest",
            Some(Path::new("/tmp/config/agent.Containerfile")),
            Path::new("/tmp/config/context"),
            &args,
            Some((1001, 1002)),
        );

        let args = command
            .get_args()
            .map(|arg| arg.to_string_lossy().into_owned())
            .collect::<Vec<_>>();
        assert_eq!(
            args,
            [
                "build",
                "--build-arg",
                "UID=1001",
                "--build-arg",
                "GID=1002",
                "--build-arg",
                "FEATURE=enabled",
                "-t",
                "localhost/custom:latest",
                "-f",
                "/tmp/config/agent.Containerfile",
                "/tmp/config/context",
            ]
        );
        assert!(!args.windows(2).any(|pair| pair == ["-f", "-"]));
    }

    #[test]
    fn build_command_embeds_default_containerfile_and_preserves_uid_gid() {
        let command = build_command(
            "localhost/cladding-default:latest",
            None,
            Path::new("/tmp/project"),
            &BTreeMap::new(),
            Some((1000, 1000)),
        );
        let args = command
            .get_args()
            .map(|arg| arg.to_string_lossy().into_owned())
            .collect::<Vec<_>>();
        assert!(
            args.windows(2)
                .any(|pair| pair == ["--build-arg", "UID=1000"])
        );
        assert!(
            args.windows(2)
                .any(|pair| pair == ["--build-arg", "GID=1000"])
        );
        assert_eq!(args.last().map(String::as_str), Some("/tmp/project"));
        assert!(args.windows(2).any(|pair| pair == ["-f", "-"]));
    }
}
