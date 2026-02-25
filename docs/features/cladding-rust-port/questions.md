# Questions: cladding Rust port

1. Q: Should the Rust implementation replace the `./cladding` script (same command name/CLI), or ship as a new binary with a wrapper?
   A: Replace it (same command name/CLI).

2. Q: Do you want to keep the exact same subcommands and semantics (`build`, `init`, `check`, `up`, `down`, `destroy`, `run`, `reload-proxy`, `help`) for the first Rust version, or are there any additions/removals/renames you want now?
   A: Match the commands/semantics.

3. Q: Where should the Rust source live in the repo? For example `cladding/` as a Cargo workspace crate, or under `tools/cladding`, or `mcp-run` workspace?
   A: Use a Cargo workspace. Have `cladding/Cargo.toml` for the main app, and move `mcp-run` into `crates/` along with any other crates.

4. Q: Should the single `cladding` binary embed the `Containerfile.cladding` and `config-template` contents and materialize them into `.cladding/` on `init`, or should it also support reading external template files when present?
   A: Embed and materialize, always.

5. Q: Do you want the Rust `cladding` binary to eliminate `jq` dependency (parse `cladding.json` directly), and to implement network/subnet selection internally (no shell helpers), or keep any external tool requirements?
   A: Yes. The only external dependency should be `podman`.

6. Q: Beyond `Containerfile.cladding` and `config-template`, should the binary also embed and materialize `pods.yaml` and `scripts/*` (jailer + proxy startup), so it is fully self-contained without relying on those files in the repo?
   A: Yes. `pods.yaml` can be rendered in-memory and passed to Podman (no file needed). Scripts should be materialized under `.cladding/scripts`.
