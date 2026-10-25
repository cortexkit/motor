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

    def test_diagnostic_uses_shared_guid_path_guest_collector(self):
        self.assertIn('. "$PSScriptRoot\\guest-evidence.ps1"', self.script)
        self.assertNotIn("function Get-PlainGuestItem(", self.script)
        shared = (HERE / "guest-evidence.ps1").read_text()
        self.assertIn("function Get-PlainGuestItem(", shared)
        self.assertIn("[IO.File]::GetAttributes($path)", shared)
        self.assertIn("[IO.Path]::Combine($path, $part)", shared)
        self.assertNotIn("Join-Path $path $part", shared)
        writer = shared.split("function Get-PlainGuestItem(", 1)[0]
        self.assertIn("[Text.UTF8Encoding]::new($false)", writer)
        self.assertNotIn("Set-Content", writer)

    def test_diagnostic_harvests_stopped_child_before_remove(self):
        cleanup = self.script.split("# Harvest the stopped child before Remove", 1)[1]
        self.assertLess(cleanup.index("Stop-VM"), cleanup.index("Save-DiagnosticGuestEvidence"))
        self.assertLess(cleanup.index("Save-DiagnosticGuestEvidence"), cleanup.index("-Operation Remove"))
        self.assertLess(cleanup.index("-Operation Remove"), cleanup.index("$parentGuard.Dispose()"))
        shared = (HERE / "guest-evidence.ps1").read_text()
        self.assertIn("Mount-VHD -Path $Path -ReadOnly -Passthru", shared)
        mount = shared.split("function Use-DiagnosticWindowsDisk", 1)[1].split("function Save-DiagnosticGuestEvidence", 1)[0]
        self.assertLess(mount.index("$mounted = $true"), mount.index("$disk = $image | Get-Disk"))
        wait = shared.split("function Wait-RunnerDiagnosticVolume", 1)[1].split("function Use-DiagnosticWindowsDisk", 1)[0]
        self.assertIn("$rawAccessPaths = Get-RunnerPropertyValue $partition 'AccessPaths'", wait)
        self.assertIn("$guidPaths = @($accessPaths | Where-Object", wait)
        self.assertIn("if ($guidPaths.Count -eq 1)", wait)
        self.assertIn("Test-RunnerVolumeGuidPath", wait)
        self.assertNotIn("Get-Volume -Partition", shared)
        self.assertNotIn("Get-Volume -DriveLetter", shared)
        self.assertNotIn("| Get-Volume", shared)
        self.assertNotIn("Get-Partition -Volume", shared)
        self.assertNotIn("Add-PartitionAccessPath", mount)
        self.assertNotIn("Remove-PartitionAccessPath", mount)
        self.assertIn("ReparsePoint", shared)

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
        shared = (HERE / "guest-evidence.ps1").read_text()
        self.assertIn("motor-winagent(?:\\.exe)?|Motor Windows job agent", shared)
        self.assertIn('"EventRecordID=$($_.record_id)"', shared)
        self.assertIn("xml=$Event.ToXml()", shared)
        self.assertIn("@('System','Application')", shared)

    def test_hyperv_spike_vhd_reads_use_guid_paths(self):
        spike = HERE.parent / "hyperv-spike"
        common = (spike / "common.ps1").read_text()
        disk = common.split("function Use-SpikeWindowsDisk", 1)[1].split("function New-SpikeIso", 1)[0]
        self.assertIn("Get-Volume", disk)
        self.assertIn("Test-SpikeVolumeGuidPath", disk)
        self.assertNotIn("Add-PartitionAccessPath", disk)
        self.assertNotIn("Test-Path", disk)
        self.assertIn("[IO.Path]::Combine($path, $component)", common)
        self.assertIn("[IO.FileAttributes]::ReparsePoint", common)

        evidence = (spike / "diagnostics.ps1").read_text()
        self.assertIn("[IO.File]::Copy($item.FullName", evidence)
        self.assertIn("Get-SpikeFileSha256", evidence)
        self.assertNotIn("Get-FileHash -LiteralPath $item.FullName", evidence)
        self.assertNotIn("Get-WinEvent -Path $path", evidence)

        measure = (spike / "measure.ps1").read_text()
        self.assertIn("[IO.FileInfo]::new($agent.FullName).GetAccessControl()", measure)
        self.assertIn("Get-SpikeFileSha256 $agent.FullName", measure)
        self.assertNotIn("Get-Acl $nonce", measure)

        boot = (spike / "boot-files.ps1").read_text()
        self.assertIn("Get-Volume -Partition $partition", boot)
        self.assertIn("$partition.VolumeGuidPath", boot)
        self.assertIn("Get-SpikeBcdBootArguments $Windows $Letter", boot)
        self.assertIn("Remove-SpikeDiskDriveLetters $disk.Number", boot)

        provision = (spike / "provision.ps1").read_text()
        self.assertIn("Get-Volume", provision)
        self.assertIn("[IO.File]::Copy", provision)
        self.assertIn("Remove-SpikeDiskDriveLetters $virtualDisk.Number", provision)
        self.assertIn('dism.exe /Apply-Image', provision)


if __name__ == "__main__":
    unittest.main()
