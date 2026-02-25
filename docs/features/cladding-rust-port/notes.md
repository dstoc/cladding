# Research Notes: cladding Rust port

## Existing cladding script behavior (./cladding)
- Entry point: POSIX shell script with subcommands `build`, `init [name]`, `check`, `up`, `down`, `destroy`, `run`, `reload-proxy`, `help`.
- Discovers project root by searching for a `.cladding` directory in cwd or parents.
- Maintains derived paths:
  - `CLADDING_ROOT` = directory containing the `cladding` script.
  - `PROJECT_ROOT` = `.cladding` directory in project.
  - `config` expected at `.cladding/config`, `home` at `.cladding/home`, `tools` at `.cladding/tools`.
- Uses `cladding.json` in `.cladding/` for:
  - `name` (lowercase alnum), `subnet` (CIDR), `sandbox_image`, `cli_image`.
- `build`:
  - Builds `mcp-run` and `run-remote` using a `rust:latest` container; installs into `.cladding/tools/bin/` (`run-remote` renamed to `run-with-network`).
  - Builds default images using `Containerfile.cladding` if images match default `localhost/cladding-default:latest`.
- `init`:
  - Creates `.cladding/` and `.cladding/config` by copying from `config-template`.
  - Writes `.cladding/cladding.json` (default images + subnet auto-selection in `10.90.X.0/24` + generated name).
  - Creates Podman network `<name>_cladding_net` using the selected subnet.
- `check`:
  - Validates required paths, config files, tools binaries, network settings, images exist.
- `up`:
  - Renders `pods.yaml` by simple string substitution and runs `podman play kube` with explicit IPs.
- `down`:
  - `podman play kube --down` on rendered YAML.
- `destroy`:
  - `podman rm -f` on the three pods by name.
- `run`:
  - Executes command in `cli` container with cwd mapped relative to project root; TTY handling for interactive vs non-interactive.
- `reload-proxy`:
  - `podman exec` into proxy container to reload Squid config.

## Related files
- `Containerfile.cladding`: Debian-based image with tooling (zsh, npm, curl, build tools, jq, python, ripgrep). Installs `@openai/codex` and `@google/gemini-cli`, creates user with host UID/GID.
- `pods.yaml`: Podman pods for `proxy`, `sandbox`, `cli` with explicit IPs, host mounts for `.cladding` config/home/tools, and initContainers for nftables jailers.
- `scripts/proxy_startup.sh`: resolves pod IPs, writes `/tmp/cli_ips.lst` and `/tmp/sandbox_ips.lst`, injects DNS into Squid config, starts Squid.
- `scripts/jail_cli.sh` + `scripts/jail_sandbox.sh`: nftables jailer scripts (not inspected yet in this feature).
- `config-template/`:
  - `squid.conf` with placeholders for DNS and ACLs.
  - `cli_domains.lst`, `sandbox_domains.lst`, `cli_host_ports.lst` allowlist files.
  - `sandbox_commands/` rego policy modules.
- `README.md` documents cladding usage, mounts, and architecture.

## Constraints implied by request
- Port `./cladding` to Rust because shell complexity is growing.
- Goal: single binary with embedded/inline `Containerfile` and `config-template` (i.e., no external template files required at runtime).
- Need to propose repo structure and Rust implementation details (dependencies, etc.).

## Decisions from Q&A
- Rust binary replaces `./cladding` (same command name/CLI).
- Subcommands/semantics should match current script.
- Move to a Cargo workspace: `cladding/` as main app, move `mcp-run` under `crates/`.
- Binary should embed and materialize templates (no external template file use).
- Only external dependency should be `podman` (no `jq`).
- Also embed and materialize `scripts/*`; `pods.yaml` can be rendered in-memory and passed to Podman (no file required).
