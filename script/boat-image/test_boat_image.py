#!/usr/bin/env python3
"""Portable recipe integrity tests, not proof of a functioning Boat host."""
import copy
import importlib.util
import json
from pathlib import Path
import tempfile
import unittest
from unittest.mock import patch, Mock

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
        self.assertIn("User=root", unit)
        self.assertIn('--tenant-vm --tenant-socket-dir /run/motor-tenant', unit)
        self.assertIn('RuntimeDirectoryMode=0700', unit)
        self.assertNotIn("[Install]", (HERE / "motor-boat-bridge.service").read_text())
        self.assertEqual(self.pins["probe_port"], 19443)

    def test_target_directories_exist_before_installing_binaries_or_units(self):
        with patch.object(install, 'run') as run:
            install.target_directories()
        self.assertEqual(run.call_args.args[:8], ('install','-d','-o','root','-g','root','-m','0755'))
        self.assertTrue({'/usr/local/bin','/usr/local/libexec','/etc/systemd/system','/etc/ssh/sshd_config.d','/etc/motor','/etc/motor/tenant'} <= set(run.call_args.args[8:]))

    def test_admission_endpoint_waits_for_identity_without_baking_it_into_image(self):
        unit = (HERE / 'motor-boat-host.service').read_text()
        path = (HERE / 'motor-boat-host.path').read_text()
        self.assertIn('ConditionPathExists=/etc/motor/tenant/id', unit)
        self.assertIn('motor-host --sandbox /etc/motor/sandbox.json', unit)
        self.assertIn('PathExists=/etc/motor/tenant/id', path)

    def test_memory_budget_subtracts_measured_idle_and_margin(self):
        self.assertEqual(start.budget(16*(1<<30), 1<<30), 15569256448)
        self.assertEqual(start.budget(16*(1<<30)+123, 1<<30), 15569256448)
        with self.assertRaises(ValueError):
            start.budget(1<<30, 1<<29)

    def test_apt_index_failure_is_fatal_and_transient_retry_is_bounded(self):
        failed = Mock(returncode=100, stdout="", stderr="503 Service Unavailable")
        with patch.object(install.subprocess, 'run', return_value=failed) as run:
            pauses = []
            with self.assertRaisesRegex(RuntimeError, "refusing package installation"):
                install.apt_update(pause=pauses.append)
            self.assertEqual(run.call_count, 3)
            self.assertEqual(pauses, [1, 2])
            self.assertIn('APT::Update::Error-Mode=any', run.call_args.args[0])
        failed.stderr = 'signature verification failed'
        with patch.object(install.subprocess, 'run', return_value=failed) as run:
            with self.assertRaises(RuntimeError):
                install.apt_update(pause=lambda _: None)
            self.assertEqual(run.call_count, 1)

    def test_archive_preflight_retries_official_indexes_only(self):
        calls = []
        def fetch(url, timeout):
            calls.append(url)
            if 'security.ubuntu.com' in url:
                raise install.urllib.error.HTTPError(url, 503, 'unavailable', {}, None)
            response = Mock(status=200)
            response.read.return_value = b'-----BEGIN PGP SIGNED MESSAGE-----\n'
            response.__enter__ = Mock(return_value=response)
            response.__exit__ = Mock(return_value=False)
            return response
        evidence = install.archive_preflight(self.pins, fetch=fetch, pause=lambda _: None)
        self.assertEqual(evidence[0]['attempts'][0]['status'], 200)
        self.assertEqual(len(evidence[2]['attempts']), 3)
        self.assertEqual(evidence[2]['attempts'][-1]['status'], 503)
        self.assertTrue(all(url.startswith(('https://archive.ubuntu.com/', 'https://security.ubuntu.com/')) for url in calls))

    def test_unsigned_or_third_party_sources_and_missing_release_dates_refuse(self):
        self.assertIn('Dir::Etc::sourcelist=/dev/null', install.APT_OPTIONS)
        self.assertIn('Dir::Etc::sourceparts=/etc/apt/motor-boat.sources.d', install.APT_OPTIONS)
        self.assertIn('Acquire::AllowInsecureRepositories=false', install.APT_OPTIONS)
        self.assertIn('APT::Get::AllowUnauthenticated=false', install.APT_OPTIONS)
        with self.assertRaises(ValueError):
            install.validate(dict(self.pins, ubuntu_archive='https://untrusted.invalid/ubuntu'))
        with tempfile.TemporaryDirectory() as tmp:
            root = Path(tmp)
            for suite in ('noble', 'noble-updates', 'noble-security'):
                (root / (suite + '_InRelease')).write_text('-----BEGIN PGP SIGNED MESSAGE-----\nDate: Wed, 01 Jan 2025 12:00:00 UTC\n')
            manifest = install.release_manifest(root)
            self.assertEqual(len(manifest), 3)
            self.assertEqual(manifest['noble_InRelease']['date'], 'Wed, 01 Jan 2025 12:00:00 UTC')
            (root / 'noble_InRelease').write_text('Date: Wed, 01 Jan 2025 12:00:00 UTC\n')
            with self.assertRaises(ValueError):
                install.release_manifest(root)


if __name__ == "__main__":
    unittest.main()
