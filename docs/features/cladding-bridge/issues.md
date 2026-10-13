# Cladding Bridge implementation issues

```text
cladding-bridge-1
Reusable fixed-endpoint mux core
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

The project intentionally stops after migrating `cladding inject`.

A future Baffle migration may reuse the bridge to connect execution-container loopback listeners to fixed host-side Baffle Unix sockets, but that migration must be planned and implemented as a separate task graph.

## cladding-bridge-1 — Implement reusable fixed-endpoint bridge core

depends-on: none

Implement the minimal multiplexed byte-stream transport required for `cladding inject`, while keeping the core reusable by a future second consumer such as host-side Baffle.

The implementation should consist of:

- a small reusable Rust mux/relay core;
- host-side fixed-target connection support;
- an execution-container Linux helper tentatively named `cladding-bridge`.

The execution-container helper must:

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

The host-side core must support one fixed destination per bridge session, with exactly these target types:

```text
TCP host:port
Unix-domain socket path
```

The destination belongs entirely to trusted host-side Cladding and must never be supplied by the remote bridge or encoded as per-stream routing data.

Conceptually, the host side may use an abstraction such as:

```text
HostTarget::Tcp(...)
HostTarget::Unix(...)
```

but the exact Rust API is an implementation choice.

A bridge session represents:

```text
one container-local listener
        ->
one mux session
        ->
one host-selected fixed target
```

Do not introduce a VM-wide daemon or one global mux carrying unrelated Cladding capabilities.

Add a minimal startup/version handshake that lets the host distinguish:

- bridge ready;
- listener bind failure;
- protocol incompatibility;
- bridge startup failure.

The protocol must not support arbitrary destination requests. A stream represents only a connection to the already-selected fixed target.

Include focused unit tests for:

- concurrency;
- byte transparency;
- half-close;
- stream failure isolation;
- transport shutdown;
- handshake behaviour;
- bounded concurrent-stream limits;
- fixed TCP target connections;
- fixed Unix-socket target connections;
- absence of destination-selection/routing messages from the remote side.

Explicitly do not add:

- Baffle runtime migration;
- generic TCP/UDS forwarding configuration;
- remote-selected destinations;
- UDP;
- TLS;
- SOCKS or HTTP CONNECT;
- persistent VM-wide tunnels;
- `cladding expose` support.

The goal is a narrow reusable fixed-endpoint transport, not a general-purpose tunnelling subsystem.

## cladding-bridge-2 — Package `cladding-bridge` as an embedded Linux helper

depends-on: cladding-bridge-1

Integrate the execution-container bridge binary into Cladding's existing embedded Linux helper pipeline.

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

so it is available through the existing `/opt/tools` mount and does not need to be installed in custom execution-container images.

Use transport-oriented helper naming/CLI rather than making the executable itself inject-specific. For example, the helper may expose a fixed-listener mode such as:

```text
cladding-bridge listen --tcp 127.0.0.1:<port>
```

The exact CLI is an implementation choice, but later consumers must not need to pretend they are invoking `cladding inject` internals.

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

Rewrite `cladding inject` as the first production consumer of the reusable fixed-endpoint bridge.

Use one long-lived interactive `podman exec` running the embedded bridge inside the already-running agent container.

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
host Cladding
    |
    v
fixed <host>:<port>
```

Host Cladding must:

1. validate the project and agent runtime;
2. resolve the requested inject destination to one fixed host-side TCP target;
3. start one `podman exec -i` for the bridge;
4. wait for the bridge readiness handshake;
5. accept logical streams from the bridge;
6. open a fresh connection to the fixed TCP target for each stream;
7. relay bytes bidirectionally;
8. remain in the foreground until interrupted or the transport fails.

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

This issue must use only the TCP host-target path. Do not migrate Baffle or add production use of Unix targets here.

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

In particular, retain the existing agent and network-sandbox `127.0.0.1:3128 -> Baffle proxy.sock` bridges. Baffle migration is a separate future project.

The end state should have no host-backed runtime socket mount required for `cladding inject`.

## cladding-bridge-5 — Validate inject on local Podman and Podman Machine

depends-on: cladding-bridge-3

Add integration coverage for the new inject transport across both supported Podman topologies.

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

Do not add a Baffle runtime integration test in this issue. Unix-target behaviour belongs in focused core tests from `cladding-bridge-1` until the separate Baffle migration project exists.

## cladding-bridge-6 — Update bridge/inject documentation and enforce the final architecture

depends-on: cladding-bridge-4, cladding-bridge-5

Update runtime and feature documentation to reflect both the reusable bridge boundary and the production inject architecture.

Document the reusable primitive as:

```text
container-local TCP listener
    ->
cladding-bridge
    ->
multiplexed podman exec
    ->
host Cladding
    ->
fixed host TCP or Unix-socket target
```

Document `cladding inject` specifically as:

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

Remove documentation for the old inject topology:

```text
agent inject UDS
host.sock
host-backed socket mount
dual-socat transport
```

Add regression checks where useful to prevent accidental restoration of the old topology.

Verify that repository production code no longer uses `socat` for `cladding inject`.

Keep documentation of the current Baffle runtime and its local `socat` bridges accurate and unchanged by this implementation.

Record the future Baffle direction clearly:

```text
execution container 127.0.0.1:3128
    -> cladding-bridge
    -> exec mux
    -> physical-host Cladding
    -> fixed host-side Baffle UDS
```

but state that moving Baffle, changing its lifecycle, and removing its Podman socket volumes are separate future work.

## Explicit future work

Do not create implementation issues for these as part of this project:

```text
Migrate Baffle out of the Podman Machine using cladding-bridge
Remove Baffle Podman socket volumes and local socat bridges after that migration
Use the mux transport for cladding expose
Replace podman-exec transport with a persistent VM-level transport such as vsock if later justified
```

The Baffle migration should get its own proposal/task graph because it changes trusted-component placement, runtime lifecycle, credential isolation, startup/health behaviour, and cleanup semantics.

This project should land with the narrowest reusable abstraction justified by the two known consumers: a fixed-endpoint byte-stream bridge with TCP/Unix host targets, and `cladding inject` as its only production consumer.
