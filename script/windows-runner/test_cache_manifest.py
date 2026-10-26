#!/usr/bin/env python3
"""Portable tests for the exact bytes that pin the private cross-build inputs."""
import hashlib
import importlib.util
import json
from pathlib import Path
import subprocess
import sys
import tempfile
import unittest

HERE = Path(__file__).resolve().parent
spec = importlib.util.spec_from_file_location("windows_cache_manifest", HERE / "cache-manifest.py")
cache_manifest = importlib.util.module_from_spec(spec)
spec.loader.exec_module(cache_manifest)


class CachePins(unittest.TestCase):
    def setUp(self):
        self.temp = tempfile.TemporaryDirectory()
        self.addCleanup(self.temp.cleanup)
        self.root = Path(self.temp.name)
        self.cache = self.root / "cache"
        self.cache.mkdir()
        self.sdk = self.cache / "xwin"
        self.sdk.mkdir()
        (self.sdk / "sdk.lib").write_bytes(b"first")
        provenance = self.root / "provenance"
        provenance.mkdir()
        self.manifest = provenance / "manifest.vsman"
        self.manifest.write_text(json.dumps(dict(info=dict(buildVersion="17.14.test"), packages=[dict(id="CRT", version="exact")])) )
        self.pins = self.root / "pins.json"
        self.data = dict(windows_cross=dict(package_manifest_sha256=hashlib.sha256(self.manifest.read_bytes()).hexdigest(), vs_channel_build="17.14.test", resolved_components={"CRT": "exact"}, sdk_subdir="xwin", cache_tree_sha256=cache_manifest.manifest(self.cache)["tree_sha256"]))

    def run_check(self):
        self.pins.write_text(json.dumps(self.data))
        return subprocess.run([sys.executable, str(HERE / "cache-manifest.py"), str(self.cache), str(self.root / "output.json"), str(self.pins)], text=True, capture_output=True)

    def test_exact_cache_and_manifest_are_accepted(self):
        result = self.run_check()
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertIn("SDK cache: 1 entries", result.stdout)
        self.assertEqual(json.loads((self.root / "output.json").read_text())["files"][0]["sha256"], hashlib.sha256(b"first").hexdigest())

    def test_changed_sdk_bytes_are_refused(self):
        (self.sdk / "sdk.lib").write_bytes(b"second")
        result = self.run_check()
        self.assertNotEqual(result.returncode, 0)
        self.assertIn("FAIL SDK cache content differs", result.stderr)

    def test_changed_microsoft_manifest_is_refused(self):
        self.manifest.write_text(self.manifest.read_text() + " ")
        result = self.run_check()
        self.assertNotEqual(result.returncode, 0)
        self.assertIn("FAIL Microsoft package manifest content changed", result.stderr)

    def test_changed_component_version_is_refused(self):
        self.data["windows_cross"]["resolved_components"]["CRT"] = "unreviewed"
        result = self.run_check()
        self.assertNotEqual(result.returncode, 0)
        self.assertIn("FAIL resolved Microsoft component changed", result.stderr)

    def test_empty_cache_is_not_an_attestation(self):
        (self.sdk / "sdk.lib").unlink()
        with self.assertRaisesRegex(ValueError, "empty"):
            cache_manifest.manifest(self.cache)

    def test_tree_hash_includes_relative_path_not_just_file_bytes(self):
        original = cache_manifest.manifest(self.cache)["tree_sha256"]
        (self.sdk / "sdk.lib").rename(self.sdk / "other.lib")
        self.assertNotEqual(cache_manifest.manifest(self.cache)["tree_sha256"], original)

    def test_generated_host_wrappers_do_not_change_the_microsoft_payload_pin(self):
        (self.cache / "clang-cl").write_text("generated host tool pointer")
        (self.cache / "cmake").mkdir()
        (self.cache / "cmake/toolchain.cmake").write_text("generated local build configuration")
        result = self.run_check()
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertEqual(json.loads((self.root / "output.json").read_text())["file_count"], 1)

    def test_done_line_order_does_not_change_tree_hash(self):
        done = self.sdk / "DONE"
        done.write_bytes(b"x86_64\nheaders.vsix\nlibraries.msi")
        original = cache_manifest.manifest(self.cache)["tree_sha256"]
        done.write_bytes(b"libraries.msi\nx86_64\nheaders.vsix")
        self.assertEqual(cache_manifest.manifest(self.cache)["tree_sha256"], original)

    def test_changed_done_line_changes_tree_hash(self):
        done = self.sdk / "DONE"
        done.write_bytes(b"x86_64\nheaders.vsix\nlibraries.msi")
        original = cache_manifest.manifest(self.cache)["tree_sha256"]
        done.write_bytes(b"x86_64\nheaders.vsix\nchanged.msi")
        self.assertNotEqual(cache_manifest.manifest(self.cache)["tree_sha256"], original)

    def test_missing_done_line_changes_tree_hash(self):
        done = self.sdk / "DONE"
        done.write_bytes(b"x86_64\nheaders.vsix\nlibraries.msi")
        original = cache_manifest.manifest(self.cache)["tree_sha256"]
        done.write_bytes(b"x86_64\nheaders.vsix")
        self.assertNotEqual(cache_manifest.manifest(self.cache)["tree_sha256"], original)

    def test_duplicate_done_line_changes_tree_hash(self):
        done = self.sdk / "DONE"
        done.write_bytes(b"x86_64\nheaders.vsix")
        original = cache_manifest.manifest(self.cache)["tree_sha256"]
        done.write_bytes(b"x86_64\nheaders.vsix\nheaders.vsix")
        self.assertNotEqual(cache_manifest.manifest(self.cache)["tree_sha256"], original)

    def test_other_done_files_remain_byte_exact(self):
        done = self.sdk / "sdk" / "DONE"
        done.parent.mkdir()
        done.write_bytes(b"first\nsecond")
        original = cache_manifest.manifest(self.cache)["tree_sha256"]
        done.write_bytes(b"second\nfirst")
        self.assertNotEqual(cache_manifest.manifest(self.cache)["tree_sha256"], original)


if __name__ == "__main__":
    unittest.main(verbosity=2)
