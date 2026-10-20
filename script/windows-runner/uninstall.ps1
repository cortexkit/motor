<# Run a copied uninstall.ps1 outside the root; this removes the parent and all state. #>
param([switch]$ConfirmRemoveParentAndState)
$ErrorActionPreference = 'Stop'
Set-StrictMode -Version Latest
if (-not $ConfirmRemoveParentAndState) { throw 'Pass -ConfirmRemoveParentAndState after copying evidence out of the runner root' }
$Root = 'C:\ProgramData\MotorRunner'
. "$PSScriptRoot\accounts.ps1"
if (-not (Test-Path -LiteralPath "$Root\.motor-runner-root")) { throw 'Runner root marker absent; nothing will be deleted' }
if ((Get-Item -LiteralPath $Root -Force).Attributes -band [IO.FileAttributes]::ReparsePoint) { throw 'Refusing a reparse root' }
$record = Get-Content "$Root\installation.json" -Raw | ConvertFrom-Json
if ($record.root -cne $Root -or $record.schema -ne 1) { throw 'Invalid installation ownership record' }
foreach ($name in @('motor-prep','motor-ssh')) {
    $expected = $record.accounts.PSObject.Properties[$name]
    $account = Get-LocalUser $name -ErrorAction SilentlyContinue
    if ($account -and (-not $expected -or $account.SID.Value -ne $expected.Value)) { throw "Account ownership changed: $name; no removal started" }
}
$service = Get-Service MotorRunner -ErrorAction SilentlyContinue
if ($service) {
    if ($service.Status -ne 'Stopped') { Stop-Service MotorRunner; $service.WaitForStatus('Stopped', [TimeSpan]::FromSeconds(40)) }
    & sc.exe delete MotorRunner | Out-Host
    if ($LASTEXITCODE -ne 0) { throw 'Runner service removal failed' }
    $service.Dispose()
}
# Use the installed trusted control script before deleting any disk or account.
& powershell.exe -NoProfile -NonInteractive -ExecutionPolicy Bypass -File "$Root\bin\hyperv.ps1" -Operation Reconcile -Root $Root
if ($LASTEXITCODE -ne 0) { throw 'VM reconciliation failed; preserving disks and accounts' }
if (@(Get-VM -Name 'motor-ally-*' -ErrorAction SilentlyContinue).Count) { throw 'Owned VM remains' }
if (@(Get-ChildItem "$Root\vm\jobs" -Force).Count) { throw 'Owned job disk remains' }
$key = 'HKLM:\SOFTWARE\Microsoft\Windows NT\CurrentVersion\Virtualization\GuestCommunicationServices\92c268b0-cddd-40a4-a664-f32e50ad35ea'
if (Test-Path -LiteralPath $key) {
    $registration = Get-ItemProperty -LiteralPath $key
    if ($registration.MotorOwner -ne 'motor-runner-hyperv' -or $registration.MotorRoot -cne $Root) { throw 'hvsock key ownership changed; preserving it and the root' }
    Remove-Item -LiteralPath $key
}
$sshdConfig = 'C:\ProgramData\ssh\sshd_config'
$config = [IO.File]::ReadAllText($sshdConfig)
$pattern = '(?ms)^# BEGIN MOTOR RUNNER\r?\n.*?^# END MOTOR RUNNER\r?\n?'
$matches = [regex]::Matches($config, $pattern)
if ($matches.Count -gt 1) { throw 'Multiple SSH ownership blocks; manual reconciliation required' }
if ($matches.Count -eq 1) {
    $updated = [regex]::Replace($config, $pattern, '')
    [IO.File]::WriteAllText($sshdConfig, $updated, [Text.UTF8Encoding]::new($false))
    & 'C:\Windows\System32\OpenSSH\sshd.exe' -t -f $sshdConfig
    if ($LASTEXITCODE -ne 0) { [IO.File]::WriteAllText($sshdConfig, $config); throw 'SSH configuration removal failed validation; original restored' }
    Restart-Service sshd
}
if ($record.PSObject.Properties['ssh_home']) {
    if ($record.ssh_home -notmatch '^C:\\Users\\motor-ssh(?:\.[^\\]+)?$') { throw 'Unexpected SSH home path' }
    $sid = $record.accounts.PSObject.Properties['motor-ssh'].Value
    $profiles = @(Get-CimInstance Win32_UserProfile | Where-Object SID -eq $sid)
    if ($profiles.Count -gt 1) { throw 'Ambiguous SSH profile ownership' }
    if ($profiles.Count -eq 1) {
        if ($profiles[0].LocalPath -ine $record.ssh_home -or $profiles[0].Loaded) { throw 'SSH profile path changed or is still loaded; close its sessions and retry removal' }
        $profiles[0] | Remove-CimInstance
    }
    if (Test-Path -LiteralPath $record.ssh_home) {
        if ((Get-Item -LiteralPath $record.ssh_home -Force).Attributes -band [IO.FileAttributes]::ReparsePoint) { throw 'Refusing a reparse SSH home' }
        Remove-Item -LiteralPath $record.ssh_home -Recurse -Force
    }
}
foreach ($name in @('motor-prep','motor-ssh')) {
    $expected = $record.accounts.PSObject.Properties[$name]
    if (-not $expected) { continue }
    $account = Get-LocalUser $name -ErrorAction SilentlyContinue
    if ($account -and $account.SID.Value -ne $expected.Value) { throw "Account SID changed: $name; refusing deletion" }
    if ($name -eq 'motor-prep') { Set-RunnerBatchRights $Root $expected.Value -Remove }
    if ($account) { Remove-LocalUser $name }
}
if ($record.PSObject.Properties['power']) {
    foreach ($setting in $record.power.ac.PSObject.Properties) {
        & powercfg.exe /setacvalueindex $record.power.scheme '238c9fa8-0aad-41ed-83f4-97be242c8f20' $setting.Name $setting.Value
        if ($LASTEXITCODE -ne 0) { throw 'Could not restore original AC sleep/hibernate setting' }
    }
    # Do not change the active scheme if the operator selected a different one.
    if ((& powercfg.exe /getactivescheme) -match $record.power.scheme) { & powercfg.exe /setactive $record.power.scheme; if ($LASTEXITCODE -ne 0) { throw 'Could not apply restored power policy' } }
}
Set-VMHost -EnableEnhancedSessionMode ([bool]$record.enhanced_session_before)
if ($record.PSObject.Properties['provision_root']) {
    $provision = [string]$record.provision_root
    if ($provision -cne 'C:\motor-spike\hyperv-ally') { throw 'Unexpected adopted provisioning root' }
    if (Test-Path -LiteralPath $provision) {
        if (-not (Test-Path -LiteralPath "$provision\.hyperv-spike-root") -or ((Get-Item -LiteralPath $provision -Force).Attributes -band [IO.FileAttributes]::ReparsePoint)) { throw 'Adopted provisioning root ownership changed' }
        if (@(Get-VM | Get-VMHardDiskDrive | Where-Object { $_.Path.StartsWith("$provision\", [StringComparison]::OrdinalIgnoreCase) }).Count) { throw 'A VM still uses the adopted provisioning root' }
        if ((Get-FileHash "$provision\parent.vhdx" -Algorithm SHA256).Hash -ne $record.provision_parent_sha256) { throw 'Adopted provisioning parent changed; preserving it' }
        & icacls.exe "$provision\parent.vhdx" /grant:r '*S-1-5-18:F' '*S-1-5-32-544:F' /Q | Out-Null
        if ($LASTEXITCODE -ne 0) { throw 'Cannot remove the adopted provisioning parent' }
        (Get-Item "$provision\parent.vhdx").IsReadOnly = $false
        Remove-Item -LiteralPath $provision -Recurse -Force
    }
}
# Clearing readonly applies only inside the owned root, never to another parent.
& icacls.exe "$Root\baselines\ally-hvsock.vhdx" /grant:r '*S-1-5-18:F' '*S-1-5-32-544:F' /Q | Out-Null
if ($LASTEXITCODE -ne 0) { throw 'Cannot restore write attributes on the owned sealed parent' }
Get-ChildItem -LiteralPath $Root -File -Recurse -Force | ForEach-Object { $_.IsReadOnly = $false }
Remove-Item -LiteralPath $Root -Recurse -Force
if ((Test-Path $Root) -or (Test-Path $key) -or (Get-LocalUser motor-prep -ErrorAction SilentlyContinue) -or (Get-LocalUser motor-ssh -ErrorAction SilentlyContinue)) { throw 'Uninstall left owned state' }
Write-Host 'PASS uninstalled: service, accounts/key, job VMs/disks, hvsock registration, sealed parent and root removed; Hyper-V remains installed'
