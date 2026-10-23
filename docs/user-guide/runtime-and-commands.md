# Runtime and command reference

`cladding run` owns the only Cladding runtime lifecycle. It starts one UUID-scoped run, keeps it in the foreground while its command runs, and removes its containers, private runtime files, and Cladding-managed volumes when the command ends.

Other terminals attach to a run by its full UUID or an unambiguous substring. `cladding ps` lists runs on the selected Podman connection.

A run uses the selected project's configuration, tools, credentials, and home directly. It does not mount the host workspace by default. Configure a host-backed mount for a target that needs project files; see [Mounts and workspace access](mounts.md).

## Select project files

Without an option, configuration-consuming commands find `.cladding/` by searching the invocation directory and its parents.

- `--cladding-dir PATH` selects the `.cladding` directory. It selects Cladding's configuration and runtime files; it does not mount the host directory into a container.
- `--config FILE` selects a JSON config file. `--config -` reads it from standard input. When omitted, Cladding reads `cladding.json` from the selected or discovered `.cladding` directory.

When both options are set, `--config` selects the configuration and `--cladding-dir` selects the runtime directory. Relative build paths and `hostPath` values resolve from the config file's directory. For a config read from standard input, they resolve from the parent of the explicitly selected `.cladding` directory, or from the invocation directory when `--cladding-dir` is omitted.

`--cladding-dir` and `--config` apply to project preparation and launch commands. Attached commands do not load project configuration. They accept `--run SELECTOR`; without it, they select the sole run for the current project. An explicit `--run` works outside the project and cannot be combined with `--config` or `--cladding-dir`.

```bash
cladding --cladding-dir /tmp/project/.cladding check
cladding --config ./custom.json run -- codex exec "Implement the task"
cladding exec --run a91e --target agent pwd
cladding logs --run a91e agent -f
cladding ps
```

`init` accepts `--cladding-dir` to select where it creates `.cladding/`; it does not read `--config`. `ps` works outside a project and does not accept project-specific options. When `run` reads `--config -`, standard input is reserved for JSON and is not forwarded to the command.

## Run a command

`cladding run` uses the selected project configuration and project CA. Run `cladding build` first to build images and initialize a missing CA. `check` and `run` validate the CA; they do not create or rotate it.

```bash
cladding run -- codex exec "Implement the task"
cladding run --env FOO=bar -- env
cladding run -v -- codex exec "Implement the task"
```

Each invocation creates a UUID-scoped runtime under `.cladding/runtime/runs/cladding-run-<UUID>`. It acquires a private lifetime lock and host control socket before it creates Podman resources. The control socket is not mounted into containers. Cladding prints the full UUID to stderr when the run is ready; workload stdout stays unchanged. Podman labels identify run resources, while the live supervisor reports readiness, expected components, and shutdown state. Project config, tools, credentials, and home remain in their project directories. Without `--config -`, Cladding forwards standard input to the command. Use repeatable `--env KEY[=VALUE]` options before `--` to set command environment variables. `-v` or `--verbose` shows startup and cleanup details.

Use a run-only Baffle secret override when one command needs a value that should not be added to the persistent project secret files:

```bash
GITHUB_TOKEN="$(mint-short-lived-token)" cladding run \
  --secret github-token=env:GITHUB_TOKEN -- codex
cladding run --secret github-token=file:/run/user/1000/token -- codex
```

The symbolic name must match a name in the Baffle session rule and daemon allowlist. Cladding writes the value to a private mode-`0600` file under the run directory and mounts it read-only into the proxy. It does not change the project secret file or pass the value to the command. Literal secret values are not accepted as `--secret` arguments.

## Enter a running container

`cladding exec` defaults to the agent. Use `--target nw-sandbox` or `--target fs-sandbox` to enter an enabled sandbox directly:

```bash
cladding exec --run a91e --target agent pwd
cladding exec --run a91e --target nw-sandbox ls /opt/config/nw_sandbox
cladding exec --run a91e --target fs-sandbox ls /opt/config/fs_sandbox
```

Host-initiated `exec --target nw-sandbox` or `exec --target fs-sandbox` runs directly in the sandbox and bypasses the agent's delegation and policy checks. For policy-checked delegation, run `run-in-nw-sandbox -- <cmd> [args...]` or `run-in-fs-sandbox -- <cmd> [args...]` inside the agent. `fs-sandbox` has no workspace mount or proxy access by default. Add an explicit target mount when it needs host files.

## Sandbox command policies

Cladding installs the pinned Gantry v0.4.8 `gantry` server and `run-remote` client from one verified Linux release archive. Both sandboxes start Gantry with the Direct backend. Gantry executes allowed commands inside the existing Podman sandbox, so Podman's network and filesystem boundaries remain in effect.

The default policy templates include `decision.rego`. It returns Gantry's structured decision and sets `filesystem` and `baffle_session` to `null`. It reads each command's `allow` and `allow_env` rules directly from the command package. Keep `main.rego` as the `sandbox.main` package file, but do not add the old dynamic `allow` router there. Gantry can report recursion when that router wraps the structured decision.

Cladding does not rewrite existing policy files. For each sandbox policy directory in an existing project, replace the old `main.rego` router with `package sandbox.main` and add this `decision.rego` adapter. Keep command-specific `allow` and `allow_env` rules unchanged:

```rego
package sandbox.main

default decision := {
  "allow": false,
  "filesystem": null,
  "baffle_session": null,
}

decision := {
  "allow": true,
  "filesystem": null,
  "baffle_session": null,
} if {
  data.sandbox[input.command].allow
  count(object.keys(input.env)) == 0
}

decision := {
  "allow": true,
  "filesystem": null,
  "baffle_session": null,
} if {
  data.sandbox[input.command].allow
  count(object.keys(input.env)) > 0
  data.sandbox[input.command].allow_env
}
```

Without a valid `data.sandbox.main.decision`, Gantry denies command requests. Read the [Gantry v0.4.8 policy reference](https://github.com/dstoc/gantry/blob/v0.4.8/docs/reference.md) before changing the policy contract.

## Temporary host connections

`cladding expose` publishes a TCP port from the selected run's agent container on a host address. It runs in the foreground, listens on `127.0.0.1` by default, and stops with Ctrl-C or when the run shuts down:

```bash
cladding expose --run a91e 3000
cladding expose --run a91e 3000 9000
cladding expose --run a91e 3000 --bind-address 192.168.1.20
```

Use `--bind-address 0.0.0.0` or `--bind-address ::` to listen on all host IPv4 or IPv6 interfaces. Select an address deliberately because other machines may be able to connect.

`cladding inject` makes one host-reachable TCP endpoint available on the selected run's agent localhost. It runs in the foreground and stops with Ctrl-C or when the run shuts down:

```bash
cladding inject --run a91e 11434
cladding inject --run a91e 5432 15432
cladding inject --run a91e db.internal:5432 15432
```

A bare port uses host `127.0.0.1` on that port. A second port changes only the agent-side listener. An explicit `host:port` selects one host endpoint resolved by the host process. The agent cannot select a different destination through the bridge. `inject` is a deliberate exception to the normal network boundary, not general host networking.

## Stop and recover a run

`cladding stop --run SELECTOR` asks the owning foreground supervisor to stop the workload and clean up its resources. It returns success only after the supervisor confirms cleanup. With no selector, `stop` requires a current project and exactly one matching run.

`cladding destroy --run SELECTOR` recovers an orphan only after it acquires the run's exclusive lifetime lock and verifies the selected Podman resources. It removes only run-owned containers, socket volumes, and managed mount volumes. It preserves user-named volumes, shared project data, and unrelated paths. Use `cladding logs --run SELECTOR <component>` to read retained logs from a failed run.

Before upgrading from a version with persistent runtimes, run that version's `cladding down` from each project. The new CLI does not adopt legacy persistent runtimes or older ephemeral runs without compatible metadata. Inspect legacy containers and volumes with Podman and verify their `cladding`, `project_root`, and `app` labels before removing them. If Podman resources are already gone and only private files remain, verify the run lifetime lock is not held, then remove only the known `.cladding/runtime/runs/cladding-run-<UUID>` directory. Cladding does not keep a global registry for private-only remnants.

Cladding does not discover or remove Podman pods left by older releases. Inspect each old pod's labels and remove only pods that identify the intended project and component:

```bash
podman pod ps -a --filter label=cladding --format '{{.Name}}'
podman pod inspect POD_NAME --format '{{.Name}} {{json .Labels}}'
podman pod rm -f POD_NAME
```

Confirm the `cladding`, `project_root`, and `app` labels and check that the workload can stop before removing an old pod by its exact name. Run resources use standalone containers and are selected by immutable container IDs.

For Baffle policy, CA, secret, and reload behavior, see [Proxy policy, CA, and credentials](../features/proxy/summary.md).
