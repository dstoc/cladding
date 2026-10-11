#!/usr/bin/env python3
"""Check pinned Baffle releases and architecture-specific CI helper artifacts."""

import re
import unittest
from pathlib import Path


WORKFLOW = Path(__file__).parents[1] / ".github/workflows/ci.yml"
TEXT = WORKFLOW.read_text()
BAFFLE_CONFIG_VALIDATOR = Path(__file__).parents[1] / "scripts/validate_baffle_config.sh"
BAFFLE_RUNTIME_VALIDATOR = (
    Path(__file__).parents[1] / "scripts/validate_baffle_runtime.sh"
)
RUNTIME_TOPOLOGY_VALIDATOR = (
    Path(__file__).parents[1] / "scripts/validate_runtime_topology.sh"
)
RUNTIME_TOPOLOGY_IMAGE = (
    Path(__file__).parents[1] / "scripts/runtime-topology-image/Containerfile"
)
PODMAN_BUILD_TIMER = Path(__file__).parents[1] / "scripts/podman-build-timer"
MACHINE_FEASIBILITY_WORKFLOW = (
    Path(__file__).parents[1]
    / ".github/workflows/podman-machine-linux-feasibility.yml"
)


def job_block(name: str) -> str:
    match = re.search(
        rf"(?ms)^  {re.escape(name)}:\n(.*?)(?=^  [a-zA-Z0-9_-]+:\n|\Z)", TEXT
    )
    if match is None:
        raise AssertionError(f"workflow job is missing: {name}")
    return match.group(1)


class BaffleCiAssetsTests(unittest.TestCase):
    def test_baffle_runtime_reports_each_sandbox_socket_failure(self):
        validator = BAFFLE_RUNTIME_VALIDATOR.read_text()
        self.assertIn("diagnose_run_socket_endpoints() {", validator)
        self.assertIn("for component in nw-sandbox fs-sandbox; do", validator)
        self.assertIn('if [ -S "$socket_path" ]; then', validator)
        self.assertIn(
            "podman inspect --format '{{.State.Status}} exit={{.State.ExitCode}}",
            validator,
        )
        self.assertIn("podman inspect --format '{{range .Mounts}}", validator)
        self.assertIn('podman logs --tail 50 "$container"', validator)
        self.assertIn(
            'redact_startup_log < "$temp_root/startup.log" > "$temp_root/startup.redacted.log"',
            validator,
        )
        self.assertIn(
            'Saved cladding up --verbose output (fixture credentials redacted):',
            validator,
        )
        self.assertIn('cat "$temp_root/startup.redacted.log"', validator)
        self.assertIn('cat "$diagnostics_file"', validator)
        self.assertIn('>> "$GITHUB_STEP_SUMMARY"', validator)
        self.assertIn("### Sandbox UDS endpoint diagnostics", validator)
        self.assertIn('diagnose_run_socket_endpoints\n  exit 1', validator)
        self.assertIn("verify_runtime_path_metadata() {", validator)
        self.assertIn(
            'stat -c "observed: mode=%a uid=%u gid=%g path=%n" "$path"',
            validator,
        )
        self.assertIn("### Runtime path metadata failure", validator)
        self.assertIn(
            "::error title=Runtime path metadata failure::%s", validator
        )
        for phase in (
            "verify Baffle $component socket directory",
            "verify Baffle $component proxy socket",
            "verify $component run directory in $container",
            "verify $component run socket in $container",
        ):
            with self.subTest(phase=phase):
                self.assertIn(phase, validator)

    def test_baffle_config_validator_prepares_managed_socket_volume_permissions(self):
        validator = BAFFLE_CONFIG_VALIDATOR.read_text()
        self.assertIn('initialize_socket_volume() {', validator)
        self.assertIn('--user "$container_uid:$container_gid"', validator)
        self.assertIn('--volume "$volume_name:/socket:U"', validator)
        self.assertIn('chmod 0700 /socket', validator)
        self.assertIn(
            'test "$(stat -c "%u:%g" /socket)" = "$(id -u):$(id -g)"',
            validator,
        )
        self.assertIn('initialize_socket_volume "$socket_volume_agent"', validator)
        self.assertIn('initialize_socket_volume "$socket_volume_nw_sandbox"', validator)

    def test_topology_validator_checks_baffle_data_sockets_from_both_consumers(self):
        validator = RUNTIME_TOPOLOGY_VALIDATOR.read_text()
        self.assertIn("assert_denied_connect() {", validator)
        self.assertIn("--connect-timeout 5 --max-time 10", validator)
        self.assertIn("https://example.com", validator)
        self.assertIn("> CONNECT example.com:443 HTTP/", validator)
        self.assertIn("< HTTP/[0-9.]+ 403", validator)
        self.assertIn('assert_denied_connect "$agent" agent', validator)
        self.assertIn('assert_denied_connect "$nw_sandbox" nw-sandbox', validator)

    def test_topology_validator_saves_startup_diagnostics_to_actions_summary(self):
        validator = RUNTIME_TOPOLOGY_VALIDATOR.read_text()
        self.assertIn('if [ -n "${GITHUB_STEP_SUMMARY:-}" ]; then', validator)
        self.assertIn("Runtime topology failure diagnostics", validator)
        self.assertIn('tail -n 200 "$temp_root/startup.log"', validator)
        self.assertIn('tail -n 200 "$temp_root/partial-startup.log"', validator)
        self.assertIn("Runtime topology startup details", validator)
        self.assertIn('tail -n 10 "$temp_root/startup.log"', validator)
        self.assertIn("grep -Ei 'error|failed|permission|denied", validator)
        self.assertIn('cut -c1-4000', validator)

    def test_topology_validator_prepares_nested_workspace_mount_point(self):
        validator = RUNTIME_TOPOLOGY_VALIDATOR.read_text()
        prepare = 'mkdir -p "$project_root/home/workspace"'
        self.assertIn(prepare, validator)
        self.assertLess(
            validator.index(prepare), validator.index('phase="start Cladding topology"')
        )

    def test_topology_validator_builds_and_times_one_minimal_shared_image(self):
        validator = RUNTIME_TOPOLOGY_VALIDATOR.read_text()
        containerfile = RUNTIME_TOPOLOGY_IMAGE.read_text()
        timer = PODMAN_BUILD_TIMER.read_text()

        self.assertIn(
            "topology_image=localhost/cladding-runtime-topology:latest", validator
        )
        self.assertIn(
            '--arg containerfile "$script_dir/runtime-topology-image/Containerfile"',
            validator,
        )
        self.assertIn(
            '--arg context "$script_dir/runtime-topology-image"', validator
        )
        self.assertEqual(validator.count("args: {UID: $uid, GID: $gid}"), 2)
        self.assertIn(".agent.build = {", validator)
        self.assertIn(
            ".nw_sandbox.build = {",
            validator,
        )
        self.assertIn('host_uid=$(id -u)', validator)
        self.assertIn('host_gid=$(id -g)', validator)
        self.assertIn("Topology runtime image build duration:", validator)
        self.assertIn('>> "$GITHUB_STEP_SUMMARY"', validator)
        self.assertIn(r'echo "Image: \`$topology_image\`."', validator)
        self.assertIn('if [ "$previous_argument" = "-t" ]', timer)
        self.assertIn('"$real_podman" "$@"', timer)
        self.assertIn('printf \'%s\\n\' "$((finished_at - started_at))"', timer)

        self.assertIn("FROM debian:trixie-slim", containerfile)
        self.assertIn(
            "apt-get install -y --no-install-recommends ca-certificates coreutils curl grep socat",
            containerfile,
        )
        self.assertIn("USER ${UID}:${GID}", containerfile)
        self.assertIn("ENV HOME=/home/user", containerfile)
        self.assertIn("WORKDIR /home/user/workspace", containerfile)
        for excluded in (
            "node",
            "codex",
            "claude-code",
            "pi-coding-agent",
            "build-essential",
            "gcc",
            "clang",
            "cargo",
            "python3",
            "python3-pip",
            "python3-venv",
        ):
            with self.subTest(excluded=excluded):
                self.assertNotIn(excluded, containerfile.lower())

    def test_local_and_machine_ci_use_the_shared_topology_validator(self):
        linux = job_block("baffle-config")
        machine = job_block("baffle-podman-machine")
        self.assertIn("scripts/validate_runtime_topology.sh default", linux)
        self.assertIn("scripts/validate_runtime_topology.sh default", machine)
        self.assertNotIn("scripts/validate_baffle_runtime.sh", machine)
        self.assertNotIn("scripts/validate_managed_mounts.sh", machine)

    def test_linux_machine_job_uses_the_pinned_rootless_remote_engine(self):
        machine = job_block("baffle-podman-machine")
        self.assertIn("runs-on: ubuntu-24.04", machine)
        self.assertIn("needs: baffle-podman-machine-assets-x86_64", machine)
        self.assertIn("PODMAN_VERSION: 5.8.2", machine)
        self.assertIn("cargo build --locked --bin cladding", machine)
        self.assertIn("podman-machine-default", machine)
        self.assertIn(
            "machine init --cpus 2 --memory 4096 --rootful=false", machine
        )
        self.assertIn(
            'info --format \'{{.Host.Security.Rootless}}\'', machine
        )
        self.assertIn('machine ssh "$machine_name" id -u', machine)
        self.assertIn(
            'exec /usr/local/bin/podman --connection "${PODMAN_MACHINE_CONNECTION:?}" "$@"',
            machine,
        )
        self.assertIn('export PATH="$podman_wrapper_dir:$PATH"', machine)
        self.assertIn('trap cleanup EXIT', machine)
        self.assertIn('machine rm --force "$machine_name"', machine)
        self.assertIn(
            'CLADDING_TOPOLOGY_TMP_DIR="$GITHUB_WORKSPACE"', machine
        )
        self.assertNotIn("baffle-macos-x86_64-build", machine)

        self.assertNotIn("macos", machine)
        self.assertNotIn("baffle-podman-machine-linux", TEXT)

    def test_linux_machine_feasibility_selects_rootless_remote_connection(self):
        workflow = MACHINE_FEASIBILITY_WORKFLOW.read_text()
        self.assertIn(
            'podman machine init --cpus 2 --memory 4096 --rootful=false "$machine_name"',
            workflow,
        )
        self.assertIn(
            'podman --connection "$machine_name" info --format', workflow
        )

    def test_linux_machine_feasibility_pins_upstream_remote_client(self):
        workflow = MACHINE_FEASIBILITY_WORKFLOW.read_text()
        self.assertIn("on:\n  workflow_dispatch:", workflow)
        self.assertIn("runs-on: ubuntu-24.04", workflow)
        install_packages = workflow.split(
            "sudo apt-get install -y \\\n", 1
        )[1].split("          podman --version", 1)[0]
        self.assertEqual(
            [
                line.strip().rstrip("\\").strip()
                for line in install_packages.splitlines()
            ],
            [
                "podman",
                "qemu-system-x86",
                "qemu-utils",
                "gvproxy",
                "virtiofsd",
                "uidmap",
                "slirp4netns",
                "fuse-overlayfs",
            ],
        )
        self.assertIn("PODMAN_VERSION: 5.8.2", workflow)
        self.assertIn("--repo podman-container-tools/podman", workflow)
        self.assertIn(
            "--pattern podman-remote-static-linux_amd64.tar.gz", workflow
        )
        self.assertIn(
            '"$release_dir/bin/podman-remote-static-linux_amd64"', workflow
        )
        self.assertIn(
            'test "$(podman --version)" = "podman version ${PODMAN_VERSION}"',
            workflow,
        )
        self.assertIn(
            'sudo ln -sf /usr/bin/gvproxy "$helper_dir/gvproxy"', workflow
        )
        self.assertIn(
            'sudo ln -sf /usr/libexec/virtiofsd "$helper_dir/virtiofsd"',
            workflow,
        )
        self.assertIn("sudo chmod 666 /dev/kvm", workflow)
        self.assertIn("for attempt in 1 2 3; do", workflow)
        self.assertIn('podman machine ssh "$machine_name" id -u', workflow)
        self.assertIn("trap cleanup EXIT", workflow)

    def test_baffle_is_not_built_from_source_in_ci(self):
        self.assertNotIn("  build-baffle:\n", TEXT)
        self.assertNotRegex(TEXT, r"(?m)^\s*run: cargo install .*baffle-proxy")

    def test_linux_helpers_are_built_once_per_architecture(self):
        cases = {
            "linux-helper-assets-x86_64": (
                "x86_64",
                "cargo build --locked --release -p mcp-run --bin mcp-run --bin run-remote --target-dir target/mcp-run",
                "cladding-linux-helpers-x86_64",
            ),
            "linux-helper-assets-aarch64": (
                "aarch64",
                "cargo build --locked --release -p mcp-run --bin mcp-run --bin run-remote --target aarch64-unknown-linux-gnu --target-dir target/mcp-run",
                "cladding-linux-helpers-aarch64",
            ),
        }
        for job, (arch, build, artifact) in cases.items():
            with self.subTest(job=job):
                block = job_block(job)
                self.assertIn(build, block)
                self.assertIn(
                    "cc -std=c11 -Wall -Wextra -Werror scripts/baffle_socket_probe.c",
                    block,
                )
                machine = "x86-64" if arch == "x86_64" else "aarch64"
                self.assertIn(f'"ELF 64-bit"*{machine}*) ;;', block)
                self.assertIn("test -x \"$tool\"", block)
                self.assertIn("actions/upload-artifact@v4", block)
                self.assertIn(f"name: {artifact}", block)

        self.assertEqual(
            len(
                re.findall(
                    r"(?m)^\s*run: cargo build --locked --release -p mcp-run --bin mcp-run --bin run-remote",
                    TEXT,
                )
            ),
            2,
        )

    def test_linux_jobs_consume_matching_helper_artifacts(self):
        for job in ("test", "baffle-config"):
            with self.subTest(job=job):
                block = job_block(job)
                self.assertIn("needs: linux-helper-assets-x86_64", block)
                self.assertIn("name: cladding-linux-helpers-x86_64", block)
                self.assertIn("Restore and verify downloaded x86_64 Linux helpers", block)
                self.assertIn("chmod 0755 \"$tool\"", block)
                self.assertIn("file -b \"$tool\"", block)
                self.assertIn('"ELF 64-bit"*x86-64*) ;;', block)
                self.assertNotIn("cargo build --locked --release -p mcp-run", block)
                self.assertNotIn("cc -std=c11 -Wall -Wextra -Werror scripts/baffle_socket_probe.c", block)

        test = job_block("test")
        self.assertIn("CLADDING_MCP_RUN_BIN=$RUNNER_TEMP/linux-helpers/bin/mcp-run", test)
        self.assertIn("CLADDING_RUN_REMOTE_BIN=$RUNNER_TEMP/linux-helpers/bin/run-remote", test)

        config = job_block("baffle-config")
        self.assertIn(
            "BAFFLE_SOCKET_PROBE_BIN=$RUNNER_TEMP/linux-helpers/bin/baffle-socket-probe",
            config,
        )

        for job, arch, artifact in (
            (
                "baffle-podman-machine-assets-x86_64",
                "x86_64",
                "cladding-linux-helpers-x86_64",
            ),
            (
                "baffle-podman-machine-assets-aarch64",
                "aarch64",
                "cladding-linux-helpers-aarch64",
            ),
        ):
            with self.subTest(job=job):
                block = job_block(job)
                self.assertIn(f"needs: linux-helper-assets-{arch}", block)
                self.assertIn(f"name: {artifact}", block)
                self.assertIn(f"Restore and verify downloaded {arch} Linux helpers", block)
                self.assertIn("chmod 0755 \"$tool\"", block)
                self.assertIn("test -x \"$tool\"", block)
                self.assertIn("file -b \"$tool\"", block)
                machine = "x86-64" if arch == "x86_64" else "aarch64"
                self.assertIn(f'"ELF 64-bit"*{machine}*) ;;', block)
                self.assertNotIn("cargo build --locked --release -p mcp-run", block)
                self.assertNotIn("cc -std=c11 -Wall -Wextra -Werror scripts/baffle_socket_probe.c", block)

    def test_each_release_download_is_pinned_verified_and_arch_checked(self):
        cases = {
            "test": ("x86_64", "baffle-proxy-v1.1.0-x86_64-unknown-linux-gnu.tar.gz"),
            "baffle-config": (
                "x86_64",
                "baffle-proxy-v1.1.0-x86_64-unknown-linux-gnu.tar.gz",
            ),
            "baffle-podman-machine-assets-x86_64": (
                "x86_64",
                "baffle-proxy-v1.1.0-x86_64-unknown-linux-gnu.tar.gz",
            ),
            "baffle-podman-machine-assets-aarch64": (
                "aarch64",
                "baffle-proxy-v1.1.0-aarch64-unknown-linux-gnu.tar.gz",
            ),
        }
        for job, (arch, archive) in cases.items():
            with self.subTest(job=job):
                block = job_block(job)
                self.assertIn(f"BAFFLE_ARCHIVE: {archive}", block)
                self.assertIn(
                    'release_url="https://github.com/dstoc/baffle/releases/download/v1.1.0"',
                    block,
                )
                self.assertIn('"$release_url/$BAFFLE_ARCHIVE"', block)
                self.assertIn('"$release_url/SHA256SUMS"', block)
                self.assertIn("awk -v archive=", block)
                self.assertIn('test "$actual" = "$expected"', block)
                self.assertIn("sha256sum", block)
                self.assertIn("shasum -a 256", block)
                self.assertIn('chmod 0755 "$RUNNER_TEMP/baffle/baffle"', block)
                self.assertIn('description=$(file -b "$RUNNER_TEMP/baffle/baffle")', block)
                machine = "x86-64" if arch == "x86_64" else "aarch64"
                self.assertIn(f'"ELF 64-bit"*{machine}*) ;;', block)

    def test_required_test_path_waits_only_for_x86_64_helper_prep(self):
        test = job_block("test")
        self.assertIn("needs: linux-helper-assets-x86_64", test)
        self.assertNotIn("aarch64", test)
        self.assertNotIn("baffle-podman-machine", test)

        config = job_block("baffle-config")
        self.assertIn("needs: linux-helper-assets-x86_64", config)
        self.assertNotIn("aarch64", config)

        for job in ("linux-helper-assets-x86_64", "linux-helper-assets-aarch64"):
            with self.subTest(job=job):
                self.assertNotRegex(job_block(job), r"(?m)^    needs:")

        intel_assets = job_block("baffle-podman-machine-assets-x86_64")
        self.assertIn("needs: linux-helper-assets-x86_64", intel_assets)
        self.assertNotIn("linux-helper-assets-aarch64", intel_assets)
        intel_build = job_block("baffle-macos-x86_64-build")
        self.assertIn("needs: baffle-podman-machine-assets-x86_64", intel_build)
        self.assertIn("cladding-baffle-linux-tools-x86_64", intel_build)
        self.assertIn("cladding-macos-x86_64", intel_build)

        machine = job_block("baffle-podman-machine")
        self.assertIn("needs: baffle-podman-machine-assets-x86_64", machine)
        self.assertNotIn("baffle-macos-x86_64-build", machine)
        self.assertNotIn("baffle-config", machine)
        self.assertNotIn(
            "needs: baffle-podman-machine\n", job_block("baffle-macos-x86_64-build")
        )

        apple_assets = job_block("baffle-podman-machine-assets-aarch64")
        self.assertIn("needs: linux-helper-assets-aarch64", apple_assets)
        self.assertNotIn("linux-helper-assets-x86_64", apple_assets)
        apple_silicon = job_block("baffle-apple-silicon-build")
        self.assertIn("needs: baffle-podman-machine-assets-aarch64", apple_silicon)
        self.assertIn("cladding-baffle-linux-tools-aarch64", apple_silicon)
        self.assertIn('test -f "$tool"', apple_silicon)
        self.assertIn('test -x "$tool"', apple_silicon)
        self.assertIn('"ELF 64-bit"*aarch64*) ;;', apple_silicon)

    def test_behavior_and_notice_coverage_remain_in_ci(self):
        test = job_block("test")
        self.assertIn("python3 scripts/test_collect_baffle_notices.py", test)
        self.assertIn("python3 scripts/test_ci_baffle_assets.py", test)

        config = job_block("baffle-config")
        for validator in (
            "scripts/validate_baffle_config.sh",
            "scripts/validate_baffle_runtime.sh default",
            "scripts/validate_baffle_runtime.sh runsc",
            "scripts/validate_managed_mounts.sh default",
            "scripts/validate_managed_mounts.sh runsc",
        ):
            self.assertIn(validator, config)


if __name__ == "__main__":
    unittest.main()
