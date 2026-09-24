#!/bin/sh
set -eu
umask 077

source=/opt/credentials
runtime=/run/squid-private
rules="$runtime/rules"

fail() {
    printf '%s\n' "credential setup failed: $1" >&2
    exit 1
}

require_file() {
    [ -s "$1" ] || fail "missing or empty private file: $1"
}

valid_secret() {
    case "$1" in
        ''|*[!A-Za-z0-9_]*) return 1 ;;
        *) return 0 ;;
    esac
}

require_file "$source/github-api-token"
require_file "$source/github-git-token"
require_file "$source/ca/interception-ca.crt"
require_file "$source/ca/interception-ca.key"
require_file /opt/config/agent/domains.lst
require_file "$source/domains.lst"
require_file "$source/rules/github-api.conf"
require_file "$source/rules/github-git.conf"

api_token=$(cat "$source/github-api-token") || fail "cannot read GitHub API token"
git_token=$(cat "$source/github-git-token") || fail "cannot read GitHub Git token"
valid_secret "$api_token" || fail "GitHub API token has unsupported characters"
valid_secret "$git_token" || fail "GitHub Git token has unsupported characters"

# Use exact, lowercase host names. Reject wildcard and suffix patterns.
if ! awk '
    NF != 1 { exit 1 }
    $0 !~ /^[a-z0-9]([a-z0-9-]*[a-z0-9])?([.][a-z0-9]([a-z0-9-]*[a-z0-9])?)*$/ { exit 1 }
    END { if (NR == 0) exit 1 }
' "$source/domains.lst"; then
    fail "interception domains must be exact lowercase host names"
fi

while IFS= read -r domain; do
    grep -Fqx "$domain" /opt/config/agent/domains.lst \
        || fail "interception domain is missing from the agent allowlist"
done < "$source/domains.lst"

[ -d "$runtime" ] || fail "private runtime directory must be a memory-backed mount"
install -d -o proxy -g proxy -m 0700 "$rules"
install -o proxy -g proxy -m 0400 "$source/domains.lst" "$runtime/domains.lst"
install -o proxy -g proxy -m 0400 "$source/ca/interception-ca.crt" "$runtime/ca.crt"
install -o proxy -g proxy -m 0400 "$source/ca/interception-ca.key" "$runtime/ca.key"

git_basic=$(printf 'x-access-token:%s' "$git_token" | base64 | tr -d '\n')
for rule in "$source"/rules/*.conf; do
    [ -f "$rule" ] || fail "no private Squid rules were provided"
    name=${rule##*/}
    case "$name" in
        github-api.conf)
            sed "s|@CLADDING_GITHUB_API_TOKEN@|$api_token|g" "$rule" > "$rules/$name"
            ;;
        github-git.conf)
            sed "s|@CLADDING_GITHUB_GIT_BASIC@|$git_basic|g" "$rule" > "$rules/$name"
            ;;
        *)
            # A service-specific private .conf file needs no Rust change or
            # renderer update. Its author owns safe Squid quoting and ACLs.
            install -o proxy -g proxy -m 0400 "$rule" "$rules/$name"
            ;;
    esac
done

chown proxy:proxy "$rules/github-api.conf" "$rules/github-git.conf"
chmod 0400 "$rules/github-api.conf" "$rules/github-git.conf"

# Fail closed if a placeholder was not replaced or if unexpected template text
# could add another Squid directive.
if grep -Fq '@CLADDING_' "$rules"/*.conf; then
    fail "credential template contains an unresolved placeholder"
fi
