# Run qualification as SYSTEM without a Rust toolchain or SYSTEM rustup profile.
param([string]$Root = 'C:\ProgramData\MotorRunner', [Parameter(Mandatory)][string]$Python,
      [switch]$OfflineCargoOnly,
      [switch]$LocalRegistryOnly,
      [string]$Evidence = ('C:\motor-runner-evidence\' + (Get-Date -Format 'yyyyMMdd-HHmmss')))
$ErrorActionPreference = 'Stop'
Set-StrictMode -Version Latest
if ($OfflineCargoOnly -and $LocalRegistryOnly) { throw 'Choose one Cargo cache diagnostic lane' }
if ($Root -cne 'C:\ProgramData\MotorRunner' -or $Evidence -notmatch '^C:\\motor-runner-evidence\\[A-Za-z0-9-]+$') { throw 'Use the owned runner root and a dedicated external evidence directory' }
$Python = (Resolve-Path -LiteralPath $Python).Path
if (Test-Path -LiteralPath $Evidence) { throw 'Fresh evidence directory required' }
New-Item -ItemType Directory -Path $Evidence -Force | Out-Null
& icacls.exe $Evidence /inheritance:r /grant:r '*S-1-5-18:(OI)(CI)F' '*S-1-5-32-544:(OI)(CI)F' /Q | Out-Null
if ($LASTEXITCODE -ne 0) { throw 'Cannot protect evidence directory' }
$wrapper = "$Evidence\run.cmd"
$diagnosticArgument = ''
if ($OfflineCargoOnly) { $diagnosticArgument += ' -OfflineCargoOnly' }
if ($LocalRegistryOnly) { $diagnosticArgument += ' -LocalRegistryOnly' }
@"
@echo off
powershell.exe -NoLogo -NoProfile -NonInteractive -ExecutionPolicy Bypass -File "$Root\bin\qualify.ps1" -Python "$Python" -Evidence "$Evidence"$diagnosticArgument > "$Evidence\qualification.log" 2>&1
>"$Evidence\exit.txt" echo %errorlevel%
"@ | Set-Content $wrapper -Encoding ascii
$name = 'MotorRunnerQualification-' + [Guid]::NewGuid().ToString()
try {
    $action = New-ScheduledTaskAction -Execute 'C:\Windows\System32\cmd.exe' -Argument "/d /c `"$wrapper`""
    $settings = New-ScheduledTaskSettingsSet -ExecutionTimeLimit (New-TimeSpan -Minutes 45) -AllowStartIfOnBatteries -DontStopIfGoingOnBatteries
    Register-ScheduledTask -TaskName $name -Action $action -Settings $settings -User SYSTEM -RunLevel Highest -Force | Out-Null
    Start-ScheduledTask $name
    $deadline = [DateTime]::UtcNow.AddMinutes(45)
    while (-not (Test-Path "$Evidence\exit.txt")) {
        if ([DateTime]::UtcNow -ge $deadline) { Stop-ScheduledTask $name; throw "Qualification timeout; preserve $Evidence" }
        Start-Sleep -Seconds 2
    }
    $code = [int](Get-Content "$Evidence\exit.txt" -Raw).Trim()
    Get-Content "$Evidence\qualification.log" | Out-Host
    if ($code -ne 0) { throw "Qualification failed exit=$code; return the complete $Evidence directory" }
    if ($OfflineCargoOnly) { Write-Host "DIAGNOSTIC offline Cargo cache measurement collected; return $Evidence" }
    elseif ($LocalRegistryOnly) { Write-Host "DIAGNOSTIC local-registry proof collected; return $Evidence" }
    else { Write-Host "PASS qualification completed; return the complete $Evidence directory before fleet enablement" }
} finally { Unregister-ScheduledTask $name -Confirm:$false -ErrorAction SilentlyContinue }
