# PRD: Native Sandbox UDS and Single Proxy Bridge Sidecar

## Objective
Reduce Cladding's steady-state container count by removing all persistent sidecars except one proxy-pod bridge sidecar.

This combines two changes:

1. Add native Unix-domain-socket transport support to `mcp-run` and `run-remote`, removing the sandbox delegated-execution `socat` bridges.
2. Move proxy-client `socat` processes into the agent and nw-sandbox instance containers, while keeping one proxy-pod sidecar that serves both proxy identity sockets.

## Motivation
The UDS network isolation runtime gives Cladding a better security model than the previous nftables jailers, but it creates many containers.

With agent, nw-sandbox, and fs-sandbox enabled, the current runtime shape is:

```text
proxy pod:
  proxy instance
  agent-proxy socket sidecar
  nw-sandbox-proxy socket sidecar

agent pod:
  agent instance
  proxy-client sidecar
  nw-sandbox-run-client sidecar
  fs-sandbox-run-client sidecar

nw-sandbox pod:
  nw-sandbox instance
  proxy-client sidecar
  run-server sidecar

fs-sandbox pod:
  fs-sandbox instance
  run-server sidecar
```

That is 12 steady-state containers for the fully enabled runtime. Most of those containers exist only to run `socat`.

The desired runtime should keep the UDS capability model while making the process model easier to inspect:

```text
proxy pod:
  proxy instance
  proxy-bridge sidecar

agent pod:
  agent instance

nw-sandbox pod:
  nw-sandbox instance

fs-sandbox pod:
  fs-sandbox instance
```

That is 5 steady-state containers for the fully enabled runtime.

## Problem statement
The current `mcp-run` transport is TCP-only:

- `crates/mcp-run/src/mcp.rs` parses `MCP_BIND_ADDR` as a `SocketAddr`.
- `mcp-run` binds a `tokio::net::TcpListener`.
- `crates/mcp-run/src/remote.rs` requires `RUN_REMOTE_SERVER` to be an `http://` or `https://` URL and sends requests with `reqwest::Client::post()`.

Because of that TCP-only contract, Cladding needs bridge containers for agent-to-sandbox command execution:

- agent `nw-sandbox-run-client`: `127.0.0.1:3001 -> nw-sandbox-run.sock`
- agent `fs-sandbox-run-client`: `127.0.0.1:3002 -> fs-sandbox-run.sock`
- nw-sandbox `run-server`: `nw-sandbox-run.sock -> 127.0.0.1:3000`
- fs-sandbox `run-server`: `fs-sandbox-run.sock -> 127.0.0.1:3000`

The proxy path has a different constraint. Squid still uses TCP listeners and `myportname` ACLs to distinguish agent from nw-sandbox traffic:

```text
agent-proxy.sock -> Squid listener 127.0.0.1:3128 name=agent
nw-sandbox-proxy.sock -> Squid listener 127.0.0.1:3129 name=nw_sandbox
```

The proxy side still needs UDS-to-TCP bridging unless Squid itself is replaced or fronted by a different proxy. But it does not need one container per socket. A single proxy-pod sidecar can supervise both `socat` listener processes.

## Proposal
Add native UDS transport to `mcp-run` and `run-remote`, move proxy-client bridges into app instance containers, and consolidate proxy-side bridge listeners into one proxy sidecar.

### Target topology
For a fully enabled project:

```text
proxy pod:
  proxy instance:
    Squid listens on 127.0.0.1:3128 name=agent
    Squid listens on 127.0.0.1:3129 name=nw_sandbox
  proxy-bridge sidecar:
    /run/cladding/proxy/agent/proxy.sock -> 127.0.0.1:3128
    /run/cladding/proxy/nw-sandbox/proxy.sock -> 127.0.0.1:3129

agent pod:
  agent instance:
    socat 127.0.0.1:3128 -> /run/cladding/proxy/agent/proxy.sock
    run-remote talks directly to sandbox UDS sockets

nw-sandbox pod:
  nw-sandbox instance:
    mcp-run listens on /run/cladding/run/nw-sandbox/run.sock
    socat 127.0.0.1:3128 -> /run/cladding/proxy/nw-sandbox/proxy.sock

fs-sandbox pod:
  fs-sandbox instance:
    mcp-run listens on /run/cladding/run/fs-sandbox/run.sock
```

`fs-sandbox` still receives no proxy environment variables and no proxy socket.

### Socket scoping
Do not mount one shared socket directory into every app container.

Once bridge clients run inside instance containers, the mounted socket paths become directly reachable by arbitrary processes in those containers. A broad shared mount would let the agent connect to `nw-sandbox`'s proxy identity socket, or let nw-sandbox connect to unrelated runtime sockets.

Use per-component socket directories:

```text
.cladding/runtime/sockets/proxy/agent/proxy.sock
.cladding/runtime/sockets/proxy/nw-sandbox/proxy.sock
.cladding/runtime/sockets/run/nw-sandbox/run.sock
.cladding/runtime/sockets/run/fs-sandbox/run.sock
```

Mount only the paths each component needs:

- agent instance gets the agent proxy socket directory and enabled sandbox run socket directories
- nw-sandbox instance gets the nw-sandbox proxy socket directory and its own run socket directory
- fs-sandbox instance gets only its own run socket directory
- proxy-bridge sidecar gets the agent and nw-sandbox proxy socket directories

Do not mount `.cladding/runtime/sockets/proxy` or `.cladding/runtime/sockets/run` wholesale into app containers.

Do not bind mount the socket files directly in the first implementation. Direct socket-file mounts are fragile for this use case:

- the source socket usually does not exist when Cladding creates the container;
- startup ordering would need to create listener sockets before creating clients;
- if a listener unlinks and recreates its socket, a file bind mount can continue pointing at the old socket inode;
- stale-socket cleanup becomes harder to reason about.

Mount the per-component directory that contains exactly one socket instead. This preserves scoped access while allowing the listener process to create, remove, and recreate the socket path inside that directory.

### Native sandbox execution sockets
The sandbox servers should bind directly to mounted socket paths:

```text
nw-sandbox:
  MCP_BIND_UDS=/run/cladding/run/nw-sandbox/run.sock

fs-sandbox:
  MCP_BIND_UDS=/run/cladding/run/fs-sandbox/run.sock
```

The agent wrappers should call `run-remote` directly over those socket paths.

Recommended internal wrapper environment:

```text
RUN_NW_SANDBOX_SOCKET=/run/cladding/run/nw-sandbox/run.sock
RUN_FS_SANDBOX_SOCKET=/run/cladding/run/fs-sandbox/run.sock
```

The embedded `run-in-nw-sandbox` and `run-in-fs-sandbox` scripts should translate the component-specific variables into `RUN_REMOTE_SOCKET` before executing `run-remote`.

### `mcp-run` server configuration
Keep `MCP_BIND_ADDR` for TCP development and tests.

Add one UDS-specific server configuration variable:

```text
MCP_BIND_UDS=/run/cladding/run/nw-sandbox/run.sock
```

Semantics:

- if `MCP_BIND_UDS` is set, bind the HTTP app to that Unix socket
- if `MCP_BIND_UDS` is unset, keep the existing `MCP_BIND_ADDR` TCP behavior
- setting both `MCP_BIND_ADDR` and `MCP_BIND_UDS` is an error
- the socket path must be absolute
- the parent directory must already exist
- stale socket files may be removed only when they are Unix sockets
- stale non-socket files at the target path must fail closed
- socket permissions should be restrictive enough that access is controlled by Cladding's mounted socket directories

`mcp-run` should serve the same routes over either transport:

- `/mcp`
- `/raw`
- `/check`

### `run-remote` client configuration
Keep `RUN_REMOTE_SERVER=http://...` support for standalone TCP use.

Add UDS support to the low-level client path in `crates/mcp-run/src/remote.rs`.

Recommended environment:

```text
RUN_REMOTE_SOCKET=/run/cladding/run/nw-sandbox/run.sock
```

Semantics:

- if `RUN_REMOTE_SOCKET` is set, send HTTP requests over that Unix socket
- if `RUN_REMOTE_SOCKET` is unset, keep existing `RUN_REMOTE_SERVER` TCP URL behavior
- setting both `RUN_REMOTE_SOCKET` and `RUN_REMOTE_SERVER` is an error
- socket paths must be absolute
- default execution sends to `/raw`
- `run-remote --check` sends to `/check`
- payload construction, cwd forwarding, env forwarding, streaming output, and exit-code behavior remain unchanged

Examples:

```bash
RUN_REMOTE_SOCKET=/run/cladding/run/nw-sandbox/run.sock run-remote --check -- true
RUN_REMOTE_SOCKET=/run/cladding/run/nw-sandbox/run.sock run-remote -- true
RUN_REMOTE_SERVER=http://127.0.0.1:8000/raw run-remote -- true
```

### In-instance proxy clients
Move the agent and nw-sandbox proxy-client bridges into their instance containers.

Agent instance startup should start:

```text
socat TCP-LISTEN:3128,bind=127.0.0.1,fork,reuseaddr \
  UNIX-CONNECT:/run/cladding/proxy/agent/proxy.sock
```

Nw-sandbox instance startup should start:

```text
socat TCP-LISTEN:3128,bind=127.0.0.1,fork,reuseaddr \
  UNIX-CONNECT:/run/cladding/proxy/nw-sandbox/proxy.sock
```

The app-facing proxy environment remains unchanged:

```text
http_proxy=http://127.0.0.1:3128
https_proxy=http://127.0.0.1:3128
HTTP_PROXY=http://127.0.0.1:3128
HTTPS_PROXY=http://127.0.0.1:3128
```

Startup command requirements:

- start required `socat` bridges in the background
- fail the container if a required bridge exits early
- forward `SIGTERM` and `SIGINT` to background bridge processes
- `exec` the primary foreground command where possible

For the agent, the primary foreground command remains the idle shell/sleep behavior.

For nw-sandbox, the primary foreground command is `mcp-run`.

### Single proxy bridge sidecar
Replace the two proxy-pod listener sidecars with one proxy-pod sidecar.

The sidecar should start and supervise one `socat` listener per enabled proxy identity:

```text
UNIX-LISTEN:/run/cladding/proxy/agent/proxy.sock,fork,reuseaddr
  -> TCP:127.0.0.1:3128

UNIX-LISTEN:/run/cladding/proxy/nw-sandbox/proxy.sock,fork,reuseaddr
  -> TCP:127.0.0.1:3129
```

If nw-sandbox is disabled, the sidecar should only start the agent listener.

The sidecar should fail if any required listener cannot bind or exits unexpectedly. This keeps proxy startup fail-closed rather than silently dropping an identity path.

### Image and binary dependencies
The default Cladding app image should include `socat`, because the agent and nw-sandbox instance containers will run proxy-client bridges directly.

Update `Containerfile.cladding` to install `socat`.

Custom agent and nw-sandbox images must provide `socat` if they enable proxy access through the standard Cladding runtime. `cladding check` should either:

- document this requirement and rely on clear startup failure logs, or
- actively verify `command -v socat` in custom images.

The second option gives better diagnostics but makes `cladding check` depend on local image availability and temporary container execution. A documented requirement plus clear startup logs is acceptable for the first implementation.

Native sandbox execution does not require `socat`; it only requires the mounted `mcp-run`, `run-remote`, and wrapper binaries under `/opt/tools/bin`.

### Cladding runtime changes
Update `cladding/src/runtime.rs`:

- remove agent run-client bridge containers
- remove sandbox run-server bridge containers
- remove agent proxy-client bridge container
- remove nw-sandbox proxy-client bridge container
- replace the two proxy listener sidecars with one proxy bridge sidecar
- mount scoped proxy socket directories into agent, nw-sandbox, and proxy bridge containers
- mount scoped run socket directories into agent and enabled sandbox containers
- set `MCP_BIND_UDS` for sandbox instances
- stop setting `MCP_BIND_ADDR` for Cladding-managed sandbox instances
- set `RUN_NW_SANDBOX_SOCKET` and `RUN_FS_SANDBOX_SOCKET` for the agent instance

Keep Squid config and listener identity semantics unchanged.

### Container-count target
After this proposal lands, a fully enabled runtime should have this steady-state shape:

```text
proxy pod: 2 containers
agent pod: 1 container
nw-sandbox pod: 1 container
fs-sandbox pod: 1 container
total: 5 containers
```

`cladding expose` may still create temporary or user-managed helper containers. Those are outside the steady-state runtime count.

## Non-goals
1. Do not remove Podman pods.
2. Do not replace Squid or change Squid listener-based identity.
3. Do not give `fs-sandbox` proxy access.
4. Do not change Rego policy semantics.
5. Do not require all non-Cladding `mcp-run` users to use UDS; TCP remains supported.
6. Do not add authentication to `mcp-run` endpoints in this proposal.
7. Do not redesign `cladding expose`.
8. Do not create or manage a custom proxy image.

## Suggested implementation shape
1. Add UDS listener support to `crates/mcp-run/src/mcp.rs`.
2. Add UDS request support to `crates/mcp-run/src/remote.rs`.
3. Add tests for UDS `/raw` and `/check` behavior.
4. Update `crates/mcp-run/README.md` with TCP and UDS examples for `run-remote`.
5. Update embedded `run-in-nw-sandbox` and `run-in-fs-sandbox` wrappers in `cladding/src/assets.rs`.
6. Add `socat` to `Containerfile.cladding`.
7. Add generated startup commands or scripts for agent and nw-sandbox instances to run proxy-client `socat`.
8. Add a single proxy bridge sidecar command that supervises both proxy UDS listeners.
9. Update `cladding/src/runtime.rs` mounts, env vars, container specs, and tests.
10. Update README diagrams and runtime docs.

## Migration plan
Existing project configuration should not need to change.

Users should run:

```bash
cladding build
cladding down
cladding up
```

`cladding build` is required because the embedded `mcp-run`, `run-remote`, wrapper scripts, and default app image contents change.

Existing config files remain valid:

- `agent/domains.lst`
- `agent/host_ports.lst`
- `nw_sandbox/domains.lst`
- `nw_sandbox/*.rego`
- `fs_sandbox/*.rego`
- `proxy/squid.conf`

Projects using custom agent or nw-sandbox images must ensure those images contain `socat`.

## Verification
For `mcp-run`:

- `cargo test -p mcp-run` covers TCP and UDS server modes.
- `/raw` over UDS streams the same event format as TCP.
- `/check` over UDS returns the same decision JSON as TCP.
- stale non-socket files at `MCP_BIND_UDS` fail closed.
- setting both `MCP_BIND_ADDR` and `MCP_BIND_UDS` is rejected.
- setting both `RUN_REMOTE_SERVER` and `RUN_REMOTE_SOCKET` is rejected.
- `run-remote --check` works against `RUN_REMOTE_SOCKET`.
- `run-remote` execution works against `RUN_REMOTE_SOCKET`.

For Cladding:

- `cladding up` no longer creates agent run-client bridge containers.
- `cladding up` no longer creates sandbox run-server bridge containers.
- `cladding up` no longer creates agent or nw-sandbox proxy-client sidecar containers.
- `cladding up` creates exactly one proxy bridge sidecar.
- inside agent, `run-in-nw-sandbox --check -- true` works.
- inside agent, `run-in-fs-sandbox --check -- true` works when fs-sandbox is enabled.
- agent proxy access still uses `127.0.0.1:3128`.
- nw-sandbox proxy access still uses `127.0.0.1:3128`.
- fs-sandbox has no proxy env vars and no proxy socket mount.
- the agent cannot connect to the nw-sandbox proxy identity socket through mounted paths.
- nw-sandbox cannot connect to the agent proxy identity socket through mounted paths.
- Squid still distinguishes agent and nw-sandbox by listener identity.
- a fully enabled project has 5 steady-state containers.
- `cladding down` removes all pods and runtime socket files.

## Success criteria
1. Native `mcp-run` UDS support removes the sandbox run bridge sidecars without changing `/raw` or `/check` semantics.
2. Agent and nw-sandbox proxy-client bridges run inside their instance containers.
3. One proxy-pod sidecar handles all enabled proxy identity socket listeners.
4. Socket mounts are scoped so containers cannot use another component's proxy identity.
5. A fully enabled runtime is reduced from 12 steady-state containers to 5.
6. Existing project config remains valid.
7. Failures are diagnosable from `cladding check`, container startup logs, or `cladding logs`.
