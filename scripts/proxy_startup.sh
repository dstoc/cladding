#!/bin/sh
set -e

echo "--- Proxy Startup (Squid) ---"

require_env() {
  name="$1"
  eval "value=\${$name:-}"
  if [ -z "$value" ]; then
    echo "Missing required environment variable: $name" >&2
    exit 1
  fi
}

require_env CLADDING_PROXY_NAME
require_env CLADDING_SANDBOX_NAME
require_env CLADDING_AGENT_NAME

# 1. Define paths
# SOURCE: The template file mounted from your host (Read-Only)
CFG_SRC="/opt/config/squid.conf"
# DESTINATION: The generated runtime config
CFG_DST="/tmp/squid_generated.conf"
AGENT_IPS_FILE="/tmp/agent_ips.lst"
SANDBOX_IPS_FILE="/tmp/nw_sandbox_ips.lst"
DNS_IP=""

# 2. Wait for peers (agent and network sandbox).
AGENT_IP=""
SANDBOX_IP=""

while [ -z "$AGENT_IP" ] || [ -z "$SANDBOX_IP" ]; do
    echo "Resolving peers..."
    AGENT_IPS="$(
      {
        getent hosts "$CLADDING_AGENT_NAME" 2>/dev/null || true
        getent hosts "$CLADDING_AGENT_NAME-agent" 2>/dev/null || true
        getent hosts "$CLADDING_AGENT_NAME-agent-node" 2>/dev/null || true
        getent hosts "$CLADDING_AGENT_NAME-infra" 2>/dev/null || true
      } | awk '$1 ~ /^[0-9]+\./ { print $1 }' | sort -u
    )"
    SANDBOX_IPS="$(
      {
        getent hosts "$CLADDING_SANDBOX_NAME" 2>/dev/null || true
        getent hosts "$CLADDING_SANDBOX_NAME-nw-sandbox" 2>/dev/null || true
        getent hosts "$CLADDING_SANDBOX_NAME-sandbox-node" 2>/dev/null || true
        getent hosts "$CLADDING_SANDBOX_NAME-infra" 2>/dev/null || true
      } | awk '$1 ~ /^[0-9]+\./ { print $1 }' | sort -u
    )"
    AGENT_IP=$(printf "%s\n" "$AGENT_IPS" | awk 'NF { print; exit }')
    SANDBOX_IP=$(printf "%s\n" "$SANDBOX_IPS" | awk 'NF { print; exit }')
    
    if [ -z "$AGENT_IP" ] || [ -z "$SANDBOX_IP" ]; then
        sleep 2
    fi
done

DNS_IP=$(awk '$1 == "nameserver" && $2 ~ /^[0-9]+\./ { print $2; exit }' /etc/resolv.conf)
if [ -z "$DNS_IP" ]; then
    DNS_IP="10.89.0.1"
fi

echo "Found agent: $AGENT_IP"
echo "Found network sandbox: $SANDBOX_IP"
echo "Using DNS: $DNS_IP"
printf "%s\n" "$AGENT_IPS" | awk 'NF' > "$AGENT_IPS_FILE"
printf "%s\n" "$SANDBOX_IPS" | awk 'NF' > "$SANDBOX_IPS_FILE"
echo "Agent allow-list file: $AGENT_IPS_FILE"
echo "Network sandbox allow-list file: $SANDBOX_IPS_FILE"

# 3. Inject IPs
# Copy the template to /tmp/
cp "$CFG_SRC" "$CFG_DST"

# Replace placeholders with actual runtime values
sed -i "s/REPLACE_DNS_IP/$DNS_IP/g" "$CFG_DST"
sed -i "s/REPLACE_PROXY_NAME/$CLADDING_PROXY_NAME/g" "$CFG_DST"
sed -i "s/REPLACE_SANDBOX_NAME/$CLADDING_SANDBOX_NAME/g" "$CFG_DST"
sed -i "s/REPLACE_AGENT_NAME/$CLADDING_AGENT_NAME/g" "$CFG_DST"

echo "Config generated at $CFG_DST. Starting Squid..."

# 4. Start Squid in foreground using generated config.
exec squid -N -f "$CFG_DST"
