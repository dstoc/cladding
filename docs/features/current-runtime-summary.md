# Current Runtime Summary

This file is the quick reference for the current Cladding runtime.

## Managed resources
- One proxy Podman pod per project: `<name>-proxy`.
- One proxy instance container inside that pod: `<name>-proxy-instance`.
- One proxy bridge sidecar inside that pod: `<name>-proxy-bridge`.
- Standalone execution containers for `<name>-agent`, `<name>-nw-sandbox`, and `<name>-fs-sandbox` when those components are enabled.
- Container names follow the `<pod-name>-instance` pattern for the execution containers.

## Runtime shape
- The proxy is the only Podman pod in the current design.
- The proxy pod uses Podman's default runtime.
- The agent, nw-sandbox, and fs-sandbox are standalone containers, not pods.
- Execution containers run with `--network none`.
- Execution containers communicate through scoped Unix-domain socket mounts under `.cladding/runtime/sockets`.

## Socket directories
Cladding creates one root runtime socket directory and per-component subdirectories:

- `.cladding/runtime/sockets`
- `.cladding/runtime/sockets/agent/inject`
- `.cladding/runtime/sockets/proxy/agent`
- `.cladding/runtime/sockets/proxy/nw-sandbox`
- `.cladding/runtime/sockets/run/nw-sandbox`
- `.cladding/runtime/sockets/run/fs-sandbox`

The proxy bridge sidecar uses the proxy socket directories. The agent uses the proxy socket for outbound HTTP proxying and the sandbox run sockets when the corresponding sandboxes are enabled. The nw-sandbox and fs-sandbox containers bind their own run sockets via `MCP_BIND_UDS`.
`cladding inject` binds the agent inject socket under `/run/cladding/agent/inject` so a foreground command can reach one host endpoint for its duration.

## `use_runsc`
- `use_runsc` applies only to the standalone execution containers.
- The proxy pod and proxy bridge stay on the default runtime.
- Optional `use_runsc` design details live in `docs/features/cladding-gvisor-runtime/prd.md`.
- When `use_runsc` is enabled, Cladding passes `--runtime runsc`, `--runtime-flag ignore-cgroups`, `--runtime-flag host-uds=all`, and `--runtime-flag network=none` to the execution container startup command.
- `cladding expose` does not receive Podman runtime flags; it is a host-side `socat` forwarder that delegates through `cladding run`.

## Blocking `cladding expose`
- `cladding expose <container-port> [host-port]` runs in the foreground on the host.
- It binds `127.0.0.1:<host-port>` and forwards through `cladding run socat ...` to `127.0.0.1:<container-port>` inside the agent container.
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

The embedded config templates are copied into `config/`. Generated runtime
scripts are refreshed under `runtime/scripts/` by `cladding up`, and embedded
binaries are written into `tools/bin/` by `cladding build`.

## Mounts
The current runtime mounts the following built-in paths for the agent and
`nw-sandbox` where applicable:

- `/opt/config`
- `/opt/scripts` on the proxy pod, sourced from `.cladding/runtime/scripts`
- `/opt/tools`
- `/home/user`
- `/home/user/workspace`
- `/home/user/workspace/.cladding` as a generated empty mask

The `fs-sandbox` default mount set is limited to read-only `/opt/config`,
read-only `/opt/tools`, and its internal run socket.

Custom mounts are applied through the direct runtime builder rather than through kube YAML.
