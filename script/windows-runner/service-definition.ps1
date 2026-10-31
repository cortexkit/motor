# Keep fresh installs and host-only updates on the same SCM command and environment.
function Close-RunnerQualificationGate([string]$Root) {
    Assert-RunnerPrivateDirectory "$Root\state\ally"
    $marker = [IO.File]::Open("$Root\state\ally\qualification-required", [IO.FileMode]::Create, [IO.FileAccess]::Write, [IO.FileShare]::Read)
    try { $marker.Flush($true) } finally { $marker.Dispose() }
    Write-Host 'Fleet admission closed: runner awaiting qualification. Only a full passing qualification or an elevated operator override opens it.'
}

function Assert-RunnerQualificationCapable([string]$Root) {
    if ((Get-RunnerQualificationGateVersion $Root) -cne 'qualification-gate-v1') {
        Set-Service -Name MotorRunner -StartupType Disabled -ErrorAction Stop
        throw 'MotorRunner left stopped with startup disabled: this binary cannot gate fleet admission. Install a gate-capable release before starting it. To accept ungated fleet jobs deliberately, run from an elevated shell: Set-Service -Name MotorRunner -StartupType Automatic; Start-Service -Name MotorRunner'
    }
}

function Set-RunnerServiceRecovery([string]$LogPath) {
    $null = Invoke-RunnerNative -FilePath "$env:SystemRoot\System32\sc.exe" -ArgumentList @('failure', 'MotorRunner', 'reset=', '86400', 'actions=', 'restart/5000/restart/15000/restart/60000') -Operation 'configure runner service recovery' -LogPath $LogPath
    $null = Invoke-RunnerNative -FilePath "$env:SystemRoot\System32\sc.exe" -ArgumentList @('failureflag', 'MotorRunner', '1') -Operation 'enable runner recovery on service failure' -LogPath $LogPath
}

function Open-RunnerQualificationGate([string]$Root) {
    & "$Root\bin\motor-runner.exe" resume --instance ally-qualification --state-root "$Root\state"
    if ($LASTEXITCODE -ne 0) { throw 'Qualification passed but fleet admission could not be opened; runner remains gated' }
}

function Get-RunnerMetricsArguments([string]$MetricsEndpoint, [string]$MetricsHostId) {
    if ([bool]$MetricsEndpoint -ne [bool]$MetricsHostId) { throw 'MetricsEndpoint and MetricsHostId must be supplied together' }
    if (-not $MetricsEndpoint) { return '' }
    $uri = $null
    if ($MetricsEndpoint -cnotmatch '\Ahttps://[A-Za-z0-9.\-:\[\]]+/?\z' -or
        -not [Uri]::TryCreate($MetricsEndpoint, [UriKind]::Absolute, [ref]$uri) -or
        -not $uri.Host -or $uri.Port -lt 1 -or $uri.Port -gt 65535) { throw 'MetricsEndpoint must be an HTTPS origin' }
    if ($MetricsHostId -cnotmatch '\A[A-Za-z0-9._-]{1,64}\z') { throw 'MetricsHostId must match [A-Za-z0-9._-]{1,64}' }
    return " --metrics-push --metrics-endpoint $MetricsEndpoint --metrics-host-id $MetricsHostId"
}

function Get-RunnerMetricsSettings([string]$Binary) {
    $tokens = @([regex]::Matches($Binary, '"[^"]*"|\S+') | ForEach-Object { $_.Value.Trim('"') })
    $endpoint = ''; $hostId = ''; $push = $false
    for ($i = 0; $i -lt $tokens.Count; $i++) {
        switch -CaseSensitive -Regex ($tokens[$i]) {
            '^--metrics-push$' { $push = $true }
            '^--metrics-(endpoint|host-id)(?:=(.*))?$' {
                $name = $Matches[1]
                if ($Matches.ContainsKey(2)) { $value = $Matches[2] } else {
                    $i++; if ($i -ge $tokens.Count) { throw 'Missing metrics option value in service command' }; $value = $tokens[$i]
                }
                if ($name -eq 'endpoint') { $endpoint = $value } else { $hostId = $value }
            }
        }
    }
    $null = Get-RunnerMetricsArguments $endpoint $hostId
    if ($push -ne [bool]$endpoint) { throw 'Incomplete metrics settings in service command' }
    return @{ MetricsEndpoint=$endpoint; MetricsHostId=$hostId }
}

function Set-RunnerMetricsCommand([string]$Binary, [string]$MetricsEndpoint, [string]$MetricsHostId) {
    $null = Get-RunnerMetricsSettings $Binary
    $suffix = Get-RunnerMetricsArguments $MetricsEndpoint $MetricsHostId
    return ([regex]::Replace($Binary, '\s+--metrics-(?:push\b|(?:endpoint|host-id)(?:=|\s+)(?:"[^"]*"|\S+))', '') + $suffix)
}

function Invoke-RunnerMetricsKey([string]$Root) {
    & "$Root\bin\motor-runner.exe" metrics-key --instance ally --state-root "$Root\state"
    if ($LASTEXITCODE -ne 0) { throw "metrics-key failed with exit code $LASTEXITCODE" }
}

function Assert-RunnerServiceCommand([string]$Expected) {
    $actual = (Get-ItemProperty 'HKLM:\SYSTEM\CurrentControlSet\Services\MotorRunner').ImagePath
    if ($actual -cne $Expected) { throw "MotorRunner service command mismatch: $actual" }
    Write-Host "Verified MotorRunner command: $actual"
}

function Get-RunnerServiceDefinition([string]$Root, [string]$MetricsEndpoint = '', [string]$MetricsHostId = '') {
    $metrics = Get-RunnerMetricsArguments $MetricsEndpoint $MetricsHostId
    return [pscustomobject]@{
        binary = ('"{0}\bin\motor-runner.exe" service --instance ally --state-root "{0}\state" --backend windows-vm --vm-config "{0}\vm.toml"' -f $Root) + $metrics
        display_name = 'Motor offline Hyper-V runner'
        start = 'auto'
        dependencies = @('vmms')
        environment = @("HOME=$Root\service-home", 'PATH=C:\Windows\System32;C:\Windows;C:\Program Files\Git\cmd;C:\Program Files\Git\bin')
    }
}
