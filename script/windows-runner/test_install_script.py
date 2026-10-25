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

    def test_every_directory_the_runner_requires_private_is_protected_by_install(self):
        # hyperv.ps1 refuses a required directory that isn't SYSTEM or
        # Administrators owned with a private ACL. A directory the installer only
        # creates implicitly, as a parent, keeps its creator as owner on Windows
        # client editions, so each one needs its own Protect-RunnerPath.
        hyperv = (HERE / "hyperv.ps1").read_text()
        install = (HERE / "install.ps1").read_text()
        listed = re.search(r"foreach \(\$path in @\((\$Root, [^)]*)\)\) \{\s*if \(-not \(Test-Path -LiteralPath \$path -PathType Container\)\)", hyperv)
        self.assertIsNotNone(listed, "the required private directory list moved")
        required = [path.strip().strip('"') for path in listed.group(1).split(",")]
        self.assertIn("$Root\\state", required)
        protected = set(re.findall(r'Protect-RunnerPath "?(\$Root[^" ]*)"? \$nativeLogPath', install))
        protected |= set(re.findall(r"Protect-RunnerPath (\$Root) \$nativeLogPath", install))
        for path in required:
            with self.subTest(path=path):
                self.assertIn(path, protected)

    def test_no_drive_qualified_variable_inside_double_quoted_strings(self):
        # In PowerShell, "$Name:" in an expandable string parses as a drive-qualified
        # variable (like $env:PATH) and the whole script fails to load. Only
        # Windows parses these scripts, so catch it here, where CI runs.
        scopes = 'env|script|global|local|private|using|function|variable|alias'
        bad = re.compile(r'\$(?!(?:%s):)([A-Za-z_][A-Za-z0-9_]*):(?![:\\/])' % scopes)
        found = []
        for script in sorted(HERE.parent.rglob('*.ps1')):
            for number, line in enumerate(script.read_text(errors='replace').splitlines(), 1):
                for quoted in re.findall(r'"([^"]*)"', line):
                    found += [f'{script.relative_to(HERE.parent)}:{number}: ${name}:' for name in bad.findall(quoted)]
        self.assertEqual(found, [], 'write ${Name}: or $($Name): instead')

    def test_volume_reader_guards_optional_properties_and_wraps_cim_results(self):
        evidence = (HERE / "guest-evidence.ps1").read_text()
        self.assertIn("function Get-RunnerPropertyValue", evidence)
        self.assertIn("$Object.PSObject.Properties[$Name]", evidence)
        self.assertIn("$rawAccessPaths = Get-RunnerPropertyValue $partition 'AccessPaths'", evidence)
        self.assertIn("$accessPaths = @($rawAccessPaths)", evidence)
        self.assertIn("$guidPaths = @($accessPaths | Where-Object", evidence)
        self.assertIn("Test-RunnerVolumeGuidPath", evidence)
        self.assertIn("Get-RunnerPropertyValue $partition 'DriveLetter'", evidence)
        self.assertIn("if ($guidPaths.Count -eq 1)", evidence)
        self.assertNotIn("Get-Volume -Partition", evidence)
        self.assertNotIn("Get-Volume -DriveLetter", evidence)
        self.assertNotIn("| Get-Volume", evidence)
        self.assertNotIn("Get-Partition -Volume", evidence)
        self.assertIn("Get-RunnerPropertyValue $disk 'Number'", evidence)
        self.assertIn("Get-RunnerPropertyValue $disk 'UniqueId'", evidence)
        self.assertIn("Get-RunnerPropertyValue $disk 'Location'", evidence)
        self.assertIn("$mapped = @(Get-VHD -DiskNumber $diskNumber)", evidence)
        self.assertIn("if ($mapped.Count -ne 1)", evidence)
        self.assertNotIn("$mapped.Count -ne 1 -or", evidence)
        self.assertIn("$partitions = @(Get-Partition -DiskNumber $diskNumber", evidence)
        self.assertIn("if ($partitions.Count -ne 1)", evidence)
        self.assertIn("$identity['partition_access_paths'] = $accessPaths", evidence)
        self.assertIn("$identity['volume_guid_path'] = $volume", evidence)

        fixture = (HERE / "test_scripts.ps1").read_text()
        self.assertIn("$property = $identity.PSObject.Properties['volume_guid_path']", fixture)
        self.assertIn("Get-RunnerPropertyValue $identity 'volume_guid_path'", fixture)
        self.assertIn("AccessPaths=$accessPaths", fixture)
        self.assertIn("Get-Volume -Partition $volumePartition", fixture)
        self.assertIn("Get-Volume-must-return-no-object", fixture)
        self.assertNotIn("$identity.volume_path", fixture)


    def test_no_assignment_to_powershell_automatic_variables(self):
        # $Error, $Input, $Args and the like are PowerShell's own read-only or
        # automatic variables; assigning one fails at run time ("Cannot
        # overwrite variable Error") or silently misbehaves. Only Windows runs
        # these scripts, so catch it here, where CI runs.
        names = 'error|input|args|host|pid|home|true|false|psitem|this|executioncontext'
        assignment = re.compile(r'(?<![\w:.])\$(%s)\s*=(?!=)' % names, re.IGNORECASE)
        found = []
        for script in sorted(HERE.parent.rglob('*.ps1')):
            for number, line in enumerate(script.read_text(errors='replace').splitlines(), 1):
                code = line.split('#', 1)[0]
                found += [f'{script.relative_to(HERE.parent)}:{number}: ${m}' for m in assignment.findall(code)]
        self.assertEqual(found, [], 'use a name of your own')

    def test_local_user_descriptions_fit_windows_limit(self):
        found = 0
        for script in sorted(HERE.glob("*.ps1")):
            for match in re.finditer(r"New-LocalUser\b[^\n]*?-Description\s+'([^']*)'", script.read_text()):
                found += 1
                self.assertLessEqual(len(match.group(1)), LOCAL_USER_DESCRIPTION_MAX,
                                     f"{script.name}: {match.group(1)!r}")
        self.assertGreater(found, 0, "no New-LocalUser -Description literal found to check")

    def test_installer_native_commands_capture_stderr_and_check_exit_code(self):
        install = (HERE / "install.ps1").read_text()
        accounts = (HERE / "accounts.ps1").read_text()
        fixture = (HERE / "test_scripts.ps1").read_text()
        self.assertIn("Invoke-RunnerNative", install)
        self.assertIn("installation-native.jsonl", install)
        self.assertNotRegex(install, r"(?m)^\s*&\s*(?:[^\r\n]*\\)?['\"]?(?:icacls|sshd|powercfg|sc)\.exe")
        self.assertIn("RedirectStandardError = $true", accounts)
        self.assertIn("stderr = $stderr", accounts)
        self.assertIn("if ($exitCode -ne 0)", accounts)
        self.assertIn("native-command-exit-code-ignores-stderr-and-logs-it", fixture)
        self.assertIn("stderr-success-probe 1>&2", fixture)
        self.assertIn("exit /b 0", fixture)
        self.assertIn('Invoke-RunnerNative -FilePath "$env:SystemRoot\\System32\\cmd.exe"', fixture)

    def test_native_commands_close_stdin_and_kill_timed_out_processes(self):
        accounts = (HERE / "accounts.ps1").read_text()
        fixture = (HERE / "test_scripts.ps1").read_text()
        self.assertIn("RedirectStandardInput = $true", accounts)
        self.assertIn("$process.StandardInput.Close()", accounts)
        self.assertIn("$process.WaitForExit([int]([TimeSpan]::FromSeconds($TimeoutSeconds).TotalMilliseconds))", accounts)
        self.assertIn("$process.Kill()", accounts)
        self.assertIn("Native command $Operation timed out after $TimeoutSeconds seconds", accounts)
        self.assertIn("native-command-closes-empty-stdin-before-waiting", fixture)
        self.assertIn("native-timeout-fixture", fixture)
        self.assertIn("termination_confirmed = $terminated", accounts)

    def test_sshd_effective_policy_check_uses_system_without_weakening_match(self):
        install = (HERE / "install.ps1").read_text()
        helper = (HERE / "verify-sshd-system.ps1").read_text()
        self.assertIn("Invoke-RunnerSshPolicyAsSystem $sshdConfig", install)
        self.assertIn("Register-ScheduledTask -TaskName $taskName -Action $action -Settings $settings -User 'SYSTEM'", install)
        self.assertIn("verify-sshd-system.ps1", install)
        self.assertIn("-Operation 'verify restricted SSH policy as SYSTEM'", helper)
        self.assertIn("-TimeoutSeconds 30", helper)
        self.assertIn("user=motor-ssh,host=localhost,addr=127.0.0.1", helper)
        self.assertIn("Match User motor-ssh", install)
        self.assertIn("AuthenticationMethods publickey", install)
        self.assertIn("PasswordAuthentication no", install)


    def test_ssh_account_resolution_precedes_config_rewrite(self):
        install = (HERE / "install.ps1").read_text()
        create = install.index("$null = New-OwnedUser 'motor-ssh'")
        self.assertIn("Resolve-RunnerTrusteeSid 'motor-ssh'", install)
        resolve = install.index("Resolve-RunnerTrusteeSid 'motor-ssh'")
        local_resolve = install.index(r'Resolve-RunnerTrusteeSid "$env:COMPUTERNAME\motor-ssh"')
        rewrite = install.index("[IO.File]::WriteAllText($sshdConfig")
        self.assertLess(create, resolve)
        self.assertLess(resolve, local_resolve)
        self.assertLess(local_resolve, rewrite)
        self.assertIn("motor-ssh name resolution returned short=", install)

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

    def test_sealed_agent_reader_waits_for_volume_and_always_removes_mount(self):
        install = (HERE / "install.ps1").read_text()
        evidence = (HERE / "guest-evidence.ps1").read_text()
        identity = (HERE / "guest-agent-identity.ps1").read_text()
        fixture = (HERE / "test_scripts.ps1").read_text()
        self.assertIn('Get-RunnerSealedAgentHash $parent "$Root\\evidence"', install)
        self.assertIn("function Wait-RunnerDiagnosticVolume", evidence)
        self.assertIn("VolumeWaitSeconds = 15", evidence)
        self.assertIn("Get-Partition -DiskNumber $DiskNumber -PartitionNumber $PartitionNumber", evidence)
        self.assertIn("$rawAccessPaths = Get-RunnerPropertyValue $partition 'AccessPaths'", evidence)
        self.assertIn("$guidPaths = @($accessPaths | Where-Object", evidence)
        self.assertIn("if ($guidPaths.Count -eq 1)", evidence)
        self.assertIn("Test-RunnerVolumeGuidPath", evidence)
        self.assertIn("Get-RunnerPropertyValue $partition 'DriveLetter'", evidence)
        self.assertNotIn("Get-Volume -Partition", evidence)
        self.assertNotIn("Get-Volume -DriveLetter", evidence)
        self.assertNotIn("| Get-Volume", evidence)
        self.assertNotIn("Get-Partition -Volume", evidence)
        self.assertNotIn("Add-PartitionAccessPath", evidence)
        self.assertNotIn("Remove-PartitionAccessPath", evidence)
        self.assertIn("[IO.File]::GetAttributes($path)", evidence)
        self.assertIn("[IO.FileAttributes]::ReparsePoint", evidence)
        self.assertIn("$identity['volume_guid_path'] = $volume", evidence)
        self.assertIn("[IO.File]::Copy($item.FullName", evidence)
        self.assertIn("Get-GuestFileSha256 $agent", evidence)
        self.assertIn("[IO.File]::Copy($Source, $stagedSource", evidence)
        self.assertIn("Get-WinEvent -Path $stagedSource", evidence)
        self.assertNotIn("Copy-Item -LiteralPath $item.FullName", evidence)
        self.assertNotIn("Get-FileHash -LiteralPath $agent.FullName", evidence)
        self.assertIn("Timed out after $TimeoutSeconds seconds", evidence)
        self.assertIn("Dismount-VHD -Path $Path", evidence)
        self.assertIn("Use-DiagnosticWindowsDisk -Path $Parent -EvidenceDirectory $EvidenceDirectory -VolumeWaitSeconds $VolumeWaitSeconds", identity)
        self.assertIn("sealed-agent-volume-wait-retries-and-reads-identity", fixture)
        self.assertIn("sealed-agent-volume-ignores-existing-drive-letter", fixture)
        self.assertIn("sealed-agent-volume-timeout-unmounts", fixture)
        self.assertIn("sealed-agent-volume-guid-path-real-vhd", fixture)

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

    def test_uninstaller_cleans_partial_ssh_block_without_late_install_flag(self):
        uninstall = (HERE / "uninstall.ps1").read_text()
        ssh_cleanup = uninstall.split("$sshdConfig = ", 1)[1].split("if ($record.PSObject.Properties['power'])", 1)[0]
        self.assertIn("Remove-RunnerSshInstallState", ssh_cleanup)
        self.assertNotIn("Properties['ssh_block']", ssh_cleanup)
        self.assertIn("if ($service) {", uninstall)
        self.assertLess(uninstall.index("if ($service) {"), uninstall.index("Remove-RunnerSshInstallState"))
        accounts = (HERE / "accounts.ps1").read_text().split("function Remove-RunnerSshInstallState", 1)[1]
        self.assertLess(accounts.index("Remove-RunnerSshConfigBlock"), accounts.index("Remove-LocalUser motor-ssh"))
        fixture = (HERE / "test_scripts.ps1").read_text()
        self.assertIn("partial-install-ssh-account-and-block-cleanup-without-ssh-block-record", fixture)
        self.assertIn("native-command-exit-code-ignores-stderr-and-logs-it", fixture)

    def test_cmd_never_echoes_a_variable_straight_into_a_redirect(self):
        # In cmd, `echo %errorlevel%>file` with a one-digit value reads as a
        # handle redirect (`0>`), so the file gets "ECHO is off." instead of
        # the code. Put the redirect first: `>file echo %errorlevel%`.
        for script in sorted(HERE.glob("*.ps1")):
            for number, line in enumerate(script.read_text().splitlines(), 1):
                self.assertIsNone(re.search(r"(?i)\becho\s+%\w+%\s*>", line), f"{script.name}:{number}: {line.strip()}")


if __name__ == "__main__":
    unittest.main()
