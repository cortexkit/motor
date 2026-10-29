<# Replace only the host payload from a verified release, in elevated Windows PowerShell 5.1. #>
[CmdletBinding(DefaultParameterSetName='Release')]
param(
    [Parameter(Mandatory, ParameterSetName='Release')][string]$Bundle,
    [Parameter(Mandatory, ParameterSetName='Rollback')][switch]$Rollback,
    [ValidateRange(1, 1800)][int]$ReadyTimeoutSeconds = 300
)
$ErrorActionPreference = 'Stop'
Set-StrictMode -Version Latest
$identity = [Security.Principal.WindowsIdentity]::GetCurrent()
$admin = ([Security.Principal.WindowsPrincipal]$identity).IsInRole([Security.Principal.WindowsBuiltInRole]::Administrator)
if (-not $admin -or $identity.User.Value -eq 'S-1-5-18') { throw 'Run update.ps1 from an elevated administrator shell, not SYSTEM; Administrators ownership is required' }
. "$PSScriptRoot\accounts.ps1"
. "$PSScriptRoot\parent-identity.ps1"
. "$PSScriptRoot\update-functions.ps1"
Invoke-RunnerHostUpdate -Root 'C:\ProgramData\MotorRunner' -Bundle $Bundle -Rollback:$Rollback -ReadyTimeoutSeconds $ReadyTimeoutSeconds
