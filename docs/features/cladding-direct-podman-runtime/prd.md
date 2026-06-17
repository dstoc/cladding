# PRD: Direct Podman Runtime Management

> Status: Historical. This proposal describes the migration to direct Podman management; the current runtime now uses a proxy pod plus standalone execution containers and is summarized in `docs/features/current-runtime-summary.md`.

## Objective
Replace `podman play kube` runtime startup/teardown with explicit Podman pod and container commands while preserving the current Cladding runtime model:

- managed Podman pods are still used
- component pod names remain `<name>-proxy`, `<name>-agent`, `<name>-nw-sandbox`, and `<name>-fs-sandbox`
- app container names remain `<pod-name>-instance`
- existing labels continue to drive `cladding ps`, active network detection, cleanup, and expose proxy ownership

This is a runtime orchestration change, not a product behavior change. The current implementation has since moved past the kube-era and now uses a proxy pod plus standalone execution containers, so treat the rest of this document as migration history rather than current guidance.

## Motivation
Cladding currently renders Kubernetes-style YAML from `cladding/src/pods.rs` and starts/stops it through `cladding/src/podman.rs::podman_play_kube`.

That has been useful while the runtime was mostly static, but it is becoming a constraint:

- optional components require template mutation instead of normal Rust data modeling
- per-container Podman flags are difficult or impossible to express cleanly through `play kube`
- `runsc` and other OCI runtime experiments behave differently under `podman play kube` than under `podman run`
- future Unix-domain-socket routing needs explicit sidecar, mount, and startup ordering control
- `play kube` failures are harder to map back to the component Cladding was starting

Direct Podman management keeps pods but gives Cladding a more precise control surface.

## Problem statement
The current runtime path is centered on generated YAML:

- `cladding/src/pods.rs::render_pods_yaml_v2` builds YAML documents from `cladding/src/pod_templates/*.yaml`
- `cladding/src/podman.rs::podman_play_kube` runs `podman play kube`
- `cladding/src/cli.rs::cmd_up` renders YAML and calls `podman_play_kube`
- `cladding/src/cli.rs::cmd_down` renders YAML and calls `podman_play_kube --down`

The YAML path encodes several Podman behaviors indirectly:

- `podman play kube` prefixes container names, which is why `runtime_container_name()` returns `<pod-name>-instance`
- init containers are used as jailers to configure nftables in the pod network namespace
- Kubernetes `hostPath`, `emptyDir`, and `configMap` concepts are used to represent Podman mounts
- pod labels are used later by `list_running_projects()` and `list_running_project_networks()`

The next runtime changes need per-component and per-container behavior that should be represented directly in Rust rather than encoded through generated YAML and then inferred back from Podman behavior.

## Proposal
Introduce a direct Podman runtime builder that creates, starts, stops, and removes Cladding pods and containers explicitly.

### Runtime model
Create internal Rust types that describe the runtime components Cladding already has:

```text
RuntimeSpec
  project_name
  project_root
  network_settings
  pods[]

RuntimePod
  name
  labels
  network
  ip
  host_aliases
  init_tasks[]
  containers[]

RuntimeContainer
  name
  image
  command
  workdir
  env
  mounts
  ports
  labels
```

The first implementation should model current behavior, not create a generic orchestration framework. It only needs enough structure to represent:

- proxy
- agent
- nw-sandbox when enabled
- fs-sandbox when enabled
- current app containers
- current jailer init behavior
- current mounts and env vars

### Startup
Replace `podman_play_kube(..., down=false)` with a direct startup sequence:

1. Ensure the selected `cladding-N` network exists, as today.
2. Create each required pod with `podman pod create`.
3. Apply current pod labels:
   - `cladding=<name>`
   - `project_root=<project-root>`
   - `app=<component>`
4. Attach pods to the selected network and preserve deterministic component IP assignment.
5. Run current jailer setup as one-shot containers in the target pod network namespace.
6. Create app containers with stable names:
   - `<name>-proxy-instance`
   - `<name>-agent-instance`
   - `<name>-nw-sandbox-instance`
   - `<name>-fs-sandbox-instance`
7. Start app containers in dependency-safe order.

The jailer containers should be modeled as runtime setup tasks rather than permanent app containers. The initial implementation may use `podman run --rm --pod <pod>` or `podman create/start/wait/rm` depending on which gives better error reporting and cleanup. They should continue to run the existing scripts:

- `scripts/jail_agent.sh`
- `scripts/jail_nw_sandbox.sh`
- `scripts/jail_fs_sandbox.sh`

This preserves current runtime semantics before the later UDS/no-network proposal removes nftables jailers.

### Teardown
Replace `podman_play_kube(..., down=true)` with explicit cleanup:

1. Stop and remove project expose proxies as today.
2. Stop and remove managed app containers.
3. Remove managed pods with `podman pod rm -f`.
4. Keep the existing legacy pod cleanup for old runtime names.

Teardown should be idempotent. Missing containers and missing pods should not fail `cladding down`.

`cladding destroy` should use the same direct cleanup primitives rather than duplicating container-name construction.

### Mounts
Move mount generation out of YAML mutation and into the runtime builder.

The runtime builder should preserve current mount behavior:

- `.cladding/config` to `/opt/config`
- `.cladding/tools` to `/opt/tools`
- `.cladding/home` to `/home/user`
- workspace parent to `/home/user/workspace`
- custom mounts from `mounts[]`
- target-specific mount application
- ignored mount removal
- named volume naming as `<cladding_name>-<volume>`

The current `configMap`-based `.cladding` mask cannot be used directly without Kubernetes YAML. Replace it with an explicit empty read-only mount, for example:

- a generated empty directory under `.cladding/runtime/empty-mask`
- or a named empty volume mounted read-only

The implementation should choose the simplest option that works in rootless Podman and preserves the current effect: `/home/user/workspace/.cladding` inside execution containers should not expose the project `.cladding` directory.

### Host aliases and DNS
Preserve current connectivity by applying host aliases with direct Podman flags where they are still needed.

Current generated YAML sets host aliases for:

- proxy to agent and nw-sandbox
- agent to proxy, nw-sandbox, and fs-sandbox
- nw-sandbox to proxy

The direct runtime should keep the same host alias behavior until the UDS/no-network proposal removes most network-based communication.

### Container naming
Keep the raw container names currently expected by CLI commands:

```text
<pod-name>-instance
```

This avoids changing:

- `cladding run`
- `cladding run-with-scissors`
- `cladding logs`
- `cladding reload-proxy`
- `cladding expose`
- user-facing migration docs

After direct Podman management is stable, a later cleanup can reconsider whether exact container names should be simplified.

### Runtime discovery
Keep project discovery based on pod labels in `cladding/src/podman.rs`.

The direct runtime must create pods with labels compatible with:

- `list_running_projects()`
- `list_running_project_networks()`
- `project_runtime_status()`
- `resolve_active_project_network_settings()`

If direct Podman creation changes inspect output shape, update `inspect_pool_network_for_pod()` and tests accordingly.

## Non-goals
1. Do not remove Podman pods.
2. Do not change user-facing component names.
3. Do not change `cladding.json` schema.
4. Do not remove nftables jailers in this proposal.
5. Do not add gVisor support in this proposal.
6. Do not redesign `cladding expose`; only preserve current behavior as closely as direct Podman allows.
7. Do not introduce a generic orchestrator abstraction beyond the current Cladding runtime needs.

## Suggested implementation shape
1. Add a new runtime module, for example `cladding/src/runtime.rs`, that builds a `RuntimeSpec` from:
   - `ExecutionConfig`
   - `NetworkSettings`
   - project root
2. Add direct Podman helpers in `cladding/src/podman.rs`:
   - `pod_create`
   - `pod_rm`
   - `container_create`
   - `container_start`
   - `container_wait`
   - `container_rm`
3. Move reusable mount-building logic out of YAML mutation in `cladding/src/pods.rs`.
4. Update `cmd_up` to call the direct runtime startup path.
5. Update `cmd_down` and `cmd_destroy` to call direct runtime cleanup.
6. Keep `render_pods_yaml_v2` and pod templates temporarily during migration, then delete them after tests cover the direct runtime.

## Migration plan
This should not require a project config migration.

Existing users should be able to run:

```bash
cladding down
cladding up
```

and get the same pod names, container names, runtime labels, mounts, env vars, and command behavior.

If stale `play kube` resources remain from old versions, the existing `down`/`destroy` cleanup path should remove them by pod/container name.

## Verification
Unit and integration verification should cover:

- `cladding up` creates the expected pods and containers without `podman play kube`
- `podman pod ps --filter label=cladding` still reports running projects
- `cladding ps` reports the same project name and project root
- `cladding run -- true` works
- `cladding run-with-scissors --target nw-sandbox -- true` works when enabled
- `cladding run-with-scissors --target fs-sandbox -- true` works when enabled
- `cladding logs agent` reads `<name>-agent-instance`
- `cladding reload-proxy` still reconfigures Squid
- `cladding down` is idempotent
- custom mounts still apply only to their configured targets
- `/home/user/workspace/.cladding` remains masked inside execution containers

## Success criteria
1. `cladding up` and `cladding down` no longer call `podman play kube`.
2. Managed runtime pods and containers keep their current user-facing names.
3. Existing CLI commands continue to work against direct-created pods.
4. Pod labels continue to support project discovery and active network resolution.
5. The direct runtime is easier to extend with per-container flags, sidecars, and runtime selection.
