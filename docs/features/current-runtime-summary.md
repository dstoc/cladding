# Current Runtime Summary

This file is the quick reference for the current Cladding runtime.

## Managed resources
- One proxy Podman pod per project: `<name>-proxy`.
- One proxy instance container inside that pod: `<name>-proxy-instance`.
- The proxy instance runs one Baffle daemon and creates the persistent agent session and, when enabled, the network-sandbox session.
- Standalone execution containers for `<name>-agent`, `<name>-nw-sandbox`, and `<name>-fs-sandbox` when those components are enabled.
- Container names follow the `<pod-name>-instance` pattern for the execution containers.

## Runtime shape
- The proxy is the only Podman pod in the current design.
- The proxy pod uses Podman's default runtime.
- The agent, nw-sandbox, and fs-sandbox are standalone containers, not pods.
- Execution containers run with `--network none`.
- Execution containers communicate through scoped Unix-domain socket mounts under `.cladding/runtime/sockets`.

## Socket directories
Cladding creates a private runtime socket root and per-component subdirectories:

- `.cladding/runtime/sockets`
- `.cladding/runtime/sockets/proxy`
- `.cladding/runtime/sockets/agent/inject`
- `.cladding/runtime/sockets/proxy/agent`
- `.cladding/runtime/sockets/proxy/control`
- `.cladding/runtime/sockets/proxy/nw-sandbox`
- `.cladding/runtime/sockets/run/nw-sandbox`
- `.cladding/runtime/sockets/run/fs-sandbox`

The proxy container mounts `.cladding/runtime/sockets/proxy` read/write and mounts its `control` directory separately at `/run/baffle`. The agent and network sandbox mount only their own proxy session socket directories. The agent uses its proxy socket for outbound HTTP proxying and the sandbox run sockets when the corresponding sandboxes are enabled. The nw-sandbox and fs-sandbox containers bind their own run sockets via `MCP_BIND_UDS`.
`cladding inject` binds the agent inject socket under `/run/cladding/agent/inject` so a foreground command can reach one host endpoint for its duration.

Each execution container keeps its `socat` listener on `127.0.0.1:3128` and forwards to its own Baffle `proxy.sock`. Baffle owns a mode-`0600` socket inside a mode-`0700` component directory. The proxy and execution containers use ordinary `keep-id` mappings, so each process uses the invoking host user's UID to access the socket without widening its permissions. The proxy receives a separate mode-`0700` control directory; execution containers do not mount it. Startup writes a private daemon-config copy with the proxy process UID as Baffle's trusted operator. No separate proxy bridge container is used.

CI checks this direct socket path with rootless Podman and its default OCI runtime, with `runsc` for execution containers, and through the macOS Podman machine. Each check verifies the socket modes and sends an HTTPS request through the component's existing `127.0.0.1:3128` endpoint. The Podman-machine check uses the machine user's UID inside its containers and verifies access with the request itself; host and VM UID values are not compared.

## `use_runsc`
- `use_runsc` applies only to the standalone execution containers.
- The proxy pod stays on Podman's default runtime.
- Optional `use_runsc` design details live in `docs/features/cladding-gvisor-runtime/prd.md`.
- When `use_runsc` is enabled, Cladding passes `--runtime runsc`, `--runtime-flag ignore-cgroups`, `--runtime-flag host-uds=all`, and `--runtime-flag network=none` to the execution container startup command.
- `cladding expose` does not receive Podman runtime flags; it is a host-side `socat` forwarder that delegates through `cladding run`.

## Blocking `cladding expose`
- `cladding expose <container-port> [host-port]` runs in the foreground on the host.
- It binds `127.0.0.1:<host-port>` by default, or the address selected with `--bind-address`, and forwards through `cladding run socat ...` to `127.0.0.1:<container-port>` inside the agent container.
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
`/run/cladding/proxy` as a read/write mount and a separate, mode-`0700`
host-backed control directory at `/run/baffle`. Only the proxy container mounts
the control directory.

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
