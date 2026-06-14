# PRD: Runtime Resource Renaming

## Objective
Rename the cladding runtime resources from the current `*-pod` / `cli-app` terminology to shorter project-scoped names:

- proxy: `<name>-proxy`
- sandbox command runner: `<name>-nw-sandbox`
- agent container/pod: `<name>-agent`

The rename applies to the generated pod names, internal DNS names, environment variables, jail scripts, proxy startup discovery, CLI target derivation, tests, and README documentation. The result should make Podman output and in-container network naming match the product vocabulary used by cladding.

The rename also applies to the user-visible config directory that contains network-sandbox command policies:

- old: `.cladding/config/sandbox_commands`
- new: `.cladding/config/nw_sandbox`

## Use Cases
1. A developer runs `cladding up` for project `demo` and sees runtime resources named around `demo-proxy`, `demo-nw-sandbox`, and `demo-agent` instead of `demo-proxy-pod`, `demo-sandbox-pod`, and `demo-cli-pod`.
2. Commands inside the agent resolve the sandbox service through the new DNS name instead of `sandbox-pod`.
3. The agent sends HTTP(S) traffic through `demo-proxy` / the configured proxy alias instead of `proxy-pod`.
4. Documentation and troubleshooting commands use the same names that appear in Podman and in runtime DNS.

## Current Behavior
Runtime names are currently derived in `cladding/src/network.rs`:

- `proxy_pod_name = <name>-proxy-pod`
- `sandbox_pod_name = <name>-sandbox-pod`
- `cli_pod_name = <name>-cli-pod`

`pods.yaml` uses those values as Kubernetes pod metadata names, while the in-pod application containers are named `proxy`, `sandbox-app`, and `cli-app`. Podman then derives raw container names such as:

- `<name>-proxy-pod-proxy`
- `<name>-sandbox-pod-sandbox-app`
- `<name>-cli-pod-cli-app`

Internal DNS aliases and scripts still use unprefixed service names such as `proxy-pod`, `sandbox-pod`, and `cli-pod`. Those names appear in:

- `pods.yaml` host aliases, proxy environment variables, `no_proxy`, and `RUN_REMOTE_SERVER`
- `pods.yaml` `POLICY_DIR=/opt/config/sandbox_commands`
- `scripts/jail_cli.sh`
- `scripts/jail_sandbox.sh`
- `scripts/proxy_startup.sh`
- `config-template/squid.conf`
- `cladding/src/cli.rs` container target derivation
- `cladding/src/pods.rs` mount injection container-name checks
- `cladding/src/podman.rs` expose target metadata
- `cladding/tests/cli_tests.rs`
- `README.md` and feature docs

## Problem Statement
The current names leak implementation details and require users to understand both pod-level names and generated Podman container names. The `cli` term is also product-facing in places where the running container is really the agent environment.

The rename needs to be complete enough that users do not see a mixed model where some code paths still talk about `cli-pod` or `sandbox-pod`. Partial renaming would be more confusing than the current state because DNS, proxy policy, jail rules, and troubleshooting commands all depend on consistent names.

## Proposal
Introduce a single canonical runtime naming model and update every runtime reference to use it.

The canonical project-scoped service names are:

- `proxy_name`: `<name>-proxy`
- `sandbox_name`: `<name>-nw-sandbox`
- `agent_name`: `<name>-agent`

These names replace the current `proxy_pod_name`, `sandbox_pod_name`, and `cli_pod_name` fields in `NetworkSettings`.

### Pod and DNS Names
`pods.yaml` should render pod metadata names as the canonical service names:

- proxy pod metadata name: `<name>-proxy`
- sandbox pod metadata name: `<name>-nw-sandbox`
- agent pod metadata name: `<name>-agent`

Internal DNS references should use the same rendered names. The template should avoid hard-coded `proxy-pod`, `sandbox-pod`, and `cli-pod` strings and instead use placeholders such as:

- `REPLACE_PROXY_NAME`
- `REPLACE_SANDBOX_NAME`
- `REPLACE_AGENT_NAME`

This applies to:

- `hostAliases`
- `http_proxy`, `https_proxy`, `HTTP_PROXY`, and `HTTPS_PROXY`
- `no_proxy` and `NO_PROXY`
- `RUN_REMOTE_SERVER`

For a project named `demo`, the agent should call the raw endpoint at `http://demo-nw-sandbox:3000/raw`.

### Config Names
The network sandbox config directory should be renamed:

- source template directory: `config-template/sandbox_commands` becomes `config-template/nw_sandbox`
- runtime project directory: `.cladding/config/sandbox_commands` becomes `.cladding/config/nw_sandbox`
- container path: `/opt/config/sandbox_commands` becomes `/opt/config/nw_sandbox`
- `POLICY_DIR` value becomes `/opt/config/nw_sandbox`
- examples directory: `examples/sandbox_commands` becomes `examples/nw_sandbox`
- bundled skill/example references to `/opt/config/sandbox_commands` become `/opt/config/nw_sandbox`

The implementation should not keep both directories active as a long-term compatibility path. For migration, `cladding init --update-scripts` or the relevant config materialization path may move or recreate the new template directory, but the runtime should read only `nw_sandbox`.

Policy package names should stay as `sandbox.*` even though the directory is renamed:

- keep `package sandbox.main`
- keep `package sandbox.curl`
- keep `data.sandbox[...]` lookups

This intentionally leaves the policy namespace generic so future sandbox types can share or compose policy modules without tying the Rego package name to the network-sandbox runtime name.

Adjacent config files should also be renamed:

- `sandbox_domains.lst` becomes `nw_sandbox_domains.lst`
- `cli_domains.lst` becomes `agent_domains.lst`
- `cli_host_ports.lst` becomes `agent_host_ports.lst`

These three files are part of the same user-facing naming model as `sandbox_commands`; they should not retain mixed `cli` / `sandbox` vocabulary in `.cladding/config`.

`cladding check` should explicitly detect legacy config entry names after the rename. If any of the following exist under `.cladding/config`, `cladding check` should fail with a clear upgrade error rather than treating them as harmless extra files:

- `sandbox_commands`
- `sandbox_domains.lst`
- `cli_domains.lst`
- `cli_host_ports.lst`

The error should name the replacement path for each legacy entry and should run in addition to the existing required-config-file check in `cladding/src/cli.rs`.

`cladding.json` also has user-visible schema keys that still use the old vocabulary:

- `sandbox_image`
- `cli_image`
- `mounts[].sandboxOnly`

If this feature is intended to remove all user-facing old vocabulary, these should become:

- `nw_sandbox_image`
- `agent_image`
- `mounts[].nwSandboxOnly`

The implementation should not accept the old keys as aliases. `cladding.json` parsing should reject unknown keys with a clear error so old `sandbox_image`, `cli_image`, and `sandboxOnly` entries are caught during upgrade instead of being silently ignored or accepted.

### Container Names
The app container terminology should be renamed in `pods.yaml`:

- `sandbox-app` becomes `nw-sandbox`
- `cli-app` becomes `agent`
- `cli-node` becomes `agent-node`

The proxy container may remain `proxy` because it already matches the target vocabulary.

Mount injection in `cladding/src/pods.rs` should apply custom mounts to `nw-sandbox` and `agent`, replacing the current checks for `sandbox-app` and `cli-app`.

With `podman play kube`, Podman may continue deriving raw container names from the pod name plus the container name. The implementation should treat `NetworkSettings` as the source of truth and should not assume old generated names. If exact raw Podman container names of `<name>-proxy`, `<name>-nw-sandbox`, and `<name>-agent` are required in `podman ps`, that is a separate runtime-construction change because `play kube` currently generates pod/container composite names.

### CLI Behavior
`cladding run` should execute in the agent container for the active project. Internally, it should derive the new Podman target container name from the rendered runtime names and the app container name:

- old: `<cli_pod_name>-cli-app`
- new: derived from `<agent_name>` and `agent`

`cladding run-with-scissors` should execute in the network sandbox container:

- old: `<sandbox_pod_name>-sandbox-app`
- new: derived from `<sandbox_name>` and `nw-sandbox`

`cladding reload-proxy`, `cladding down`, and `cladding destroy` should use the new canonical names.

### Proxy and Jail Scripts
The scripts materialized under `.cladding/scripts` should resolve the new names:

- `scripts/jail_cli.sh` should become agent-oriented and resolve `<name>-nw-sandbox` and `<name>-proxy`.
- `scripts/jail_sandbox.sh` should resolve `<name>-proxy`.
- `scripts/proxy_startup.sh` should discover the agent and sandbox peers through the new DNS names and any generated Podman container aliases that remain relevant.

Because the scripts are static assets today, they need rendered placeholders for the project-scoped names or environment variables passed from `pods.yaml`. Prefer passing explicit environment variables from `pods.yaml`:

- `CLADDING_PROXY_NAME`
- `CLADDING_SANDBOX_NAME`
- `CLADDING_AGENT_NAME`

The scripts should use those variables and fail with a clear message if any required name is missing.

Script filenames should also follow the new vocabulary:

- `scripts/jail_cli.sh` becomes `scripts/jail_agent.sh`
- `scripts/jail_sandbox.sh` becomes `scripts/jail_nw_sandbox.sh`

### Squid Config
`config-template/squid.conf` should replace `visible_hostname proxy-pod` and `acl cli_sandbox_host dstdomain sandbox-pod` with values based on the new runtime names.

Because Squid config is host-mounted and not currently rendered per run, the startup script should perform placeholder replacement when generating `/tmp/squid_generated.conf`. This keeps project-specific DNS names out of static config files while preserving the current config-template workflow.

### Expose Feature
The expose feature remains scoped to the agent container. User-facing text and metadata should move from `cli-app` to `agent`.

Labels should be updated from:

- `cladding_expose_target=cli-app`

to:

- `cladding_expose_target=agent`

Existing expose proxies with the old label are not migrated. They should continue to be cleaned up by `cladding down` / `cladding destroy` only if the implementation includes a legacy cleanup pass.

### Documentation
README and feature documentation should replace product-facing `cli` language when it refers to the agent runtime:

- `cli-pod` becomes `<name>-agent`
- `sandbox-pod` becomes `<name>-nw-sandbox`
- `proxy-pod` becomes `<name>-proxy`
- `cli-app` becomes `agent`
- `sandbox-app` becomes `nw-sandbox`
- `sandbox_commands` becomes `nw_sandbox`
- `sandbox_domains.lst` becomes `nw_sandbox_domains.lst`
- `cli_domains.lst` becomes `agent_domains.lst`
- `cli_host_ports.lst` becomes `agent_host_ports.lst`

Historical docs under `docs/features/initial-implementation` may remain unchanged if treated as archived design notes, but active README and current feature docs should be updated.

## Migration Plan
This is a breaking runtime-name change for currently running projects.

1. Before upgrading, users should run `cladding down` or `cladding destroy` with the old version.
2. After upgrading, `cladding up` creates resources with the new names.
3. If old resources are still running, `cladding ps` may still discover them by labels, but commands that derive container names from the new model should not be expected to operate on old running pods.
4. `cladding destroy` should perform best-effort removal of both new names and legacy pod names for the current project to make recovery straightforward.

`cladding.json` schema migration is required for generated project config. New configs should use `nw_sandbox_image`, `agent_image`, and `nwSandboxOnly`; old keys should fail validation as unknown keys with upgrade guidance.

## Suggested Implementation Shape
1. Rename fields in `cladding/src/network.rs` from pod-specific names to canonical runtime names.
2. Update `pods.yaml` placeholders and all internal DNS references to use rendered project-scoped names.
3. Rename config template paths and container paths for `nw_sandbox`.
4. Rename domain and host-port config files.
5. Rename `cladding.json` schema keys to `nw_sandbox_image`, `agent_image`, and `nwSandboxOnly`, and reject unknown keys.
6. Keep Rego package names and `data.sandbox` lookups unchanged.
7. Add `cladding check` detection for legacy config entries.
8. Add runtime-name environment variables to the relevant containers in `pods.yaml`.
9. Update and rename `scripts/jail_cli.sh`, `scripts/jail_sandbox.sh`, and `scripts/proxy_startup.sh` to read those variables.
10. Update `config-template/squid.conf` placeholders and the proxy startup generation step.
11. Update `cladding/src/cli.rs` command targets, destroy/down cleanup names, reload-proxy target, expose target labels, and user-facing messages.
12. Update `cladding/src/pods.rs` mount injection target names.
13. Update `cladding/src/podman.rs` expose parsing tests and any target-name filters.
14. Update `cladding/tests/cli_tests.rs` and add assertions that rendered YAML contains the new names and no old DNS names.
15. Update README, active feature docs, and bundled skill/example docs.

## Fallback Behavior
The new runtime should not silently fall back to old DNS names during normal operation. If a script cannot resolve the configured project-scoped name, it should keep the existing wait-and-retry behavior while logging the name it is trying to resolve.

For cleanup only, commands may include best-effort legacy name removal for:

- `<name>-proxy-pod`
- `<name>-sandbox-pod`
- `<name>-cli-pod`

This fallback is only for teardown and should not be used for `run`, `run-with-scissors`, proxy reload, or new pod startup.

## Non-Goals
1. Changing the shared Podman network pool names (`cladding-N`) or subnet allocation.
2. Changing the sandbox policy engine or command policy semantics.
3. Adding configurable runtime names to `cladding.json`.
4. Supporting mixed old/new DNS names as a long-term compatibility layer.
5. Migrating already-running pods in place.
6. Redesigning `podman play kube` usage solely to force exact raw Podman container names.
7. Renaming the `mcp-run` binary or crate.

## Verification
1. Unit tests for `resolve_network_settings("demo", 1)` assert:
   - proxy name is `demo-proxy`
   - sandbox name is `demo-nw-sandbox`
   - agent name is `demo-agent`
2. Rendered `pods.yaml` tests assert:
   - old strings `proxy-pod`, `sandbox-pod`, `cli-pod`, `sandbox-app`, and `cli-app` are absent
   - new strings `demo-proxy`, `demo-nw-sandbox`, `demo-agent`, `nw-sandbox`, and `agent` are present
   - `RUN_REMOTE_SERVER` points to `http://demo-nw-sandbox:3000/raw`
3. Mount tests assert custom mounts still apply to `nw-sandbox` and `agent`.
4. Script tests or shellcheck-style checks assert jail scripts reference `CLADDING_PROXY_NAME`, `CLADDING_SANDBOX_NAME`, and `CLADDING_AGENT_NAME` rather than hard-coded old DNS names.
5. Expose metadata tests assert `cladding_expose_target=agent`.
6. Config materialization tests assert:
   - `config-template/nw_sandbox` exists
   - rendered pods use `POLICY_DIR=/opt/config/nw_sandbox`
   - README no longer references `.cladding/config/sandbox_commands`
7. `cladding check` tests assert legacy config entries fail with replacement guidance for `sandbox_commands`, `sandbox_domains.lst`, `cli_domains.lst`, and `cli_host_ports.lst`.
8. Policy tests assert the generated Rego still uses `package sandbox.*` and `data.sandbox[...]`.
9. Config parsing tests assert new schema keys are accepted and old keys fail as unknown keys with clear errors.
10. Integration verification with Podman:
   - `cladding up`
   - `cladding run getent hosts <name>-proxy`
   - `cladding run getent hosts <name>-nw-sandbox`
   - `cladding run curl http://<name>-nw-sandbox:3000/raw` or equivalent health check
   - `cladding reload-proxy`
   - `cladding expose 3000`
   - `cladding down`
11. README examples are updated to the new names.

## Success Criteria
1. New projects start pods named `<name>-proxy`, `<name>-nw-sandbox`, and `<name>-agent`.
2. Runtime DNS, proxy environment variables, jail scripts, and `RUN_REMOTE_SERVER` use the new names.
3. `cladding run`, `cladding run-with-scissors`, `cladding reload-proxy`, `cladding expose`, `cladding down`, and `cladding destroy` work with the new names.
4. Rendered runtime YAML contains no `proxy-pod`, `sandbox-pod`, `cli-pod`, `sandbox-app`, or `cli-app` references.
5. Runtime config uses `nw_sandbox`, not `sandbox_commands`.
6. `cladding check` fails when old renamed config entries are still present.
7. Tests cover the new naming model and fail if old runtime names are reintroduced.
