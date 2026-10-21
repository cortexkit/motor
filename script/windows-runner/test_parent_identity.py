#!/usr/bin/env python3
"""Portable structural controls for the native parent-identity fixtures."""
from pathlib import Path
import unittest

HERE = Path(__file__).resolve().parent


class ParentVerificationTests(unittest.TestCase):
    def test_prepare_uses_guard_before_creating_job_resources(self):
        script = (HERE / "hyperv.ps1").read_text()
        prepare = script.split("    'Prepare' {", 1)[1].split("    'Id'", 1)[0]
        # This also provides a fast red control for the former per-job digest.
        self.assertNotIn("Get-FileHash", prepare)
        guard = prepare.index("$parentGuard = Open-RunnerVerifiedParent $Parent $ParentSha256 $parentRecord")
        self.assertLess(guard, prepare.index("New-VHD"))
        self.assertIn("finally { $parentGuard.Dispose() }", prepare)

    def test_run_guards_before_start_and_releases_on_failure(self):
        script = (HERE / "hyperv.ps1").read_text()
        run = script.split("    'Run' {", 1)[1].split("    'Harvest'", 1)[0]
        self.assertNotIn("Get-FileHash", run)
        self.assertLess(run.index("Open-RunnerVerifiedParent"), run.index("Start-VM"))
        self.assertIn("finally { if ($parentGuard) { $parentGuard.Dispose() } }", run)

    def test_installer_deploys_identity_helper(self):
        self.assertIn("'parent-identity.ps1'", (HERE / "install.ps1").read_text())


if __name__ == "__main__":
    unittest.main()
