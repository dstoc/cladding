#!/usr/bin/env python3
"""Build the trusted, executable-free manifest used by released Cladding."""

from __future__ import annotations

import argparse
import hashlib
import json
import pathlib
import re
import subprocess
import tarfile
import tempfile
import tomllib


ROOT = pathlib.Path(__file__).resolve().parents[1]
PINS = json.loads((ROOT / "scripts/dependency-pins.json").read_text())
ARCHES = {
    "x86_64": ("x86_64-unknown-linux-gnu", 62, "ld-linux-x86-64.so.2"),
    "aarch64": ("aarch64-unknown-linux-gnu", 183, "ld-linux-aarch64.so.1"),
}
GANTRY_MAX_GLIBC = (2, 35)


def digest(data: bytes) -> str:
    return hashlib.sha256(data).hexdigest()


def safe_member_name(name: str) -> str:
    if "\\" in name or name.startswith("/"):
        raise ValueError(f"unsafe archive path: {name!r}")
    clean = name.removesuffix("/")
    parts = clean.split("/")
    if not clean or any(part in ("", ".", "..") for part in parts) or ":" in clean:
        raise ValueError(f"unsafe archive path: {name!r}")
    return clean


def archive_members(archive: pathlib.Path) -> tuple[dict[str, tarfile.TarInfo], list[dict[str, str]]]:
    members: dict[str, tarfile.TarInfo] = {}
    entries: list[dict[str, str]] = []
    with tarfile.open(archive, "r:gz") as package:
        for member in package.getmembers():
            name = safe_member_name(member.name)
            if name in members:
                raise ValueError(f"duplicate archive entry: {name}")
            if member.isdir():
                kind = "directory"
            elif member.isfile():
                kind = "file"
            else:
                raise ValueError(f"archive entry is not a regular file or directory: {name}")
            members[name] = member
            entries.append({"path": name, "kind": kind})
        package.close()
    if not members:
        raise ValueError(f"archive is empty: {archive}")
    return members, entries


def archive_bytes(archive: pathlib.Path, members: dict[str, tarfile.TarInfo], name: str) -> bytes:
    member = members.get(name)
    if member is None or not member.isfile():
        raise ValueError(f"archive is missing regular file {name}: {archive.name}")
    with tarfile.open(archive, "r:gz") as package:
        stream = package.extractfile(member)
        if stream is None:
            raise ValueError(f"could not read {name} from {archive.name}")
        return stream.read()


def validate_elf(data: bytes, arch: str, expected_machine: int, path: str) -> None:
    if (
        len(data) < 64
        or data[:4] != b"\x7fELF"
        or data[4:7] != b"\x02\x01\x01"
        or int.from_bytes(data[16:18], "little") not in (2, 3)
        or int.from_bytes(data[20:24], "little") != 1
    ):
        raise ValueError(f"{path} is not a 64-bit little-endian Linux ELF executable")
    if data[7] not in (0, 3):
        raise ValueError(f"{path} does not identify a Linux ELF executable")
    machine = int.from_bytes(data[18:20], "little")
    if machine != expected_machine:
        raise ValueError(f"{path} has ELF machine {machine}, expected {arch}")


def validate_loader(data: bytes, arch: str, expected_machine: int, loader: str) -> None:
    validate_elf(data, arch, expected_machine, "release executable")
    program_offset = int.from_bytes(data[32:40], "little")
    entry_size = int.from_bytes(data[54:56], "little")
    count = int.from_bytes(data[56:58], "little")
    found = False
    for index in range(count):
        header = program_offset + index * entry_size
        if entry_size < 56 or header + 56 > len(data):
            break
        if int.from_bytes(data[header : header + 4], "little") != 3:
            continue
        offset = int.from_bytes(data[header + 8 : header + 16], "little")
        size = int.from_bytes(data[header + 32 : header + 40], "little")
        if offset + size > len(data):
            break
        interpreter = data[offset : offset + size].split(b"\0", 1)[0].decode("utf-8", "replace")
        found = interpreter.endswith(loader)
        break
    if not found:
        raise ValueError(f"release executable does not use the required GNU loader {loader}")


def validate_gantry(data: bytes, arch: str, machine: int, loader: str, path: str) -> None:
    validate_loader(data, arch, machine, loader)
    versions = [
        (int(major), int(minor))
        for major, minor in re.findall(rb"GLIBC_(\d+)\.(\d+)", data)
    ]
    if not versions:
        raise ValueError(f"{path} has no GNU libc symbol-version requirements")
    if max(versions) > GANTRY_MAX_GLIBC:
        raise ValueError(f"{path} exceeds the pinned Gantry GLIBC_2.35 baseline")


def download(
    repo: str,
    tag: str,
    filename: str,
    expected: str,
    sums_expected: str,
    destination: pathlib.Path,
) -> None:
    url = f"https://github.com/{repo}/releases/download/{tag}/{filename}"
    subprocess.run(
        ["curl", "--fail", "--location", "--retry", "3", "--output", str(destination), url],
        check=True,
    )
    actual = digest(destination.read_bytes())
    if actual != expected:
        raise ValueError(f"SHA-256 mismatch for {repo} {tag} {filename}: {actual}")
    sums_path = destination.with_name("SHA256SUMS")
    subprocess.run(
        [
            "curl",
            "--fail",
            "--location",
            "--retry",
            "3",
            "--output",
            str(sums_path),
            f"https://github.com/{repo}/releases/download/{tag}/SHA256SUMS",
        ],
        check=True,
    )
    if digest(sums_path.read_bytes()) != sums_expected:
        raise ValueError(f"SHA256SUMS checksum mismatch for {repo} {tag}")
    declared = {}
    for line in sums_path.read_text().splitlines():
        fields = line.split()
        if len(fields) == 2:
            declared[fields[1].lstrip("*")] = fields[0]
    if declared.get(filename) != expected:
        raise ValueError(f"SHA256SUMS does not pin the expected digest for {filename}")


def bridge_record(bridge_dir: pathlib.Path, tag: str, arch: str) -> dict:
    target, machine, _ = ARCHES[arch]
    filename = f"cladding-bridge-{tag}-{target}.tar.gz"
    archive = bridge_dir / filename
    archive_sha = digest(archive.read_bytes())
    expected_archives = {}
    for line in (bridge_dir / "cladding-bridge-SHA256SUMS").read_text().splitlines():
        parts = line.split()
        if len(parts) != 2:
            raise ValueError("invalid cladding-bridge-SHA256SUMS entry")
        expected_archives[parts[1].lstrip("*")] = parts[0]
    if expected_archives.get(filename) != archive_sha:
        raise ValueError(f"bridge archive checksum does not match its packaged checksum: {filename}")

    executable_sums = {}
    for line in (bridge_dir / "cladding-bridge-executable-SHA256SUMS").read_text().splitlines():
        parts = line.split()
        if len(parts) != 2:
            raise ValueError("invalid cladding-bridge-executable-SHA256SUMS entry")
        executable_sums[parts[1].lstrip("*")] = parts[0]

    members, entries = archive_members(archive)
    data = archive_bytes(archive, members, "cladding-bridge")
    validate_elf(data, arch, machine, "cladding-bridge")
    executable_sha = digest(data)
    checksum_name = f"cladding-bridge-{tag}-{target}"
    if executable_sums.get(checksum_name) != executable_sha:
        raise ValueError(f"bridge executable checksum mismatch for {target}")
    return {
        "name": "cladding-bridge",
        "source_repository": "dstoc/cladding",
        "release_tag": tag,
        "archive_filename": filename,
        "architecture": target,
        "archive_sha256": archive_sha,
        "archive_entries": entries,
        "executables": [
            {
                "archive_path": "cladding-bridge",
                "install_path": "cladding-bridge",
                "sha256": executable_sha,
            }
        ],
    }


def upstream_record(
    temporary: pathlib.Path,
    docs_dir: pathlib.Path,
    tool: str,
    arch: str,
) -> dict:
    pin = PINS["dependencies"][tool]
    artifact = pin["archives"][arch]
    archive = temporary / artifact["filename"]
    download(
        pin["repository"],
        f"v{pin['version']}",
        artifact["filename"],
        artifact["sha256"],
        pin["sha256sums_sha256"],
        archive,
    )
    members, entries = archive_members(archive)
    target, machine, loader = ARCHES[arch]

    if tool == "gantry":
        root = artifact["package_root"]
        executables = []
        for name in ("gantry", "run-remote"):
            path = f"{root}/{name}"
            data = archive_bytes(archive, members, path)
            validate_gantry(data, arch, machine, loader, name)
            executables.append({"archive_path": path, "install_path": name, "sha256": digest(data)})
        for source, destination in (
            (f"{root}/LICENSE", docs_dir / "gantry/LICENSE"),
            (f"{root}/README.md", docs_dir / "gantry/README.md"),
            (f"{root}/docs/reference.md", docs_dir / "gantry/docs/reference.md"),
        ):
            data = archive_bytes(archive, members, source)
            if arch == "x86_64":
                destination.parent.mkdir(parents=True, exist_ok=True)
                destination.write_bytes(data)
        name = "gantry"
    else:
        data = archive_bytes(archive, members, "baffle")
        validate_loader(data, arch, machine, loader)
        executables = [{"archive_path": "baffle", "install_path": "baffle", "sha256": digest(data)}]
        if arch == "x86_64":
            for source, destination in (
                ("LICENSE", docs_dir / "baffle/LICENSE"),
                (
                    "share/doc/baffle/licenses/THIRD-PARTY-NOTICES.txt",
                    docs_dir / "baffle/THIRD-PARTY-NOTICES.txt",
                ),
                (
                    "share/doc/baffle/licenses/webpki-root-certs-CDLA-Permissive-2.0.txt",
                    docs_dir / "baffle/webpki-root-certs-CDLA-Permissive-2.0.txt",
                ),
            ):
                (docs_dir / "baffle").mkdir(parents=True, exist_ok=True)
                destination.write_bytes(archive_bytes(archive, members, source))
        name = "baffle"

    return {
        "name": name,
        "source_repository": pin["repository"],
        "release_tag": f"v{pin['version']}",
        "archive_filename": artifact["filename"],
        "architecture": target,
        "archive_sha256": artifact["sha256"],
        "archive_entries": entries,
        "executables": executables,
    }


def generate(args: argparse.Namespace) -> None:
    release_tag = args.release_tag
    cargo_version = tomllib.loads((ROOT / "Cargo.toml").read_text())["package"]["version"]
    if not re.fullmatch(r"v\d+\.\d+\.\d+", release_tag) or release_tag != f"v{cargo_version}":
        raise ValueError(
            f"release tag {release_tag!r} must match Cargo package version v{cargo_version}"
        )

    bridge_dir = args.bridge_dir.resolve()
    docs_dir = args.docs_dir.resolve()
    docs_dir.mkdir(parents=True, exist_ok=True)
    platforms = {}
    with tempfile.TemporaryDirectory(prefix="cladding-dependency-manifest-") as temporary_name:
        temporary = pathlib.Path(temporary_name)
        for arch, (target, _machine, _loader) in ARCHES.items():
            records = [bridge_record(bridge_dir, release_tag, arch)]
            records.extend(
                [upstream_record(temporary, docs_dir, "gantry", arch), upstream_record(temporary, docs_dir, "baffle", arch)]
            )
            platforms[target] = records

    manifest = {
        "schema_version": 1,
        "cladding_release": {"version": cargo_version, "tag": release_tag},
        "platforms": platforms,
    }
    output = args.output.resolve()
    output.parent.mkdir(parents=True, exist_ok=True)
    output.write_text(json.dumps(manifest, sort_keys=True, indent=2) + "\n")


def main() -> None:
    parser = argparse.ArgumentParser()
    parser.add_argument("--release-tag", required=True)
    parser.add_argument("--bridge-dir", required=True, type=pathlib.Path)
    parser.add_argument("--output", required=True, type=pathlib.Path)
    parser.add_argument("--docs-dir", required=True, type=pathlib.Path)
    args = parser.parse_args()
    try:
        generate(args)
    except (OSError, ValueError, subprocess.CalledProcessError, tarfile.TarError) as error:
        raise SystemExit(f"dependency manifest generation failed: {error}") from error


if __name__ == "__main__":
    main()
