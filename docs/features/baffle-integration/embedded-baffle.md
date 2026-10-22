# Embedded Baffle binary

A normal Cladding build requires `CLADDING_BAFFLE_BIN`. The build script checks
that the supplied executable is a 64-bit Linux GNU binary for the target
architecture. It does not download or compile Baffle.

Fetch the pinned Baffle **2.2.0** release before building. The script selects
the Linux archive for the requested architecture and verifies its checksum
against that release's `SHA256SUMS` file:

```sh
scripts/fetch_baffle.sh x86_64 /tmp/cladding-baffle-x86_64
CLADDING_BAFFLE_BIN=/tmp/cladding-baffle-x86_64/baffle cargo build --locked
```

Use `aarch64` in both paths when the Cladding target is aarch64. CI uses the
same fetch script for Linux tests and release builds.

## Build requirements

Supported architectures are `x86_64` and `aarch64`. The fetch script verifies
the archive checksum. The build script also checks the executable's ELF format,
architecture, and GNU loader before it embeds the binary.

On Linux, Cladding builds its embedded helper programs with Cargo for the
matching Linux GNU target. On macOS, it builds them in a Rust container for the
matching Linux architecture; this path requires a working Podman installation.
Both paths keep helper outputs under Cargo's `OUT_DIR`. Release builds may
instead supply the three prebuilt helper paths.
CI runs `baffle --version` on native Linux x86_64 and aarch64 runners. The
x86_64 integration suite also starts the executable that Cladding extracts
during `cladding build` in Cladding's default proxy image.

Published release archives are built for Linux x86_64, Linux aarch64, and
Apple Silicon macOS. Each archive embeds the Linux GNU Baffle executable for
the matching Podman guest architecture. The release workflow downloads the
pinned Baffle v2.2.0 Linux archive and checks its checksum against that
release's `SHA256SUMS` before extraction. It uses the x86_64 archive for the
x86_64 Linux package and the aarch64 archive for the aarch64 Linux and Apple
Silicon macOS packages. CI verifies the same published binaries and runs the
native executable smoke check for both architectures. The x86_64 integration
suite checks the binary in the default proxy image. The
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
3. Update the published Baffle binary pin in `build.rs` and
   `scripts/fetch_baffle.sh`. Update the published `baffle-client` dependency
   when Cladding uses a new client API. Then update CI and release workflows.
   Keep checksum verification before archive extraction.
4. Update the architecture and redistribution checks in
   `scripts/test_ci_baffle_assets.py`.
5. Run the runtime smoke test against the selected proxy image for each
   supported architecture.

## Redistribution notices

The Cladding release archive includes Baffle's upstream `LICENSE`,
`THIRD-PARTY-NOTICES.txt`, and the `webpki-root-certs` CDLA Permissive 2.0
license text. The third-party notice file contains license text and attribution
for the dependencies in the published Baffle executable. Cladding copies these
files from the checksum-verified upstream archive under `licenses/baffle/`.

## Session file migration

Baffle 2.2.0 accepts session files in version 2 format. Cladding's generated
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
