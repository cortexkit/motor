# Windows PowerShell parser, policy, and temporary-VHD fixtures. Never boots a VM.
$ErrorActionPreference = 'Stop'
Set-StrictMode -Version Latest
# Any failure that escapes a check reports its script line and stack, so even
# a bare .NET exception says where it was thrown; break then rethrows it.
$count = 0
trap { Write-Host "FAILED at $($_.InvocationInfo.ScriptName):$($_.InvocationInfo.ScriptLineNumber) after $count passing checks: $($_.Exception.GetType().FullName): $($_.Exception.Message)"; Write-Host $_.ScriptStackTrace; break }
$files = @(Get-ChildItem $PSScriptRoot -Filter '*.ps1') + @(
    Get-Item "$PSScriptRoot\..\hyperv-spike\common.ps1"
    Get-Item "$PSScriptRoot\..\hyperv-spike\boot-files.ps1"
    Get-Item "$PSScriptRoot\..\hyperv-spike\diagnostics.ps1"
    Get-Item "$PSScriptRoot\..\hyperv-spike\measure.ps1"
    Get-Item "$PSScriptRoot\..\hyperv-spike\provision.ps1"
    Get-Item "$PSScriptRoot\..\windows-vm\bootstrap.ps1"
    Get-Item "$PSScriptRoot\..\windows-vm\setup-job-baseline.ps1"
)
$count = 0
foreach ($file in $files) {
    $tokens = $null; $errors = $null
    $null = [Management.Automation.Language.Parser]::ParseFile($file.FullName, [ref]$tokens, [ref]$errors)
    if ($errors.Count) { throw "Parse failure $($file.Name): $($errors | Out-String)" }
    Write-Host "PASS powershell-parse-$($file.Name)"
    $count++
}
. "$PSScriptRoot\accounts.ps1"
. "$PSScriptRoot\qualify-peer.ps1"
$root = Join-Path ([IO.Path]::GetTempPath()) ('motor-runner-script-tests-' + [Guid]::NewGuid())
New-Item $root -ItemType Directory | Out-Null
$script:effective = @('[Unicode]','Unicode=yes','[Version]','signature="$CHICAGO$"','Revision=1','[Privilege Rights]','SeBatchLogonRight = motor-prep,*S-1-5-32-544','SeDenyInteractiveLogonRight = *S-1-5-32-546')
# The old installation's policy export names motor-prep, so resolve it to the recorded SID.
$script:RunnerResolveTrusteeName = { param($name) if ($name -in @('motor-prep','motor-ssh',"$env:COMPUTERNAME\motor-ssh")) { 'S-1-5-21-111-222-333-1001' } else { throw "unknown account $name" } }
function global:secedit.exe {
    $cfg = $args[[array]::IndexOf($args,'/cfg') + 1]
    if ($args -contains '/export') { $script:effective | ForEach-Object { $_ -replace '\*S-1-5-21-111-222-333-1001','motor-prep' } | Set-Content -LiteralPath $cfg -Encoding Unicode }
    elseif ($args -contains '/configure') { $script:effective = @(Get-Content -LiteralPath $cfg) }
    else { throw 'Unexpected secedit operation in fixture' }
    $global:LASTEXITCODE = 0
}
$script:prepPresent = $true
$script:prepRemoved = $false
function global:Get-LocalUser { if ($script:prepPresent) { return [pscustomobject]@{ SID=[pscustomobject]@{Value='S-1-5-21-111-222-333-1001'} } }; return $null }
function global:Get-CimInstance { return @() }
function global:Remove-LocalUser { $script:prepRemoved = $true }
if ((Get-Command secedit.exe).CommandType -ne 'Function') { throw 'Policy fixture mock is not installed; refusing to run a real secedit command' }
try {
    $sid = 'S-1-5-21-111-222-333-1001'
    Remove-RunnerPrepAccount $root $sid
    if (-not $script:prepRemoved) { throw 'Legacy install retained its prep account' }
    if (($script:effective -join "`n") -match ([regex]::Escape($sid) + '|motor-prep')) { throw 'Legacy install retained prep batch rights' }
    if (($script:effective -join "`n") -notmatch 'SeBatchLogonRight = \*S-1-5-32-544') { throw 'Cleanup lost unrelated administrator rights' }
    Write-Host 'PASS legacy-install-removes-prep-account-rights'
    $count++
    $script:prepPresent = $false
    $script:prepRemoved = $false
    $script:effective = @('[Unicode]','Unicode=yes','[Version]','signature="$CHICAGO$"','Revision=1','[Privilege Rights]','SeBatchLogonRight = *S-1-5-32-544')
    Remove-RunnerPrepAccount $root $sid
    if ($script:prepRemoved -or (($script:effective -join "`n") -notmatch 'SeBatchLogonRight = \*S-1-5-32-544')) { throw 'Cleanup of an already-removed prep account was not idempotent' }
    Write-Host 'PASS already-removed-prep-account-cleanup-is-idempotent'
    $count++
    $rejected = $false
    try { Remove-RunnerBatchRights $root 'S-1-5-32-545' } catch { $rejected = $true }
    if (-not $rejected) { throw 'Policy cleanup accepted a broad Users group SID' }
    Write-Host 'PASS legacy-prep-cleanup-rejects-broad-group-sid'
    $count++
} finally {
    Remove-Item Function:\secedit.exe,Function:\Get-LocalUser,Function:\Get-CimInstance,Function:\Remove-LocalUser
    Remove-Item $root -Recurse -Force
}

if ([Environment]::OSVersion.Platform -eq [PlatformID]::Win32NT) {
    if ((Resolve-RunnerTrusteeSid 'motor-ssh') -cne 'S-1-5-21-111-222-333-1001' -or (Resolve-RunnerTrusteeSid "$env:COMPUTERNAME\motor-ssh") -cne 'S-1-5-21-111-222-333-1001') { throw 'FAIL sshd-account-name-resolves-to-recorded-sid' }
    Write-Host 'PASS sshd-account-name-resolves-to-recorded-sid'
    $count++
    $nativeFixtureRoot = Join-Path ([IO.Path]::GetTempPath()) ('motor-runner-native-tests-' + [Guid]::NewGuid())
    New-Item -ItemType Directory -Path $nativeFixtureRoot | Out-Null
    try {
        $nativeLog = Join-Path $nativeFixtureRoot 'native-commands.jsonl'
        $nativeScript = Join-Path $nativeFixtureRoot 'stderr-success.cmd'
        [IO.File]::WriteAllText($nativeScript, "@echo off`r`necho stderr-success-probe 1>&2`r`nexit /b 0`r`n", [Text.Encoding]::ASCII)
        $native = Invoke-RunnerNative -FilePath "$env:SystemRoot\System32\cmd.exe" -ArgumentList @('/d', '/c', $nativeScript) -Operation 'stderr-success-fixture' -LogPath $nativeLog
        if ($native.exit_code -ne 0 -or $native.stderr -notmatch 'stderr-success-probe' -or (Get-Content -LiteralPath $nativeLog -Raw) -notmatch 'stderr-success-probe') {
            throw 'FAIL native-command-exit-code-ignores-stderr-and-logs-it'
        }
        Write-Host 'PASS native-command-exit-code-ignores-stderr-and-logs-it'
        $count++

        $stdinScript = Join-Path $nativeFixtureRoot 'stdin-eof.ps1'
        [IO.File]::WriteAllText($stdinScript, '[Console]::In.ReadToEnd(); [Console]::Out.WriteLine("native-stdin-eof-probe")', [Text.UTF8Encoding]::new($false))
        $stdinResult = Invoke-RunnerNative -FilePath "$env:SystemRoot\System32\WindowsPowerShell\v1.0\powershell.exe" -ArgumentList @('-NoProfile','-NonInteractive','-ExecutionPolicy','Bypass','-File',$stdinScript) -Operation 'native-empty-stdin-fixture' -LogPath $nativeLog -TimeoutSeconds 5
        if ($stdinResult.exit_code -ne 0 -or $stdinResult.stdout -notmatch 'native-stdin-eof-probe') {
            throw 'FAIL native-command-closes-empty-stdin-before-waiting'
        }
        Write-Host 'PASS native-command-closes-empty-stdin-before-waiting'
        $count++

        $timeoutScript = Join-Path $nativeFixtureRoot 'timeout.ps1'
        [IO.File]::WriteAllText($timeoutScript, 'Start-Sleep -Seconds 30', [Text.UTF8Encoding]::new($false))
        $timeoutStarted = [Diagnostics.Stopwatch]::StartNew()
        $timeoutError = $null
        try {
            $null = Invoke-RunnerNative -FilePath "$env:SystemRoot\System32\WindowsPowerShell\v1.0\powershell.exe" -ArgumentList @('-NoProfile','-NonInteractive','-ExecutionPolicy','Bypass','-File',$timeoutScript) -Operation 'native-timeout-fixture' -LogPath $nativeLog -TimeoutSeconds 1
        } catch {
            $timeoutError = $_.Exception.Message
        }
        $timeoutRecords = @(Get-Content -LiteralPath $nativeLog | ForEach-Object { $_ | ConvertFrom-Json } | Where-Object operation -eq 'native-timeout-fixture')
        if ($timeoutError -notmatch 'Native command native-timeout-fixture timed out after 1 seconds' -or $timeoutStarted.Elapsed.TotalSeconds -gt 10 -or $timeoutRecords.Count -ne 1 -or -not $timeoutRecords[0].timed_out -or -not $timeoutRecords[0].termination_confirmed -or $timeoutRecords[0].process_id -le 0) {
            throw "FAIL native-command-timeout-kills-and-identifies-operation: $timeoutError"
        }
        Write-Host 'PASS native-command-timeout-kills-and-identifies-operation'
        $count++

        # A directory an elevated administrator creates on a client edition of
        # Windows is owned by that user; Protect-RunnerPath must leave it owned
        # by Administrators, or the runner and uninstaller refuse it.
        $ownedRoot = Join-Path $nativeFixtureRoot 'protected-owner'
        New-Item -ItemType Directory -Path $ownedRoot | Out-Null
        Protect-RunnerPath $ownedRoot $nativeLog
        $owner = (Get-Acl -LiteralPath $ownedRoot).GetOwner([Security.Principal.SecurityIdentifier]).Value
        if ($owner -ne 'S-1-5-32-544') { throw "FAIL protect-runner-path-sets-administrators-owner (owner $owner)" }
        Write-Host 'PASS protect-runner-path-sets-administrators-owner'
        $count++

        $sshDirectory = Join-Path $nativeFixtureRoot 'ssh'
        $keyLine = 'ssh-ed25519 AAAAC3NzaC1lZDI1NTE5AAAAIFixture fixture'
        Set-RunnerSshAuthorizedKey -Directory $sshDirectory -KeyLine $keyLine
        foreach ($pass in 1..2) {
            foreach ($path in @($sshDirectory, (Join-Path $sshDirectory 'authorized_keys'))) {
                $acl = Get-Acl -LiteralPath $path
                $rules = @($acl.GetAccessRules($true, $true, [Security.Principal.SecurityIdentifier]))
                $trustees = @($rules | ForEach-Object { $_.IdentityReference.Value } | Sort-Object)
                if ($acl.GetOwner([Security.Principal.SecurityIdentifier]).Value -ne 'S-1-5-32-544' -or -not $acl.AreAccessRulesProtected -or ($trustees -join ',') -cne 'S-1-5-18,S-1-5-32-544' -or @($rules | Where-Object { $_.IsInherited -or $_.AccessControlType -ne 'Allow' -or $_.FileSystemRights -ne 'FullControl' }).Count) {
                    throw "FAIL ssh-key-exact-private-acl pass=$pass path=$path"
                }
                if ($pass -eq 1) {
                    $acl.AddAccessRule([Security.AccessControl.FileSystemAccessRule]::new([Security.Principal.SecurityIdentifier]::new('S-1-5-32-545'), 'Read', 'Allow'))
                    Set-Acl -LiteralPath $path -AclObject $acl
                }
            }
            if ([IO.File]::ReadAllText((Join-Path $sshDirectory 'authorized_keys')) -cne ($keyLine + "`r`n")) { throw 'FAIL ssh-key-content' }
            Set-RunnerSshAuthorizedKey -Directory $sshDirectory -KeyLine $keyLine
        }
        Write-Host 'PASS ssh-key-fixed-directory-exact-private-acl-and-idempotent-repair'
        $count++

        $installText = [IO.File]::ReadAllText((Join-Path $PSScriptRoot 'install.ps1'))
        $sshBlock = [regex]::Match($installText, '(?ms)^# BEGIN MOTOR RUNNER\r?\n.*?^# END MOTOR RUNNER').Value
        if (-not $sshBlock.Contains('AuthorizedKeysFile __PROGRAMDATA__/MotorRunner/ssh/authorized_keys')) { throw 'FAIL ssh-key-fixed-config-path' }
        $configFixture = Join-Path $nativeFixtureRoot 'sshd_config'
        $originalConfig = "Port 22`r`n# unrelated configuration`r`n"
        [IO.File]::WriteAllText($configFixture, $originalConfig, [Text.UTF8Encoding]::new($false))
        $rejectScript = Join-Path $nativeFixtureRoot 'sshd-reject.cmd'
        [IO.File]::WriteAllText($rejectScript, "@echo off`r`nexit /b 1`r`n", [Text.Encoding]::ASCII)
        $configRejected = $false
        try {
            Set-RunnerSshConfigBlock -ConfigPath $configFixture -Block $sshBlock -LogPath $nativeLog -SshdPath "$env:SystemRoot\System32\cmd.exe" -SshdArguments @('/d', '/c', $rejectScript)
        } catch { $configRejected = $true }
        if (-not $configRejected -or [IO.File]::ReadAllText($configFixture) -cne $originalConfig) { throw 'FAIL ssh-config-validation-restores-original' }
        Set-RunnerSshConfigBlock -ConfigPath $configFixture -Block $sshBlock -LogPath $nativeLog -SshdPath "$env:SystemRoot\System32\cmd.exe" -SshdArguments @('/d', '/c', $nativeScript)
        if (-not [IO.File]::ReadAllText($configFixture).Contains($sshBlock)) { throw 'FAIL ssh-config-validated-block-installed' }
        Write-Host 'PASS ssh-config-validation-restores-original-and-accepts-valid-block'
        $count++

        $partialRoot = Join-Path $nativeFixtureRoot 'partial-install'
        New-Item -ItemType Directory -Path $partialRoot | Out-Null
        $partialSid = 'S-1-5-21-111-222-333-1001'
        $sshHomeFixture = "C:\Users\motor-ssh.test-$([Guid]::NewGuid().ToString('N'))"
        if (Test-Path -LiteralPath $sshHomeFixture) { throw 'FAIL partial-install-test-profile-path-not-fresh' }
        $partialRecord = @{schema=1; root=$partialRoot; accounts=@{'motor-ssh'=$partialSid}; ssh_home=$sshHomeFixture}
        $partialRecordPath = Join-Path $partialRoot 'installation.json'
        [IO.File]::WriteAllText($partialRecordPath, (ConvertTo-Json -InputObject $partialRecord -Compress), [Text.UTF8Encoding]::new($false))
        $originalSshd = "Port 22`r`n"
        $ownedSshd = $originalSshd + "# BEGIN MOTOR RUNNER`r`nMatch User motor-ssh`r`n    ForceCommand C:/ProgramData/MotorRunner/bin/motor-runner.exe connect`r`nMatch all`r`n# END MOTOR RUNNER`r`n"
        $sshdConfig = Join-Path $partialRoot 'sshd_config'
        [IO.File]::WriteAllText($sshdConfig, $ownedSshd, [Text.UTF8Encoding]::new($false))
        $partialRecordRead = Get-Content -LiteralPath $partialRecordPath -Raw | ConvertFrom-Json
        if ($partialRecordRead.PSObject.Properties['ssh_block']) { throw 'FAIL partial-install-fixture-omits-late-ssh-block-flag' }
        $global:partialSshSid = $partialSid
        $global:partialSshAccountRemoved = $false
        $global:partialSshdRestarted = $false
        function global:Get-LocalUser {
            [CmdletBinding()]
            param([string]$Name)
            if ($Name -eq 'motor-ssh' -and -not $global:partialSshAccountRemoved) { return [pscustomobject]@{SID=[pscustomobject]@{Value=$global:partialSshSid}} }
            return $null
        }
        function global:Remove-LocalUser {
            param([string]$Name)
            if ($Name -eq 'motor-ssh') { $global:partialSshAccountRemoved = $true }
        }
        function global:Get-CimInstance { return @() }
        function global:Restart-Service {
            param([string]$Name)
            if ($Name -eq 'sshd') { $global:partialSshdRestarted = $true }
        }
        try {
            New-Item -Path "$sshHomeFixture\.ssh" -ItemType Directory -Force | Out-Null
            Set-Content -LiteralPath "$sshHomeFixture\.ssh\authorized_keys" -Value $keyLine
            Set-RunnerSshAuthorizedKey -Directory (Join-Path $partialRoot 'ssh') -KeyLine $keyLine
            $removedSshState = Remove-RunnerSshInstallState -Installation $partialRecordRead -SshdConfig $sshdConfig -LogPath $nativeLog -SshdPath "$env:SystemRoot\System32\cmd.exe" -SshdArguments @('/d', '/c', $nativeScript)
            if (-not $removedSshState -or [IO.File]::ReadAllText($sshdConfig) -cne $originalSshd -or -not $global:partialSshAccountRemoved -or -not $global:partialSshdRestarted) {
                throw 'FAIL partial-install-ssh-account-and-block-cleanup'
            }
            if ((Test-Path -LiteralPath $sshHomeFixture) -or (Test-Path -LiteralPath (Join-Path $partialRoot 'ssh'))) { throw 'FAIL ssh-cleanup-removes-old-and-hand-fixed-key-layouts' }
            $removedAgain = Remove-RunnerSshInstallState -Installation $partialRecordRead -SshdConfig $sshdConfig -LogPath $nativeLog -SshdPath "$env:SystemRoot\System32\cmd.exe" -SshdArguments @('/d', '/c', $nativeScript)
            if ($removedAgain -or [IO.File]::ReadAllText($sshdConfig) -cne $originalSshd) { throw 'FAIL ssh-cleanup-idempotent' }
            $freshRecord = [pscustomobject]@{schema=1; root=$partialRoot; accounts=[pscustomobject]@{}}
            Set-RunnerSshAuthorizedKey -Directory (Join-Path $partialRoot 'ssh') -KeyLine $keyLine
            $null = Remove-RunnerSshInstallState -Installation $freshRecord -SshdConfig $sshdConfig -LogPath $nativeLog
            if (Test-Path -LiteralPath (Join-Path $partialRoot 'ssh')) { throw 'FAIL ssh-cleanup-without-profile-record' }
            Write-Host 'PASS ssh-cleanup-old-hand-fixed-and-profile-free-layouts-is-idempotent'
            $count++
        } finally {
            Remove-Item Function:\Get-LocalUser,Function:\Remove-LocalUser,Function:\Get-CimInstance,Function:\Restart-Service
            Remove-Variable -Name partialSshAccountRemoved,partialSshdRestarted,partialSshSid -Scope Global -ErrorAction SilentlyContinue
            if (Test-Path -LiteralPath $sshHomeFixture) { Remove-Item -LiteralPath $sshHomeFixture -Recurse -Force }
        }
        Write-Host 'PASS partial-install-ssh-account-and-block-cleanup-without-ssh-block-record'
        $count++
    } finally { Remove-Item -LiteralPath $nativeFixtureRoot -Recurse -Force }
    . "$PSScriptRoot\guest-agent-identity.ps1"
    . "$PSScriptRoot\guest-evidence.ps1"
    $parentAgentHash = 'A' * 64
    $otherAgentHash = 'B' * 64
    if ((Get-RunnerProvisionedAgentHash "$parentAgentHash  motor-winagent.exe") -cne $parentAgentHash) { throw 'FAIL parent-agent-record-match' }
    if ((Get-RunnerProvisionedAgentHash "$otherAgentHash  motor-winagent.exe") -cne $otherAgentHash) { throw 'FAIL parent-agent-record-mismatch' }
    if (Get-RunnerProvisionedAgentHash $null) { throw 'FAIL parent-agent-record-missing' }
    Write-Host 'PASS parent-agent-record-match-mismatch-missing'
    $count++
    $identityFixture = Join-Path ([IO.Path]::GetTempPath()) ('motor-agent-identity-' + [Guid]::NewGuid())
    $identityVolumeVhdx = Join-Path $identityFixture 'volume-guid-fixture.vhdx'
    $identityVolumeMounted = $false
    New-Item -ItemType Directory -Path $identityFixture | Out-Null
    try {
        Import-Module Hyper-V
        # Keep a drive letter and GUID path together; the reader must not require Get-Volume.
        New-VHD -Path $identityVolumeVhdx -Dynamic -SizeBytes 128MB | Out-Null
        $volumeImage = Mount-VHD -Path $identityVolumeVhdx -Passthru
        $identityVolumeMounted = $true
        $volumeDisk = $volumeImage | Get-Disk
        $volumeDiskNumber = Get-RunnerPropertyValue $volumeDisk 'Number'
        if ($null -eq $volumeDiskNumber) { throw 'FAIL sealed-agent-volume-fixture-disk-number-missing' }
        $volumeDiskNumber = [int]$volumeDiskNumber
        Initialize-Disk -Number $volumeDiskNumber -PartitionStyle GPT | Out-Null
        $volumePartition = New-Partition -DiskNumber $volumeDiskNumber -UseMaximumSize -AssignDriveLetter
        Format-Volume -Partition $volumePartition -FileSystem NTFS -NewFileSystemLabel IdentityFixture -Confirm:$false | Out-Null
        $volumePartitionNumber = Get-RunnerPropertyValue $volumePartition 'PartitionNumber'
        if ($null -eq $volumePartitionNumber) { throw 'FAIL sealed-agent-volume-fixture-partition-number-missing' }
        $volumePartitionNumber = [int]$volumePartitionNumber
        $volumePartition = Get-Partition -DiskNumber $volumeDiskNumber -PartitionNumber $volumePartitionNumber
        $volumeBinding = Wait-RunnerDiagnosticVolume -DiskNumber $volumeDiskNumber -PartitionNumber $volumePartitionNumber -TimeoutSeconds 15
        $identityVolumePath = [string]$volumeBinding.volume_path
        $identityVolumeAccessPaths = @($volumeBinding.access_paths)
        $identityVolumeDriveLetter = [string](Get-RunnerPropertyValue $volumePartition 'DriveLetter')
        $guidAccessPaths = @($identityVolumeAccessPaths | Where-Object { Test-RunnerVolumeGuidPath ([string]$_) })
        $letterAccessPaths = @($identityVolumeAccessPaths | Where-Object { [string]$_ -match '\A[A-Z]:\\\z' })
        if ($guidAccessPaths.Count -ne 1 -or -not $identityVolumeDriveLetter -or $letterAccessPaths.Count -lt 1) {
            throw 'FAIL sealed-agent-volume-fixture-requires-a-letter-and-one-volume-guid-access-path'
        }
        [IO.Directory]::CreateDirectory([IO.Path]::Combine($identityVolumePath, 'Windows')) | Out-Null
        $identityVolumeRecord = [IO.Path]::Combine($identityVolumePath, 'build\provision\winagent.sha256')
        [IO.Directory]::CreateDirectory([IO.Path]::GetDirectoryName($identityVolumeRecord)) | Out-Null
        [IO.File]::WriteAllText($identityVolumeRecord, "$parentAgentHash  motor-winagent.exe`n", [Text.UTF8Encoding]::new($false))

        $global:identityFixtureParent = Join-Path $identityFixture 'parent.vhdx'
        $global:identityFixtureVolumePath = $identityVolumePath
        $global:identityFixtureDriveLetter = $identityVolumeDriveLetter
        $global:identityFixtureDismounted = $false
        $global:identityFixtureNeverGuidPath = $false
        $global:identityFixtureAccessPathQueries = 0
        $global:identityFixtureVolumeQueries = 0
        $global:identityFixtureAddCalls = 0
        $global:identityFixtureAccessPathTouched = $false
        [IO.File]::WriteAllText($global:identityFixtureParent, 'fixture VHDX')
        function global:Mount-VHD {
            [CmdletBinding()]
            param([string]$Path, [switch]$ReadOnly, [switch]$Passthru)
            if (-not $ReadOnly -or -not $Passthru) { throw 'Fixture requires read-only passthrough mount' }
            return [pscustomobject]@{Path=$Path}
        }
        function global:Get-Disk {
            [CmdletBinding()]
            param([Parameter(ValueFromPipeline=$true)]$InputObject)
            process { return [pscustomobject]@{Number=42; UniqueId='fixture-disk'; Location='fixture'} }
        }
        function global:Get-VHD {
            [CmdletBinding()]
            param([int]$DiskNumber)
            return [pscustomobject]@{Path=$global:identityFixtureParent; ParentPath='fixture-parent'}
        }
        function global:Get-Partition {
            [CmdletBinding()]
            param([int]$DiskNumber, [int]$PartitionNumber)
            $global:identityFixtureAccessPathQueries++
            $accessPaths = @([string]$global:identityFixtureDriveLetter + ':\')
            if (-not $global:identityFixtureNeverGuidPath -and $global:identityFixtureAccessPathQueries -ge 3) { $accessPaths += $global:identityFixtureVolumePath }
            return [pscustomobject]@{DiskNumber=42; PartitionNumber=1; DriveLetter=$global:identityFixtureDriveLetter; AccessPaths=$accessPaths; GptType='{ebd0a0a2-b9e5-4433-87c0-68b6b72699c7}'; Guid='fixture-partition'}
        }
        function global:Add-PartitionAccessPath {
            [CmdletBinding()]
            param([Parameter(ValueFromPipeline=$true)]$InputObject, [switch]$AssignDriveLetter)
            process { $global:identityFixtureAddCalls++ }
        }
        function global:Get-Volume {
            [CmdletBinding()]
            param($Partition, [string]$DriveLetter, [string[]]$Path)
            $global:identityFixtureVolumeQueries++
            return
        }
        if (@(Get-Volume -Partition $volumePartition).Count -ne 0) { throw 'FAIL sealed-agent-volume-fixture-Get-Volume-must-return-no-object' }
        $global:identityFixtureVolumeQueries = 0
        function global:Remove-PartitionAccessPath {
            [CmdletBinding()]
            param([Parameter(ValueFromPipeline=$true)]$InputObject, [string]$AccessPath)
            process { $global:identityFixtureAccessPathTouched = $true }
        }
        function global:Dismount-VHD {
            [CmdletBinding()]
            param([string]$Path)
            if ($global:identityFixtureAddCalls -ne 0 -or $global:identityFixtureAccessPathTouched) { throw 'Fixture access path was modified' }
            $global:identityFixtureDismounted = $true
        }
        try {
            $identityEvidence = Join-Path $identityFixture 'evidence'
            New-Item -ItemType Directory -Path $identityEvidence | Out-Null
            $hash = Get-RunnerSealedAgentHash $global:identityFixtureParent $identityEvidence 3
            $identity = Get-Content -LiteralPath (Join-Path $identityEvidence 'mounted-disk-identity.json') -Raw | ConvertFrom-Json
            $identityAccessPaths = @(Get-RunnerPropertyValue $identity 'partition_access_paths')
            $identityVolumeGuidPath = Get-RunnerPropertyValue $identity 'volume_guid_path'
            $identityDriveLetter = Get-RunnerPropertyValue $identity 'drive_letter'
            $fixtureLetterPath = [string]$global:identityFixtureDriveLetter + ':\'
            if ($hash -cne $parentAgentHash -or $global:identityFixtureAccessPathQueries -lt 3 -or $global:identityFixtureVolumeQueries -ne 0 -or $global:identityFixtureAddCalls -ne 0 -or $global:identityFixtureAccessPathTouched -or -not $global:identityFixtureDismounted -or $identityVolumeGuidPath -cne $identityVolumePath -or $identityDriveLetter -cne $global:identityFixtureDriveLetter -or $identityAccessPaths -notcontains $identityVolumePath -or $identityAccessPaths -notcontains $fixtureLetterPath) { throw 'FAIL sealed-agent-volume-wait-retries-and-reads-identity' }
            Write-Host 'PASS sealed-agent-volume-wait-retries-and-reads-identity'
            $count++

            $global:identityFixtureDriveLetter = 'F'
            $global:identityFixtureDismounted = $false
            $global:identityFixtureAccessPathQueries = 0
            $global:identityFixtureVolumeQueries = 0
            $hash = Get-RunnerSealedAgentHash $global:identityFixtureParent $identityEvidence 3
            $identity = Get-Content -LiteralPath (Join-Path $identityEvidence 'mounted-disk-identity.json') -Raw | ConvertFrom-Json
            $identityVolumeGuidPath = Get-RunnerPropertyValue $identity 'volume_guid_path'
            $identityDriveLetter = Get-RunnerPropertyValue $identity 'drive_letter'
            if ($hash -cne $parentAgentHash -or $global:identityFixtureAccessPathQueries -lt 3 -or $global:identityFixtureVolumeQueries -ne 0 -or $global:identityFixtureAddCalls -ne 0 -or $global:identityFixtureAccessPathTouched -or -not $global:identityFixtureDismounted -or $identityVolumeGuidPath -cne $identityVolumePath -or $identityDriveLetter -cne 'F') { throw 'FAIL sealed-agent-volume-ignores-existing-drive-letter' }
            Write-Host 'PASS sealed-agent-volume-ignores-existing-drive-letter'
            $count++

            $global:identityFixtureDismounted = $false
            $global:identityFixtureNeverGuidPath = $true
            $global:identityFixtureAccessPathQueries = 0
            $global:identityFixtureVolumeQueries = 0
            $failure = $null
            $expectedTimeout = $script:RunnerDiagnosticVolumeTimeoutFormat -f 1,42,1,$script:RunnerDiagnosticNoGuidPathMessage
            # Start without the previous case's record, so a stale volume path can't
            # satisfy or fail this check; under strict mode an absent property throws.
            $identityFile = Join-Path $identityEvidence 'mounted-disk-identity.json'
            Remove-Item -LiteralPath $identityFile -ErrorAction SilentlyContinue
            try { $null = Get-RunnerSealedAgentHash $global:identityFixtureParent $identityEvidence 1 } catch { $failure = [string]$_ }
            $recordedVolumePath = $null
            if (Test-Path -LiteralPath $identityFile) {
                $identity = Get-Content -LiteralPath $identityFile -Raw | ConvertFrom-Json
                $property = $identity.PSObject.Properties['volume_guid_path']
                if ($property) { $recordedVolumePath = $property.Value }
            }
            if ($failure -notlike ('*' + $expectedTimeout + '*') -or $global:identityFixtureVolumeQueries -ne 0 -or $global:identityFixtureAddCalls -ne 0 -or $global:identityFixtureAccessPathTouched -or -not $global:identityFixtureDismounted -or $recordedVolumePath) { throw "FAIL sealed-agent-volume-timeout-unmounts: $failure" }
            Write-Host 'PASS sealed-agent-volume-timeout-unmounts'
            $count++
        } finally {
            Remove-Item Function:\Mount-VHD,Function:\Get-Disk,Function:\Get-VHD,Function:\Get-Partition,Function:\Add-PartitionAccessPath,Function:\Remove-PartitionAccessPath,Function:\Dismount-VHD -ErrorAction SilentlyContinue
            Remove-Variable -Name identityFixtureParent,identityFixtureVolumePath,identityFixtureDriveLetter,identityFixtureDismounted,identityFixtureNeverGuidPath,identityFixtureAccessPathQueries,identityFixtureAddCalls,identityFixtureAccessPathTouched -Scope Global -ErrorAction SilentlyContinue
        }

        Dismount-VHD -Path $identityVolumeVhdx
        $identityVolumeMounted = $false
        $identityEvidence = Join-Path $identityFixture 'real-vhd-evidence'
        New-Item -ItemType Directory -Path $identityEvidence | Out-Null
        $global:identityFixtureVolumeQueries = 0
        $hash = Get-RunnerSealedAgentHash $identityVolumeVhdx $identityEvidence 15
        $identity = Get-Content -LiteralPath (Join-Path $identityEvidence 'mounted-disk-identity.json') -Raw | ConvertFrom-Json
        $actualVolumeGuidPath = Get-RunnerPropertyValue $identity 'volume_guid_path'
        $actualDriveLetter = Get-RunnerPropertyValue $identity 'drive_letter'
        $actualVolumePath = Get-RunnerPropertyValue $identity 'volume_path'
        $actualAccessPaths = @(Get-RunnerPropertyValue $identity 'partition_access_paths')
        $actualLetterPath = $identityVolumeDriveLetter + ':\'
        if ($hash -cne $parentAgentHash -or $global:identityFixtureVolumeQueries -ne 0 -or -not (Test-RunnerVolumeGuidPath $actualVolumeGuidPath) -or $actualDriveLetter -cne $identityVolumeDriveLetter -or $actualVolumePath -cne $identityVolumePath -or $actualAccessPaths -notcontains $identityVolumePath -or $actualAccessPaths -notcontains $actualLetterPath) { throw 'FAIL sealed-agent-volume-guid-path-real-vhd' }
        Write-Host 'PASS sealed-agent-volume-guid-path-real-vhd'
        $count++
    } finally {
        Remove-Item Function:\Mount-VHD,Function:\Get-Disk,Function:\Get-VHD,Function:\Get-Partition,Function:\Add-PartitionAccessPath,Function:\Get-Volume,Function:\Remove-PartitionAccessPath,Function:\Dismount-VHD -ErrorAction SilentlyContinue
        Remove-Variable -Name identityFixtureVolumeQueries -Scope Global -ErrorAction SilentlyContinue
        if ($identityVolumeMounted) { Dismount-VHD -Path $identityVolumeVhdx -ErrorAction SilentlyContinue }
        if (Test-Path -LiteralPath $identityVolumeVhdx) { Remove-Item -LiteralPath $identityVolumeVhdx -Force -ErrorAction SilentlyContinue }
        Remove-Item -LiteralPath $identityFixture -Recurse -Force -ErrorAction SilentlyContinue
    }
    . "$PSScriptRoot\parent-identity.ps1"
    $hypervPath = Join-Path $PSScriptRoot 'hyperv.ps1'
    $hypervTokens = $null
    $hypervErrors = $null
    $hypervAst = [Management.Automation.Language.Parser]::ParseFile($hypervPath, [ref]$hypervTokens, [ref]$hypervErrors)
    if ($hypervErrors.Count) { throw "FAIL parent-identity-guard-fixture-source-parse: $($hypervErrors | Out-String)" }
    $guards = @($hypervAst.FindAll({
        param($node)
        $node -is [Management.Automation.Language.AssignmentStatementAst] -and
        $node.Left -is [Management.Automation.Language.VariableExpressionAst] -and
        $node.Left.VariablePath.UserPath -ieq 'parentGuard' -and
        $node.Right.Extent.Text -match 'Open-RunnerVerifiedParent'
    }, $true) | Sort-Object { $_.Extent.StartOffset })
    if ($guards.Count -ne 2) {
        throw "FAIL parent-identity-guard-fixture-expected-two-assignments-found-$($guards.Count)"
    }
    if ($guards[0].Right.Extent.Text -notmatch '\$Parent\s+\$ParentSha256\s+\$parentRecord') {
        throw 'FAIL parent-identity-guard-fixture-Prepare-assignment-not-found'
    }
    if ($guards[1].Right.Extent.Text -notmatch '\$metadata\.parent\s+\$metadata\.sha256\s+\$parentRecord') {
        throw 'FAIL parent-identity-guard-fixture-Run-assignment-not-found'
    }
    Write-Host 'PASS parent-identity-guard-fixture-found-Prepare-and-Run-assignments'
    $count++
    $fixture = Join-Path ([IO.Path]::GetTempPath()) ('motor-parent-tests-' + [Guid]::NewGuid())
    New-Item $fixture -ItemType Directory | Out-Null
    $Parent = Join-Path $fixture 'parent.vhdx'
    $record = Join-Path $fixture 'parent-verified.json'
    $parentRecord = $record
    $parentGuard = $null
    $lifetimeGuard = $null
    $capability = 'S-1-15-3-1024-2268835264-3721307629-241982045-173645152-1490879176-104643441-2915960892-1612460704'
    $readAcl = [Security.AccessControl.FileSecurity]::new()
    $readSddl = "O:BAG:BAD:P(A;;FR;;;SY)(A;;FR;;;BA)(A;;FR;;;S-1-5-83-0)(A;;FR;;;$capability)"
    $readAcl.SetSecurityDescriptorSddlForm($readSddl)
    $writeAcl = [Security.AccessControl.FileSecurity]::new()
    $writeAcl.SetSecurityDescriptorSddlForm('O:BAG:BAD:P(A;;FA;;;SY)(A;;FA;;;BA)')
    function Set-FixtureParent([string]$Contents, [DateTime]$LastWrite) {
        if (Test-Path -LiteralPath $Parent) {
            Set-Acl -LiteralPath $Parent -AclObject $writeAcl
            [IO.File]::SetAttributes($Parent, [IO.FileAttributes]::Normal)
        }
        [IO.File]::WriteAllText($Parent, $Contents, [Text.UTF8Encoding]::new($false))
        [IO.File]::SetLastWriteTimeUtc($Parent, $LastWrite)
        [IO.File]::SetAttributes($Parent, [IO.FileAttributes]::ReadOnly)
        Set-Acl -LiteralPath $Parent -AclObject $readAcl
    }
    function Assert-FixtureRejected([string]$Name) {
        $rejected = $false
        try {
            $unexpected = Open-RunnerVerifiedParent $Parent $ParentSha256 $record
            $unexpected.Dispose()
        } catch {
            if ([string]$_ -notlike '*parent changed since verification; restart the service to re-verify*') { throw }
            $rejected = $true
        }
        if (-not $rejected) { throw "FAIL $Name : changed parent was accepted" }
        $reuseRejected = $false
        try { $null = Reuse-RunnerParentVerification $Parent $ParentSha256 $record } catch {
            if ([string]$_ -notlike '*parent changed since verification; restart the service to re-verify*') { throw }
            $reuseRejected = $true
        }
        if (-not $reuseRejected) { throw "FAIL $Name : reused parent record accepted" }
        Write-Host "PASS $Name"
        $script:count++
    }
    try {
        $lastWrite = [DateTime]::UtcNow.AddDays(-1)
        Set-FixtureParent 'original' $lastWrite
        $ParentSha256 = (Get-FileHash -LiteralPath $Parent -Algorithm SHA256).Hash
        $hashLog = Join-Path $fixture 'parent-hash.jsonl'
        $verification = Confirm-RunnerParentVerification $Parent $ParentSha256 $record $hashLog
        if ($verification.cache_hit) { throw 'FAIL parent-identity-record-written: missing record did not require verification' }
        if (-not (Test-Path -LiteralPath $record)) { throw 'FAIL parent-identity-record-written' }
        Write-Host 'PASS parent-identity-record-written'
        $count++
        $verification = Reuse-RunnerParentVerification $Parent $ParentSha256 $record
        if (-not $verification.cache_hit -or $verification.hash_duration_ms -ne 0) { throw 'FAIL parent-identity-reuses-valid-record-without-hashing' }
        Write-Host 'PASS parent-identity-reuses-valid-record-without-hashing'
        $count++
        $hashEvent = Get-Content -LiteralPath $hashLog -Raw | ConvertFrom-Json
        if ($hashEvent.event -ne 'parent-sha256' -or -not $hashEvent.hash_matches_configuration -or $hashEvent.hash_duration_ms -lt 0 -or -not $hashEvent.change_time_filetime) { throw 'FAIL parent-identity-startup-hash-duration-logged' }
        $recordData = Get-Content -LiteralPath $record -Raw | ConvertFrom-Json
        if ($recordData.schema -ne 2 -or -not $recordData.change_time_filetime) { throw 'FAIL parent-identity-change-time-recorded' }
        Write-Host 'PASS parent-identity-change-time-recorded'
        $count++
        Write-Host 'PASS parent-identity-startup-hash-duration-logged'
        $count++
        $verification = Confirm-RunnerParentVerification $Parent $ParentSha256 $record $hashLog
        if ($verification.cache_hit -or $verification.hash_duration_ms -lt 0 -or -not $verification.change_time_filetime) { throw 'FAIL parent-identity-startup-rehashes-existing-record' }
        $hashEvents = @(Get-Content -LiteralPath $hashLog | ForEach-Object { $_ | ConvertFrom-Json })
        if ($hashEvents.Count -ne 2 -or -not $hashEvents[1].hash_matches_configuration -or -not $hashEvents[1].change_time_filetime) { throw 'FAIL parent-identity-startup-rehashes-existing-record' }
        Write-Host 'PASS parent-identity-startup-rehashes-existing-record'
        $count++
        # Run the exact assignment from Prepare against the tiny sealed file.
        . ([scriptblock]::Create($guards[0].Extent.Text))
        $before = Get-RunnerParentIdentity $parentGuard
        $reader = [Motor.ParentFile]::Open($Parent)
        $reader.Dispose()
        $parentGuard.Dispose(); $parentGuard = $null
        Write-Host 'PASS parent-identity-unchanged-and-concurrent-reader'
        $count++

        foreach ($sddl in @($readSddl.Replace("(A;;FR;;;$capability)", "(A;;FRFW;;;$capability)"), ($readSddl + '(A;;FR;;;BU)'))) {
            $badAcl = [Security.AccessControl.FileSecurity]::new()
            $badAcl.SetSecurityDescriptorSddlForm($sddl)
            Set-Acl -LiteralPath $Parent -AclObject $badAcl
            $probe = [Motor.ParentFile]::Open($Parent)
            $rejected = $false
            try { Assert-RunnerFileAcl $probe -ParentFile } catch { $rejected = $true } finally {
                $probe.Dispose()
                Set-Acl -LiteralPath $Parent -AclObject $readAcl
            }
            if (-not $rejected) { throw 'FAIL parent-capability-read-only-and-no-other-grant' }
        }
        Write-Host 'PASS parent-capability-read-only-and-no-other-grant'
        $count++

        # Model the runner's lifetime guard, not just a per-job check. Make the
        # ACL writable so the failure proves sharing rather than access denial.
        $lifetimeGuard = [Motor.ParentFile]::Open($Parent)
        Set-Acl -LiteralPath $Parent -AclObject $writeAcl
        [IO.File]::SetAttributes($Parent, [IO.FileAttributes]::Normal)
        $denied = $false
        try {
            $writer = [IO.File]::Open($Parent, [IO.FileMode]::Open, [IO.FileAccess]::ReadWrite, ([IO.FileShare]::ReadWrite -bor [IO.FileShare]::Delete))
            try {
                $bytes = [Text.Encoding]::UTF8.GetBytes('modified')
                $writer.Write($bytes, 0, $bytes.Length)
            } finally { $writer.Dispose() }
        } catch {
            if (($_.Exception.GetBaseException().HResult -band 0xffff) -ne 32) { throw }
            $denied = $true
        } finally {
            [IO.File]::SetAttributes($Parent, [IO.FileAttributes]::ReadOnly)
            Set-Acl -LiteralPath $Parent -AclObject $readAcl
        }
        if (-not $denied) { throw 'FAIL parent-lifetime-guard-blocks-same-size-content-rewrite' }
        Write-Host 'PASS parent-lifetime-guard-blocks-same-size-content-rewrite'
        $count++
        # The parent is fresh under a GUID-named directory on every run. Do not
        # infer a ChangeTime update from the preceding sharing test's ACL writes:
        # they can finish within one filesystem timestamp tick. Repeatedly flip
        # Hidden to its opposite and back, preserving every original attribute,
        # until the held handle observes the change or the bounded window ends.
        $changeBefore = [Motor.ParentFile]::ChangeTime($lifetimeGuard)
        $changeAfter = $changeBefore
        $originalAttributes = [IO.File]::GetAttributes($Parent)
        $flippedAttributes = [IO.FileAttributes]([int]$originalAttributes -bxor [int][IO.FileAttributes]::Hidden)
        $changeClock = [Diagnostics.Stopwatch]::StartNew()
        try {
            Set-Acl -LiteralPath $Parent -AclObject $writeAcl
            do {
                [IO.File]::SetAttributes($Parent, $flippedAttributes)
                [IO.File]::SetAttributes($Parent, $originalAttributes)
                $changeAfter = [Motor.ParentFile]::ChangeTime($lifetimeGuard)
                if ($changeAfter -gt $changeBefore) { break }
                Start-Sleep -Milliseconds 25
            } while ($changeClock.ElapsedMilliseconds -lt 2000)
        } finally {
            [IO.File]::SetAttributes($Parent, $originalAttributes)
            Set-Acl -LiteralPath $Parent -AclObject $readAcl
        }
        $after = Get-RunnerParentIdentity $lifetimeGuard
        foreach ($key in $before.Keys) {
            if ($before[$key] -cne $after[$key]) { throw "FAIL ChangeTime-only fixture unexpectedly changed $key" }
        }
        if ($changeAfter -le $changeBefore) { throw 'FAIL ChangeTime-only fixture did not advance ChangeTime within 2 seconds of attribute flips' }
        $parentGuard = Open-RunnerVerifiedParent $Parent $ParentSha256 $record
        $parentGuard.Dispose(); $parentGuard = $null
        Write-Host 'PASS parent-identity-accepts-changetime-only-update'
        $count++
        $verification = Reuse-RunnerParentVerification $Parent $ParentSha256 $record
        if (-not $verification.cache_hit -or $verification.hash_duration_ms -ne 0) { throw 'FAIL parent-identity-reuse-ignores-changetime-only-update' }
        Write-Host 'PASS parent-identity-reuse-ignores-changetime-only-update'
        $count++

        $changeProbe = Join-Path $fixture 'same-size-change-probe.bin'
        [IO.File]::WriteAllText($changeProbe, 'original', [Text.UTF8Encoding]::new($false))
        $probe = [Motor.ParentFile]::Open($changeProbe)
        $probeBefore = [Motor.ParentFile]::ChangeTime($probe)
        $probe.Dispose()
        $probeLastWrite = [IO.File]::GetLastWriteTimeUtc($changeProbe)
        $probeAfter = $probeBefore
        $probeClock = [Diagnostics.Stopwatch]::StartNew()
        do {
            $contents = if (([IO.File]::ReadAllText($changeProbe) -ceq 'original')) { 'modified' } else { 'original' }
            [IO.File]::WriteAllText($changeProbe, $contents, [Text.UTF8Encoding]::new($false))
            [IO.File]::SetLastWriteTimeUtc($changeProbe, $probeLastWrite)
            $probe = [Motor.ParentFile]::Open($changeProbe)
            $probeAfter = [Motor.ParentFile]::ChangeTime($probe)
            $probe.Dispose()
            if ($probeAfter -gt $probeBefore) { break }
            Start-Sleep -Milliseconds 25
        } while ($probeClock.ElapsedMilliseconds -lt 2000)
        if ($probeAfter -le $probeBefore) { throw 'FAIL parent-identity-change-time-detects-same-size-rewrite-restored-lastwrite' }
        Write-Host 'PASS parent-identity-change-time-detects-same-size-rewrite-restored-lastwrite'
        $count++
        $lifetimeGuard.Dispose(); $lifetimeGuard = $null

        # The service is stopped here. Reuse applies the same identity-only
        # rule as Prepare/Run; the next service start's full hash rejects content
        # rewritten with the original size and LastWriteTime.
        Set-FixtureParent 'modified' $lastWrite
        $verification = Reuse-RunnerParentVerification $Parent $ParentSha256 $record
        if (-not $verification.cache_hit -or $verification.hash_duration_ms -ne 0) { throw 'FAIL parent-identity-reuse-uses-stable-identity-only' }
        Write-Host 'PASS parent-identity-reuse-uses-stable-identity-only'
        $count++
        $denied = $false
        try { $null = Confirm-RunnerParentVerification $Parent $ParentSha256 $record $hashLog } catch {
            if ([string]$_ -notlike '*parent SHA-256 mismatch*') { throw }
            $denied = $true
        }
        if (-not $denied -or (Test-Path -LiteralPath $record)) { throw 'FAIL parent-identity-startup-rehash-catches-same-size-rewrite-restored-lastwrite' }
        Write-Host 'PASS parent-identity-startup-rehash-catches-same-size-rewrite-restored-lastwrite'
        $count++

        Set-FixtureParent 'original' $lastWrite
        $ParentSha256 = (Get-FileHash -LiteralPath $Parent -Algorithm SHA256).Hash
        $null = Write-RunnerParentVerification $Parent $ParentSha256 $record
        Set-FixtureParent 'modified' $lastWrite.AddMinutes(1)
        Assert-FixtureRejected 'parent-identity-rejects-same-size-rewrite-with-changed-lastwrite'
        $metadata = @{parent=$Parent; sha256=$ParentSha256}
        $denied = $false
        try {
            . ([scriptblock]::Create($guards[1].Extent.Text))
            $parentGuard.Dispose(); $parentGuard = $null
        } catch {
            if ([string]$_ -notlike '*parent changed since verification; restart the service to re-verify*') { throw }
            $denied = $true
        }
        if (-not $denied) { throw 'FAIL parent-identity-run-rejects-change-before-boot' }
        Write-Host 'PASS parent-identity-run-rejects-change-before-boot'
        $count++
        $denied = $false
        try { $null = Confirm-RunnerParentVerification $Parent $ParentSha256 $record } catch {
            if ([string]$_ -notlike '*parent SHA-256 mismatch*') { throw }
            $denied = $true
        }
        if (-not $denied -or (Test-Path -LiteralPath $record)) { throw 'FAIL parent-identity-startup-rejects-changed-content' }
        Write-Host 'PASS parent-identity-startup-rejects-changed-content'
        $count++

        Set-FixtureParent 'original' $lastWrite
        $null = Write-RunnerParentVerification $Parent $ParentSha256 $record
        Set-Acl -LiteralPath $Parent -AclObject $writeAcl
        [IO.File]::SetAttributes($Parent, [IO.FileAttributes]::Normal)
        [IO.File]::SetLastWriteTimeUtc($Parent, $lastWrite.AddHours(1))
        [IO.File]::SetAttributes($Parent, [IO.FileAttributes]::ReadOnly)
        Set-Acl -LiteralPath $Parent -AclObject $readAcl
        Assert-FixtureRejected 'parent-identity-rejects-timestamp-change'
        $verification = Confirm-RunnerParentVerification $Parent $ParentSha256 $record
        if ($verification.cache_hit) { throw 'FAIL parent-identity-startup-refreshes-changed-identity' }
        $parentGuard = Open-RunnerVerifiedParent $Parent $ParentSha256 $record
        $parentGuard.Dispose(); $parentGuard = $null
        Write-Host 'PASS parent-identity-startup-refreshes-changed-identity'
        $count++

        Set-FixtureParent 'original' $lastWrite
        $null = Write-RunnerParentVerification $Parent $ParentSha256 $record
        $metadata = @{parent=$Parent; sha256=$ParentSha256}
        . ([scriptblock]::Create($guards[1].Extent.Text))
        $parentGuard.Dispose(); $parentGuard = $null
        Write-Host 'PASS parent-identity-run-rechecks-before-boot'
        $count++
        Remove-Item -LiteralPath $record -Force
        Assert-FixtureRejected 'parent-identity-rejects-missing-record'
        $null = Write-RunnerParentVerification $Parent $ParentSha256 $record
        Set-Content -LiteralPath $record -Value '{broken json' -Encoding ascii
        Assert-FixtureRejected 'parent-identity-rejects-unreadable-record'
        $verification = Confirm-RunnerParentVerification $Parent $ParentSha256 $record
        if ($verification.cache_hit) { throw 'FAIL parent-identity-startup-refreshes-unreadable-record' }
        Write-Host 'PASS parent-identity-startup-refreshes-unreadable-record'
        $count++
        $failed = $false
        try { Write-RunnerParentVerification $Parent ('0' * 64) $record } catch { $failed = $true }
        if (-not $failed -or (Test-Path -LiteralPath $record)) { throw 'FAIL parent-identity-failed-validation-invalidates-record' }
        Write-Host 'PASS parent-identity-failed-validation-invalidates-record'
        $count++

        # Use a writable file so a denied write proves sharing, not the sealed ACL.
        $sharingFile = Join-Path $fixture 'sharing.bin'
        [IO.File]::WriteAllText($sharingFile, 'sharing')
        $parentGuard = [Motor.ParentFile]::Open($sharingFile)
        foreach ($access in @([IO.FileAccess]::ReadWrite, [IO.FileAccess]::Write)) {
            $denied = $false
            try {
                $writer = [IO.File]::Open($sharingFile, [IO.FileMode]::Open, $access, [IO.FileShare]::ReadWrite)
                $writer.Dispose()
            } catch { $denied = $true }
            if (-not $denied) { throw 'FAIL parent-guard-denies-write-and-delete: write admitted' }
        }
        $denied = $false
        try { [IO.File]::Delete($sharingFile) } catch { $denied = $true }
        if (-not $denied) { throw 'FAIL parent-guard-denies-write-and-delete: delete admitted' }
        $parentGuard.Dispose(); $parentGuard = $null
        $writer = [IO.File]::Open($sharingFile, [IO.FileMode]::Open, [IO.FileAccess]::ReadWrite, [IO.FileShare]::ReadWrite)
        $denied = $false
        try { $unexpected = [Motor.ParentFile]::Open($sharingFile); $unexpected.Dispose() } catch { $denied = $true } finally { $writer.Dispose() }
        if (-not $denied) { throw 'FAIL parent-guard-refuses-existing-writer' }
        [IO.File]::Delete($sharingFile)
        Write-Host 'PASS parent-guard-denies-write-and-delete-and-refuses-existing-writer'
        $count++

        # Run the production peer evidence validators with synthetic VM logs.
        $peerNonce = '0123456789abcdef0123456789abcdef'
        $nonce = $peerNonce
        $victimVm = [Guid]'11111111-2222-3333-4444-555555555555'
        $attackerVm = [Guid]'aaaaaaaa-bbbb-cccc-dddd-eeeeeeeeeeee'
        $frameText = ConvertTo-Json -InputObject ([ordered]@{kind='hello'; nonce=$nonce; version=4}) -Compress
        $framePayload = [Text.UTF8Encoding]::new($false, $true).GetBytes($frameText)
        $frameLength = $framePayload.Length
        $frame = [byte[]]::new($frameLength + 4)
        $frame[0] = [byte](($frameLength -shr 24) -band 0xff)
        $frame[1] = [byte](($frameLength -shr 16) -band 0xff)
        $frame[2] = [byte](($frameLength -shr 8) -band 0xff)
        $frame[3] = [byte]($frameLength -band 0xff)
        [Array]::Copy($framePayload, 0, $frame, 4, $frameLength)
        $null = Convert-RunnerPeerHello $frame $nonce
        $peerStart = [DateTimeOffset]::Parse('2026-10-22T10:00:00Z')
        $peerTimeline = [ordered]@{
            hello_validated=$true; victim_start_ms=1000; boot_observed_ms=2000
            attacker_start_ms=22000; proof_observed_ms=23000; host_connect_ms=24000
            hello_ms=25000; settled_ms=85000; victim_start_utc=$peerStart.ToString('o')
            boot_utc=$peerStart.AddSeconds(1).ToString('o')
            boot_observed_utc=$peerStart.AddSeconds(2).ToString('o')
            attacker_start_utc=$peerStart.AddSeconds(22).ToString('o')
            proof_observed_utc=$peerStart.AddSeconds(23).ToString('o')
            host_connect_utc=$peerStart.AddSeconds(24).ToString('o')
            hello_utc=$peerStart.AddSeconds(25).ToString('o')
            boot_event_id=18601; boot_record_id=42; boot_vm_id=$victimVm.ToString('D')
            victim_vm_id=$victimVm.ToString('D'); attacker_vm_id=$attackerVm.ToString('D')
            hello_nonce=$peerNonce
        }
        $peerProof = @([pscustomobject]@{target=$victimVm.ToString('D'); transport_connected=$false})
        $peerLog = "boot nonce=$peerNonce`nhvsock listening`nhvsock refused peer vm_id=e0e16197dd564a1091955ee7a155a838`nhvsock accepted peer vm_id=a42e7cdad03f480c9cc2a4de20abb878`n"
        $peerResult = Assert-RunnerPeerEvidence $nonce $victimVm $attackerVm $peerProof $peerTimeline $peerLog
        if ($peerResult -ne 'cross-vm-attempt-did-not-consume-single-listener') { throw 'FAIL peer-false-with-final-hello-and-single-parent-accept' }
        Write-Host 'PASS peer-false-with-final-hello-and-single-parent-accept'
        $count++
        function Assert-PeerFixtureFailure([string]$Name, [scriptblock]$Action) {
            $rejected = $false
            try { & $Action | Out-Null } catch { $rejected = $true }
            if (-not $rejected) { throw "FAIL $Name" }
            Write-Host "PASS $Name"
            $script:count++
        }
        function Copy-PeerTimeline($Timeline) {
            $copy = [ordered]@{}
            foreach ($key in $Timeline.Keys) { $copy[$key] = $Timeline[$key] }
            return $copy
        }
        $badTimeline = Copy-PeerTimeline $peerTimeline
        $badTimeline['hello_validated'] = $false
        Assert-PeerFixtureFailure 'peer-host-hello-failure-is-unproven' {
            Assert-RunnerPeerEvidence $peerNonce $victimVm $attackerVm $peerProof $badTimeline $peerLog
        }
        $badTimeline = Copy-PeerTimeline $peerTimeline
        $badTimeline['attacker_start_ms'] = 21999
        $badTimeline['attacker_start_utc'] = $peerStart.AddMilliseconds(21999).ToString('o')
        Assert-PeerFixtureFailure 'peer-attacker-before-boot-margin-is-unproven' {
            Assert-RunnerPeerEvidence $peerNonce $victimVm $attackerVm $peerProof $badTimeline $peerLog
        }
        $connectedProof = @([pscustomobject]@{target=$victimVm.ToString('D'); transport_connected=$true})
        Assert-PeerFixtureFailure 'peer-connected-attacker-needs-native-refusal' {
            Assert-RunnerPeerEvidence $peerNonce $victimVm $attackerVm $connectedProof $peerTimeline $peerLog
        }
        $noLoopback = $peerLog.Replace("hvsock refused peer vm_id=e0e16197dd564a1091955ee7a155a838`n", '')
        Assert-PeerFixtureFailure 'peer-requires-native-loopback-refusal' {
            Assert-RunnerPeerEvidence $peerNonce $victimVm $attackerVm $peerProof $peerTimeline $noLoopback
        }
        $nonParent = $peerLog.Replace('a42e7cdad03f480c9cc2a4de20abb878', 'bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb')
        Assert-PeerFixtureFailure 'peer-rejects-non-parent-accept' {
            Assert-RunnerPeerEvidence $peerNonce $victimVm $attackerVm $peerProof $peerTimeline $nonParent
        }

        function New-PeerBootXml([Guid]$VmId, [string]$SystemTime, [long]$RecordId) {
            return "<Event xmlns='http://schemas.microsoft.com/win/2004/08/events/event'><System><EventID>18601</EventID><Channel>Microsoft-Windows-Hyper-V-Worker-Admin</Channel><TimeCreated SystemTime='$SystemTime'/><EventRecordID>$RecordId</EventRecordID></System><UserData><VmlEventLog><VmId>$VmId</VmId></VmlEventLog></UserData></Event>"
        }
        function New-PeerEvent([long]$RecordId, [string]$Xml) {
            $event = [pscustomobject]@{RecordId=$RecordId; Xml=$Xml}
            $event | Add-Member -MemberType ScriptMethod -Name ToXml -Value { $this.Xml }
            return $event
        }
        Initialize-RunnerPeerSocket
        $upperXml = New-PeerBootXml $victimVm $peerStart.AddSeconds(1).ToString('o') 42
        $upperXml = $upperXml.Replace($victimVm.ToString('D'), $victimVm.ToString('D').ToUpperInvariant())
        $bootXml = $upperXml
        $since = $peerStart.UtcDateTime
        if (-not (Get-RunnerPeerBootMatch $bootXml $victimVm $since)) { throw 'FAIL peer-boot-uppercase-userdata-guid-matches' }
        Write-Host 'PASS peer-boot-uppercase-userdata-guid-matches'
        $count++
        $otherXml = New-PeerBootXml $attackerVm $peerStart.AddSeconds(1).ToString('o') 43
        if ($null -ne (Get-RunnerPeerBootMatch $otherXml $victimVm $peerStart.UtcDateTime)) { throw 'FAIL peer-boot-different-vm-is-rejected' }
        Write-Host 'PASS peer-boot-different-vm-is-rejected'
        $count++
        $oldXml = New-PeerBootXml $victimVm $peerStart.AddSeconds(-5).ToString('o') 44
        if ($null -ne (Get-RunnerPeerBootMatch $oldXml $victimVm $peerStart.UtcDateTime)) { throw 'FAIL peer-boot-event-before-start-is-rejected' }
        Write-Host 'PASS peer-boot-event-before-start-is-rejected'
        $count++
        $script:peerBootPoll = 0
        # A real event's RecordId equals the EventRecordID in its XML, which is
        # what the match reports, so each fixture event carries its XML's id.
        $script:peerBootUnrelated = New-PeerEvent 43 $otherXml
        $script:peerBootCurrent = New-PeerEvent 42 $upperXml
        function global:Get-WinEvent {
            [CmdletBinding()]
            param([hashtable]$FilterHashtable)
            $script:peerBootPoll++
            if ($script:peerBootPoll -eq 1) { return $script:peerBootUnrelated }
            return $script:peerBootCurrent
        }
        $eventPath = Join-Path $fixture 'peer-boot-fixture.xml'
        $bootMatch = Wait-RunnerPeerBoot $victimVm $since $eventPath
        $queryEvidence = Get-Content -LiteralPath ($eventPath + '.queries.json') -Raw | ConvertFrom-Json
        if ($script:peerBootPoll -lt 2 -or $bootMatch.record_id -ne 42 -or $queryEvidence.queries.Count -lt 2) { throw 'FAIL peer-boot-polls-past-unrelated-event-and-retains-queries' }
        Write-Host 'PASS peer-boot-polls-past-unrelated-event-and-retains-queries'
        $count++
        Remove-Item Function:\Get-WinEvent

        function global:Complete-SpikeNativeProcess($Monitor, [string]$Stdout, [string]$Stderr) {
            [IO.File]::WriteAllText($Stdout, '')
            [IO.File]::WriteAllText($Stderr, '')
            return [pscustomobject]@{exitCode=$script:peerMonitorExit; stdout=''; stderr=''}
        }
        function New-MonitorFixture([int]$ExitAfterWaits, [int]$ProcessId) {
            $process = [pscustomobject]@{Id=$ProcessId; Waits=0; ExitAfterWaits=$ExitAfterWaits; Killed=$false}
            $process | Add-Member -MemberType ScriptMethod -Name WaitForExit -Value {
                param([int]$Milliseconds)
                $this.Waits++
                return $this.Waits -ge $this.ExitAfterWaits
            }
            $process | Add-Member -MemberType ScriptMethod -Name Kill -Value { $this.Killed = $true }
            return [pscustomobject]@{job="fixture-$ProcessId"; process=$process}
        }
        $script:peerMonitorExit = 137
        $monitorStatus = Stop-RunnerPeerMonitor (New-MonitorFixture 2 43210) $fixture 'fixture boot-gate failure'
        if (-not $monitorStatus.forced -or -not $monitorStatus.terminated -or $monitorStatus.reason -notmatch 'fixture boot-gate failure' -or -not (Test-Path (Join-Path $fixture 'peer-monitor-43210.status.json'))) { throw 'FAIL peer-monitor-intentional-kill-retains-reason-with-empty-output' }
        Write-Host 'PASS peer-monitor-intentional-kill-retains-reason-with-empty-output'
        $count++
        $script:peerMonitorExit = 7
        $monitorStatus = Stop-RunnerPeerMonitor (New-MonitorFixture 1 43211) $fixture $null
        if (-not $monitorStatus.terminated -or $monitorStatus.failure -notmatch 'Run monitor exited nonzero') { throw 'FAIL peer-monitor-unexpected-nonzero-has-explicit-reason' }
        Write-Host 'PASS peer-monitor-unexpected-nonzero-has-explicit-reason'
        $count++
        $monitorStatus = Stop-RunnerPeerMonitor (New-MonitorFixture 99 43212) $fixture $null
        $qualifySource = [IO.File]::ReadAllText((Join-Path $PSScriptRoot 'qualify.ps1'))
        if ($monitorStatus.terminated -or $qualifySource -notmatch 'if \(-not \$status\.terminated\) \{ \$canRemove = \$false \}') { throw 'FAIL peer-monitor-unreaped-preserves-disks' }
        Write-Host 'PASS peer-monitor-unreaped-preserves-disks'
        $count++
        $primaryMessage = 'primary boot-gate failure'
        $observed = $null
        try { Assert-RunnerPeerCleanup ([IO.InvalidDataException]::new($primaryMessage)) @('secondary cleanup failure') } catch { $observed = $_.Exception.Message }
        if ($observed -cne $primaryMessage) { throw 'FAIL peer-cleanup-preserves-primary-boot-gate-error' }
        Write-Host 'PASS peer-cleanup-preserves-primary-boot-gate-error'
        $count++

        $qualifySource = [IO.File]::ReadAllText((Join-Path $PSScriptRoot 'qualify.ps1'))
        $helloCapture = $qualifySource.IndexOf('$timeline.hello_ms = $peerClock.ElapsedMilliseconds')
        $settleWait = $qualifySource.IndexOf('Wait-RunnerQualificationWindow $peerClock $timeline.hello_ms 60000')
        if ($helloCapture -lt 0 -or $settleWait -lt $helloCapture) { throw 'FAIL peer-settle-production-window-is-anchored-to-recorded-hello' }
        Write-Host 'PASS peer-settle-production-window-is-anchored-to-recorded-hello'
        $count++
        if ($qualifySource.IndexOf('$peerError = $_') -lt 0 -or $qualifySource.IndexOf('phase=$peerPhase; message=[string]$_') -lt 0) { throw 'FAIL peer-run-monitor-catch-retains-original-error-and-writes-reason' }
        Write-Host 'PASS peer-run-monitor-catch-retains-original-error-and-writes-reason'
        $count++
        Remove-Item Function:\Complete-SpikeNativeProcess,Function:\Get-WinEvent -ErrorAction SilentlyContinue
    } finally {
        if ($lifetimeGuard) { $lifetimeGuard.Dispose() }
        if ($parentGuard) { $parentGuard.Dispose() }
        if (Test-Path -LiteralPath $Parent) { Set-Acl -LiteralPath $Parent -AclObject $writeAcl }
        Remove-Item -LiteralPath $fixture -Recurse -Force
    }
} else {
    Write-Host 'SKIP native parent-identity fixtures: Windows and an elevated token are required'
}
Write-Host "PASS Windows runner PowerShell tests: $count checks; PowerShell=$($PSVersionTable.PSVersion)"
