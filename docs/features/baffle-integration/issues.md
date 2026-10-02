# Baffle integration — implementation issue descriptions

Ready-to-file descriptions for the issue graph in [the Baffle integration proposal](./prd.md). Each section below is a separate issue for `dstoc/cladding`, identified by `baffle-N`. Copy its heading into the issue title and the remaining section into its body. The `depends-on:` line must remain at the end of each issue description. These IDs are planning identifiers, not GitHub issue numbers. `baffle-12` is deferred beyond phase one.

# Historical implementation issue breakdown

This file records the requirements used to implement the Baffle runtime. Its
Squid descriptions refer to the replaced pre-Baffle system. For current
behavior, see `docs/features/current-runtime-summary.md`,
`docs/features/proxy/summary.md`, and `README.md`.

## baffle-1 — Replace Squid configuration with native Baffle TOML

**Context:** [Baffle integration proposal](./prd.md), especially *User-editable configuration*.

### Objective

Replace Cladding's Squid proxy configuration, per-component domain lists and host-port lists with native, user-editable Baffle TOML. There is no requirement to translate or support legacy Squid configuration.

### Implementation

- Materialize these templates through `cladding init`, preserving user edits on subsequent initializations:
  - `.cladding/config/proxy/daemon.toml`
  - `.cladding/config/proxy/sessions/agent.toml`
  - `.cladding/config/proxy/sessions/nw-sandbox.toml`
- Set `daemon.create_mode = "file_only"` and `daemon.session_config_dir` to the mounted session directory. Reference fixed in-container paths for the control socket, scoped data-socket root, CA, private key and secret directory. Expose native `[secrets].allowed` without introducing a Cladding-specific proxy policy language.
- Set both session templates to `persistent = true` and use stable named data sockets: `agent/proxy.sock` and `nw-sandbox/proxy.sock`. Include a conservative, explicit example HTTPS host rule; Baffle rejects active sessions without rules. The network-sandbox template may exist when that component is disabled, but must not be instantiated.
- Ensure ownership, directory/file modes and symlink handling satisfy Baffle's file-only configuration validation inside the proxy container, including supported rootless UID mappings.
- Remove Squid configuration and template generation, domain/host-port-list validation and obsolete proxy-specific wiring. Do **not** remove unrelated agent, network-sandbox or filesystem-sandbox Rego and command-policy configuration.
- Keep the existing read-only `config/` mounts into agent and network sandbox. Their ability to read non-secret Baffle TOML is intentional; configuration must contain no literal secrets, private keys or usable control-socket capabilities.

### Acceptance criteria

- `cladding init` creates valid Baffle configuration without requiring Squid artifacts and does not overwrite existing user-edited TOML.
- Both session templates parse under the pinned Baffle version; permissions pass file-only checks in the supported container setup.
- The legacy Squid lists and policy-translation paths are no longer required.
- A disabled network sandbox does not cause its session to be created.

depends-on: none

---

## baffle-2 — Provision project CA and credential storage

**Context:** [Baffle integration proposal](./prd.md), especially *Project CA and injection credentials*.

### Objective

Give each Cladding project one durable Baffle interception CA and a private location for user-provisioned injection credentials, separate from editable policy TOML and disposable runtime state.

### Implementation

- Use `.cladding/credentials/baffle/ca.crt`, `ca-key.pem` and `secrets/<secret-name>`. Create the parent and secrets directories with mode `0700`; use `0600` for private keys and secret files, and `0644` for the public certificate.
- Generate a Baffle-compatible certificate/key pair **before** container creation so the certificate exists for bind mounts. Include CA basic constraints, certificate-signing key usage, appropriate validity and a matching signing key. Prefer a host-portable in-process implementation over an external host service.
- Write newly generated material atomically; reuse a valid existing CA. Report incomplete, invalid or expired material instead of silently replacing it. Document explicit rotation rather than performing implicit rotation.
- Create an empty `secrets/` directory but do not generate, copy, rewrite, log or place secret values into TOML, environment variables or images. Users or external secret managers create individual files; Baffle authorizes their symbolic identifiers through `[secrets].allowed`.
- Arrange a read-only private credentials mount for the proxy container with suitable Baffle trusted-UID ownership. Prepare a **separate public-certificate-only** mount for the agent and network sandbox. Neither execution container may receive the CA private key or secret files.
- Preserve the project's persistent CA and injection secrets across `down` and `destroy`. For `cladding run`, use isolated one-off CA and runtime paths, and remove ephemeral credentials during normal one-off cleanup. If using an explicitly selected project's existing secrets, reference them read-only without copying values and document the deliberate selection.
- Keep credentials out of source control and out of the agent's accessible workspace. A newly initialized `.cladding` already has an internal ignore file; document precautions for other project layouts.

### Acceptance criteria

- Restarting a persistent project reuses the same valid CA; malformed material produces a clear failure and is not overwritten.
- Proxy-side Baffle can read the CA and explicitly permitted secret files, while execution containers can access only the public certificate.
- `run` creates and cleans up isolated ephemeral CA material.
- Permission, validity, missing-secret and ownership cases have automated coverage.

depends-on: none

---

## baffle-3 — Build and embed Baffle from crates.io

**Context:** [Baffle integration proposal](./prd.md), especially *Binary packaging and proxy image*.

### Objective

Package a pinned Linux `baffle` executable with Cladding, just as Cladding currently embeds `mcp-run` and `run-remote`, without depending on GitHub binary release downloads.

### Implementation

- Pin an actually published `baffle-proxy` crate version, build/install its `baffle` binary with Cargo `--locked` and embed the result through Cladding's `build.rs` and `src/assets.rs` mechanism.
- Extract the executable to `.cladding/tools/bin/baffle` during `cladding build`, set its executable mode, and include it in Cladding's required-tool and outdated-embedded-tool checks.
- Provide `CLADDING_BAFFLE_BIN` to supply an externally built compatible Linux binary, particularly when packaging macOS host executables. Avoid building Baffle repeatedly in each release job when a validated build artifact can be reused.
- Account for the Baffle crate's minimum Rust version and native build dependencies (including CMake/libclang) in the **build** environment, not the runtime image.
- Explicitly validate the chosen Linux architecture and GNU-libc/runtime dependencies against supported Cladding execution environments. Diagnose unsupported target combinations instead of silently embedding a host-incompatible binary.
- Keep the version reproducible; document the update procedure and any third-party license/notice obligations for redistributed binaries.

### Acceptance criteria

- A Cladding build embeds the pinned Baffle executable; `cladding build` materializes and validates it.
- A macOS Cladding release can embed a prebuilt compatible Linux Baffle binary via the override.
- A test invokes the extracted executable in the selected proxy runtime image.
- No GitHub release download is part of normal Baffle packaging.

**External prerequisite:** Publish the selected `baffle-proxy` version to crates.io before enabling its normal build path.

depends-on: none

---

## baffle-4 — Replace the Squid proxy image and implement container-managed startup

**Context:** [Baffle integration proposal](./prd.md), especially *Proxy-container startup and supervision*.

### Objective

Run one Baffle daemon per project inside Cladding's existing proxy pod. Baffle's container startup script—not host-side Cladding control calls—owns initial session creation and daemon supervision.

### Implementation

- Replace the default Squid image with a small Linux image compatible with the embedded Baffle executable. Include a shell and only the libraries/utilities required by startup; no Cargo or Rust toolchain in the runtime image. Preserve a compatible proxy-image override.
- Mount `tools/bin/baffle` read-only, native configuration read-only, private credentials read-only and appropriate scoped runtime socket directories read/write. Keep the control socket inside a private proxy-container path, not in the execution-container mounts.
- Replace `proxy_startup.sh` so it verifies mounted inputs, starts `baffle daemon --config /opt/config/proxy/daemon.toml`, waits **inside the proxy container** for its control socket, then runs `baffle create agent.toml` and conditionally `baffle create nw-sandbox.toml`.
- The session TOML files set `persistent = true`; each create command returns instead of holding an ephemeral lease. On partial provisioning failure, terminate the daemon and exit unsuccessfully. Forward termination signals, reap child processes and propagate an unexpected daemon exit.
- Make proxy startup errors diagnosable in container logs and status.
- Do **not** add a Cladding-level session-readiness wait or change the order in which Cladding starts its ordinary execution containers. Transient early proxy failures are an accepted phase-one behavior.

### Acceptance criteria

- Starting the proxy container establishes the agent session and, only when enabled, the network-sandbox session.
- Stopping the proxy container terminates Baffle and its persistent sessions without host-side session teardown.
- Invalid configuration, missing credentials and unexpected daemon exits produce useful diagnostics and nonzero container exit.
- No Squid process is required.

depends-on: baffle-1, baffle-2, baffle-3

---

## baffle-5 — Connect scoped Baffle Unix data sockets and remove the Squid bridge

**Context:** [Baffle integration proposal](./prd.md), especially *Data sockets, UID mapping and isolation*.

### Objective

Replace Squid's per-component TCP listeners and the proxy `socat` bridge sidecar with Baffle's independently authorized Unix data sockets, while preserving each execution container's existing local proxy interface.

### Implementation

- Use the existing host-backed paths under `.cladding/runtime/sockets/proxy/agent/` and `proxy/nw-sandbox/`, containing stable `proxy.sock` names. Mount their parent into the Baffle proxy container at its configured data-socket root; mount only the appropriate scoped subdirectory into each execution container.
- Retain the agent and network-sandbox `socat` listeners at `127.0.0.1:3128` and the existing `http_proxy`/`https_proxy` settings. Applications continue to connect to the same local HTTP proxy endpoint, using CONNECT for HTTPS origins.
- Verify that Baffle-owned mode-`0600` socket files and mode-`0700` parent directories are actually accessible across rootless Podman UID mappings, execution-container `keep-id` settings, optional `runsc`, and Podman-machine environments where supported. Do not assume a socket's apparent host owner implies compatible in-container credentials.
- Remove `<name>-proxy-bridge` if direct access succeeds. If direct access cannot preserve required permissions, implement a **minimal trusted socket relay in the proxy pod** as a documented fallback; do not broaden socket modes, share the control socket or add another proxy implementation.
- Preserve the execution containers' `--network none` setting and the filesystem sandbox's lack of proxy egress. The agent and network sandbox continue to receive their existing read-only `config/` mount, including readable non-secret Baffle TOML. **Do not add configuration hiding.**

### Acceptance criteria

- The agent and enabled network sandbox use distinct Baffle sessions and can connect through their existing `127.0.0.1:3128` endpoints.
- No execution container receives a Baffle control socket, CA private key or injection credential file.
- The old Squid bridge sidecar is removed when direct socket access meets the supported-platform tests; any necessary trusted relay is narrowly scoped and documented.
- A disabled network sandbox does not expose a network-sandbox data socket.

depends-on: baffle-4

---

## baffle-6 — Install Baffle's public CA in execution-container trust stores

**Context:** [Baffle integration proposal](./prd.md), especially *Public CA trust in execution containers*.

### Objective

Make the project's Baffle interception CA trusted by applications in the agent and enabled network sandbox, without permanently running those workloads as root or using a privileged Podman container.

### Implementation

- Mount **only** the public `ca.crt` read-only into each applicable execution container at `/run/cladding/ca/baffle.crt`.
- Immediately after creating each execution container, run a one-time `podman exec --user 0` that copies the certificate into `/usr/local/share/ca-certificates/baffle.crt` and invokes `update-ca-certificates` in the default Debian-based image.
- Add `ENV NODE_USE_SYSTEM_CA=1` to `Containerfile.cladding` before normal Node.js applications start. Do not depend on configuring a separate CA-bundle environment variable for every application.
- Do not change the workload's normal unprivileged user, add `--privileged` or run the initialization step in the host namespace.
- If the CA install command fails, propagate the error so the lifecycle layer can clean up resources created by that invocation. Ensure the command works in the supported standard and `runsc` execution modes.
- The filesystem sandbox needs no CA installation while it has no proxy egress. Document equivalent initialization requirements for custom images and limits of application-specific trust stores and certificate pinning.

### Acceptance criteria

- Default agent and network-sandbox containers trust Baffle-intercepted HTTPS after Cladding provisions the certificate.
- Their ordinary processes still run under the configured unprivileged account.
- Representative curl, Git and Node.js HTTPS requests work with interception; installation failures are visible and actionable.
- No private CA material enters the execution containers.

depends-on: baffle-2

---

## baffle-7 — Integrate Baffle with Cladding lifecycle, including run

**Context:** [Baffle integration proposal](./prd.md), especially *Target architecture* and *Reload, commands and cleanup*.

### Objective

Wire the new Baffle proxy image, scoped sockets, persistent credentials and post-start CA installation into the existing Podman runtime and Cladding lifecycle without adding a production proxy-readiness barrier.

### Implementation

- Update the runtime specification, proxy pod/container inventory, mount construction and required-binary/image/config checks. Remove expectations of the Squid bridge sidecar and its local TCP ports.
- In `cladding up`, prepare/validate the project CA before any container needing it is created; materialize runtime scripts and create containers through the existing Podman lifecycle. Install the public CA via `podman exec --user 0` after creating each agent/network-sandbox container.
- Let the proxy container's startup script provision its sessions independently. Cladding must **not** block agent/network-sandbox startup waiting for a Baffle session to become ready. Installation of the CA is synchronous, but Baffle session readiness is not.
- Integrate the same behavior with `cladding run`, including its unique names, private runtime namespace, noninteractive `--config -` behavior and ownership-aware cleanup. Avoid accidentally reusing a different project's CA or secret source.
- Preserve running-project discovery, name collision/incomplete-runtime reporting and normal `check`, `build`, `up`, `down`, `destroy` and `run` semantics. `down` and `destroy` remove disposable runtime resources and proxy sessions, not a persistent project's CA or injection credentials.
- On container-creation or CA-installation failure, report the cause and clean up only resources owned by the failing invocation. Do not change unrelated `cladding expose` and `cladding inject` behavior.

### Acceptance criteria

- The complete proxy/agent/network-sandbox arrangement works under normal `up` and isolated `run` lifecycles.
- A normal project restart retains its CA; cleanup removes disposable sockets and containers without deleting persistent credentials.
- The expected runtime inventory matches the new container set and no longer includes Squid components.
- CA-installation failures follow existing cleanup conventions; session startup failures remain diagnosable through proxy-container status/logs.
- No production Cladding-level proxy-readiness barrier is introduced.

depends-on: baffle-4, baffle-5, baffle-6

---

## baffle-8 — Replace Squid reconfigure with Baffle session reload

**Context:** [Baffle integration proposal](./prd.md), especially *Reload, commands and cleanup*.

### Objective

Preserve `cladding reload-proxy` as the user-facing command, backed by Baffle's explicit reload of active file-backed sessions.

### Implementation

- Replace the current `podman exec ... squid -k reconfigure` action with an exec into the proxy container that runs `baffle reload --all` against its private control socket.
- Read the **existing user-edited native session files**. Do not regenerate TOML from a separate Cladding policy format.
- Preserve and display each session's `reloaded`, `unchanged` or `failed` status, and fail the Cladding command if any reload fails.
- Verify that policy, injection-credential and socket changes apply to newly accepted connections without cancelling connections already accepted under the previous policy. Invalid file updates must leave the old effective session intact.
- Handle explicitly the limits of Baffle's non-disruptive reload when old policy generations or old listeners remain in use. Note that daemon settings, CA replacement and binary upgrades need a container restart.

### Acceptance criteria

- Valid edits to `agent.toml` or `nw-sandbox.toml` take effect after `cladding reload-proxy` without restarting accepted connections.
- Unchanged configurations are reported as no-ops; invalid updates report failure without disrupting the prior policy.
- Credential updates affect newly accepted connections, with no claim of immediate revocation for already-open connections.
- Reloading one failing session does not hide the results of other sessions.

depends-on: baffle-7

---

## baffle-9 — Add integration and security coverage for the Baffle runtime

**Context:** [Baffle integration proposal](./prd.md), especially *Validation*.

### Objective

Exercise Baffle integration end-to-end, beyond isolated command construction and TOML parsing tests, and verify the intended phase-one isolation and lifecycle behavior.

### Implementation

- Test `init` output against the pinned Baffle schema and file-only ownership/permissions requirements. Ensure there are no required Squid templates and that existing unrelated sandbox policies remain intact.
- Start the actual proxy image with agent/network-sandbox fixtures and verify exact-host HTTPS allow/deny, explicit destination ports, plaintext HTTP rejection, separate agent and network-sandbox policies, and no network-sandbox session when the component is disabled.
- Verify scoped data sockets and permissions in rootless Podman; test supported `runsc` and Podman-machine combinations where CI or an appropriate test runner is available. If a trusted fallback relay was necessary, test that boundary rather than relaxing socket access.
- Exercise intercepted TLS with the project CA installed via `podman exec`. Include representative curl, Git and Node.js behavior, and assert that credential injection occurs only for authorized intercepted requests. Do not place actual production secrets in test fixtures or logs.
- Test persistent CA reuse, invalid/expired or partial CA material, missing/mispermissioned injection credentials, one-off isolation, normal shutdown and failure cleanup.
- Test changed, unchanged and invalid reloads; confirm existing accepted connections continue with their original policy and credential snapshot while newly accepted connections use the replacement.
- Verify the agent/network sandbox can still read ordinary non-secret `config/` content; **do not** test or require hiding policy TOML. Verify only that control sockets, CA private keys and injection secret files remain unavailable.
- Integration fixtures may wait for test services and sessions; this does **not** authorize adding a readiness barrier to production Cladding.

### Acceptance criteria

- Automated integration tests cover the actual Baffle proxy image and representative HTTPS client workloads.
- Tests catch regressions in socket scoping, secret isolation, trust installation, reload semantics and `run` cleanup.
- The supported-platform test matrix and any test-environment limitations are explicitly documented.

depends-on: baffle-7, baffle-8

---

## baffle-10 — Update CI, embedded-binary packaging and releases

**Context:** [Baffle integration proposal](./prd.md), especially *Binary packaging and proxy image* and *Validation*.

### Objective

Produce repeatable Cladding releases containing a compatible Baffle executable, with automated checks for the final container integration.

### Implementation

- Install/build the pinned crates.io `baffle-proxy` version in supported Linux builders, accounting for Rust, CMake, libclang and native TLS dependencies. Reuse caches and validated artifacts so routine CI does not compile Baffle unnecessarily.
- Supply `CLADDING_BAFFLE_BIN` when producing Cladding binaries on other hosts, including macOS. Ensure every release embeds a Linux Baffle binary for the expected Podman guest architecture rather than the host OS.
- Verify the extracted executable starts inside the default proxy runtime image and that its dynamic dependencies resolve. Diagnose unsupported or mismatched targets explicitly.
- Run relevant Rust unit tests, template validation and Baffle integration tests in CI. Where a target requires a separate environment (for example Podman-machine or `runsc`), document and gate it consistently with Cladding's supported-platform policy.
- Package any required Baffle and transitive-dependency license and notice materials with the appropriate release artifacts. Preserve Cladding's existing release flow for its other embedded tools.

### Acceptance criteria

- The release build embeds and verifies the intended pinned Baffle executable for each supported execution target.
- CI runs the implemented integration suite and fails on Baffle packaging/runtime incompatibility.
- The updated workflow does not download Baffle GitHub binary release assets and includes required redistribution notices.

depends-on: baffle-3, baffle-9

---

## baffle-11 — Update documentation and remove obsolete Squid guidance

**Context:** [Baffle integration proposal](./prd.md), especially *Security and compatibility notes*.

### Objective

Make the README, CLI help, architecture reference and configuration documentation match the implemented Baffle runtime rather than describing Squid as current.

### Implementation

- Document the user-facing `init` → `build` → `up` workflow, native Baffle daemon/session TOML paths, conditional network-sandbox session, local proxy endpoint, scoped data sockets and the absence of a production session-readiness barrier.
- Explain project-scoped CA creation/reuse, installation into the agent/network-sandbox system stores with container-root `podman exec`, `NODE_USE_SYSTEM_CA=1` in the default image, custom-image requirements, CA rotation and certificate-pinning limitations.
- Document the separate injection-credential directory, per-file permissions, symbolic secret names, allowed-secret declarations and safe provisioning/rotation. Clearly distinguish readable non-secret configuration from private CA keys, secrets and the phase-one private control socket.
- Document `cladding reload-proxy` and Baffle's changed/unchanged/failed results, including old connections retaining old policy and credentials and daemon/CA changes requiring restart.
- Describe the deliberate incompatibilities: no old Squid config support, no ordinary plaintext HTTP, no wildcard hosts or literal-IP rules, and no Baffle-enforced destination-IP filtering. Clarify that a local `http://127.0.0.1:3128` proxy URL still supports HTTPS-over-CONNECT.
- Update architecture diagrams, runtime inventory, configuration examples, current-runtime summary and command help. Remove or clearly label obsolete Squid material. Keep `cladding expose`, `cladding inject` and filesystem-sandbox behavior accurately represented.
- Include an operator troubleshooting section for UID/socket permissions, failed file-only validation, CA installation failures, missing credentials, early connection failures and proxy logs.

### Acceptance criteria

- Following the README works with the default Baffle runtime without referring to Squid.
- Security boundaries and explicit migration incompatibilities match the implemented system.
- Existing documentation no longer presents the Squid bridge as the current architecture.

depends-on: baffle-7, baffle-8, baffle-9

---

## baffle-12 — DEFERRED: authorize per-command network-sandbox proxy sessions

**Context:** [Baffle integration proposal](./prd.md), especially *Non-goals for phase one* and *Data sockets, UID mapping and isolation*.

**Status:** Deferred; do not implement as part of the initial Squid replacement.

### Objective

Eventually allow the network sandbox to obtain independently configured, short-lived Baffle sessions for individual commands without gaining unrestricted authority over the project's other sessions.

### Design and future implementation

- Preserve file-only session configuration: commands should select only administrator-approved session files and should not supply unrestricted inline policy.
- Define an explicit authorization boundary before exposing any control capability. Baffle currently authenticates control clients by Unix peer UID, and file-only mode does **not** restrict that UID to a subset of files or operations. Directly mounting the existing control socket would allow operations beyond the intended per-command scope.
- Assess a narrow privileged broker in Cladding or Baffle-side authorization extensions that permit only designated creation requests and prevent stopping, reloading or enumerating unrelated sessions.
- Associate each per-command session with an enforceable lease; deliver only its assigned data socket to the command and release the session when the command exits, is cancelled or its parent fails. Prevent leaked sessions after partial provisioning and interrupted supervision.
- Keep project-wide agent and baseline network-sandbox sessions unaffected. Readability of non-secret Baffle TOML in the agent and network sandbox is permitted and **must not** be treated as the security boundary; protection belongs at the control interface and socket access layer.
- Specify lifecycle, threat model, tests and any cross-repository Baffle work in a separate implementation proposal before starting this deferred issue.

### Acceptance criteria

- A delegated command can create/use only an explicitly authorized file-backed session and cannot control another workload's sessions.
- Session lifetime is bound to the command, including error, cancellation and interrupted-controller paths.
- Ordinary agent/network-sandbox proxy sessions and the privacy of CA signing keys and credential values are preserved.

depends-on: baffle-9
