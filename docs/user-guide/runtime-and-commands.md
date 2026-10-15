# Runtime and command reference

Cladding supports a persistent project runtime and a one-off runtime:

- `cladding up` starts the standalone containers for a project. `cladding exec` runs a command in one of those containers.
- `cladding run` creates a temporary runtime for one command and removes its containers, private runtime files, and Cladding-managed volumes when the command ends.

Both modes use the selected project's configuration, tools, credentials, and home directly. Neither mode mounts the host workspace by default. Configure a host-backed mount for a target that needs project files; see [Mounts and workspace access](mounts.md).

## Select project files

Without an option, configuration-consuming commands find `.cladding/` by searching the invocation directory and its parents.

- `--cladding-dir PATH` selects the `.cladding` directory. It selects Cladding's configuration and runtime files; it does not mount the host directory into a container.
- `--config FILE` selects a JSON config file. `--config -` reads it from standard input. When omitted, Cladding reads `cladding.json` from the selected or discovered `.cladding` directory.

When both options are set, `--config` selects the configuration and `--cladding-dir` selects the runtime directory. Relative build paths and `hostPath` values resolve from the config file's directory. For a config read from standard input, they resolve from the parent of the explicitly selected `.cladding` directory, or from the invocation directory when `--cladding-dir` is omitted.

These options apply to `build`, `check`, `up`, `down`, `destroy`, `run`, `exec`, `logs`, `reload-proxy`, `expose`, and `inject`. Put shared options before the command:

```bash
cladding --cladding-dir /tmp/project/.cladding up
cladding --config ./custom.json exec --target agent pwd
cladding --config ./custom.json run -- codex exec "Implement the task"
cat custom.json | cladding --cladding-dir /tmp/project/.cladding --config - check
```

`init` accepts `--cladding-dir` to select where it creates `.cladding/`; it does not read `--config`. `ps` lists all running projects and does not accept project-specific options. When `run` reads `--config -`, standard input is reserved for JSON and is not forwarded to the command.

## Run a command

`cladding run` uses the selected project configuration and persistent project CA. Run `cladding build` first to build images and initialize a missing CA. `check` and `run` validate the CA; they do not create or rotate it.

```bash
cladding run -- codex exec "Implement the task"
cladding run --env FOO=bar -- env
cladding run -v -- codex exec "Implement the task"
```

Each invocation creates a UUID-scoped runtime under `.cladding/runtime/runs/<instance>` for private files such as generated scripts and masks. Podman containers and Cladding-managed volumes use the run's private identity and are removed when the command ends. Project config, tools, credentials, and home remain in their project directories. Without `--config -`, Cladding forwards standard input to the command. Use repeatable `--env KEY[=VALUE]` options before `--` to set command environment variables. Successful startup and cleanup output is quiet by default; `-v` or `--verbose` shows Podman and certificate setup output, instance details, and cleanup activity.

Use a run-only Baffle secret override when one command needs a value that should not be added to the persistent project secret files:

```bash
GITHUB_TOKEN="$(mint-short-lived-token)" cladding run \
  --secret github-token=env:GITHUB_TOKEN -- codex
cladding run --secret github-token=file:/run/user/1000/token -- codex
```

The symbolic name must match a name in the Baffle session rule and daemon allowlist. Cladding writes the value to a private mode-`0600` file under the run directory and mounts it read-only into the proxy. It does not change the project secret file or pass the value to the command. Literal secret values are not accepted as `--secret` arguments. Persistent runtimes and `exec` continue to use project credentials.

## Enter a running container

`cladding exec` defaults to the agent. Use `--target nw-sandbox` or `--target fs-sandbox` to enter an enabled sandbox directly:

```bash
cladding exec --target agent pwd
cladding exec --target nw-sandbox ls /opt/config/nw_sandbox
cladding exec --target fs-sandbox ls /opt/config/fs_sandbox
```

Host-initiated `exec --target nw-sandbox` or `exec --target fs-sandbox` runs directly in the sandbox and bypasses the agent's delegation and policy checks. For policy-checked delegation, run `run-in-nw-sandbox -- <cmd> [args...]` or `run-in-fs-sandbox -- <cmd> [args...]` inside the agent. `fs-sandbox` has no workspace mount or proxy access by default. Add an explicit target mount when it needs host files.

## Temporary host connections

`cladding expose` publishes a TCP port from the agent container on a host address. It runs in the foreground, listens on `127.0.0.1` by default, and stops with Ctrl-C:

```bash
cladding expose 3000
cladding expose 3000 9000
cladding expose 3000 --bind-address 192.168.1.20
```

Use `--bind-address 0.0.0.0` or `--bind-address ::` to listen on all host IPv4 or IPv6 interfaces. Select an address deliberately because other machines may be able to connect.

`cladding inject` makes one host-reachable TCP endpoint available on agent localhost. It runs in the foreground and stops with Ctrl-C:

```bash
cladding inject 11434
cladding inject 5432 15432
cladding inject db.internal:5432 15432
```

A bare port uses host `127.0.0.1` on that port. A second port changes only the agent-side listener. An explicit `host:port` selects one host endpoint resolved by the host process. The agent cannot select a different destination through the bridge. `inject` is a deliberate exception to the normal network boundary, not general host networking.

## Remove resources

`cladding down` stops and removes the current standalone containers and Cladding-managed volumes. `cladding destroy` force-removes the current runtime. Both keep the persistent project CA and secrets. Explicitly named Podman volumes are not managed or removed by Cladding.

Cladding does not discover or remove Podman pods left by older releases. Inspect each old pod's labels and remove only pods that identify the intended project and component:

```bash
podman pod ps -a --filter label=cladding --format '{{.Name}}'
podman pod inspect POD_NAME --format '{{.Name}} {{json .Labels}}'
podman pod rm -f POD_NAME
```

Confirm the `cladding`, `project_root`, and `app` labels and check that the workload can stop before removing the pod by its exact name. Current `down` and `destroy` commands manage standalone containers and do not remove old pods.

For Baffle policy, CA, secret, and reload behavior, see [Proxy policy, CA, and credentials](../features/proxy/summary.md).
