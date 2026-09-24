#!/bin/sh
set -eu

feature_dir=$(CDPATH= cd -- "$(dirname -- "$0")/.." && pwd)
reference_dir="$feature_dir/reference"
test_name="cladding-proxy-test-$$"
network="$test_name-net"
proxy_container="$test_name-proxy"
origin_container="$test_name-origin"
client_container="$test_name-client"
proxy_image="$test_name-squid"
test_image="$test_name-tools"
temporary=$(mktemp -d "${TMPDIR:-/tmp}/cladding-proxy-test.XXXXXX")

cleanup() {
    docker rm -f "$client_container" "$proxy_container" "$origin_container" >/dev/null 2>&1 || true
    docker network rm "$network" >/dev/null 2>&1 || true
    docker image rm "$proxy_image" "$test_image" >/dev/null 2>&1 || true
    rm -rf "$temporary"
}
trap cleanup EXIT HUP INT TERM

command -v docker >/dev/null 2>&1 || {
    printf '%s\n' "docker is required to run the Squid integration test" >&2
    exit 2
}
docker info >/dev/null

mkdir -p "$temporary/certs" "$temporary/client-certs" "$temporary/config/agent" \
    "$temporary/config/nw_sandbox" "$temporary/scripts/proxy" \
    "$temporary/credentials/rules" "$temporary/credentials/ca"
chmod 0700 "$temporary/credentials"

# A separate test CA lets Squid verify its controlled upstream origin. The
# client trust bundle contains this CA and the public interception CA only.
openssl req -x509 -newkey rsa:2048 -nodes -sha256 -days 2 \
    -keyout "$temporary/certs/origin-ca.key" \
    -out "$temporary/certs/origin-ca.crt" \
    -subj "/CN=Cladding controlled origin CA" \
    -addext "basicConstraints=critical,CA:TRUE,pathlen:0" \
    -addext "keyUsage=critical,keyCertSign,cRLSign" >/dev/null 2>&1
openssl req -newkey rsa:2048 -nodes \
    -keyout "$temporary/certs/origin.key" \
    -out "$temporary/certs/origin.csr" \
    -subj "/CN=github.com" >/dev/null 2>&1
cat > "$temporary/certs/origin.ext" <<'EOF'
subjectAltName=DNS:github.com,DNS:api.github.com,DNS:allowed.test,DNS:evil.test,DNS:denied.test
extendedKeyUsage=serverAuth
EOF
openssl x509 -req -sha256 -days 2 \
    -in "$temporary/certs/origin.csr" \
    -CA "$temporary/certs/origin-ca.crt" \
    -CAkey "$temporary/certs/origin-ca.key" \
    -CAcreateserial \
    -extfile "$temporary/certs/origin.ext" \
    -out "$temporary/certs/origin.crt" >/dev/null 2>&1
openssl req -x509 -newkey rsa:2048 -nodes -sha256 -days 2 \
    -keyout "$temporary/certs/invalid.key" \
    -out "$temporary/certs/invalid.crt" \
    -subj "/CN=api.github.com" \
    -addext "subjectAltName=DNS:github.com,DNS:api.github.com,DNS:allowed.test,DNS:evil.test" \
    -addext "extendedKeyUsage=serverAuth" >/dev/null 2>&1
openssl req -newkey rsa:2048 -nodes \
    -keyout "$temporary/certs/wrong-host.key" \
    -out "$temporary/certs/wrong-host.csr" \
    -subj "/CN=wrong.invalid" >/dev/null 2>&1
cat > "$temporary/certs/wrong-host.ext" <<'EOF'
subjectAltName=DNS:wrong.invalid
extendedKeyUsage=serverAuth
EOF
openssl x509 -req -sha256 -days 2 \
    -in "$temporary/certs/wrong-host.csr" \
    -CA "$temporary/certs/origin-ca.crt" \
    -CAkey "$temporary/certs/origin-ca.key" \
    -CAcreateserial \
    -extfile "$temporary/certs/wrong-host.ext" \
    -out "$temporary/certs/wrong-host.crt" >/dev/null 2>&1

cp "$reference_dir/squid.conf" "$temporary/scripts/proxy/squid.conf"
cp "$reference_dir/credentials/domains.lst" "$temporary/credentials/domains.lst"
cp "$reference_dir/credentials/rules/github-api.conf" "$temporary/credentials/rules/github-api.conf"
cp "$reference_dir/credentials/rules/github-git.conf" "$temporary/credentials/rules/github-git.conf"
cat > "$temporary/config/agent/domains.lst" <<'EOF'
github.com
api.github.com
allowed.test
evil.test
EOF
cat > "$temporary/config/agent/host_ports.lst" <<'EOF'
443
EOF
cat > "$temporary/config/nw_sandbox/domains.lst" <<'EOF'
github.com
api.github.com
allowed.test
evil.test
EOF
printf '%s' 'TEST_API_TOKEN_123' > "$temporary/credentials/github-api-token"
printf '%s' 'TEST_GIT_TOKEN_456' > "$temporary/credentials/github-git-token"
chmod 0600 "$temporary/credentials/github-api-token" "$temporary/credentials/github-git-token"

"$reference_dir/provision-ca.sh" "$temporary/credentials/ca" "$temporary/public-ca"
first_ca_fingerprint=$(openssl x509 -in "$temporary/credentials/ca/interception-ca.crt" -noout -fingerprint -sha256)
"$reference_dir/provision-ca.sh" "$temporary/credentials/ca" "$temporary/public-ca"
second_ca_fingerprint=$(openssl x509 -in "$temporary/credentials/ca/interception-ca.crt" -noout -fingerprint -sha256)
[ "$first_ca_fingerprint" = "$second_ca_fingerprint" ] || {
    printf '%s\n' "persistent CA was not reused" >&2
    exit 1
}
cat "$temporary/public-ca/interception-ca.crt" "$temporary/certs/origin-ca.crt" \
    > "$temporary/client-certs/client-trust-bundle.crt"
cp "$temporary/public-ca/interception-ca.crt" "$temporary/client-certs/interception-ca.crt"
cp "$temporary/certs/origin-ca.crt" "$temporary/client-certs/origin-ca.crt"

docker build --pull -t "$proxy_image" -f reference/Containerfile "$feature_dir"
docker build --pull -t "$test_image" -f reference/Containerfile.test "$feature_dir"
docker network create "$network" >/dev/null

docker run -d --name "$origin_container" --network "$network" \
    --network-alias origin \
    --network-alias github.com \
    --network-alias api.github.com \
    --network-alias allowed.test \
    --network-alias evil.test \
    --network-alias denied.test \
    -v "$temporary/certs:/certs:ro" \
    "$test_image" python3 /opt/cladding-proxy-test/origin.py >/dev/null

docker run -d --name "$proxy_container" --network "$network" \
    -e CLADDING_PROXY_NAME=cladding-test \
    -e CLADDING_SANDBOX_NAME=nw-sandbox \
    -v "$temporary/config:/opt/config:ro" \
    -v "$temporary/scripts:/opt/scripts:ro" \
    -v "$temporary/credentials:/opt/credentials:ro" \
    -v "$temporary/certs/origin-ca.crt:/usr/local/share/ca-certificates/proxy-test-origin.crt:ro" \
    --tmpfs /run/squid-private:rw,noexec,nosuid,nodev,size=4m \
    "$proxy_image" >/dev/null

docker run -d --name "$client_container" --network "container:$proxy_container" \
    -v "$temporary/client-certs:/test-certs:ro" \
    "$test_image" sleep infinity >/dev/null

proxy_ready=false
attempt=0
while [ "$attempt" -lt 30 ]; do
    if docker exec "$client_container" python3 -c \
        'import socket; s=socket.create_connection(("127.0.0.1",3128),1); s.close()' \
        >/dev/null 2>&1; then
        proxy_ready=true
        break
    fi
    attempt=$((attempt + 1))
    sleep 1
done
[ "$proxy_ready" = true ] || {
    printf '%s\n' "Squid did not start; container output follows" >&2
    docker logs "$proxy_container" >&2 || true
    exit 1
}

docker exec "$client_container" python3 /opt/cladding-proxy-test/test_proxy.py

# Missing credentials and malformed private rules must stop startup. Keep the
# check output in the temporary directory so it can be scanned for secret data.
mkdir -p "$temporary/missing-credentials"
cp -a "$temporary/credentials/." "$temporary/missing-credentials/"
rm "$temporary/missing-credentials/github-api-token"
if docker run --rm --network "$network" \
    -e CLADDING_PROXY_NAME=cladding-missing-test \
    -e CLADDING_SANDBOX_NAME=nw-sandbox \
    -v "$temporary/config:/opt/config:ro" \
    -v "$temporary/scripts:/opt/scripts:ro" \
    -v "$temporary/missing-credentials:/opt/credentials:ro" \
    --tmpfs /run/squid-private:rw,noexec,nosuid,nodev,size=4m \
    "$proxy_image" >"$temporary/missing.out" 2>&1; then
    printf '%s\n' "proxy started without the required API credential" >&2
    exit 1
fi

mkdir -p "$temporary/malformed-credentials"
cp -a "$temporary/credentials/." "$temporary/malformed-credentials/"
printf '%s\n' 'this_is_not_a_squid_directive' >> "$temporary/malformed-credentials/rules/github-api.conf"
if docker run --rm --network "$network" \
    -e CLADDING_PROXY_NAME=cladding-malformed-test \
    -e CLADDING_SANDBOX_NAME=nw-sandbox \
    -v "$temporary/config:/opt/config:ro" \
    -v "$temporary/scripts:/opt/scripts:ro" \
    -v "$temporary/malformed-credentials:/opt/credentials:ro" \
    --tmpfs /run/squid-private:rw,noexec,nosuid,nodev,size=4m \
    "$proxy_image" >"$temporary/malformed.out" 2>&1; then
    printf '%s\n' "proxy started with a malformed private rule" >&2
    exit 1
fi

for secret in TEST_API_TOKEN_123 TEST_GIT_TOKEN_456; do
    if docker logs "$proxy_container" 2>&1 | grep -Fq "$secret" \
        || grep -Fq "$secret" "$temporary/scripts/proxy/squid.conf" \
        || grep -Fq "$secret" "$temporary/missing.out" \
        || grep -Fq "$secret" "$temporary/malformed.out"; then
        printf '%s\n' "credential appeared in logs, public config, or error output" >&2
        exit 1
    fi
done

docker exec "$client_container" test ! -e /opt/credentials
docker exec "$client_container" test ! -e /run/squid-private/ca.key

printf '%s\n' "Squid 7 TLS interception reference tests passed."
