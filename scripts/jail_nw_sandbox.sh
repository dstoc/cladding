#!/bin/sh
set -e # Exit immediately if a command fails

echo "Starting Firewall Setup for network sandbox..."

require_env() {
  name="$1"
  eval "value=\${$name:-}"
  if [ -z "$value" ]; then
    echo "Missing required environment variable: $name" >&2
    exit 1
  fi
}

require_env CLADDING_PROXY_NAME

# Install dependencies
apk add --no-cache nftables iproute2

# 1. Resolve Proxy IP
# We wait until we get an IP, just in case Proxy is slow to start
PROXY_IP=""
while [ -z "$PROXY_IP" ]; do
  echo "Waiting for proxy..."
  PROXY_IP=$(getent hosts "$CLADDING_PROXY_NAME" | awk '$1 ~ /^[0-9]+\./ { print $1; exit }')
  sleep 1
done

echo "Proxy ($CLADDING_PROXY_NAME) detected at: $PROXY_IP"

# 2. Flush existing rules (start fresh)
nft flush ruleset

# 3. Create Table and Chains
nft add table ip filter
nft add chain ip filter INPUT { type filter hook input priority 0 \; policy accept \; }
nft add chain ip filter OUTPUT { type filter hook output priority 0 \; policy accept \; }

# 4. RULES

# Allow Loopback (Localhost) - Critical for internal app processes
nft add rule ip filter OUTPUT oifname "lo" accept

# Allow Return Traffic (Stateful firewall)
nft add rule ip filter OUTPUT ct state established,related accept

# Allow Outbound to Proxy
nft add rule ip filter OUTPUT ip daddr $PROXY_IP accept

# Log and Drop everything else
# (Optional: remove 'log prefix' if you don't want logs spamming podman logs)
nft add rule ip filter OUTPUT log prefix \"DROP_NW_SANDBOX: \" drop
nft add rule ip filter OUTPUT drop

if [ "${JAILER_HOLD:-0}" = "1" ]; then
  echo "Network sandbox firewall locked. Sleeping infinity..."
  exec sleep infinity
fi

echo "Network sandbox firewall locked. Exiting."
