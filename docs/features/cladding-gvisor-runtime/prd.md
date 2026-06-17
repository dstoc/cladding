# PRD: Optional gVisor Runtime Support

## Objective
Add optional support for running Cladding-managed containers with gVisor `runsc`.

The initial support should be configuration-driven and should pass the Podman runtime flags Cladding needs for the supported rootless environment:

```text
--runtime runsc
--runtime-flag ignore-cgroups
--runtime-flag host-uds=all
```

## Motivation
gVisor provides an additional userspace kernel isolation boundary around containers. For Cladding, that is useful because the runtime already separates work across the agent, optional network sandbox, optional filesystem sandbox, proxy, and bridge components.

Cladding now manages pods and containers directly with Podman. Non-proxy execution pods run with `--network none` and communicate through scoped Unix-domain sockets. This gives Cladding a practical place to apply OCI runtime flags consistently without changing the user-facing runtime model.

The initial gVisor support should stay narrow:

- use a single project-level opt-in
- use Podman's configured `runsc` runtime name
- always pass `ignore-cgroups`
- always pass `host-uds=all`
- preserve the existing direct-Podman and UDS topology
- fail explicitly when the host `runsc` setup is not usable

## Problem statement
The current runtime has no way to select an alternate OCI runtime for Cladding-managed containers.

Users who want to test or use gVisor need Cladding to apply the runtime selection consistently across the containers it creates. Passing `--runtime` manually is not enough because Cladding creates multiple pods, instance containers, persistent bridge containers, and transient expose helpers.

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
When `use_runsc` is true, pass the runtime options as global Podman options before the subcommand:

```text
--runtime runsc
--runtime-flag ignore-cgroups
--runtime-flag host-uds=all
```

`host-uds=all` is required because the current runtime uses host-mounted Unix socket directories in both directions:

- proxy bridge and sandbox `mcp-run` containers create/bind Unix sockets in mounted directories
- agent, nw-sandbox proxy clients, `run-remote`, and expose helpers connect to existing Unix sockets in mounted directories

`host-uds=create` alone is not enough because client containers need to open existing sockets. `host-uds=open` alone is not enough because server containers need to create socket endpoints. Since the first implementation uses one global runtime switch for all managed containers, Cladding should pass `host-uds=all` consistently instead of trying to compute a per-container minimum.

Apply the runtime consistently to Cladding-created containers:

- pod infra containers created by `podman pod create`
- agent instance container
- nw-sandbox instance container, when enabled
- fs-sandbox instance container, when enabled
- proxy instance container
- persistent proxy bridge sidecar
- transient `cladding expose` pod-sidecar and host-helper containers
- any future one-shot setup containers created through the runtime helper layer

Apply the runtime options to `podman pod create` as well as `podman run`. Podman pods have infra containers, and the infra container must not fall back to Podman's default runtime when `use_runsc` is enabled.

### User namespace behavior
The current direct runtime uses `--userns keep-id` for the agent, nw-sandbox, and fs-sandbox pods, and does not use it for the proxy pod.

The first implementation should preserve that behavior. The agent, nw-sandbox, and fs-sandbox pods should keep `--userns keep-id` under `use_runsc=true`.

Do not add a fallback that drops `--userns keep-id` when `use_runsc` is true.

When `use_runsc=true`, create the keep-id execution pods with `--infra=false`. In local testing, standalone `runsc + --userns keep-id` works, and `runsc` pods with `--infra=false` work. The failing combination is a default pod infra container plus a later `runsc` container joining the infra container's keep-id user namespace.

Podman does not allow `pod create --infra=false` to also specify a pod network mode. For runsc no-infra execution pods, omit pod-level `--network` and pass the execution pod's network mode, currently `none`, to the managed instance container's `podman run` command.

Keep the proxy pod on the default infra behavior for now. The proxy pod does not use `--userns keep-id`, and it is intentionally multi-container from startup.

### Scope of runtime selection
Initial runtime selection should be global for all Cladding-managed containers.

Do not add per-component runtime selection in the first implementation. A global switch is enough to test and support the gVisor runtime while keeping behavior understandable.

Future per-component overrides may be useful, for example:

- run execution containers under `runsc`
- keep proxy or expose helpers under the default runtime

That should be deferred until there is a concrete compatibility need.

### Validation
`cladding check` should validate the runtime configuration before `cladding up`:

- `use_runsc` is a boolean if present
- `runsc` is available through Podman or on `PATH`

Validation should not require launching a container. The authoritative compatibility test remains `cladding up`, because `runsc` failures can depend on Podman version, rootless user namespace behavior, image behavior, and host runtime configuration.

`cladding up` and `cladding expose` should include the failing Podman command context if `runsc` startup fails.

## Non-goals
1. Do not install or upgrade `runsc`.
2. Do not add per-component runtime overrides in the first implementation.
3. Do not make gVisor the default runtime.
4. Do not claim complete gVisor compatibility without integration tests.
5. Do not preserve legacy fallback behavior if the user explicitly requested `runsc`.
6. Do not expose `runsc` path or runtime-flag tuning in `cladding.json` in the first implementation.
7. Do not change Cladding's UDS network isolation model.
8. Do not change current user namespace behavior as part of this feature.

## Suggested implementation shape
1. Add `use_runsc: bool` to `cladding/src/config.rs` with a default of `false`.
2. Extend top-level unknown-key validation to allow `use_runsc`.
3. Add tests for default, valid boolean, non-boolean, and unknown-key behavior.
4. Thread the runtime setting into the runtime spec or Podman execution layer.
5. Add a Podman command helper that appends `--runtime runsc --runtime-flag ignore-cgroups --runtime-flag host-uds=all` before Podman subcommands when enabled.
6. Apply the helper to pod creation, managed instance container startup, runtime task execution, and `cladding expose` helper containers.
7. Use `--infra=false` for `use_runsc=true` agent, nw-sandbox, and fs-sandbox pods while preserving `--userns keep-id`.
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
2. Cladding passes `--runtime runsc`, `--runtime-flag ignore-cgroups`, and `--runtime-flag host-uds=all` as global options on direct Podman commands that create pods or run containers.
3. Existing projects keep using the default Podman runtime when the config is absent.
4. Runtime failures produce actionable Podman/runsc error output.
5. Current UDS communication, proxy access, and user namespace behavior are preserved.
