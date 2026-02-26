# PRD: Dynamic Shared Network Allocation in `cladding up`

## Objective
Move cladding network allocation from `cladding init` to `cladding up`, using a shared global pool of Podman networks named `cladding-N`.

This change removes the `subnet` configuration dependency from `.cladding/cladding.json` and makes network selection runtime-driven:
- `cladding init` performs no network operations.
- `cladding up` selects an unused `cladding-N` network (or creates one) and uses it for pod startup.

## Use Cases
1. A developer runs `cladding init` in a new project and gets config/templates without requiring Podman network provisioning.
2. A developer runs `cladding up`; cladding auto-selects a currently-unused `cladding-N` network from a shared pool and starts pods.
3. Multiple projects run concurrently; each gets a distinct active `cladding-N` network.
4. A stopped project runs `cladding up` later and can attach to any currently-unused pool network (no sticky network requirement).

## Functional Requirements
1. `cladding.json` schema removes `subnet`.
2. `cladding init`:
   - must not check, create, or modify Podman networks.
   - continues generating `.cladding` structure and default config.
3. `cladding up`:
   - determines network at runtime from pool `cladding-0` through `cladding-255`.
   - treats a network as unused when it has no running cladding pods attached.
   - may assume `cladding-N` networks are not used by non-cladding containers.
   - selects any unused network (no project-to-network stickiness required).
   - if chosen network does not exist, creates it with subnet `10.90.N.0/24`.
4. Subnet mapping:
   - `cladding-N` maps to `10.90.N.0/24`, with `N` in `0..255`.
5. Pod startup network settings:
   - continue providing deterministic in-subnet static pod IPs (proxy `.2`, sandbox `.3`, cli `.4`) based on selected `N`.
   - continue using existing pod name derivation based on project `name`.
6. Internal project/network discovery:
   - implementation may reuse/extend internals used by `cladding ps` to identify active cladding runtime usage.
   - no required user-facing output changes for `cladding ps`.
7. Command compatibility updates:
   - commands that currently derive network settings from config `subnet` (e.g., `up`, `down`, `destroy`, `run`, `reload-proxy`) must derive equivalent settings from runtime-selected/active `cladding-N` network state.
8. Exhaustion behavior:
   - if all `cladding-0..255` networks are in use by running cladding projects, `cladding up` fails with a clear actionable error.

## Non-Goals
1. Backward compatibility/migration logic for old `cladding.json` files containing `subnet`.
2. Expanding pool size beyond `0..255` or changing CIDR size from `/24`.
3. Sticky per-project network assignment across restarts.
4. Changing user-facing `cladding ps` output format.
5. Supporting non-cladding container sharing on `cladding-N` networks.

## Design Considerations
1. Simplicity:
   - users should not need to reason about subnets in config.
   - network lifecycle should be implicit in `cladding up`.
2. Determinism:
   - naming and subnet mapping must remain predictable (`cladding-N` <-> `10.90.N.0/24`).
3. Operational clarity:
   - failure messages should clearly distinguish:
     - no free network slots,
     - Podman inspection/create failures,
     - inability to resolve active network for project-scoped commands.
4. Minimal UX change:
   - keep CLI surface unchanged where possible; behavior shifts behind existing commands.

## Technical Considerations
1. Config and validation:
   - remove `subnet` from `Config` and JSON parsing/validation in `cladding/src/config.rs`.
   - update generated default config in `write_default_cladding_config` to omit `subnet`.
2. Network settings model:
   - replace `resolve_network_settings(name, subnet)` with a model that accepts selected pool index/network identity and computes subnet/IPs.
   - preserve existing pod IP allocation logic within selected subnet.
3. Network pool management:
   - add helper(s) to:
     - enumerate running cladding pods/projects and their attached `cladding-N` networks,
     - compute used `N` set,
     - pick first available `N`,
     - ensure/create `cladding-N` with expected subnet.
4. Command flow updates:
   - `cmd_init`: remove final `ensure_network_settings` step.
   - `cmd_up`: perform pool selection + ensure network before `podman play kube`.
   - `cmd_down` / `cmd_destroy` / `cmd_run` / `cmd_reload_proxy`: resolve active runtime network for current project without `subnet` config dependency.
5. `ps` reuse:
   - internal data retrieval for running cladding projects can be extended as needed; no requirement to expose network field in printed output.
6. Documentation/tests:
   - update README sections that currently document init-time subnet selection and `<name>_cladding_net` creation.
   - update unit/integration tests that construct `Config` with `subnet`.
   - add tests for pool allocation, exhaustion, and init-without-network behavior.

## Success Metrics
1. `cladding init` succeeds without any Podman network existence/creation calls.
2. `cladding up` starts projects using `cladding-N` and `10.90.N.0/24` with no `subnet` key in config.
3. Concurrent projects are assigned unique active pool networks.
4. A stopped project can restart on a different available `cladding-N` network successfully.
5. When pool is exhausted, error is explicit and actionable.
6. Existing CLI workflows (`up`, `down`, `run`, `destroy`, `reload-proxy`) remain functional under runtime network resolution.

## Open Questions
1. For commands run while project pods are not active (`down`, `destroy`, `reload-proxy` edge cases), should missing active network be a hard error with guidance, or should commands attempt best-effort fallback inference?
2. Should unused `cladding-N` networks ever be garbage-collected automatically, or remain persistent once created?
