# PRD: Optional gVisor Runtime Support

> Status: Current. `docs/features/current-runtime-summary.md` is the runtime topology reference; this PRD remains the current design record for optional `use_runsc` support.

## Objective
Add optional support for running Cladding-managed standalone execution containers with gVisor `runsc`.

The initial support should be configuration-driven and should pass the Podman runtime flags Cladding needs for the supported rootless environment:

```text
--runtime runsc
--runtime-flag ignore-cgroups
--runtime-flag host-uds=all
--runtime-flag network=none
```

## Motivation
gVisor provides an additional userspace kernel isolation boundary around containers. For Cladding, that is useful because the runtime already separates work across the proxy pod and standalone execution containers for the agent, optional network sandbox, and optional filesystem sandbox.

Cladding now manages Podman resources directly. The proxy stays on Podman's default runtime, while standalone execution containers run with `--network none` and communicate through scoped Unix-domain sockets. This gives Cladding a practical place to apply OCI runtime flags without changing the user-facing runtime model.

The initial gVisor support should stay narrow:

- use a single project-level opt-in
- use Podman's configured `runsc` runtime name
- always pass `ignore-cgroups`
- always pass `host-uds=all`
- preserve the existing direct-Podman and UDS topology
- fail explicitly when the host `runsc` setup is not usable

## Problem statement
The current runtime has no way to select an alternate OCI runtime for Cladding-managed standalone execution containers.

Users who want to test or use gVisor need Cladding to apply the runtime selection consistently across the execution containers it creates. Passing `--runtime` manually is not enough because Cladding creates a proxy pod plus separate standalone containers, and `cladding expose` is host-side rather than a Podman helper container flow.

The implementation needs to add runtime selection without weakening existing behavior:

- agent, nw-sandbox, and fs-sandbox should keep current file ownership behavior where possible
- UDS communication should continue to work
- proxy access should continue to use the existing UDS bridge model
- startup failures should expose the underlying Podman/runsc error

## Proposal
Add an optional boolean config that selects gVisor for managed containers.

### Config schema
Add a top-level boolean to `cladding.json`:

```json
{
  "name": "demo",
  "use_runsc": true,
  "agent": {
    "image": "localhost/cladding-default:latest"
  },
  "nw_sandbox": {
    "enabled": true
  }
}
```

Semantics:

- absent `use_runsc` means `false`
- `use_runsc: false` means use Podman's default OCI runtime
- `use_runsc: true` means use `runsc` for Cladding-managed containers

The config should not expose `runsc` path or runtime flag knobs in the first implementation. Cladding should call `runsc` through Podman using the supported default command shape. If a host needs a specific `runsc` binary, it should configure Podman's runtime path outside the project config.

### Podman flags
When `use_runsc` is true, pass the runtime options as global Podman options before the subcommand for standalone execution container startup:

```text
--runtime runsc
--runtime-flag ignore-cgroups
--runtime-flag host-uds=all
--runtime-flag network=none
```

`host-uds=all` is required because the current runtime uses host-mounted Unix socket directories in both directions:

- proxy bridge and sandbox `mcp-run` containers create/bind Unix sockets in mounted directories
- agent and nw-sandbox proxy clients, plus `run-remote`, connect to existing Unix sockets in mounted directories

`host-uds=create` alone is not enough because client containers need to open existing sockets. `host-uds=open` alone is not enough because server containers need to create socket endpoints. Since the first implementation uses one global runtime switch for all managed containers, Cladding should pass `host-uds=all` consistently instead of trying to compute a per-container minimum.

Apply the runtime consistently to the standalone execution containers only:

- agent instance container
- nw-sandbox instance container, when enabled
- fs-sandbox instance container, when enabled

Do not apply the runtime to the proxy pod, the proxy instance container, the proxy bridge sidecar, or `cladding expose`. The proxy pod remains on Podman's default runtime.

### User namespace behavior
The current direct runtime uses `--userns keep-id` for the agent, nw-sandbox, and fs-sandbox standalone containers, and does not use it for the proxy pod.

The first implementation should preserve that behavior. The agent, nw-sandbox, and fs-sandbox containers should keep `--userns keep-id` under `use_runsc=true`.

Do not add a fallback that drops `--userns keep-id` when `use_runsc` is true.

Do not use execution pods or `--infra=false` workarounds in the current design.

The standalone execution containers should continue to run with `--network none`. When `use_runsc=true`, Cladding should pass `--runtime-flag network=none` to those `podman run` commands.

Keep the proxy pod on the default Podman runtime and default pod behavior. The proxy pod does not use `--userns keep-id`, and it is intentionally multi-container from startup.

### Scope of runtime selection
Initial runtime selection should be limited to the standalone execution containers.

Do not add per-component runtime selection in the first implementation beyond the execution-container scope. That is enough to test and support the gVisor runtime while keeping behavior understandable.

Future per-component overrides may be useful, for example:

- keep proxy under the default runtime
- keep `cladding expose` host-side

That should be deferred until there is a concrete compatibility need.

### Validation
`cladding check` should validate the runtime configuration before `cladding up`:

- `use_runsc` is a boolean if present
- `runsc` is available through Podman or on `PATH`

Validation should not require launching a container. The authoritative compatibility test remains `cladding up`, because `runsc` failures can depend on Podman version, rootless user namespace behavior, image behavior, and host runtime configuration.

`cladding up` should include the failing Podman command context if `runsc` startup fails. `cladding expose` does not receive Podman runtime flags.

## Non-goals
1. Do not install or upgrade `runsc`.
2. Do not add per-component runtime overrides in the first implementation.
3. Do not make gVisor the default runtime.
4. Do not claim complete gVisor compatibility without integration tests.
5. Do not preserve legacy fallback behavior if the user explicitly requested `runsc`.
6. Do not expose `runsc` path or runtime-flag tuning in `cladding.json` in the first implementation.
7. Do not change Cladding's UDS network isolation model.
8. Do not change current user namespace behavior as part of this feature.
9. Do not introduce execution pods or `--infra=false` workarounds.

## Suggested implementation shape
1. Add `use_runsc: bool` to `cladding/src/config.rs` with a default of `false`.
2. Extend top-level unknown-key validation to allow `use_runsc`.
3. Add tests for default, valid boolean, non-boolean, and unknown-key behavior.
4. Thread the runtime setting into the runtime spec or Podman execution layer.
5. Add a Podman command helper that appends `--runtime runsc --runtime-flag ignore-cgroups --runtime-flag host-uds=all` before Podman subcommands when enabled.
6. Apply the helper only to standalone execution container startup.
7. Keep the proxy pod on Podman's default runtime and default pod behavior.
8. Add `cladding check` validation for `runsc` availability when `use_runsc` is true.
9. Add unit tests for command construction with and without `use_runsc`.
10. Document the host prerequisite that Podman must be able to resolve `runsc`.

## Migration plan
Existing projects do not need to change.

To opt in, a project adds:

```json
{
  "use_runsc": true
}
```

If gVisor startup fails, users can remove `use_runsc` or set it to `false` and return to Podman's default runtime without changing component config.

## Verification
Verification should include host integration tests where Podman and runsc are available:

- `cladding check` accepts a valid `use_runsc` config
- `cladding check` rejects non-boolean `use_runsc`
- `cladding check` reports missing `runsc` when `use_runsc` is true
- `cladding up` creates all enabled components under `runsc`
- `cladding run -- true` works
- `run-in-nw-sandbox --check -- true` works from the agent
- `run-in-fs-sandbox --check -- true` works from the agent
- proxy access still works through the UDS bridge
- `cladding expose` works when `use_runsc` is true
- removing `use_runsc` returns the project to the default Podman runtime

Manual diagnostics should include:

```bash
podman inspect <name>-agent-instance --format '{{.OCIRuntime}}'
```

or the equivalent Podman inspect field for confirming the selected runtime in the supported Podman version.

## Success criteria
1. gVisor support is opt-in through `use_runsc` in `cladding.json`.
2. Cladding passes `--runtime runsc`, `--runtime-flag ignore-cgroups`, `--runtime-flag host-uds=all`, and `--runtime-flag network=none` on standalone execution container startup when `use_runsc` is enabled.
3. Existing projects keep using the default Podman runtime when the config is absent.
4. Runtime failures produce actionable Podman/runsc error output.
5. Current UDS communication, proxy access, and user namespace behavior are preserved.
