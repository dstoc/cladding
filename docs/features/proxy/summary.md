# Proxy Feature Summary

## Purpose
- Provide controlled outbound internet access for `agent` and `nw-sandbox` through a single policy enforcement point.
- Enforce listener-based identity and destination-based egress restrictions.

## Current Implementation
1. Proxy engine: Squid (`docker.io/ubuntu/squid:latest`) in `<name>-proxy`.
2. Startup entrypoint: `scripts/proxy_startup.sh`.
3. Runtime config template: `config-template/proxy/squid.conf`.
4. Domain allow-lists:
- `config/agent/domains.lst`
- `config/nw_sandbox/domains.lst`
5. Reload command:
- `./reload-proxy-config`

## Runtime Flow
1. `./up` starts pods on `secure_net`.
2. `<name>-proxy` startup script copies `/opt/config/proxy/squid.conf` to `/tmp/squid_generated.conf`.
3. Startup reads container DNS nameserver and injects it into generated Squid config.
4. Startup points Squid at the network-sandbox domain list when network sandboxing is enabled, otherwise it uses an empty temp file.
5. Startup launches Squid in foreground with `/tmp/squid_generated.conf`.
6. `agent` and `nw-sandbox` send outbound HTTP(S) through the proxy listeners.

## Policy Model
1. Listener identity:
- `from_agent` matches the `agent` Squid listener
- `from_nw_sandbox` matches the `nw_sandbox` Squid listener
2. Destination control:
- `agent_domains` from `config/agent/domains.lst`
- `nw_sandbox_domains` from `config/nw_sandbox/domains.lst`
3. Host access:
- `agent_host_ports` controls `host.containers.internal`
4. Port/method guardrails:
- CONNECT only allowed to SSL ports (443)
- Safe ports restricted to 80/443
- Default deny for unmatched traffic

## Related Security Layer
- Proxy mediation still combines with the container runtime isolation and the per-component policy layers under `.cladding/config/`.

## Verification
1. Proxy health:
- `podman logs <name>-proxy-proxy`
2. Tunnel success test:
- `podman exec -it <name>-agent-agent curl -v https://googleapis.com`
3. Expected proxy log signal:
- `TCP_TUNNEL/200 ... CONNECT googleapis.com:443 ...`
