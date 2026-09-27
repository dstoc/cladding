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
