# Migrating from v0.1 to v0.2

v0.2 contains breaking runtime and configuration renames. Existing projects
must update `.cladding/cladding.json`, move config files into the new
component directories, refresh embedded scripts/tools, and recreate running
pods.

## Quick Checklist

1. Before installing v0.2, stop the old runtime:

   ```bash
   cladding down
   ```

2. Update `.cladding/cladding.json` to the v0.2 schema.
3. Move files under `.cladding/config/` to the normalized layout.
4. Refresh embedded scripts and tools:

   ```bash
   cladding init --update-scripts
   cladding build
   ```

5. Validate the migrated project:

   ```bash
   cladding check
   ```

6. Start the new runtime:

   ```bash
   cladding up
   ```

If old pods or containers remain after `down`, use:

```bash
cladding destroy
```

## cladding.json Changes

Top-level image keys were replaced by component objects.

Old:

```json
{
  "name": "myproject",
  "cli_image": "localhost/cladding-default:latest",
  "sandbox_image": "localhost/cladding-default:latest"
}
```

New:

```json
{
  "name": "myproject",
  "agent": {
    "image": "localhost/cladding-default:latest"
  },
  "nw_sandbox": {
    "enabled": true,
    "image": "localhost/cladding-default:latest"
  }
}
```

Key replacements:

| v0.1 key | v0.2 key |
| --- | --- |
| `cli_image` | `agent.image` |
| `agent_image` | `agent.image` |
| `sandbox_image` | `nw_sandbox.image` |
| `nw_sandbox_image` | `nw_sandbox.image` |

`agent.image`, `nw_sandbox.image`, and `fs_sandbox.image` default to
`localhost/cladding-default:latest` when omitted. `agent` is required and cannot
be disabled. `nw_sandbox` and `fs_sandbox` are optional; when absent, that
sandbox is disabled.

Example with the optional filesystem sandbox enabled:

```json
{
  "name": "myproject",
  "agent": {},
  "nw_sandbox": {
    "enabled": true
  },
  "fs_sandbox": {
    "enabled": true
  }
}
```

v0.2 rejects unknown `cladding.json` keys. This is intentional so old config
is caught early by `cladding check`.

## Mount Target Changes

`mounts[].sandboxOnly` and `mounts[].nwSandboxOnly` were replaced by explicit
targets.

Old:

```json
{
  "mount": "/cache",
  "hostPath": "../cache",
  "nwSandboxOnly": true
}
```

New:

```json
{
  "mount": "/cache",
  "hostPath": "../cache",
  "targets": ["nw-sandbox"]
}
```

Valid targets are:

- `agent`
- `nw-sandbox`
- `fs-sandbox`

When `targets` is omitted, the mount applies to the agent and to `nw-sandbox`
when it is enabled. `fs-sandbox` only receives mounts that explicitly target
`fs-sandbox`.

Duplicate `mount` paths are now checked per target, so the same container path
can be backed by different host paths in different components:

```json
{
  "mounts": [
    {
      "mount": "/workspace",
      "hostPath": "../workspace-ro",
      "readOnly": true,
      "targets": ["agent"]
    },
    {
      "mount": "/workspace",
      "hostPath": "../workspace-rw",
      "targets": ["fs-sandbox"]
    }
  ]
}
```

## Config Directory Layout

Configuration under `.cladding/config/` is now grouped by component.

Move existing files as follows:

| v0.1 path | v0.2 path |
| --- | --- |
| `.cladding/config/cli_domains.lst` | `.cladding/config/agent/domains.lst` |
| `.cladding/config/cli_host_ports.lst` | `.cladding/config/agent/host_ports.lst` |
| `.cladding/config/agent_domains.lst` | `.cladding/config/agent/domains.lst` |
| `.cladding/config/agent_host_ports.lst` | `.cladding/config/agent/host_ports.lst` |
| `.cladding/config/sandbox_commands/` | `.cladding/config/nw_sandbox/` |
| `.cladding/config/sandbox_domains.lst` | `.cladding/config/nw_sandbox/domains.lst` |
| `.cladding/config/nw_sandbox_domains.lst` | `.cladding/config/nw_sandbox/domains.lst` |
| `.cladding/config/squid.conf` | `.cladding/config/proxy/squid.conf` |

For the optional filesystem sandbox, v0.2 uses:

```text
.cladding/config/fs_sandbox/main.rego
```

`cladding init` creates the template file. If `fs_sandbox` is enabled,
`cladding check` requires `fs_sandbox/main.rego`.

After moving files, remove the old paths. `cladding check` reports old file
names explicitly, for example:

```text
error: legacy config/sandbox_commands exists (...)
hint: replace config/sandbox_commands with config/nw_sandbox
```

## Runtime Name Changes

The project runtime vocabulary changed:

| v0.1 term | v0.2 term |
| --- | --- |
| `cli` | `agent` |
| `sandbox` | `nw-sandbox` |

Pod and DNS names are now:

- `<name>-agent`
- `<name>-proxy`
- `<name>-nw-sandbox`
- `<name>-fs-sandbox`, only when enabled

Raw Podman container names now use the `instance` app container suffix:

- `<name>-agent-instance`
- `<name>-proxy-instance`
- `<name>-nw-sandbox-instance`
- `<name>-fs-sandbox-instance`, only when enabled

Prefer `cladding` commands over raw Podman names:

```bash
cladding run <cmd>
cladding run-with-scissors --target nw-sandbox -- <cmd>
cladding run-with-scissors --target fs-sandbox -- <cmd>
cladding logs agent -f
cladding logs proxy -f
cladding logs nw-sandbox -f
cladding logs fs-sandbox -f
```

## Sandbox Command Helpers

`run-with-network` is no longer installed by `cladding build`.

v0.2 installs:

- `.cladding/tools/bin/mcp-run`
- `.cladding/tools/bin/run-remote`
- `.cladding/tools/bin/run-in-nw-sandbox`
- `.cladding/tools/bin/run-in-fs-sandbox`

From inside the agent container, use:

```bash
run-in-nw-sandbox -- <cmd> [args...]
run-in-fs-sandbox -- <cmd> [args...]
```

From the host, use:

```bash
cladding run-with-scissors --target nw-sandbox -- <cmd> [args...]
cladding run-with-scissors --target fs-sandbox -- <cmd> [args...]
```

`--target` defaults to `nw-sandbox`.

## Optional Sandboxes

`nw_sandbox` can now be disabled:

```json
{
  "name": "myproject",
  "agent": {},
  "nw_sandbox": {
    "enabled": false
  }
}
```

`fs_sandbox` is disabled unless configured. It uses `mcp-run` and Rego policy
like `nw_sandbox`, but it is intended for different filesystem access rather
than broader network access. It does not receive proxy access and its jail
blocks new outbound network traffic.

Enable it with:

```json
{
  "fs_sandbox": {
    "enabled": true
  }
}
```

Then customize mounts with `targets: ["fs-sandbox"]` as needed.

## Validation and Troubleshooting

Run:

```bash
cladding check
```

`check` now catches:

- old top-level config keys such as `cli_image` and `sandbox_image`
- old config file names such as `sandbox_commands` and `squid.conf`
- missing nested config files such as `agent/domains.lst`
- missing `fs_sandbox/main.rego` when `fs_sandbox` is enabled
- mounts that target disabled components

If you still see old runtime names in `podman ps`, stop and recreate the
runtime:

```bash
cladding down
cladding up
```

If cleanup fails because old resources are stuck:

```bash
cladding destroy
cladding up
```
