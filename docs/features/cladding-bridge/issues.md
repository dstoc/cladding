# Cladding Bridge implementation issues

```text
cladding-bridge-1
Core multiplexed inject bridge
        |
        v
cladding-bridge-2
Package bridge as embedded Linux helper
        |
        v
cladding-bridge-3
Replace cladding inject transport
        |
        +-------------------+
        |                   |
        v                   v
cladding-bridge-4     cladding-bridge-5
Remove legacy        Podman Machine +
inject UDS/socat     local integration
        |                   |
        +---------+---------+
                  |
                  v
          cladding-bridge-6
          Documentation and
          final regression cleanup
```

## cladding-bridge-1 — Implement the multiplexed inject bridge

depends-on: none

Implement the minimal stream-multiplexing machinery required for the new `cladding inject` transport.

The implementation should consist of a small reusable Rust core plus an agent-side Linux executable tentatively named `cladding-bridge`.

The agent-side helper must:

- bind a caller-specified loopback TCP endpoint such as `127.0.0.1:15432`;
- use stdin/stdout as its single transport to host Cladding;
- create one logical multiplexed stream for each accepted TCP connection;
- support multiple simultaneous connections;
- copy data bidirectionally with bounded buffering/backpressure;
- preserve EOF and TCP half-close semantics;
- isolate per-stream failures;
- terminate when its transport disappears;
- write diagnostics only to stderr.

Use an established multiplexing implementation such as yamux rather than defining a custom stream framing protocol unless investigation demonstrates a concrete blocker.

Add a minimal startup/version handshake that lets the host distinguish:

- bridge ready;
- listener bind failure;
- protocol incompatibility;
- bridge startup failure.

The protocol must not support an arbitrary destination request. A stream represents only a connection; the host controls the fixed destination independently.

Include focused unit tests for concurrency, byte transparency, half-close, stream failure, transport shutdown, handshake behaviour, and a bounded concurrent-stream limit.

Explicitly do not add generic TCP/UDS forwarding, UDP, TLS, SOCKS, routing configuration, persistent tunnels, or `cladding expose` support.

## cladding-bridge-2 — Package `cladding-bridge` as an embedded Linux helper

depends-on: cladding-bridge-1

Integrate the agent-side bridge binary into Cladding's existing embedded Linux helper pipeline.

Build and package:

```text
x86_64-unknown-linux-gnu
aarch64-unknown-linux-gnu
```

alongside the existing Linux execution helpers.

Materialize it as:

```text
/opt/tools/bin/cladding-bridge
```

so it is available through the existing `/opt/tools` mount and does not need to be installed in custom agent images.

Update:

- helper build logic;
- embedded asset materialization;
- architecture verification;
- CI helper artifacts;
- Apple Silicon packaging path where Linux aarch64 helpers are embedded;
- release packaging where applicable;
- existing helper-selection regression tests.

Do not change any runtime behaviour in this issue.

## cladding-bridge-3 — Replace `cladding inject` with the multiplexed exec transport

depends-on: cladding-bridge-1, cladding-bridge-2

Rewrite `cladding inject` to use one long-lived interactive `podman exec` running the embedded bridge inside the already-running agent container.

Target topology:

```text
agent application
    |
    v
127.0.0.1:<container-port>
    |
    v
cladding-bridge
    |
    v
multiplexed podman exec stdin/stdout
    |
    v
host cladding
    |
    v
fixed <host>:<port>
```

Host Cladding must:

1. validate the project and agent runtime;
2. start one `podman exec -i` for the bridge;
3. wait for the bridge readiness handshake;
4. accept logical streams from the bridge;
5. open a fresh connection to the fixed host endpoint for each stream;
6. relay bytes bidirectionally;
7. remain in the foreground until interrupted or the transport fails.

Preserve the current user-facing CLI semantics, including defaulting the agent port from the host endpoint when omitted.

The host destination must remain entirely host-controlled. No mux message may allow the agent to choose another destination.

Ensure that host `localhost` refers to the physical host process running Cladding, including when Podman is connected to a Podman Machine.

Replace existing dual-child `socat` supervision with bounded supervision of the one `podman exec` transport.

On SIGINT/SIGTERM:

- stop accepting new work;
- close the mux transport;
- allow the remote bridge to exit on EOF;
- wait for bounded graceful termination;
- force termination only if required.

Do not use `pkill` or command-line pattern matching to clean up the remote helper.

## cladding-bridge-4 — Remove the legacy inject UDS and `socat` topology

depends-on: cladding-bridge-3

Remove runtime state and code that existed solely for the old inject implementation.

Remove:

```text
.cladding/runtime/sockets/agent/inject
/run/cladding/agent/inject
<container-port>/host.sock
```

and the agent host-path mount that exposes that directory.

Remove inject-specific code for:

- creating host socket directories;
- checking stale `host.sock` paths;
- host `socat`;
- agent `socat`;
- socket artifact cleanup;
- `socat_required("inject")`;
- `pkill -f` listener cleanup;
- old inject process supervision.

Update runtime socket-directory generation/tests accordingly.

Do **not** remove `socat` from Cladding generally.

In particular, retain the existing agent and network-sandbox `127.0.0.1:3128 -> Baffle proxy.sock` bridges.

The end state should have no host-backed runtime socket mount required for `cladding inject`.

## cladding-bridge-5 — Validate inject on local Podman and Podman Machine

depends-on: cladding-bridge-3

Add integration coverage for the new transport across both supported Podman topologies.

### Local rootless Podman

Verify:

- an agent connection reaches a host-side test service;
- multiple simultaneous connections work;
- request/response data is byte-correct;
- long-lived connections remain functional;
- closing one connection does not affect others;
- stopping inject removes the agent listener;
- no bridge process remains after command exit.

### Podman Machine

Run the equivalent test through the rootless Podman Machine connection.

Specifically prove that:

```text
cladding inject localhost:<port>
```

connects to a service listening on the physical CI host, not to localhost inside the Podman Machine VM.

This should not use a host-shared UDS or host bind mount.

### Failure cases

Exercise at least:

- agent listener port already occupied;
- host destination refuses connections;
- agent container exits;
- exec/mux transport closes unexpectedly;
- SIGINT/SIGTERM during active streams.

All lifecycle waits must be bounded and failure diagnostics should identify which side of the tunnel failed.

Reuse the existing Podman Machine CI environment rather than introducing a second Machine setup.

## cladding-bridge-6 — Update inject documentation and enforce the final architecture

depends-on: cladding-bridge-4, cladding-bridge-5

Update runtime and feature documentation to reflect the final inject architecture.

Document:

```text
agent TCP
    ->
cladding-bridge
    ->
multiplexed podman exec
    ->
host Cladding
    ->
fixed host TCP endpoint
```

Remove documentation for the old:

```text
agent inject UDS
host.sock
host-backed socket mount
dual-socat transport
```

Add regression checks where useful to prevent accidental restoration of the old topology.

Verify that repository production code no longer uses `socat` for `cladding inject`.

Keep documentation of the unrelated Baffle `socat` bridges unchanged.

Record `cladding expose` reuse as possible future work only; do not modify expose as part of this project.

## Explicit future work

No implementation issue should be created yet for these unless separately prioritized:

```text
Use the mux transport for cladding expose
Replace local Baffle TCP-to-UDS socat bridges
Generalize cladding-bridge into a generic forwarding tool
```

The project should land with the narrowest abstraction that fully solves `cladding inject`.
