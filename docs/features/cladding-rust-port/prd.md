# PRD: Rust Port of `cladding`

## Objective
Replace the `./cladding` shell script with a Rust binary that preserves the exact CLI, behavior, and semantics while reducing script complexity and removing external tool dependencies (other than `podman`). The binary should be self-contained: it embeds and materializes `Containerfile.cladding`, `config-template/`, and runtime scripts; `pods.yaml` is rendered in-memory and piped to `podman`.

## Use Cases
1. New project setup via `./cladding init` creates `.cladding/`, writes `cladding.json`, materializes embedded templates, and provisions the Podman network.
2. Developers run `./cladding build` to compile `mcp-run` and `run-with-network`, and build default images using the embedded Containerfile.
3. Teams use `./cladding up` and `./cladding down` to start/stop the pods with the same network, IP, and mount semantics as today.
4. Users run commands inside the CLI container with `./cladding run <cmd>` preserving cwd-relative logic and TTY behavior.
5. Operators reload Squid with `./cladding reload-proxy` after updating allowlists.

## Functional Requirements

### 1. CLI Compatibility
1. The Rust binary replaces `./cladding` and preserves exact subcommands and semantics:
   - `build`, `init [name]`, `check`, `up`, `down`, `destroy`, `run`, `reload-proxy`, `help`.
2. CLI output and error messages should remain close enough for users to recognize (minor wording changes allowed if meaning is preserved).
3. Exit codes must match current behavior (non-zero on errors; zero on success).

### 2. Embedded Assets and Materialization
1. The binary must embed these assets and materialize them into `.cladding/` (or use them in-memory):
   - `Containerfile.cladding` (embedded string, used as build input).
   - `config-template/` contents (materialized under `.cladding/config/`).
   - `scripts/*` (materialized under `.cladding/scripts/`).
   - `pods.yaml` (embedded template, rendered in-memory and piped to `podman play kube`).
2. `init` should write templates only when missing, preserving user-edited files in `.cladding/`.

### 3. Config and Validation
1. `cladding.json` is parsed directly by Rust (no `jq`).
2. Validation logic matches the shell script:
   - `name` is lower-case alphanumeric.
   - `subnet` is valid CIDR in IPv4 and large enough for required IPs.
   - `cli_image` and `sandbox_image` are strings.
3. The Rust version must reimplement:
   - subnet selection in `10.90.X.0/24` space,
   - network name derivation (`<name>_cladding_net`),
   - derived IPs for proxy/sandbox/cli.

### 4. Podman Integration
1. The only external dependency is `podman` (invoked via `std::process::Command`).
2. Network creation/inspection behavior must match:
   - `podman network exists` and `podman network inspect` semantics.
3. Pod lifecycle behavior must match:
   - `podman play kube` to start/stop.
   - `podman rm -f` for `destroy`.
4. `reload-proxy` must exec `squid -k reconfigure -f /tmp/squid_generated.conf` in the proxy container.

### 5. Build Behavior
1. `build` compiles `mcp-run` and `run-remote` via a containerized Rust build (matching current approach) and installs into `.cladding/tools/bin/` as:
   - `mcp-run`
   - `run-with-network` (renamed from `run-remote`).
2. Default images are built with the embedded `Containerfile.cladding` when configured image names equal `localhost/cladding-default:latest`.

### 6. Runtime Execution (`run`)
1. Maintains cwd-relative mapping to `/home/user/workspace` inside the CLI container.
2. Preserves TTY behavior (interactive vs non-interactive).
3. Maintains env injection of `LANG`, `TERM`, `COLORTERM`, and `FORCE_COLOR` consistent with current script.

## Non-Goals
1. Changing the CLI interface, behavior, or output contract in this migration.
2. Modifying pod specs or container behavior outside of what is required to embed assets.
3. Introducing a dependency on additional external tools (e.g., `jq`, `sed`, `awk`).
4. Reworking the security model, network ACLs, or policy enforcement.
5. Introducing new subcommands or removing existing ones.

## Design Considerations
1. **Self-contained binary**: embed all templates and scripts to simplify distribution and reduce repo runtime dependencies.
2. **Parity-first**: behavioral compatibility over optimization; avoid breaking existing workflows.
3. **Minimal external surface**: only `podman` is required; everything else is internalized.
4. **Preserve user edits**: do not overwrite `.cladding/config/*` or `.cladding/scripts/*` if they already exist.

## Technical Considerations

### 1. Repo Structure Proposal
Use a Cargo workspace at repo root:
- `Cargo.toml` (workspace root)
- `cladding/` (Rust binary crate for the CLI)
- `crates/`
  - `crates/mcp-run/` (moved from current `mcp-run/`)
  - optional shared crates (e.g., `cladding-core` for config/IP math/podman helpers)

### 2. Rust Implementation Sketch
- **CLI framework**: `clap` (derive-based) for subcommands and flags.
- **Serialization**: `serde`, `serde_json` for `cladding.json`.
- **Templating**: minimal string replacement (e.g., `replace`) or `tinytemplate` if needed for clarity.
- **Asset embedding**:
  - `include_str!`/`include_bytes!` or `rust-embed` for directories.
  - Materialize with `std::fs` and preserve existing files.
- **Process execution**:
  - `std::process::Command` with careful stdout/stderr handling.
- **Error handling**: `thiserror` + `anyhow` for ergonomic error messages.
- **Logging**: simple stderr output; optional `tracing` if structured logs are desired.

### 3. Subnet/IP Calculations
- Reimplement `ipv4_to_int` / `int_to_ipv4` in Rust and ensure:
  - subnet validation and broadcast calculations match current logic,
  - reserved IPs for proxy/sandbox/cli are consistent.

### 4. Podman Interaction
- Encapsulate Podman calls behind a small helper module for:
  - `network exists`, `network inspect`, `network create`,
  - `play kube` (stdin input),
  - `rm -f`, `exec`.
- Ensure error messages include stderr from Podman for user clarity.

## Success Metrics
1. All existing `cladding` commands behave identically in a fresh repo.
2. No dependency on `jq` or other external tools besides `podman`.
3. `init` produces the same folder structure and config contents as today.
4. `up`/`down`/`destroy` operate correctly with embedded templates.
5. Existing user workflows continue without change (CLI compatibility confirmed).

## Open Questions
1. Should we remove or deprecate the on-disk `Containerfile.cladding`, `pods.yaml`, and `scripts/*` from the repo once embedding is complete, or keep them as sources of truth for maintenance?
2. Do we want a `--print-templates` or `--dump-embedded` debug flag for auditing embedded content?
3. Should the Rust binary tolerate missing `podman` with a specific exit code and message, or fail generically?
