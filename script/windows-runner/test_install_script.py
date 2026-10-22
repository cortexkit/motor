#!/usr/bin/env python3
"""Static checks on the Windows installer that can't wait for a Windows host."""
import re
from pathlib import Path
import unittest

HERE = Path(__file__).resolve().parent

# New-LocalUser rejects a -Description longer than 48 characters, and the
# installer only discovers that after it has created its root, so an over-long
# literal leaves a half-finished install on the host.
LOCAL_USER_DESCRIPTION_MAX = 48


class InstallScriptTests(unittest.TestCase):
    def test_installer_rejects_system_before_modifying_host(self):
        script = (HERE / "install.ps1").read_text()
        reject_system = script.index("if ($identity.User.Value -eq 'S-1-5-18')")
        self.assertLess(reject_system, script.index("New-Item $Root"))
        self.assertIn("elevated administrator shell, not a SYSTEM scheduled task", script)
        owner_failure = next(line for line in script.splitlines() if "Cannot set trusted parent owner" in line)
        self.assertIn("elevated administrator shell", owner_failure)

    def test_local_user_descriptions_fit_windows_limit(self):
        found = 0
        for script in sorted(HERE.glob("*.ps1")):
            for match in re.finditer(r"New-LocalUser\b[^\n]*?-Description\s+'([^']*)'", script.read_text()):
                found += 1
                self.assertLessEqual(len(match.group(1)), LOCAL_USER_DESCRIPTION_MAX,
                                     f"{script.name}: {match.group(1)!r}")
        self.assertGreater(found, 0, "no New-LocalUser -Description literal found to check")

    def test_installer_pins_guest_agent_hash_from_sealed_provisioning_record(self):
        install = (HERE / "install.ps1").read_text()
        identity = (HERE / "guest-agent-identity.ps1").read_text()
        provision = (HERE / ".." / "windows-vm" / "setup-job-baseline.ps1").read_text()
        self.assertIn('. "$PSScriptRoot\\guest-agent-identity.ps1"', install)
        self.assertIn("Get-RunnerSealedAgentHash $parent", install)
        self.assertIn("guest_agent_sha256 = '$parentAgentHash'", install)
        self.assertIn("build\\provision\\winagent.sha256", identity)
        self.assertIn("Use-DiagnosticWindowsDisk -Path $Parent", identity)
        self.assertIn(r"\A([0-9A-Fa-f]{64})\s+motor-winagent\.exe\z", identity)
        self.assertIn("if (-not $match.Success) { return $null }", identity)
        self.assertIn("return $match.Groups[1].Value.ToUpperInvariant()", identity)
        self.assertIn("Get-RunnerProvisionedAgentHash ([IO.File]::ReadAllText($record.FullName))", identity)
        self.assertNotIn("ProgramData\\Motor\\motor-winagent.exe", identity)
        self.assertIn("$agentHash  motor-winagent.exe", provision)
        fixture = (HERE / "test_scripts.ps1").read_text()
        self.assertIn("parent-agent-record-match-mismatch-missing", fixture)
        self.assertIn("Get-RunnerProvisionedAgentHash $null", fixture)

    def test_service_refuses_mismatch_and_reports_readiness(self):
        root = HERE.parents[1] / "crates/motor-runner/src"
        host = (root / "windows/hyperv_host.rs").read_text()
        runner = (root / "engine.rs").read_text()
        status = (root / "disk.rs").read_text()
        protocol = (HERE.parents[1] / "crates/motor-protocol/src/types.rs").read_text()
        self.assertIn("vm.guest_agent_sha256.as_deref()", host)
        self.assertIn('join("bin/motor-winagent.exe")', host)
        self.assertIn("set_guest_agent_readiness(guest_agent_readiness)", host)
        self.assertIn("guest_agent_refusal(", runner)
        self.assertIn("windows_guest_agent: self.core.config.windows_runtime.guest_agent_readiness()", status)
        self.assertIn("pub windows_guest_agent: Option<WindowsGuestAgentReadiness>", protocol)
        qualification = (HERE / "qualify-jobs.py").read_text()
        self.assertIn("self.request_status(hello_timeout)", qualification)
        self.assertIn("require_windows_guest_agent_ready", qualification)

    def test_cmd_never_echoes_a_variable_straight_into_a_redirect(self):
        # In cmd, `echo %errorlevel%>file` with a one-digit value reads as a
        # handle redirect (`0>`), so the file gets "ECHO is off." instead of
        # the code. Put the redirect first: `>file echo %errorlevel%`.
        for script in sorted(HERE.glob("*.ps1")):
            for number, line in enumerate(script.read_text().splitlines(), 1):
                self.assertIsNone(re.search(r"(?i)\becho\s+%\w+%\s*>", line), f"{script.name}:{number}: {line.strip()}")


if __name__ == "__main__":
    unittest.main()
