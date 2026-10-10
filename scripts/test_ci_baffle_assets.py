#!/usr/bin/env python3
"""Check the pinned Baffle release flow and its CI job dependencies."""

import re
import unittest
from pathlib import Path


WORKFLOW = Path(__file__).parents[1] / ".github/workflows/ci.yml"
TEXT = WORKFLOW.read_text()


def job_block(name: str) -> str:
    match = re.search(
        rf"(?ms)^  {re.escape(name)}:\n(.*?)(?=^  [a-zA-Z0-9_-]+:\n|\Z)", TEXT
    )
    if match is None:
        raise AssertionError(f"workflow job is missing: {name}")
    return match.group(1)


class BaffleCiAssetsTests(unittest.TestCase):
    def test_baffle_is_not_built_from_source_in_ci(self):
        self.assertNotIn("  build-baffle:\n", TEXT)
        self.assertNotRegex(TEXT, r"(?m)^\s*run: cargo install .*baffle-proxy")

    def test_each_release_download_is_pinned_verified_and_arch_checked(self):
        cases = {
            "test": ("x86_64", "baffle-proxy-v1.1.0-x86_64-unknown-linux-gnu.tar.gz"),
            "baffle-config": (
                "x86_64",
                "baffle-proxy-v1.1.0-x86_64-unknown-linux-gnu.tar.gz",
            ),
            "baffle-podman-machine-assets-x86_64": (
                "x86_64",
                "baffle-proxy-v1.1.0-x86_64-unknown-linux-gnu.tar.gz",
            ),
            "baffle-podman-machine-assets-aarch64": (
                "aarch64",
                "baffle-proxy-v1.1.0-aarch64-unknown-linux-gnu.tar.gz",
            ),
        }
        for job, (arch, archive) in cases.items():
            with self.subTest(job=job):
                block = job_block(job)
                self.assertIn(f"BAFFLE_ARCHIVE: {archive}", block)
                self.assertIn(
                    'release_url="https://github.com/dstoc/baffle/releases/download/v1.1.0"',
                    block,
                )
                self.assertIn('"$release_url/$BAFFLE_ARCHIVE"', block)
                self.assertIn('"$release_url/SHA256SUMS"', block)
                self.assertIn("awk -v archive=", block)
                self.assertIn('test "$actual" = "$expected"', block)
                self.assertIn("sha256sum", block)
                self.assertIn("shasum -a 256", block)
                self.assertIn('chmod 0755 "$RUNNER_TEMP/baffle/baffle"', block)
                self.assertIn('description=$(file -b "$RUNNER_TEMP/baffle/baffle")', block)
                machine = "x86-64" if arch == "x86_64" else "aarch64"
                self.assertIn(f'"ELF 64-bit"*{machine}*) ;;', block)

    def test_linux_and_macos_jobs_wait_only_for_matching_assets(self):
        for name in ("test", "baffle-config", "baffle-podman-machine-assets-x86_64", "baffle-podman-machine-assets-aarch64"):
            with self.subTest(job=name):
                self.assertNotRegex(job_block(name), r"(?m)^    needs:")

        intel = job_block("baffle-podman-machine")
        self.assertIn("needs: baffle-podman-machine-assets-x86_64", intel)
        self.assertNotIn("baffle-config", intel)
        self.assertIn("cladding-baffle-linux-tools-x86_64", intel)

        apple_silicon = job_block("baffle-apple-silicon-build")
        self.assertIn("needs: baffle-podman-machine-assets-aarch64", apple_silicon)
        self.assertIn("cladding-baffle-linux-tools-aarch64", apple_silicon)

    def test_behavior_and_notice_coverage_remain_in_ci(self):
        test = job_block("test")
        self.assertIn("python3 scripts/test_collect_baffle_notices.py", test)
        self.assertIn("python3 scripts/test_ci_baffle_assets.py", test)

        config = job_block("baffle-config")
        for validator in (
            "scripts/validate_baffle_config.sh",
            "scripts/validate_baffle_runtime.sh default",
            "scripts/validate_baffle_runtime.sh runsc",
            "scripts/validate_managed_mounts.sh default",
            "scripts/validate_managed_mounts.sh runsc",
        ):
            self.assertIn(validator, config)


if __name__ == "__main__":
    unittest.main()
