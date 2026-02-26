# Research Notes: Cladding mounts + volumes

## Codebase
- `cladding.json` is read from `.cladding/cladding.json` by `cladding/src/config.rs`.
  - Schema currently expects string keys only: `name`, `subnet`, `sandbox_image`, `cli_image`.
  - `Config` struct only includes those four fields.
- `cladding init` writes `.cladding/cladding.json` and materializes `.cladding/config/` + `.cladding/scripts/` from embedded templates.
- `pods.yaml` is embedded as a raw string and rendered via simple `String::replace` in `cladding/src/assets.rs::render_pods_yaml`.
  - Placeholders: `PROJECT_ROOT`, `REPLACE_*` for pod names, images, IPs.
  - No YAML parsing or structural manipulation today.
- Mounts in `pods.yaml` are currently fixed:
  - `proxy`: `config-dir` -> `/opt/config`, `scripts-dir` -> `/opt/scripts` (both `hostPath`).
  - `sandbox-app` + `cli-app`:
    - `/opt/config` (read-only), `/opt/tools` (read-only), `/home/user` (rw), `/home/user/workspace` (rw), `/home/user/workspace/.cladding` masked via `emptyDir`.
  - `sandbox-node` + `cli-node`: `/opt/scripts` (read-only).
- Workspace mount uses hostPath `PROJECT_ROOT/..` (project root’s parent) mapped to `/home/user/workspace`.
- `.cladding` inside the workspace is masked by a separate `emptyDir` volume mounted at `/home/user/workspace/.cladding`.

## Docs
- `README.md` documents current mounts and their purposes in a table.
- Note: README claims `cladding.json` lives at `.cladding/config/cladding.json`, but code reads `.cladding/cladding.json` (inconsistency).

## Existing PRDs
- `docs/features/cladding-rust-port/prd.md` confirms `pods.yaml` is embedded and rendered in-memory via string replacement.
- `docs/features/cladding-rust-port/notes.md` documents the same fixed mount set and the simple template substitution approach.

## Files of interest
- `cladding/src/config.rs`
- `cladding/src/assets.rs`
- `cladding/src/cli.rs`
- `pods.yaml`
- `README.md`
