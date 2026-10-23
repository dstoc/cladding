# Proxy Configuration

Cladding stores editable proxy configuration in `.cladding/config/proxy/`. `cladding init` creates these native Baffle TOML files:

- `daemon.toml` configures the daemon, its private control socket, the scoped data-socket root, CA paths, and the secret directory.
- `sessions/agent.toml` configures the agent session.
- `sessions/nw-sandbox.toml` configures the network-sandbox session.

Both session templates are persistent and use stable data-socket names. They
have no explicit rules and set `unmatched = "deny"`. A fresh project cannot
reach any proxy destination until you add a rule. Add exact hostnames to the
relevant session file to allow the destinations that component needs.

`cladding init` preserves existing session files. If a file contains the old
template's `example.com` rule and you did not add it intentionally, remove it
to block that destination.

The network-sandbox session file may exist when the network sandbox is disabled. The file's presence does not create an active session.

## Selecting session files

Cladding selects `sessions/agent.toml` for the agent and
`sessions/nw-sandbox.toml` for the network sandbox by default. Set
`proxy.agent.session_config` or `proxy.nw_sandbox.session_config` in
`.cladding/cladding.json` to choose another file. Each value is relative to
`.cladding/config/proxy/sessions/` and can include nested directories. Cladding
rejects absolute paths, `.` and `..` path components, and symlinks. The two
components have independent selections.

`cladding build` and `cladding check` apply the same filesystem checks to the
Baffle configuration tree and validate the selected version 2 session file
and component socket name for each enabled component. A disabled network
sandbox does not require its selected file. Baffle validates the session
policy schema and rules when it creates or reloads a session. `cladding run`
uses the selected files directly from the project config directory.
`cladding reload-proxy --run SELECTOR` reloads the active sessions in the
selected run from their selected files. A run reads its session selections at
launch; start a new run to use changed selections.

Example `cladding.json`:

```json
{
  "name": "myproject",
  "agent": { "image": "localhost/cladding-default:latest" },
  "nw_sandbox": { "enabled": true },
  "proxy": {
    "agent": { "session_config": "agent-restricted.toml" },
    "nw_sandbox": { "session_config": "restricted/network.toml" }
  }
}
```

Use this setup sequence:

```bash
cladding init
# Edit .cladding/config/proxy/daemon.toml and the session files as needed.
cladding build
cladding check
cladding run -- sleep 300
```

`cladding build` initializes the project CA when needed and validates and
reuses it on later builds. `cladding check` and `cladding run` validate the CA
but do not initialize or rotate it. Run `cladding build` before `run` when the
project CA is not initialized.

`cladding run` starts the Baffle proxy and enabled execution containers. The
proxy creates the agent session and creates the network-sandbox session only
when that component is enabled. Cladding waits for these sessions before it
marks the run ready. Attach with `cladding logs --run SELECTOR proxy` if a
request continues to fail.

## File ownership and access

The proxy configuration uses Baffle's `file_only` create mode. Initialization creates directories with mode `0755` and TOML files with mode `0644`, owned by the user who runs `cladding init`. Rootless Podman maps the host user's files to the proxy container's trusted UID. Initialization rejects symlinks on the managed configuration paths because Baffle's file-only mode requires real files and directories.

`cladding build` and `cladding check` require these paths to be owned by root or the invoking user. They reject group- or world-writable paths, unreadable files, directories without owner read and execute access, symbolic links, and non-regular entries.

Cladding mounts `config/` read-only into the agent and network sandbox. They can read the non-secret Baffle TOML by design. Store symbolic secret names in `[secrets].allowed`; do not put secret values, private keys, or control-socket paths with access capabilities in TOML. In phase one, execution containers do not mount the Baffle control socket.

The control socket stays inside the proxy container. The agent and network
sandbox receive only their own mode-`0600` data socket inside a mode-`0700`
component directory. Baffle binds each socket directly in its component's
Podman-managed volume on both Linux rootless Podman and macOS Podman machine.
The filesystem sandbox receives no proxy socket or proxy environment by
default.

## Runtime status

Each run's proxy container runs one Baffle daemon in a minimal Debian trixie-slim image. Its startup script verifies the mounted config and credentials, starts the daemon, waits until `baffle list` accepts control commands, then creates the agent session and the network-sandbox session when enabled. Cladding marks the run ready only after these sessions and any fake defaults are available. On shutdown, the script forwards the signal to Baffle; the daemon stops its sessions with the proxy container.

Cladding waits for the required Baffle sessions before it starts the primary workload. Startup errors appear in proxy logs and the run's exit status. `cladding reload-proxy --run SELECTOR` invokes `baffle reload --all` in the selected run's proxy. CI installs the pinned Baffle binary and validates both session files in rootless Podman.

## Reloading policy

`cladding reload-proxy --run SELECTOR` asks Baffle to reload every active file-backed session in that run from the existing native TOML files. Cladding does not regenerate those files. Baffle prints a `reloaded`, `unchanged`, or `failed` result for each session. It continues after a session failure, and the command exits unsuccessfully if any session fails.

A successful reload applies changed policy, resolved injection credentials, and data-socket paths to newly accepted connections. Connections that Baffle already accepted keep their previous policy and credentials until they close. A credential reload does not revoke credentials from open connections. Invalid file updates leave the session's previous effective configuration in place.

Baffle can reject a non-disruptive reload when it cannot retain an old policy generation or listener. Inspect `cladding logs --run SELECTOR proxy` when a session reports `failed`. Reload applies file-backed session changes only. Changes to daemon settings, the CA, or the Baffle binary require a new run.

## Adding an allow rule

To allow HTTPS through `CONNECT` to one exact hostname, add a hostname table to
the relevant session file. Baffle uses HTTPS tunneling on port `443` by default
for a rule with no interception settings:

```toml
[rules."api.example.com"]
```

Keep `unmatched = "deny"` so all destinations without a rule stay blocked.
Add only the hostnames a component needs. For path restrictions or header
injection, use an interception rule and ensure clients trust this project's CA.

## Policy and credential example

The following optional policy restricts interception to paths on one exact
host and uses a symbolic credential name. Add it only when the component needs
this access:

```toml
version = 2
persistent = true
unmatched = "deny"

[rules."registry.example"]
paths = ["/v2/**"]

[[rules."registry.example".inject]]
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
creates private credential storage but does not create the CA. On the first
successful `cladding build`, Cladding runs
`baffle ca init --config /opt/config/proxy/daemon.toml` in the selected proxy
image. Baffle writes the configured CA pair under
`.cladding/credentials/baffle/`. Cladding validates and reuses the pair on
later builds. `cladding check` and `cladding run` validate it before runtime
creation. Baffle creates an ECDSA P-256 certificate that
expires after 365 days, a mode-`0600` private key, and a mode-`0644` public
certificate. A missing or invalid pair after bootstrap is an error; Cladding
does not replace it automatically.

Cladding installs only the public `ca.crt` in the agent and enabled
network-sandbox system trust stores by running `podman exec --user 0` after
container creation. UID 0 applies to that exec inside the container; the
workload remains unprivileged. The default image sets `NODE_USE_SYSTEM_CA=1`.
Custom agent and network-sandbox images need `sh`, `cp`, a writable
`/usr/local/share/ca-certificates/`, and `update-ca-certificates` or a
compatible command that updates the system trust store. Applications with
private CA bundles may need separate setup. Certificate-pinned applications
may reject intercepted connections.

To rotate the CA, stop the project and move the full
`.cladding/credentials/baffle/` directory to a protected backup outside
version control. Run `cladding build` to generate a new CA, then provision the
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
  problem. Confirm that the proxy and consumer mount the same component volume.
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
- **A request fails after run startup:** Check `cladding logs --run SELECTOR
  proxy` and the proxy container exit status for startup errors.
- **A reload fails or policy seems stale:** Read each `reloaded`, `unchanged`,
  or `failed` result, then inspect `cladding logs --run SELECTOR proxy`. A failed update keeps
  the previous session policy. Existing connections keep their old policy
  and credentials until they close.
