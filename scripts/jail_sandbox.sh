#!/bin/sh
set -e # Exit immediately if a command fails

echo "Starting Firewall Setup for SANDBOX..."

# Install dependencies
apk add --no-cache nftables iproute2

# 1. Resolve Proxy IP
# We wait until we get an IP, just in case Proxy is slow to start
PROXY_IP=""
while [ -z "$PROXY_IP" ]; do
  echo "Waiting for proxy..."
  PROXY_IP=$(getent hosts proxy-pod | awk '$1 ~ /^[0-9]+\./ { print $1; exit }')
  sleep 1
done

echo "Proxy detected at: $PROXY_IP"

# 2. Flush existing rules (start fresh)
nft flush ruleset

# 3. Create Table and Chains
nft add table ip filter
nft add chain ip filter INPUT { type filter hook input priority 0 \; policy accept \; }
nft add chain ip filter OUTPUT { type filter hook output priority 0 \; policy accept \; }

# 4. RULES

# Allow Loopback (Localhost) - Critical for internal app processes
nft add rule ip filter OUTPUT oifname "lo" accept

# Allow DNS (UDP/TCP 53)
nft add rule ip filter OUTPUT udp dport 53 accept
nft add rule ip filter OUTPUT tcp dport 53 accept

# Allow Return Traffic (Stateful firewall)
nft add rule ip filter OUTPUT ct state established,related accept

# Allow Outbound to Proxy
nft add rule ip filter OUTPUT ip daddr $PROXY_IP accept

# Log and Drop everything else
# (Optional: remove 'log prefix' if you don't want logs spamming podman logs)
nft add rule ip filter OUTPUT log prefix \"DROP_SANDBOX: \" drop
nft add rule ip filter OUTPUT drop

echo "Sandbox Firewall Locked. Sleeping infinity..."
exec sleep infinity
