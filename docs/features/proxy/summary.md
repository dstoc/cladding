# Proxy Configuration

Cladding stores editable proxy configuration in `.cladding/config/proxy/`. `cladding init` creates these native Baffle TOML files:

- `daemon.toml` configures the daemon, its private control socket, the scoped data-socket root, CA paths, and the secret directory.
- `sessions/agent.toml` configures the agent session.
- `sessions/nw-sandbox.toml` configures the network-sandbox session.

Both session templates are persistent and use stable data-socket names. Each template includes an explicit `example.com:443` HTTPS tunnel rule. Baffle rejects active sessions without host rules. Add exact hostnames and ports to the relevant session file to change its policy.

The network-sandbox session file may exist when the network sandbox is disabled. The file's presence does not create an active session.

## File ownership and access

The proxy configuration uses Baffle's `file_only` create mode. Initialization creates directories with mode `0755` and TOML files with mode `0644`, owned by the user who runs `cladding init`. Rootless Podman maps the host user's files to the proxy container's trusted UID. Initialization rejects symlinks on the managed configuration paths because Baffle's file-only mode requires real files and directories.

Cladding mounts `config/` read-only into the agent and network sandbox. They can read the non-secret Baffle TOML by design. Store symbolic secret names in `[secrets].allowed`; do not put secret values, private keys, or control-socket paths with access capabilities in TOML.

## Runtime status

The proxy pod runs one Baffle daemon in a minimal Debian trixie-slim image. Its startup script verifies the mounted config and credentials, starts the daemon, waits until `baffle list` accepts control commands, then creates the persistent agent session and the network-sandbox session when enabled. On shutdown, the script forwards the signal to Baffle; the daemon stops its persistent sessions with the proxy container.

Cladding does not wait for Baffle session readiness before it starts execution containers. Early proxy requests can fail while the proxy container starts. Startup errors appear in proxy logs and the container exit status. `cladding reload-proxy` invokes `baffle reload --all` and requires a Baffle-enabled proxy runtime. CI installs the pinned Baffle binary and validates both session files in rootless Podman.

## Reloading policy

`cladding reload-proxy` asks Baffle to reload every active file-backed session from the existing native TOML files. Cladding does not regenerate those files. Baffle prints a `reloaded`, `unchanged`, or `failed` result for each session. It continues after a session failure, and the command exits unsuccessfully if any session fails.

A successful reload applies changed policy, resolved injection credentials, and data-socket paths to newly accepted connections. Connections that Baffle already accepted keep their previous policy and credentials until they close. A credential reload does not revoke credentials from open connections. Invalid file updates leave the session's previous effective configuration in place.

Baffle can reject a non-disruptive reload when it cannot retain an old policy generation or listener. Inspect `cladding logs proxy` when a session reports `failed`. Reload applies file-backed session changes only. Changes to daemon settings, the CA, or the Baffle binary require a proxy container restart.
