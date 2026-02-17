#!/bin/sh
set -e

echo "--- Proxy Startup ---"

# 1. Define paths
# SOURCE: The template file mounted from your host (Read-Only)
CFG_SRC="/opt/haproxy/haproxy.cfg"
# DESTINATION: The temporary location where we write the final config with IPs
CFG_DST="/tmp/haproxy_generated.cfg"

# 2. Wait for Peers (CLI and Sandbox)
CLI_IP=""
SANDBOX_IP=""

while [ -z "$CLI_IP" ] || [ -z "$SANDBOX_IP" ]; do
    echo "Resolving peers..."
    CLI_IP=$(getent hosts cli-pod | awk '{ print $1 }' | head -n 1)
    SANDBOX_IP=$(getent hosts sandbox-pod | awk '{ print $1 }' | head -n 1)
    
    if [ -z "$CLI_IP" ] || [ -z "$SANDBOX_IP" ]; then
        sleep 2
    fi
done

echo "Found CLI: $CLI_IP"
echo "Found Sandbox: $SANDBOX_IP"

# 3. Inject IPs
# Copy the template to /tmp/
cp "$CFG_SRC" "$CFG_DST"

# Replace placeholders with actual IPs
sed -i "s/REPLACE_CLI_IP/$CLI_IP/g" "$CFG_DST"
sed -i "s/REPLACE_SANDBOX_IP/$SANDBOX_IP/g" "$CFG_DST"

echo "Config generated at $CFG_DST. Starting HAProxy..."

# 4. Start HAProxy using the NEW config file in /tmp
exec haproxy -f "$CFG_DST"
