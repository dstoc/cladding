# PRD: UDS-Based Network Isolation

## Objective
Replace in-container nftables jailers with a Unix-domain-socket communication model.

All non-proxy execution pods should run without external network access. Communication that currently uses pod IPs, DNS names, and nftables allow rules should move to pod-local loopback ports backed by sidecar bridges over host-mounted Unix sockets.

The proxy remains the only component with normal network egress.

The shared Cladding bridge network pool (`cladding-N`) should be removed as part of this change. Internal component communication no longer needs a shared bridge, and the proxy pod can use Podman's default network for outbound internet/DNS access.

## Motivation
The current network isolation model depends on init-container jailers:

- `scripts/jail_agent.sh`
- `scripts/jail_nw_sandbox.sh`
- `scripts/jail_fs_sandbox.sh`

Those scripts install and configure `nftables` inside the pod network namespace. This works with the current default OCI runtime, but it has several problems:

- it depends on kernel netfilter APIs being available inside the container runtime
- it failed under `runsc` because gVisor does not expose the required netlink/netfilter API
- it requires `NET_ADMIN` in init containers
- it couples security policy to mutable in-container firewall setup
- it still uses network reachability and source IP identity between Cladding components

A UDS-based model gives Cladding more explicit capabilities: a component can only talk to services for which Cladding mounts a socket and starts a bridge.

## Problem statement
The current runtime uses direct network paths:

- agent uses `http://<name>-proxy:8080` for internet proxying
- nw-sandbox uses `http://<name>-proxy:8080` for internet proxying
- agent uses `http://<name>-nw-sandbox:3000/raw` for network-sandbox delegated execution
- agent uses `http://<name>-fs-sandbox:3000/raw` for filesystem-sandbox delegated execution
- proxy identifies clients by source IP and generated Squid config

The jailer scripts narrow those paths with nftables. Without the jailers, networked pods can often reach more than intended. With gVisor, the jailers do not work.

We need a model where the absence of a mounted socket and sidecar bridge means the communication path does not exist.

## Proposal
Move component communication to Unix sockets and pod-local loopback adapters.

### Runtime topology
Each managed component remains a Podman pod, but non-proxy pods should not attach to the shared Cladding bridge network.

Recommended initial network modes:

- proxy pod: Podman's default network, with outbound egress as required by Squid
- agent pod: `--network=none`
- nw-sandbox pod: `--network=none`
- fs-sandbox pod: `--network=none`

Inside a pod, app containers can still use loopback. Sidecars in the same pod listen on loopback ports that match the current app expectations and bridge those connections to Unix sockets.

This deliberately relies on the pod-shared loopback interface. It does not rely on one container in a pod having network access while another does not.

Because internal communication moves to UDS, Cladding should stop allocating and managing shared bridge networks:

- no `cladding-N` network pool
- no deterministic component IP addresses
- no Podman `--ip` assignments for managed pods
- no host aliases for component DNS names
- no source-IP discovery for proxy identity

The proxy pod's default Podman network is sufficient for internet and DNS access.

### Socket directory
Create a runtime socket directory under `.cladding`, for example:

```text
.cladding/runtime/sockets/
```

The directory should be created by `cladding up` with restrictive permissions.

Mount only the required socket paths into the sidecar containers that need them. The main app containers should not receive broad access to the socket directory unless there is a concrete reason.

For example:

```text
agent app container:
  no /run/cladding/sockets mount
  HTTP proxy: http://127.0.0.1:3128
  RUN_NW_SANDBOX_SERVER=http://127.0.0.1:3001/raw
  RUN_FS_SANDBOX_SERVER=http://127.0.0.1:3002/raw

agent sidecars:
  127.0.0.1:3128 -> /run/cladding/sockets/agent-proxy.sock
  127.0.0.1:3001 -> /run/cladding/sockets/nw-sandbox-run.sock
  127.0.0.1:3002 -> /run/cladding/sockets/fs-sandbox-run.sock
```

### Proxy path
Replace direct proxy networking with an identity-specific socket path.

Initial shape:

```text
agent pod:
  proxy-client sidecar listens on 127.0.0.1:3128
  forwards to /run/cladding/sockets/agent-proxy.sock

nw-sandbox pod:
  proxy-client sidecar listens on 127.0.0.1:3128
  forwards to /run/cladding/sockets/nw-sandbox-proxy.sock

proxy pod:
  proxy-socket sidecar accepts agent-proxy.sock
  proxy-socket sidecar accepts nw-sandbox-proxy.sock
  forwards to Squid on localhost
```

The app containers continue to use normal HTTP proxy environment variables, but those variables point at local loopback:

```text
http_proxy=http://127.0.0.1:3128
https_proxy=http://127.0.0.1:3128
HTTP_PROXY=http://127.0.0.1:3128
HTTPS_PROXY=http://127.0.0.1:3128
```

`fs-sandbox` should receive no proxy sidecar and no proxy environment variables.

Agent host access should continue to be enforced by the proxy. The existing `agent/host_ports.lst` file remains meaningful: requests from the agent identity to `host.containers.internal` should be allowed only for ports listed in `/opt/config/agent/host_ports.lst`.

### Proxy identity
The old source-IP identity model should be replaced.

Identify clients by the proxy-side socket endpoint they use. The proxy pod should bridge each identity-specific UDS endpoint to a distinct Squid localhost port. A single Squid instance can listen on multiple ports, and each port should be named in Squid config.

For example:

- `/run/cladding/sockets/agent-proxy.sock` means agent identity
- `/run/cladding/sockets/nw-sandbox-proxy.sock` means nw-sandbox identity

The proxy pod should run byte-for-byte bridge sidecars:

```text
agent-proxy.sock -> 127.0.0.1:3128
nw-sandbox-proxy.sock -> 127.0.0.1:3129
```

Squid config should name those listeners and use `myportname` ACLs:

```squidconf
http_port 127.0.0.1:3128 name=agent
http_port 127.0.0.1:3129 name=nw_sandbox

acl from_agent myportname agent
acl from_nw_sandbox myportname nw_sandbox

http_access allow from_agent agent_domains
http_access allow from_agent agent_host agent_host_ports
http_access allow from_nw_sandbox nw_sandbox_domains
http_access deny all
```

This keeps identity out of the app containers. The app only knows about its pod-local proxy port, while Cladding controls which proxy socket bridge exists for each pod.

### Delegated execution paths
Replace direct agent-to-sandbox HTTP networking with UDS bridges.

For nw-sandbox:

```text
nw-sandbox pod:
  mcp-run listens on 127.0.0.1:3000
  run-server sidecar accepts /run/cladding/sockets/nw-sandbox-run.sock
  run-server sidecar forwards to 127.0.0.1:3000

agent pod:
  run-client sidecar listens on 127.0.0.1:3001
  run-client sidecar forwards to /run/cladding/sockets/nw-sandbox-run.sock
  RUN_NW_SANDBOX_SERVER=http://127.0.0.1:3001/raw
```

For fs-sandbox:

```text
fs-sandbox pod:
  mcp-run listens on 127.0.0.1:3000
  run-server sidecar accepts /run/cladding/sockets/fs-sandbox-run.sock
  run-server sidecar forwards to 127.0.0.1:3000

agent pod:
  run-client sidecar listens on 127.0.0.1:3002
  run-client sidecar forwards to /run/cladding/sockets/fs-sandbox-run.sock
  RUN_FS_SANDBOX_SERVER=http://127.0.0.1:3002/raw
```

The agent app container does not need DNS or pod IPs for sandbox endpoints.

### Sidecar implementation
Use `socat` for byte-for-byte bridge paths where no protocol modification is required:

- loopback TCP to UDS
- UDS to loopback TCP

The proxy identity model above deliberately keeps every bridge byte-for-byte. No sidecar should need to parse HTTP, inspect CONNECT requests, or inject proxy credentials.

### Removal of jailers
Remove the nftables jailer init containers from non-proxy pods:

- no `NET_ADMIN` init containers
- no `apk add nftables`
- no runtime dependence on netfilter

The existing jail scripts can remain in the repository during migration, but they should not be used by the new UDS runtime path.

### `cladding expose`
`cladding expose` should remain supported under `--network=none`.

Today it starts an `alpine/socat` helper on the shared Podman network and forwards host traffic to the agent IP and target port. In the UDS model, the agent pod has no network address that an expose helper should target.

Replace that direct network target with a temporary UDS bridge pair created by `cladding expose`.

When the project is already running and the user runs:

```bash
cladding expose 3000 9000
```

Cladding should:

1. Create a temporary socket path under the runtime directory:

   ```text
   .cladding/runtime/expose/agent-3000-9000.sock
   ```

2. Start an expose sidecar inside the already-running agent pod:

   ```text
   UNIX-LISTEN:/run/cladding/expose/agent-3000-9000.sock,fork,reuseaddr
     -> TCP:127.0.0.1:3000
   ```

   Because the sidecar joins the agent pod, `127.0.0.1:3000` means the agent pod's loopback namespace.

3. Start a separate host-facing expose helper container:

   ```text
   TCP-LISTEN:9000,fork,reuseaddr,bind=127.0.0.1
     -> UNIX-CONNECT:/run/cladding/expose/agent-3000-9000.sock
   ```

Traffic then flows as:

```text
host 127.0.0.1:9000
  -> expose helper container
  -> temporary Unix socket
  -> expose sidecar in agent pod
  -> 127.0.0.1:3000 inside agent pod
```

This keeps the existing dynamic `cladding expose` UX without publishing new ports on the already-created agent pod. It also avoids attaching the agent pod to a network.

The expose helper and expose sidecar should both be labeled for cleanup:

```text
cladding=<name>
project_root=<project-root>
cladding_expose=true
cladding_expose_target=agent
cladding_expose_container_port=<container-port>
cladding_expose_host_port=<host-port>
cladding_expose_role=host-helper|pod-sidecar
```

`cladding expose list` should continue listing host-facing exposes by host port and container port. It may derive the public status from the host-helper container, but cleanup should understand both roles.

`cladding expose stop <host-port>` should:

- remove the host-helper container
- remove the matching pod-sidecar container
- remove the matching socket file

`cladding down` should:

- remove all expose host-helper containers for the project
- remove all expose pod-sidecar containers for the project
- delete `.cladding/runtime/expose/`

`cladding up` should start from a clean runtime socket directory so stale socket files from crashes cannot affect the new runtime.

`cladding destroy` should also remove expose containers and runtime socket files.

## Non-goals
1. Do not remove Podman pods.
2. Do not add gVisor support in this proposal, though this change should make it easier.
3. Do not preserve source-IP based Squid identity.
4. Do not add proxy username/password identity.
5. Do not require `NET_ADMIN` or nftables in the new runtime path.
6. Do not attach the agent pod to a network to support `cladding expose`.
7. Do not change `mcp-run` protocol semantics.

## Suggested implementation shape
1. Implement direct Podman runtime management first so sidecars, mounts, and network modes can be expressed per pod/container.
2. Add a runtime socket directory setup step under `.cladding/runtime/sockets`.
3. Add sidecar container specs for:
   - agent proxy client bridge
   - nw-sandbox proxy client bridge
   - agent-to-nw-sandbox run bridge
   - agent-to-fs-sandbox run bridge
   - proxy-side socket listeners
   - sandbox-side run socket listeners
4. Change non-proxy pods to `--network=none`.
5. Change app env vars to loopback endpoints.
6. Remove host aliases and DNS dependencies for non-proxy communication.
7. Remove jailer init tasks from the UDS runtime path.
8. Update Squid config generation to use the new identity model.
9. Rework `cladding expose` to create temporary UDS bridge pairs instead of forwarding to the agent pod IP.

## Cleanup milestone
After the UDS bridges are in place, remove the old shared-network management code.

The cleanup should remove or substantially simplify:

- `cladding/src/network.rs` network pool allocation and deterministic IP calculation
- `select_available_network_settings_for_config()`
- `resolve_active_project_network_settings()`
- `ensure_pool_network_settings()`
- `list_running_project_networks()` if it is only needed to recover `cladding-N`
- `podman network create`, `podman network inspect`, and `podman network ls` usage for runtime startup
- `podman play kube --network` and `--ip` equivalents in the direct runtime path
- generated host aliases that only exist for component-to-component DNS
- proxy startup peer IP discovery and `/tmp/*_ips.lst` generation
- Squid `src` ACLs for agent and nw-sandbox identity

Project runtime discovery should continue to use pod labels:

- `cladding=<name>`
- `project_root=<project-root>`
- `app=<component>`

Commands that currently resolve the active network should instead resolve the running project directly by labels and expected component names. For example:

- `cladding run` should find `<name>-agent-instance`
- `cladding run-with-scissors` should find the selected sandbox instance
- `cladding logs` should find the selected component instance
- `cladding expose` should find the running agent pod by label/name and attach the expose sidecar to it

`cladding ps` should report running projects based on labeled pods only; it should not require or display a `cladding-N` network.

## Migration plan
This should be implemented as a runtime behavior change without requiring existing project config changes.

Existing config files should continue to define the same policy inputs:

- `agent/domains.lst`
- `agent/host_ports.lst`
- `nw_sandbox/domains.lst`
- `nw_sandbox/*.rego`
- `fs_sandbox/*.rego`
- `proxy/squid.conf`

`agent/host_ports.lst` should continue to be required. It authorizes agent-originated proxy access to `host.containers.internal` ports. The enforcement moves from nftables plus Squid source-IP ACLs to Squid listener-identity ACLs, but the user-facing allowlist file remains the same.

## Verification
Observable checks should include:

- non-proxy pods are created with no external network
- agent can reach the proxy only through `127.0.0.1:3128`
- agent can reach `host.containers.internal:<port>` through the proxy only when `<port>` is listed in `agent/host_ports.lst`
- nw-sandbox can reach the proxy only through `127.0.0.1:3128`
- fs-sandbox has no proxy environment variables and no proxy bridge sidecar
- no `cladding-N` network is created during `cladding up`
- non-proxy pods have no network and the proxy pod uses Podman's default network
- agent can call `run-in-nw-sandbox --check -- true`
- agent can call `run-in-fs-sandbox --check -- true`
- removing the nw-sandbox run socket breaks `run-in-nw-sandbox` fail-closed
- removing the proxy socket breaks internet access fail-closed
- Squid can distinguish agent and nw-sandbox identities without source IP
- `cladding expose 3000 9000` creates a host-helper container, an agent-pod sidecar, and a temporary expose socket
- host traffic to `127.0.0.1:9000` reaches `127.0.0.1:3000` inside the agent pod
- `cladding expose stop 9000` removes both expose containers and the socket file
- `cladding down` removes all project expose containers and deletes `.cladding/runtime/expose/`

## Success criteria
1. No non-proxy runtime path depends on nftables.
2. Non-proxy pods do not attach to the shared Cladding network.
3. Agent, nw-sandbox, and fs-sandbox communication works through explicit UDS bridges.
4. Squid identity no longer depends on source IP.
5. `cladding expose` remains dynamic without giving the agent pod external network access.
6. Cladding no longer creates or inspects `cladding-N` bridge networks.
7. The runtime is compatible with OCI runtimes that do not expose netfilter APIs.
