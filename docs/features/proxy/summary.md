# Proxy Configuration

Cladding stores editable proxy configuration in `.cladding/config/proxy/`. `cladding init` creates these native Baffle TOML files:

- `daemon.toml` configures the daemon, its private control socket, the scoped data-socket root, CA paths, and the secret directory.
- `sessions/agent.toml` configures the agent session.
- `sessions/nw-sandbox.toml` configures the network-sandbox session.

Both session templates are persistent and use stable data-socket names. Each template includes an explicit `example.com:443` HTTPS tunnel rule. Baffle rejects active sessions without host rules. Add exact hostnames and ports to the relevant session file to change its policy.

The network-sandbox session file may exist when the network sandbox is disabled. The file's presence does not create an active session.

Use this setup sequence:

```bash
cladding init
# Edit .cladding/config/proxy/daemon.toml and the session files as needed.
cladding build
cladding up
```

`cladding up` starts the Baffle proxy pod and enabled execution containers. The
proxy starts its daemon and creates the persistent agent session. It creates
the network-sandbox session only when that component is enabled. Cladding does
not wait for session readiness before starting execution containers, so an
early request to `127.0.0.1:3128` can fail during startup. Retry the request
after the proxy starts and inspect `cladding logs proxy` if it continues to
fail.

## File ownership and access

The proxy configuration uses Baffle's `file_only` create mode. Initialization creates directories with mode `0755` and TOML files with mode `0644`, owned by the user who runs `cladding init`. Rootless Podman maps the host user's files to the proxy container's trusted UID. Initialization rejects symlinks on the managed configuration paths because Baffle's file-only mode requires real files and directories.

Cladding mounts `config/` read-only into the agent and network sandbox. They can read the non-secret Baffle TOML by design. Store symbolic secret names in `[secrets].allowed`; do not put secret values, private keys, or control-socket paths with access capabilities in TOML. In phase one, execution containers do not mount the Baffle control socket.

The control socket stays inside the proxy container. The agent and network
sandbox receive only their own mode-`0600` data socket inside a mode-`0700`
component directory. On Linux, Baffle binds those sockets in the scoped
runtime socket directories. With macOS Podman machine, a trusted `socat`
process relays each enabled socket through a separate Podman-managed volume.
The filesystem sandbox receives no proxy socket or proxy environment by
default.

## Runtime status

The proxy pod runs one Baffle daemon in a minimal Debian trixie-slim image. Its startup script verifies the mounted config and credentials, starts the daemon, waits until `baffle list` accepts control commands, then creates the persistent agent session and the network-sandbox session when enabled. On shutdown, the script forwards the signal to Baffle; the daemon stops its persistent sessions with the proxy container.

Cladding does not wait for Baffle session readiness before it starts execution containers. Early proxy requests can fail while the proxy container starts. Startup errors appear in proxy logs and the container exit status. `cladding reload-proxy` invokes `baffle reload --all` and requires a Baffle-enabled proxy runtime. CI installs the pinned Baffle binary and validates both session files in rootless Podman.

## Reloading policy

`cladding reload-proxy` asks Baffle to reload every active file-backed session from the existing native TOML files. Cladding does not regenerate those files. Baffle prints a `reloaded`, `unchanged`, or `failed` result for each session. It continues after a session failure, and the command exits unsuccessfully if any session fails.

A successful reload applies changed policy, resolved injection credentials, and data-socket paths to newly accepted connections. Connections that Baffle already accepted keep their previous policy and credentials until they close. A credential reload does not revoke credentials from open connections. Invalid file updates leave the session's previous effective configuration in place.

Baffle can reject a non-disruptive reload when it cannot retain an old policy generation or listener. Inspect `cladding logs proxy` when a session reports `failed`. Reload applies file-backed session changes only. Changes to daemon settings, the CA, or the Baffle binary require a proxy container restart.

## Policy and credential example

The default templates allow HTTPS tunneling to the exact hostname
`example.com` on port `443`. Add only the hostnames and ports that a component
needs. A tunnel rule permits HTTPS through `CONNECT`; it does not inspect
paths or inject headers. For path restrictions or header injection, use an
interception rule and ensure clients trust this project's CA.

For example, a session can restrict interception to paths on one exact host
and use a symbolic credential name:

```toml
[[rules]]
host = "registry.example"
mode = "intercept"
ports = [443]
paths = ["/v2/**"]

[[rules.inject]]
header = "Authorization"
secret = "registry-token"
format = "bearer"
```

Add the same symbolic name to `[secrets].allowed` in `daemon.toml` and
provision a regular file named `registry-token` under
`.cladding/credentials/baffle/secrets/`. Keep the secrets directory at mode
`0700` and each secret file at mode `0600` on Unix hosts. Use a protected
secret manager or provisioning process. Never store the value in TOML, a
command argument, an environment variable, or a log. Cladding does not create,
read, rewrite, or log secret values. The proxy mounts the credentials
directory read-only. Execution containers do not receive it.

The project CA and injection credentials are separate. `cladding init`
creates or reuses a valid project-scoped CA in
`.cladding/credentials/baffle/`. It installs only the public `ca.crt` in the
agent and enabled network-sandbox system trust stores by running
`podman exec --user 0` after container creation. UID 0 applies to that exec
inside the container; the workload remains unprivileged. The default image
sets `NODE_USE_SYSTEM_CA=1`. Custom agent and network-sandbox images need `sh`,
`cp`, a writable `/usr/local/share/ca-certificates/`, and
`update-ca-certificates` or a compatible command that updates the system
trust store. Applications with private CA bundles may need separate setup.
Certificate-pinned applications may reject intercepted connections.

To rotate the CA, stop the project and move the full
`.cladding/credentials/baffle/` directory to a protected backup outside
version control. Start the project to generate a new CA, then provision the
required secret files again. Distribute the new public certificate and update
client trust stores before removing trust in the old CA. The backup includes
the private CA key and any injection secrets.

## Proxy behavior and migration limits

The application's proxy URL stays `http://127.0.0.1:3128`. The `http://`
scheme is for the connection to the local HTTP proxy; Baffle allows HTTPS
origins through `CONNECT` but rejects ordinary plaintext HTTP origins. Rules
must use exact DNS hostnames and explicit ports. Baffle does not accept
wildcard hostnames or literal IP addresses.

Baffle checks hostnames and ports. It does not pin DNS results or filter
destination IP addresses. A rule for a hostname does not prevent that name
from resolving to an internal or private IP. Use separate DNS or network
controls if the proxy's outbound addresses need limits.

Cladding does not read or convert Squid configuration, domain lists, or
host-port lists. Replace them with native Baffle TOML. The `cladding expose`
and `cladding inject` commands remain separate, deliberate host-network
exceptions. The filesystem sandbox remains without proxy egress by default.

## Troubleshooting

- **UID or socket permission errors:** Keep the socket parent at mode `0700`
  and the socket at `0600`. Cladding uses ordinary rootless `keep-id` mappings
  so the proxy and its matching execution container can access the same
  socket as the invoking user. Do not widen socket modes to fix a UID mapping
  problem. On macOS with Podman machine, confirm that the proxy image includes
  `socat` for the scoped relay path.
- **File-only validation fails:** Keep the managed TOML paths as real files
  and directories, without symlinks. The generated directories use mode
  `0755`; TOML files use mode `0644`. They must not be group- or world-writable
  and must be owned by the user mapped to Baffle's trusted proxy UID.
- **CA installation fails:** Check that the custom agent or network-sandbox
  image has `sh`, `cp`, a writable system CA directory, and a working
  `update-ca-certificates` command. Cladding reports an installation error
  during startup and cleans up resources it created for that invocation.
- **A session reports a missing credential:** Confirm that the name in the
  session rule matches both `[secrets].allowed` and a file under the secrets
  directory. Confirm that the file is readable by the proxy's mapped UID and
  has mode `0600` on the host.
- **A request fails just after `up`:** Cladding does not wait for Baffle
  session readiness. Retry after startup. Check `cladding logs proxy` and the
  proxy container exit status for startup errors.
- **A reload fails or policy seems stale:** Read each `reloaded`, `unchanged`,
  or `failed` result, then inspect `cladding logs proxy`. A failed update keeps
  the previous session policy. Existing connections keep their old policy
  and credentials until they close.
