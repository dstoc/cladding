#!/usr/bin/env python3
"""Check pinned Baffle releases and architecture-specific CI helper artifacts."""

import re
import unittest
from pathlib import Path


WORKFLOW = Path(__file__).parents[1] / ".github/workflows/ci.yml"
TEXT = WORKFLOW.read_text()
RELEASE_WORKFLOW = Path(__file__).parents[1] / ".github/workflows/release-please.yml"
RELEASE_TEXT = RELEASE_WORKFLOW.read_text()
EMBEDDED_BAFFLE_DOC = (
    Path(__file__).parents[1] / "docs/features/baffle-integration/embedded-baffle.md"
)
BAFFLE_PRD = Path(__file__).parents[1] / "docs/features/baffle-integration/prd.md"
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
PODMAN_MACHINE_BIND_VALIDATOR = (
    Path(__file__).parents[1] / "scripts/validate_podman_machine_binds.sh"
)


def job_block(name: str, workflow: str = TEXT) -> str:
    match = re.search(
        rf"(?ms)^  {re.escape(name)}:\n(.*?)(?=^  [a-zA-Z0-9_-]+:\n|\Z)", workflow
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

    def test_baffle_runtime_stops_tls_origins_gracefully(self):
        validator = BAFFLE_RUNTIME_VALIDATOR.read_text()
        origin = (Path(__file__).parent / "baffle_test_origin.py").read_text()

        self.assertIn("signal.signal(signal.SIGTERM, request_shutdown)", origin)
        self.assertIn("shutdown_requested.wait()", origin)
        self.assertIn("server.shutdown()", origin)
        self.assertIn("server.server_close()", origin)
        self.assertIn("stop_origin_container() {", validator)
        self.assertIn('podman stop "$container_name"', validator)
        self.assertIn("[ \"$exit_code\" != 0 ]", validator)
        self.assertIn('[ "$elapsed_ms" -ge 5000 ]', validator)
        self.assertIn('podman rm "$origin_name"', validator)
        self.assertIn('podman rm "$run_origin_name"', validator)
        self.assertEqual(validator.count('stop_origin_container "$origin_name"'), 2)
        self.assertEqual(validator.count('stop_origin_container "$run_origin_name"'), 1)
        self.assertIn('cleanup_origin_container "$origin_name"', validator)
        self.assertIn('cleanup_origin_container "$run_origin_name"', validator)
        self.assertNotIn('podman rm -f "$origin_name"', validator)
        self.assertNotIn('podman rm -f "$run_origin_name"', validator)

    def test_baffle_runtime_failure_trap_releases_run_before_bounded_reap(self):
        validator = BAFFLE_RUNTIME_VALIDATOR.read_text()
        cleanup = validator.split("cleanup() {", 1)[1].split("\n}\n", 1)[0]
        reap = validator.split("reap_finished_run() {", 1)[1].split("\n}\n", 1)[0]
        probe = validator.split(
            'phase="start one-off runtime for failure-trap cleanup check"', 1
        )[1]

        self.assertLess(
            cleanup.index('cleanup_origin_container "$run_origin_name"'),
            cleanup.index('touch "$temp_root/workspace/.run-origin-ready"'),
        )
        self.assertIn('"$temp_root/workspace/.failure-run-finish"', cleanup)
        self.assertIn('wait_for_process_exit "$run_pid" 50', cleanup)
        self.assertIn('kill -KILL "$run_pid"', cleanup)
        self.assertIn(
            "one-off run did not exit after its workload gates were released",
            cleanup,
        )
        self.assertIn('kill -0 "$run_pid" 2>/dev/null', reap)
        self.assertLess(
            reap.index('kill -0 "$run_pid"'), reap.index('wait "$run_pid"')
        )
        self.assertIn(".failure-run-finish", probe)
        self.assertNotIn("while :; do sleep", probe)
        self.assertNotIn('wait "$run_child"', validator)

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
        self.assertIn("needs: embedded-linux-tools-x86_64", machine)
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

    def test_linux_machine_job_runs_the_four_case_bind_mount_reproduction(self):
        machine = job_block("baffle-podman-machine")
        validator = PODMAN_MACHINE_BIND_VALIDATOR.read_text()
        repro = "scripts/validate_podman_machine_binds.sh"
        topology = "scripts/validate_runtime_topology.sh default"
        self.assertIn(repro, machine)
        self.assertLess(machine.index(repro), machine.index(topology))
        self.assertIn("PODMAN_MACHINE_CONNECTION", machine)
        self.assertIn("id", validator)
        self.assertIn("stat -c '%u:%g %a %n'", validator)
        self.assertIn('"$machine_ssh" sh -lc', validator)
        for probe in (
            'run_probe A keep-id "$host_workspace"',
            'run_probe B default "$host_workspace"',
            'run_probe C keep-id "$vm_workspace"',
            'run_probe D default "$vm_workspace"',
        ):
            with self.subTest(probe=probe):
                self.assertIn(probe, validator)
        self.assertIn("--userns keep-id", validator)
        self.assertIn(
            "::notice title=Podman Machine bind diagnostics::%s", validator
        )
        self.assertIn('>> "$GITHUB_STEP_SUMMARY"', validator)

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

    def test_complete_embedded_linux_tools_are_built_once_per_architecture(self):
        cases = {
            "embedded-linux-tools-x86_64": (
                "x86_64",
                "cargo build --locked --release -p mcp-run --bin mcp-run --bin run-remote --target-dir target/mcp-run",
                "baffle-proxy-v1.1.0-x86_64-unknown-linux-gnu.tar.gz",
                "x86-64",
            ),
            "embedded-linux-tools-aarch64": (
                "aarch64",
                "cargo build --locked --release -p mcp-run --bin mcp-run --bin run-remote --target aarch64-unknown-linux-gnu --target-dir target/mcp-run",
                "baffle-proxy-v1.1.0-aarch64-unknown-linux-gnu.tar.gz",
                "aarch64",
            ),
        }
        for job, (arch, build, archive, machine) in cases.items():
            with self.subTest(job=job):
                block = job_block(job)
                self.assertIn(build, block)
                self.assertIn(
                    "cc -std=c11 -Wall -Wextra -Werror scripts/baffle_socket_probe.c",
                    block,
                )
                self.assertIn(f"BAFFLE_ARCHIVE: {archive}", block)
                self.assertIn(
                    'release_url="https://github.com/dstoc/baffle/releases/download/v1.1.0"',
                    block,
                )
                self.assertIn('"$release_url/SHA256SUMS"', block)
                self.assertIn('test "$actual" = "$expected"', block)
                self.assertIn('tar -xzf "$archive_path"', block)
                self.assertIn('cp "$RUNNER_TEMP/baffle/baffle"', block)
                for name in (
                    "baffle",
                    "mcp-run",
                    "run-remote",
                    "baffle-socket-probe",
                ):
                    self.assertIn(f'"$asset_dir/{name}"', block)
                self.assertIn(f'"ELF 64-bit"*{machine}*) ;;', block)
                self.assertIn('test -x "$tool"', block)
                self.assertIn("actions/upload-artifact@v4", block)
                self.assertIn(f"name: embedded-linux-tools-{arch}", block)

        self.assertEqual(
            len(re.findall(r"(?m)^  embedded-linux-tools-[a-z0-9_]+:$", TEXT)),
            2,
        )
        self.assertEqual(
            TEXT.count("Download and verify pinned x86_64 Baffle release"), 1
        )
        self.assertEqual(
            TEXT.count("Download and verify pinned aarch64 Baffle release"), 1
        )

    def test_ci_consumers_download_matching_complete_artifacts_directly(self):
        cases = (
            ("test", "x86_64", "x86-64"),
            ("baffle-config", "x86_64", "x86-64"),
            ("baffle-podman-machine", "x86_64", "x86-64"),
            ("baffle-apple-silicon-build", "aarch64", "aarch64"),
        )
        for job, arch, machine in cases:
            with self.subTest(job=job):
                block = job_block(job)
                artifact = f"embedded-linux-tools-{arch}"
                self.assertIn(f"needs: {artifact}", block)
                self.assertIn(f"name: {artifact}", block)
                self.assertIn("actions/download-artifact@v4", block)
                if job == "baffle-apple-silicon-build":
                    self.assertIn("Restore and verify Linux aarch64 tools", block)
                else:
                    self.assertIn("Restore and verify embedded", block)
                self.assertIn('chmod 0755 "$tool"', block)
                self.assertIn('test -x "$tool"', block)
                self.assertIn('file -b "$tool"', block)
                self.assertIn(f'"ELF 64-bit"*{machine}*) ;;', block)
                self.assertIn("baffle-socket-probe", block)
                self.assertNotIn("Download and verify pinned", block)
                self.assertNotIn("linux-helper-assets", block)
                self.assertNotIn("baffle-podman-machine-assets", block)

        test = job_block("test")
        self.assertIn(
            "CLADDING_BAFFLE_BIN=$RUNNER_TEMP/linux-tools/bin/baffle", test
        )
        self.assertIn(
            "CLADDING_MCP_RUN_BIN=$RUNNER_TEMP/linux-tools/bin/mcp-run", test
        )
        self.assertIn(
            "CLADDING_RUN_REMOTE_BIN=$RUNNER_TEMP/linux-tools/bin/run-remote",
            test,
        )
        config = job_block("baffle-config")
        self.assertIn(
            "BAFFLE_SOCKET_PROBE_BIN=$RUNNER_TEMP/linux-tools/bin/baffle-socket-probe",
            config,
        )
        self.assertIn('"$RUNNER_TEMP/linux-tools/bin/baffle"', config)

    def test_linux_asset_graph_has_no_intermediate_staging_jobs(self):
        for old_job in (
            "linux-helper-assets-x86_64",
            "linux-helper-assets-aarch64",
            "baffle-podman-machine-assets-x86_64",
            "baffle-podman-machine-assets-aarch64",
        ):
            with self.subTest(job=old_job):
                self.assertNotRegex(TEXT, rf"(?m)^  {re.escape(old_job)}:$")
        self.assertNotIn("cladding-linux-helpers-", TEXT)
        self.assertNotIn("cladding-baffle-linux-tools-", TEXT)

    def test_release_jobs_use_pinned_verified_baffle_release_archives(self):
        self.assertNotIn("  build-baffle:\n", RELEASE_TEXT)
        self.assertNotRegex(
            RELEASE_TEXT, r"(?m)^\s*run: cargo install .*baffle-proxy"
        )
        self.assertNotIn("scripts/collect-baffle-notices.py", RELEASE_TEXT)

        upload = job_block("upload-binaries", RELEASE_TEXT)
        self.assertIn("needs: [release-please, build-helpers]", upload)
        self.assertNotIn("embedded-linux-tools-", upload)
        release_platforms = re.findall(
            r"(?m)^          - os: ([^\s]+)\n            target: ([^\s]+)\n"
            r"            baffle_arch: ([^\s]+)\n            helper_arch: ([^\s]+)$",
            upload,
        )
        self.assertEqual(
            release_platforms,
            [
                (
                    "ubuntu-latest",
                    "x86_64-unknown-linux-gnu",
                    "x86_64",
                    "x86_64",
                ),
                (
                    "ubuntu-24.04-arm",
                    "aarch64-unknown-linux-gnu",
                    "aarch64",
                    "aarch64",
                ),
                (
                    "macos-latest",
                    "aarch64-apple-darwin",
                    "aarch64",
                    "aarch64",
                ),
            ],
        )

        download = upload.split(
            "- name: Download and verify pinned Baffle v1.1.0 release", 1
        )[1].split("- uses: dtolnay/rust-toolchain@stable", 1)[0]
        self.assertIn(
            "BAFFLE_ARCHIVE: baffle-proxy-v1.1.0-${{ matrix.baffle_arch }}-unknown-linux-gnu.tar.gz",
            download,
        )
        self.assertIn(
            'release_url="https://github.com/dstoc/baffle/releases/download/v1.1.0"',
            download,
        )
        self.assertIn('"$release_url/$BAFFLE_ARCHIVE"', download)
        self.assertIn('"$release_url/SHA256SUMS"', download)
        self.assertIn("awk -v archive=", download)
        self.assertIn('test "$actual" = "$expected"', download)
        self.assertLess(
            download.index('test "$actual" = "$expected"'),
            download.index("tar -xzf"),
        )
        self.assertIn("sha256sum", download)
        self.assertIn("shasum -a 256", download)
        self.assertIn('test -s "$RUNNER_TEMP/baffle/LICENSE"', download)
        self.assertIn(
            'test -s "$RUNNER_TEMP/baffle/share/doc/baffle/licenses/THIRD-PARTY-NOTICES.txt"',
            download,
        )
        self.assertIn(
            'test -s "$RUNNER_TEMP/baffle/share/doc/baffle/licenses/webpki-root-certs-CDLA-Permissive-2.0.txt"',
            download,
        )
        self.assertIn(
            'description=$(file -b "$RUNNER_TEMP/baffle/baffle")', download
        )
        self.assertIn('x86_64:"ELF 64-bit"*x86-64*) ;;', download)
        self.assertIn('aarch64:"ELF 64-bit"*aarch64*) ;;', download)
        self.assertIn(
            'CLADDING_BAFFLE_BIN=$RUNNER_TEMP/baffle/baffle', upload
        )

    def test_release_package_copies_upstream_baffle_redistribution_files(self):
        upload = job_block("upload-binaries", RELEASE_TEXT)
        package = upload.split("- name: Package", 1)[1].split(
            "- name: Upload to release", 1
        )[0]
        for path in (
            '"$RUNNER_TEMP/baffle/LICENSE" "$staging/licenses/baffle/LICENSE"',
            '"$RUNNER_TEMP/baffle/share/doc/baffle/licenses/THIRD-PARTY-NOTICES.txt" "$staging/licenses/baffle/"',
            '"$RUNNER_TEMP/baffle/share/doc/baffle/licenses/webpki-root-certs-CDLA-Permissive-2.0.txt" "$staging/licenses/baffle/"',
        ):
            with self.subTest(path=path):
                self.assertIn(path, package)
        self.assertNotIn("THIRD_PARTY_NOTICES.md", package)
        self.assertNotIn("baffle-Cargo.lock", package)

    def test_baffle_docs_describe_source_builds_and_published_release_assets(self):
        guide = EMBEDDED_BAFFLE_DOC.read_text()
        prd = BAFFLE_PRD.read_text()
        self.assertIn("pinned Baffle v1.1.0 Linux archive", guide)
        self.assertIn("`SHA256SUMS` before extraction", guide)
        self.assertIn("under `licenses/baffle/`", guide)
        self.assertNotIn("Do not use Baffle's GitHub release assets", guide)
        self.assertIn(
            "Do not download GitHub release assets for this source-build path.",
            prd,
        )
        self.assertIn(
            "The workflows use the explicit v1.1.0 release archives", prd
        )

    def test_required_test_path_waits_only_for_x86_64_asset_prep(self):
        test = job_block("test")
        self.assertIn("needs: embedded-linux-tools-x86_64", test)
        self.assertNotIn("aarch64", test)
        self.assertNotIn("baffle-podman-machine", test)

        config = job_block("baffle-config")
        self.assertIn("needs: embedded-linux-tools-x86_64", config)
        self.assertNotIn("aarch64", config)

        for arch in ("x86_64", "aarch64"):
            with self.subTest(arch=arch):
                producer = job_block(f"embedded-linux-tools-{arch}")
                self.assertNotRegex(producer, r"(?m)^    needs:")

        linux_machine = job_block("baffle-podman-machine")
        self.assertIn("needs: embedded-linux-tools-x86_64", linux_machine)
        self.assertNotIn("baffle-config", linux_machine)

        apple_silicon = job_block("baffle-apple-silicon-build")
        self.assertIn("needs: embedded-linux-tools-aarch64", apple_silicon)
        self.assertIn("embedded-linux-tools-aarch64", apple_silicon)
        self.assertIn('test -f "$tool"', apple_silicon)
        self.assertIn('test -x "$tool"', apple_silicon)
        self.assertIn('"ELF 64-bit"*aarch64*) ;;', apple_silicon)

    def test_ci_and_release_platforms_exclude_intel_macos(self):
        for reference in (
            "baffle-macos-x86_64-build",
            "macos-15-intel",
            "cladding-macos-x86_64",
            "x86_64-apple-darwin",
        ):
            with self.subTest(reference=reference):
                self.assertNotIn(reference, TEXT)
                self.assertNotIn(reference, RELEASE_TEXT)

        self.assertIn("embedded-linux-tools-x86_64", TEXT)
        self.assertIn("embedded-linux-tools-x86_64", TEXT)
        self.assertIn("embedded-linux-tools-aarch64", TEXT)
        self.assertIn("baffle-podman-machine:", TEXT)
        self.assertIn("scripts/validate_runtime_topology.sh default", TEXT)
        self.assertIn("baffle-apple-silicon-build:", TEXT)
        self.assertEqual(
            re.findall(r"(?m)^    runs-on: (macos-[^\s]+)$", TEXT),
            ["macos-15"],
        )
        self.assertIn(
            "runs-on: macos-15", job_block("baffle-apple-silicon-build")
        )

        release_upload = job_block("upload-binaries", RELEASE_TEXT)
        release_platforms = re.findall(
            r"(?m)^          - os: ([^\s]+)\n            target: ([^\s]+)$",
            release_upload,
        )
        self.assertEqual(
            release_platforms,
            [
                ("ubuntu-latest", "x86_64-unknown-linux-gnu"),
                ("ubuntu-24.04-arm", "aarch64-unknown-linux-gnu"),
                ("macos-latest", "aarch64-apple-darwin"),
            ],
        )

    def test_behavior_and_release_packaging_checks_remain_in_ci(self):
        test = job_block("test")
        self.assertIn("python3 scripts/test_ci_baffle_assets.py", test)
        self.assertNotIn("test_collect_baffle_notices.py", test)

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
