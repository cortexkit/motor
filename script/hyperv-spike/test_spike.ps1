# Portable parser and predicate tests. Does not boot or inspect a Windows VM.
$ErrorActionPreference = 'Stop'
Set-StrictMode -Version Latest
$passed = 0
$failed = 0
function Test-SpikeLogic([string]$Name, [scriptblock]$Check) {
    try { & $Check; Write-Host "PASS $Name"; $script:passed++ }
    catch { Write-Host "FAIL $Name $_"; $script:failed++ }
}
function Assert-Logic($Condition) { if (-not $Condition) { throw 'assertion failed' } }
function Assert-Refused([scriptblock]$Check, [string]$Reason) {
    $message = ''
    try { & $Check | Out-Null } catch { $message = $_.Exception.Message }
    if ($message -notlike "*$Reason*") { throw "expected refusal '$Reason', got '$message'" }
}
Write-Host "PowerShell $($PSVersionTable.PSVersion) $([Environment]::OSVersion)"
$repo = (Resolve-Path "$PSScriptRoot/../..").Path
foreach ($relative in @('script/hyperv-spike/common.ps1','script/hyperv-spike/provision.ps1','script/hyperv-spike/measure.ps1','crates/motor-winagent/examples/isolation.ps1','script/windows-vm/bootstrap.ps1','script/windows-vm/setup-job-baseline.ps1')) {
    Test-SpikeLogic "parse-$relative" {
        $tokens = $null; $errors = $null
        [void][Management.Automation.Language.Parser]::ParseFile((Join-Path $repo $relative), [ref]$tokens, [ref]$errors)
        if ($errors.Count -ne 0) { throw ($errors | Out-String) }
    }
}
$tokens = $null; $errors = $null
$ast = [Management.Automation.Language.Parser]::ParseFile((Join-Path $repo 'crates/motor-winagent/examples/isolation.ps1'), [ref]$tokens, [ref]$errors)
$function = $ast.Find({ param($node) $node -is [Management.Automation.Language.FunctionDefinitionAst] -and $node.Name -eq 'Assert-MotorIsolation' }, $true)
. ([scriptblock]::Create($function.Extent.Text))
function New-Evidence {
    return @{ user='GUEST\motor-job'; admin=$false; groups=@('S-1-5-32-545'); adapters=@(); routes=@(); localCanary=$false; uncCanary=$false; secretReadable=$false }
}
Test-SpikeLogic 'isolation-accepts-standard-offline-private-guest' {
    $output = @(Assert-MotorIsolation (New-Evidence))
    Assert-Logic ($output.Count -eq 3)
    Assert-Logic ($output[0] -eq 'PASS standard-account' -and $output[1] -eq 'PASS no-network-route' -and $output[2] -eq 'PASS host-files-inaccessible')
}
Test-SpikeLogic 'standard-account-denies-admin' {
    $e = New-Evidence; $e.admin = $true
    Assert-Refused { Assert-MotorIsolation $e } 'standard-account'
}
Test-SpikeLogic 'standard-account-denies-admin-group-and-wrong-user' {
    $e = New-Evidence; $e.groups += 'S-1-5-32-544'
    Assert-Refused { Assert-MotorIsolation $e } 'standard-account'
    $e = New-Evidence; $e.user = 'GUEST\Administrator'
    Assert-Refused { Assert-MotorIsolation $e } 'standard-account'
}
Test-SpikeLogic 'offline-denies-adapter-and-route' {
    $e = New-Evidence; $e.adapters = @('Ethernet')
    Assert-Refused { Assert-MotorIsolation $e } 'no-network-route'
    $e = New-Evidence; $e.routes = @('0.0.0.0/0')
    Assert-Refused { Assert-MotorIsolation $e } 'no-network-route'
}
Test-SpikeLogic 'host-files-denies-local-unc-and-agent-secret' {
    foreach ($key in @('localCanary','uncCanary','secretReadable')) {
        $e = New-Evidence; $e[$key] = $true
        Assert-Refused { Assert-MotorIsolation $e } 'host-files-inaccessible'
    }
}
. "$PSScriptRoot/common.ps1"
Test-SpikeLogic 'pass-fail-helper-never-passes-false' { Assert-Refused { Assert-Spike $false 'sentinel' 'fixture' } 'FAIL sentinel' }
Test-SpikeLogic 'unix-time-is-milliseconds' {
    $now = Get-SpikeUnixMs
    Assert-Logic ($now -gt 1700000000000 -and $now -lt 2100000000000)
}
Test-SpikeLogic 'iso-stream-writer-compiles' {
    $ast = [Management.Automation.Language.Parser]::ParseFile((Join-Path $PSScriptRoot 'common.ps1'), [ref]$tokens, [ref]$errors)
    $source = $ast.Find({ param($node) $node -is [Management.Automation.Language.StringConstantExpressionAst] -and $node.Value -like '*public static class MotorSpikeIso*' }, $true)
    Add-Type -TypeDefinition $source.Value
}
Write-Host "Checks: $passed passed, $failed failed, 0 skipped"
if ($failed -ne 0) { exit 1 }
