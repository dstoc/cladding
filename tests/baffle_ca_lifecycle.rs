#![cfg(unix)]

use rcgen::{BasicConstraints, CertificateParams, DnType, IsCa, KeyPair, KeyUsagePurpose};
use std::env;
use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::os::unix::fs::symlink;
use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};
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
    let socket_volume_state = podman_log.with_extension("volumes");
    fs::write(&socket_volume_state, "").unwrap();
    let certificate_source = root.join("test-ca.crt");
    let private_key_source = root.join("test-ca-key.pem");
    write_test_ca(&certificate_source, &private_key_source);
    let podman = bin_dir.join("podman");
    fs::write(
        &podman,
        r#"#!/bin/sh
case "$1:$2" in
  info:*|image:exists)
    if [ -n "${CLADDING_TEST_PODMAN_PREFLIGHT_DELAY:-}" ]; then
      sleep "$CLADDING_TEST_PODMAN_PREFLIGHT_DELAY"
    fi
    ;;
  rm:*|container:rm|volume:rm)
    if [ -n "${CLADDING_TEST_PODMAN_REMOVE_DELAY:-}" ]; then
      sleep "$CLADDING_TEST_PODMAN_REMOVE_DELAY"
    fi
    ;;
esac
if [ -n "${CLADDING_TEST_PODMAN_DELAY:-}" ]; then sleep "$CLADDING_TEST_PODMAN_DELAY"; fi
printf '%s\n' "$*" >> "$CLADDING_PODMAN_LOG"
if [ "$1" = "run" ]; then
  helper_name=
  helper_project=
  helper_root=
  helper_app=
  helper_control_socket=
  helper_expected_roles=
  helper_metadata_version=
  helper_private_identity=
  socket_helper=
  copy_seed_helper=
  next=
  for argument in "$@"; do
    if [ -n "$next" ]; then
      case "$next" in
        name) helper_name=$argument ;;
        label)
          case "$argument" in
            app=*) helper_app=${argument#app=} ;;
            cladding=*) helper_project=${argument#cladding=} ;;
            project_root=*) helper_root=${argument#project_root=} ;;
            cladding_run_control_socket=*) helper_control_socket=${argument#cladding_run_control_socket=} ;;
            cladding_run_expected_roles=*) helper_expected_roles=${argument#cladding_run_expected_roles=} ;;
            cladding_run_metadata_version=*) helper_metadata_version=${argument#cladding_run_metadata_version=} ;;
            cladding_run_private_identity=*) helper_private_identity=${argument#cladding_run_private_identity=} ;;
          esac
          ;;
        volume)
          case "$argument" in
            *:/socket:U|*:/socket/*:U) socket_helper=yes ;;
            *:/source:ro) copy_seed_helper=yes ;;
          esac
          ;;
      esac
      next=
      continue
    fi
    case "$argument" in
      --name) next=name ;;
      --label) next=label ;;
      --volume) next=volume ;;
    esac
  done
  if [ "${CLADDING_TEST_FAIL_SOCKET_VOLUME_PREP:-}" = "1" ] && [ -n "$socket_helper" ]; then
    if [ -n "$helper_name" ]; then
      printf '%s|%s|%s|%s|running|present|%s|%s|%s|%s\n' \
        "$helper_name" "$helper_project" "$helper_root" "$helper_app" \
        "$helper_control_socket" "$helper_expected_roles" "$helper_metadata_version" \
        "$helper_private_identity" \
        >> "$CLADDING_PODMAN_STATE"
    fi
    exit 44
  fi
  if [ "${CLADDING_TEST_FAIL_COPY_SEED:-}" = "1" ] && [ -n "$copy_seed_helper" ]; then
    if [ -n "$helper_name" ]; then
      printf '%s|%s|%s|%s|running|present|%s|%s|%s|%s\n' \
        "$helper_name" "$helper_project" "$helper_root" "$helper_app" \
        "$helper_control_socket" "$helper_expected_roles" "$helper_metadata_version" \
        "$helper_private_identity" \
        >> "$CLADDING_PODMAN_STATE"
    fi
    exit 45
  fi
fi
case "$1:$2" in
  run:*|exec:--user|rm:-f)
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
  if [ "$3" = "/opt/tools/bin/baffle" ] && [ "$4" = "list" ]; then
    if [ -n "${CLADDING_TEST_BAFFLE_LIST_GATE:-}" ]; then
      touch "${CLADDING_TEST_BAFFLE_LIST_GATE}.started"
      while [ ! -e "${CLADDING_TEST_BAFFLE_LIST_GATE}.release" ]; do sleep 0.01; done
    fi
    printf '{"sessions":[]}\n'
    exit 0
  fi
  if [ "$3" = "/opt/tools/bin/baffle" ] && [ "$4" = "create" ]; then
    if [ -n "${CLADDING_TEST_BAFFLE_CREATE_GATE:-}" ]; then
      touch "${CLADDING_TEST_BAFFLE_CREATE_GATE}.started"
      while [ ! -e "${CLADDING_TEST_BAFFLE_CREATE_GATE}.release" ]; do sleep 0.01; done
    fi
    printf '%s\n' '{"id":"mock-session","socket":"/run/cladding/proxy/agent/proxy.sock","persistent":true,"ca_certificate_pem":null,"credential_seed_base64url":"AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA"}'
    exit 0
  fi
  printf '%s' "${CLADDING_TEST_USER_STDOUT:-}"
  printf '%s' "${CLADDING_TEST_USER_STDERR:-}" >&2
  exit 0
fi
if [ "$1" = "rm" ] && [ "$CLADDING_TEST_FAIL_CLEANUP" = "1" ]; then
  exit 43
fi
state=$CLADDING_PODMAN_STATE
volume_state=$CLADDING_PODMAN_VOLUMES
if [ "$1" = "image" ] && [ "$2" = "exists" ]; then exit 0; fi
if [ "$1" = "info" ]; then
  printf '{"runsc":{"path":"runsc"}}\n'
  exit 0
fi
if [ "$1" = "volume" ] && [ "$2" = "create" ]; then
  shift 2
  volume_name=
  cladding_name=
  project_root=
  resource=
  socket_volume=
  while [ "$#" -gt 0 ]; do
    if [ "$1" = "--label" ]; then
      shift
      case "$1" in
        cladding=*) cladding_name=${1#cladding=} ;;
        project_root=*) project_root=${1#project_root=} ;;
        cladding_resource=*) resource=${1#cladding_resource=} ;;
        cladding_socket_volume=*) socket_volume=${1#cladding_socket_volume=} ;;
      esac
    else
      volume_name=$1
    fi
    shift
  done
  printf '%s|%s|%s|%s|%s\n' \
    "$volume_name" "$cladding_name" "$project_root" "$resource" "$socket_volume" \
    >> "$volume_state"
  printf '%s\n' "$volume_name"
  exit 0
fi
if [ "$1" = "volume" ] && [ "$2" = "exists" ]; then
  if [ -f "$volume_state" ] \
    && awk -F '|' -v name="$3" '$1 == name { found = 1 } END { exit !found }' "$volume_state"; then
    exit 0
  fi
  exit 1
fi
if [ "$1" = "volume" ] && [ "$2" = "inspect" ]; then
  row=$(awk -F '|' -v name="$5" '$1 == name { print; exit }' "$volume_state")
  if [ -z "$row" ]; then exit 1; fi
  volume_name=$(printf '%s\n' "$row" | cut -d '|' -f 1)
  cladding_name=$(printf '%s\n' "$row" | cut -d '|' -f 2)
  project_root=$(printf '%s\n' "$row" | cut -d '|' -f 3)
  resource=$(printf '%s\n' "$row" | cut -d '|' -f 4)
  socket_volume=$(printf '%s\n' "$row" | cut -d '|' -f 5)
  printf '{"Name":"%s","Labels":{"cladding":"%s","project_root":"%s","cladding_resource":"%s","cladding_socket_volume":"%s"}}\n' \
    "$volume_name" "$cladding_name" "$project_root" "$resource" "$socket_volume"
  exit 0
fi
if [ "$1" = "volume" ] && [ "$2" = "rm" ]; then
  name=$3
  awk -F '|' -v name="$name" '$1 != name' "$volume_state" > "$volume_state.tmp"
  mv "$volume_state.tmp" "$volume_state"
  exit 0
fi
if [ "$1" = "container" ] && [ "$2" = "ps" ]; then
  if [ "${CLADDING_TEST_FAIL_CONTAINER_PS:-}" = "1" ]; then
    echo 'fake container listing failure' >&2
    exit 42
  fi
  if [ "${CLADDING_TEST_CONTAINER_PS_JSON+x}" = "x" ]; then
    printf '%s\n' "$CLADDING_TEST_CONTAINER_PS_JSON"
    exit 0
  fi
  printf '['
  separator=
  if [ -f "$state" ]; then
    while IFS='|' read -r name project_name project_root app state_status labels_mode; do
      [ -n "$name" ] || continue
      [ -n "$state_status" ] || state_status=running
      if [ "$labels_mode" = "missing" ]; then
        printf '%s{"Names":["%s"],"State":"%s","Labels":{}}' \
          "$separator" "$name" "$state_status"
      else
        printf '%s{"Names":["%s"],"State":"%s","Labels":{"app":"%s","cladding":"%s","project_root":"%s"}}' \
          "$separator" "$name" "$state_status" "$app" "$project_name" "$project_root"
      fi
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
if [ "$1" = "inspect" ]; then
  row=$(awk -F '|' -v name="$2" '$1 == name { print; exit }' "$state")
  if [ -z "$row" ]; then exit 1; fi
  app=$(printf '%s\n' "$row" | cut -d '|' -f 4)
  project_name=$(printf '%s\n' "$row" | cut -d '|' -f 2)
  project_root=$(printf '%s\n' "$row" | cut -d '|' -f 3)
  control_socket=$(printf '%s\n' "$row" | cut -d '|' -f 7)
  expected_roles=$(printf '%s\n' "$row" | cut -d '|' -f 8)
  metadata_version=$(printf '%s\n' "$row" | cut -d '|' -f 9)
  private_identity=$(printf '%s\n' "$row" | cut -d '|' -f 10)
  cwd_mappings=$(printf '%s\n' "$row" | cut -d '|' -f 11-)
  python3 - "$app" "$project_name" "$project_root" "$control_socket" \
    "$expected_roles" "$metadata_version" "$private_identity" "$cwd_mappings" <<'PY'
import json
import sys

app, project_name, project_root, socket, roles, version, identity, raw = sys.argv[1:]
mappings = json.loads(raw)
mounts = []
for mapping in mappings:
    copied = mapping["mount_type"] == "copy"
    mounts.append({
        "Destination": mapping["destination"],
        "Type": "volume" if copied else "bind",
        "Source": "/mock/copy-volume" if copied else mapping["host_source"],
        "Name": mapping.get("volume_name"),
        "RW": not mapping["read_only"],
    })
labels = {
    "app": app,
    "cladding": project_name,
    "project_root": project_root,
    "cladding_run_control_socket": socket,
    "cladding_run_expected_roles": roles,
    "cladding_run_metadata_version": version,
    "cladding_run_private_identity": identity,
}
print(json.dumps([{"Id": "fake-" + app + "-id", "Config": {"Labels": labels}, "Mounts": mounts}]))
PY
  exit 0
fi
if [ "$1" = "container" ] && [ "$2" = "inspect" ]; then
  if [ "${CLADDING_TEST_FAIL_CONTAINER_INSPECT:-}" = "1" ]; then
    echo 'fake container inspection failure' >&2
    exit 42
  fi
  row=$(awk -F '|' -v name="$5" '$1 == name { print; exit }' "$state")
  if [ -z "$row" ]; then exit 1; fi
  project_name=$(printf '%s\n' "$row" | cut -d '|' -f 2)
  project_root=$(printf '%s\n' "$row" | cut -d '|' -f 3)
  app=$(printf '%s\n' "$row" | cut -d '|' -f 4)
  control_socket=$(printf '%s\n' "$row" | cut -d '|' -f 7)
  expected_roles=$(printf '%s\n' "$row" | cut -d '|' -f 8)
  metadata_version=$(printf '%s\n' "$row" | cut -d '|' -f 9)
  private_identity=$(printf '%s\n' "$row" | cut -d '|' -f 10)
  printf '{"State":{"Status":"running"},"Config":{"Labels":{"app":"%s","cladding":"%s","project_root":"%s","cladding_run_control_socket":"%s","cladding_run_expected_roles":"%s","cladding_run_metadata_version":"%s","cladding_run_private_identity":"%s"}}}\n' \
    "$app" "$project_name" "$project_root" "$control_socket" "$expected_roles" "$metadata_version" "$private_identity"
  exit 0
fi
if [ "$1" = "rm" ]; then
  name=$3
  if [ -f "$state" ] \
    && awk -F '|' -v name="$name" '$1 == name { found = 1 } END { exit !found }' "$state"; then
    awk -F '|' -v name="$name" '$1 != name' "$state" > "$state.tmp"
    mv "$state.tmp" "$state"
    exit 0
  fi
  echo "Error: no such container: $name" >&2
  exit 1
fi
if [ "$1" = "run" ] || [ "$1" = "--runtime" ] || [ "$1" = "--runtime-flag" ]; then
  run_seen=
  detached=
  name=
  project_name=
  project_root=
  app=
  control_socket=
  expected_roles=
  metadata_version=
  private_identity=
  cwd_mappings='[]'
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
            cladding_run_control_socket=*) control_socket=${argument#cladding_run_control_socket=} ;;
            cladding_run_expected_roles=*) expected_roles=${argument#cladding_run_expected_roles=} ;;
            cladding_run_metadata_version=*) metadata_version=${argument#cladding_run_metadata_version=} ;;
            cladding_run_private_identity=*) private_identity=${argument#cladding_run_private_identity=} ;;
            cladding_run_cwd_v1=*) cwd_mappings=${argument#cladding_run_cwd_v1=} ;;
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
    if [ -n "$credentials_dir" ] && [ -n "$secret_source_dir" ] \
      && [ "$secret_source_dir" != "$credentials_dir/secrets" ]; then
      if [ -n "$(find "$credentials_dir/secrets" -mindepth 1 -print -quit)" ]; then
        echo "one-off runtime copied project secrets into its private credentials" >&2
        exit 1
      fi
      printf '%s\n' 'ONE_OFF_SECRET_SOURCE_IS_SEPARATE_AND_PRIVATE' >> "$CLADDING_PODMAN_LOG"
    fi
    printf '%s|%s|%s|%s|running|present|%s|%s|%s|%s|%s\n' \
      "$name" "$project_name" "$project_root" "$app" "$control_socket" \
      "$expected_roles" "$metadata_version" "$private_identity" "$cwd_mappings" >> "$state"
    exit 0
  fi
  credentials_dir=
  config_dir=
  tools_dir=
  home_dir=
  baffle_binary=
  runtime_script=
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
    let run_mount_source = root.join("run-mount-source");
    fs::create_dir_all(&run_mount_source).unwrap();
    let mut project_config: serde_json::Value =
        serde_json::from_slice(&fs::read(&config_path).unwrap()).unwrap();
    project_config["proxy"]["agent"]["session_config"] = "custom/agent.toml".into();
    project_config["proxy"]["nw_sandbox"]["session_config"] = "restricted/network.toml".into();
    project_config["mounts"] = serde_json::json!([
        {
            "mount": "/home/user/.cache/kache",
            "volume": "cladding-kache",
            "targets": ["agent"]
        },
        {
            "mount": "/empty",
            "targets": ["agent"]
        },
        {
            "mount": "/runtime-mount",
            "hostPath": run_mount_source.to_str().unwrap(),
            "targets": ["agent"]
        }
    ]);
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
    assert!(!run_without_ca_stderr.contains("runtime metrics:"));
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

    fs::write(project_root.join("credentials/baffle/ca.crt"), &certificate).unwrap();
    fs::write(
        project_root.join("credentials/baffle/ca-key.pem"),
        &private_key,
    )
    .unwrap();
    fs::write(&socket_volume_state, "cladding-kache||||\n").unwrap();
    fs::write(&podman_log, "").unwrap();
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
        .env("CLADDING_TEST_USER_STDERR", "user-error")
        .env("CLADDING_TEST_PODMAN_DELAY", "0.01")
        .env("CLADDING_TEST_PODMAN_PREFLIGHT_DELAY", "0.04");
    let verbose_run_output = run.output().unwrap();
    assert_success(verbose_run_output.clone());
    let verbose_stdout = String::from_utf8_lossy(&verbose_run_output.stdout);
    let verbose_stderr = String::from_utf8_lossy(&verbose_run_output.stderr);
    assert!(verbose_stderr.contains("starting one-off instance:"));
    assert!(verbose_stdout.contains("helper stdout: run -d"));
    assert!(verbose_stdout.contains("container-id: fake-podman-id"));
    assert!(verbose_stdout.contains("user-output"));
    assert!(!verbose_stdout.contains("credential_seed_base64url"));
    assert!(!verbose_stdout.contains("AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA"));
    assert!(verbose_stderr.contains("helper stderr:"));
    assert!(verbose_stderr.contains("cleaning up one-off instance:"));
    assert!(verbose_stderr.contains("user-error"));
    let run_calls = read_log(&podman_log);
    assert!(run_calls.contains("/opt/tools/bin/baffle create custom/agent.toml"));
    assert!(run_calls.contains("/opt/tools/bin/baffle list"));
    assert!(
        !run_calls.contains("volume ls"),
        "one-off cleanup must use its prepared resource record without a stale-volume scan: {run_calls}"
    );
    let metrics_line = verbose_stderr
        .lines()
        .find(|line| line.starts_with("runtime metrics: run "))
        .expect("verbose run prints runtime metrics");
    for phase in ["prep=", "startup=", "command=", "cleanup="] {
        assert!(
            metrics_line.contains(phase),
            "missing {phase}: {metrics_line}"
        );
    }
    for operation in [
        "discovery/inspect=",
        "create/start/exec=",
        "removal=",
        "helpers/other=",
    ] {
        assert!(
            metrics_line.contains(operation),
            "missing {operation}: {metrics_line}"
        );
    }
    assert!(
        metrics_line.contains("preparation runtime-specs=1 custom-mount-paths-prepared=1"),
        "one-off run should prepare its runtime spec once: {metrics_line}"
    );
    for operation in ["discovery/inspect=", "removal="] {
        let count = metrics_line
            .split(operation)
            .nth(1)
            .unwrap()
            .split('/')
            .next()
            .unwrap()
            .parse::<usize>()
            .unwrap();
        assert!(
            count > 0,
            "expected measured {operation} calls: {metrics_line}"
        );
    }
    let create_exec = metrics_line
        .split("create/start/exec=")
        .nth(1)
        .expect("summary includes Podman create/start/exec operations");
    let (operation_count, operation_elapsed) = create_exec.split_once('/').unwrap();
    assert!(
        operation_count.parse::<usize>().unwrap() > 0,
        "{metrics_line}"
    );
    let elapsed = operation_elapsed
        .split_whitespace()
        .next()
        .unwrap()
        .strip_suffix('s')
        .unwrap()
        .parse::<f64>()
        .unwrap();
    assert!(
        elapsed >= 0.01,
        "fake Podman delay was not measured: {metrics_line}"
    );
    let preparation_elapsed = metric_duration(metrics_line, "prep");
    let startup_elapsed = metric_duration(metrics_line, "startup");
    assert!(
        preparation_elapsed > startup_elapsed,
        "delayed preflight work should be counted in prep, not startup: {metrics_line}"
    );

    let run_log = read_log(&podman_log);
    assert!(
        run_log
            .lines()
            .any(|line| { line.contains("--volume cladding-kache:/home/user/.cache/kache") }),
        "one-off run should mount the explicitly named Podman volume verbatim: {run_log}"
    );
    assert!(
        run_log.lines().any(|line| {
            line.contains(
                "--mount type=tmpfs,dst=/empty,tmpfs-mode=1777,U=true,notmpcopyup,ro=true",
            )
        }),
        "one-off run should mount a typeless mount as an empty read-only tmpfs: {run_log}"
    );
    assert!(
        !run_log.lines().any(|line| line.contains("-empty-empty")),
        "one-off run must not create or mount an implicit named volume: {run_log}"
    );
    assert!(
        !run_log
            .lines()
            .any(|line| line == "volume rm cladding-kache"),
        "one-off cleanup must not remove an explicitly named volume: {run_log}"
    );
    assert_eq!(
        fs::read_to_string(&socket_volume_state).unwrap(),
        "cladding-kache||||\n",
        "one-off cleanup must preserve the external volume"
    );
    fs::write(&socket_volume_state, "").unwrap();
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
        !run_log.lines().any(|line| {
            line.contains("--entrypoint /opt/tools/bin/baffle") && line.contains("ca init")
        }),
        "cladding run attempted to initialize another Baffle CA"
    );
    let runtime_script = run_log
        .lines()
        .find_map(|line| line.strip_prefix("RUNTIME_SCRIPT_SOURCE="))
        .expect("one-off proxy should mount its generated startup script");
    let runtime_root = Path::new(runtime_script).ancestors().nth(3).unwrap();
    assert!(
        !run_log.contains("runtime/sockets/proxy"),
        "one-off run should not create or mount a host proxy socket directory: {run_log}"
    );
    assert_eq!(
        runtime_root.parent().unwrap(),
        project_root.join("runtime/runs")
    );
    assert!(
        !runtime_root.exists(),
        "run should remove its private runtime root"
    );
    assert!(!runtime_root.join("config").exists());
    assert!(!runtime_root.join("tools").exists());
    assert!(!runtime_root.join("home").exists());
    assert!(!runtime_root.join("credentials").exists());
    assert!(
        !project_root.join("home/workspace").exists(),
        "cladding run must not create the former default workspace mount point"
    );

    if podman_state.exists() {
        fs::remove_file(&podman_state).unwrap();
    }
    fs::write(&podman_log, "").unwrap();
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
    let failed_start_log = read_log(&podman_log);
    let removed_run_containers: Vec<_> = failed_start_log
        .lines()
        .filter_map(|line| line.strip_prefix("rm -f "))
        .filter(|name| name.starts_with("cladding-run-") && name.ends_with("-instance"))
        .collect();
    assert_eq!(
        removed_run_containers.len(),
        3,
        "failed one-off startup must clean each component exactly once, including components that never started: {failed_start_log}"
    );
    for component in ["proxy", "agent", "nw-sandbox"] {
        assert_eq!(
            removed_run_containers
                .iter()
                .filter(|name| name.ends_with(&format!("-{component}-instance")))
                .count(),
            1,
            "failed one-off startup must remove the {component} component once: {failed_start_log}"
        );
    }
    assert_eq!(
        fs::read_to_string(&socket_volume_state).unwrap(),
        "",
        "failed startup must remove its owned socket volumes"
    );

    let mut verbose_startup_failure = cli(
        &root,
        &bin_dir,
        &podman_log,
        &certificate_source,
        &private_key_source,
    );
    verbose_startup_failure.args([
        "--cladding-dir",
        project_root.to_str().unwrap(),
        "run",
        "-v",
        "true",
    ]);
    verbose_startup_failure.env("CLADDING_TEST_FAIL_CA_INSTALL", "1");
    let verbose_startup_failure_output = verbose_startup_failure.output().unwrap();
    assert!(!verbose_startup_failure_output.status.success());
    let verbose_startup_failure_stderr =
        String::from_utf8_lossy(&verbose_startup_failure_output.stderr);
    let startup_metrics = verbose_startup_failure_stderr
        .lines()
        .find(|line| line.starts_with("runtime metrics: run "))
        .expect("failed startup prints runtime metrics");
    assert!(startup_metrics.contains("startup="));
    assert!(startup_metrics.contains("cleanup="));

    let mut copy_failure_config = project_config.clone();
    copy_failure_config["mounts"]
        .as_array_mut()
        .unwrap()
        .push(serde_json::json!({
            "mount": "/run-copy",
            "hostPath": run_mount_source.to_str().unwrap(),
            "type": "copy",
            "targets": ["agent"]
        }));
    fs::write(
        &config_path,
        serde_json::to_vec_pretty(&copy_failure_config).unwrap(),
    )
    .unwrap();
    if podman_state.exists() {
        fs::remove_file(&podman_state).unwrap();
    }
    fs::write(&podman_log, "").unwrap();
    let mut copy_seed_failure = cli(
        &root,
        &bin_dir,
        &podman_log,
        &certificate_source,
        &private_key_source,
    );
    copy_seed_failure
        .args([
            "--cladding-dir",
            project_root.to_str().unwrap(),
            "run",
            "-v",
            "true",
        ])
        .env("CLADDING_TEST_FAIL_COPY_SEED", "1");
    let copy_seed_failure_output = copy_seed_failure.output().unwrap();
    assert!(!copy_seed_failure_output.status.success());
    let copy_seed_failure_stderr = String::from_utf8_lossy(&copy_seed_failure_output.stderr);
    assert!(
        copy_seed_failure_stderr.contains("copy mount volume seeding"),
        "unexpected managed copy seed failure: {copy_seed_failure_stderr}"
    );
    let copy_seed_failure_log = read_log(&podman_log);
    let copy_volume = copy_seed_failure_log
        .lines()
        .find(|line| line.starts_with("volume create ") && line.contains("managed-mount"))
        .and_then(|line| line.split_whitespace().last())
        .expect("failed copy startup created a managed volume");
    let copy_seed_helper = format!("{copy_volume}-seed");
    assert!(
        copy_seed_failure_log.contains(&format!("--name {copy_seed_helper}")),
        "seed helper identity must be present in its create command: {copy_seed_failure_log}"
    );
    assert_eq!(
        copy_seed_failure_log
            .lines()
            .filter(|line| line.trim() == format!("rm -f {copy_seed_helper}"))
            .count(),
        1,
        "failed copy seed helper must be retired once: {copy_seed_failure_log}"
    );
    assert!(
        copy_seed_failure_log.contains(&format!("volume rm {copy_volume}")),
        "failed copy startup must remove the volume it attempted to create: {copy_seed_failure_log}"
    );
    assert!(
        !copy_seed_failure_log.contains("volume ls"),
        "complete one-off attempt records should avoid stale volume discovery: {copy_seed_failure_log}"
    );
    let copy_instance = String::from_utf8_lossy(&copy_seed_failure_output.stderr)
        .lines()
        .find_map(|line| line.strip_prefix("starting one-off instance: "))
        .expect("verbose copy run reports its instance name")
        .to_string();
    assert!(
        !project_root
            .join("runtime/runs")
            .join(copy_instance)
            .exists(),
        "failed copy startup should remove its private runtime root"
    );
    for component in ["proxy", "agent", "nw-sandbox"] {
        assert_eq!(
            copy_seed_failure_log
                .lines()
                .filter(|line| {
                    line.strip_prefix("rm -f ").is_some_and(|name| {
                        name.starts_with("cladding-run-")
                            && name.ends_with(&format!("-{component}-instance"))
                    })
                })
                .count(),
            1,
            "failed copy startup must clean the {component} component once, even though startup did not reach it: {copy_seed_failure_log}"
        );
    }
    fs::write(
        &config_path,
        serde_json::to_vec_pretty(&project_config).unwrap(),
    )
    .unwrap();

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
    assert!(
        cleanup_failure_stderr
            .matches("podman rm failed (exit code 43)")
            .count()
            >= 3,
        "cleanup should aggregate failures across component removals: {cleanup_failure_stderr}"
    );
    assert!(!cleanup_failure_stderr.contains("runtime metrics:"));
    let cleanup_failure_log = read_log(&podman_log);
    let cleanup_failure_script = cleanup_failure_log
        .lines()
        .find_map(|line| line.strip_prefix("RUNTIME_SCRIPT_SOURCE="))
        .expect("failed cleanup ran a component with the private startup script");
    let retained_runtime_root = Path::new(cleanup_failure_script)
        .ancestors()
        .nth(3)
        .unwrap();
    assert!(
        retained_runtime_root.exists(),
        "private runtime root must remain while container removal fails"
    );

    if podman_state.exists() {
        fs::remove_file(&podman_state).unwrap();
    }
    let mut verbose_cleanup_failure = cli(
        &root,
        &bin_dir,
        &podman_log,
        &certificate_source,
        &private_key_source,
    );
    verbose_cleanup_failure.args([
        "--cladding-dir",
        project_root.to_str().unwrap(),
        "run",
        "-v",
        "sh",
        "-c",
        "printf user-output",
    ]);
    verbose_cleanup_failure
        .env("CLADDING_TEST_USER_STDOUT", "user-output")
        .env("CLADDING_TEST_FAIL_CLEANUP", "1");
    let verbose_cleanup_failure_output = verbose_cleanup_failure.output().unwrap();
    assert!(!verbose_cleanup_failure_output.status.success());
    let verbose_cleanup_failure_stderr =
        String::from_utf8_lossy(&verbose_cleanup_failure_output.stderr);
    let cleanup_metrics = verbose_cleanup_failure_stderr
        .lines()
        .find(|line| line.starts_with("runtime metrics: run "))
        .expect("failed cleanup prints runtime metrics");
    assert!(cleanup_metrics.contains("cleanup="));
    if podman_state.exists() {
        fs::remove_file(&podman_state).unwrap();
    }
    assert_eq!(
        fs::read(project_root.join("credentials/baffle/ca.crt")).unwrap(),
        certificate
    );
    assert_eq!(
        fs::read(project_root.join("credentials/baffle/ca-key.pem")).unwrap(),
        private_key
    );

    fs::write(&socket_volume_state, "cladding-kache||||\n").unwrap();
    fs::write(&podman_log, "").unwrap();
    let mut failed_socket_run = cli(
        &root,
        &bin_dir,
        &podman_log,
        &certificate_source,
        &private_key_source,
    );
    failed_socket_run
        .args([
            "--cladding-dir",
            project_root.to_str().unwrap(),
            "run",
            "-v",
            "true",
        ])
        .env("CLADDING_TEST_FAIL_SOCKET_VOLUME_PREP", "1");
    let failed_socket_run_output = failed_socket_run.output().unwrap();
    assert!(!failed_socket_run_output.status.success());
    let failed_socket_instance = String::from_utf8_lossy(&failed_socket_run_output.stderr)
        .lines()
        .find_map(|line| line.strip_prefix("starting one-off instance: "))
        .expect("verbose failed run reports its instance name")
        .to_string();
    let failed_socket_helper = format!("{failed_socket_instance}-baffle-socket-volume-permissions");
    let failed_socket_run_log = read_log(&podman_log);
    assert!(failed_socket_run_log.contains(&format!("--name {failed_socket_helper}")));
    assert_eq!(
        failed_socket_run_log
            .lines()
            .filter(|line| line.trim() == format!("rm -f {failed_socket_helper}"))
            .count(),
        1,
        "one-off cleanup must consume the recorded socket helper identity: {failed_socket_run_log}"
    );
    assert!(
        !failed_socket_run_log.contains("volume ls"),
        "complete one-off attempt records should avoid stale volume discovery: {failed_socket_run_log}"
    );
    for component in ["proxy", "agent", "nw-sandbox"] {
        assert_eq!(
            failed_socket_run_log
                .lines()
                .filter(|line| {
                    line.strip_prefix("rm -f ").is_some_and(|name| {
                        name.starts_with("cladding-run-")
                            && name.ends_with(&format!("-{component}-instance"))
                    })
                })
                .count(),
            1,
            "failed permission setup must clean the {component} component once: {failed_socket_run_log}"
        );
    }
    assert!(
        !project_root
            .join("runtime/runs")
            .join(failed_socket_instance)
            .exists(),
        "failed socket preparation should remove its private runtime root"
    );
    assert_eq!(
        fs::read_to_string(&socket_volume_state).unwrap(),
        "cladding-kache||||\n",
        "failed one-off socket preparation should preserve external volumes"
    );

    fs::write(&podman_log, "").unwrap();
    fs::write(&socket_volume_state, "").unwrap();
    let parallel_logs = [root.join("parallel-a.log"), root.join("parallel-b.log")];
    let mut parallel_a = cli(
        &root,
        &bin_dir,
        &parallel_logs[0],
        &certificate_source,
        &private_key_source,
    );
    parallel_a
        .args([
            "--cladding-dir",
            project_root.to_str().unwrap(),
            "run",
            "-v",
            "true",
        ])
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .env("CLADDING_TEST_PODMAN_DELAY", "0.01");
    let mut parallel_b = cli(
        &root,
        &bin_dir,
        &parallel_logs[1],
        &certificate_source,
        &private_key_source,
    );
    parallel_b
        .args([
            "--cladding-dir",
            project_root.to_str().unwrap(),
            "run",
            "-v",
            "true",
        ])
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .env("CLADDING_TEST_PODMAN_DELAY", "0.01");
    let parallel_a = parallel_a.spawn().unwrap();
    let parallel_b = parallel_b.spawn().unwrap();
    let parallel_a_output = parallel_a.wait_with_output().unwrap();
    let parallel_b_output = parallel_b.wait_with_output().unwrap();
    assert_success(parallel_a_output.clone());
    assert_success(parallel_b_output.clone());
    let parallel_instances: Vec<_> = parallel_logs
        .iter()
        .map(|log_path| {
            let log = read_log(log_path);
            let proxy_start = log
                .lines()
                .find(|line| line.contains("run -d") && line.contains("--name "))
                .expect("parallel run starts its proxy container");
            proxy_start
                .split("--name ")
                .nth(1)
                .and_then(|name| name.split_whitespace().next())
                .and_then(|name| name.strip_suffix("-proxy-instance"))
                .expect("parallel proxy name includes the run identity")
                .to_string()
        })
        .collect();
    assert_ne!(parallel_instances[0], parallel_instances[1]);
    for instance in parallel_instances {
        assert!(
            !project_root.join("runtime/runs").join(instance).exists(),
            "successful concurrent run should remove its private runtime root"
        );
    }

    fs::write(&podman_log, "").unwrap();
    fs::write(&socket_volume_state, "").unwrap();
    let readiness_gate = root.join("baffle-readiness-gate");
    let mut cancelled_run = cli(
        &root,
        &bin_dir,
        &podman_log,
        &certificate_source,
        &private_key_source,
    );
    cancelled_run
        .args([
            "--cladding-dir",
            project_root.to_str().unwrap(),
            "run",
            "--",
            "cladding-cancelled-workload-token",
        ])
        .env("CLADDING_TEST_BAFFLE_LIST_GATE", &readiness_gate)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    let cancelled_run = cancelled_run.spawn().unwrap();
    let mut cancelled_run = Some(cancelled_run);
    let gate_started = PathBuf::from(format!("{}.started", readiness_gate.display()));
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
    while !gate_started.exists() && std::time::Instant::now() < deadline {
        std::thread::sleep(std::time::Duration::from_millis(10));
    }
    if !gate_started.exists() {
        let mut child = cancelled_run.take().unwrap();
        let _ = child.kill();
        let output = child.wait_with_output().unwrap();
        panic!(
            "run never reached the gated Baffle readiness check\nstdout: {}\nstderr: {}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
    }
    let child = cancelled_run.take().unwrap();
    // SAFETY: the child is the Cladding process spawned by this test.
    let signal_result = unsafe { libc::kill(child.id() as libc::pid_t, libc::SIGTERM) };
    assert_eq!(signal_result, 0, "send SIGTERM to the one-off run");
    let output = child.wait_with_output().unwrap();
    assert_eq!(output.status.code(), Some(128 + libc::SIGTERM));
    let cancellation_log = read_log(&podman_log);
    assert!(cancellation_log.contains("/opt/tools/bin/baffle list"));
    assert!(!cancellation_log.contains("/opt/tools/bin/baffle create"));
    assert!(
        !cancellation_log.contains("cladding-cancelled-workload-token"),
        "cancellation during readiness must prevent workload launch"
    );
    let cancelled_proxy_start = cancellation_log
        .lines()
        .find(|line| is_detached_container_run(line))
        .expect("cancelled run starts a proxy container");
    let cancelled_instance = cancelled_proxy_start
        .split("--name ")
        .nth(1)
        .and_then(|name| name.split_whitespace().next())
        .and_then(|name| name.strip_suffix("-proxy-instance"))
        .expect("cancelled proxy name includes the run identity");
    assert!(
        !project_root
            .join("runtime/runs")
            .join(cancelled_instance)
            .exists(),
        "cancellation during readiness must remove the one-off runtime root"
    );
    assert!(
        cancellation_log.contains("rm -f") && cancellation_log.contains("volume rm"),
        "cancellation during readiness must remove runtime containers and volumes"
    );

    fs::write(&podman_log, "").unwrap();
    fs::write(&socket_volume_state, "").unwrap();
    let session_gate = root.join("baffle-session-create-gate");
    let mut cancelled_session = cli(
        &root,
        &bin_dir,
        &podman_log,
        &certificate_source,
        &private_key_source,
    );
    cancelled_session
        .args([
            "--cladding-dir",
            project_root.to_str().unwrap(),
            "run",
            "--",
            "cladding-cancelled-session-workload-token",
        ])
        .env("CLADDING_TEST_BAFFLE_CREATE_GATE", &session_gate)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    let cancelled_session = cancelled_session.spawn().unwrap();
    let mut cancelled_session = Some(cancelled_session);
    let session_gate_started = PathBuf::from(format!("{}.started", session_gate.display()));
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
    while !session_gate_started.exists() && std::time::Instant::now() < deadline {
        std::thread::sleep(std::time::Duration::from_millis(10));
    }
    if !session_gate_started.exists() {
        let mut child = cancelled_session.take().unwrap();
        let _ = child.kill();
        let output = child.wait_with_output().unwrap();
        panic!(
            "run never reached the gated Baffle session creation\nstdout: {}\nstderr: {}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
    }
    let child = cancelled_session.take().unwrap();
    // SAFETY: the child is the Cladding process spawned by this test.
    let signal_result = unsafe { libc::kill(child.id() as libc::pid_t, libc::SIGTERM) };
    assert_eq!(signal_result, 0, "send SIGTERM during session creation");
    let output = child.wait_with_output().unwrap();
    assert_eq!(output.status.code(), Some(128 + libc::SIGTERM));
    let cancellation_log = read_log(&podman_log);
    assert!(cancellation_log.contains("/opt/tools/bin/baffle list"));
    assert!(cancellation_log.contains("/opt/tools/bin/baffle create"));
    assert!(
        !cancellation_log.contains("cladding-cancelled-session-workload-token"),
        "cancellation during session creation must prevent workload launch"
    );
    let cancelled_proxy_start = cancellation_log
        .lines()
        .find(|line| is_detached_container_run(line))
        .expect("cancelled session creation starts a proxy container");
    let cancelled_instance = cancelled_proxy_start
        .split("--name ")
        .nth(1)
        .and_then(|name| name.split_whitespace().next())
        .and_then(|name| name.strip_suffix("-proxy-instance"))
        .expect("cancelled proxy name includes the run identity");
    assert!(
        !project_root
            .join("runtime/runs")
            .join(cancelled_instance)
            .exists(),
        "cancellation during session creation must remove the one-off runtime root"
    );
    assert!(
        cancellation_log.contains("rm -f") && cancellation_log.contains("volume rm"),
        "cancellation during session creation must remove runtime containers and volumes"
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
    let (bridge, gantry_dir, baffle) = write_development_tool_overrides(root);
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
            "CLADDING_PODMAN_VOLUMES",
            podman_log.with_extension("volumes"),
        )
        .env("CLADDING_TEST_CA_CERT", certificate_source)
        .env("CLADDING_TEST_CA_KEY", private_key_source)
        .env("CLADDING_BRIDGE_BIN", bridge)
        .env("CLADDING_GANTRY_DIR", gantry_dir)
        .env("CLADDING_BAFFLE_BIN", baffle);
    command
}

fn write_development_tool_overrides(root: &Path) -> (PathBuf, PathBuf, PathBuf) {
    let arch = env!("CLADDING_LINUX_ARCHITECTURE");
    let (machine, loader) = match arch {
        "x86_64-unknown-linux-gnu" => (62u16, "/lib64/ld-linux-x86-64.so.2"),
        "aarch64-unknown-linux-gnu" => (183u16, "/lib/ld-linux-aarch64.so.1"),
        other => panic!("unsupported development fixture architecture {other}"),
    };
    let override_dir = root.join("development-tool-overrides");
    let gantry_dir = override_dir.join("gantry");
    fs::create_dir_all(&gantry_dir).unwrap();
    let bridge = override_dir.join("cladding-bridge");
    let baffle = override_dir.join("baffle");
    fs::write(&bridge, basic_elf(machine)).unwrap();
    fs::write(&baffle, gnu_elf(machine, loader, None)).unwrap();
    fs::write(
        gantry_dir.join("gantry"),
        gnu_elf(machine, loader, Some("GLIBC_2.34")),
    )
    .unwrap();
    fs::write(
        gantry_dir.join("run-remote"),
        gnu_elf(machine, loader, Some("GLIBC_2.34")),
    )
    .unwrap();
    for path in [
        &bridge,
        &baffle,
        &gantry_dir.join("gantry"),
        &gantry_dir.join("run-remote"),
    ] {
        fs::set_permissions(path, fs::Permissions::from_mode(0o755)).unwrap();
    }
    (bridge, gantry_dir, baffle)
}

fn basic_elf(machine: u16) -> Vec<u8> {
    let mut bytes = vec![0; 64];
    bytes[..4].copy_from_slice(b"\x7fELF");
    bytes[4] = 2;
    bytes[5] = 1;
    bytes[6] = 1;
    bytes[7] = 0;
    bytes[16..18].copy_from_slice(&3u16.to_le_bytes());
    bytes[18..20].copy_from_slice(&machine.to_le_bytes());
    bytes[20..24].copy_from_slice(&1u32.to_le_bytes());
    bytes
}

fn gnu_elf(machine: u16, loader: &str, glibc: Option<&str>) -> Vec<u8> {
    let mut bytes = vec![0; 320];
    bytes[..4].copy_from_slice(b"\x7fELF");
    bytes[4] = 2;
    bytes[5] = 1;
    bytes[6] = 1;
    bytes[7] = 0;
    bytes[16..18].copy_from_slice(&3u16.to_le_bytes());
    bytes[18..20].copy_from_slice(&machine.to_le_bytes());
    bytes[20..24].copy_from_slice(&1u32.to_le_bytes());
    bytes[32..40].copy_from_slice(&64u64.to_le_bytes());
    bytes[54..56].copy_from_slice(&56u16.to_le_bytes());
    bytes[56..58].copy_from_slice(&1u16.to_le_bytes());
    bytes[64..68].copy_from_slice(&3u32.to_le_bytes());
    bytes[72..80].copy_from_slice(&160u64.to_le_bytes());
    bytes[96..104].copy_from_slice(&((loader.len() + 1) as u64).to_le_bytes());
    bytes[160..160 + loader.len()].copy_from_slice(loader.as_bytes());
    if let Some(glibc) = glibc {
        bytes[224..224 + glibc.len()].copy_from_slice(glibc.as_bytes());
    }
    bytes
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

fn metric_duration(metrics_line: &str, phase: &str) -> f64 {
    metrics_line
        .split_whitespace()
        .find_map(|field| field.strip_prefix(&format!("{phase}=")))
        .expect("metrics line contains requested phase")
        .strip_suffix('s')
        .expect("phase duration ends in seconds")
        .parse::<f64>()
        .expect("phase duration is numeric")
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
