# PRD: Config Layout Normalization

## Objective
Normalize the `.cladding/config` layout so component-specific config lives under the component directory that consumes it:

- agent config under `.cladding/config/agent/`
- network-sandbox config under `.cladding/config/nw_sandbox/`
- proxy config under `.cladding/config/proxy/`

The first concrete migration is to move the current flat allowlist files:

- `.cladding/config/agent_domains.lst` becomes `.cladding/config/agent/domains.lst`
- `.cladding/config/agent_host_ports.lst` becomes `.cladding/config/agent/host_ports.lst`
- `.cladding/config/nw_sandbox_domains.lst` becomes `.cladding/config/nw_sandbox/domains.lst`

The recommended additional normalization is to move the proxy config:

- `.cladding/config/squid.conf` becomes `.cladding/config/proxy/squid.conf`

## Motivation
The runtime renaming work removed old `cli` and generic `sandbox` vocabulary, but the config layout is still partly flat:

```text
.cladding/config/
  agent_domains.lst
  agent_host_ports.lst
  nw_sandbox/
    main.rego
    curl.rego
  nw_sandbox_domains.lst
  squid.conf
```

This makes related files look unrelated. For example, agent network policy is split between root-level files, while network-sandbox command policy is grouped under `nw_sandbox/` but its domain allowlist remains at the root. The result is still understandable, but it is harder to extend cleanly when more component-specific settings are added.

Grouping files by runtime component makes the config shape match the product model:

```text
.cladding/config/
  agent/
    domains.lst
    host_ports.lst
  nw_sandbox/
    domains.lst
    main.rego
    curl.rego
  proxy/
    squid.conf
```

## Problem statement
`cladding/src/cli.rs` currently treats these config entries as top-level required paths:

- `agent_domains.lst`
- `agent_host_ports.lst`
- `nw_sandbox`
- `nw_sandbox_domains.lst`
- `squid.conf`

`config-template/squid.conf` also points Squid at top-level mounted paths:

- `/opt/config/agent_domains.lst`
- `/opt/config/agent_host_ports.lst`
- `/opt/config/nw_sandbox_domains.lst`

`scripts/jail_agent.sh` reads host port policy from:

- `/opt/config/agent_host_ports.lst`

Because `config-template/` is embedded by `cladding/src/assets.rs` and materialized into `.cladding/config/`, changing the template layout changes both new-project initialization and the runtime paths mounted into containers.

The migration must be explicit. If an existing project still has the old flat files, `cladding check` should fail with targeted replacement hints instead of letting the runtime fail later with missing allowlists.

## Proposal
Move component-specific config into component directories and update all runtime paths to match.

### Agent config
Create a new template directory:

```text
config-template/agent/
  domains.lst
  host_ports.lst
```

Runtime paths become:

- `.cladding/config/agent/domains.lst`
- `.cladding/config/agent/host_ports.lst`
- `/opt/config/agent/domains.lst`
- `/opt/config/agent/host_ports.lst`

Update the consumers:

- `config-template/squid.conf` should read agent domains from `/opt/config/agent/domains.lst`.
- `config-template/squid.conf` should read agent host ports from `/opt/config/agent/host_ports.lst`.
- `scripts/jail_agent.sh` should read host ports from `/opt/config/agent/host_ports.lst`.
- README and active feature docs should refer to the grouped paths.

### Network-sandbox config
Keep the existing `nw_sandbox/` directory as the network-sandbox policy directory and add its domain allowlist beside the Rego modules:

```text
config-template/nw_sandbox/
  domains.lst
  main.rego
  curl.rego
```

Runtime paths become:

- `.cladding/config/nw_sandbox/domains.lst`
- `/opt/config/nw_sandbox/domains.lst`

Update `config-template/squid.conf` to read network-sandbox domains from `/opt/config/nw_sandbox/domains.lst`.

This intentionally keeps `POLICY_DIR=/opt/config/nw_sandbox`. `mcp-run` already walks the policy directory recursively and only loads files whose extension is `.rego`, so a sibling `domains.lst` file should not affect policy compilation. The implementation should include a test or verification step for this behavior because it is the main compatibility risk of placing non-Rego config in the policy directory.

### Proxy config
Move Squid config into a proxy-specific directory:

```text
config-template/proxy/
  squid.conf
```

Runtime path becomes:

- `.cladding/config/proxy/squid.conf`
- `/opt/config/proxy/squid.conf`

Update the proxy pod/script integration so Squid reads the grouped config path. The current startup script renders placeholders from the mounted Squid config into `/tmp/squid_generated.conf`; that behavior should remain, but the source file should be `/opt/config/proxy/squid.conf`.

This is the only additional normalization recommended for this follow-on. It completes the same grouping rule for the proxy component and removes the last component-specific root-level config file.

### Required config checks
Update `cladding/src/cli.rs` so `required_config_entries()` requires:

- `agent`
- `nw_sandbox`
- `proxy`

The top-level directory check is not enough on its own. `cladding check` should also validate the required nested files:

- `agent/domains.lst`
- `agent/host_ports.lst`
- `nw_sandbox/domains.lst`
- at least one `nw_sandbox/**/*.rego` policy file through the existing runtime or a lightweight check
- `proxy/squid.conf`

If the implementation keeps `required_config_entries()` as a flat list of relative paths instead of top-level entries, the required list can be:

- `agent/domains.lst`
- `agent/host_ports.lst`
- `nw_sandbox`
- `nw_sandbox/domains.lst`
- `proxy/squid.conf`

Either implementation is acceptable as long as the error output names the missing relative path clearly.

### Legacy config checks
Extend the current legacy config detection in `cladding/src/cli.rs` to reject both the pre-runtime-renaming names and the newly superseded flat names.

Existing legacy checks should remain:

- `sandbox_commands` -> `nw_sandbox`
- `sandbox_domains.lst` -> `nw_sandbox/domains.lst`
- `cli_domains.lst` -> `agent/domains.lst`
- `cli_host_ports.lst` -> `agent/host_ports.lst`

Add checks for the intermediate flat names:

- `agent_domains.lst` -> `agent/domains.lst`
- `agent_host_ports.lst` -> `agent/host_ports.lst`
- `nw_sandbox_domains.lst` -> `nw_sandbox/domains.lst`
- `squid.conf` -> `proxy/squid.conf`

`cladding check` should report all detected legacy paths in one pass, matching the behavior added during runtime renaming. It should not accept old flat paths as aliases.

### Materialization behavior
`cladding init` should materialize the new grouped template layout for new projects.

For existing projects, `cladding init` without an explicit migration mode should not delete or move user-edited files. The safe behavior is:

- create missing new grouped template files when they do not exist
- leave old flat files in place
- rely on `cladding check` to fail with explicit replacement hints until the user migrates or removes the old paths

If a future migration command is added, it can move user files automatically. That is not required for this proposal.

## Non-goals
1. Do not add compatibility aliases for old flat config paths.
2. Do not rename `nw_sandbox` back to a generic `sandbox` directory.
3. Do not change Rego package names; `package sandbox.*` remains valid.
4. Do not redesign `cladding.json` schema or move image/mount settings out of `cladding.json`.
5. Do not introduce a generic config registry or plugin system.
6. Do not move `.cladding/tools`, `.cladding/scripts`, or `.cladding/home`; this proposal only covers `.cladding/config`.

## Suggested implementation shape
1. Move template files:
   - `config-template/agent_domains.lst` to `config-template/agent/domains.lst`
   - `config-template/agent_host_ports.lst` to `config-template/agent/host_ports.lst`
   - `config-template/nw_sandbox_domains.lst` to `config-template/nw_sandbox/domains.lst`
   - `config-template/squid.conf` to `config-template/proxy/squid.conf`
2. Update runtime file references in:
   - `config-template/proxy/squid.conf`
   - `scripts/jail_agent.sh`
   - the proxy startup path in `pods.yaml` or `scripts/proxy_startup.sh`, depending on where the source Squid config path is currently defined
3. Update `cladding/src/cli.rs` required and legacy config checks.
4. Add tests for required nested config paths and legacy flat-path errors.
5. Update README, active feature docs, examples, and skill references that mention the old flat paths.

## Migration plan
Manual migration for an existing project should be:

```bash
mkdir -p .cladding/config/agent .cladding/config/proxy
mv .cladding/config/agent_domains.lst .cladding/config/agent/domains.lst
mv .cladding/config/agent_host_ports.lst .cladding/config/agent/host_ports.lst
mv .cladding/config/nw_sandbox_domains.lst .cladding/config/nw_sandbox/domains.lst
mv .cladding/config/squid.conf .cladding/config/proxy/squid.conf
cladding check
```

Projects that still have pre-renaming paths should migrate directly to the normalized paths:

- `.cladding/config/cli_domains.lst` -> `.cladding/config/agent/domains.lst`
- `.cladding/config/cli_host_ports.lst` -> `.cladding/config/agent/host_ports.lst`
- `.cladding/config/sandbox_domains.lst` -> `.cladding/config/nw_sandbox/domains.lst`
- `.cladding/config/sandbox_commands/` -> `.cladding/config/nw_sandbox/`

## Verification
1. `cladding init` creates:
   - `.cladding/config/agent/domains.lst`
   - `.cladding/config/agent/host_ports.lst`
   - `.cladding/config/nw_sandbox/domains.lst`
   - `.cladding/config/nw_sandbox/*.rego`
   - `.cladding/config/proxy/squid.conf`
2. `cladding check` passes for a project with only the normalized config layout.
3. `cladding check` fails with replacement hints when any old flat file exists:
   - `agent_domains.lst`
   - `agent_host_ports.lst`
   - `nw_sandbox_domains.lst`
   - `squid.conf`
   - `cli_domains.lst`
   - `cli_host_ports.lst`
   - `sandbox_domains.lst`
   - `sandbox_commands`
4. `mcp-run` continues to load Rego policies from `POLICY_DIR=/opt/config/nw_sandbox` while ignoring `domains.lst`.
5. Rendered runtime config points Squid at:
   - `/opt/config/agent/domains.lst`
   - `/opt/config/agent/host_ports.lst`
   - `/opt/config/nw_sandbox/domains.lst`
6. The agent jail script reads host-port allowlist data from `/opt/config/agent/host_ports.lst`.
7. `cargo fmt --check` and `cargo test --workspace` pass.

## Success criteria
1. No component-specific config files remain at the root of `config-template/` except directories.
2. New projects use only grouped config paths.
3. Existing projects with old flat paths fail `cladding check` with explicit migration guidance.
4. Proxy, agent jail, and network-sandbox policy loading all use the normalized paths.
5. User-facing docs describe one config layout without mixed flat and grouped examples.
