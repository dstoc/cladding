# Pinned Linux helper dependencies

Released Cladding binaries contain a small dependency manifest with trusted
SHA-256 checksums. They do not contain the `cladding-bridge`, Gantry,
`run-remote`, or Baffle executable bytes. `cladding build` downloads only the
Linux architecture required by the selected Cladding binary, verifies each
archive and executable, and installs the tools under `.cladding/tools/bin/`.
The bridge comes from the same Cladding release tag. Gantry stays pinned at
0.4.8 and Baffle stays pinned at 2.2.0.

The first build and builds after a dependency checksum changes need network
access. If every installed executable matches its manifest checksum, the build
uses those files without a download. `cladding check` is offline. It reports
missing, outdated, and corrupt tools and keeps the remediation hint to run
`cladding build`.

After upgrading an existing project to a release with this installer, run
`cladding build` once while online to install or replace its helper executables,
then run `cladding check` to verify them.

## Development builds

A development Cargo version does not identify a published bridge release. If
you build Cladding from source and run `cladding build`, provide all three
explicit local overrides:

```sh
CLADDING_BRIDGE_BIN=/path/to/linux/cladding-bridge \
CLADDING_GANTRY_DIR=/path/to/gantry-release \
CLADDING_BAFFLE_BIN=/path/to/linux/baffle \
cladding build
```

`CLADDING_GANTRY_DIR` must contain both `gantry` and `run-remote`. Cladding
checks that all four local files are regular Linux ELF executables for the
selected architecture. It also checks the GNU loader and pinned GLIBC 2.35
ceiling for Gantry, plus the GNU loader for Baffle. These variables apply to
the `cladding build` process; they are not Cargo compile-time inputs.

The fetch scripts provide the pinned upstream archives for local work and CI:

```sh
scripts/fetch_gantry.sh aarch64 /tmp/cladding-gantry
scripts/fetch_baffle.sh aarch64 /tmp/cladding-baffle
```

Use `x86_64` for an x86_64 Linux guest. Use `aarch64` for an aarch64 Linux
guest, including Apple Silicon Podman Machine. Supply a matching
`cladding-bridge` executable as the bridge override.

## Release verification and redistribution

The release workflow packages the bridge binaries for both Linux architectures
and verifies their archive and executable checksums. It verifies the pinned
Gantry and Baffle release archives against digests kept in
`scripts/dependency-pins.json`, then generates the full manifest before it
compiles Cladding host binaries. The manifest records each source repository,
release tag, archive name, Linux architecture, archive checksum, expected
archive entries, and executable checksums. Runtime downloads use only these
manifest values; Cladding does not query a latest version or trust a downloaded
checksum file.

## Updating the pinned version

1. Verify both Linux GNU releases and their SHA256SUMS files. Update the
   trusted archive digests in `scripts/dependency-pins.json`.
2. Review the archive entry list, executable architecture, license, and
   redistribution notices. Update the generator and its tests if the layout
   changes.
3. Update the downstream `baffle-client` dependency only when Cladding needs a
   new client API. Keep helper release versions unchanged otherwise.
4. Run the workflow and runtime checks for both supported architectures.

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
