# Proxy Feature Summary

## Purpose
- Provide controlled outbound internet access for `cli-app` and `sandbox-app` through a single policy enforcement point.
- Enforce source-based and destination-based egress restrictions.

## Current Implementation
1. Proxy engine: Squid (`docker.io/ubuntu/squid:latest`) in `proxy-pod`.
2. Startup entrypoint: `scripts/proxy_startup.sh`.
3. Runtime config template: `config/squid.conf`.
4. Domain allow-lists:
- `config/cli_domains.lst`
- `config/sandbox_domains.lst`
5. Reload command:
- `./reload-proxy-config`

## Runtime Flow
1. `./up` starts pods on `secure_net`.
2. `proxy-pod` startup script resolves peer IPs for CLI and Sandbox pods.
3. Startup writes:
- `/tmp/cli_ips.lst`
- `/tmp/sandbox_ips.lst`
4. Startup reads container DNS nameserver and injects it into generated Squid config.
5. Startup launches Squid in foreground with `/tmp/squid_generated.conf`.
6. `cli-app` and `sandbox-app` send outbound traffic to `proxy-pod:8080` (via env vars for `cli-app`).

## Policy Model
1. Source identity:
- `cli_src` matches `/tmp/cli_ips.lst`
- `sandbox_src` matches `/tmp/sandbox_ips.lst`
2. Destination control:
- `cli_domains` from `config/cli_domains.lst`
- `sandbox_domains` from `config/sandbox_domains.lst`
3. Port/method guardrails:
- CONNECT only allowed to SSL ports (443)
- Safe ports restricted to 80/443
- Default deny for unmatched traffic

## Related Security Layer
- nftables jailers still apply in:
1. `scripts/jail_cli.sh`
2. `scripts/jail_sandbox.sh`
- These restrict direct egress and force proxy-mediated access paths.

## Verification
1. Proxy health:
- `podman logs proxy-pod-proxy`
2. Tunnel success test:
- `podman exec -it cli-pod-cli-app curl -v https://googleapis.com`
3. Expected proxy log signal:
- `TCP_TUNNEL/200 ... CONNECT googleapis.com:443 ...`
