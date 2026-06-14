#!/bin/sh
set -e

echo "Starting Firewall Setup for agent..."

require_env() {
  name="$1"
  eval "value=\${$name:-}"
  if [ -z "$value" ]; then
    echo "Missing required environment variable: $name" >&2
    exit 1
  fi
}

require_env CLADDING_PROXY_NAME

# Install nftables.
apk add --no-cache nftables

host_from_server() {
  server="${1#*://}"
  server="${server%%/*}"
  printf '%s\n' "${server%%:*}"
}

resolve_host_ip() {
  getent hosts "$1" 2>/dev/null | awk '$1 ~ /^[0-9]+\./ { print $1; exit }'
}

add_unique_host() {
  host="$1"
  if [ -z "$host" ]; then
    return
  fi
  case " $SANDBOX_HOSTS " in
    *" $host "*) ;;
    *) SANDBOX_HOSTS="$SANDBOX_HOSTS $host" ;;
  esac
}

SANDBOX_HOSTS=""
if [ -n "${RUN_NW_SANDBOX_SERVER:-}" ]; then
  add_unique_host "$(host_from_server "$RUN_NW_SANDBOX_SERVER")"
fi
if [ -n "${RUN_FS_SANDBOX_SERVER:-}" ]; then
  add_unique_host "$(host_from_server "$RUN_FS_SANDBOX_SERVER")"
fi
if [ -n "${CLADDING_SANDBOX_NAME:-}" ]; then
  add_unique_host "$CLADDING_SANDBOX_NAME"
fi

# 1. Resolve Internal Services
# We need to know where our friends are.
PROXY_IP=""
HOST_IP=""
SANDBOX_IPS=""

while :; do
  if [ -z "$PROXY_IP" ]; then
    PROXY_IP=$(resolve_host_ip "$CLADDING_PROXY_NAME")
  fi
  if [ -z "$HOST_IP" ]; then
    HOST_IP=$(resolve_host_ip host.containers.internal)
  fi

  READY=1
  RESOLVED_SANDBOX_IPS=""
  for SANDBOX_HOST in $SANDBOX_HOSTS; do
    SANDBOX_IP=$(resolve_host_ip "$SANDBOX_HOST")
    if [ -z "$SANDBOX_IP" ]; then
      READY=0
      break
    fi
    case " $RESOLVED_SANDBOX_IPS " in
      *" $SANDBOX_IP "*) ;;
      *) RESOLVED_SANDBOX_IPS="$RESOLVED_SANDBOX_IPS $SANDBOX_IP" ;;
    esac
  done

  if [ -n "$PROXY_IP" ] && [ -n "$HOST_IP" ] && [ "$READY" -eq 1 ]; then
    SANDBOX_IPS="$RESOLVED_SANDBOX_IPS"
    break
  fi

  echo "Waiting for proxy, host gateway, and enabled sandbox endpoints..."
  sleep 2
done

if [ -n "$SANDBOX_IPS" ]; then
  echo "Network sandbox endpoints: ${SANDBOX_IPS# }"
else
  echo "Network sandbox disabled"
fi
echo "Proxy ($CLADDING_PROXY_NAME) detected at:             $PROXY_IP"
echo "Host detected at:                                      $HOST_IP"

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
for SANDBOX_IP in $SANDBOX_IPS; do
  nft add rule ip filter OUTPUT ip daddr "$SANDBOX_IP" tcp dport 3000 accept
done

# D. Allow Outbound to Host (Direct Access)
# Allow host gateway access; use allowlist if present.
HOST_PORTS_FILE="/opt/config/agent/host_ports.lst"
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
# The agent sends allowed internet traffic through the proxy.
nft add rule ip filter OUTPUT ip daddr $PROXY_IP tcp dport 8080 accept

# F. Drop Everything Else
# If it's not the network sandbox or proxy, it's blocked.
nft add rule ip filter OUTPUT log prefix \"BLOCKED_AGENT: \" drop
nft add rule ip filter OUTPUT drop

if [ "${JAILER_HOLD:-0}" = "1" ]; then
  echo "Agent firewall locked. Traffic restricted to network sandbox and proxy. Sleeping infinity..."
  exec sleep infinity
fi

echo "Agent firewall locked. Traffic restricted to network sandbox and proxy. Exiting."
