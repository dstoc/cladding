# Cladding Bridge: Multiplexed transport for `cladding inject`

## Summary

Replace the current `cladding inject` host-shared Unix-domain-socket transport with a Cladding-owned multiplexed stream carried over a single `podman exec` stdin/stdout connection.

The change is intentionally narrow:

- make `cladding inject` work cleanly with both local Podman and Podman Machine;
- remove its dependence on host-shared Unix sockets;
- remove its dependence on `socat`;
- support multiple concurrent connections through one long-lived `podman exec`;
- preserve the existing user-facing `cladding inject` semantics.

This project does **not** replace `socat` elsewhere in Cladding. In particular, the existing execution-container `127.0.0.1:3128 -> Baffle UDS` bridges remain unchanged.

The bridge may later be reusable by `cladding expose`, but expose is not part of this proposal.

## Context

Today `cladding inject` temporarily makes a host-reachable TCP endpoint available inside the agent container.

For example:

```text
cladding inject localhost:5432 15432
```

allows software in the agent to connect to `127.0.0.1:15432` and have that connection forwarded to host `localhost:5432`.

The current implementation uses two `socat` processes and a host-backed Unix socket:

```text
agent application
       |
       v
127.0.0.1:<container-port>
       |
       v
agent socat
       |
       v
/run/cladding/agent/inject/<port>/host.sock
       |
       | host bind mount
       v
host socat
       |
       v
<host>:<host-port>
```

This is reasonable for local Podman, but the host-backed UDS becomes problematic with Podman Machine:

- the agent is inside a VM;
- the Unix socket lives on the physical host;
- exposing a host Unix socket into the VM requires shared-filesystem/socket behaviour that should not be part of the security or runtime architecture;
- local Podman and Podman Machine therefore require different assumptions.

The existing design also requires `socat` both on the host and inside the agent image, creates filesystem state for every injection, and requires process-pattern cleanup of the agent listener.

## Goals

The new design should:

1. Work with both local Podman and Podman Machine using the same topology.
2. Require no host-shared Unix socket.
3. Require no `socat` for `cladding inject`.
4. Support multiple simultaneous TCP connections during one inject command.
5. Use one long-lived `podman exec` rather than one exec per connection.
6. Keep the host endpoint fixed by the trusted host-side invocation.
7. Preserve ordinary full-duplex TCP byte-stream behaviour.
8. Shut down cleanly and predictably.
9. Keep the implementation small and specific to this use case.

## Non-goals

This project does not:

- replace the agent or network-sandbox Baffle `socat` bridges;
- replace Baffle data or control Unix sockets;
- replace `mcp-run` / `run-remote` Unix sockets;
- change `cladding expose`;
- provide generic TCP/Unix socket forwarding commands;
- provide UDP forwarding;
- provide TLS termination;
- provide SOCKS or HTTP proxy functionality;
- support arbitrary destinations chosen by the agent;
- provide persistent/background inject tunnels;
- introduce a general-purpose networking subsystem.

Future work may reuse the multiplexing implementation for `cladding expose`, but that should be justified separately.

## Proposed architecture

Introduce a small embedded Linux helper, tentatively named `cladding-bridge`.

The host-side Cladding process starts it inside the running agent with one interactive `podman exec`:

```text
podman exec -i <agent-container> \
    /opt/tools/bin/cladding-bridge inject \
    --listen 127.0.0.1:<container-port>
```

stdin/stdout of that exec become a multiplexed binary transport between host Cladding and `cladding-bridge` inside the agent.

The resulting data path is:

```text
agent application A --\
agent application B ----> 127.0.0.1:<container-port>
agent application C --/              |
                                     v
                              cladding-bridge
                                     |
                           logical mux streams
                                     |
                              podman exec I/O
                                     |
                                     v
                               host cladding
                                     |
                  +------------------+------------------+
                  |                  |                  |
                  v                  v                  v
          <host endpoint>    <host endpoint>    <host endpoint>
          connection A       connection B       connection C
```

Every connection accepted by the agent-side listener creates a new logical stream. For each logical stream, host Cladding opens a fresh TCP connection to the one host endpoint selected by the user.

## Capability boundary

The agent must **not** be able to choose the destination of an injected connection.

For:

```text
cladding inject db.internal:5432 15432
```

the capability granted to the agent is:

```text
connect repeatedly to exactly db.internal:5432
```

The agent-side bridge therefore only communicates new streams, stream bytes, and stream close/reset state. It must never send arbitrary destination requests such as `CONNECT 10.0.0.5:22`.

The destination belongs exclusively to the trusted host-side Cladding process. This keeps `cladding inject` a narrow endpoint capability rather than turning it into a general tunnel into the host network.

## Multiplexing

Use an established Rust stream-multiplexing implementation rather than defining a custom framing protocol. Yamux is the preferred starting point unless implementation investigation identifies a materially better fit.

One `cladding inject` invocation corresponds to one multiplexing session. The agent side opens logical streams and the host side accepts them. No routing metadata is required because all streams have the same fixed destination.

## Transport establishment

Host Cladding should:

1. Validate that the project and agent container are running.
2. Determine the requested agent listener port and fixed host destination.
3. Start one interactive `podman exec -i` running `cladding-bridge`.
4. Wait for an explicit bridge readiness indication.
5. Enter the mux session.
6. For each incoming logical stream, connect to the configured host endpoint and relay bytes bidirectionally.
7. Remain in the foreground until interrupted or either side fails.

The helper should not bind the agent listener until its configuration has been validated. The host command should not report inject as ready until the remote listener has successfully bound.

This removes timing sleeps and gives deterministic errors for cases such as:

- port already in use;
- helper missing;
- incompatible helper version;
- agent container disappearing;
- `podman exec` failure.

## Protocol startup

Before multiplexing begins, use a minimal version/readiness handshake. The exact encoding is an implementation detail, but it should provide at least protocol version and ready/startup-failure state.

Once the mux begins, stdout becomes protocol data and must never contain logging or diagnostic text. Diagnostics from `cladding-bridge` must go to stderr.

A protocol version mismatch should fail closed with a clear host-side error.

## Stream semantics

Each logical stream represents one ordinary TCP connection.

Required behaviour:

- full-duplex byte transparency;
- independent concurrent streams;
- bounded buffering and backpressure;
- no global blocking when one connection is slow;
- propagate EOF and half-close correctly;
- one failed destination connection affects only that logical stream;
- transport failure terminates the entire inject command;
- idle connections may remain open indefinitely.

When one side reaches EOF, the corresponding write side of the opposite socket should be shut down while allowing the reverse direction to continue until it also completes.

The implementation should place a reasonable upper bound on concurrent logical streams to prevent accidental unbounded host file-descriptor consumption. The exact default may be chosen during implementation; it should be comfortably above normal coding-agent workloads.

## Host endpoint semantics

Existing CLI behaviour should be preserved.

```text
cladding inject 5432
```

continues to mean host `localhost:5432` exposed as agent `localhost:5432`.

```text
cladding inject db.internal:5432 15432
```

continues to mean host `db.internal:5432` exposed as agent `127.0.0.1:15432`.

Importantly, with Podman Machine, `localhost` continues to mean the physical host running `cladding`, not the Podman Machine VM. This is one of the principal reasons to terminate the injected connection in the host Cladding process.

## Lifecycle

`cladding inject` remains a blocking foreground command.

Normal shutdown should be:

```text
Ctrl-C / SIGTERM
    |
    v
host stops accepting mux streams
    |
    v
mux transport closes
    |
    v
agent bridge observes transport EOF
    |
    v
agent listener closes
    |
    v
active streams close
    |
    v
bridge exits
    |
    v
podman exec exits
```

There should be no `pkill -f` cleanup.

The bridge process exists only as the child of the interactive `podman exec`, so losing the transport should naturally terminate it. Host-side shutdown should still have bounded waits and a forced termination fallback for abnormal cases.

## Runtime topology changes

The current inject-specific host socket topology can be removed:

```text
.cladding/runtime/sockets/agent/inject/
/run/cladding/agent/inject
```

and `/run/cladding/agent/inject` should no longer be mounted into the agent.

The following inject-specific objects become unnecessary:

```text
<port>/host.sock
host socat process
agent socat process
inject socket-directory cleanup
agent listener pkill cleanup
```

This leaves Cladding's inter-container Unix sockets entirely managed inside Podman volumes rather than requiring a special host-backed socket path for inject.

## Helper packaging

`cladding-bridge` should be built as an embedded Linux helper alongside the existing execution helpers.

Required architectures:

```text
x86_64-unknown-linux-gnu
aarch64-unknown-linux-gnu
```

It should be materialized into `/opt/tools/bin/cladding-bridge` and therefore become available in the agent through Cladding's existing `/opt/tools` mount. The helper should not need to exist in the custom agent image itself.

The CI embedded-tools artifact should contain it alongside the existing helper binaries.

## Relationship to `socat`

This proposal deliberately does not remove `socat` generally.

The existing Baffle compatibility bridge remains:

```text
127.0.0.1:3128
    |
    v
socat
    |
    v
Baffle proxy.sock
```

That is a simple local TCP-to-UDS adaptation and does not cross the host/VM boundary. There is currently insufficient benefit to replacing it with Cladding-owned code.

The principle is:

> Replace `socat` where the required topology exceeds what simple socket forwarding can cleanly provide, not merely because a new bridge helper exists.

For this proposal that means `cladding inject` only.

## Relationship to `cladding expose`

`cladding expose` has a structurally similar inverse problem and could later reuse the mux implementation:

```text
host TCP listener
    |
    v
mux over podman exec
    |
    v
agent bridge
    |
    v
agent localhost:<port>
```

However, expose is explicitly outside this project.

The implementation should avoid unnecessarily coupling the mux library to inject-specific concepts, but should not add APIs or abstractions solely for hypothetical expose reuse. Generalize when the second consumer exists.

## Testing

Unit tests should cover:

- listener configuration;
- handshake/version validation;
- multiple simultaneous logical streams;
- byte-transparent bidirectional copying;
- destination connection failure isolated to one stream;
- half-close handling;
- stream concurrency limits;
- mux transport failure;
- clean agent-side exit after transport EOF.

Integration testing should cover local rootless Podman and Podman Machine.

For local rootless Podman, start a normal Cladding runtime and verify an agent connection reaches a host-side test service, including multiple simultaneous connections. Verify that stopping `cladding inject` removes the listener and leaves no socket artifacts or helper process behind.

For Podman Machine, run the same test through the configured rootless Podman Machine connection. The test must specifically prove that `cladding inject localhost:<port>` reaches a service listening on the physical CI host rather than a service inside the VM. This is the primary topology regression this project is intended to prevent.

Failure coverage should include:

- agent listener port already occupied;
- host destination unavailable;
- agent container stops during inject;
- Podman Machine connection disappears;
- helper exits unexpectedly;
- host command receives SIGINT/SIGTERM.

All waits must be bounded so failed tunnel cleanup cannot hang CI.

## Documentation changes

Update the runtime documentation to describe inject as:

```text
agent TCP
    ->
multiplexed podman-exec transport
    ->
host TCP
```

Remove references to:

```text
runtime/sockets/agent/inject
/run/cladding/agent/inject
inject host.sock
inject socat processes
```

Do not alter documentation describing the unrelated Baffle `socat` bridges.

## Completion criteria

The project is complete when:

1. `cladding inject` uses one long-lived multiplexed `podman exec` transport.
2. Multiple simultaneous agent connections work correctly.
3. The destination is fixed by the host invocation and cannot be selected by the agent.
4. Local Podman and Podman Machine use the same inject architecture.
5. `localhost` host targets resolve on the actual host under Podman Machine.
6. The inject host-backed UDS mount and socket directories are removed.
7. `cladding inject` no longer invokes or requires `socat`.
8. No `pkill` or process-pattern cleanup is required.
9. Normal and failure shutdown paths are bounded and covered by tests.
10. Existing Baffle and sandbox UDS topology remains unchanged.
