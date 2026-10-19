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
Write-Host "PowerShell $($PSVersionTable.PSVersion) $([Environment]::OSVersion) zone=$([TimeZoneInfo]::Local.Id)"
$repo = (Resolve-Path "$PSScriptRoot/../..").Path
foreach ($relative in @('script/hyperv-spike/common.ps1','script/hyperv-spike/process.ps1','script/hyperv-spike/boot-files.ps1','script/hyperv-spike/build-probe.ps1','script/hyperv-spike/diagnostics.ps1','script/hyperv-spike/provision.ps1','script/hyperv-spike/measure.ps1','crates/motor-winagent/examples/isolation.ps1','script/windows-vm/bootstrap.ps1','script/windows-vm/setup-job-baseline.ps1','script/windows-vm/install-powershell.ps1')) {
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
. "$PSScriptRoot/boot-files.ps1"
. "$PSScriptRoot/diagnostics.ps1"
Test-SpikeLogic 'native-argument-quoting-keeps-quotes-spaces-and-trailing-slashes' {
    Assert-Logic ((Convert-SpikeNativeArgument '') -ceq '""')
    Assert-Logic ((Convert-SpikeNativeArgument 'a b') -ceq '"a b"')
    Assert-Logic ((Convert-SpikeNativeArgument 'a"b') -ceq '"a\"b"')
    Assert-Logic ((Convert-SpikeNativeArgument 'C:\tail\') -ceq '"C:\tail\\"')
    Assert-Refused { Convert-SpikeNativeArgument ('a' + [char]0) } 'NUL'
}
Test-SpikeLogic 'managed-native-exit-zero-nonzero-and-stderr-under-stop' {
    $scratch = Join-Path ([IO.Path]::GetTempPath()) ('motor-native-' + [Guid]::NewGuid().ToString('N'))
    New-Item $scratch -ItemType Directory | Out-Null
    try {
        foreach ($code in @(0,23)) {
            $command = '[Console]::Out.Write("fixture output"); [Console]::Error.Write("Updating crates.io index"); exit ' + $code
            $encoded = [Convert]::ToBase64String([Text.Encoding]::Unicode.GetBytes($command))
            $result = Invoke-SpikeNative (Get-Process -Id $PID).Path @('-NoProfile','-NonInteractive','-EncodedCommand',$encoded) $scratch "fixture-$code"
            Assert-Logic ($result.exitCode -eq $code -and $result.stdout -ceq 'fixture output' -and $result.stderr -ceq 'Updating crates.io index')
        }
    } finally { Remove-Item $scratch -Recurse -Force }
}
Test-SpikeLogic 'managed-native-argument-roundtrip-with-spaces-quotes-and-empty-value' {
    $scratch = Join-Path ([IO.Path]::GetTempPath()) ('motor-native-argv-' + [Guid]::NewGuid().ToString('N'))
    New-Item $scratch -ItemType Directory | Out-Null
    try {
        $file = Join-Path $scratch 'arguments.ps1'
        # Inspect CLR argv, before Windows PowerShell 5.1 reinterprets file args.
        Set-Content $file '[Console]::Out.Write((ConvertTo-Json -InputObject @([Environment]::GetCommandLineArgs()) -Compress))' -Encoding UTF8
        $expected = @('space value','quote"value','C:\trail\','')
        $running = Start-SpikeNativeProcess (Get-Process -Id $PID).Path (@('-NoProfile','-NonInteractive','-File',$file) + $expected)
        try {
            Assert-Logic ($running.process.WaitForExit(10000))
            $result = Complete-SpikeNativeProcess $running '' ''
            $allArguments = ConvertFrom-Json -InputObject $result.stdout
            $actual = $allArguments[($allArguments.Count - 4)..($allArguments.Count - 1)]
            if ($result.exitCode -ne 0 -or $actual.Count -ne 4 -or ($actual -join '|') -cne ($expected -join '|')) { throw "Native argv mismatch: actualCount=$($actual.Count) expected=$($expected -join '|') actual=$($actual -join '|') json=$($result.stdout) stderr=$($result.stderr)" }
        } finally { Close-SpikeNativeProcess $running }
    } finally { Remove-Item $scratch -Recurse -Force }
}
Test-SpikeLogic 'managed-native-capture-drains-both-large-streams' {
    $encoded = [Convert]::ToBase64String([Text.Encoding]::Unicode.GetBytes('[Console]::Out.Write(("O"*200000)); [Console]::Error.Write(("E"*200000))'))
    $running = Start-SpikeNativeProcess (Get-Process -Id $PID).Path @('-NoProfile','-NonInteractive','-EncodedCommand',$encoded)
    try {
        Assert-Logic ($running.process.WaitForExit(10000))
        $result = Complete-SpikeNativeProcess $running '' ''
        Assert-Logic ($result.exitCode -eq 0 -and $result.stdout.Length -eq 200000 -and $result.stderr.Length -eq 200000)
    } finally { Close-SpikeNativeProcess $running }
}
Test-SpikeLogic 'managed-native-timeout-kills-before-capturing-status' {
    $encoded = [Convert]::ToBase64String([Text.Encoding]::Unicode.GetBytes('Start-Sleep -Seconds 10'))
    $running = Start-SpikeNativeProcess (Get-Process -Id $PID).Path @('-NoProfile','-NonInteractive','-EncodedCommand',$encoded)
    try {
        Assert-Logic (-not $running.process.WaitForExit(100))
        $running.process.Kill(); $running.process.WaitForExit()
        $result = Complete-SpikeNativeProcess $running '' ''
        Assert-Logic ($result.exitCode -ne 0)
    } finally { Close-SpikeNativeProcess $running }
}
Test-SpikeLogic 'missing-native-exit-status-never-becomes-zero' {
    $process = [pscustomobject]@{ HasExited=$true; ExitCode=$null }
    $process | Add-Member ScriptMethod WaitForExit {}
    $stdout = [Threading.Tasks.TaskCompletionSource[string]]::new(); $stdout.SetResult('')
    $stderr = [Threading.Tasks.TaskCompletionSource[string]]::new(); $stderr.SetResult('')
    Assert-Refused { Complete-SpikeNativeProcess ([pscustomobject]@{process=$process; stdout=$stdout.Task; stderr=$stderr.Task}) '' '' } 'exit status is missing'
}
Test-SpikeLogic 'guest-evidence-never-descends-through-reparse-ancestors' {
    $visited = [Collections.Generic.List[string]]::new()
    $read = {
        param($path)
        $leaf = Split-Path $path -Leaf
        $visited.Add($leaf)
        if ($leaf -eq 'Temp') { throw 'Followed a guest reparse ancestor' }
        $attributes = if ($leaf -eq 'Local') { [IO.FileAttributes]::ReparsePoint } else { [IO.FileAttributes]::Directory }
        [pscustomobject]@{ FullName=$path; Attributes=$attributes }
    }
    $item = Get-SpikePlainGuestItem ([IO.Path]::GetTempPath()) 'Users\motor-job\AppData\Local\Temp\motor-tree.tar' $read
    Assert-Logic ($null -eq $item -and ($visited -join ',') -eq 'Users,motor-job,AppData,Local')
    Assert-Refused { Get-SpikePlainGuestItem ([IO.Path]::GetTempPath()) 'Users\..\Windows' $read } 'Invalid guest diagnostic path'
}
Test-SpikeLogic 'bcdboot-arguments-bind-source-and-explicit-esp-with-verbose-output' {
    $arguments = @(Get-SpikeBcdBootArguments 'F:\Windows' 'E')
    Assert-Logic (($arguments -join '|') -eq 'F:\Windows|/s|E:|/f|UEFI|/v')
    $arguments = @(Get-SpikeBcdBootArguments 'F:\Windows' 'G' -Offline)
    Assert-Logic (($arguments -join '|') -eq 'F:\Windows|/s|G:|/f|UEFI|/v|/offline')
}
function New-EspPartition { return @{ DiskNumber=2; PartitionNumber=1; GptType='{c12a7328-f81f-11d2-ba4b-00a0c93ec93b}'; DriveLetter='E' } }
function New-EspVolume { return @{ FileSystem='FAT32'; DriveLetter='E' } }
Test-SpikeLogic 'esp-requires-fat32-not-an-assumed-format-request' {
    Assert-SpikeEspState (New-EspPartition) (New-EspVolume) 2
    $v = New-EspVolume; $v.FileSystem = 'NTFS'
    Assert-Refused { Assert-SpikeEspState (New-EspPartition) $v 2 } 'esp-fat32'
}
Test-SpikeLogic 'esp-requires-matching-live-letter-and-disk-identity' {
    $p = New-EspPartition; $p.DriveLetter = ''
    Assert-Refused { Assert-SpikeEspState $p (New-EspVolume) 2 } 'esp-mounted-letter'
    $p = New-EspPartition; $p.DriveLetter = 'F'
    Assert-Refused { Assert-SpikeEspState $p (New-EspVolume) 2 } 'esp-mounted-letter'
    $p = New-EspPartition; $p.DiskNumber = 0
    Assert-Refused { Assert-SpikeEspState $p (New-EspVolume) 2 } 'esp-partition-identity'
    $p = New-EspPartition; $p.GptType = '{ebd0a0a2-b9e5-4433-87c0-68b6b72699c7}'
    Assert-Refused { Assert-SpikeEspState $p (New-EspVolume) 2 } 'esp-partition-identity'
}
$ast = [Management.Automation.Language.Parser]::ParseFile((Join-Path $repo 'script/windows-vm/install-powershell.ps1'), [ref]$tokens, [ref]$errors)
foreach ($name in @('Get-MotorNativeExitHex','Invoke-MotorPowerShellProbe','Assert-MotorPowerShellProbe')) {
    $function = $ast.Find({ param($node) $node -is [Management.Automation.Language.FunctionDefinitionAst] -and $node.Name -eq $name }, $true)
    . ([scriptblock]::Create($function.Extent.Text))
}
Test-SpikeLogic 'native-exit-hex-preserves-coreclr-and-loader-status' {
    Assert-Logic ((Get-MotorNativeExitHex -2146233082) -ceq '0x80131506')
    Assert-Logic ((Get-MotorNativeExitHex -1073741515) -ceq '0xC0000135')
    Assert-Logic ((Get-MotorNativeExitHex -1) -ceq '0xFFFFFFFF')
    Assert-Logic ((Get-MotorNativeExitHex 0) -ceq '0x00000000')
}
Test-SpikeLogic 'powershell-version-requires-success-exact-output-and-no-timeout' {
    $p = @{ timedOut=$false; exitCode=0; exitHex='0x00000000'; stdout="7.6.7`r`n"; stderr='' }
    Assert-MotorPowerShellProbe $p '7.6.7'
    $p.stdout = ''
    Assert-Refused { Assert-MotorPowerShellProbe $p '7.6.7' } 'Unexpected PowerShell version'
    $p.stdout = '7.6.6'
    Assert-Refused { Assert-MotorPowerShellProbe $p '7.6.7' } 'Unexpected PowerShell version'
    $p.stdout = '7.6.7'; $p.exitCode = -2146233082; $p.exitHex = '0x80131506'
    Assert-Refused { Assert-MotorPowerShellProbe $p '7.6.7' } '0x80131506'
    $p.timedOut = $true
    Assert-Refused { Assert-MotorPowerShellProbe $p '7.6.7' } 'timed out'
}
Test-SpikeLogic 'powershell-probe-captures-native-stdout-stderr-and-nonzero-exit' {
    $scratch = Join-Path ([IO.Path]::GetTempPath()) ('motor-pwsh-test-' + [Guid]::NewGuid().ToString('N'))
    New-Item $scratch -ItemType Directory | Out-Null
    try {
        $executable = (Get-Process -Id $PID).Path
        $p = Invoke-MotorPowerShellProbe $executable '$PSVersionTable.PSVersion.ToString()' $scratch
        Assert-MotorPowerShellProbe $p $PSVersionTable.PSVersion.ToString()
        $p = Invoke-MotorPowerShellProbe $executable '[Console]::Out.Write("fixture output"); [Console]::Error.Write("fixture error"); exit 23' $scratch
        Assert-Logic ($p.exitCode -eq 23 -and $p.exitHex -ceq '0x00000017')
        Assert-Logic ($p.stdout -ceq 'fixture output' -and $p.stderr -ceq 'fixture error')
        Assert-Logic ((Get-Content (Join-Path $scratch 'pwsh-probe.stderr.log') -Raw) -ceq 'fixture error')
        Assert-Logic ((Get-Content (Join-Path $scratch 'pwsh-probe.json') -Raw | ConvertFrom-Json).exitCode -eq 23)
    } finally { Remove-Item $scratch -Recurse -Force }
}
Test-SpikeLogic 'powershell-probe-timeout-kills-and-records-child' {
    $scratch = Join-Path ([IO.Path]::GetTempPath()) ('motor-pwsh-timeout-' + [Guid]::NewGuid().ToString('N'))
    New-Item $scratch -ItemType Directory | Out-Null
    try {
        $p = Invoke-MotorPowerShellProbe (Get-Process -Id $PID).Path 'Start-Sleep -Seconds 10' $scratch 100
        Assert-Logic $p.timedOut
        Assert-Refused { Assert-MotorPowerShellProbe $p '7.6.7' } 'timed out'
    } finally { Remove-Item $scratch -Recurse -Force }
}
Test-SpikeLogic 'pass-fail-helper-never-passes-false' { Assert-Refused { Assert-Spike $false 'sentinel' 'fixture' } 'FAIL sentinel' }
Test-SpikeLogic 'unix-time-is-milliseconds' {
    $now = Get-SpikeUnixMs
    Assert-Logic ($now -gt 1700000000000 -and $now -lt 2100000000000)
}
Test-SpikeLogic 'unix-time-uses-utc-epoch-independent-of-local-zone' {
    Assert-Logic ((Get-SpikeUnixMs ([DateTimeOffset]'1970-01-01T01:00:00+01:00')) -eq 0)
    Assert-Logic ((Get-SpikeUnixMs ([DateTimeOffset]'1970-01-01T01:00:01.234+01:00')) -eq 1234)
    $expected = [long](([DateTime]::UtcNow.Ticks - 621355968000000000L) / 10000)
    Assert-Logic ([Math]::Abs((Get-SpikeUnixMs) - $expected) -lt 1000)
}
Test-SpikeLogic 'iso-stream-writer-compiles' {
    $ast = [Management.Automation.Language.Parser]::ParseFile((Join-Path $PSScriptRoot 'common.ps1'), [ref]$tokens, [ref]$errors)
    $source = $ast.Find({ param($node) $node -is [Management.Automation.Language.StringConstantExpressionAst] -and $node.Value -like '*public static class MotorSpikeIso*' }, $true)
    Add-Type -TypeDefinition $source.Value
}
Write-Host "Checks: $passed passed, $failed failed, 0 skipped"
if ($failed -ne 0) { exit 1 }
& "$PSScriptRoot/test_servicing.ps1"
if (-not $?) { exit 1 }
