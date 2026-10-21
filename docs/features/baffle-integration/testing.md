# Baffle integration test matrix

The Baffle tests use the pinned **baffle-proxy 2.1.0** executable. They run
from the **linux-integration** GitHub Actions job with rootless Podman.

| Environment | Coverage |
| --- | --- |
| Ubuntu, default Podman runtime | Actual Cladding proxy, agent, and network-sandbox containers. The suite checks version 2 file-backed session startup, exact host, port, and path policy, plaintext HTTP rejection, separate component policies, disabled network-sandbox lifecycle, trust installation, curl, Git, Node.js, token replacement, same-session fake credential substitution, cross-session fake rejection, secret and control-socket isolation, reload snapshots, invalid reloads, persistent CA reuse, shutdown, and run cleanup after a nonzero command. |
| Ubuntu, runsc execution runtime | The same Cladding policy and client suite, with the agent and network sandbox running under runsc. The startup validator also checks scoped UDS access under runsc. |
| Ubuntu, rootless Podman Machine | The Linux x86_64 guest uses the validated Baffle and helper assets. CI checks four host and VM bind combinations, then runs the shared runtime topology validator against the Machine. |
| Apple Silicon macOS build | Cladding embeds and tests a Linux aarch64 Baffle executable on an ARM64 macOS host. |

The CI asset jobs download the published Linux GNU Baffle 2.1.0 archives on
native x86_64 and aarch64 runners. Each job verifies the archive checksum and
ELF architecture, then runs `baffle --version`. The x86_64 integration job
starts the binary in Cladding's default proxy image and runs the policy and
client suite under default Podman and runsc. The Linux-hosted Podman Machine
job checks the x86_64 guest topology with the validated Baffle and helper
assets. The Apple Silicon job builds Cladding with the Linux aarch64
executable and checks its embedded architecture. The release workflow
downloads and verifies the published Baffle archives independently, then
publishes Linux x86_64, Linux aarch64, and Apple Silicon macOS archives.

GitHub-hosted macOS ARM runners do not support nested virtualization, so CI
runs the Podman Machine topology checks on Linux
([GitHub runner limitations](https://docs.github.com/en/actions/reference/runners/github-hosted-runners#limitations-for-arm64-macos-runners)). Run `scripts/validate_baffle_config.sh` on an Apple Silicon host with Podman Machine to verify the Linux aarch64 socket path.

The Linux runtime suite builds a private TLS origin and uses only dummy
credentials. Its event log records credential categories such as old and new;
it does not record credential values. It creates no external test service and
requires no production readiness barrier.

The Rust test suite also checks persistent CA reuse, invalid, expired, and
partial CA material, credential file permissions, direct project-state mounts
for `cladding run`, symlinked tools, per-run runtime isolation, and runtime
cleanup with mocked Podman.

To run the client suite locally, build Cladding with the pinned Baffle binary
and run these commands from the repository root:

    CLADDING_BIN="$(pwd)/target/debug/cladding" scripts/validate_baffle_runtime.sh default
    CLADDING_BIN="$(pwd)/target/debug/cladding" scripts/validate_baffle_runtime.sh runsc

Both modes require rootless Podman. The runsc mode also requires the runsc
runtime. The Linux-hosted Podman Machine job runs the bind-mount reproduction
and shared runtime topology checks. It does not run the curl, Git, Node.js,
policy-reload, or one-off client suite above.
