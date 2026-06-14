#!/bin/sh
set -e

echo "Starting Firewall Setup for filesystem sandbox..."

# Install dependencies.
apk add --no-cache nftables

# 1. Flush existing rules (start fresh).
nft flush ruleset

# 2. Create table and chains.
nft add table ip filter
nft add chain ip filter INPUT { type filter hook input priority 0 \; policy accept \; }
nft add chain ip filter OUTPUT { type filter hook output priority 0 \; policy accept \; }

# 3. Rules.
nft add rule ip filter OUTPUT oifname "lo" accept
nft add rule ip filter OUTPUT ct state established,related accept
nft add rule ip filter OUTPUT log prefix \"DROP_FS_SANDBOX: \" drop
nft add rule ip filter OUTPUT drop

if [ "${JAILER_HOLD:-0}" = "1" ]; then
  echo "Filesystem sandbox firewall locked. Sleeping infinity..."
  exec sleep infinity
fi

echo "Filesystem sandbox firewall locked. Exiting."
