# Portable PowerShell parser and policy-transform fixtures. Never boots a VM.
$ErrorActionPreference = 'Stop'
Set-StrictMode -Version Latest
$files = @(Get-ChildItem $PSScriptRoot -Filter '*.ps1') + @(
    Get-Item "$PSScriptRoot\..\hyperv-spike\common.ps1"
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
$root = Join-Path ([IO.Path]::GetTempPath()) ('motor-runner-script-tests-' + [Guid]::NewGuid())
New-Item $root -ItemType Directory | Out-Null
$script:effective = @('[Unicode]','Unicode=yes','[Version]','signature="$CHICAGO$"','Revision=1','[Privilege Rights]','SeBatchLogonRight = *S-1-5-32-544','SeDenyInteractiveLogonRight = *S-1-5-32-546')
# Like the real secedit, the export names a resolvable local account instead of
# listing its SID, so the policy code must resolve names before comparing.
$script:RunnerResolveTrusteeName = { param($name) if ($name -eq 'motor-prep') { 'S-1-5-21-111-222-333-1001' } else { throw "unknown account $name" } }
function global:secedit.exe {
    $cfg = $args[[array]::IndexOf($args,'/cfg') + 1]
    if ($args -contains '/export') { $script:effective | ForEach-Object { $_ -replace '\*S-1-5-21-111-222-333-1001','motor-prep' } | Set-Content -LiteralPath $cfg -Encoding Unicode }
    elseif ($args -contains '/configure') { $script:effective = @(Get-Content -LiteralPath $cfg) }
    else { throw 'Unexpected secedit operation in fixture' }
    $global:LASTEXITCODE = 0
}
if ((Get-Command secedit.exe).CommandType -ne 'Function') { throw 'Policy fixture mock is not installed; refusing to run a real secedit command' }
try {
    $sid = 'S-1-5-21-111-222-333-1001'
    Set-RunnerBatchRights $root $sid
    foreach ($right in @('SeBatchLogonRight','SeDenyInteractiveLogonRight','SeDenyRemoteInteractiveLogonRight','SeDenyNetworkLogonRight','SeDenyServiceLogonRight')) {
        if (@($script:effective | Where-Object { $_ -match "^$right = .*\*$sid" }).Count -ne 1) { throw "Missing exact SID assignment $right" }
    }
    if (($script:effective -join "`n") -notmatch 'SeBatchLogonRight = \*S-1-5-32-544,\*S-1-5-21-111-222-333-1001') { throw 'Existing administrator trustee was lost' }
    Set-RunnerBatchRights $root $sid -Remove
    if (($script:effective -join "`n") -match ([regex]::Escape($sid) + '|motor-prep')) { throw 'Uninstall retained owned SID rights' }
    if (($script:effective -join "`n") -notmatch 'SeBatchLogonRight = \*S-1-5-32-544') { throw 'Uninstall lost unrelated administrator rights' }
    Write-Host 'PASS rights-add-remove-preserves-unrelated-trustees'
    $count++
    $rejected = $false
    try { Set-RunnerBatchRights $root 'S-1-5-32-545' } catch { $rejected = $true }
    if (-not $rejected) { throw 'Policy accepted a broad Users group SID' }
    Write-Host 'PASS rights-rejects-broad-group-sid'
    $count++
} finally {
    Remove-Item Function:\secedit.exe
    Remove-Item $root -Recurse -Force
}
# Parse the actual Prepare/Run guard assignment, not a test-only approximation.
$tokens = $null; $errors = $null
$control = [Management.Automation.Language.Parser]::ParseFile("$PSScriptRoot\hyperv.ps1", [ref]$tokens, [ref]$errors)
$guards = @($control.FindAll({ param($node) $node -is [Management.Automation.Language.AssignmentStatementAst] -and $node.Left.Extent.Text -eq '$parentGuard' -and $node.Right.Extent.Text -match '^Open-RunnerVerifiedParent ' }, $true))
if ($guards.Count -ne 2) { throw 'FAIL parent-identity-production-guards: Prepare and Run must both acquire the verified guard' }
Write-Host 'PASS parent-identity-production-guards'
$count++

if ([Environment]::OSVersion.Platform -eq [PlatformID]::Win32NT) {
    . "$PSScriptRoot\parent-identity.ps1"
    $fixture = Join-Path ([IO.Path]::GetTempPath()) ('motor-parent-tests-' + [Guid]::NewGuid())
    New-Item $fixture -ItemType Directory | Out-Null
    $Parent = Join-Path $fixture 'parent.vhdx'
    $record = Join-Path $fixture 'parent-verified.json'
    $parentRecord = $record
    $parentGuard = $null
    $readAcl = [Security.AccessControl.FileSecurity]::new()
    $readAcl.SetSecurityDescriptorSddlForm('O:BAG:BAD:P(A;;FR;;;SY)(A;;FR;;;BA)(A;;FR;;;S-1-5-83-0)')
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
        $hashEvent = Get-Content -LiteralPath $hashLog -Raw | ConvertFrom-Json
        if ($hashEvent.event -ne 'parent-sha256' -or -not $hashEvent.hash_matches_configuration -or $hashEvent.hash_duration_ms -lt 0) { throw 'FAIL parent-identity-startup-hash-duration-logged' }
        Write-Host 'PASS parent-identity-startup-hash-duration-logged'
        $count++
        $savedWriter = (Get-Item Function:\Write-RunnerParentVerification).ScriptBlock
        Set-Item Function:\Write-RunnerParentVerification -Value { throw 'FAIL parent-identity-startup-cache-hit-without-hashing: unexpected digest' }
        try {
            $verification = Confirm-RunnerParentVerification $Parent $ParentSha256 $record
            if (-not $verification.cache_hit -or $verification.hash_duration_ms -ne 0) { throw 'FAIL parent-identity-startup-cache-hit-without-hashing' }
        } finally { Set-Item Function:\Write-RunnerParentVerification -Value $savedWriter }
        Write-Host 'PASS parent-identity-startup-cache-hit-without-hashing'
        $count++
        # Run the exact assignment from Prepare against the tiny sealed file.
        . ([scriptblock]::Create($guards[0].Extent.Text))
        $before = Get-RunnerParentIdentity $parentGuard
        $reader = [Motor.ParentFile]::Open($Parent)
        $reader.Dispose()
        $parentGuard.Dispose(); $parentGuard = $null
        Write-Host 'PASS parent-identity-unchanged-and-concurrent-reader'
        $count++

        # Use equal-length bytes, then restore every compared field except ChangeTime.
        Set-FixtureParent 'modified' $lastWrite
        $probe = [Motor.ParentFile]::Open($Parent)
        try { $after = Get-RunnerParentIdentity $probe } finally { $probe.Dispose() }
        foreach ($key in $before.Keys) {
            if ($key -ne 'change_time_utc' -and $before[$key] -cne $after[$key]) { throw "FAIL same-size fixture unexpectedly changed $key" }
        }
        if ($before.change_time_utc -ceq $after.change_time_utc) { throw 'FAIL same-size fixture did not advance ChangeTime' }
        Assert-FixtureRejected 'parent-identity-rejects-same-size-rewrite-with-restored-lastwrite'
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
    } finally {
        if ($parentGuard) { $parentGuard.Dispose() }
        if (Test-Path -LiteralPath $Parent) { Set-Acl -LiteralPath $Parent -AclObject $writeAcl }
        Remove-Item -LiteralPath $fixture -Recurse -Force
    }
} else {
    Write-Host 'SKIP native parent-identity fixtures: Windows and an elevated token are required'
}
Write-Host "PASS Windows runner PowerShell tests: $count checks; PowerShell=$($PSVersionTable.PSVersion)"
