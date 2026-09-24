#!/bin/sh
set -eu

fail() {
    printf '%s\n' "proxy startup failed: $1" >&2
    exit 1
}

: "${CLADDING_PROXY_NAME:?Missing CLADDING_PROXY_NAME}"
[ -d /run/squid-private ] || fail "/run/squid-private must be mounted as private tmpfs"
[ -f /opt/scripts/proxy/squid.conf ] || fail "missing public Squid config"
[ -f /opt/config/agent/domains.lst ] || fail "missing agent domain allowlist"
[ -f /opt/config/agent/host_ports.lst ] || fail "missing agent host-port allowlist"

if grep -Eq '/tmp/(agent|nw_sandbox)_ips\.lst|acl (agent|nw_sandbox)_src src|^http_port 8080([[:space:]]|$)' /opt/scripts/proxy/squid.conf; then
    fail "Squid config uses the old source-IP proxy identity model"
fi

/usr/local/bin/cladding-proxy-render-credentials

if [ -f /usr/local/share/ca-certificates/proxy-test-origin.crt ]; then
    update-ca-certificates >/dev/null 2>&1 \
        || fail "test origin CA trust setup failed"
fi

dns_ip=$(awk '$1 == "nameserver" && $2 ~ /^[0-9]+\./ { print $2; exit }' /etc/resolv.conf)
[ -n "$dns_ip" ] || dns_ip=10.89.0.1

runtime_domains=/opt/config/nw_sandbox/domains.lst
if [ -n "${CLADDING_SANDBOX_NAME:-}" ]; then
    [ -f "$runtime_domains" ] || fail "missing network-sandbox domain allowlist"
else
    runtime_domains=/run/squid-private/empty-nw-domains.lst
    : > "$runtime_domains"
fi

generated=/tmp/squid_generated.conf
sed \
    -e "s/REPLACE_DNS_IP/$dns_ip/g" \
    -e "s/REPLACE_PROXY_NAME/$CLADDING_PROXY_NAME/g" \
    -e "s|REPLACE_NW_SANDBOX_DOMAINS_FILE|$runtime_domains|g" \
    /opt/scripts/proxy/squid.conf > "$generated"
chmod 0644 "$generated"

install -d -o proxy -g proxy -m 0700 /run/squid/ssl_db
if [ ! -f /run/squid/ssl_db/index.txt ]; then
    runuser -u proxy -- /usr/local/libexec/security_file_certgen \
        -c -s /run/squid/ssl_db -M 4MB >/dev/null 2>&1 \
        || fail "Squid certificate database initialization failed"
fi

if ! /usr/local/sbin/squid -k parse -f "$generated" \
    >/run/squid-private/parse.log 2>&1; then
    fail "Squid configuration validation failed; parser output is private"
fi
rm -f /run/squid-private/parse.log

exec /usr/local/sbin/squid -N -f "$generated"
