# Baffle integration test matrix

The Baffle tests use the pinned **baffle-proxy 0.2.0** executable. They run
from the **baffle-config** GitHub Actions job with rootless Podman.

| Environment | Coverage |
| --- | --- |
| Ubuntu, default Podman runtime | Actual Cladding proxy, agent, and network-sandbox containers. The suite checks file-only session startup, exact host, port, and path policy, plaintext HTTP rejection, separate component policies, disabled network-sandbox lifecycle, trust installation, curl, Git, Node.js, token replacement, secret and control-socket isolation, reload snapshots, invalid reloads, persistent CA reuse, shutdown, and run cleanup after a nonzero command. |
| Ubuntu, runsc execution runtime | The same Cladding policy and client suite, with the agent and network sandbox running under runsc. The startup validator also checks scoped UDS access under runsc. |
| Intel macOS, Podman machine | Linux amd64 Baffle startup with the named-volume UDS relay. The validator checks enabled and disabled sessions, socket permissions and access, daemon shutdown, and relay cleanup. |

The CI packaging matrix builds Baffle on native Linux x86_64 and aarch64
runners. It checks each ELF architecture and starts each executable in the
default proxy image built from `Containerfile.proxy`. It also builds Cladding
with each binary, which checks the embedded binary's architecture and GNU
loader. The release workflow reuses those validated Baffle files and publishes
Linux x86_64, Linux aarch64, Intel macOS, and Apple Silicon macOS archives.

The Linux runtime suite builds a private TLS origin and uses only dummy
credentials. Its event log records credential categories such as old and new;
it does not record credential values. It creates no external test service and
requires no production readiness barrier.

The Rust test suite also checks persistent CA reuse, invalid, expired, and
partial CA material, credential file permissions, and one-off credential
isolation without a container runtime.

To run the client suite locally, build Cladding with the pinned Baffle binary
and run these commands from the repository root:

    CLADDING_BIN="$(pwd)/target/debug/cladding" scripts/validate_baffle_runtime.sh default
    CLADDING_BIN="$(pwd)/target/debug/cladding" scripts/validate_baffle_runtime.sh runsc

Both modes require rootless Podman. The runsc mode also requires the runsc
runtime. The macOS Podman-machine job currently runs the startup and socket
checks only; it does not run the curl, Git, Node.js, policy-reload, or one-off
client suite.
