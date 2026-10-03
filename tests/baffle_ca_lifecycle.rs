#![cfg(unix)]

use rcgen::{BasicConstraints, CertificateParams, DnType, IsCa, KeyPair, KeyUsagePurpose};
use std::env;
use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};
use std::sync::atomic::{AtomicU64, Ordering};
use time::{Duration, OffsetDateTime};

static TEMP_COUNTER: AtomicU64 = AtomicU64::new(0);

#[test]
fn build_check_up_and_run_follow_the_persistent_and_ephemeral_ca_lifecycles() {
    let root = temp_dir("baffle-ca-lifecycle");
    let project_root = root.join(".cladding");
    let bin_dir = root.join("mock-bin");
    fs::create_dir_all(&bin_dir).unwrap();
    let podman_log = root.join("podman.log");
    let certificate_source = root.join("test-ca.crt");
    let private_key_source = root.join("test-ca-key.pem");
    write_test_ca(&certificate_source, &private_key_source);
    let podman = bin_dir.join("podman");
    fs::write(
        &podman,
        r#"#!/bin/sh
printf '%s\n' "$*" >> "$CLADDING_PODMAN_LOG"
case "$1:$2" in
  pod:create|pod:rm|run:*|exec:--user|rm:-f)
    if [ "$CLADDING_TEST_HELPER_NOISE" = "1" ]; then
      printf 'helper stdout: %s %s\n' "$1" "$2"
      printf 'helper stderr: %s %s\n' "$1" "$2" >&2
    fi
    ;;
esac
if [ "$CLADDING_TEST_HELPER_NOISE" = "1" ]; then
  for argument in "$@"; do
    if [ "$argument" = "-d" ]; then
      printf 'container-id: fake-podman-id\\n'
      break
    fi
  done
fi
if [ "$1" = "exec" ] && [ "$2" = "--user" ]; then
  if [ "$CLADDING_TEST_FAIL_CA_INSTALL" = "1" ]; then
    exit 42
  fi
  exit 0
fi
if [ "$1" = "exec" ]; then
  printf '%s' "${CLADDING_TEST_USER_STDOUT:-}"
  printf '%s' "${CLADDING_TEST_USER_STDERR:-}" >&2
  exit 0
fi
if [ "$1" = "rm" ] && [ "$CLADDING_TEST_FAIL_CLEANUP" = "1" ]; then
  exit 43
fi
if [ "$1" = "pod" ] && [ "$2" = "ps" ]; then
  if [ -f "$CLADDING_PODMAN_STATE" ]; then
    project_name=$(sed -n '1p' "$CLADDING_PODMAN_STATE")
    project_root=$(sed -n '2p' "$CLADDING_PODMAN_STATE")
    printf '[{"Labels":{"cladding":"%s","project_root":"%s"}}]\n' "$project_name" "$project_root"
  else
    printf '[]\n'
  fi
  exit 0
fi
if [ "$1" = "pod" ] && [ "$2" = "create" ]; then
  shift 2
  project_name=
  project_root=
  while [ "$#" -gt 0 ]; do
    if [ "$1" = "--label" ]; then
      shift
      case "$1" in
        cladding=*) project_name=${1#cladding=} ;;
        project_root=*) project_root=${1#project_root=} ;;
      esac
    fi
    shift
  done
  printf '%s\n%s\n' "$project_name" "$project_root" > "$CLADDING_PODMAN_STATE"
  exit 0
fi
if [ "$1" = "image" ] && [ "$2" = "exists" ]; then
  exit 0
fi
if [ "$1" = "info" ]; then
  printf '{"runsc":{"path":"runsc"}}\n'
  exit 0
fi
if [ "$2" = "exists" ]; then
  if [ -f "$CLADDING_PODMAN_STATE" ]; then exit 0; else exit 1; fi
fi
if [ "$2" = "inspect" ]; then
  if { [ "$1" = "container" ] && [ "$4" = "{{json .Config.Labels}}" ]; } || \
     { [ "$1" = "pod" ] && [ "$4" = "{{json .Labels}}" ]; }; then
    project_name=$(sed -n '1p' "$CLADDING_PODMAN_STATE")
    project_root=$(sed -n '2p' "$CLADDING_PODMAN_STATE")
    printf '{"cladding":"%s","project_root":"%s"}\n' "$project_name" "$project_root"
  else
    printf 'running\n'
  fi
  exit 0
fi
if [ "$1" = "run" ]; then
  shift
  credentials_dir=
  config_dir=
  secret_source_dir=
  while [ "$#" -gt 0 ]; do
    if [ "$1" = "--volume" ]; then
      shift
      case "$1" in
        *:/opt/credentials/baffle/secrets:ro)
          secret_source_dir=${1%:/opt/credentials/baffle/secrets:ro}
          ;;
        *:/opt/credentials/baffle:ro)
          credentials_dir=${1%:/opt/credentials/baffle:ro}
          ;;
        *:/opt/credentials/baffle)
          credentials_dir=${1%:/opt/credentials/baffle}
          ;;
        *:/opt/config:ro)
          config_dir=${1%:/opt/config:ro}
          ;;
      esac
    fi
    shift
  done
  if [ -n "$config_dir" ] && [ -f "$config_dir/proxy/sessions/agent.toml" ]; then
    printf '%s\n' 'PROJECT_AGENT_SESSION_SNAPSHOT' >> "$CLADDING_PODMAN_LOG"
    cat "$config_dir/proxy/sessions/agent.toml" >> "$CLADDING_PODMAN_LOG"
  fi
  if [ -n "$secret_source_dir" ]; then
    printf 'PROJECT_BAFFLE_SECRET_SOURCE=%s\n' "$secret_source_dir" >> "$CLADDING_PODMAN_LOG"
  fi
  if [ -n "$credentials_dir" ] && [ -n "$secret_source_dir" ] \
    && [ "$credentials_dir" != "$secret_source_dir" ]; then
    if [ -n "$(find "$credentials_dir/secrets" -mindepth 1 -print -quit)" ]; then
      echo "one-off runtime copied project secrets into its private credentials" >&2
      exit 1
    fi
    printf '%s\n' 'ONE_OFF_SECRET_SOURCE_IS_SEPARATE_AND_PRIVATE' >> "$CLADDING_PODMAN_LOG"
  fi
  if [ -n "$credentials_dir" ]; then
    cp "$CLADDING_TEST_CA_CERT" "$credentials_dir/ca.crt"
    cp "$CLADDING_TEST_CA_KEY" "$credentials_dir/ca-key.pem"
  fi
fi
exit 0
"#,
    )
    .unwrap();
    fs::set_permissions(&podman, fs::Permissions::from_mode(0o755)).unwrap();

    let mut init = cli(
        &root,
        &bin_dir,
        &podman_log,
        &certificate_source,
        &private_key_source,
    );
    init.args([
        "--cladding-dir",
        project_root.to_str().unwrap(),
        "init",
        "demo",
    ]);
    assert_success(init.output().unwrap());

    let project_secret_path = project_root.join("credentials/baffle/secrets/test-token");
    fs::write(&project_secret_path, b"project-only secret value").unwrap();
    fs::set_permissions(&project_secret_path, fs::Permissions::from_mode(0o600)).unwrap();

    let agent_session_path = project_root.join("config/proxy/sessions/agent.toml");
    let project_session = format!(
        "{}\n[rules.\"example.com\"]\nports = [443]\npaths = [\"/**\"]\n\n[[rules.\"example.com\".inject]]\nheader = \"Authorization\"\nsecret = \"test-token\"\nformat = \"bearer\"\n",
        fs::read_to_string(&agent_session_path).unwrap()
    );
    fs::write(&agent_session_path, &project_session).unwrap();

    let mut run_before_build = cli(
        &root,
        &bin_dir,
        &podman_log,
        &certificate_source,
        &private_key_source,
    );
    run_before_build.args([
        "--cladding-dir",
        project_root.to_str().unwrap(),
        "run",
        "sh",
        "-c",
        "printf user-output; printf user-error >&2",
    ]);
    run_before_build
        .env("CLADDING_TEST_HELPER_NOISE", "1")
        .env("CLADDING_TEST_USER_STDOUT", "user-output")
        .env("CLADDING_TEST_USER_STDERR", "user-error");
    let run_before_build_output = run_before_build.output().unwrap();
    assert_success(run_before_build_output.clone());
    assert_eq!(
        String::from_utf8_lossy(&run_before_build_output.stdout),
        "user-output",
        "quiet run should preserve only the user command's stdout"
    );
    assert_eq!(
        String::from_utf8_lossy(&run_before_build_output.stderr),
        "user-error",
        "quiet run should preserve only the user command's stderr"
    );
    assert!(
        read_log(&podman_log).contains("[rules.\"example.com\"]"),
        "cladding run did not pass the project agent session policy to its private runtime"
    );
    let podman_commands = read_log(&podman_log);
    assert!(
        podman_commands.contains(&format!(
            "{}:/opt/credentials/baffle/secrets:ro",
            project_root.join("credentials/baffle/secrets").display()
        )),
        "cladding run did not mount the selected project secrets read-only"
    );
    assert!(
        podman_commands.contains("ONE_OFF_SECRET_SOURCE_IS_SEPARATE_AND_PRIVATE"),
        "cladding run did not keep project secret values out of private runtime credentials"
    );
    assert_eq!(
        fs::read(&project_secret_path).unwrap(),
        b"project-only secret value",
        "cladding run changed the project secret source"
    );
    assert_eq!(
        fs::read_to_string(&agent_session_path).unwrap(),
        project_session,
        "cladding run changed the project's agent session policy"
    );
    assert!(
        !project_root.join("credentials/baffle/ca.crt").exists(),
        "ephemeral run must not initialize the persistent project CA"
    );
    assert!(
        !project_root.join("credentials/baffle/ca-key.pem").exists(),
        "ephemeral run must not create the persistent project CA key"
    );
    let podman_state = podman_log.with_extension("state");
    if podman_state.exists() {
        fs::remove_file(&podman_state).unwrap();
    }

    let ca_init_count_before_build = read_log(&podman_log)
        .lines()
        .filter(|line| line.starts_with("run --rm "))
        .count();
    assert_eq!(
        ca_init_count_before_build, 1,
        "ephemeral run should initialize its private CA before project build"
    );
    let mut build = cli(
        &root,
        &bin_dir,
        &podman_log,
        &certificate_source,
        &private_key_source,
    );
    build.args(["--cladding-dir", project_root.to_str().unwrap(), "build"]);
    assert_success(build.output().unwrap());
    let certificate = fs::read(project_root.join("credentials/baffle/ca.crt")).unwrap();
    let private_key = fs::read(project_root.join("credentials/baffle/ca-key.pem")).unwrap();
    let ca_init_count_after_build = read_log(&podman_log)
        .lines()
        .filter(|line| line.starts_with("run --rm "))
        .count();
    assert_eq!(
        ca_init_count_after_build,
        ca_init_count_before_build + 1,
        "build should initialize the project CA once"
    );

    let mut second_build = cli(
        &root,
        &bin_dir,
        &podman_log,
        &certificate_source,
        &private_key_source,
    );
    second_build.args(["--cladding-dir", project_root.to_str().unwrap(), "build"]);
    assert_success(second_build.output().unwrap());
    assert_eq!(
        read_log(&podman_log)
            .lines()
            .filter(|line| line.starts_with("run --rm "))
            .count(),
        ca_init_count_after_build,
        "a repeated build must reuse the CA"
    );
    assert_eq!(
        fs::read(project_root.join("credentials/baffle/ca.crt")).unwrap(),
        certificate
    );
    assert_eq!(
        fs::read(project_root.join("credentials/baffle/ca-key.pem")).unwrap(),
        private_key
    );

    let mut check = cli(
        &root,
        &bin_dir,
        &podman_log,
        &certificate_source,
        &private_key_source,
    );
    check.args(["--cladding-dir", project_root.to_str().unwrap(), "check"]);
    assert_success(check.output().unwrap());

    fs::write(project_root.join("credentials/baffle/ca.crt"), b"corrupt").unwrap();
    let mut invalid_check = cli(
        &root,
        &bin_dir,
        &podman_log,
        &certificate_source,
        &private_key_source,
    );
    invalid_check.args(["--cladding-dir", project_root.to_str().unwrap(), "check"]);
    let invalid_result = invalid_check.output().unwrap();
    assert!(!invalid_result.status.success());
    assert!(String::from_utf8_lossy(&invalid_result.stderr).contains("invalid Baffle CA"));
    fs::write(project_root.join("credentials/baffle/ca.crt"), &certificate).unwrap();

    let config_path = project_root.join("cladding.json");
    let mut config: serde_json::Value =
        serde_json::from_slice(&fs::read(&config_path).unwrap()).unwrap();
    config["use_runsc"] = true.into();
    fs::write(&config_path, serde_json::to_vec_pretty(&config).unwrap()).unwrap();

    fs::remove_file(project_root.join("credentials/baffle/ca.crt")).unwrap();
    fs::remove_file(project_root.join("credentials/baffle/ca-key.pem")).unwrap();
    fs::write(&podman_log, "").unwrap();
    let mut up = cli(
        &root,
        &bin_dir,
        &podman_log,
        &certificate_source,
        &private_key_source,
    );
    up.args(["--cladding-dir", project_root.to_str().unwrap(), "up"]);
    let up_result = up.output().unwrap();
    assert!(!up_result.status.success());
    let up_stderr = String::from_utf8_lossy(&up_result.stderr);
    assert!(
        up_stderr.contains("incomplete Baffle CA"),
        "up failed for an unexpected reason: {up_stderr}"
    );
    let up_calls = read_log(&podman_log);
    assert!(
        !up_calls.contains("pod create"),
        "up created a pod before CA validation"
    );
    assert!(
        !up_calls.lines().any(is_detached_container_run),
        "up started a container before CA validation"
    );
    assert!(
        !up_calls.contains("run --rm"),
        "up attempted CA initialization"
    );

    fs::write(project_root.join("credentials/baffle/ca.crt"), &certificate).unwrap();
    fs::write(
        project_root.join("credentials/baffle/ca-key.pem"),
        &private_key,
    )
    .unwrap();
    let mut run = cli(
        &root,
        &bin_dir,
        &podman_log,
        &certificate_source,
        &private_key_source,
    );
    run.args([
        "--cladding-dir",
        project_root.to_str().unwrap(),
        "run",
        "--verbose",
        "--",
        "sh",
        "-c",
        "printf user-output; printf user-error >&2",
    ]);
    run.env("CLADDING_TEST_HELPER_NOISE", "1")
        .env("CLADDING_TEST_USER_STDOUT", "user-output")
        .env("CLADDING_TEST_USER_STDERR", "user-error");
    let verbose_run_output = run.output().unwrap();
    assert_success(verbose_run_output.clone());
    let verbose_stdout = String::from_utf8_lossy(&verbose_run_output.stdout);
    let verbose_stderr = String::from_utf8_lossy(&verbose_run_output.stderr);
    assert!(verbose_stdout.contains("starting one-off instance:"));
    assert!(verbose_stdout.contains("helper stdout: pod create"));
    assert!(verbose_stdout.contains("container-id: fake-podman-id"));
    assert!(verbose_stdout.contains("user-output"));
    assert!(verbose_stderr.contains("helper stderr:"));
    assert!(verbose_stderr.contains("cleaning up one-off instance:"));
    assert!(verbose_stderr.contains("user-error"));

    if podman_state.exists() {
        fs::remove_file(&podman_state).unwrap();
    }
    let mut failing_run = cli(
        &root,
        &bin_dir,
        &podman_log,
        &certificate_source,
        &private_key_source,
    );
    failing_run.args([
        "--cladding-dir",
        project_root.to_str().unwrap(),
        "run",
        "true",
    ]);
    failing_run
        .env("CLADDING_TEST_HELPER_NOISE", "1")
        .env("CLADDING_TEST_FAIL_CA_INSTALL", "1");
    let failing_run_output = failing_run.output().unwrap();
    assert!(!failing_run_output.status.success());
    let failure_stderr = String::from_utf8_lossy(&failing_run_output.stderr);
    assert!(
        failure_stderr.contains("Baffle CA installation failed (exit code 42)"),
        "unexpected helper failure output: {failure_stderr}"
    );
    assert!(failure_stderr.contains("stdout:\nhelper stdout: exec --user"));
    assert!(failure_stderr.contains("stderr:\nhelper stderr: exec --user"));
    assert!(!failure_stderr.contains("user-error"));

    if podman_state.exists() {
        fs::remove_file(&podman_state).unwrap();
    }
    let mut cleanup_failure_run = cli(
        &root,
        &bin_dir,
        &podman_log,
        &certificate_source,
        &private_key_source,
    );
    cleanup_failure_run.args([
        "--cladding-dir",
        project_root.to_str().unwrap(),
        "run",
        "sh",
        "-c",
        "printf user-output",
    ]);
    cleanup_failure_run
        .env("CLADDING_TEST_HELPER_NOISE", "1")
        .env("CLADDING_TEST_USER_STDOUT", "user-output")
        .env("CLADDING_TEST_FAIL_CLEANUP", "1");
    let cleanup_failure_output = cleanup_failure_run.output().unwrap();
    assert!(!cleanup_failure_output.status.success());
    assert_eq!(
        String::from_utf8_lossy(&cleanup_failure_output.stdout),
        "user-output"
    );
    let cleanup_failure_stderr = String::from_utf8_lossy(&cleanup_failure_output.stderr);
    assert!(cleanup_failure_stderr.contains("cleanup failed for one-off instance"));
    assert!(cleanup_failure_stderr.contains("podman rm failed (exit code 43)"));
    assert!(cleanup_failure_stderr.contains("stdout:\nhelper stdout: rm -f"));
    assert!(cleanup_failure_stderr.contains("stderr:\nhelper stderr: rm -f"));
    assert_eq!(
        fs::read(project_root.join("credentials/baffle/ca.crt")).unwrap(),
        certificate
    );
    assert_eq!(
        fs::read(project_root.join("credentials/baffle/ca-key.pem")).unwrap(),
        private_key
    );

    fs::remove_dir_all(root).unwrap();
}

fn cli(
    root: &Path,
    bin_dir: &Path,
    podman_log: &Path,
    certificate_source: &Path,
    private_key_source: &Path,
) -> Command {
    let mut command = Command::new(env!("CARGO_BIN_EXE_cladding"));
    let path = format!(
        "{}:{}",
        bin_dir.display(),
        env::var("PATH").unwrap_or_default()
    );
    command
        .current_dir(root)
        .env("PATH", path)
        .env("CLADDING_PODMAN_LOG", podman_log)
        .env("CLADDING_PODMAN_STATE", podman_log.with_extension("state"))
        .env("CLADDING_TEST_CA_CERT", certificate_source)
        .env("CLADDING_TEST_CA_KEY", private_key_source);
    command
}

fn assert_success(output: Output) {
    assert!(
        output.status.success(),
        "command failed\nstdout: {}\nstderr: {}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
}

fn read_log(path: &Path) -> String {
    fs::read_to_string(path).unwrap_or_default()
}

fn is_detached_container_run(line: &str) -> bool {
    let args = line.split_whitespace().collect::<Vec<_>>();
    args.windows(2).any(|tokens| tokens == ["run", "-d"]) && args.contains(&"--name")
}

fn write_test_ca(certificate_path: &Path, private_key_path: &Path) {
    let mut params = CertificateParams::default();
    params
        .distinguished_name
        .push(DnType::CommonName, "Baffle Interception CA");
    params.is_ca = IsCa::Ca(BasicConstraints::Unconstrained);
    params.key_usages = vec![KeyUsagePurpose::KeyCertSign, KeyUsagePurpose::CrlSign];
    params.not_before = OffsetDateTime::now_utc() - Duration::days(1);
    params.not_after = OffsetDateTime::now_utc() + Duration::days(3650);
    let key_pair = KeyPair::generate().unwrap();
    let certificate = params.self_signed(&key_pair).unwrap();
    fs::write(certificate_path, certificate.pem()).unwrap();
    fs::write(private_key_path, key_pair.serialize_pem()).unwrap();
}

fn temp_dir(name: &str) -> PathBuf {
    let path = env::temp_dir().join(format!(
        "cladding-{name}-{}-{}",
        std::process::id(),
        TEMP_COUNTER.fetch_add(1, Ordering::Relaxed)
    ));
    if path.exists() {
        fs::remove_dir_all(&path).unwrap();
    }
    fs::create_dir_all(&path).unwrap();
    path
}
