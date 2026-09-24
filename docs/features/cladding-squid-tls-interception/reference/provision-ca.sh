#!/bin/sh
set -eu
umask 077

if [ "$#" -ne 2 ]; then
    printf '%s\n' "usage: $0 PRIVATE_CA_DIR PUBLIC_CA_DIR" >&2
    exit 2
fi

private_dir=$1
public_dir=$2
key="$private_dir/interception-ca.key"
cert="$private_dir/interception-ca.crt"

fail() {
    printf '%s\n' "CA provisioning failed: $1" >&2
    exit 1
}

mkdir -p "$private_dir" "$public_dir"
chmod 0700 "$private_dir"

if [ -e "$key" ] || [ -e "$cert" ]; then
    [ -s "$key" ] && [ -s "$cert" ] \
        || fail "CA key and certificate must both exist or both be absent"
    openssl pkey -in "$key" -noout >/dev/null 2>&1 \
        || fail "existing CA private key is invalid"
    openssl x509 -in "$cert" -noout >/dev/null 2>&1 \
        || fail "existing CA certificate is invalid"
    openssl x509 -in "$cert" -checkend 0 -noout >/dev/null 2>&1 \
        || fail "existing CA certificate is expired"
    openssl x509 -in "$cert" -noout -ext basicConstraints 2>/dev/null | grep -Fq 'CA:TRUE' \
        || fail "existing CA certificate is not a CA"
    openssl x509 -in "$cert" -noout -ext keyUsage 2>/dev/null | grep -Fq 'Certificate Sign' \
        || fail "existing CA certificate cannot sign certificates"
    key_hash=$(openssl pkey -in "$key" -pubout -outform DER 2>/dev/null | sha256sum | awk '{print $1}')
    cert_hash=$(openssl x509 -in "$cert" -pubkey -noout 2>/dev/null | openssl pkey -pubin -outform DER 2>/dev/null | sha256sum | awk '{print $1}')
    [ "$key_hash" = "$cert_hash" ] || fail "existing CA key does not match its certificate"
else
    temporary=$(mktemp -d "$private_dir/.ca.XXXXXX")
    trap 'rm -rf "$temporary"' EXIT HUP INT TERM
    openssl genpkey -algorithm RSA -pkeyopt rsa_keygen_bits:3072 \
        -out "$temporary/interception-ca.key" >/dev/null 2>&1 \
        || fail "private key generation failed"
    openssl req -x509 -new -sha256 -days 3650 \
        -key "$temporary/interception-ca.key" \
        -out "$temporary/interception-ca.crt" \
        -subj "/CN=Cladding TLS Interception CA" \
        -addext "basicConstraints=critical,CA:TRUE,pathlen:0" \
        -addext "keyUsage=critical,keyCertSign,cRLSign" >/dev/null 2>&1 \
        || fail "CA certificate generation failed"
    chmod 0600 "$temporary/interception-ca.key"
    chmod 0644 "$temporary/interception-ca.crt"
    mv "$temporary/interception-ca.key" "$key"
    mv "$temporary/interception-ca.crt" "$cert"
    rmdir "$temporary"
    trap - EXIT HUP INT TERM
fi

chmod 0600 "$key"
chmod 0644 "$cert"
public_temporary="$public_dir/.interception-ca.crt.tmp"
cp "$cert" "$public_temporary"
chmod 0644 "$public_temporary"
mv "$public_temporary" "$public_dir/interception-ca.crt"
