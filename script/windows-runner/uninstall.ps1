<# Run a copied uninstall.ps1 outside the root; this removes the parent and all state. #>
param([switch]$ConfirmRemoveParentAndState)
$ErrorActionPreference = 'Stop'
Set-StrictMode -Version Latest
if (-not $ConfirmRemoveParentAndState) { throw 'Pass -ConfirmRemoveParentAndState after copying evidence out of the runner root' }
$Root = 'C:\ProgramData\MotorRunner'
$nativeLogPath = Join-Path $Root 'uninstall-native.jsonl'
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
    $null = Invoke-RunnerNative -FilePath "$env:SystemRoot\System32\sc.exe" -ArgumentList @('delete', 'MotorRunner') -Operation 'delete runner service' -LogPath $nativeLogPath
    $service.Dispose()
}
# Use the installed trusted control script before deleting any disk or account.
$null = Invoke-RunnerNative -FilePath "$env:SystemRoot\System32\WindowsPowerShell\v1.0\powershell.exe" -ArgumentList @('-NoProfile', '-NonInteractive', '-ExecutionPolicy', 'Bypass', '-File', "$Root\bin\hyperv.ps1", '-Operation', 'Reconcile', '-Root', $Root) -Operation 'reconcile runner VMs before uninstall' -LogPath $nativeLogPath
if (@(Get-VM -Name 'motor-ally-*' -ErrorAction SilentlyContinue).Count) { throw 'Owned VM remains' }
if (@(Get-ChildItem "$Root\vm\jobs" -Force).Count) { throw 'Owned job disk remains' }
$key = 'HKLM:\SOFTWARE\Microsoft\Windows NT\CurrentVersion\Virtualization\GuestCommunicationServices\92c268b0-cddd-40a4-a664-f32e50ad35ea'
if (Test-Path -LiteralPath $key) {
    $registration = Get-ItemProperty -LiteralPath $key
    if ($registration.MotorOwner -ne 'motor-runner-hyperv' -or $registration.MotorRoot -cne $Root) { throw 'hvsock key ownership changed; preserving it and the root' }
    Remove-Item -LiteralPath $key
}
$sshdConfig = 'C:\ProgramData\ssh\sshd_config'
$null = Remove-RunnerSshInstallState -Installation $record -SshdConfig $sshdConfig -LogPath $nativeLogPath
if ($record.PSObject.Properties['power']) {
    foreach ($setting in $record.power.ac.PSObject.Properties) {
        $null = Invoke-RunnerNative -FilePath "$env:SystemRoot\System32\powercfg.exe" -ArgumentList @('/setacvalueindex', $record.power.scheme, '238c9fa8-0aad-41ed-83f4-97be242c8f20', $setting.Name, [string]$setting.Value) -Operation "restore power setting $($setting.Name)" -LogPath $nativeLogPath
    }
    # Do not change the active scheme if the operator selected a different one.
    $activeScheme = Invoke-RunnerNative -FilePath "$env:SystemRoot\System32\powercfg.exe" -ArgumentList @('/getactivescheme') -Operation 'read active power scheme during uninstall' -LogPath $nativeLogPath
    if ($activeScheme.stdout -match [regex]::Escape($record.power.scheme)) {
        $null = Invoke-RunnerNative -FilePath "$env:SystemRoot\System32\powercfg.exe" -ArgumentList @('/setactive', $record.power.scheme) -Operation 'restore active power scheme' -LogPath $nativeLogPath
    }
}
Set-VMHost -EnableEnhancedSessionMode ([bool]$record.enhanced_session_before)
if ($record.PSObject.Properties['provision_root']) {
    $provision = [string]$record.provision_root
    if ($provision -cne 'C:\motor-spike\hyperv-ally') { throw 'Unexpected adopted provisioning root' }
    if (Test-Path -LiteralPath $provision) {
        if (-not (Test-Path -LiteralPath "$provision\.hyperv-spike-root") -or ((Get-Item -LiteralPath $provision -Force).Attributes -band [IO.FileAttributes]::ReparsePoint)) { throw 'Adopted provisioning root ownership changed' }
        if (@(Get-VM | Get-VMHardDiskDrive | Where-Object { $_.Path.StartsWith("$provision\", [StringComparison]::OrdinalIgnoreCase) }).Count) { throw 'A VM still uses the adopted provisioning root' }
        if ((Get-FileHash "$provision\parent.vhdx" -Algorithm SHA256).Hash -ne $record.provision_parent_sha256) { throw 'Adopted provisioning parent changed; preserving it' }
        $null = Invoke-RunnerNative -FilePath "$env:SystemRoot\System32\icacls.exe" -ArgumentList @("$provision\parent.vhdx", '/grant:r', '*S-1-5-18:F', '*S-1-5-32-544:F', '/Q') -Operation 'restore adopted parent ACL before removal' -LogPath $nativeLogPath
        (Get-Item "$provision\parent.vhdx").IsReadOnly = $false
        Remove-Item -LiteralPath $provision -Recurse -Force
    }
}
# Clearing readonly applies only inside the owned root, never to another parent.
$null = Invoke-RunnerNative -FilePath "$env:SystemRoot\System32\icacls.exe" -ArgumentList @("$Root\baselines\ally-hvsock.vhdx", '/grant:r', '*S-1-5-18:F', '*S-1-5-32-544:F', '/Q') -Operation 'restore owned parent ACL before removal' -LogPath $nativeLogPath
# Job checkouts in staging can hold paths longer than 260 characters, which
# Windows PowerShell 5.1 cmdlets cannot reach. .NET's extended-length form
# (\\?\) can, so both the readonly clear and the removal go through it.
# The walk never enters a reparse point, so a link inside the root can't lead
# the readonly clear outside it; Directory.Delete removes such links without
# following them.
$longRoot = '\\?\' + $Root
$pending = [Collections.Generic.Stack[string]]::new()
$pending.Push($longRoot)
while ($pending.Count) {
    $directory = $pending.Pop()
    foreach ($entry in [IO.Directory]::EnumerateFileSystemEntries($directory)) {
        $attributes = [IO.File]::GetAttributes($entry)
        if ($attributes -band [IO.FileAttributes]::ReparsePoint) { continue }
        if ($attributes -band [IO.FileAttributes]::Directory) {
            $pending.Push($entry)
        } elseif ($attributes -band [IO.FileAttributes]::ReadOnly) {
            [IO.File]::SetAttributes($entry, $attributes -bxor [IO.FileAttributes]::ReadOnly)
        }
    }
}
[IO.Directory]::Delete($longRoot, $true)
if ((Test-Path $Root) -or (Test-Path $key) -or (Get-LocalUser motor-prep -ErrorAction SilentlyContinue) -or (Get-LocalUser motor-ssh -ErrorAction SilentlyContinue)) { throw 'Uninstall left owned state' }
Write-Host 'PASS uninstalled: service, accounts/key, job VMs/disks, hvsock registration, sealed parent and root removed; Hyper-V remains installed'
