# Proposal: Selective Squid TLS interception and credential injection

## Motivation

CLAD-04 needs a Squid configuration that can add delegated credentials for GitHub API and Git-over-HTTPS requests without changing the existing proxy layout. The agent and network sandbox already use separate loopback listeners in one proxy pod. The proxy already enforces listener identity, destination allowlists, port restrictions, and default deny.

The current default is `docker.io/ubuntu/squid:latest` in `src/config/types.rs`. It is not a stable image pin. The current template is [scripts/proxy/squid.conf](../../../scripts/proxy/squid.conf). It has no TLS interception. The runtime startup copies that template to `/tmp/squid_generated.conf` and starts Squid.

This spike adds a runnable reference under `docs/features/cladding-squid-tls-interception/reference/`. It keeps the existing listeners and access policy. CLAD-04 can integrate this reference into the built-in proxy image and runtime mounts.

## Problem statement

An SSL bump decision made only from the ClientHello SNI is not enough to protect delegated credentials. The client controls SNI. The client can also make the CONNECT authority, SNI, and decrypted HTTP `Host` disagree. A private Bearer or Basic header must reach only the exact configured service when all relevant names agree.

Squid 7's `request_header_add` appends a field. It does not replace an existing `Authorization` field. Squid must first remove every incoming Authorization field on a credential-eligible request, then add exactly one delegated value.

## Proposal

Keep the two named listeners in the existing proxy pod:

| Listener | TLS behavior |
| --- | --- |
| `127.0.0.1:3128`, `name=agent` | Peek at ClientHello. Bump only when the exact CONNECT host and client-requested SNI both match a private interception domain. Splice all other HTTPS. |
| `127.0.0.1:3129`, `name=nw_sandbox` | Keep the current ordinary CONNECT tunnel. Do not enable SSL Bump on this port. |

The reference [Squid configuration](reference/squid.conf) extends the current ACL and access-rule shape. It retains `from_agent`, `from_nw_sandbox`, `agent_domains`, `agent_host`, `agent_host_ports`, safe-port checks, and the final `http_access deny all` rule. Interception does not add a destination to `agent_domains` or replace that policy.

### TLS bump selection

Use exact lowercase host names in `credentials/domains.lst`, without a leading dot or wildcard. The initial entries are `github.com` and `api.github.com`. The renderer requires each entry to appear as an exact line in `/opt/config/agent/domains.lst`.

The staged bump rules are:

```squid
acl intercept_connect_hosts dstdomain -n "/run/squid-private/domains.lst"
acl intercept_client_sni ssl::server_name --client-requested \
    "/run/squid-private/domains.lst"
acl bump_step1 at_step SslBump1

ssl_bump peek bump_step1
ssl_bump bump from_agent intercept_connect_hosts intercept_client_sni
ssl_bump splice all
```

The `--client-requested` option matters. Without it, Squid's default `ssl::server_name` ACL can match any available name from the CONNECT URI, client SNI, or upstream certificate. The bump rule must test the CONNECT host and client-requested SNI as separate ACLs. If SNI is absent, Squid falls back to the CONNECT target; the decrypted request still must pass the exact URL and Host ACLs below.

The Squid 7.2 parser supports wildcard `include` directives. The reference includes `/run/squid-private/rules/*.conf`. Its rule files have stable names and do not depend on evaluation order. The integration test exercises both files through the wildcard include.

### Credential rules

The private API and Git rules are [github-api.conf](reference/credentials/rules/github-api.conf) and [github-git.conf](reference/credentials/rules/github-git.conf). They require all of these fast ACLs to match:

- the `agent` listener;
- an HTTPS request;
- the exact destination URL host;
- an exact, anchored HTTP `Host` value;
- for Git, a Git smart-HTTP path under `*.git`.

Each rule applies `request_header_access Authorization deny ...` before `request_header_add Authorization ...`. The ACLs require both a `proto HTTPS` effective URL and Squid's `connections_encrypted` check, which confirms TLS on the client and upstream connections. Squid 7.2 reconstructs origin-form requests on a bumped TLS connection as `https` URLs using the decrypted Host value. Its `HttpHeaderTools` implementation removes denied fields before it adds configured fields. The API test sends two client Authorization fields and checks that the origin receives exactly one delegated Bearer value. The Git test also supplies a client Authorization field and checks for exactly one delegated Basic value.

The host-side `.cladding/credentials` layout is:

```text
.cladding/credentials/
  domains.lst
  github-api-token
  github-git-token
  ca/
    interception-ca.crt
    interception-ca.key
  rules/
    github-api.conf
    github-git.conf
```

The checked-in rule examples contain markers only. The startup renderer reads the API token and Git token from private input files. It builds Git Basic authentication from `x-access-token:<token>`. It writes expanded GitHub rules and copies every other private `.conf` rule verbatim into `/run/squid-private`, which CLAD-04 must mount as a small Squid-only tmpfs. A new service therefore needs an exact interception-domain entry, its own private rule file, and exact agent allowlist lines. It needs no Rust change. It does not write credentials into `/opt/config` or the image. Token characters outside the supported GitHub token alphabet fail startup.

Missing credentials, invalid exact-domain entries, missing agent allowlist entries, unresolved placeholders, or invalid Squid rules stop startup. `squid -k parse` runs before the listener starts. Parse output is kept in private tmpfs and is not copied to public logs.

### CA and trust

Run [provision-ca.sh](reference/provision-ca.sh) before starting the proxy:

```bash
docs/features/cladding-squid-tls-interception/reference/provision-ca.sh \
  .cladding/credentials/ca .cladding/runtime/proxy-ca
```

The script creates a dedicated RSA CA if both private files are absent. It reuses a valid matching pair. A partial, invalid, or mismatched pair fails instead of silently rotating the CA. The private key has mode `0600`; the public certificate has mode `0644`. For one-off jobs, use a job-specific private and public directory. For persistent project runs, keep the CA pair in the project credential directory so clients retain trust.

Only the Squid container receives `.cladding/credentials`. The proxy startup copies the key and certificate to its private tmpfs, changes ownership to the Squid runtime user, initializes the dynamic certificate database as that user, parses the configuration, then starts Squid. The signing key is not mounted in the agent or network-sandbox containers.

CLAD-04 must mount the public CA certificate as a separate read-only file into the agent and network sandbox. Both images must add it to the OS trust store before use. The reference test uses `curl --cacert` and Git's `GIT_SSL_CAINFO` to prove certificate validation. Applications with their own stores need their normal trust configuration, such as `NODE_EXTRA_CA_CERTS` or a Java truststore import. The network-sandbox listener stays spliced; it receives no delegated credentials.

The helper path is `/usr/local/libexec/security_file_certgen`. The startup script creates `/run/squid/ssl_db` and initializes it as UID 13 (`proxy`) with `security_file_certgen -c -s /run/squid/ssl_db -M 4MB` before Squid starts. The database size matches the `sslcrtd_program` configuration. The database and rendered secrets use container runtime storage. CLAD-04 must verify the same ownership behavior under rootless Podman.

### Pinned Squid image

[reference/Containerfile](reference/Containerfile) builds Squid 7.2 from the upstream `SQUID_7_2` release. It pins the Ubuntu 26.04 multi-architecture base digest to `sha256:cd21a4f68a617580279d4b091cb18e3af9fa8a87500665f0ae5f7f757d17d367` and verifies the Squid source archive SHA-256 `5e077be1d83a9e696ce8d0d9e723b1273152207a091404be68a4b9a9e18c7003`.

The configure options are `--with-openssl`, `--enable-ssl-crtd`, and `--enable-http-violations`. The image build checks the reported compile options, Squid version, and helper path. The startup runs `squid -k parse` against the generated reference config. The `request_header_access` directive is conditional on `--enable-http-violations` in Squid 7.

This build avoids relying on a floating Squid tag or on an image compiled with GnuTLS. Squid's own configuration reference marks `request_header_access` and `sslcrtd_program` as Squid 7 directives; `request_header_access` requires `--enable-http-violations`, and `sslcrtd_program` requires `--enable-ssl-crtd`.

### Security boundary and redirects

The reference has no `sslproxy_cert_error allow` rule and does not set `DONT_VERIFY_PEER` or `DONT_VERIFY_DOMAIN`. Squid uses the system CA store and validates the upstream certificate and host name. The tests prove rejection for an untrusted upstream certificate and for a trusted certificate with the wrong host name.

Credential injection is tied to the decrypted request's effective URL host and exact Host header. A CONNECT/SNI mismatch is spliced because both names must match before bumping. A bumped request whose Host differs does not satisfy the credential rule. Redirects create new requests; an allowed redirect target outside the exact private domains receives no delegated credential. The tests check both CONNECT/SNI and decrypted Host disagreement against a controlled origin.

The normal access and cache logs do not include Authorization. The renderer does not print token contents. Parser output is suppressed into private tmpfs, and the integration test scans logs, public config, and startup errors for its synthetic credentials.

## Non-goals

- Do not replace the socket-bridge sidecar or add a proxy, broker, or production network.
- Do not change the network-sandbox filtering policy or enable SSL Bump on its listener.
- Do not add wildcard credential domains or inject credentials into HTTP requests.
- Do not commit credential-bearing rule files, CA keys, or generated certificates.
- Do not modify Rust runtime integration in CLAD-03. CLAD-04 must add private credential mounts, public CA mounts, tmpfs, and pre-start CA provisioning.

## Verification

Run the reference integration test with Docker:

```bash
docs/features/cladding-squid-tls-interception/reference/test.sh
```

The test builds the pinned image, runs `squid -k parse`, and uses a local HTTPS origin with synthetic credentials. It checks agent API and Git credential replacement, an unrelated spliced host, denied domains, the network-sandbox tunnel, HTTP, duplicate client Authorization, CONNECT/SNI and Host mismatches, redirects, invalid upstream certificates, missing or malformed credentials, CA reuse, and log redaction. No real credential is required. The integration test runs in [proxy-tls-interception.yml](../../../.github/workflows/proxy-tls-interception.yml).

This runner has no Docker, Podman, Squid, or Buildah executable, and its network proxy denies Docker Hub registry access. The local checks for this change are shell syntax, Python syntax, CA generation/reuse, and the repository's Rust CI commands. The image build and local-origin integration test are reproducible in the PR workflow; rootless Podman ownership remains a CLAD-04 integration check.

## Success criteria

- The reference image reports Squid 7.2, OpenSSL, SSL certificate generation, and HTTP violations support.
- The 3128 listener bumps only when exact CONNECT and client-requested SNI ACLs match.
- The 3129 listener remains an ordinary tunnel.
- Each delegated credential replaces all client Authorization fields only for its exact HTTPS destination, TLS-protected upstream connection, and Git path.
- Upstream certificate, host name, and redirect checks prevent delegated-credential leakage.
- Private keys and generated rules remain in proxy-only storage; public CA trust is distributed separately.
- The reference test passes without real credentials and CLAD-04 has a specific runtime integration list.

## References

- [Squid 7 `ssl::server_name` ACL and staged bump steps](https://www.squid-cache.org/Doc/config/acl/)
- [Squid peek and splice behavior](https://wiki.squid-cache.org/Features/SslPeekAndSplice)
- [Squid 7 `request_header_access`](https://www.squid-cache.org/Doc/config/request_header_access/)
- [Squid 7 `request_header_add`](https://www.squid-cache.org/Doc/config/request_header_add/)
- [Squid 7 dynamic certificate helper](https://www.squid-cache.org/Doc/config/sslcrtd_program/)
- [Squid 7 outgoing TLS verification settings](https://www.squid-cache.org/Doc/config/tls_outgoing_options/)
- [Squid 7.2 source release](https://github.com/squid-cache/squid/releases/tag/SQUID_7_2)
- [Squid Rock 7.2 build definition](https://github.com/canonical/squid-rock/blob/main/squid/7.2-26.04/rockcraft.yaml), which identifies its build as the GnuTLS variant.
