#!/usr/bin/env python3
"""Repinning metadata must never authorize new Microsoft component or SDK bytes."""
import contextlib
import hashlib
import importlib.util
import io
import json
from pathlib import Path
import tempfile
import unittest
from unittest.mock import Mock

SPEC = importlib.util.spec_from_file_location("repin_manifest", Path(__file__).with_name("repin-manifest.py"))
assert SPEC is not None and SPEC.loader is not None
REPIN = importlib.util.module_from_spec(SPEC)
SPEC.loader.exec_module(REPIN)


class RepinManifestTests(unittest.TestCase):
    def setUp(self):
        self.temporary = tempfile.TemporaryDirectory()
        self.addCleanup(self.temporary.cleanup)
        self.root = Path(self.temporary.name)
        self.path = self.root / REPIN.PIN_PATH
        self.path.parent.mkdir(parents=True)
        self.pins = {
            "cargo_xwin": "0.23.1", "vs_manifest": "17", "sdk": "10.0.26100",
            "crt": "14.44.17.14", "arch": "x86_64", "variant": "desktop",
            "package_manifest_sha256": "a" * 64, "vs_channel_build": "17.14.37806.7",
            "resolved_components": {"CRT.Headers": "14.44.35220", "Win11SDK_10.0.26100": "10.0.26100.15"},
            "cache_tree_sha256": "b" * 64,
        }
        self.config = {"unrelated": {"keep": True}, "windows_cross": self.pins}
        self.original = json.dumps(self.config, indent=2) + "\n"
        self.path.write_text(self.original)
        self.package = {
            "info": {"buildVersion": "17.14.99999.1"},
            "packages": [{"id": name, "version": version}
                         for name, version in self.pins["resolved_components"].items()],
        }
        self.build = Mock(return_value="b" * 64)

    def run_repin(self):
        package = json.dumps(self.package).encode()
        with contextlib.redirect_stdout(io.StringIO()) as output:
            REPIN.repin(self.root, fetch=lambda pins: package, build=self.build)
        return package, output.getvalue()

    def test_changed_component_version_refuses_without_writing_or_building(self):
        self.package["packages"][0]["version"] = "14.44.99999"
        with self.assertRaisesRegex(ValueError, "CRT.Headers: expected 14.44.35220, got.*14.44.99999"):
            self.run_repin()
        self.assertEqual(self.path.read_text(), self.original)
        self.build.assert_not_called()

    def test_changed_tree_hash_refuses_without_writing(self):
        self.build.return_value = "c" * 64
        with self.assertRaisesRegex(ValueError, f"SDK tree changed: expected {'b' * 64}, got {'c' * 64}"):
            self.run_repin()
        self.assertEqual(self.path.read_text(), self.original)
        self.build.assert_called_once()

    def test_digest_only_change_passes_and_changes_only_metadata(self):
        package, output = self.run_repin()
        expected = json.loads(self.original)
        expected["windows_cross"].update(
            package_manifest_sha256=hashlib.sha256(package).hexdigest(),
            vs_channel_build="17.14.99999.1",
        )
        self.assertEqual(json.loads(self.path.read_text()), expected)
        self.build.assert_called_once_with(self.root, expected)
        self.assertIn("vs_channel_build: 17.14.37806.7 -> 17.14.99999.1", output)
        self.assertIn(f"Unchanged SDK tree SHA256={'b' * 64}", output)


if __name__ == "__main__":
    unittest.main()
