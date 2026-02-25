# Notes: mcp-run Rego Policy Engine

## Request Summary
- Replace the current JSON-based policy validation in `mcp-run` with a Rego-based policy engine using `regorus`.
- Use a router policy like `package sandbox.main` with `allow` delegating to command-specific policies under `data.sandbox[...]`.
- Add hot reload using filesystem notifications/watching for policy folder changes.
- Expected policy input includes:
  - `command` (requested executable token, e.g. `curl`)
  - `path` (resolved executable path, e.g. `/usr/bin/curl`)
  - `args` (argument vector)

## Existing Product Findings

### Current policy architecture (`crates/mcp-run/src/policy.rs`)
- Policy file is a single JSON file loaded from `POLICY_FILE` via `load_policy`.
- Top-level policy shape is `Vec<CommandRule>` with strict serde schema and validation for regex patterns.
- Validation is Rust-implemented logic (`validate_invocation`) with rule OR semantics by command name.
- Rule features today:
  - arg checks: `exact`, `regex`, `hash` (SHA-256), with optional `position` and `required`
  - env allowlist by key per rule (`env: Vec<String>`)
- Errors are explicit and user-facing:
  - `CommandNotAllowed`
  - `RuleValidationFailed` with details across attempted rules.

### Current server wiring (`crates/mcp-run/src/mcp.rs`, `crates/mcp-run/src/raw.rs`, `crates/mcp-run/src/executor.rs`)
- `AppConfig` requires `POLICY_FILE` and loads policy once at startup in `serve`.
- Loaded policy is stored in `Arc<Policy>` and injected into both MCP (`/mcp`) and raw (`/raw`) paths.
- Both execution paths call `spawn_network_tool_process`, which calls `validate_invocation` before process spawn.
- There is no policy hot reload today.

### Config/runtime surface
- `POLICY_FILE` currently points to `/opt/config/sandbox_commands.json` in both `Containerfile.sandbox` and `pods.yaml`.
- `README.md` and `config-template/sandbox_commands.json` document JSON policy format as current user-facing contract.
- `cladding` build step installs `run-remote` as `run-with-network` helper; helper behavior depends on policy-enforced command execution success.

### Dependency status
- `regorus = "0.9.1"` is already present in `crates/mcp-run/Cargo.toml`.
- No file watcher crate is currently declared (likely need to add one, e.g. `notify`).

## Existing PRD Style Findings
- Feature docs are stored at `docs/features/<feature>/` with `notes.md`, `questions.md`, and `prd.md`.
- PRDs are structured with sections: objective, use cases, functional requirements, non-goals, design considerations, technical considerations, success metrics, open questions.

## Initial Design Implications
- Rego engine should become the shared policy gate used by both `/mcp` and `/raw` paths to avoid drift.
- Need to define canonical policy layout (single file vs folder with router + per-command modules).
- Need to define policy decision contract (query path, expected boolean/object return, error mapping).
- Hot reload likely requires:
  - watcher lifecycle tied to server startup
  - debounce/coalesce for editor write bursts
  - atomic swap of compiled policy engine (e.g. `ArcSwap`/`RwLock`)
  - behavior when reload fails (keep last-known-good vs fail-closed)
- Migration/documentation updates are required because current public docs and templates reference JSON policy.

## Ambiguities To Resolve
- Exact on-disk policy directory structure and required entrypoint file names.
- Whether env gating remains part of v1 Rego input/decisions (user listed only `command`, `path`, `args`).
- Reload failure behavior and startup strictness for invalid policy sets.
- Backward compatibility strategy for existing `sandbox_commands.json` users.

## Clarified Decisions From Q&A
- Primary authorization key is `input.command`.
- `input.path` remains available for optional stricter checks in policy modules.
- Rego input contract will include `env` in addition to `command`, `path`, and `args`.
- Reload failure mode is fail-closed: deny all invocations until a valid policy set is loaded.
- Migration requires temporary dual support: Rego directory engine plus legacy JSON fallback; JSON support will be removed in a later feature.
- Startup behavior is also fail-closed: if no valid policy can be loaded, server still starts in deny-all mode and continues watching for policy fixes.
- Current ambiguities are resolved enough to draft PRD.
- Config split decision: `POLICY_DIR` is the Rego policy directory, while `POLICY_FILE` remains legacy JSON during transition.
