#!/usr/bin/env python3
"""Qualification must own the only path that reopens fleet admission."""
from pathlib import Path
import unittest

HERE = Path(__file__).resolve().parent


class AdmissionGateTests(unittest.TestCase):
    def test_install_and_update_close_gate_before_service_start(self):
        install = (HERE / "install.ps1").read_text()
        update = (HERE / "update-functions.ps1").read_text()
        for script in (install, update):
            self.assertIn("Close-RunnerQualificationGate $Root", script)
            self.assertLess(script.index("Close-RunnerQualificationGate $Root"), script.index("Start-Service"))
            self.assertNotIn("Open-RunnerQualificationGate $Root", script)
        self.assertLess(install.index("Close-RunnerQualificationGate $Root"), install.index("New-Service -Name MotorRunner"))
        self.assertLess(update.index("Close-RunnerQualificationGate $Root"), update.index('Move-Item -LiteralPath "$next\\bin"'))

    def test_every_update_start_requires_gate_capability_including_rollback(self):
        update = (HERE / "update-functions.ps1").read_text()
        starts = update.split("Start-Service -Name MotorRunner")
        self.assertEqual(len(starts), 3)
        for prefix in starts[:-1]:
            self.assertTrue(prefix.rstrip().endswith("Assert-RunnerQualificationCapable $Root"))
        helpers = (HERE / "service-definition.ps1").read_text()
        self.assertIn("qualification-gate-version", helpers)
        self.assertIn("-cne 'qualification-gate-v1'", helpers)
        self.assertIn("MotorRunner left stopped", helpers)
        self.assertIn("Start-Service -Name MotorRunner", helpers)

    def test_only_full_success_opens_gate_after_all_checks_and_evidence(self):
        qualify = (HERE / "qualify.ps1").read_text()
        close = qualify.index("Close-RunnerQualificationGate $Root")
        self.assertLess(close, qualify.index("if ($OfflineCargoOnly) {"))
        self.assertLess(close, qualify.index("if ($LocalRegistryOnly) {"))
        self.assertEqual(qualify.count("Open-RunnerQualificationGate $Root"), 1)
        opening = qualify.index("Open-RunnerQualificationGate $Root")
        for check in ("PASS final-nothing-left-behind", "QUALIFIED-SINGLE-SLOT.txt", "$evidenceCopied = $true"):
            self.assertLess(qualify.index(check), opening)
        failure = qualify.split("$failure = $_", 1)[1]
        self.assertIn("Close-RunnerQualificationGate $Root", failure)
        self.assertNotIn("Open-RunnerQualificationGate", failure)
        self.assertIn("Failed or partial qualification keeps fleet admission closed", qualify)
        system = (HERE / "run-system.ps1").read_text()
        self.assertIn("fleet admission remains closed", system)
        self.assertIn("resume --instance ally-qualification", system)

    def test_marker_is_flushed_and_resume_uses_the_protected_endpoint(self):
        helpers = (HERE / "service-definition.ps1").read_text()
        self.assertIn('Assert-RunnerPrivateDirectory "$Root\\state\\ally"', helpers)
        self.assertIn('"$Root\\state\\ally\\qualification-required"', helpers)
        self.assertIn("$marker.Flush($true)", helpers)
        self.assertIn('resume --instance ally-qualification --state-root "$Root\\state"', helpers)
        jobs = (HERE / "qualify-jobs.py").read_text()
        self.assertIn('"connect", "--instance", "ally-qualification"', jobs)


if __name__ == "__main__":
    unittest.main()
