#!/usr/bin/env python3
"""Portable source-level checks for the operator-only connection diagnostic."""
from pathlib import Path
import re
import unittest

HERE = Path(__file__).resolve().parent


class ConnectDiagnosticTests(unittest.TestCase):
    def setUp(self):
        self.script = (HERE / "diagnose-connect.ps1").read_text()

    def test_diagnostic_uses_installed_production_prepare(self):
        self.assertIn('"$Root\\bin\\hyperv.ps1" -Operation Prepare', self.script)
        self.assertIn('-Cache $cache -Nonce $nonce', self.script)
        self.assertIn("$nonce = ([Guid]$job).ToString('N')", self.script)
        self.assertNotIn("New-VM ", self.script)
        self.assertNotIn("New-VHD ", self.script)

    def test_shared_guest_collector_matches_standalone_snapshot(self):
        embedded = self.script.split("function Save-Json(", 1)[1].split("function Save-HostVmEvents(", 1)[0]
        shared = (HERE / "guest-evidence.ps1").read_text().split("function Save-Json(", 1)[1]
        self.assertEqual(embedded.strip(), shared.strip())
        writer = shared.split("function Get-PlainGuestItem(", 1)[0]
        self.assertIn("[Text.UTF8Encoding]::new($false)", writer)
        self.assertNotIn("Set-Content", writer)

    def test_diagnostic_harvests_stopped_child_before_remove(self):
        cleanup = self.script.split("# Harvest the stopped child before Remove", 1)[1]
        self.assertLess(cleanup.index("Stop-VM"), cleanup.index("Save-DiagnosticGuestEvidence"))
        self.assertLess(cleanup.index("Save-DiagnosticGuestEvidence"), cleanup.index("-Operation Remove"))
        self.assertLess(cleanup.index("-Operation Remove"), cleanup.index("$parentGuard.Dispose()"))
        self.assertIn("Mount-VHD -Path $Path -ReadOnly -Passthru", self.script)
        mount = self.script.split("function Use-DiagnosticWindowsDisk", 1)[1].split("function Save-DiagnosticGuestEvidence", 1)[0]
        self.assertLess(mount.index("$mounted = $true"), mount.index("$disk = $image | Get-Disk"))
        self.assertIn("ReparsePoint", self.script)

    def test_probe_endpoint_and_timeout_match_host_transport(self):
        source = (HERE.parents[1] / "crates/motor-winagent/src/transport.rs").read_text()
        service = re.search(r'HVSOCK_SERVICE_ID: &str = "([^"]+)"', source)
        attempt = re.search(r'HVSOCK_CONNECT_ATTEMPT_MS: u32 = (\d+)', source)
        assert service is not None and attempt is not None
        self.assertIn(service.group(1), self.script)
        self.assertIn(f"connect_attempt_ms={attempt.group(1)}", self.script)
        self.assertIn("socket(34, 1, 1)", self.script)
        self.assertIn("setsockopt(handle, 1, 1, ref timeoutMs, 4)", self.script)
        self.assertIn("$clock.Elapsed.TotalSeconds -lt 120", self.script)
        self.assertNotIn("extern int send(", self.script)
        self.assertNotIn("BitConverter.ToUInt32", self.script)

    def test_guest_event_export_is_service_filtered_and_preserves_xml(self):
        self.assertIn("motor-winagent(?:\\.exe)?|Motor Windows job agent", self.script)
        self.assertIn('"EventRecordID=$($_.record_id)"', self.script)
        self.assertIn("xml=$Event.ToXml()", self.script)
        self.assertIn("@('System','Application')", self.script)


if __name__ == "__main__":
    unittest.main()
