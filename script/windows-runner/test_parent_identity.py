#!/usr/bin/env python3
"""Portable structural controls for the native parent-identity fixtures."""
from pathlib import Path
import unittest

HERE = Path(__file__).resolve().parent


class ParentVerificationTests(unittest.TestCase):
    def test_process_parent_guard_is_held_across_control_calls(self):
        windows = HERE.parents[1] / "crates/motor-runner/src/windows"
        host = (windows / "hyperv_host.rs").read_text()
        self.assertLess(host.index(".share_mode(1)"), host.index("checked_validation("))
        self.assertLess(host.index("checked_validation("), host.index("*lifetime = Some((parent, file))"))
        self.assertIn("parent_guard: Mutex<Option<(PathBuf, fs::File)>>", (windows / "agent.rs").read_text())

    def test_startup_accepts_matching_record_before_hashing(self):
        helper = (HERE / "parent-identity.ps1").read_text()
        startup = helper.split("function Confirm-RunnerParentVerification", 1)[1]
        self.assertLess(startup.index("Open-RunnerVerifiedParent"), startup.index("Write-RunnerParentVerification"))
        script = (HERE / "hyperv.ps1").read_text()
        validate = script.split("    'Validate' {", 1)[1].split("    { $_ -in", 1)[0]
        self.assertIn("Confirm-RunnerParentVerification", validate)
        self.assertNotIn("Remove-Item", validate)

    def test_startup_validation_has_a_separate_fifteen_minute_budget(self):
        windows = HERE.parents[1] / "crates/motor-runner/src/windows"
        self.assertIn("checked_validation(", (windows / "hyperv_host.rs").read_text())
        control = (windows / "hyperv.rs").read_text().split("pub(super) fn checked_validation", 1)[1].split("fn checked_setup", 1)[0]
        self.assertIn("Duration::from_secs(15 * 60)", control)
        native = (windows.parent / "native_process.rs").read_text()
        self.assertIn("output_with_timeout(command, Duration::from_secs(120), cancelled)", native)
        self.assertIn("WaitForExit(180000)", (HERE / "qualify.ps1").read_text())

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
