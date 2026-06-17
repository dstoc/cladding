# PRD: Blocking `cladding expose`

## Objective
Replace the current persistent `cladding expose` implementation with a blocking host-side TCP forwarder.

The new implementation should be intentionally simple:

```bash
cladding expose <container-port> [host-port]
```

starts a foreground listener on `127.0.0.1:<host-port>` and forwards each connection through `cladding run` to `127.0.0.1:<container-port>` inside the agent container. The user stops the expose session with Ctrl-C.

## Motivation
`cladding expose` was originally designed around persistent helper containers:

- an expose sidecar inside the agent pod
- a host-facing helper container
- a shared runtime socket under `.cladding/runtime/expose`
- label-based list and stop operations
- cleanup during `cladding down` and `cladding destroy`

That model no longer fits the current runtime shape. The agent is now a standalone container, not a pod, so the existing pod-sidecar command cannot join the agent pod. Keeping persistent expose support would require a new network-namespace attachment design, additional lifecycle tracking, and more compatibility work around `runsc`.

For the primary development use case, a blocking command is enough:

1. start the service in the agent container;
2. run `cladding expose <container-port> [host-port]`;
3. use the host port while that command is running;
4. press Ctrl-C when done.

This shifts expose from project-managed runtime state to an explicit foreground session.

## Problem Statement
The current implementation in `cladding/src/cli.rs` still assumes the agent has a pod:

```text
cladding expose
  -> podman run --pod <name>-agent alpine/socat ...
  -> podman run --network host alpine/socat ...
```

The current runtime in `cladding/src/runtime.rs` creates the agent as `RuntimePlacement::Standalone`. There is no `<name>-agent` pod for the expose sidecar to join.

As a result, the existing expose flow is broken or ineffective after the switch to standalone execution containers.

The current implementation also has more state than the feature now needs:

- `ExposeSubcommand::List`
- `ExposeSubcommand::Stop`
- expose labels in `append_expose_labels`
- expose container listing helpers in `cladding/src/podman.rs`
- `.cladding/runtime/expose` socket cleanup
- persistent helper-container cleanup in `down` and `destroy`

That state exists only to support long-lived detached expose mappings.

## Proposal
Make `cladding expose` a blocking command that runs until interrupted.

### CLI
Keep the existing create command shape:

```bash
cladding expose <container-port> [host-port]
```

Semantics:

- `<container-port>` is the TCP port inside the agent container.
- `[host-port]` is the localhost TCP port to bind on the host.
- if `[host-port]` is omitted, use the same numeric port as `<container-port>`.
- bind only to `127.0.0.1`.
- do not search for the next available port.
- if the requested host port is unavailable, fail with a clear error.
- the command runs in the foreground until the user interrupts it.

Example:

```bash
cladding expose 5432 15432
```

should behave like:

```bash
socat TCP-LISTEN:15432,bind=127.0.0.1,reuseaddr,fork \
  EXEC:'cladding run socat STDIO TCP:127.0.0.1:5432'
```

The previous `cladding expose 3000` case remains supported, but it now blocks and maps `127.0.0.1:3000` on the host to `127.0.0.1:3000` in the agent container.

### Remove persistent expose management
Remove these user-facing subcommands:

```bash
cladding expose list
cladding expose stop <host-port>
```

There is no project-owned expose state to list or stop. A running expose session is the process the user started. Ctrl-C is the stop operation.

`cladding down` and `cladding destroy` no longer need to remove expose helper containers or `.cladding/runtime/expose`.

### Implementation model
Use host `socat` as the first implementation.

`cmd_expose` should:

1. load the current project config;
2. verify the project is running with the existing project status path;
3. verify the agent container exists;
4. verify host `socat` exists on `PATH`;
5. build and exec or spawn a foreground `socat` process;
6. forward the `socat` exit status as the `cladding expose` exit status.

The command should not create Podman containers.

The host listener process should run a per-connection `cladding run` command that starts `socat` inside the agent:

```text
host client
  -> host socat TCP-LISTEN:<host-port>
  -> cladding run socat STDIO TCP:127.0.0.1:<container-port>
  -> agent-local service
```

`cladding run` already uses non-interactive `podman exec -i` when stdin/stdout are not terminals, so it is suitable for raw byte forwarding. It must not allocate a TTY for expose connections.

### Command construction
Avoid constructing the `EXEC:` command with unescaped user input beyond validated numeric ports.

The only dynamic values in the initial implementation should be validated `u16` ports. The generated command may be a string because `socat EXEC:` requires one, but it should be produced by a small helper with unit tests.

Recommended helper shape:

```rust
fn build_blocking_expose_command(container_port: u16, host_port: u16) -> Command
```

It should produce:

```text
socat
TCP-LISTEN:<host-port>,bind=127.0.0.1,reuseaddr,fork
EXEC:cladding run socat STDIO TCP\\:127.0.0.1\\:<container-port>
```

If the final implementation needs shell quoting for `EXEC:`, keep the quoting in one tested helper.

### Dependencies
The host must have `socat` installed.

The agent image must also have `socat` installed. The default Cladding image already needs `socat` for inline proxy bridges. Custom agent images that want `cladding expose` must include it.

Do not make `cladding check` fail when host `socat` is missing. `socat` is only required for `cladding expose`, not for normal `check`, `up`, `run`, or sandbox execution workflows.

When `cladding expose` cannot find host `socat`, print:

```text
missing: socat (required for cladding expose)
```

and fail before starting the listener.

If agent-side `socat` is missing, the per-connection `cladding run socat ...` command will fail. That is acceptable for the first implementation, but the error should not be hidden by `cladding expose`.

### Signal behavior
Ctrl-C should stop the foreground host `socat` process and return control to the user.

No additional cleanup is required because the command does not create containers or runtime socket files.

If `socat` has forked child processes for active connections, normal `socat` signal behavior is acceptable for the first implementation. Cladding does not need to track per-connection child PIDs.

### Documentation
Update `README.md` to describe the blocking behavior:

```bash
cladding expose <containerport> [hostport] # block while forwarding localhost hostport to agent containerport
```

Remove `cladding expose list` and `cladding expose stop` from the README.

The docs should explicitly say that stopping expose is done with Ctrl-C.

### Tests
Update CLI parsing tests:

- `cladding expose <container-port>` parses;
- `cladding expose <container-port> <host-port>` parses;
- `cladding expose` without ports still fails;
- `cladding expose list` no longer parses;
- `cladding expose stop <host-port>` no longer parses.

Add command-construction tests for the blocking `socat` command:

- host bind uses `127.0.0.1`;
- omitted host port defaults to container port;
- host port and container port are placed in the expected positions;
- no Podman runtime flags are involved.

Remove or rewrite tests that assert:

- expose labels;
- pod-sidecar command construction;
- host-helper command construction;
- runtime expose socket paths.

## Non-goals
1. Do not support detached expose mappings in this implementation.
2. Do not preserve `cladding expose list`.
3. Do not preserve `cladding expose stop`.
4. Do not create expose helper containers.
5. Do not create `.cladding/runtime/expose` socket files.
6. Do not expose ports from nw-sandbox or fs-sandbox.
7. Do not add automatic host-port search.
8. Do not implement a native Rust TCP proxy in the first implementation.

## Suggested implementation shape
1. Remove `ExposeSubcommand` and simplify `ExposeArgs` to only positional `container_port` and optional `host_port`.
2. Replace `cmd_expose_create`, `cmd_expose_stop`, and `cmd_expose_list` with one blocking `cmd_expose`.
3. Remove expose cleanup calls from `cmd_down` and `cmd_destroy`.
4. Remove expose helper-container listing imports and call sites from `cladding/src/cli.rs`.
5. Remove expose-specific Podman listing types and helpers from `cladding/src/podman.rs` if they are no longer referenced.
6. Add a small `socat_required` helper or reuse the existing executable lookup pattern.
7. Add `build_blocking_expose_command(container_port, host_port)` and unit tests.
8. Update README command examples.

## Migration plan
This is an intentional behavior change.

Before:

```bash
cladding expose 3000 9000
cladding expose list
cladding expose stop 9000
```

After:

```bash
cladding expose 3000 9000
# command blocks
# press Ctrl-C to stop forwarding
```

Users who need multiple exposed ports should run one `cladding expose` process per mapping, usually in separate terminals.

There is no migration for existing detached expose helper containers. `cladding down` and `cladding destroy` may keep a best-effort legacy cleanup pass for one release if desired, but new expose sessions should not create any persistent resources.

## Verification
Manual verification:

1. Start a service in the agent container, for example `python3 -m http.server 8000`.
2. Run `cladding expose 8000 18000`.
3. From the host, run `curl http://127.0.0.1:18000/` and confirm it reaches the agent service.
4. Press Ctrl-C in the `cladding expose` terminal.
5. Confirm `curl http://127.0.0.1:18000/` no longer connects.
6. Confirm `podman ps` shows no expose helper containers were created.
7. Confirm `cladding down` does not need expose cleanup to complete.

Automated verification:

- `cargo test -p cladding` passes.
- CLI parse tests reflect the new blocking-only expose surface.
- command-construction tests assert the expected `socat` arguments.

## Success criteria
1. `cladding expose <container-port> [host-port]` starts a blocking localhost forward to the agent container.
2. Ctrl-C stops the expose session without requiring `cladding expose stop`.
3. `cladding expose list` and `cladding expose stop` are removed from the CLI.
4. The implementation does not create expose helper containers or runtime expose socket files.
5. The implementation works with the agent as a standalone container.
