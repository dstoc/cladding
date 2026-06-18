# PRD: Blocking `cladding inject`

## Objective
Add a blocking inverse of `cladding expose` that lets processes inside the agent reach one explicitly selected host-reachable TCP endpoint through agent localhost.

The command should be:

```bash
cladding inject <host-endpoint> [container-port]
```

It starts a foreground bridge where `127.0.0.1:<container-port>` inside the agent connects to `<host-endpoint>` from the host. Bare port endpoints default to host localhost. The user stops the inject session with Ctrl-C.

## Motivation
Some host tools expose useful local-only services, for example:

- a model server on `127.0.0.1:11434`
- a development API on `127.0.0.1:8080`
- a database tunnel on `127.0.0.1:15432`
- a service on another host-reachable address, such as `192.168.1.50:5432`

The agent runs as a standalone `--network none` container. That is the right default, but it means agent processes cannot treat host localhost as their own localhost. `cladding expose` solves the opposite direction by publishing an agent-local port on host localhost while a foreground command is running.

`cladding inject` should provide the same operational model in the other direction:

1. start or identify the host-reachable service;
2. run `cladding inject <host-endpoint> [container-port]`;
3. agent processes connect to `127.0.0.1:<container-port>`;
4. press Ctrl-C when done.

This keeps host-mediated access explicit, temporary, and scoped to one TCP endpoint.

## Problem statement
The obvious mirror of `cladding expose` does not work.

`cladding expose` can run one host listener and start a fresh `cladding run socat ...` command for each host-side connection:

```text
host client
  -> host socat TCP-LISTEN:<host-port>
  -> cladding run socat STDIO TCP:127.0.0.1:<container-port>
  -> agent-local service
```

For the inverse direction, the listener must be inside the agent:

```text
agent client
  -> 127.0.0.1:<container-port> inside agent
  -> bridge back to host
  -> <host-endpoint> from the host
```

The agent cannot call `cladding run`; only the host can. A single `podman exec` stdio stream is also not a good multiplexing boundary for a forked agent-side TCP listener. If multiple accepted agent connections share one `STDIO` endpoint, their byte streams are not isolated.

The implementation needs a bridge that gives each accepted agent connection a separate stream to a host process, without giving the agent general host networking and without changing the running container's Podman network mode.

## Proposal
Add `cladding inject` as a blocking command managed entirely by the host.

### CLI
Add a top-level command:

```bash
cladding inject <host-endpoint> [container-port]
```

Semantics:

- `<host-endpoint>` is either `<port>` or `<host>:<port>`.
- a bare `<port>` means `127.0.0.1:<port>` from the host.
- `<host>:<port>` connects to that exact host and TCP port from the host.
- `[container-port]` is the TCP port to listen on inside the agent at `127.0.0.1`.
- if `[container-port]` is omitted, use the same numeric port as the host endpoint port.
- host and container ports are validated as `u16` in the range `1..=65535`.
- hostnames must be non-empty and limited to a conservative first-pass character set such as ASCII letters, digits, `.`, `_`, and `-`; IPv4 literals are accepted by the same rule.
- reject host endpoint strings containing `:`, `,`, whitespace, or other `socat` address syntax characters outside the single separator between host and port.
- IPv6 literals are out of scope for the first implementation unless a bracketed parser is added explicitly.
- bind only to `127.0.0.1` inside the agent.
- connect only to the requested host endpoint from the host-side bridge.
- do not search for alternate ports.
- if either listener cannot bind or the host endpoint cannot be reached, surface the underlying `socat` or Podman error.
- the command runs in the foreground until the user interrupts it.

Example:

```bash
cladding inject 11434
```

lets an agent process connect to `127.0.0.1:11434` and reach host `127.0.0.1:11434` while the command is running.

Example with distinct ports:

```bash
cladding inject 5432 15432
```

maps agent `127.0.0.1:15432` to host `127.0.0.1:5432`.

Example with an explicit host:

```bash
cladding inject db.internal:5432 15432
```

maps agent `127.0.0.1:15432` to `db.internal:5432` as resolved and connected by the host.

### Bridge model
Use a scoped runtime Unix-domain socket as the per-connection handoff between the agent listener and host listener.

The agent runtime should include a dedicated inject socket mount:

```text
host:  .cladding/runtime/sockets/agent/inject
agent: /run/cladding/agent/inject
```

This keeps inject sockets with the rest of Cladding's generated runtime socket state instead of placing control sockets in the user's home directory or workspace.

In `cladding/src/runtime.rs`, this should use the existing `build_scoped_socket_mount` path rather than a custom mount implementation:

```text
socket dir:  agent/inject
mount path:  /run/cladding/agent/inject
```

`cladding inject` should create a deterministic per-container-port directory under the host runtime socket directory, for example:

```text
.cladding/runtime/sockets/agent/inject/<container-port>/
```

and use the matching agent path:

```text
/run/cladding/agent/inject/<container-port>/host.sock
```

Then run two foreground-managed processes:

1. Host-side socket bridge:

   ```bash
   socat UNIX-LISTEN:<host-socket>,fork,reuseaddr TCP:<host>:<host-port>
   ```

2. Agent-side localhost listener, started from the host with non-interactive `podman exec` or an equivalent `cladding run` helper:

   ```bash
   socat TCP-LISTEN:<container-port>,bind=127.0.0.1,fork,reuseaddr \
     UNIX-CONNECT:<agent-socket>
   ```

Connection flow:

```text
agent client
  -> 127.0.0.1:<container-port> inside agent
  -> agent socat TCP-LISTEN
  -> UNIX-CONNECT:/run/cladding/agent/inject/.../host.sock
  -> host socat UNIX-LISTEN
  -> TCP:<host>:<host-port> from the host
```

Both `socat` commands use `fork`, so each accepted connection receives its own socket stream. There is no multiplexing over one `podman exec` stdio stream.

### Runtime state
`cladding inject` should not create Podman containers, labels, or project-owned detached mappings.

The runtime should create the parent socket mount during `cladding up`, alongside the existing scoped socket directories. The `cladding inject` command may create a temporary per-port directory and Unix socket under `.cladding/runtime/sockets/agent/inject` while it is running. On normal exit or Ctrl-C, it should remove the socket path and its per-port directory.

At most one active inject session may bind a given container port. If `.cladding/runtime/sockets/agent/inject/<container-port>/host.sock` already exists, or if the agent-side listener cannot bind `127.0.0.1:<container-port>`, `cladding inject` should fail with a clear error instead of choosing another path or port.

Because the session is foreground-owned, there should be no `cladding inject list` or `cladding inject stop` in the first implementation. Ctrl-C is the stop operation.

`cladding down` and `cladding destroy` do not need to know about active inject sessions. If those commands stop the agent while `cladding inject` is running, the inject command should fail when its child process exits.

### Project and container validation
`cmd_inject` should mirror the current `cmd_expose` preflight checks in `cladding/src/cli.rs`:

1. require `podman`;
2. require host `socat` with an inject-specific missing-dependency message;
3. load `cladding.json`;
4. verify the current project is running with `project_runtime_status`;
5. verify the agent container exists with `podman_container_exists`;
6. verify `/run/cladding/agent/inject` exists inside the agent container;
7. fail if an inject socket path already exists for the requested container port;
8. create or reuse the per-port directory after verifying the socket path itself is absent;
9. start the host-side `socat`;
10. start the agent-side `socat`;
11. wait for either child to exit, then terminate the other child, remove the agent-side listener if needed, and return an appropriate status.

The agent image must also contain `socat`, as it already must for `cladding expose` and the default proxy bridge behavior.

### Command construction
The implementation should keep all shell-sensitive construction in small tested helpers.

Suggested helpers:

```rust
struct InjectHostEndpoint {
    host: String,
    port: u16,
}

fn parse_inject_host_endpoint(raw: &str) -> Result<InjectHostEndpoint>

fn build_inject_host_bridge_command(host_socket: &Path, endpoint: &InjectHostEndpoint) -> Command

fn build_inject_agent_listener_args(agent_socket: &str, container_port: u16) -> Vec<String>
```

The host bridge should produce:

```text
socat
UNIX-LISTEN:<host-socket>,fork,reuseaddr
TCP:<host>:<host-port>
```

The agent listener args should produce:

```text
socat
TCP-LISTEN:<container-port>,bind=127.0.0.1,fork,reuseaddr
UNIX-CONNECT:<agent-socket>
```

The endpoint parser should normalize bare ports to host `127.0.0.1`. Host strings are user input and must be validated before they are interpolated into `socat` address strings. Validation should protect `socat` address parsing specifically, not only shell parsing, because the implementation passes address strings directly as process arguments. Socket paths should be generated by Cladding from the project root and validated container port.

This syntax is intentionally an explicit temporary network exception. The agent still only sees `127.0.0.1:<container-port>`, but the host-side bridge can connect to any host endpoint the host can reach.

### Child process management
Unlike `cladding expose`, `cladding inject` needs to supervise two long-lived children.

The implementation should prefer explicit child management over composing everything into one shell string:

- spawn the host-side `socat` with `Command`;
- spawn the agent-side process through a reusable non-interactive agent exec helper;
- install SIGINT and SIGTERM handling;
- when a signal arrives or either child exits, terminate both children;
- after terminating the `podman exec` child, run a best-effort agent-side cleanup such as `podman exec <agent> pkill -f <exact inject listener pattern>` so the `socat TCP-LISTEN` process is not left running in the agent;
- remove the temporary socket directory after children have exited.

The first implementation can use normal `socat` behavior for forked per-connection children. It does not need to track each forked connection process individually.

### Interaction with `use_runsc`
`cladding inject` should not receive Podman runtime flags. It operates against an already-running agent container through `podman exec`, matching `cladding expose`.

When `use_runsc` is enabled, the feature depends on the existing runtime requirement that host-mounted Unix-domain sockets work for execution containers. The current gVisor support passes `--runtime-flag host-uds=all` when creating standalone execution containers, so the mounted socket bridge should remain compatible with that runtime shape.

Projects must be restarted with `cladding up` after upgrading to a version that adds the inject mount. If an older running agent container does not have `/run/cladding/agent/inject`, `cladding inject` should fail with a clear hint to run `cladding down` and `cladding up`.

### Documentation
Update `README.md` to include:

```bash
cladding inject <host-endpoint> [containerport] # block while forwarding agent localhost containerport to a host-reachable endpoint
```

The usage section should state that `cladding inject` runs in the foreground and is stopped with Ctrl-C.

The docs should avoid describing this as general host networking. It exposes one requested TCP endpoint to the agent for the lifetime of one foreground command. Bare ports use host localhost; explicit `host:port` endpoints are broader and should be described as a deliberate temporary network exception.

## Non-goals
1. Do not give the agent general access to the host network.
2. Do not use `--network host` or change the agent container network mode.
3. Do not create persistent inject mappings.
4. Do not add `cladding inject list` or `cladding inject stop`.
5. Do not add automatic port selection.
6. Do not support injecting into nw-sandbox or fs-sandbox in the first implementation.
7. Do not use the project workspace `.cladding` path inside the agent; it is intentionally masked.
8. Do not implement a generic TCP proxy framework before this concrete command needs it.

## Suggested implementation shape
1. Add `CommandSpec::Inject(InjectArgs)` in `cladding/src/cli.rs`.
2. Add `InjectArgs` with `HOST_ENDPOINT` and optional `CONTAINERPORT`.
3. Extract shared project-running and agent-container checks from `cmd_expose` if that keeps `cmd_expose` and `cmd_inject` small.
4. Add a runtime socket mount for the agent in `cladding/src/runtime.rs` using `build_scoped_socket_mount`:
   - host path: `.cladding/runtime/sockets/agent/inject`
   - agent path: `/run/cladding/agent/inject`
5. Ensure `RuntimeSpec::generated_runtime_socket_dirs` includes the new directory through the existing scoped socket mount collection.
6. Add helpers to derive:
   - host inject root: `.cladding/runtime/sockets/agent/inject`
   - host socket path: `.cladding/runtime/sockets/agent/inject/<container-port>/host.sock`
   - agent socket path: `/run/cladding/agent/inject/<container-port>/host.sock`
7. Add `build_inject_host_bridge_command`.
8. Add `parse_inject_host_endpoint`.
9. Add `build_inject_agent_listener_args`.
10. Add a non-interactive agent exec spawn helper so `cmd_inject` can supervise the agent-side child instead of calling a blocking `cmd_run`.
    - It should build `podman exec -i <agent-container> socat ...` directly or through a small lower-level helper.
    - It should not use the current blocking `run_podman_exec` path, because that path owns signal handling and waits for the child internally.
11. Add signal handling and paired child cleanup for `cmd_inject`.
12. Make `socat_required` command-aware, or add an inject-specific wrapper, so missing host `socat` does not print "required for cladding expose" during `cladding inject`.
13. Remove the per-port socket directory on exit.
14. Update README usage and current runtime summary.

## Verification
Unit tests should cover:

- `cladding inject 11434` parses as host `127.0.0.1`, host port `11434`, and container port `11434`;
- `cladding inject 5432 15432` parses as host `127.0.0.1`, host port `5432`, and container port `15432`;
- `cladding inject db.internal:5432 15432` parses as host `db.internal`, host port `5432`, and container port `15432`;
- `cladding inject` without ports fails;
- `cladding inject list` and `cladding inject stop <port>` fail to parse;
- invalid port values fail to parse;
- invalid host endpoint strings fail to parse;
- host endpoint strings containing `socat` address delimiters or whitespace fail to parse;
- omitted container port defaults to the host endpoint port;
- generated host socket paths stay under `.cladding/runtime/sockets/agent/inject`;
- generated agent socket paths stay under `/run/cladding/agent/inject`;
- generated socket paths use the requested container port as the only dynamic path component;
- existing socket paths for the requested container port are treated as conflicts;
- the agent runtime spec includes the inject socket mount;
- host bridge command binds `UNIX-LISTEN` to the generated socket and connects to `TCP:<host>:<host-port>`;
- agent listener args bind `TCP-LISTEN:<container-port>,bind=127.0.0.1` and connect to the generated Unix socket;
- cleanup invokes an agent-side listener removal path when the supervised `podman exec` process is terminated;
- command construction does not include Podman runtime flags.

Manual integration verification should cover:

1. Start a host service:

   ```bash
   python3 -m http.server 18080 --bind 127.0.0.1
   ```

2. Start a project with `cladding up`.
3. Run:

   ```bash
   cladding inject 18080 8080
   ```

4. In another terminal, verify from the agent:

   ```bash
   cladding run curl -sS http://127.0.0.1:8080/
   ```

5. Press Ctrl-C in the `cladding inject` terminal.
6. Verify the agent connection fails after the inject command exits.
7. Verify no Podman containers were created for inject.
8. Verify the per-port directory under `.cladding/runtime/sockets/agent/inject` was removed.
9. Repeat the same flow with `use_runsc: true` on a host that supports the existing runsc setup.
10. Repeat with an explicit host endpoint, such as a LAN address or test hostname reachable from the host, and verify the agent can reach it only while `cladding inject` is running.

## Success criteria
1. `cladding inject <host-endpoint> [container-port]` starts a foreground bridge from agent localhost to the requested host-reachable endpoint.
2. Agent processes can connect to the injected container port while the command is running.
3. Ctrl-C stops both host-side and agent-side bridge processes.
4. The feature does not create persistent containers, labels, or detached mappings.
5. Existing `cladding expose`, `cladding run`, `cladding down`, and `cladding destroy` behavior is unchanged.
