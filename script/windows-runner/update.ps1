<# Replace only the host payload from a verified release, in elevated Windows PowerShell 5.1. #>
[CmdletBinding(DefaultParameterSetName='Release')]
param(
    [Parameter(Mandatory, ParameterSetName='Release')][string]$Bundle,
    [Parameter(Mandatory, ParameterSetName='Rollback')][switch]$Rollback,
    [Parameter(Mandatory, ParameterSetName='Metrics')][switch]$StageMetrics,
    [Parameter(Mandatory, ParameterSetName='Key')][switch]$PrintMetricsKey,
    [ValidateNotNullOrEmpty()][string]$MetricsEndpoint = '',
    [ValidateNotNullOrEmpty()][string]$MetricsHostId = '',
    [switch]$DisableMetrics,
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
. "$PSScriptRoot\service-definition.ps1"
$Root = 'C:\ProgramData\MotorRunner'
$null = Get-RunnerMetricsArguments $MetricsEndpoint $MetricsHostId
if ($DisableMetrics -and ($MetricsEndpoint -or $MetricsHostId)) { throw 'DisableMetrics cannot be combined with metrics settings' }
if ($PrintMetricsKey) {
    Assert-RunnerPrivateDirectory $Root
    Invoke-RunnerMetricsKey $Root
    return
}
if ($StageMetrics) {
    if (-not $MetricsEndpoint -and -not $DisableMetrics) { throw 'StageMetrics requires a metrics pair or DisableMetrics' }
    Assert-RunnerPrivateDirectory $Root
    $definition = Get-RunnerUpdateServiceDefinition
    $definition.binary = Set-RunnerMetricsCommand $definition.binary $MetricsEndpoint $MetricsHostId
    Set-RunnerUpdateServiceDefinition $definition "$Root\installation-native.jsonl"
    Write-Host 'Metrics command staged; MotorRunner was not stopped or started. Applies at the next planned restart.'
    return
}
Invoke-RunnerHostUpdate -Root $Root -Bundle $Bundle -Rollback:$Rollback -ReadyTimeoutSeconds $ReadyTimeoutSeconds -MetricsEndpoint $MetricsEndpoint -MetricsHostId $MetricsHostId -DisableMetrics:$DisableMetrics
