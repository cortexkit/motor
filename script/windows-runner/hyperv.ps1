# Trusted control plane. Only installed, ACL-protected copies run as SYSTEM.
param(
    [Parameter(Mandatory)][ValidateSet('Validate','Prepare','Id','Run','Remove','Reconcile','GrantPrep','RevokePrep','Harvest','QualifyInspect')][string]$Operation,
    [Parameter(Mandatory)][string]$Root,
    [string]$Job, [string]$Parent, [string]$ParentSha256, [string]$Cache,
    [string]$Nonce, [string]$PeerTargetsJson, [UInt64]$OverlayQuotaBytes = 42949672960, [string]$Staging,
    [switch]$LightweightHarvest
)
$ErrorActionPreference = 'Stop'
Set-StrictMode -Version Latest
[Console]::OutputEncoding = [Text.UTF8Encoding]::new($false)
. "$PSScriptRoot\common.ps1"
. "$PSScriptRoot\parent-identity.ps1"
. "$PSScriptRoot\guest-evidence.ps1"
. "$PSScriptRoot\prep-acl.ps1"
if ($Root -cne 'C:\ProgramData\MotorRunner' -or -not (Test-Path -LiteralPath "$Root\.motor-runner-root")) { throw 'Unowned runner root' }
foreach ($path in @('C:\ProgramData', $Root, "$Root\baselines", "$Root\state")) {
    if ((Get-Item -LiteralPath $path -Force).Attributes -band [IO.FileAttributes]::ReparsePoint) { throw "Reparse root refused: $path" }
}
Import-Module Hyper-V
$prefix = 'motor-ally-'
$owner = "MotorRunner:$Root"
$parentRecord = "$Root\state\parent-verified.json"
function Convert-RunnerPeerTargets([string]$Json) {
    if (-not $Json.TrimStart().StartsWith('[') -or -not $Json.TrimEnd().EndsWith(']')) { throw 'Peer targets must be a JSON array of UUID strings' }
    $decoded = ConvertFrom-Json -InputObject $Json
    $quoted = [Collections.Generic.List[string]]::new()
    foreach ($value in $decoded) {
        if ($value -isnot [string]) { throw 'Peer targets must be a flat JSON array of UUID strings' }
        $quoted.Add('"' + ([Guid]::Parse($value)).ToString('D') + '"')
    }
    # PowerShell 5.1 decorates Object[] passed to ConvertTo-Json with value/Count
    # and @(... ConvertFrom-Json) can nest arrays. Canonical UUIDs need no JSON
    # escaping, so join their quoted strings explicitly instead of serializing.
    return '[' + ($quoted -join ',') + ']'
}
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
function Assert-JobDiskBinding([string]$Id) {
    $destination = "$Root\evidence\$Id-guest"
    New-Item -ItemType Directory -Path $destination -Force | Out-Null
    $expected = [IO.Path]::GetFullPath("$(Job-Path $Id)\os.vhdx")
    $binding = [ordered]@{utc=[DateTime]::UtcNow.ToString('o'); requested_child=$expected; vm_present=$false; disk_present=(Test-Path -LiteralPath $expected); matched=$false}
    try {
        $vm = Owned-VM $Id
        if ($vm) {
            $binding['vm_present'] = $true
            $binding['vm_id'] = $vm.Id.ToString()
            $binding['vm_state'] = [string]$vm.State
            $os = @(Get-VMHardDiskDrive -VM $vm | Where-Object { $_.ControllerNumber -eq 0 -and $_.ControllerLocation -eq 0 })
            if ($os.Count -ne 1) { throw 'Expected exactly one attached OS disk at controller 0/location 0' }
            $actual = [IO.Path]::GetFullPath($os[0].Path)
            $binding['attached_os_path'] = $actual
            if (-not [StringComparer]::OrdinalIgnoreCase.Equals($expected, $actual)) { throw 'Attached OS disk does not match the owned job child; preserving resources' }
        }
        if ($binding.disk_present) {
            $vhd = Get-VHD -Path $expected
            $binding['vhd_path'] = $vhd.Path
            $binding['parent_path'] = $vhd.ParentPath
            $binding['vhd_type'] = [string]$vhd.VhdType
            $binding['disk_identifier'] = [string]$vhd.DiskIdentifier
            if (-not [StringComparer]::OrdinalIgnoreCase.Equals($expected, [IO.Path]::GetFullPath($vhd.Path)) -or [string]$vhd.VhdType -ne 'Differencing') { throw 'Requested evidence disk is not the owned differencing child' }
        }
        $binding['matched'] = $true
        Save-Json "$destination\job-disk-binding.json" $binding
        Evidence $Id 'job-disk-binding' $binding
    } catch {
        $binding['error'] = [string]$_
        Save-Json "$destination\job-disk-binding.json" $binding
        Evidence $Id 'job-disk-binding-failed' $binding
        throw
    }
}
function Harvest-Job([string]$Id, [switch]$Lightweight) {
    $destination = "$Root\evidence\$Id-guest"
    New-Item -ItemType Directory -Path $destination -Force | Out-Null
    try {
        Assert-JobDiskBinding $Id
        $vm = Owned-VM $Id
        if ($vm -and $vm.State -ne 'Off') { Stop-VM -VM $vm -TurnOff -Force }
        $vm = Owned-VM $Id
        if ($vm -and $vm.State -ne 'Off') { throw 'VM is not Off; refusing to mount a running guest disk' }
        $disk = "$(Job-Path $Id)\os.vhdx"
        if (Test-Path -LiteralPath $disk) {
            Save-DiagnosticGuestEvidence $disk $destination -Lightweight:$Lightweight
            # Qualification consumers already use these flat names; preserve
            # them alongside the full path/absence manifest and event exports.
            foreach ($name in @('hyperv-agent.log','peer-proof.json')) {
                $source = "$destination\files\ProgramData\Motor\$name"
                if (Test-Path -LiteralPath $source) { Copy-Item -LiteralPath $source -Destination "$destination\$name" -Force }
            }
            Evidence $Id 'guest-evidence-harvested' @{destination=$destination; read_only=$true; lightweight=[bool]$Lightweight}
        } else {
            Save-Json "$destination\guest-disk-state.json" @{present=$false; reason='No disposable OS disk was created'}
            Evidence $Id 'guest-evidence-skipped' @{reason='no-job-disk'}
        }
    } catch {
        $_ | Out-String | Set-Content -LiteralPath "$destination\harvest-error.txt" -Encoding UTF8
        Evidence $Id 'guest-evidence-failed' ([string]$_)
        throw
    }
}
function Remove-Job([string]$Id, [switch]$LightweightHarvest) {
    # A binding mismatch is not an ordinary collection failure: never delete a
    # different attached disk after silently inspecting the expected path.
    Assert-JobDiskBinding $Id
    $vm = Owned-VM $Id
    if ($vm -and $vm.State -ne 'Off') { Stop-VM -VM $vm -TurnOff -Force }
    # Every path collects logs before destruction. Only protocol-complete jobs
    # use the lightweight log copy; failures and recovery include full diagnostics.
    try { Harvest-Job $Id -Lightweight:$LightweightHarvest } catch { Write-Warning "Guest harvest failed before removal; inspect $Root\evidence\$Id-guest: $_" }
    $vm = Owned-VM $Id
    if ($vm) { Remove-VM -VM $vm -Force }
    $directory = Job-Path $Id
    if (Test-Path -LiteralPath $directory) { Remove-Item -LiteralPath $directory -Recurse -Force }
    if ((Owned-VM $Id) -or (Test-Path -LiteralPath $directory)) { throw 'Job VM or disk remains after removal' }
    Evidence $Id 'nothing-left-behind' @{vm_absent=$true; job_directory_absent=$true}
}
function Grant([string]$Path, [string]$Role, [string]$Sid) {
    Grant-RunnerPrepPath $Path $Sid $Role
}
if ($Operation -in @('Prepare','Id','Run','Remove','Harvest','QualifyInspect')) {
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
        # Windows registers the two PowerShell Direct session services on every
        # Hyper-V host (HV_GUID_VM_SESSION_SERVICE_ID and _ID_2). They name the
        # guest-side vmicvmsession service the host connects to; no host process
        # listens on them for guests. Any other registration still needs review.
        $windowsDefaults = @{ '999E53D4-3D5C-4C3E-8779-BED06EC056E1' = 'VM Session Service'; 'A5201C21-2770-4C11-A68E-F182EDB29220' = 'VM Session Service-2' }
        $foreign = @($registrations | Where-Object {
            $_.service -ne '92c268b0-cddd-40a4-a664-f32e50ad35ea' -and
            -not ($windowsDefaults.ContainsKey($_.service.ToUpperInvariant()) -and $_.properties.ElementName -ceq $windowsDefaults[$_.service.ToUpperInvariant()])
        })
        if ($foreign.Count) { throw 'Additional host hvsock registrations require an isolation review; no foreign registration was changed' }
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
        if ([IO.Path]::GetDirectoryName($Parent) -cne "$Root\baselines") { throw 'Parent must be an owned sealed baseline' }
        $started = [Diagnostics.Stopwatch]::StartNew()
        try { $verification = Confirm-RunnerParentVerification $Parent $ParentSha256 $parentRecord "$Root\evidence\parent-verification.jsonl" } catch {
            Evidence 'parent-verification' 'startup-parent-verification-failed' @{elapsed_ms=$started.Elapsed.TotalMilliseconds; error=[string]$_}
            throw
        } finally { $started.Stop() }
        Evidence 'parent-verification' 'startup-parent-verification' $verification
        Write-Output "PASS native-host-validation cache_hit=$($verification.cache_hit) hash_duration_ms=$($verification.hash_duration_ms)"
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
        Grant $full Stage $sid
        foreach ($name in @('fetch-manifests','fetch-git-deps','home')) { Grant "$full\$name" Read $sid }
        Grant "$full\fetch-temp" Modify $sid
        Grant "$Root\state\ally\prep-cargo" Read $sid
        Grant "$Root\prep-toolchain" Read $sid
    }
    'Prepare' {
        if ($Nonce -notmatch '\A[0-9a-f]{32}\z') { throw 'Nonce must be exactly 32 hexadecimal characters, without hyphens, BOM or newline' }
        if ([IO.Path]::GetDirectoryName($Parent) -cne "$Root\baselines") { throw 'Parent must be an owned sealed baseline' }
        if ([IO.Path]::GetDirectoryName($Cache) -cne "$Root\vm\caches" -or [IO.Path]::GetFileName($Cache) -notmatch '^[0-9a-f]{64}\.vhdx$') { throw 'Invalid cache disk' }
        $parentGuard = Open-RunnerVerifiedParent $Parent $ParentSha256 $parentRecord
        try {
            $directory = Job-Path $Job
            if ((Test-Path -LiteralPath $directory) -or (Owned-VM $Job)) { throw 'Job resources already exist' }
            New-Item -ItemType Directory -Path "$directory\nonce" | Out-Null
            [IO.File]::WriteAllText("$directory\nonce\nonce.txt", $Nonce, [Text.UTF8Encoding]::new($false))
            if ($PeerTargetsJson) {
                [IO.File]::WriteAllText("$directory\nonce\peer-targets.json", (Convert-RunnerPeerTargets $PeerTargetsJson), [Text.UTF8Encoding]::new($false))
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
        } finally { $parentGuard.Dispose() }
    }
    'Id' { (Owned-VM $Job).Id.Guid }
    'QualifyInspect' {
        Assert-JobDiskBinding $Job
        $vm = Owned-VM $Job
        if (-not $vm -or $vm.State -ne 'Running') { throw 'Qualification VM is not Running' }
        $inspection = @{worker_pid=0; vm_id=$vm.Id.ToString(); requested_child="$(Job-Path $Job)\os.vhdx"}
        try {
            $worker = Get-CimInstance -Namespace root/virtualization/v2 -ClassName Msvm_ComputerSystem -Filter "Name='$($vm.Id)'" -OperationTimeoutSec 2
            $inspection.worker_pid = [uint32]$worker.ProcessID
        } catch { $inspection['worker_pid_error'] = [string]$_ }
        $inspection | ConvertTo-Json -Compress
    }
    'Run' {
        $vm = $null
        $parentGuard = $null
        try {
            $vm = Owned-VM $Job
            $directory = Job-Path $Job
            $metadata = Get-Content "$directory\job.json" -Raw | ConvertFrom-Json
            # Prepare and Run are separate processes; recheck before any VM boot.
            $parentGuard = Open-RunnerVerifiedParent $metadata.parent $metadata.sha256 $parentRecord
            $parentIdentity = Get-RunnerParentIdentity $parentGuard
            Assert-Isolated $vm
            Start-VM -VM $vm
            do {
                $vm = Owned-VM $Job
                Assert-Isolated $vm
                if ((Get-Item "$directory\os.vhdx").Length -gt $metadata.quota) { throw 'Overlay growth quota exceeded' }
                Evidence $Job 'host-isolation-sample' @{vm_id=$vm.Id.Guid; state=[string]$vm.State; adapters=0; integration_services=0; enhanced_session=$false}
                Start-Sleep -Milliseconds 250
            } while ($vm.State -ne 'Off')
            Assert-Isolated $vm
            $identity = Get-RunnerParentIdentity $parentGuard
            foreach ($key in $identity.Keys) {
                if ($identity[$key] -cne $parentIdentity[$key]) { throw 'parent changed since verification; restart the service to re-verify' }
            }
            Evidence $Job 'normal-off' @{parent_unchanged=$true; parent_sha256=$metadata.sha256}
            [Console]::Error.WriteLine("Hyper-V Run monitor job=$Job exited normally: VM off, parent unchanged")
        } catch {
            $failure = $_
            # Emit the original reason before best-effort evidence or VM stop;
            # a disappearing VM must not turn a sample failure into empty output.
            [Console]::Error.WriteLine("Hyper-V Run monitor job=$Job failed: $failure")
            try { Evidence $Job 'host-isolation-failed' ([string]$failure) } catch { [Console]::Error.WriteLine("Monitor failure evidence write failed: $_") }
            try { if ($vm -and $vm.State -ne 'Off') { Stop-VM -VM $vm -TurnOff -Force } } catch { [Console]::Error.WriteLine("Monitor failure VM stop failed: $_") }
            throw $failure
        } finally { if ($parentGuard) { $parentGuard.Dispose() } }
    }
    'Harvest' { Harvest-Job $Job }
    'Remove' { Remove-Job $Job -LightweightHarvest:$LightweightHarvest }
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
