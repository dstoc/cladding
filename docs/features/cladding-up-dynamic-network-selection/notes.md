# Research Notes: Dynamic network selection in `cladding up`

## Request summary
- Current behavior: `cladding init` selects an unused subnet (`10.90.X.0/24`), writes it to `.cladding/cladding.json`, and creates a Podman network derived from config name.
- Requested behavior: remove subnet/network config from `cladding.json`; have `cladding up` select an unused `cladding-N` network at runtime (or create one) and use it. Suggestion from request: possibly reuse/extend `cladding ps` functionality to help detect unused networks.
- User decision: `cladding-N` networks are a shared global pool across all projects (not project-specific).
- User decision: network reuse can be based on cladding usage only; assume `cladding-N` networks are not used by non-cladding containers.
- User decision: no migration/backward-compat behavior for `subnet`; remove it from config schema.
- User decision: `cladding init` should not perform any network operations; network selection/creation happens in `cladding up`.
- User decision: network assignment does not need stickiness per project across restarts; any currently-unused pool network is acceptable on each `up`.
- User decision: keep subnet mapping as `10.90.N.0/24` for `cladding-N`, with `N` in `0..255`.
- User decision: no `cladding ps` output change is required; any `ps` work is internal support only.

## Current implementation findings

### Config schema and init path
- File: `cladding/src/config.rs`
- `Config` currently requires: `name`, `subnet`, `sandbox_image`, `cli_image` (`mounts` optional).
- `load_cladding_config` hard-requires `subnet` and validates CIDR.
- `write_default_cladding_config`:
  - checks for existing network named `<name>_cladding_net` via `podman network exists`.
  - calls `pick_available_subnet()` that scans `10.90.0.0/24 ... 10.90.255.0/24` against `podman network inspect` output.
  - writes generated JSON containing `subnet`.

### Network settings derivation and usage
- File: `cladding/src/network.rs`
- `resolve_network_settings(name, subnet)` derives:
  - network name `<name>_cladding_net`
  - static pod IPs from subnet (`.2`, `.3`, `.4`)
  - pod names from `name`.
- IP allocation requires subnet to be present and valid.

### Command flows
- File: `cladding/src/cli.rs`
- `cmd_init`:
  - creates `.cladding` structure and `cladding.json`.
  - loads config, resolves network settings from `name + subnet`, and calls `ensure_network_settings` (network created during init if absent).
- Requested change requires deleting the `cmd_init` network provisioning step.
- `cmd_up`:
  - loads config, runs checks, resolves network settings from `name + subnet`.
  - calls `ensure_network_settings` and then `podman_play_kube(..., --network <derived-name> --ip <derived-ips>)`.
- `cmd_down`, `cmd_destroy`, `cmd_run`, `cmd_reload_proxy` all currently resolve network settings from `name + subnet` as well.

### Podman helpers and ps
- File: `cladding/src/podman.rs`
- `ensure_network_settings` currently enforces network subnet exact match; creates network if missing.
- `list_running_projects()` uses `podman pod ps --filter label=cladding --filter status=running --format json`.
  - Returns `name`, `project_root`, pod count.
  - Does not track network names or network usage.

### Labels available in pods
- File: `pods.yaml`
- Each pod has labels:
  - `cladding: CLADDING_NAME`
  - `project_root: PROJECT_ROOT`
- No explicit label containing network identifier.

## Existing docs and PRDs
- `README.md` currently documents:
  - `cladding init` auto-selects subnet and creates network `<name>_cladding_net`.
- `docs/features/cladding-rust-port/prd.md` includes subnet-centric behavior and name pattern `<name>_cladding_net`.

## Implications for requested feature
- Removing `subnet` from schema impacts multiple commands, not only `up`, because several commands call `resolve_network_settings(name, subnet)`.
- Config parser should reject or ignore removed field based on strictness policy; user requested no special handling and field removal from schema (i.e., treat old files as out-of-contract).
- Current static `--ip` behavior in `podman play kube` requires deterministic subnet/IP values; runtime network selection must still provide subnet to compute these addresses, or implementation must remove explicit `--ip` flags and redesign host alias assumptions.
- To select an unused `cladding-N` network, the code needs a reliable “network in use by running cladding project” signal:
  - either infer from pod metadata/inspection,
  - or add persisted/project label metadata and lookup,
  - or extend `ps` internals to include network info.
- Shared global pool implies lifecycle and collision rules become product-significant:
  - how `unused` is defined (no running cladding pods vs no attached containers at all),
  - whether non-cladding attachments disqualify a `cladding-N` network from reuse.
- Resolved: “unused” means no running cladding pods attached; non-cladding attachment checks are out of scope by assumption.
