#!/usr/bin/env python3
"""Portable seed checks; none of these claim to have booted Windows."""
import importlib.util
from pathlib import Path
import tempfile
import unittest
from unittest.mock import patch
import xml.etree.ElementTree as ET

HERE = Path(__file__).resolve().parent
spec = importlib.util.spec_from_file_location("hyperv_seed", HERE / "seed.py")
assert spec is not None and spec.loader is not None
seed = importlib.util.module_from_spec(spec)
spec.loader.exec_module(seed)


class HyperVSeedTests(unittest.TestCase):
    def test_only_windows_pe_and_seed_drive_discovery_change_in_answer(self):
        template = (seed.SETUP / "autounattend.xml").read_text()
        expected = ET.fromstring(seed.original.job_answer(template.replace("__BOOTSTRAP_PASSWORD__", "Aa1!test")))
        windows_pe = expected.find(f"{seed.TAG('settings')}[@pass='windowsPE']")
        assert windows_pe is not None
        expected.remove(windows_pe)
        actual = ET.fromstring(seed.answer(template, "Aa1!test"))
        path = f"{seed.TAG('settings')}[@pass='specialize']/{seed.TAG('component')}[@name='Microsoft-Windows-Deployment']/{seed.TAG('RunSynchronous')}/{seed.TAG('RunSynchronousCommand')}/{seed.TAG('Path')}"
        actual_command = actual.find(path)
        expected_command = expected.find(path)
        assert actual_command is not None and expected_command is not None
        command = actual_command.text
        assert command is not None
        self.assertIn("for %d in (D E F", command)
        self.assertIn("-File %d:\\register-job-bootstrap.ps1", command)
        actual_command.text = expected_command.text
        self.assertEqual(ET.tostring(expected), ET.tostring(actual))
        self.assertNotIn("__BOOTSTRAP_PASSWORD__", seed.answer(template, "Aa1!test"))

    def test_template_drift_fails_before_rendering_password(self):
        with self.assertRaisesRegex(ValueError, "Unexpected unattended template"):
            seed.answer("<unattend/>", "Aa1!test")

    def test_stage_copies_same_hardened_scripts_and_feature_source(self):
        with tempfile.TemporaryDirectory() as tmp:
            root = Path(tmp)
            archive = root / seed.ARCHIVE_NAME
            archive.write_bytes(b"test-only archive")
            target = root / "seed"
            with patch.object(seed, "verify_archive") as verify:
                seed.stage(target, archive)
                self.assertEqual(verify.call_count, 2)
            self.assertTrue((target / "job-baseline").is_file())
            self.assertTrue((target / "hyperv-spike").is_file())
            self.assertFalse((target / "authorized_keys").exists())
            for name in ("bootstrap.ps1", "setup-guest.ps1", "setup-job-baseline.ps1", "install-powershell.ps1", "register-job-bootstrap.ps1", "finalize-job-baseline.ps1"):
                self.assertEqual((target / name).read_bytes(), (seed.SETUP / name).read_bytes(), name)
            self.assertTrue((target / "motor-winagent/examples/hyperv-spike.rs").is_file())
            self.assertEqual((target / "motor-winagent/Cargo.lock").read_bytes(), (seed.REPO / "crates/motor-winagent/Cargo.lock").read_bytes())
            with self.assertRaisesRegex(ValueError, "already exists"):
                seed.stage(target, archive)

    def test_invalid_archive_never_creates_seed(self):
        with tempfile.TemporaryDirectory() as tmp:
            target = Path(tmp) / "seed"
            with patch.object(seed, "verify_archive", side_effect=ValueError("bad SHA256")):
                with self.assertRaisesRegex(ValueError, "SHA256"):
                    seed.stage(target, Path(tmp) / "missing.zip")
            self.assertFalse(target.exists())

    def test_prebuilt_runner_seed_installs_artifacts_not_agent_sources(self):
        with tempfile.TemporaryDirectory() as tmp:
            root = Path(tmp)
            archive = root / seed.ARCHIVE_NAME
            archive.write_bytes(b"test archive")
            agent = root / "agent.exe"
            runner = root / "runner.exe"
            agent.write_bytes(b"MZ prebuilt hvsock agent")
            runner.write_bytes(b"MZ prebuilt peer runner")
            target = root / "seed"
            with patch.object(seed, "verify_archive"):
                seed.stage(target, archive, agent, runner)
            self.assertEqual((target / "motor-winagent-prebuilt.exe").read_bytes(), agent.read_bytes())
            self.assertEqual((target / "motor-runner-prebuilt.exe").read_bytes(), runner.read_bytes())
            self.assertTrue((target / "hyperv-runner").is_file())
            self.assertFalse((target / "motor-winagent").exists())
            self.assertFalse((target / "authorized_keys").exists())

    def test_partial_prebuilt_pair_is_refused_before_seed_creation(self):
        with tempfile.TemporaryDirectory() as tmp:
            root = Path(tmp)
            target = root / "seed"
            with self.assertRaisesRegex(ValueError, "together"):
                seed.stage(target, root / "unused.zip", root / "agent.exe")
            self.assertFalse(target.exists())


if __name__ == "__main__":
    unittest.main(verbosity=2)
