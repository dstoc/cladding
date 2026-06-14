# Proxy Feature Summary

## Purpose
- Provide controlled outbound internet access for `agent` and `nw-sandbox` through a single policy enforcement point.
- Enforce source-based and destination-based egress restrictions.

## Current Implementation
1. Proxy engine: Squid (`docker.io/ubuntu/squid:latest`) in `<name>-proxy`.
2. Startup entrypoint: `scripts/proxy_startup.sh`.
3. Runtime config template: `config/squid.conf`.
4. Domain allow-lists:
- `config/agent_domains.lst`
- `config/nw_sandbox_domains.lst`
5. Reload command:
- `./reload-proxy-config`

## Runtime Flow
1. `./up` starts pods on `secure_net`.
2. `<name>-proxy` startup script resolves peer IPs for the agent and network sandbox.
3. Startup writes:
- `/tmp/agent_ips.lst`
- `/tmp/nw_sandbox_ips.lst`
4. Startup reads container DNS nameserver and injects it into generated Squid config.
5. Startup launches Squid in foreground with `/tmp/squid_generated.conf`.
6. `agent` and `nw-sandbox` send outbound traffic to `<name>-proxy:8080` (via env vars for `agent`).

## Policy Model
1. Source identity:
- `agent_src` matches `/tmp/agent_ips.lst`
- `nw_sandbox_src` matches `/tmp/nw_sandbox_ips.lst`
2. Destination control:
- `agent_domains` from `config/agent_domains.lst`
- `nw_sandbox_domains` from `config/nw_sandbox_domains.lst`
3. Port/method guardrails:
- CONNECT only allowed to SSL ports (443)
- Safe ports restricted to 80/443
- Default deny for unmatched traffic

## Related Security Layer
- nftables jailers still apply in:
1. `scripts/jail_agent.sh`
2. `scripts/jail_nw_sandbox.sh`
- These restrict direct egress and force proxy-mediated access paths.

## Verification
1. Proxy health:
- `podman logs <name>-proxy-proxy`
2. Tunnel success test:
- `podman exec -it <name>-agent-agent curl -v https://googleapis.com`
3. Expected proxy log signal:
- `TCP_TUNNEL/200 ... CONNECT googleapis.com:443 ...`
