#!/usr/bin/env python3
"""Portable structural controls for the native parent-identity fixtures."""
from pathlib import Path
import unittest

HERE = Path(__file__).resolve().parent


class ParentVerificationTests(unittest.TestCase):
    def test_changetime_fixture_uses_unique_file_and_bounded_metadata_flips(self):
        fixture = (HERE / "test_scripts.ps1").read_text()
        self.assertIn("'motor-parent-tests-' + [Guid]::NewGuid()", fixture)
        self.assertIn("$Parent = Join-Path $fixture 'parent.vhdx'", fixture)
        change = fixture.split("# The parent is fresh under a GUID-named directory", 1)[1].split("# Without a lifetime guard", 1)[0]
        self.assertIn("$originalAttributes = [IO.File]::GetAttributes($Parent)", change)
        self.assertIn("-bxor [int][IO.FileAttributes]::Hidden", change)
        self.assertLess(change.index("SetAttributes($Parent, $flippedAttributes)"), change.index("SetAttributes($Parent, $originalAttributes)"))
        self.assertIn("$changeClock.ElapsedMilliseconds -lt 2000", change)
        self.assertIn("Start-Sleep -Milliseconds 25", change)
        self.assertIn("$changeAfter = [Motor.ParentFile]::ChangeTime($lifetimeGuard)", change)
        self.assertIn("$changeAfter -le $changeBefore", change)
        self.assertIn("finally {", change)
        self.assertIn("Set-Acl -LiteralPath $Parent -AclObject $readAcl", change)
        self.assertIn("foreach ($key in $before.Keys)", change)
        self.assertIn("Open-RunnerVerifiedParent $Parent $ParentSha256 $record", change)

    def test_guest_collection_asserts_child_attachment_and_mounted_volume(self):
        script = (HERE / "hyperv.ps1").read_text()
        binding = script.split("function Assert-JobDiskBinding(", 1)[1].split("function Harvest-Job(", 1)[0]
        self.assertIn("Get-VMHardDiskDrive -VM $vm", binding)
        self.assertIn("OrdinalIgnoreCase.Equals($expected, $actual)", binding)
        remove = script.split("function Remove-Job(", 1)[1].split("function Grant(", 1)[0]
        self.assertLess(remove.index("Assert-JobDiskBinding $Id"), remove.index("Remove-VM"))
        collector = (HERE / "guest-evidence.ps1").read_text()
        self.assertIn("Get-VHD -DiskNumber $disk.Number", collector)
        self.assertIn("Get-Partition -DriveLetter $partition.DriveLetter", collector)
        self.assertIn("$reverse[0].DiskNumber -ne $disk.Number", collector)
        self.assertIn("mounted-disk-identity.json", collector)
        self.assertNotIn("New-VHD", collector)

    def test_qualification_settle_is_bounded_and_offline_proof_remains_required(self):
        native = (HERE.parents[1] / "crates/motor-runner/src/windows/hyperv.rs").read_text()
        self.assertIn("Qualification-only timing heuristic", native)
        self.assertIn("while dropped.elapsed() < Duration::from_secs(60)", native)
        self.assertIn("GetProcessIoCounters", native)
        settle = native.split("fn qualification_settle(", 1)[1].split("fn verify_qualification_guest(", 1)[0]
        self.assertNotIn("break;", settle)
        self.assertLess(native.index("qualification_settle(config, id, worker_pid, dropped)?"), native.index("verify_qualification_guest(config, id, &nonce, frame.len(), written, dropped)?"))
        self.assertIn("super::agent::qualification_disconnect_proof", native)
        self.assertIn("guest-terminal-read-verified", native)
        self.assertIn("first-safe-host-observation-after-power-off-not-guest-write-time", native)

    def test_peer_targets_avoid_powershell_51_array_serialization(self):
        script = (HERE / "hyperv.ps1").read_text()
        formatter = script.split("function Convert-RunnerPeerTargets(", 1)[1].split("function Job-Path(", 1)[0]
        code = "\n".join(line for line in formatter.splitlines() if not line.lstrip().startswith("#"))
        self.assertIn("ConvertFrom-Json -InputObject $Json", code)
        self.assertIn("[Guid]::Parse($value)", code)
        self.assertIn(".ToString('D')", code)
        self.assertIn("return '[' + ($quoted -join ',') + ']'", code)
        self.assertNotIn("ConvertTo-Json", code)
        self.assertIn("(Convert-RunnerPeerTargets $PeerTargetsJson)", script)
        qualify = (HERE / "qualify.ps1").read_text()
        self.assertNotIn("ConvertTo-Json -InputObject @($target)", qualify)

    def test_failed_job_removal_harvests_before_vm_and_disk_destruction(self):
        script = (HERE / "hyperv.ps1").read_text()
        remove = script.split("function Remove-Job(", 1)[1].split("function Grant(", 1)[0]
        self.assertIn("Harvest-Job $Id -Lightweight:$LightweightHarvest", remove)
        self.assertLess(remove.index("Harvest-Job $Id"), remove.index("Remove-VM"))
        self.assertLess(remove.index("Harvest-Job $Id"), remove.index("Remove-Item"))
        control = (HERE.parents[1] / "crates/motor-runner/src/windows/hyperv.rs").read_text()
        cleanup = control.split("fn cleanup(&mut self, known: bool)", 1)[1].split("pub(super) fn finalize", 1)[0]
        self.assertLess(cleanup.index("child.kill()"), cleanup.index('"Remove"'))
        self.assertIn('let extra = if known {', cleanup)
        self.assertIn('vec!["-LightweightHarvest".into()]', cleanup)

    def test_qualification_exports_evidence_on_failure_without_weakening_success(self):
        script = (HERE / "qualify.ps1").read_text()
        self.assertIn("Copy-QualificationEvidence", script.rsplit("} finally {", 1)[1])
        self.assertLess(script.index("Copy-QualificationEvidence\n    $evidenceCopied = $true"), script.index("QUALIFIED-SINGLE-SLOT.txt"))

    def test_process_parent_guard_is_held_across_control_calls(self):
        windows = HERE.parents[1] / "crates/motor-runner/src/windows"
        host = (windows / "hyperv_host.rs").read_text()
        self.assertLess(host.index(".share_mode(1)"), host.index("checked_validation("))
        self.assertLess(host.index("checked_validation("), host.index("*lifetime = Some((parent, file))"))
        self.assertIn("parent_guard: Mutex<Option<(PathBuf, fs::File)>>", (windows / "agent.rs").read_text())
        guard_check = host.split("pub(super) fn ensure_parent_guard", 1)[1]
        self.assertIn("Windows parent verification guard is missing", guard_check)
        self.assertIn("file.metadata()", guard_check)
        prep = (windows / "prep.rs").read_text()
        boot = (windows / "hyperv.rs").read_text().split("pub(super) fn boot(", 1)[1]
        self.assertIn("super::ensure_parent_guard(config)?", prep)
        self.assertGreaterEqual(boot.count("super::ensure_parent_guard(config)?"), 2)

    def test_every_startup_hashes_and_records_changetime_as_evidence_only(self):
        helper = (HERE / "parent-identity.ps1").read_text()
        startup = helper.split("function Confirm-RunnerParentVerification", 1)[1]
        self.assertIn("return Write-RunnerParentVerification", startup)
        self.assertNotIn("Open-RunnerVerifiedParent", startup)
        self.assertIn("change_time_filetime=$changeTimeEvidence", helper)
        self.assertIn("change_time_filetime=$changeTime", helper)
        self.assertNotIn("change_time_filetime", helper.split("function Get-RunnerParentIdentity", 1)[1].split("function Assert-RunnerFileAcl", 1)[0])
        script = (HERE / "hyperv.ps1").read_text()
        validate = script.split("    'Validate' {", 1)[1].split("    { $_ -in", 1)[0]
        self.assertIn("Confirm-RunnerParentVerification", validate)
        self.assertNotIn("Remove-Item", validate)

    def test_same_size_rewrite_restoring_lastwrite_is_blocked_or_rehashed_on_restart(self):
        fixture = (HERE / "test_scripts.ps1").read_text()
        self.assertIn("parent-lifetime-guard-blocks-same-size-content-rewrite", fixture)
        self.assertIn("Set-FixtureParent 'modified' $lastWrite", fixture)
        self.assertIn("parent-identity-startup-rehash-catches-same-size-rewrite-restored-lastwrite", fixture)

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
        self.assertIn("'guest-evidence.ps1'", (HERE / "install.ps1").read_text())


if __name__ == "__main__":
    unittest.main()
