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
function global:secedit.exe {
    $cfg = $args[[array]::IndexOf($args,'/cfg') + 1]
    if ($args -contains '/export') { $script:effective | Set-Content -LiteralPath $cfg -Encoding Unicode }
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
    if (($script:effective -join "`n") -match [regex]::Escape($sid)) { throw 'Uninstall retained owned SID rights' }
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
Write-Host "PASS Windows runner portable PowerShell tests: $count checks; PowerShell=$($PSVersionTable.PSVersion)"
