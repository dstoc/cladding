# Proxy CONNECT Failures in `agent` (2026-02-17)

## Status
- Resolved.
- Current proxy path is Squid-based, not HAProxy-based.

## Final Outcome
1. `agent` can establish HTTPS tunnels through `<name>-proxy:8080`.
2. Confirmed by Squid access log result:
- `TCP_TUNNEL/200 ... CONNECT googleapis.com:443 ...`
3. Proxy process starts cleanly with generated runtime config (`/tmp/squid_generated.conf`).

## Root Cause Summary
1. The prior HAProxy approach was unstable for this use case due to CONNECT handling complexity plus dynamic environment coupling (source identity ACLs, resolver assumptions, and runtime parsing edge cases).
2. DNS resolver assumptions were also brittle when network subnets changed.
3. Source-IP identity needed runtime discovery against pod/container names, not a single static IP assumption.

## What Changed
1. Proxy engine switched from HAProxy to Squid in `pods.yaml`.
2. Added Squid config template at `config/proxy/squid.conf`.
3. Updated `scripts/proxy_startup.sh` to:
- discover agent/network-sandbox IPv4 addresses
- write `/tmp/agent_ips.lst` and `/tmp/nw_sandbox_ips.lst`
- inject runtime DNS into generated config
- start Squid in foreground with `/tmp/squid_generated.conf`
4. Updated `reload-proxy-config` to use Squid reconfigure command.
5. Kept domain allow-lists in:
- `config/agent/domains.lst`
- `config/nw_sandbox/domains.lst`

## Notes
1. Squid logs may include ICMP pinger warnings in this containerized environment.
2. Those pinger warnings do not block proxy CONNECT traffic.
