# PRD: Optional gVisor Runtime Support

## Objective
Add optional support for running Cladding-managed containers with gVisor `runsc`.

The initial support should be configuration-driven and should pass the Podman runtime flags Cladding needs for the supported environment:

```text
--runtime=<runsc>
--runtime-flag=ignore-cgroups
```

This proposal assumes the direct Podman runtime and UDS-based network isolation work have landed first.

## Motivation
gVisor provides an additional userspace kernel isolation boundary around containers. For Cladding, that is attractive because the project already separates work across agent, network sandbox, filesystem sandbox, and proxy components.

Experiments showed several important constraints:

- `podman run --runtime=<newer-runsc> --runtime-flag=ignore-cgroups alpine sh` can work
- older/default `runsc` may fail in rootless environments while trying to use systemd cgroups
- `podman play kube` plus `io.podman.annotations.userns: keep-id` can fail even when direct `podman run --userns=keep-id` works
- gVisor does not expose the netfilter API needed by the current nftables jailers

Therefore gVisor support should not be bolted onto the current `play kube` + nftables model. It should build on direct Podman control and the UDS network isolation model.

## Problem statement
The current Cladding runtime cannot reliably run under `runsc`:

- `podman play kube` hides or globalizes runtime options
- rootless cgroup integration may require `--runtime-flag=ignore-cgroups`
- nftables jailers fail under gVisor with netlink/netfilter errors
- the current proxy identity model depends on network source IPs

Even if a single `podman run` command works, that does not prove the Cladding runtime works. Cladding needs a supported way to apply OCI runtime flags consistently to pods, app containers, and sidecars.

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

The config should not expose `runsc` path or `ignore_cgroups` knobs in the first implementation. Cladding should call `runsc` through Podman using the supported default command shape. If a host needs a specific `runsc` binary, it should configure Podman's runtime path outside the project config.

### Podman flags
When `use_runsc` is true, pass the runtime options to managed `podman create`/`podman run` calls:

```text
--runtime runsc
--runtime-flag ignore-cgroups
```

Apply the runtime consistently to:

- app containers
- sidecar containers
- one-shot setup containers that remain after the UDS migration

The direct Podman runtime proposal should provide the command construction point for this. Do not route gVisor support through `podman play kube`.

### Dependency on UDS network isolation
The supported gVisor mode requires the UDS-based network isolation runtime.

Do not support gVisor with nftables jailers. gVisor does not expose the required netfilter APIs, so a gVisor runtime path using the current jail scripts would fail during startup or silently weaken isolation if jailers were skipped.

If the implementation still has multiple runtime modes during migration, `cladding check` should reject:

```text
use_runsc = true
```

unless the project/runtime is using the no-nftables UDS isolation path.

### User namespace behavior
Do not reintroduce `io.podman.annotations.userns: keep-id` through a kube annotation.

Under direct Podman management, test whether direct `--userns=keep-id` is compatible with the selected `runsc` build. If it is compatible, keep existing file ownership behavior. If it is not compatible, fail with an explicit error rather than silently changing ownership semantics.

The first implementation should preserve current rootless file ownership behavior when possible. If preserving it is not possible under `runsc`, document that limitation and require an explicit opt-in.

### Scope of runtime selection
Initial runtime selection should be global for all Cladding-managed containers.

Do not add per-component runtime selection in the first implementation. A global switch is enough to test and support the gVisor runtime while keeping behavior understandable.

Future per-component overrides may be useful, for example:

- run execution containers under `runsc`
- keep proxy under the default runtime

That should be deferred until there is a concrete compatibility need.

### Validation
`cladding check` should validate the runtime configuration before `cladding up`:

- `use_runsc` is a boolean if present
- `runsc` can be found by Podman or on `PATH`
- UDS/no-nftables runtime mode is active when `runsc` is requested

`cladding up` should include the failing Podman command context if `runsc` startup fails.

## Non-goals
1. Do not install or upgrade `runsc`.
2. Do not support gVisor under `podman play kube`.
3. Do not support gVisor with nftables jailers.
4. Do not add per-component runtime overrides in the first implementation.
5. Do not make gVisor the default runtime.
6. Do not claim complete gVisor compatibility without integration tests.
7. Do not preserve legacy fallback behavior if the user explicitly requested `runsc`.
8. Do not expose `runsc` path or runtime-flag tuning in `cladding.json` in the first implementation.

## Suggested implementation shape
1. Land direct Podman runtime management.
2. Land UDS network isolation and remove nftables jailers from the supported runtime path.
3. Add `use_runsc: bool` to `cladding/src/config.rs`.
4. Extend unknown-key validation for `use_runsc`.
5. Add a Podman command helper that appends runtime flags to create/run commands.
6. Thread runtime config through app container, sidecar, and setup container creation.
7. Add `cladding check` validation for runsc availability and incompatible modes.
8. Document the required host prerequisites.

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

- `cladding check` accepts a valid `runsc` config
- `cladding check` rejects non-boolean `use_runsc`
- `cladding check` reports missing `runsc` when `use_runsc` is true
- `cladding up` creates all enabled components under `runsc`
- `cladding run -- true` works
- `run-in-nw-sandbox --check -- true` works from the agent
- `run-in-fs-sandbox --check -- true` works from the agent
- proxy access still works through the UDS bridge
- no startup container attempts to run `nft`
- removing `use_runsc` returns the project to the default Podman runtime

Manual diagnostics should include:

```bash
podman inspect <name>-agent-instance --format '{{.OCIRuntime}}'
```

or the equivalent Podman inspect field for confirming the selected runtime in the supported Podman version.

## Success criteria
1. gVisor support is opt-in through `use_runsc` in `cladding.json`.
2. Cladding passes `--runtime=<runsc>` and `--runtime-flag=ignore-cgroups` through direct Podman commands.
3. gVisor mode does not use nftables jailers.
4. Existing projects keep using the default Podman runtime when the config is absent.
5. Runtime failures produce actionable Podman/runsc error output.
