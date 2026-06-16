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

# 1. Define paths
# SOURCE: The template file mounted from your host (Read-Only)
CFG_SRC="/opt/config/proxy/squid.conf"
# DESTINATION: The generated runtime config
CFG_DST="/tmp/squid_generated.conf"
NW_SANDBOX_DOMAINS_FILE="/tmp/nw_sandbox_domains.lst"
DNS_IP=""

if [ -n "${CLADDING_SANDBOX_NAME:-}" ]; then
    NW_SANDBOX_DOMAINS_FILE="/opt/config/nw_sandbox/domains.lst"
else
    : > "$NW_SANDBOX_DOMAINS_FILE"
fi

DNS_IP=$(awk '$1 == "nameserver" && $2 ~ /^[0-9]+\./ { print $2; exit }' /etc/resolv.conf)
if [ -z "$DNS_IP" ]; then
    DNS_IP="10.89.0.1"
fi

echo "Using DNS: $DNS_IP"
echo "Network sandbox domains file: $NW_SANDBOX_DOMAINS_FILE"

if grep -Eq '/tmp/(agent|nw_sandbox)_ips\.lst|acl (agent|nw_sandbox)_src src|^http_port 8080([[:space:]]|$)' "$CFG_SRC"; then
    echo "error: /opt/config/proxy/squid.conf uses the old source-IP proxy identity model" >&2
    echo "hint: remove .cladding/config/proxy/squid.conf and run 'cladding init' to regenerate it from the current template" >&2
    exit 1
fi

# 2. Inject runtime values
# Copy the template to /tmp/
cp "$CFG_SRC" "$CFG_DST"

# Replace placeholders with actual runtime values
sed -i "s/REPLACE_DNS_IP/$DNS_IP/g" "$CFG_DST"
sed -i "s/REPLACE_PROXY_NAME/$CLADDING_PROXY_NAME/g" "$CFG_DST"
sed -i "s|REPLACE_NW_SANDBOX_DOMAINS_FILE|$NW_SANDBOX_DOMAINS_FILE|g" "$CFG_DST"

echo "Config generated at $CFG_DST. Starting Squid..."

# 3. Start Squid in foreground using generated config.
exec squid -N -f "$CFG_DST"
