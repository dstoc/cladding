#![cfg(unix)]

use rcgen::{BasicConstraints, CertificateParams, DnType, IsCa, KeyPair, KeyUsagePurpose};
use std::env;
use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::os::unix::fs::symlink;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};
use std::sync::atomic::{AtomicU64, Ordering};
use time::{Duration, OffsetDateTime};

static TEMP_COUNTER: AtomicU64 = AtomicU64::new(0);

#[test]
fn run_reuses_project_config_tools_credentials_and_persistent_ca() {
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
  pod:rm|run:*|exec:--user|rm:-f)
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
state=$CLADDING_PODMAN_STATE
if [ "$1" = "image" ] && [ "$2" = "exists" ]; then exit 0; fi
if [ "$1" = "info" ]; then
  printf '{"runsc":{"path":"runsc"}}\n'
  exit 0
fi
if [ "$1" = "container" ] && [ "$2" = "ps" ]; then
  printf '['
  separator=
  if [ -f "$state" ]; then
    while IFS='|' read -r name project_name project_root app; do
      [ -n "$name" ] || continue
      printf '%s{"Names":["%s"],"Labels":{"app":"%s","cladding":"%s","project_root":"%s"}}' \
        "$separator" "$name" "$app" "$project_name" "$project_root"
      separator=,
    done < "$state"
  fi
  printf ']\n'
  exit 0
fi
if [ "$1" = "container" ] && [ "$2" = "exists" ]; then
  if [ -f "$state" ] && awk -F '|' -v name="$3" '$1 == name { found = 1 } END { exit !found }' "$state"; then
    exit 0
  fi
  exit 1
fi
if [ "$1" = "pod" ] && [ "$2" = "exists" ]; then
  if [ -f "$CLADDING_LEGACY_PODS" ] && awk -F '|' -v name="$3" '$1 == name { found = 1 } END { exit !found }' "$CLADDING_LEGACY_PODS"; then
    exit 0
  fi
  exit 1
fi
if [ "$1" = "pod" ] && [ "$2" = "inspect" ]; then
  row=$(awk -F '|' -v name="$5" '$1 == name { print; exit }' "$CLADDING_LEGACY_PODS")
  if [ -z "$row" ]; then exit 1; fi
  app=$(printf '%s\n' "$row" | cut -d '|' -f 2)
  project_name=$(printf '%s\n' "$row" | cut -d '|' -f 3)
  project_root=$(printf '%s\n' "$row" | cut -d '|' -f 4)
  if [ "$4" = "{{json .Labels}}" ]; then
    printf '{"app":"%s","cladding":"%s","project_root":"%s"}\n' \
      "$app" "$project_name" "$project_root"
  else
    printf 'Running\n'
  fi
  exit 0
fi
if [ "$1" = "container" ] && [ "$2" = "inspect" ]; then
  row=$(awk -F '|' -v name="$5" '$1 == name { print; exit }' "$state")
  if [ -z "$row" ]; then exit 1; fi
  project_name=$(printf '%s\n' "$row" | cut -d '|' -f 2)
  project_root=$(printf '%s\n' "$row" | cut -d '|' -f 3)
  app=$(printf '%s\n' "$row" | cut -d '|' -f 4)
  if [ "$4" = "{{json .Config.Labels}}" ]; then
    printf '{"app":"%s","cladding":"%s","project_root":"%s"}\n' \
      "$app" "$project_name" "$project_root"
  else
    printf 'running\n'
  fi
  exit 0
fi
if [ "$1" = "rm" ]; then
  name=$3
  if [ -f "$state" ]; then
    awk -F '|' -v name="$name" '$1 != name' "$state" > "$state.tmp"
    mv "$state.tmp" "$state"
  fi
  exit 0
fi
if [ "$1" = "pod" ] && [ "$2" = "rm" ]; then
  name=$4
  awk -F '|' -v name="$name" '$1 != name' "$CLADDING_LEGACY_PODS" > "$CLADDING_LEGACY_PODS.tmp"
  mv "$CLADDING_LEGACY_PODS.tmp" "$CLADDING_LEGACY_PODS"
  awk -F '|' -v name="$name-instance" '$1 != name' "$state" > "$state.tmp"
  mv "$state.tmp" "$state"
  exit 0
fi
if [ "$1" = "run" ] || [ "$1" = "--runtime" ] || [ "$1" = "--runtime-flag" ]; then
  run_seen=
  detached=
  name=
  project_name=
  project_root=
  app=
  next=
  for argument in "$@"; do
    if [ -n "$next" ]; then
      case "$next" in
        name) name=$argument ;;
        label)
          case "$argument" in
            cladding=*) project_name=${argument#cladding=} ;;
            project_root=*) project_root=${argument#project_root=} ;;
            app=*) app=${argument#app=} ;;
          esac
          ;;
      esac
      next=
      continue
    fi
    case "$argument" in
      run) run_seen=yes ;;
      -d) [ -n "$run_seen" ] && detached=yes ;;
      --name) next=name ;;
      --label) next=label ;;
    esac
  done
  if [ -n "$detached" ] && [ -n "$name" ]; then
    credentials_dir=
    config_dir=
    secret_source_dir=
    tools_dir=
    home_dir=
    baffle_binary=
    runtime_script=
    runtime_sockets=
    agent_session_config=agent.toml
    nw_sandbox_session_config=nw-sandbox.toml
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
          *:/opt/tools/bin/baffle:ro)
            baffle_binary=${1%:/opt/tools/bin/baffle:ro}
            ;;
          *:/opt/tools:ro)
            tools_dir=${1%:/opt/tools:ro}
            ;;
          *:/home/user)
            home_dir=${1%:/home/user}
            ;;
          *:/opt/scripts/proxy_startup.sh:ro)
            runtime_script=${1%:/opt/scripts/proxy_startup.sh:ro}
            ;;
          *:/run/cladding/proxy*)
            runtime_sockets=${1%:/run/cladding/proxy*}
            ;;
        esac
      elif [ "$1" = "--env" ]; then
        shift
        case "$1" in
          CLADDING_AGENT_SESSION_CONFIG=*) agent_session_config=${1#CLADDING_AGENT_SESSION_CONFIG=} ;;
          CLADDING_NW_SANDBOX_SESSION_CONFIG=*) nw_sandbox_session_config=${1#CLADDING_NW_SANDBOX_SESSION_CONFIG=} ;;
        esac
      fi
      shift
    done
    if [ -n "$config_dir" ] && [ -f "$config_dir/proxy/sessions/$agent_session_config" ]; then
      printf 'PROJECT_AGENT_SESSION_CONFIG=%s\n' "$agent_session_config" >> "$CLADDING_PODMAN_LOG"
      printf 'PROJECT_NW_SANDBOX_SESSION_CONFIG=%s\n' "$nw_sandbox_session_config" >> "$CLADDING_PODMAN_LOG"
      cat "$config_dir/proxy/sessions/$agent_session_config" >> "$CLADDING_PODMAN_LOG"
    fi
    if [ -n "$secret_source_dir" ]; then
      printf 'PROJECT_BAFFLE_SECRET_SOURCE=%s\n' "$secret_source_dir" >> "$CLADDING_PODMAN_LOG"
    fi
    if [ -n "$credentials_dir" ]; then
      printf 'PROJECT_CREDENTIALS_SOURCE=%s\n' "$credentials_dir" >> "$CLADDING_PODMAN_LOG"
    fi
    if [ -n "$tools_dir" ]; then
      printf 'PROJECT_TOOLS_SOURCE=%s\n' "$tools_dir" >> "$CLADDING_PODMAN_LOG"
    fi
    if [ -n "$home_dir" ]; then
      printf 'PROJECT_HOME_SOURCE=%s\n' "$home_dir" >> "$CLADDING_PODMAN_LOG"
    fi
    if [ -n "$baffle_binary" ]; then
      printf 'PROJECT_BAFFLE_BINARY_SOURCE=%s\n' "$baffle_binary" >> "$CLADDING_PODMAN_LOG"
    fi
    if [ -n "$runtime_script" ]; then
      printf 'RUNTIME_SCRIPT_SOURCE=%s\n' "$runtime_script" >> "$CLADDING_PODMAN_LOG"
      runtime_root=${runtime_script%/runtime/scripts/proxy_startup.sh}
      if [ ! -e "$runtime_root/config" ] && [ ! -e "$runtime_root/tools" ] \
        && [ ! -e "$runtime_root/credentials" ] && [ ! -e "$runtime_root/home" ]; then
        printf '%s\n' 'RUNTIME_HAS_NO_PROJECT_COPIES' >> "$CLADDING_PODMAN_LOG"
      fi
    fi
    if [ -n "$runtime_sockets" ]; then
      printf 'RUNTIME_SOCKETS_SOURCE=%s\n' "$runtime_sockets" >> "$CLADDING_PODMAN_LOG"
    fi
    if [ -n "$credentials_dir" ] && [ -n "$secret_source_dir" ] \
      && [ "$secret_source_dir" != "$credentials_dir/secrets" ]; then
      if [ -n "$(find "$credentials_dir/secrets" -mindepth 1 -print -quit)" ]; then
        echo "one-off runtime copied project secrets into its private credentials" >&2
        exit 1
      fi
      printf '%s\n' 'ONE_OFF_SECRET_SOURCE_IS_SEPARATE_AND_PRIVATE' >> "$CLADDING_PODMAN_LOG"
    fi
    printf '%s|%s|%s|%s\n' "$name" "$project_name" "$project_root" "$app" >> "$state"
    exit 0
  fi
  credentials_dir=
  config_dir=
  tools_dir=
  home_dir=
  baffle_binary=
  runtime_script=
  runtime_sockets=
  agent_session_config=agent.toml
  nw_sandbox_session_config=nw-sandbox.toml
  one_shot=0
  while [ "$#" -gt 0 ]; do
    if [ "$1" = "--rm" ]; then one_shot=1; fi
    if [ "$1" = "--env" ]; then
      shift
      case "$1" in
        CLADDING_AGENT_SESSION_CONFIG=*) agent_session_config=${1#CLADDING_AGENT_SESSION_CONFIG=} ;;
        CLADDING_NW_SANDBOX_SESSION_CONFIG=*) nw_sandbox_session_config=${1#CLADDING_NW_SANDBOX_SESSION_CONFIG=} ;;
      esac
    fi
    if [ "$1" = "--volume" ]; then
      shift
      case "$1" in
        *:/opt/credentials/baffle:ro*)
          credentials_dir=${1%:/opt/credentials/baffle:ro*}
          ;;
        *:/opt/credentials/baffle)
          credentials_dir=${1%:/opt/credentials/baffle}
          ;;
        *:/opt/config:ro*)
          config_dir=${1%:/opt/config:ro*}
          ;;
        *:/opt/tools:ro*)
          tools_dir=${1%:/opt/tools:ro*}
          ;;
        *:/home/user*)
          home_dir=${1%:/home/user*}
          ;;
        *:/opt/tools/bin/baffle:ro*)
          baffle_binary=${1%:/opt/tools/bin/baffle:ro*}
          ;;
        *:/opt/scripts/proxy_startup.sh:ro*)
          runtime_script=${1%:/opt/scripts/proxy_startup.sh:ro*}
          ;;
        *:/run/cladding/proxy*)
          runtime_sockets=${1%:/run/cladding/proxy*}
          ;;
      esac
    fi
    shift
  done
  if [ -n "$config_dir" ] && [ -f "$config_dir/proxy/sessions/$agent_session_config" ]; then
    printf 'PROJECT_AGENT_SESSION_CONFIG=%s\n' "$agent_session_config" >> "$CLADDING_PODMAN_LOG"
    printf 'PROJECT_NW_SANDBOX_SESSION_CONFIG=%s\n' "$nw_sandbox_session_config" >> "$CLADDING_PODMAN_LOG"
    cat "$config_dir/proxy/sessions/$agent_session_config" >> "$CLADDING_PODMAN_LOG"
  fi
  if [ -n "$credentials_dir" ]; then
    printf 'PROJECT_CREDENTIALS_SOURCE=%s\n' "$credentials_dir" >> "$CLADDING_PODMAN_LOG"
  fi
  if [ -n "$tools_dir" ]; then
    printf 'PROJECT_TOOLS_SOURCE=%s\n' "$tools_dir" >> "$CLADDING_PODMAN_LOG"
  fi
  if [ -n "$home_dir" ]; then
    printf 'PROJECT_HOME_SOURCE=%s\n' "$home_dir" >> "$CLADDING_PODMAN_LOG"
  fi
  if [ -n "$baffle_binary" ]; then
    printf 'PROJECT_BAFFLE_BINARY_SOURCE=%s\n' "$baffle_binary" >> "$CLADDING_PODMAN_LOG"
  fi
  if [ -n "$runtime_script" ]; then
    printf 'RUNTIME_SCRIPT_SOURCE=%s\n' "$runtime_script" >> "$CLADDING_PODMAN_LOG"
    runtime_root=${runtime_script%/runtime/scripts/proxy_startup.sh}
    if [ ! -e "$runtime_root/config" ] && [ ! -e "$runtime_root/tools" ] \
      && [ ! -e "$runtime_root/credentials" ] && [ ! -e "$runtime_root/home" ]; then
      printf '%s\n' 'RUNTIME_HAS_NO_PROJECT_COPIES' >> "$CLADDING_PODMAN_LOG"
    fi
  fi
  if [ -n "$runtime_sockets" ]; then
    printf 'RUNTIME_SOCKETS_SOURCE=%s\n' "$runtime_sockets" >> "$CLADDING_PODMAN_LOG"
  fi
  if [ -n "$credentials_dir" ] && [ "$one_shot" = "1" ]; then
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

    fs::remove_dir(project_root.join("tools")).unwrap();
    fs::create_dir_all(root.join("tools")).unwrap();
    symlink("../tools", project_root.join("tools")).unwrap();

    let project_secret_path = project_root.join("credentials/baffle/secrets/test-token");
    fs::write(&project_secret_path, b"project-only secret value").unwrap();
    fs::set_permissions(&project_secret_path, fs::Permissions::from_mode(0o600)).unwrap();

    let agent_session_path = project_root.join("config/proxy/sessions/custom/agent.toml");
    fs::create_dir_all(agent_session_path.parent().unwrap()).unwrap();
    let project_session = format!(
        "{}\n[rules.\"example.com\"]\nports = [443]\npaths = [\"/**\"]\n\n[[rules.\"example.com\".inject]]\nheader = \"Authorization\"\nsecret = \"test-token\"\nformat = \"bearer\"\n",
        fs::read_to_string(project_root.join("config/proxy/sessions/agent.toml")).unwrap()
    );
    fs::write(&agent_session_path, &project_session).unwrap();
    let selected_nw_session = project_root.join("config/proxy/sessions/restricted/network.toml");
    fs::create_dir_all(selected_nw_session.parent().unwrap()).unwrap();
    fs::write(
        &selected_nw_session,
        "version = 2\npersistent = true\nsocket_name = \"nw-sandbox/proxy.sock\"\nunmatched = \"deny\"\n",
    )
    .unwrap();
    let config_path = project_root.join("cladding.json");
    let mut project_config: serde_json::Value =
        serde_json::from_slice(&fs::read(&config_path).unwrap()).unwrap();
    project_config["proxy"]["agent"]["session_config"] = "custom/agent.toml".into();
    project_config["proxy"]["nw_sandbox"]["session_config"] = "restricted/network.toml".into();
    fs::write(
        &config_path,
        serde_json::to_vec_pretty(&project_config).unwrap(),
    )
    .unwrap();

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
    run_before_build.env("CLADDING_TEST_HELPER_NOISE", "1");
    let run_before_build_output = run_before_build.output().unwrap();
    assert!(!run_before_build_output.status.success());
    assert!(String::from_utf8_lossy(&run_before_build_output.stderr).contains("missing tools"));
    assert!(
        !project_root.join("credentials/baffle/ca.crt").exists(),
        "cladding run must not initialize the persistent project CA"
    );
    assert!(
        !project_root.join("credentials/baffle/ca-key.pem").exists(),
        "cladding run must not create the persistent project CA key"
    );

    let ca_init_count_before_build = read_log(&podman_log)
        .lines()
        .filter(|line| line.starts_with("run --rm "))
        .count();
    assert_eq!(
        ca_init_count_before_build, 0,
        "cladding run must not initialize a project CA"
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
    let podman_state = podman_log.with_extension("state");

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
    let mut run_without_ca = cli(
        &root,
        &bin_dir,
        &podman_log,
        &certificate_source,
        &private_key_source,
    );
    run_without_ca.args([
        "--cladding-dir",
        project_root.to_str().unwrap(),
        "run",
        "true",
    ]);
    let run_without_ca_result = run_without_ca.output().unwrap();
    assert!(!run_without_ca_result.status.success());
    let run_without_ca_stderr = String::from_utf8_lossy(&run_without_ca_result.stderr);
    assert!(
        run_without_ca_stderr.contains("Baffle CA is not initialized")
            || run_without_ca_stderr.contains("incomplete Baffle CA"),
        "run failed for an unexpected reason: {run_without_ca_stderr}"
    );
    let run_without_ca_calls = read_log(&podman_log);
    assert!(!run_without_ca_calls.contains("pod create"));
    assert!(!run_without_ca_calls.lines().any(is_detached_container_run));
    assert!(!run_without_ca_calls.contains("run --rm"));
    assert!(!project_root.join("credentials/baffle/ca.crt").exists());
    assert!(!project_root.join("credentials/baffle/ca-key.pem").exists());

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
        "--env",
        "LANG=run-value",
        "--env",
        "RUN_ONLY=command-value",
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
    assert!(verbose_stdout.contains("helper stdout: run -d"));
    assert!(verbose_stdout.contains("container-id: fake-podman-id"));
    assert!(verbose_stdout.contains("user-output"));
    assert!(verbose_stderr.contains("helper stderr:"));
    assert!(verbose_stderr.contains("cleaning up one-off instance:"));
    assert!(verbose_stderr.contains("user-error"));

    let run_log = read_log(&podman_log);
    let command_env_calls = run_log
        .lines()
        .filter(|line| line.contains("LANG=run-value") || line.contains("RUN_ONLY=command-value"))
        .collect::<Vec<_>>();
    assert_eq!(
        command_env_calls.len(),
        1,
        "command environment overrides should reach only the user command: {run_log}"
    );
    assert!(command_env_calls[0].starts_with("exec -i -w "));
    assert!(
        command_env_calls[0]
            .contains("--env LANG=C.UTF-8 --env LANG=run-value --env RUN_ONLY=command-value")
    );
    assert!(run_log.contains("PROJECT_AGENT_SESSION_CONFIG=custom/agent.toml"));
    assert!(run_log.contains("PROJECT_NW_SANDBOX_SESSION_CONFIG=restricted/network.toml"));
    assert!(run_log.contains("[rules.\"example.com\"]"));
    assert!(run_log.contains(&format!(
        "PROJECT_CREDENTIALS_SOURCE={}",
        project_root.join("credentials/baffle").display()
    )));
    assert!(run_log.contains(&format!(
        "{}:/opt/tools:ro",
        project_root.join("tools").display()
    )));
    assert!(run_log.contains(&format!(
        "{}:/home/user",
        project_root.join("home").display()
    )));
    assert!(run_log.contains(&format!(
        "PROJECT_BAFFLE_BINARY_SOURCE={}",
        project_root.join("tools/bin/baffle").display()
    )));
    assert!(run_log.contains("RUNTIME_HAS_NO_PROJECT_COPIES"));
    assert!(
        project_root
            .join("tools")
            .symlink_metadata()
            .unwrap()
            .file_type()
            .is_symlink()
    );
    assert_eq!(
        fs::read(&project_secret_path).unwrap(),
        b"project-only secret value"
    );
    assert_eq!(
        fs::read_to_string(&agent_session_path).unwrap(),
        project_session
    );
    assert_eq!(
        fs::read(project_root.join("credentials/baffle/ca.crt")).unwrap(),
        certificate,
        "cladding run changed the persistent Baffle CA certificate"
    );
    assert_eq!(
        fs::read(project_root.join("credentials/baffle/ca-key.pem")).unwrap(),
        private_key,
        "cladding run changed the persistent Baffle CA key"
    );
    assert!(
        !run_log.lines().any(|line| line.starts_with("run --rm ")),
        "cladding run attempted to initialize another Baffle CA"
    );
    let runtime_script = run_log
        .lines()
        .find_map(|line| line.strip_prefix("RUNTIME_SCRIPT_SOURCE="))
        .expect("one-off proxy should mount its generated startup script");
    let runtime_root = Path::new(runtime_script).ancestors().nth(3).unwrap();
    let runtime_sockets = run_log
        .lines()
        .find_map(|line| line.strip_prefix("RUNTIME_SOCKETS_SOURCE="))
        .expect("one-off proxy should mount its runtime socket directory");
    assert_eq!(
        runtime_sockets,
        runtime_root.join("runtime/sockets/proxy").to_str().unwrap()
    );
    assert!(!runtime_root.starts_with(&project_root));
    assert!(
        !runtime_root.exists(),
        "run should remove its private runtime root"
    );
    assert!(!runtime_root.join("config").exists());
    assert!(!runtime_root.join("tools").exists());
    assert!(!runtime_root.join("home").exists());
    assert!(!runtime_root.join("credentials").exists());

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

    let mut up = cli(
        &root,
        &bin_dir,
        &podman_log,
        &certificate_source,
        &private_key_source,
    );
    up.args(["--cladding-dir", project_root.to_str().unwrap(), "up"]);
    assert_success(up.output().unwrap());
    let startup_log = read_log(&podman_log);
    assert!(
        !startup_log.contains("pod create"),
        "cladding up must not create Podman pods: {startup_log}"
    );
    let proxy_start = startup_log
        .lines()
        .find(|line| line.contains("--name demo-proxy-instance"))
        .expect("up starts the named proxy container");
    assert!(proxy_start.contains("--network default"), "{proxy_start}");
    assert!(proxy_start.contains("--userns keep-id"), "{proxy_start}");
    assert!(proxy_start.contains("--init"), "{proxy_start}");
    assert!(proxy_start.contains("app=proxy"), "{proxy_start}");

    let mut down = cli(
        &root,
        &bin_dir,
        &podman_log,
        &certificate_source,
        &private_key_source,
    );
    down.args([
        "--cladding-dir",
        project_root.to_str().unwrap(),
        "down",
        "-v",
    ]);
    assert_success(down.output().unwrap());
    let cleanup_log = read_log(&podman_log);
    assert!(
        !cleanup_log.lines().any(|line| line.starts_with("pod rm ")),
        "down must not remove pods for the standalone topology: {cleanup_log}"
    );

    let legacy_pods = podman_log.with_extension("legacy-pods");
    let container_state = podman_log.with_extension("state");
    fs::write(
        &legacy_pods,
        format!("demo-proxy|proxy|demo|{}\n", project_root.display()),
    )
    .unwrap();
    fs::write(
        &container_state,
        format!(
            "demo-proxy-instance|demo|{}|proxy\n",
            project_root.display()
        ),
    )
    .unwrap();
    fs::write(&podman_log, "").unwrap();
    let mut down_legacy = cli(
        &root,
        &bin_dir,
        &podman_log,
        &certificate_source,
        &private_key_source,
    );
    down_legacy.args(["--cladding-dir", project_root.to_str().unwrap(), "down"]);
    assert_success(down_legacy.output().unwrap());
    let legacy_cleanup_log = read_log(&podman_log);
    assert_eq!(
        legacy_cleanup_log
            .lines()
            .filter(|line| line.starts_with("pod rm -f demo-proxy"))
            .count(),
        1,
        "down should remove an owned legacy proxy pod: {legacy_cleanup_log}"
    );
    assert!(fs::read_to_string(&legacy_pods).unwrap().is_empty());
    assert!(fs::read_to_string(&container_state).unwrap().is_empty());

    fs::write(
        &legacy_pods,
        format!("demo-agent|unrelated|demo|{}\n", project_root.display()),
    )
    .unwrap();
    fs::write(
        &container_state,
        "demo-agent-instance|unrelated|/tmp/foreign/.cladding|agent\n",
    )
    .unwrap();
    fs::write(&podman_log, "").unwrap();
    let mut down_foreign_pod = cli(
        &root,
        &bin_dir,
        &podman_log,
        &certificate_source,
        &private_key_source,
    );
    down_foreign_pod.args(["--cladding-dir", project_root.to_str().unwrap(), "down"]);
    assert!(!down_foreign_pod.output().unwrap().status.success());
    let foreign_pod_log = read_log(&podman_log);
    assert!(!foreign_pod_log.contains("pod rm -f demo-agent"));
    assert!(!foreign_pod_log.contains("rm -f demo-agent-instance"));
    assert!(!fs::read_to_string(&legacy_pods).unwrap().is_empty());

    fs::write(&legacy_pods, "").unwrap();
    fs::write(
        &container_state,
        "demo-agent-instance|unrelated|/tmp/foreign/.cladding|agent\n",
    )
    .unwrap();
    fs::write(&podman_log, "").unwrap();
    let mut down_foreign_container = cli(
        &root,
        &bin_dir,
        &podman_log,
        &certificate_source,
        &private_key_source,
    );
    down_foreign_container.args(["--cladding-dir", project_root.to_str().unwrap(), "down"]);
    assert!(!down_foreign_container.output().unwrap().status.success());
    assert!(!read_log(&podman_log).contains("rm -f demo-agent-instance"));
    assert_eq!(
        fs::read_to_string(&container_state).unwrap(),
        "demo-agent-instance|unrelated|/tmp/foreign/.cladding|agent\n"
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
        .env(
            "CLADDING_LEGACY_PODS",
            podman_log.with_extension("legacy-pods"),
        )
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
