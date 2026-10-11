# Embedded Baffle binary

A normal local build without `CLADDING_BAFFLE_BIN` embeds `baffle-proxy`
**1.0.0** from crates.io. The package publishes the `baffle` executable and
declares Rust 1.96 as its minimum supported version. Cladding pins that
version in `build.rs` and invokes Cargo with `--locked`.

## Build requirements

The normal build path requires a Linux GNU host with the same architecture as
the Cladding target. Supported architectures are `x86_64` and `aarch64`. The
build environment needs Rust 1.96 or newer, CMake, Clang, and libclang. These
tools are needed to build Baffle and do not belong in the proxy runtime image.

For Cladding builds on macOS or other cross-build hosts, set
`CLADDING_BAFFLE_BIN` to a prebuilt 64-bit Linux GNU executable for the matching
Podman guest architecture. The build checks the ELF format and architecture.
CI runs Baffle in Cladding's default proxy image to check its loader and shared
libraries. The integration suite also starts the executable that Cladding
extracts during `cladding build`.

Published release archives are built for Linux x86_64, Linux aarch64, and
Apple Silicon macOS. Each archive embeds the Linux GNU Baffle executable for
the matching Podman guest architecture. The release workflow downloads the
pinned Baffle v1.1.0 Linux archive and checks its checksum against that
release's `SHA256SUMS` before extraction. It uses the x86_64 archive for the
x86_64 Linux package and the aarch64 archive for the aarch64 Linux and Apple
Silicon macOS packages. CI uses the same verified published binaries, checks
them in the default proxy image, and runs the integration suite. The
Linux-hosted Podman Machine job checks the x86_64 guest topology. The Apple
Silicon build job checks the embedded aarch64 executable.

Cladding writes the embedded executable to `.cladding/tools/bin/baffle` and
sets its mode to executable. `cladding check` reports a missing or outdated
copy. Run `cladding build` to refresh it.

## Updating the pinned version

1. Confirm that the new Baffle version publishes Linux GNU release archives
   for x86_64 and aarch64, with a `SHA256SUMS` file.
2. Review the release archive contents for the executable, architecture,
   license, and third-party notices.
3. Update the explicit Baffle version and archive names in the CI and release
   workflows. Keep checksum verification before archive extraction.
4. Update the architecture and redistribution checks in
   `scripts/test_ci_baffle_assets.py`.
5. Update `BAFFLE_VERSION` in `build.rs` separately when changing the version
   used by local source builds.
6. Run the runtime smoke test against the selected proxy image for each
   supported architecture.

## Redistribution notices

The Cladding release archive includes Baffle's upstream `LICENSE`,
`THIRD-PARTY-NOTICES.txt`, and the `webpki-root-certs` CDLA Permissive 2.0
license text. The third-party notice file contains license text and attribution
for the dependencies in the published Baffle executable. Cladding copies these
files from the checksum-verified upstream archive under `licenses/baffle/`.

## Session file migration

Baffle 1.0 accepts session files in version 2 format. Cladding's generated
files use this format. `cladding init` does not rewrite files that already
exist, so migrate existing version 1 session files before you start or reload
the proxy after upgrading Cladding.

For each session file:

1. Change `version = 1` to `version = 2`.
2. Remove `operation = "create"`.
3. Move `persistent` and `socket_name` from `[session]` to the document root.
4. Replace each `[[rules]]` entry and its `host` field with a hostname-keyed
   table such as `[rules."example.com"]`.
5. Move that rule's `mode`, `ports`, and `paths` fields into its hostname
   table. Change `[[rules.inject]]` to `[[rules."example.com".inject]]` for
   each injection. Omit `ports = [443]` when port 443 is the only allowed
   port. A tunnel rule can omit `mode = "tunnel"`. Baffle infers interception
   from `paths` or `inject`.

Cladding's generated sessions set `unmatched = "deny"` to keep the existing
default-deny policy for hostnames that have no rule. Keep this setting unless
you intend to allow unmatched HTTPS hostnames.
