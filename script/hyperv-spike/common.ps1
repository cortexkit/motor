# Shared operator helpers. Dot-source from Windows PowerShell 5.1, elevated.
$ErrorActionPreference = 'Stop'
Set-StrictMode -Version Latest
. "$PSScriptRoot\process.ps1"

function Assert-Spike($Condition, [string]$Name, $Evidence) {
    if (-not $Condition) { throw "FAIL $Name $Evidence" }
    Write-Host "PASS $Name $Evidence"
}

function Assert-SpikeSmartAppControl([scriptblock]$ReadState = {
    $path = 'HKLM:\SYSTEM\CurrentControlSet\Control\CI\Policy'
    if (-not (Test-Path -LiteralPath $path)) { return $null }
    $policy = Get-ItemProperty -LiteralPath $path -ErrorAction Stop
    $value = $policy.PSObject.Properties['VerifiedAndReputablePolicyState']
    if ($null -eq $value) { return $null }
    return [int]$value.Value
}) {
    try { $state = & $ReadState }
    catch { throw "FAIL Smart App Control preflight could not read policy: $_" }
    if ($null -eq $state) {
        Write-Host 'EVIDENCE Smart App Control state not present; other application-control policies are not assessed'
        return
    }
    if ($state -eq 1) {
        throw 'FAIL Smart App Control is enforcing; unsigned spike binaries will be blocked. Host policy is an operator decision; no setting was changed.'
    }
    if ($state -notin @(0,2)) { throw "FAIL Unknown Smart App Control state: $state" }
    # Evaluation can become enforcement later. This is a read-only check, not
    # permission to bypass Smart App Control or any other application policy.
    Assert-Spike $true 'smart-app-control-not-enforcing' "VerifiedAndReputablePolicyState=$state (0=off, 2=evaluation)"
}

function Enter-Spike([string]$Root) {
    Assert-Spike ([Environment]::OSVersion.Platform -eq 'Win32NT') 'windows-host' $PSVersionTable.PSVersion
    Assert-Spike ($Root -match '^C:\\motor-spike\\hyperv(?:-[A-Za-z0-9-]+)?$') 'dedicated-spike-root' $Root
    $admin = ([Security.Principal.WindowsPrincipal][Security.Principal.WindowsIdentity]::GetCurrent()).IsInRole([Security.Principal.WindowsBuiltInRole]::Administrator)
    Assert-Spike $admin 'elevated-host' ([Security.Principal.WindowsIdentity]::GetCurrent().Name)
    Assert-SpikeSmartAppControl
    Import-Module Hyper-V
    Assert-Spike ((Get-Service vmms).Status -eq 'Running') 'hyperv-service' (Get-Service vmms).Status
    Assert-Spike ((Get-CimInstance Win32_ComputerSystem).HypervisorPresent) 'hypervisor-present' (Get-CimInstance Win32_OperatingSystem).Caption
    Assert-Spike (@(Get-Process 'qemu-system-*' -ErrorAction SilentlyContinue).Count -eq 0) 'no-concurrent-qemu' 'stop other spike VMs first'
    $marker = Join-Path $Root '.hyperv-spike-root'
    if ((Test-Path $Root) -and -not (Test-Path $marker)) {
        Assert-Spike (@(Get-ChildItem $Root -Force).Count -eq 0) 'unmarked-root-must-be-empty' $Root
    }
    New-Item $Root -ItemType Directory -Force | Out-Null
    & icacls.exe $Root /inheritance:r /grant:r '*S-1-5-18:(OI)(CI)F' '*S-1-5-32-544:(OI)(CI)F' | Out-Host
    Assert-Spike ($LASTEXITCODE -eq 0) 'private-spike-root' $Root
    Set-Content $marker 'motor Hyper-V spike scratch; not a production instance' -Encoding ascii
    $lock = [IO.File]::Open((Join-Path $Root 'spike.lock'), 'OpenOrCreate', 'ReadWrite', 'None')
    try {
        Assert-Spike (@(Get-VM -Name 'motor-hv-*' -ErrorAction SilentlyContinue).Count -eq 0) 'no-orphan-spike-vm' 'prefix=motor-hv-'
        Get-CimInstance Win32_Processor | Select-Object Name,NumberOfCores,NumberOfLogicalProcessors | Format-List | Out-Host
        Get-CimInstance Win32_OperatingSystem | Select-Object Caption,Version,TotalVisibleMemorySize,FreePhysicalMemory | Format-List | Out-Host
        Get-Module Hyper-V | Select-Object Name,Version | Format-Table | Out-Host
        return $lock
    } catch { $lock.Dispose(); throw }
}

function New-SpikeVM([string]$Name, [string]$Disk, [string]$Directory, [string]$Pipe = '') {
    New-VM -Name $Name -Generation 2 -MemoryStartupBytes 8GB -VHDPath $Disk -Path $Directory | Out-Null
    Set-VM -Name $Name -ProcessorCount 4 -AutomaticCheckpointsEnabled $false -CheckpointType Disabled -AutomaticStartAction Nothing -AutomaticStopAction TurnOff
    Set-VMMemory -VMName $Name -DynamicMemoryEnabled $false
    Get-VMNetworkAdapter -VMName $Name | Remove-VMNetworkAdapter
    Set-VMFirmware -VMName $Name -EnableSecureBoot On -SecureBootTemplate MicrosoftWindows -FirstBootDevice (Get-VMHardDiskDrive -VMName $Name)
    # Disable optional integration services, including host/guest file copy.
    # The spike never opens VMConnect or an Enhanced Session with redirected drives.
    Get-VMIntegrationService -VMName $Name | Disable-VMIntegrationService
    if ($Pipe) { Set-VMComPort -VMName $Name -Number 1 -Path "\\.\pipe\$Pipe" -DebuggerMode Off }
    Assert-Spike ((Get-VM -Name $Name).Generation -eq 2) 'generation2' $Name
    Assert-Spike ((Get-VMFirmware -VMName $Name).SecureBoot -eq 'On') 'secure-boot' ((Get-VMFirmware -VMName $Name) | Out-String)
}

function Remove-SpikeVM([string]$Name) {
    $vm = Get-VM -Name $Name -ErrorAction SilentlyContinue
    if ($vm) {
        if ($vm.State -ne 'Off') { Stop-VM -VM $vm -TurnOff -Force }
        Remove-VM -VM $vm -Force
    }
    Assert-Spike (-not (Get-VM -Name $Name -ErrorAction SilentlyContinue)) 'vm-removed' $Name
}

function Wait-SpikeOff([string]$Name, [int]$Seconds) {
    $deadline = [DateTime]::UtcNow.AddSeconds($Seconds)
    while ((Get-VM -Name $Name).State -ne 'Off') {
        if ([DateTime]::UtcNow -ge $deadline) { throw "FAIL shutdown-timeout $Name after ${Seconds}s" }
        Start-Sleep -Seconds 2
    }
    Assert-Spike $true 'vm-off' $Name
}

function Test-SpikeVolumeGuidPath([string]$Path) {
    return [regex]::IsMatch($Path, '\A\\\\\?\\Volume\{[0-9A-Fa-f]{8}-(?:[0-9A-Fa-f]{4}-){3}[0-9A-Fa-f]{12}\}\\\z')
}

function Get-SpikeFileSha256([string]$Path) {
    $stream = [IO.File]::OpenRead($Path)
    $sha = [Security.Cryptography.SHA256]::Create()
    try { return [BitConverter]::ToString($sha.ComputeHash($stream)).Replace('-', '') } finally {
        $sha.Dispose()
        $stream.Dispose()
    }
}

function Get-SpikePlainGuestItem([string]$Volume, [string]$Relative, [scriptblock]$ReadItem) {
    if (-not (Test-SpikeVolumeGuidPath $Volume)) { throw 'Guest volume path is not a volume GUID path' }
    # .NET path access works before PowerShell registers any mount-point drive.
    if (-not $ReadItem) {
        $ReadItem = {
            param($Path)
            try { $attributes = [IO.File]::GetAttributes($Path) } catch [IO.FileNotFoundException] { return $null } catch [IO.DirectoryNotFoundException] { return $null }
            if ($attributes -band [IO.FileAttributes]::Directory) {
                $info = [IO.DirectoryInfo]::new($Path)
                $length = 0
            } else {
                $info = [IO.FileInfo]::new($Path)
                $length = $info.Length
            }
            [pscustomobject]@{FullName=$info.FullName; Name=$info.Name; Attributes=$attributes; PSIsContainer=[bool]($attributes -band [IO.FileAttributes]::Directory); Length=$length; LastWriteTimeUtc=$info.LastWriteTimeUtc}
        }
    }
    # Use .NET to combine and inspect components without resolving a PowerShell drive.
    $path = $Volume
    foreach ($component in $Relative.Split('\')) {
        if (-not $component -or $component -in @('.','..') -or $component.Contains(':') -or $component.Contains('/')) { throw 'Invalid guest diagnostic path' }
        $path = [IO.Path]::Combine($path, $component)
        $item = & $ReadItem $path
        if (-not $item) { return $null }
        # Check each ancestor before descending so a guest junction cannot
        # redirect host inspection outside the mounted volume.
        if ($item.Attributes -band [IO.FileAttributes]::ReparsePoint) {
            Write-Host "EVIDENCE skipped-guest-reparse-path $path"
            return $null
        }
    }
    return $item
}

function Remove-SpikeDiskDriveLetters([int]$DiskNumber) {
    foreach ($partition in @(Get-Partition -DiskNumber $DiskNumber -ErrorAction Stop)) {
        $letter = [string]$partition.DriveLetter
        if ($letter -match '\A[A-Z]\z') {
            $accessPath = $letter + ':\'
            $partition | Remove-PartitionAccessPath -AccessPath $accessPath -ErrorAction Stop | Out-Null
        }
    }
}

function Use-SpikeWindowsDisk([string]$Path, [scriptblock]$Action, [switch]$ReadOnly) {
    $mounted = $false
    try {
        $image = Mount-VHD -Path $Path -ReadOnly:$ReadOnly -Passthru
        $mounted = $true
        $disk = $image | Get-Disk
        $partition = @(Get-Partition -DiskNumber $disk.Number | Where-Object { $_.GptType -eq '{ebd0a0a2-b9e5-4433-87c0-68b6b72699c7}' })
        Assert-Spike ($partition.Count -eq 1) 'unique-windows-partition' "$Path disk=$($disk.Number)"
        $partition = Get-Partition -DiskNumber $disk.Number -PartitionNumber $partition[0].PartitionNumber
        $volumes = @($partition | Get-Volume)
        Assert-Spike ($volumes.Count -eq 1 -and (Test-SpikeVolumeGuidPath ([string]$volumes[0].Path))) 'windows-volume-guid-path' "$Path disk=$($disk.Number) partition=$($partition.PartitionNumber)"
        $reverse = @(Get-Partition -Volume $volumes[0])
        Assert-Spike ($reverse.Count -eq 1 -and $reverse[0].DiskNumber -eq $disk.Number -and $reverse[0].PartitionNumber -eq $partition.PartitionNumber) 'volume-guid-maps-to-mounted-partition' $volumes[0].Path
        # Use the Storage volume GUID path instead of a session drive letter.
        $volume = [string]$volumes[0].Path
        $windows = Get-SpikePlainGuestItem $volume 'Windows'
        Assert-Spike ($windows -and $windows.PSIsContainer) 'windows-partition-mounted' $volume
        & $Action $volume
    } finally { if ($mounted) { Dismount-VHD -Path $Path } }
}

function New-SpikeIso([string]$Tree, [string]$Path, [string]$VolumeName = 'CKSETUP') {
    if (-not ('MotorSpikeIso' -as [type])) {
        Add-Type @'
using System;
using System.IO;
using System.Runtime.InteropServices;
using System.Runtime.InteropServices.ComTypes;
public static class MotorSpikeIso {
    public static void Save(object source, string path) {
        var stream = (IStream)source;
        var count = Marshal.AllocCoTaskMem(4);
        try {
            using (var output = File.Create(path)) {
                var buffer = new byte[65536];
                while (true) {
                    stream.Read(buffer, buffer.Length, count);
                    int read = Marshal.ReadInt32(count);
                    if (read == 0) break;
                    output.Write(buffer, 0, read);
                }
            }
        } finally { Marshal.FreeCoTaskMem(count); }
    }
}
'@
    }
    $image = New-Object -ComObject IMAPI2FS.MsftFileSystemImage
    $image.FileSystemsToCreate = 3
    $image.VolumeName = $VolumeName
    $image.Root.AddTree($Tree, $false)
    $result = $image.CreateResultImage()
    [MotorSpikeIso]::Save($result.ImageStream, $Path)
    Assert-Spike ((Get-Item $Path).Length -gt 0) 'seed-iso' ((Get-FileHash $Path).Hash)
}

function Get-SpikeUnixMs([DateTimeOffset]$Instant = [DateTimeOffset]::UtcNow) { return $Instant.ToUnixTimeMilliseconds() }

function Test-SpikePipeAbsent([string]$Pipe) {
    $path = "\\.\pipe\$Pipe"
    $deadline = [DateTime]::UtcNow.AddSeconds(15)
    do {
        $present = @([IO.Directory]::GetFiles('\\.\pipe\') | Where-Object { $_ -eq $path }).Count -ne 0
        if (-not $present) { return $true }
        Start-Sleep -Milliseconds 100
    } while ([DateTime]::UtcNow -lt $deadline)
    return $false
}

# Motor's Hyper-V socket service ID. It must equal HVSOCK_SERVICE_ID in
# crates/motor-winagent/src/transport.rs; a portable test compares the two.
$SpikeHvsockServiceId = '92c268b0-cddd-40a4-a664-f32e50ad35ea'
# Marker value written into the service registration so install/uninstall only
# ever touch a key this spike created.
$SpikeHvsockOwner = 'motor-hyperv-spike'

function Get-SpikeServiceKeyPath {
    return "HKLM:\SOFTWARE\Microsoft\Windows NT\CurrentVersion\Virtualization\GuestCommunicationServices\$SpikeHvsockServiceId"
}

# Decide what install or uninstall must do with the registration, given whether
# the key exists and the owner marker it carries. Never overwrites or deletes a
# key with the same GUID that something else created.
function Get-SpikeServiceKeyAction([ValidateSet('install','uninstall')][string]$Operation, [bool]$Exists, [string]$Owner) {
    if ($Exists -and $Owner -ne $SpikeHvsockOwner) {
        throw "FAIL hvsock-service-key-not-owned existing key has owner '$Owner'; refusing to $Operation"
    }
    if ($Operation -eq 'install') { if ($Exists) { return 'present' } else { return 'create' } }
    if ($Exists) { return 'remove' } else { return 'absent' }
}

function Get-SpikeServiceKeyState([string]$Path = (Get-SpikeServiceKeyPath)) {
    if (-not (Test-Path -LiteralPath $Path)) { return [pscustomobject]@{ exists=$false; owner=''; element_name='' } }
    $key = Get-ItemProperty -LiteralPath $Path
    $owner = if ($key.PSObject.Properties['MotorOwner']) { [string]$key.MotorOwner } else { '' }
    $name = if ($key.PSObject.Properties['ElementName']) { [string]$key.ElementName } else { '' }
    return [pscustomobject]@{ exists=$true; owner=$owner; element_name=$name }
}

# Options that only make sense for one transport are refused up front, before
# any VM or disk is created.
function Assert-SpikeTransportOptions([string]$Transport, [string]$GuestAgentPath, [bool]$CompareSerialQueues, [int]$SerialQueueBytes) {
    if ($Transport -eq 'hvsock') {
        # The sealed parent's agent speaks COM; the socket agent is injected per job.
        if (-not $GuestAgentPath) { throw 'FAIL hvsock-requires-agent-override pass -GuestAgentPath with an agent built by build-probe.ps1 -Transport hvsock' }
        if ($CompareSerialQueues -or $SerialQueueBytes -ne 2097152) { throw 'FAIL hvsock-refuses-serial-options serial queue options apply only to -Transport com' }
    } elseif ($Transport -ne 'com') { throw "FAIL unknown-transport $Transport" }
}

function Get-SpikeClientArgs([string]$Transport, [string]$Pipe, [string]$VmId, [string]$Nonce, [string]$Canary, [string]$HostName, [string]$Result, [int]$StressBytes) {
    $channel = if ($Transport -eq 'hvsock') { @('--vm-id', $VmId) } else { @('--pipe', $Pipe) }
    $arguments = $channel + @('--nonce', $Nonce, '--canary', $Canary, '--host', $HostName, '--result', $Result)
    if ($StressBytes) { $arguments += @('--stress-bytes', [string]$StressBytes) }
    return ,$arguments
}
