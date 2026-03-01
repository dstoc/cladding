#!/bin/sh
set -e

echo "Starting Firewall Setup for CLI..."

# Install nftables (we don't need bind-tools/dig anymore!)
apk add --no-cache nftables

# 1. Resolve Internal Services
# We need to know where our friends are.
SANDBOX_IP=""
PROXY_IP=""
HOST_IP=""

while [ -z "$SANDBOX_IP" ] || [ -z "$PROXY_IP" ] || [ -z "$HOST_IP" ]; do
  echo "Waiting for sandbox, proxy, and host gateway..."
  SANDBOX_IP=$(getent hosts sandbox-pod | awk '$1 ~ /^[0-9]+\./ { print $1; exit }')
  PROXY_IP=$(getent hosts proxy-pod | awk '$1 ~ /^[0-9]+\./ { print $1; exit }')
  HOST_IP=$(getent hosts host.containers.internal | awk '$1 ~ /^[0-9]+\./ { print $1; exit }')
  sleep 2
done

echo "Sandbox detected at: $SANDBOX_IP"
echo "Proxy detected at:   $PROXY_IP"
echo "Host detected at:    $HOST_IP"

# 2. Flush and Start Fresh
nft flush ruleset
nft add table ip filter
nft add chain ip filter OUTPUT { type filter hook output priority 0 \; policy accept \; }

# 3. RULES

# A. Allow Loopback (Localhost)
# Essential for local processes talking to themselves
nft add rule ip filter OUTPUT oifname "lo" accept

# B. Allow Return Traffic
# Allow replies to come back to us
nft add rule ip filter OUTPUT ct state established,related accept

# C. Allow Outbound to Sandbox (Direct Access)
nft add rule ip filter OUTPUT ip daddr $SANDBOX_IP tcp dport 3000 accept

# D. Allow Outbound to Host (Direct Access)
# Allow host gateway access; use allowlist if present.
HOST_PORTS_FILE="/opt/config/cli_host_ports.lst"
HOST_PORTS=""
if [ -r "$HOST_PORTS_FILE" ]; then
  HOST_PORTS=$(awk 'NF && $1 !~ /^#/ { print $1 }' "$HOST_PORTS_FILE")
fi

if [ -n "$HOST_PORTS" ]; then
  for PORT in $HOST_PORTS; do
    nft add rule ip filter OUTPUT ip daddr $HOST_IP tcp dport $PORT accept
  done
fi

# E. Allow Outbound to Proxy (Internet Access)
# The CLI will send all Google traffic here
nft add rule ip filter OUTPUT ip daddr $PROXY_IP tcp dport 8080 accept

# F. Drop Everything Else
# If it's not Sandbox or Proxy, it's blocked.
nft add rule ip filter OUTPUT log prefix \"BLOCKED_CLI: \" drop
nft add rule ip filter OUTPUT drop

if [ "${JAILER_HOLD:-0}" = "1" ]; then
  echo "CLI Firewall Locked. Traffic restricted to Sandbox & Proxy. Sleeping infinity..."
  exec sleep infinity
fi

echo "CLI Firewall Locked. Traffic restricted to Sandbox & Proxy. Exiting."
