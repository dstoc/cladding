# Embedded Baffle binary

Cladding embeds `baffle-proxy` **0.2.0** from crates.io. The package publishes
the `baffle` executable and declares Rust 1.96 as its minimum supported
version. Cladding pins that version in `build.rs` and invokes Cargo with
`--locked`.

## Build requirements

The normal build path requires a Linux GNU host with the same architecture as
the Cladding target. Supported architectures are `x86_64` and `aarch64`. The
build environment needs Rust 1.96 or newer, CMake, Clang, and libclang. These
tools are needed to build Baffle and do not belong in the proxy runtime image.

For Cladding builds on macOS or other cross-build hosts, set
`CLADDING_BAFFLE_BIN` to a prebuilt 64-bit Linux GNU executable for the matching
Podman guest architecture. The build checks the ELF format and architecture.
CI also runs the extracted executable in Cladding's selected default proxy
image to check its loader and shared-library requirements.

Cladding writes the embedded executable to `.cladding/tools/bin/baffle` and
sets its mode to executable. `cladding check` reports a missing or outdated
copy. Run `cladding build` to refresh it.

## Updating the pinned version

1. Confirm that the new `baffle-proxy` version is published on crates.io.
2. Review its tagged `Cargo.toml` for its binary name, Rust minimum version,
   native build requirements, target support, and license.
3. Update `BAFFLE_VERSION` and the minimum Rust version in `build.rs`.
4. Update the Rust toolchain and Cargo install version in the CI and release
   workflows.
5. Build one Linux GNU binary for each supported architecture. Use the
   resulting artifact through `CLADDING_BAFFLE_BIN` for Cladding release jobs.
6. Regenerate the release notices with
   `scripts/collect-baffle-notices.py` from the installed crate source.
7. Run the runtime smoke test against the selected proxy image for each
   supported architecture.

Do not use Baffle's GitHub release assets for normal packaging. Cargo's locked
crate graph is the source of the executable and its release notices.

## Redistribution notices

`baffle-proxy` 0.2.0 declares the MIT license. The Baffle executable also
contains code from its locked dependencies, and each dependency keeps its own
license terms. Release archives include `THIRD_PARTY_NOTICES.md`, the upstream
`Cargo.lock` when present in the published crate, and license or notice files
shipped in each crate archive. Review entries with no declared license before
publishing a release.
