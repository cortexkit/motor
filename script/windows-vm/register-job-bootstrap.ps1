# Registered during Windows Setup's elevated specialize pass, before first logon.
# The task avoids a UAC consent prompt on the temporary builder account.
$ErrorActionPreference = 'Stop'
if (-not (Test-Path "$PSScriptRoot\job-baseline")) { throw 'Job-image seed marker is missing' }
$action = New-ScheduledTaskAction -Execute 'C:\Windows\System32\WindowsPowerShell\v1.0\powershell.exe' -Argument "-NoProfile -ExecutionPolicy Bypass -File $PSScriptRoot\bootstrap.ps1 -JobBaseline"
$trigger = New-ScheduledTaskTrigger -AtLogOn
Register-ScheduledTask -TaskName MotorBootstrap -Action $action -Trigger $trigger -User SYSTEM -RunLevel Highest -Force | Out-Null
