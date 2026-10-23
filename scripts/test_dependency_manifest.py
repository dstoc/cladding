#!/usr/bin/env python3
"""Test trusted dependency-manifest generation and archive path checks."""

import importlib.util
import io
import json
import shutil
import tempfile
import tomllib
import unittest
from unittest.mock import patch
from pathlib import Path


ROOT = Path(__file__).parents[1]
PINS = json.loads((ROOT / "scripts/dependency-pins.json").read_text())
SPEC = importlib.util.spec_from_file_location(
    "generate_dependency_manifest", ROOT / "scripts/generate_dependency_manifest.py"
)
if SPEC is None or SPEC.loader is None:
    raise RuntimeError("could not import dependency manifest generator")
GENERATOR = importlib.util.module_from_spec(SPEC)
SPEC.loader.exec_module(GENERATOR)


class DependencyManifestTests(unittest.TestCase):
    def test_dependency_versions_and_archive_digests_remain_pinned(self):
        gantry = PINS["dependencies"]["gantry"]
        baffle = PINS["dependencies"]["baffle"]
        self.assertEqual(gantry["version"], "0.4.8")
        self.assertEqual(baffle["version"], "2.2.0")
        self.assertEqual(
            gantry["archives"]["x86_64"]["sha256"],
            "f4b5eb7c96a838a094f2312c806a3e4e16a7d3bdb5df21b8b882d05aa5267bc7",
        )
        self.assertEqual(
            baffle["archives"]["aarch64"]["sha256"],
            "0be70e2071116327f47f9d57c262b344ffa872e604ad536f15a723cbd01229de",
        )

    def test_manifest_tag_must_match_cargo_package_version_before_download(self):
        version = tomllib.loads((ROOT / "Cargo.toml").read_text())["package"]["version"]
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            args = GENERATOR.argparse.Namespace(
                release_tag="v0.0.0",
                bridge_dir=root / "missing-bridge-assets",
                output=root / "dependency-manifest.json",
                docs_dir=root / "docs",
            )
            with self.assertRaisesRegex(ValueError, f"Cargo package version v{version}"):
                GENERATOR.generate(args)
            self.assertFalse((root / "docs").exists())

    def test_archive_reader_rejects_traversal_and_symbolic_links(self):
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            traversal = root / "traversal.tar.gz"
            with GENERATOR.tarfile.open(traversal, "w:gz") as package:
                item = GENERATOR.tarfile.TarInfo("../outside")
                item.size = 1
                package.addfile(item, io.BytesIO(b"x"))
            with self.assertRaisesRegex(ValueError, "unsafe archive path"):
                GENERATOR.archive_members(traversal)

            symlink = root / "symlink.tar.gz"
            with GENERATOR.tarfile.open(symlink, "w:gz") as package:
                item = GENERATOR.tarfile.TarInfo("baffle")
                item.type = GENERATOR.tarfile.SYMTYPE
                item.linkname = "../../outside"
                package.addfile(item)
            with self.assertRaisesRegex(ValueError, "not a regular file or directory"):
                GENERATOR.archive_members(symlink)

            prefixed_traversal = root / "prefixed-traversal.tar.gz"
            with GENERATOR.tarfile.open(prefixed_traversal, "w:gz") as package:
                item = GENERATOR.tarfile.TarInfo("./../outside")
                item.size = 1
                package.addfile(item, io.BytesIO(b"x"))
            with self.assertRaisesRegex(ValueError, "unsafe archive path"):
                GENERATOR.archive_members(prefixed_traversal)

    def test_archive_reader_rejects_duplicate_names_after_prefix_normalization(self):
        with tempfile.TemporaryDirectory() as directory:
            archive = Path(directory) / "duplicate.tar.gz"
            with GENERATOR.tarfile.open(archive, "w:gz") as package:
                for name in ("./baffle", "baffle"):
                    item = GENERATOR.tarfile.TarInfo(name)
                    item.size = 1
                    package.addfile(item, io.BytesIO(b"x"))
            with self.assertRaisesRegex(ValueError, "duplicate archive entry: baffle"):
                GENERATOR.archive_members(archive)

    def test_baffle_upstream_layout_generates_record_and_documents(self):
        pin = GENERATOR.PINS["dependencies"]["baffle"]
        for arch, (_target, machine, loader) in GENERATOR.ARCHES.items():
            with self.subTest(architecture=arch), tempfile.TemporaryDirectory() as directory:
                root = Path(directory)
                archive_path = root / "baffle-upstream-layout.tar.gz"
                executable = bytearray(256)
                executable[:7] = b"\x7fELF\x02\x01\x01"
                executable[7] = 0
                executable[16:18] = (3).to_bytes(2, "little")
                executable[18:20] = machine.to_bytes(2, "little")
                executable[20:24] = (1).to_bytes(4, "little")
                executable[32:40] = (64).to_bytes(8, "little")
                executable[54:56] = (56).to_bytes(2, "little")
                executable[56:58] = (1).to_bytes(2, "little")
                executable[64:68] = (3).to_bytes(4, "little")
                executable[72:80] = (160).to_bytes(8, "little")
                executable[96:104] = (len(loader) + 1).to_bytes(8, "little")
                executable[160 : 160 + len(loader)] = loader.encode()

                files = {
                    "./baffle": bytes(executable),
                    "./LICENSE": b"baffle license\n",
                    "./share/doc/baffle/licenses/THIRD-PARTY-NOTICES.txt": b"third party notices\n",
                    "./share/doc/baffle/licenses/webpki-root-certs-CDLA-Permissive-2.0.txt": b"root certificate notice\n",
                }
                with GENERATOR.tarfile.open(archive_path, "w:gz") as package:
                    root_entry = GENERATOR.tarfile.TarInfo("./")
                    root_entry.type = GENERATOR.tarfile.DIRTYPE
                    package.addfile(root_entry)
                    for name, data in files.items():
                        item = GENERATOR.tarfile.TarInfo(name)
                        item.size = len(data)
                        package.addfile(item, io.BytesIO(data))

                archive_sha = GENERATOR.digest(archive_path.read_bytes())
                pinned_archive = pin["archives"][arch]
                old_sha = pinned_archive["sha256"]
                pinned_archive["sha256"] = archive_sha

                def copy_fixture(_repo, _tag, _filename, _expected, _sums_expected, destination):
                    shutil.copyfile(archive_path, destination)

                docs_dir = root / "docs"
                download_dir = root / "downloads"
                download_dir.mkdir()
                try:
                    with patch.object(GENERATOR, "download", side_effect=copy_fixture):
                        record = GENERATOR.upstream_record(
                            download_dir, docs_dir, "baffle", arch
                        )
                finally:
                    pinned_archive["sha256"] = old_sha

                self.assertEqual(record["archive_sha256"], archive_sha)
                self.assertEqual(record["executables"][0]["archive_path"], "./baffle")
                self.assertIn({"path": ".", "kind": "directory"}, record["archive_entries"])
                self.assertIn({"path": "baffle", "kind": "file"}, record["archive_entries"])
                if arch == "x86_64":
                    self.assertEqual(
                        (docs_dir / "baffle/LICENSE").read_bytes(), b"baffle license\n"
                    )
                    self.assertEqual(
                        (docs_dir / "baffle/THIRD-PARTY-NOTICES.txt").read_bytes(),
                        b"third party notices\n",
                    )
                    self.assertEqual(
                        (docs_dir / "baffle/webpki-root-certs-CDLA-Permissive-2.0.txt").read_bytes(),
                        b"root certificate notice\n",
                    )
                else:
                    self.assertFalse((docs_dir / "baffle").exists())

    def test_archive_reader_records_only_regular_files_and_directories(self):
        with tempfile.TemporaryDirectory() as directory:
            archive = Path(directory) / "safe.tar.gz"
            with GENERATOR.tarfile.open(archive, "w:gz") as package:
                folder = GENERATOR.tarfile.TarInfo("package")
                folder.type = GENERATOR.tarfile.DIRTYPE
                package.addfile(folder)
                file = GENERATOR.tarfile.TarInfo("package/tool")
                file.size = 3
                package.addfile(file, io.BytesIO(b"elf"))
            _members, entries = GENERATOR.archive_members(archive)
            self.assertEqual(
                entries,
                [
                    {"path": "package", "kind": "directory"},
                    {"path": "package/tool", "kind": "file"},
                ],
            )

    def test_elf_validator_rejects_non_executable_elf_headers(self):
        data = bytearray(64)
        data[:7] = b"\x7fELF\x02\x01\x01"
        data[16:18] = (3).to_bytes(2, "little")
        data[18:20] = (62).to_bytes(2, "little")
        data[20:24] = (1).to_bytes(4, "little")
        GENERATOR.validate_elf(bytes(data), "x86_64", 62, "bridge")

        data[16:18] = (1).to_bytes(2, "little")
        with self.assertRaisesRegex(ValueError, "not a 64-bit little-endian Linux ELF executable"):
            GENERATOR.validate_elf(bytes(data), "x86_64", 62, "bridge")


if __name__ == "__main__":
    unittest.main()
