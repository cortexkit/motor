# Trusted control plane. Only installed, ACL-protected copies run as SYSTEM.
param(
    [Parameter(Mandatory)][ValidateSet('Validate','Prepare','Id','Run','Remove','Reconcile','GrantPrep','RevokePrep','Harvest')][string]$Operation,
    [Parameter(Mandatory)][string]$Root,
    [string]$Job, [string]$Parent, [string]$ParentSha256, [string]$Cache,
    [string]$Nonce, [string]$PeerTargetsJson, [UInt64]$OverlayQuotaBytes = 42949672960, [string]$Staging
)
$ErrorActionPreference = 'Stop'
Set-StrictMode -Version Latest
[Console]::OutputEncoding = [Text.UTF8Encoding]::new($false)
. "$PSScriptRoot\common.ps1"
if ($Root -cne 'C:\ProgramData\MotorRunner' -or -not (Test-Path -LiteralPath "$Root\.motor-runner-root")) { throw 'Unowned runner root' }
foreach ($path in @('C:\ProgramData', $Root)) {
    if ((Get-Item -LiteralPath $path -Force).Attributes -band [IO.FileAttributes]::ReparsePoint) { throw "Reparse root refused: $path" }
}
Import-Module Hyper-V
$prefix = 'motor-ally-'
$owner = "MotorRunner:$Root"
function Job-Path([string]$Id) { return Join-Path "$Root\vm\jobs" $Id }
function Owned-VM([string]$Id) {
    $vm = Get-VM -Name "$prefix$Id" -ErrorAction SilentlyContinue
    if ($vm -and $vm.Notes -cne "$owner`:$Id") {
        # New-VM and Set-VM are separate calls. An off VM may survive a crash
        # between them; its prewritten journal and exact disk path bind it here.
        $directory = Job-Path $Id
        $disks = @(Get-VMHardDiskDrive -VM $vm)
        if ($vm.Notes -or -not (Test-Path -LiteralPath "$directory\job.json") -or $disks.Count -ne 1 -or $disks[0].Path -cne "$directory\os.vhdx") { throw 'VM ownership mismatch; preserving it' }
    }
    return $vm
}
function Evidence([string]$Id, [string]$Event, $Detail) {
    @{utc=[DateTime]::UtcNow.ToString('o'); job=$Id; event=$Event; detail=$Detail} | ConvertTo-Json -Compress -Depth 8 | Add-Content -LiteralPath "$Root\evidence\$Id.jsonl" -Encoding UTF8
}
function Assert-Isolated($VM) {
    if (@(Get-VMNetworkAdapter -VM $VM).Count -ne 0) { throw 'VM has a network adapter' }
    if (@(Get-VMComPort -VMName $VM.Name | Where-Object Path).Count -ne 0) { throw 'VM has a COM mapping' }
    if (@(Get-VMIntegrationService -VM $VM | Where-Object Enabled).Count -ne 0) { throw 'VM has an enabled integration service' }
    if ((Get-VMHost).EnableEnhancedSessionMode) { throw 'Enhanced Session mode must remain disabled' }
    if ($VM.Generation -ne 2) { throw 'Job VM is not Generation 2' }
}
function Remove-Job([string]$Id) {
    $vm = Owned-VM $Id
    if ($vm) {
        if ($vm.State -ne 'Off') { Stop-VM -VM $vm -TurnOff -Force }
        Remove-VM -VM $vm -Force
    }
    $directory = Job-Path $Id
    if (Test-Path -LiteralPath $directory) { Remove-Item -LiteralPath $directory -Recurse -Force }
    if ((Owned-VM $Id) -or (Test-Path -LiteralPath $directory)) { throw 'Job VM or disk remains after removal' }
    Evidence $Id 'nothing-left-behind' @{vm_absent=$true; job_directory_absent=$true}
}
function Grant([string]$Path, [string]$Permission, [string]$Sid) {
    if (-not (Test-Path -LiteralPath $Path)) { return }
    & icacls.exe $Path /grant:r "*${Sid}:$Permission" /Q | Out-Null
    if ($LASTEXITCODE -ne 0) { throw "Prep ACL failed at $Path" }
}
if ($Operation -in @('Prepare','Id','Run','Remove','Harvest')) {
    if ($Job -notmatch '^[0-9a-f]{8}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{12}$') { throw 'Invalid job UUID' }
}
switch ($Operation) {
    'Validate' {
        if ((Get-Service vmms).Status -ne 'Running') { throw 'Hyper-V service unavailable' }
        if ((Get-VMHost).EnableEnhancedSessionMode) { throw 'Enhanced Session mode enabled' }
        $key = 'HKLM:\SOFTWARE\Microsoft\Windows NT\CurrentVersion\Virtualization\GuestCommunicationServices\92c268b0-cddd-40a4-a664-f32e50ad35ea'
        $servicesRoot = 'HKLM:\SOFTWARE\Microsoft\Windows NT\CurrentVersion\Virtualization\GuestCommunicationServices'
        $registrations = @(Get-ChildItem -LiteralPath $servicesRoot | ForEach-Object { @{service=$_.PSChildName; properties=(Get-ItemProperty -LiteralPath $_.PSPath | Select-Object ElementName)} })
        $registrations | ConvertTo-Json -Depth 4 | Set-Content "$Root\evidence\host-hvsock-registrations.json" -Encoding UTF8
        if (@($registrations | Where-Object { $_.service -ne '92c268b0-cddd-40a4-a664-f32e50ad35ea' }).Count) { throw 'Additional host hvsock registrations require an isolation review; no foreign registration was changed' }
        $registration = Get-ItemProperty -LiteralPath $key
        if ($registration.MotorOwner -ne 'motor-runner-hyperv' -or $registration.MotorRoot -cne $Root) { throw 'hvsock registration not owned by runner' }
        $prep = Get-LocalUser -Name motor-prep
        if (-not $prep.Enabled) { throw 'Prep account disabled' }
        $admins = Get-LocalGroup -SID 'S-1-5-32-544'
        if (@(Get-LocalGroupMember -Group $admins | Where-Object SID -eq $prep.SID).Count) { throw 'Prep account is an administrator' }
        # Every service-owned path has a protected SYSTEM/Administrators DACL.
        # Explicit batch-account grants are added only to staging and fetch cache.
        foreach ($path in @($Root, "$Root\bin", "$Root\state", "$Root\prep-secret.dpapi")) {
            $acl = Get-Acl -LiteralPath $path
            foreach ($rule in $acl.Access) {
                $sid = $rule.IdentityReference.Translate([Security.Principal.SecurityIdentifier]).Value
                if ($rule.AccessControlType -eq 'Allow' -and $sid -notin @('S-1-5-18','S-1-5-32-544')) {
                    if ($path -eq "$Root\bin" -and $sid -eq (Get-Content "$Root\ssh-sid.txt" -Raw).Trim() -and -not ($rule.FileSystemRights -band [Security.AccessControl.FileSystemRights]::Write)) { continue }
                    throw "Unexpected root grant $sid at $path"
                }
            }
        }
        Write-Output 'PASS native-host-validation'
    }
    { $_ -in @('GrantPrep','RevokePrep') } {
        $parentStage = [IO.Path]::GetFullPath("$Root\staging")
        $full = [IO.Path]::GetFullPath($Staging)
        if ([IO.Path]::GetDirectoryName($full) -cne $parentStage -or [IO.Path]::GetFileName($full) -notmatch '^[0-9a-f-]{36}$') { throw 'Staging must name one owned job' }
        $sid = (Get-LocalUser motor-prep).SID.Value
        if ($Operation -eq 'RevokePrep') {
            foreach ($path in @($full, "$full\fetch-manifests", "$full\fetch-git-deps", "$full\home", "$full\fetch-temp")) {
                if (Test-Path -LiteralPath $path) {
                    & icacls.exe $path /remove:g "*$sid" /Q | Out-Null
                    if ($LASTEXITCODE -ne 0) { throw "Prep ACL revocation failed at $path" }
                }
            }
            break
        }
        Grant $full '(X)' $sid
        foreach ($name in @('fetch-manifests','fetch-git-deps','home')) { Grant "$full\$name" '(OI)(CI)RX' $sid }
        Grant "$full\fetch-temp" '(OI)(CI)M' $sid
        Grant "$Root\state\ally\prep-cargo" '(OI)(CI)M' $sid
        Grant "$Root\prep-toolchain" '(OI)(CI)RX' $sid
    }
    'Prepare' {
        if ($Nonce -notmatch '^[0-9a-f-]{36}$') { throw 'Invalid nonce' }
        if ([IO.Path]::GetDirectoryName($Parent) -cne "$Root\baselines" -or -not (Get-Item -LiteralPath $Parent).IsReadOnly) { throw 'Parent must be an owned sealed baseline' }
        if ([IO.Path]::GetDirectoryName($Cache) -cne "$Root\vm\caches" -or [IO.Path]::GetFileName($Cache) -notmatch '^[0-9a-f]{64}\.vhdx$') { throw 'Invalid cache disk' }
        if ((Get-FileHash -LiteralPath $Parent -Algorithm SHA256).Hash -ne $ParentSha256) { throw 'Parent hash changed before job' }
        $directory = Job-Path $Job
        if ((Test-Path -LiteralPath $directory) -or (Owned-VM $Job)) { throw 'Job resources already exist' }
        New-Item -ItemType Directory -Path "$directory\nonce" | Out-Null
        [IO.File]::WriteAllText("$directory\nonce\nonce.txt", $Nonce, [Text.UTF8Encoding]::new($false))
        if ($PeerTargetsJson) {
            $targets = @($PeerTargetsJson | ConvertFrom-Json)
            foreach ($target in $targets) { [void][Guid]::Parse([string]$target) }
            [IO.File]::WriteAllText("$directory\nonce\peer-targets.json", (ConvertTo-Json -InputObject $targets -Compress), [Text.UTF8Encoding]::new($false))
        }
        New-SpikeIso "$directory\nonce" "$directory\nonce.iso" 'MOTORNONCE'
        Remove-Item "$directory\nonce" -Recurse -Force
        New-VHD -Path "$directory\os.vhdx" -ParentPath $Parent -Differencing | Out-Null
        if (-not (Test-Path -LiteralPath $Cache)) { New-VHD -Path $Cache -Dynamic -SizeBytes 40GB | Out-Null }
        @{parent=$Parent; sha256=$ParentSha256; quota=$OverlayQuotaBytes} | ConvertTo-Json | Set-Content "$directory\job.json" -Encoding ascii
        $vm = New-VM -Name "$prefix$Job" -Generation 2 -MemoryStartupBytes 8GB -VHDPath "$directory\os.vhdx" -Path "$directory\config"
        Set-VM -VM $vm -Notes "$owner`:$Job" -ProcessorCount 4 -AutomaticCheckpointsEnabled $false -CheckpointType Disabled -AutomaticStartAction Nothing -AutomaticStopAction TurnOff
        Set-VMMemory -VM $vm -DynamicMemoryEnabled $false
        Get-VMNetworkAdapter -VM $vm | Remove-VMNetworkAdapter
        Get-VMIntegrationService -VM $vm | Disable-VMIntegrationService
        Set-VMFirmware -VM $vm -EnableSecureBoot On -SecureBootTemplate MicrosoftWindows -FirstBootDevice (Get-VMHardDiskDrive -VM $vm)
        Add-VMHardDiskDrive -VM $vm -ControllerType SCSI -ControllerNumber 0 -ControllerLocation 1 -Path $Cache
        Add-VMDvdDrive -VM $vm -Path "$directory\nonce.iso"
        Assert-Isolated $vm
        Evidence $Job 'created-offline' @{vm_id=$vm.Id.Guid; adapters=0; com_ports=0; integration_services=0; enhanced_session=$false; parent_sha256=$ParentSha256; nonce_delivery='read-only-dvd'; cache=$Cache}
    }
    'Id' { (Owned-VM $Job).Id.Guid }
    'Run' {
        $vm = Owned-VM $Job
        $directory = Job-Path $Job
        $metadata = Get-Content "$directory\job.json" -Raw | ConvertFrom-Json
        Assert-Isolated $vm
        try {
            Start-VM -VM $vm
            do {
                $vm = Owned-VM $Job
                Assert-Isolated $vm
                if ((Get-Item "$directory\os.vhdx").Length -gt $metadata.quota) { throw 'Overlay growth quota exceeded' }
                Evidence $Job 'host-isolation-sample' @{vm_id=$vm.Id.Guid; state=[string]$vm.State; adapters=0; integration_services=0; enhanced_session=$false}
                Start-Sleep -Milliseconds 250
            } while ($vm.State -ne 'Off')
            Assert-Isolated $vm
            if ((Get-FileHash -LiteralPath $metadata.parent -Algorithm SHA256).Hash -ne $metadata.sha256) { throw 'Parent hash changed after job' }
            Evidence $Job 'normal-off' @{parent_unchanged=$true; parent_sha256=$metadata.sha256}
        } catch {
            if ($vm -and $vm.State -ne 'Off') { Stop-VM -VM $vm -TurnOff -Force }
            Evidence $Job 'host-isolation-failed' ([string]$_)
            throw
        }
    }
    'Harvest' {
        $vm = Owned-VM $Job
        if ($vm -and $vm.State -ne 'Off') { Stop-VM -VM $vm -TurnOff -Force }
        $destination = "$Root\evidence\$Job-guest"
        New-Item $destination -ItemType Directory -Force | Out-Null
        Use-SpikeWindowsDisk "$(Job-Path $Job)\os.vhdx" {
            param($volume)
            foreach ($file in @('hyperv-agent.log','peer-proof.json')) {
                $source = Join-Path $volume "ProgramData\Motor\$file"
                if (Test-Path -LiteralPath $source) { Copy-Item -LiteralPath $source -Destination $destination }
            }
        } -ReadOnly
    }
    'Remove' { Remove-Job $Job }
    'Reconcile' {
        foreach ($vm in @(Get-VM -Name "$prefix*" -ErrorAction SilentlyContinue)) {
            $id = $vm.Name.Substring($prefix.Length)
            if ($id -notmatch '^[0-9a-f-]{36}$') { throw 'Unexpected runner VM name' }
            Remove-Job $id
        }
        foreach ($directory in @(Get-ChildItem "$Root\vm\jobs" -Directory)) {
            if ($directory.Name -notmatch '^[0-9a-f-]{36}$') { throw 'Unexpected job disk directory' }
            Remove-Job $directory.Name
        }
    }
}
