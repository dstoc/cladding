#!/usr/bin/env python3
"""Tests for the Baffle redistribution notice generator."""

import importlib.util
import sys
import tempfile
import unittest
from pathlib import Path


SCRIPT = Path(__file__).with_name("collect-baffle-notices.py")
SPEC = importlib.util.spec_from_file_location("collect_baffle_notices", SCRIPT)
NOTICES = importlib.util.module_from_spec(SPEC)
sys.dont_write_bytecode = True
SPEC.loader.exec_module(NOTICES)


class CollectBaffleNoticesTests(unittest.TestCase):
    def test_notice_paths_match_copied_license_file_names(self):
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            crate = root / "proxy-extra"
            crate.mkdir()
            (crate / "Cargo.toml").write_text("[package]\nname = 'proxy-extra'\n")
            (crate / "LICENSE-MIT").write_text("MIT license text\n")
            (crate / "NOTICE.txt").write_text("Copyright notice\n")
            output = root / "output"
            packages = [
                {
                    "name": "proxy-extra",
                    "version": "1.2.3",
                    "manifest_path": str(crate / "Cargo.toml"),
                    "license": "MIT",
                    "license_file": "LICENSE-MIT",
                    "repository": "https://example.com/proxy-extra",
                }
            ]

            missing = NOTICES.collect_notices(packages, output)

            self.assertEqual(missing, [])
            notice = (output / "THIRD_PARTY_NOTICES.md").read_text()
            self.assertIn("licenses/proxy-extra-1.2.3/LICENSE-MIT", notice)
            self.assertIn("licenses/proxy-extra-1.2.3/NOTICE.txt", notice)
            self.assertNotIn("licenses/proxy-extra-1.2.3/proxy-extra`", notice)
            self.assertEqual(
                (output / "licenses/proxy-extra-1.2.3/LICENSE-MIT").read_text(),
                "MIT license text\n",
            )
            self.assertEqual(
                (output / "licenses/proxy-extra-1.2.3/NOTICE.txt").read_text(),
                "Copyright notice\n",
            )

    def test_packages_without_declared_licenses_are_reported(self):
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            crate = root / "unknown-license"
            crate.mkdir()
            manifest = crate / "Cargo.toml"
            manifest.write_text("[package]\nname = 'unknown-license'\n")
            output = root / "output"
            packages = [
                {
                    "name": "unknown-license",
                    "version": "0.1.0",
                    "manifest_path": str(manifest),
                    "license": None,
                    "license_file": None,
                }
            ]

            missing = NOTICES.collect_notices(packages, output)

            self.assertEqual(missing, ["unknown-license 0.1.0"])
            self.assertIn("`NOASSERTION`", (output / "THIRD_PARTY_NOTICES.md").read_text())


if __name__ == "__main__":
    unittest.main()
