# Current Runtime Summary

This file is the quick reference for the current Cladding runtime.

## Managed resources
- One standalone proxy container per project: `<name>-proxy-instance`.
- The proxy instance runs one Baffle daemon and creates the persistent agent session and, when enabled, the network-sandbox session.
- Standalone execution containers use the names `<name>-agent-instance`, `<name>-nw-sandbox-instance`, and `<name>-fs-sandbox-instance` when enabled.

## Runtime shape
- The proxy, agent, nw-sandbox, and fs-sandbox run as standalone containers.
- The proxy uses Podman's default runtime.
- The proxy uses Podman's default network. Execution containers use `--network none`.
- All runtime containers use `--userns keep-id`.
- Execution containers communicate through scoped Unix-domain socket mounts under `.cladding/runtime/sockets`.
- The proxy creates a persistent Baffle session for the agent. It creates a network-sandbox session only when that component is enabled.
- Cladding does not wait for Baffle sessions to become ready before it starts execution containers. Early requests to the local proxy can fail during startup.

## CLI execution and lifecycle
`cladding init` creates project configuration and layout. `cladding build`
builds local images, refreshes embedded tools, and initializes the persistent
project Baffle CA when needed. Later builds validate and reuse that CA.
`cladding check` and `cladding up` validate the same project prerequisites,
including the CA. They do not initialize or rotate persistent CA material.

`cladding run [--env KEY[=VALUE] ...] <command> [args...]` creates a unique
temporary runtime, runs the command in its agent container, and removes the
runtime when the command ends. Repeat `--env` to set multiple variables for
the command. Each invocation uses the selected project's config, tools,
credentials, and home directly. This preserves project symlinks and lets the proxy use the
selected Baffle session files and persistent CA. The command validates the CA;
`cladding build` creates it. Each run keeps generated scripts, masks, sockets,
containers, and volumes under UUID-scoped runtime identity. It does not copy
project config, tools, credentials, or home into the runtime directory. Run
requires a project initialized with `cladding init` and prepared with
`cladding build`.

`cladding exec [--target agent|nw-sandbox|fs-sandbox] [--env KEY[=VALUE] ...]
<command> [args...]` runs a command in an already-running project. It defaults
to `agent`; a sandbox target must be enabled. Direct host execution into either
sandbox bypasses the agent-side delegation and policy path. The `fs-sandbox` starts
in `/home/user` unless its configuration adds a workspace mount.

## Baffle policy and trust
`cladding init` creates native daemon and session TOML under
`.cladding/config/proxy/`. Baffle uses `file_only` mode. The agent and network
sandbox can read those non-secret configuration files, but the Baffle control
socket stays private inside the proxy container.

Each project has a persistent CA under `.cladding/credentials/baffle/`.
During the first successful `cladding build`, Cladding runs Baffle's
`ca init --config /opt/config/proxy/daemon.toml` in the selected proxy image.
Cladding validates and reuses the CA on later builds. `check` and `up` validate
the CA and fail when it is missing or invalid; `up` never initializes it.
After each agent and enabled network-sandbox container starts, Cladding uses
`podman exec --user 0` to install the public certificate in that container's
system trust store. This exec uses container root; the application remains
unprivileged. The default image sets `NODE_USE_SYSTEM_CA=1`. Custom images
must provide a compatible command to install the certificate. The filesystem
sandbox does not receive the CA.

Injection secrets are provisioned separately under
`.cladding/credentials/baffle/secrets/`. The proxy receives the credentials
directory read-only. Execution containers receive neither the secret files
nor the CA private key. Protect secret files as mode `0600` and the credentials
directories as mode `0700`; never commit these files.

The local application endpoint stays `http://127.0.0.1:3128`. Baffle accepts
HTTPS origins through `CONNECT` and rejects ordinary plaintext HTTP. Rules
use exact DNS hostnames and explicit ports. Baffle does not filter resolved
destination IP addresses. See the [proxy configuration reference](proxy/summary.md)
for migration limits and troubleshooting.

## Socket directories
Cladding creates a private runtime socket root and per-component subdirectories:

- `.cladding/runtime/sockets`
- `.cladding/runtime/sockets/proxy`
- `.cladding/runtime/sockets/agent/inject`
- `.cladding/runtime/sockets/proxy/agent`
- `.cladding/runtime/sockets/proxy/nw-sandbox`
- `.cladding/runtime/sockets/run/nw-sandbox`
- `.cladding/runtime/sockets/run/fs-sandbox`

On Linux, the proxy container mounts `.cladding/runtime/sockets/proxy` read/write for Baffle's scoped data sockets. On macOS with Podman machine, Baffle keeps those sockets inside the proxy container and a trusted `socat` process exposes each enabled session through a separate Podman-managed volume. Each volume overlays only its matching directory under `/run/cladding/proxy` in the proxy container and is mounted only into its matching execution container. This keeps the relay socket mode at `0600` and its parent at `0700`; the macOS shared host mount cannot provide those socket modes. In both modes, the agent and network sandbox mount only their own proxy session socket directories. The proxy's control socket and runtime configuration stay inside the proxy container. The sticky-mode `1733` `/run/baffle` directory contains a proxy-owned mode-`0700` subdirectory with mode-`0600` config and socket files; a symlink preserves Baffle's default control-socket path. The agent uses its proxy socket for outbound HTTP proxying and the sandbox run sockets when the corresponding sandboxes are enabled. The nw-sandbox and fs-sandbox containers bind their own run sockets via `MCP_BIND_UDS`.
`cladding inject` binds the agent inject socket under `/run/cladding/agent/inject` so a foreground command can reach one host endpoint for its duration.

Each execution container keeps its `socat` listener on `127.0.0.1:3128` and forwards to its own Baffle `proxy.sock`. Baffle owns a mode-`0600` socket inside a mode-`0700` component directory. The proxy and execution containers use ordinary `keep-id` mappings, so each process uses the invoking host user's UID to access the socket without widening its permissions. Baffle's mode-`0600` control socket and runtime configuration stay in a mode-`0700` directory inside the proxy container. Startup writes a private daemon-config copy with the proxy process UID as Baffle's trusted operator. No separate proxy bridge container is used.

CI checks Baffle's direct socket path with rootless Podman and its default OCI runtime, and with `runsc` for execution containers. It checks the trusted relay through the macOS Podman machine. Each check verifies the socket modes and sends an HTTPS request through the component's existing `127.0.0.1:3128` endpoint. The Podman-machine check uses the machine user's UID inside its containers and verifies access with the request itself; host and VM UID values are not compared.

## `use_runsc`
- `use_runsc` applies only to the standalone execution containers.
- The proxy stays on Podman's default runtime.
- Optional `use_runsc` design details live in `docs/features/cladding-gvisor-runtime/prd.md`.
- When `use_runsc` is enabled, Cladding passes `--runtime runsc`, `--runtime-flag ignore-cgroups`, `--runtime-flag host-uds=all`, and `--runtime-flag network=none` to the execution container startup command.
- `cladding expose` does not receive Podman runtime flags; it is a host-side `socat` forwarder that delegates through `cladding exec`.

## Blocking `cladding expose`
- `cladding expose <container-port> [host-port]` runs in the foreground on the host.
- It binds `127.0.0.1:<host-port>` by default, or the address selected with `--bind-address`, and forwards through `cladding exec socat ...` to `127.0.0.1:<container-port>` inside the agent container.
- No persistent expose containers are created.

## Blocking `cladding inject`
- `cladding inject <host-endpoint> [container-port]` runs in the foreground on the host.
- It mounts `/run/cladding/agent/inject` into the agent side and forwards the requested agent-local port to a host-reachable endpoint for the lifetime of that command.
- Bare ports resolve to host `localhost`; explicit `host:port` targets are temporary exceptions for that command.

## Config and scripts materialization
`cladding init` materializes the project layout under `.cladding`:

- `config/`
- `home/`
- `tools/`
- `runtime/`
- `runtime/empty-mask/`

The embedded config templates are copied into `config/`. The proxy startup
script is refreshed at `runtime/scripts/proxy_startup.sh` by `cladding init`,
`cladding build`, and `cladding up`. Embedded binaries are written into
`tools/bin/` by `cladding build`.

## Mounts
The proxy container receives read-only mounts for `/opt/config`,
`/opt/credentials/baffle`, `/opt/tools/bin/baffle`, and
`/opt/scripts/proxy_startup.sh`. It receives the scoped proxy socket root at
`/run/cladding/proxy` as a read/write mount. On Linux, Baffle binds its data
sockets there. On macOS with Podman machine, Baffle binds its data sockets
inside the proxy container and `socat` relays each enabled socket through a
component-specific Podman-managed volume. The proxy and matching execution
container share that volume; other execution containers do not receive it.
The control socket and runtime configuration always stay inside the container.
`/run/baffle` has sticky mode `1733`; the proxy process creates a mode-`0700`
child directory that owns the mode-`0600` runtime config and control socket. A
symlink at the default socket path keeps in-container Baffle commands working.
No execution container mounts `/run/baffle`.

Custom proxy images must provide a sticky, writable `/run/baffle` directory.
They must also include `socat` when Cladding runs on macOS with Podman machine.
The default proxy image provides both requirements.

The current runtime mounts the following built-in paths for the agent and
`nw-sandbox` where applicable:

- `/opt/config`
- `/run/cladding/ca/baffle.crt`
- `/opt/tools`
- `/home/user`
- `/home/user/workspace`
- `/home/user/workspace/.cladding` as a generated empty mask

The `fs-sandbox` default mount set is limited to read-only `/opt/config`,
read-only `/opt/tools`, and its internal run socket.

Custom mounts are applied through the direct runtime builder rather than through kube YAML.
