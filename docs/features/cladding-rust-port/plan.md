# Plan: Make `cladding` More Idiomatic Rust

Goal: evolve the current shell-port into a modular, testable Rust CLI while preserving behavior.

## Step 1: Project Structure + Errors
- Split `cladding/src/main.rs` into modules: `cli`, `config`, `network`, `podman`, `assets`, `fs`, `error`.
- Introduce `thiserror` + `anyhow` for error handling; remove manual `ExitCode` plumbing.
- Keep behavior identical; only refactor and error surface improvements.

## Step 2: CLI Modernization
- Move to `clap` derive for subcommands, help, and argument validation.
- Add `--project-root` override (hidden or documented) to ease testing and scripting.
- Ensure output strings remain compatible.

## Step 3: Podman Wrapper
- Implement a `Podman` helper struct with typed methods (`network_exists`, `network_inspect`, `build_image`, `play_kube`, etc.).
- Standardize error messages with captured stderr on failure.

## Step 4: Assets + FS Helpers
- Replace manual embedded file lists with `include_dir` or `rust-embed`.
- Centralize file materialization, permissions, and symlink checks.

## Step 5: Pure-Function Tests
- Add unit tests for:
  - name normalization
  - CIDR parsing + IP math
  - YAML rendering substitutions (golden test)
- Add temp-dir tests for `init` (skip Podman-dependent sections).

## Step 6: Documentation + Dev UX
- Add `cladding/README.md` or move CLI docs there.
- Optional: `cargo xtask` for build/check/lint flows.

---
After each step, I will check back with you before proceeding to the next.
