#!/bin/sh
set -e

echo "--- Proxy Startup (Squid) ---"

# 1. Define paths
# SOURCE: The template file mounted from your host (Read-Only)
CFG_SRC="/opt/config/squid.conf"
# DESTINATION: The generated runtime config
CFG_DST="/tmp/squid_generated.conf"
CLI_IPS_FILE="/tmp/cli_ips.lst"
SANDBOX_IPS_FILE="/tmp/sandbox_ips.lst"
DNS_IP=""

# 2. Wait for Peers (CLI and Sandbox)
CLI_IP=""
SANDBOX_IP=""

while [ -z "$CLI_IP" ] || [ -z "$SANDBOX_IP" ]; do
    echo "Resolving peers..."
    CLI_IPS="$(
      {
        getent hosts cli-pod 2>/dev/null || true
        getent hosts cli-pod-cli-app 2>/dev/null || true
        getent hosts cli-pod-cli-node 2>/dev/null || true
        getent hosts cli-pod-infra 2>/dev/null || true
      } | awk '$1 ~ /^[0-9]+\./ { print $1 }' | sort -u
    )"
    SANDBOX_IPS="$(
      {
        getent hosts sandbox-pod 2>/dev/null || true
        getent hosts sandbox-pod-sandbox-app 2>/dev/null || true
        getent hosts sandbox-pod-sandbox-node 2>/dev/null || true
        getent hosts sandbox-pod-infra 2>/dev/null || true
      } | awk '$1 ~ /^[0-9]+\./ { print $1 }' | sort -u
    )"
    CLI_IP=$(printf "%s\n" "$CLI_IPS" | awk 'NF { print; exit }')
    SANDBOX_IP=$(printf "%s\n" "$SANDBOX_IPS" | awk 'NF { print; exit }')
    
    if [ -z "$CLI_IP" ] || [ -z "$SANDBOX_IP" ]; then
        sleep 2
    fi
done

DNS_IP=$(awk '$1 == "nameserver" && $2 ~ /^[0-9]+\./ { print $2; exit }' /etc/resolv.conf)
if [ -z "$DNS_IP" ]; then
    DNS_IP="10.89.0.1"
fi

echo "Found CLI: $CLI_IP"
echo "Found Sandbox: $SANDBOX_IP"
echo "Using DNS: $DNS_IP"
printf "%s\n" "$CLI_IPS" | awk 'NF' > "$CLI_IPS_FILE"
printf "%s\n" "$SANDBOX_IPS" | awk 'NF' > "$SANDBOX_IPS_FILE"
echo "CLI allow-list file: $CLI_IPS_FILE"
echo "Sandbox allow-list file: $SANDBOX_IPS_FILE"

# 3. Inject IPs
# Copy the template to /tmp/
cp "$CFG_SRC" "$CFG_DST"

# Replace placeholders with actual runtime values
sed -i "s/REPLACE_DNS_IP/$DNS_IP/g" "$CFG_DST"

echo "Config generated at $CFG_DST. Starting Squid..."

# 4. Start Squid in foreground using generated config.
exec squid -N -f "$CFG_DST"
