#!/usr/bin/env python3
"""Collect license texts and attribution for the locked Baffle crate graph."""

import json
import shutil
import subprocess
import sys
from pathlib import Path


def main() -> int:
    if len(sys.argv) != 3:
        print(
            "usage: collect-baffle-notices.py <baffle-Cargo.toml> <output-dir>",
            file=sys.stderr,
        )
        return 2

    manifest = Path(sys.argv[1]).resolve()
    output_dir = Path(sys.argv[2]).resolve()
    metadata = subprocess.run(
        [
            "cargo",
            "metadata",
            "--manifest-path",
            str(manifest),
            "--locked",
            "--format-version",
            "1",
        ],
        check=True,
        capture_output=True,
        text=True,
    )
    packages = json.loads(metadata.stdout)["packages"]
    output_dir.mkdir(parents=True, exist_ok=True)
    license_dir = output_dir / "licenses"
    license_dir.mkdir(exist_ok=True)

    lines = [
        "# Third-party notices for the embedded Baffle executable",
        "",
        "This executable is built from the locked `baffle-proxy` crate graph. The",
        "license text for each package is included under `licenses/` when the",
        "published crate contains it. Package license expressions and source",
        "links are listed below.",
        "",
    ]
    missing_licenses = []

    for package in sorted(packages, key=lambda item: (item["name"], item["version"])):
        name = package["name"]
        version = package["version"]
        crate_dir = Path(package["manifest_path"]).resolve().parent
        expression = package.get("license") or "NOASSERTION"
        declared_license_file = package.get("license_file")
        if expression == "NOASSERTION" and declared_license_file:
            expression = f"license file `{Path(declared_license_file).name}`"
        source_url = package.get("repository") or f"https://crates.io/crates/{name}/{version}"
        safe_name = "".join(char if char.isalnum() or char in "-_." else "_" for char in name)
        destination = license_dir / f"{safe_name}-{version}"
        destination.mkdir(exist_ok=True)

        license_files = []
        if declared_license_file:
            source = Path(declared_license_file)
            if not source.is_absolute():
                source = crate_dir / source
            if source.is_file() and source.name not in license_files:
                shutil.copy2(source, destination / source.name)
                license_files.append(source.name)
        for pattern in ("LICENSE*", "COPYING*", "NOTICE*", "COPYRIGHT*", "PATENTS*"):
            for source in sorted(crate_dir.glob(pattern)):
                if source.is_file() and source.name not in license_files:
                    shutil.copy2(source, destination / source.name)
                    license_files.append(source.name)

        if expression == "NOASSERTION":
            missing_licenses.append(f"{name} {version}")

        lines.append(f"- **{name} {version}** — `{expression}`; {source_url}.")
        if license_files:
            lines.append(
                "  Included text: "
                + ", ".join(f"`licenses/{safe_name}-{version}/{name}`" for name in license_files)
                + "."
            )
        else:
            lines.append("  No license text file was included in the published crate archive.")

    (output_dir / "THIRD_PARTY_NOTICES.md").write_text("\n".join(lines) + "\n")

    lockfile = manifest.parent / "Cargo.lock"
    if lockfile.is_file():
        shutil.copy2(lockfile, output_dir / "Cargo.lock")

    if missing_licenses:
        print(
            "packages without a declared license: " + ", ".join(missing_licenses),
            file=sys.stderr,
        )
        return 1
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
