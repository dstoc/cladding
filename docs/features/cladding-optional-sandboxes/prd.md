# PRD: Optional Execution Sandboxes

## Objective
Add an optional filesystem sandbox runtime alongside the existing network sandbox, and clean up the configuration model so each execution component is configured through a component object:

- `agent`
- `nw_sandbox`
- `fs_sandbox`

The filesystem sandbox uses `mcp-run`, like `nw-sandbox`, but is intended to grant broader or different filesystem access rather than broader network access. It should only exist when configured and enabled.

This proposal also replaces the mount-specific `nwSandboxOnly` boolean with explicit mount targets so mounts can apply to `agent`, `nw-sandbox`, `fs-sandbox`, or a deliberate subset.

## Motivation
The current runtime has three pods:

- `<name>-proxy`
- `<name>-nw-sandbox`
- `<name>-agent`

The agent runs user-facing commands with constrained network access. When a command needs network access, the agent delegates to the `mcp-run` server in `<name>-nw-sandbox`. That model is useful, but it only expresses one kind of privilege separation: network access.

Some workflows need a different split. For example, the agent may need broad read-only visibility into a workspace, while a delegated tool needs read-write access to generated files, build directories, package caches, or a separate working tree. That should not require giving the main agent broader write access, and it should not be modeled as "network sandbox only" just because `nw-sandbox` is currently the only secondary execution container.

A dedicated `fs-sandbox` keeps the model understandable:

- `nw-sandbox`: delegated command execution with controlled network access
- `fs-sandbox`: delegated command execution with controlled filesystem access
- `agent`: main interactive environment, always present

## Problem statement
The current implementation assumes there is exactly one optional-ish sandbox concept, and that concept is the network sandbox.

In `cladding/src/config.rs`, `Config` has top-level image fields:

- `nw_sandbox_image`
- `agent_image`

Mount entries have a special-case boolean:

- `mounts[].nwSandboxOnly`

In `cladding/src/pods.rs`, mount injection is hard-coded to app containers named `agent` and `nw-sandbox`, and `nwSandboxOnly` is applied by checking whether the current container name is `nw-sandbox`.

In `cladding/src/network.rs`, `NetworkSettings` has one sandbox identity:

- `sandbox_ip`
- `sandbox_name`

In `pods.yaml`, the agent always receives:

- host aliases for `<name>-nw-sandbox`
- `CLADDING_SANDBOX_NAME`
- `RUN_REMOTE_SERVER=http://<name>-nw-sandbox:3000/raw`

In `scripts/jail_agent.sh`, the agent jailer always resolves the network sandbox and allows direct traffic to it on port `3000`.

This makes `fs-sandbox` awkward to add directly. A second sandbox would force ambiguous naming, boolean mount flags would not scale, and disabling either sandbox would leave stale DNS, env vars, jail rules, and CLI assumptions.

## Proposal
Introduce explicit component config for execution containers and make secondary sandboxes optional.

### Config schema
Replace the top-level image keys with component objects:

```json
{
  "name": "demo",
  "agent": {
    "image": "cladding-agent:local"
  },
  "nw_sandbox": {
    "enabled": true,
    "image": "cladding-sandbox:local"
  },
  "fs_sandbox": {
    "enabled": true,
    "image": "cladding-sandbox:local"
  }
}
```

`agent` is required and cannot be disabled.

`nw_sandbox` and `fs_sandbox` are optional component objects:

- if the object is absent, the component is disabled
- if the object is present and `enabled` is absent, the component is enabled
- if `enabled` is `false`, the component is disabled
- if `image` is absent, the component uses the default cladding build image

`agent.enabled` should not be accepted. The agent is a required runtime component, so accepting an ignored `enabled` key would create misleading config.

The old top-level keys should be rejected, not accepted as aliases:

- `agent_image` -> `agent.image`
- `nw_sandbox_image` -> `nw_sandbox.image`
- `sandbox_image` -> `nw_sandbox.image`
- `cli_image` -> `agent.image`

`cladding.json` already rejects unknown keys in `cladding/src/config.rs`; extend that behavior to reject old keys with targeted hints.

### Generated default config
`cladding init` should continue creating a useful default project that supports current workflows. The generated config should include `agent` and `nw_sandbox` enabled by default:

```json
{
  "name": "demo",
  "agent": {
    "image": "cladding-default:latest"
  },
  "nw_sandbox": {
    "enabled": true,
    "image": "cladding-default:latest"
  }
}
```

It should not include `fs_sandbox` by default. A filesystem sandbox should only exist when a project explicitly opts in.

This keeps new project behavior close to today while allowing projects to disable network delegation by setting:

```json
{
  "nw_sandbox": {
    "enabled": false
  }
}
```

### Filesystem sandbox runtime
When `fs_sandbox.enabled` is true, render a fourth pod:

- pod metadata name: `<name>-fs-sandbox`
- app container name: `fs-sandbox`
- default app command: `mcp-run`
- raw endpoint: `http://<name>-fs-sandbox:3000/raw`
- policy directory: `/opt/config/fs_sandbox`
- project config directory: `.cladding/config/fs_sandbox/`
- template config directory: `config-template/fs_sandbox/`

The initial filesystem sandbox uses the default cladding build image unless configured otherwise. It should still use policy-enforced `mcp-run`; filesystem privilege does not imply unrestricted command execution.

The default `config-template/fs_sandbox/` should include minimal Rego policy files equivalent in shape to `config-template/nw_sandbox/`, but it should not need a `domains.lst` file.

### Network behavior
The `fs-sandbox` is about filesystem access, not network expansion. It should not be able to communicate with the proxy and should not have outbound network access.

Recommended initial behavior:

- `fs-sandbox` runs `mcp-run` on port `3000`.
- The agent can connect directly to `<name>-fs-sandbox:3000` when `fs_sandbox` is enabled.
- `fs-sandbox` receives no proxy environment variables.
- `fs-sandbox` has no host alias for `<name>-proxy`.
- `fs-sandbox` jail rules allow loopback and established traffic, then drop all new outbound traffic.
- `fs-sandbox` should only receive run requests from the agent; it should not initiate network requests to the proxy, the host, the network sandbox, or the internet.

This makes the first implementation narrow: `fs-sandbox` adds filesystem separation without also becoming a second network policy surface.

### Agent environment
Replace the single ambiguous remote variable with component-specific endpoints.

When `nw_sandbox` is enabled, set:

```text
RUN_NW_SANDBOX_SERVER=http://<name>-nw-sandbox:3000/raw
```

When `fs_sandbox` is enabled, set:

```text
RUN_FS_SANDBOX_SERVER=http://<name>-fs-sandbox:3000/raw
```

The agent jailer should allow direct outbound traffic to enabled sandbox endpoints only:

- allow `<name>-nw-sandbox:3000` only when `nw_sandbox` is enabled
- allow `<name>-fs-sandbox:3000` only when `fs_sandbox` is enabled

If no secondary sandbox is enabled, the agent should not wait for or allow any sandbox endpoint.

### CLI behavior
`cladding run` remains scoped to the agent container.

`cladding run-with-scissors` currently targets `nw-sandbox`. With optional sandboxes, it should require the target sandbox to be enabled.

Recommended CLI shape:

```bash
cladding run-with-scissors --target nw-sandbox -- <cmd>
cladding run-with-scissors --target fs-sandbox -- <cmd>
```

The `--target` option is optional and defaults to `nw-sandbox`. If `nw_sandbox` is disabled, the command should fail with:

- a clear error that `nw_sandbox` is disabled
- a hint to enable it in `cladding.json` or choose `--target fs-sandbox` if available

No new host-facing `cladding` subcommand is required for the first implementation.

The existing `run-with-network` helper should be removed as part of this change. It names the old assumption that the only delegated execution target is the network sandbox.

There are two command layers:

- host/user CLI: `cladding run-with-scissors`, with `--target` when the user needs a target other than the default `nw-sandbox`
- in-agent helpers: `run-in-nw-sandbox` and `run-in-fs-sandbox`

The in-agent helpers should be small wrapper scripts that set the appropriate `RUN_REMOTE_SERVER` value for the child process and then delegate to the low-level `run-remote` client. Runtime pod env should still expose component-specific endpoint variables (`RUN_NW_SANDBOX_SERVER` and `RUN_FS_SANDBOX_SERVER`); it should not expose the legacy ambiguous `RUN_REMOTE_SERVER` variable directly.

### Mount targets
Replace `mounts[].nwSandboxOnly` with `mounts[].targets`.

Example:

```json
{
  "mounts": [
    {
      "mount": "/home/user/workspace",
      "hostPath": "../workspace-ro",
      "readOnly": true,
      "targets": ["agent"]
    },
    {
      "mount": "/home/user/workspace",
      "hostPath": "../workspace-rw",
      "targets": ["fs-sandbox"]
    },
    {
      "mount": "/opt/network-cache",
      "volume": "network-cache",
      "targets": ["nw-sandbox"]
    }
  ]
}
```

Valid target names are the app container names:

- `agent`
- `nw-sandbox`
- `fs-sandbox`

The config parser should reject unknown target names. It should also reject targets that are known but disabled for the project, because accepting a mount for a disabled component hides a likely config mistake.

When `targets` is omitted, the mount should apply to the default execution containers:

- `agent`
- `nw-sandbox`, if `nw_sandbox` is enabled

`fs-sandbox` should only receive custom mounts that explicitly target `fs-sandbox`.

`mounts[].nwSandboxOnly` should be rejected with a hint:

```text
replace 'mounts[N].nwSandboxOnly' with 'mounts[N].targets': ["nw-sandbox"]
```

The older `mounts[].sandboxOnly` legacy hint should also point to `targets: ["nw-sandbox"]`.

### Duplicate mount paths
The current parser rejects duplicate `mount` paths across all entries. That prevents expressing different host paths for the same container mount path in different targets, which is central to the fs-sandbox use case.

Change duplicate detection from "unique by mount path" to "unique by `(target, mount path)` after target expansion".

This should allow:

```json
[
  {
    "mount": "/home/user/workspace",
    "hostPath": "../workspace-ro",
    "readOnly": true,
    "targets": ["agent"]
  },
  {
    "mount": "/home/user/workspace",
    "hostPath": "../workspace-rw",
    "targets": ["fs-sandbox"]
  }
]
```

But it should reject two active entries that both target `agent` at `/home/user/workspace`.

The same rule should apply to `ignore`: an ignored mount removes the default mount only for the targeted component.

### Config layout
`config-template/nw_sandbox/` remains the network-sandbox policy directory.

Add:

```text
config-template/fs_sandbox/
  main.rego
```

Only materialize and require `config/fs_sandbox/` when `fs_sandbox` is enabled. `cladding check` should not fail if `config/fs_sandbox/` is absent and the component is disabled.

Likewise, `config/nw_sandbox/` should only be required when `nw_sandbox` is enabled. If `nw_sandbox` is disabled, old `config/nw_sandbox/` files may exist without being used, but `cladding check` should not require them.

Legacy path checks should still run for old names that are always wrong, such as:

- `sandbox_commands`
- `sandbox_domains.lst`
- `nw_sandbox_domains.lst`

### Runtime rendering
`pods.yaml` should stop assuming that every component exists. Split the current monolithic static pod template into per-component pod templates and render only enabled components.

Recommended template shape:

```text
pod-templates/
  empty-mask.yaml
  proxy.yaml
  agent.yaml
  nw-sandbox.yaml
  fs-sandbox.yaml
```

The exact file names can follow the repository's Rust module layout, but the important behavior is that disabled components are never rendered and do not leave stale placeholders in generated YAML. This is cleaner for this feature because optional components affect pod documents, host aliases, jail env vars, and agent env vars.

`cladding/src/pods.rs` should apply custom mounts by target component instead of testing `container_name != "nw-sandbox" && container_name != "agent"`.

### Network settings
Replace the singular sandbox fields with explicit optional component identities.

Current shape in `cladding/src/network.rs`:

- `sandbox_ip`
- `sandbox_name`
- `cli_ip`

Recommended shape:

- `agent_ip`
- `agent_name`
- `proxy_ip`
- `proxy_name`
- `nw_sandbox: Option<ComponentNetworkSettings>`
- `fs_sandbox: Option<ComponentNetworkSettings>`

`ComponentNetworkSettings` can contain:

- `ip`
- `name`

Use stable addresses inside the pool:

- proxy: `10.90.N.2`
- nw-sandbox: `10.90.N.3`, when enabled
- fs-sandbox: `10.90.N.4`, when enabled
- agent: `10.90.N.5`

Using fixed slots avoids address churn when a project toggles one sandbox on or off. It costs one additional address but keeps DNS, tests, and troubleshooting simpler.

## Non-goals
1. Do not implement unrestricted command execution in `fs-sandbox`; it still uses `mcp-run` and Rego policy.
2. Do not add proxy/domain allowlists for `fs-sandbox`; it has no proxy access.
3. Do not make the agent optional.
4. Do not accept old `agent_image`, `nw_sandbox_image`, `cli_image`, `sandbox_image`, `nwSandboxOnly`, or `sandboxOnly` keys as aliases.
5. Do not introduce a generic plugin system for arbitrary container types.
6. Do not automatically migrate user-edited `cladding.json`; report targeted errors and hints instead.

## Suggested implementation shape
1. Add component config structs in `cladding/src/config.rs`:
   - required `agent.image`
   - optional `nw_sandbox.enabled` / `nw_sandbox.image`
   - optional `fs_sandbox.enabled` / `fs_sandbox.image`
2. Replace `MountConfig.nw_sandbox_only` with a parsed target set.
3. Update validation so duplicate mount paths are checked per expanded target.
4. Refactor `cladding/src/network.rs` around explicit component network settings and optional sandbox components.
5. Split pod rendering into per-component templates in `cladding/src/pods.rs` so disabled sandbox pods are not rendered.
6. Add `fs-sandbox` pod rendering with `mcp-run`, `POLICY_DIR=/opt/config/fs_sandbox`, and inbound-only jail behavior.
7. Update `scripts/jail_agent.sh` to handle zero, one, or two sandbox endpoints.
8. Add `scripts/jail_fs_sandbox.sh` for the filesystem sandbox's outbound restrictions.
9. Stop installing `run-with-network` into `.cladding/tools/bin`; install `run-remote`, `run-in-nw-sandbox`, and `run-in-fs-sandbox` instead.
10. Update `check_required_images()` and `check_required_config_files()` in `cladding/src/cli.rs` to consider only enabled components.
11. Update `cladding run-with-scissors` to select a sandbox target and fail cleanly when that target is disabled.
12. Update README, examples, and feature docs for the component-object config, target-based mounts, and removal of `run-with-network`.

## Migration plan
Existing `cladding.json` files should be updated manually.

Old:

```json
{
  "name": "demo",
  "nw_sandbox_image": "cladding-sandbox:local",
  "agent_image": "cladding-agent:local",
  "mounts": [
    {
      "mount": "/opt/nw-only",
      "hostPath": "../nw-only",
      "nwSandboxOnly": true
    }
  ]
}
```

New:

```json
{
  "name": "demo",
  "agent": {
    "image": "cladding-agent:local"
  },
  "nw_sandbox": {
    "enabled": true,
    "image": "cladding-sandbox:local"
  },
  "mounts": [
    {
      "mount": "/opt/nw-only",
      "hostPath": "../nw-only",
      "targets": ["nw-sandbox"]
    }
  ]
}
```

To add a filesystem sandbox:

```json
{
  "fs_sandbox": {
    "enabled": true,
    "image": "cladding-sandbox:local"
  },
  "mounts": [
    {
      "mount": "/home/user/workspace",
      "hostPath": "../workspace-ro",
      "readOnly": true,
      "targets": ["agent"]
    },
    {
      "mount": "/home/user/workspace",
      "hostPath": "../workspace-rw",
      "targets": ["fs-sandbox"]
    }
  ]
}
```

`cladding check` should catch old schema keys before runtime rendering and print replacement hints.

## Verification
1. `cladding init` generates component-object config with `agent` and enabled `nw_sandbox`, but no `fs_sandbox`.
2. A project with no `fs_sandbox` renders only proxy, agent, and nw-sandbox pods.
3. A project with `nw_sandbox.enabled=false` renders no nw-sandbox pod, does not require `config/nw_sandbox`, does not set `RUN_NW_SANDBOX_SERVER` or `RUN_REMOTE_SERVER`, and does not make the agent jailer wait for a network sandbox.
4. A project with enabled `fs_sandbox` renders `<name>-fs-sandbox`, starts `mcp-run`, and points `POLICY_DIR` at `/opt/config/fs_sandbox`.
5. The agent receives `RUN_FS_SANDBOX_SERVER` only when `fs_sandbox` is enabled.
6. `cladding run-with-scissors --target fs-sandbox -- <cmd>` targets the fs-sandbox container when enabled.
7. `cladding run-with-scissors` without `--target` keeps targeting nw-sandbox when enabled.
8. The fs-sandbox pod has no proxy env vars, no proxy host alias, and no jail rule allowing outbound traffic to the proxy.
9. `RUN_REMOTE_SERVER` is not set for any runtime component.
10. `mounts[].targets` applies mounts only to the named enabled containers.
11. Omitted `mounts[].targets` applies to the agent and enabled `nw-sandbox`, but not to `fs-sandbox`.
12. Duplicate mount paths are allowed across different targets and rejected within the same target.
13. `mounts[].nwSandboxOnly` and `mounts[].sandboxOnly` fail validation with migration hints.
14. Old top-level image keys fail validation with migration hints.
15. `.cladding/tools/bin/run-with-network` is no longer installed or documented; `.cladding/tools/bin/run-in-nw-sandbox` and `.cladding/tools/bin/run-in-fs-sandbox` are installed as wrappers around `run-remote`.
16. `cargo fmt --check`, `cargo test --workspace`, and shell syntax checks for all jail scripts pass.

## Success criteria
1. `fs-sandbox` is absent unless `fs_sandbox.enabled` is true.
2. `nw-sandbox` can be disabled without stale pod, DNS, env var, jail, image, or config-file requirements.
3. `agent`, `nw_sandbox`, and `fs_sandbox` images are configured through component objects.
4. Mount targeting is explicit and can express different filesystem access for `agent` and `fs-sandbox`.
5. Existing old schema keys are rejected with clear replacement hints.
