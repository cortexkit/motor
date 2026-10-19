#!/usr/bin/env python3
"""Portable recipe integrity tests, not proof of a functioning Boat host."""
import copy
import importlib.util
import json
from pathlib import Path
import tempfile
import unittest

HERE = Path(__file__).resolve().parent
spec = importlib.util.spec_from_file_location("boat_image_install", HERE / "install.py")
if spec is None or spec.loader is None:
    raise ImportError("cannot load Boat recipe")
install = importlib.util.module_from_spec(spec)
spec.loader.exec_module(install)
start_spec = importlib.util.spec_from_file_location("boat_start", HERE / "motor-boat-start.py")
if start_spec is None or start_spec.loader is None:
    raise ImportError("cannot load Boat startup budget helper")
start = importlib.util.module_from_spec(start_spec)
start_spec.loader.exec_module(start)


class RecipeTests(unittest.TestCase):
    def setUp(self):
        self.pins = json.loads((HERE / "pins.json").read_text())

    def test_pins_match_real_runner_inputs(self):
        install.validate(self.pins)
        self.assertEqual(self.pins["nats"], "2.15.0")
        self.assertIn("libsqlite3-dev", self.pins["packages"])
        self.assertIn("python3", self.pins["packages"])
        self.assertEqual(set(self.pins["rust_toolchains"]), {"1.89.0", "1.91.0", "1.96.0", "1.99.0"})

    def test_wrong_input_hash_is_refused(self):
        pins = copy.deepcopy(self.pins)
        pins["runner_inputs_sha256"]["script/runner/tools.txt"] = "0" * 64
        with self.assertRaisesRegex(ValueError, "input changed"):
            install.validate(pins)

    def test_unpinned_versions_and_endpoint_port_are_refused(self):
        for key, value in (("bun", "latest"), ("probe_port", 9443)):
            with self.subTest(key=key), self.assertRaises(ValueError):
                install.validate(dict(self.pins, **{key: value}))

    def test_checksum_parser_rejects_missing_ambiguous_and_malformed(self):
        sha = "a" * 64
        self.assertEqual(install.checksum(f"{sha} *file.tgz\n", "file.tgz"), sha)
        for text in ("bad file.tgz", f"{sha} other", f"{sha} file.tgz\n{sha} file.tgz"):
            with self.subTest(text=text), self.assertRaises(ValueError):
                install.checksum(text, "file.tgz")

    def test_download_verifies_bytes_not_filename(self):
        with tempfile.TemporaryDirectory() as tmp:
            source, target = Path(tmp, "source"), Path(tmp, "target")
            source.write_bytes(b"hello")
            expected = "2cf24dba5fb0a30e26e83b2ac5b9e29e1b161e5c1fa7425e73043362938b9824"
            self.assertEqual(install.digest(source), expected)
            install.download(source.as_uri(), target, expected)
            self.assertEqual(target.read_bytes(), b"hello")
            with self.assertRaisesRegex(ValueError, "mismatch"):
                install.download(source.as_uri(), target, "0" * 64)

    def test_host_unit_has_explicit_boat_profile_and_no_enabled_bridge(self):
        unit = (HERE / "motor-boat.service").read_text()
        self.assertIn("--boat-profile", unit)
        self.assertIn("--max-weight @VCPUS@", unit)
        self.assertIn("--job-network veth", unit)
        self.assertIn("User=motor-boat", unit)
        self.assertNotIn("[Install]", (HERE / "motor-boat-bridge.service").read_text())
        self.assertEqual(self.pins["probe_port"], 19443)

    def test_memory_budget_subtracts_measured_idle_and_margin(self):
        self.assertEqual(start.budget(16*(1<<30), 1<<30), 15569256448)
        self.assertEqual(start.budget(16*(1<<30)+123, 1<<30), 15569256448)
        with self.assertRaises(ValueError):
            start.budget(1<<30, 1<<29)


if __name__ == "__main__":
    unittest.main()
