# Shared operator helpers. Dot-source from Windows PowerShell 5.1, elevated.
$ErrorActionPreference = 'Stop'
Set-StrictMode -Version Latest

function Assert-Spike($Condition, [string]$Name, $Evidence) {
    if (-not $Condition) { throw "FAIL $Name $Evidence" }
    Write-Host "PASS $Name $Evidence"
}

function Enter-Spike([string]$Root) {
    Assert-Spike ([Environment]::OSVersion.Platform -eq 'Win32NT') 'windows-host' $PSVersionTable.PSVersion
    Assert-Spike ($Root -match '^C:\\motor-spike\\hyperv(?:-[A-Za-z0-9-]+)?$') 'dedicated-spike-root' $Root
    $admin = ([Security.Principal.WindowsPrincipal][Security.Principal.WindowsIdentity]::GetCurrent()).IsInRole([Security.Principal.WindowsBuiltInRole]::Administrator)
    Assert-Spike $admin 'elevated-host' ([Security.Principal.WindowsIdentity]::GetCurrent().Name)
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

function Use-SpikeWindowsDisk([string]$Path, [scriptblock]$Action, [switch]$ReadOnly) {
    $mounted = $false
    try {
        $disk = Mount-VHD -Path $Path -ReadOnly:$ReadOnly -Passthru | Get-Disk
        $mounted = $true
        $partition = @(Get-Partition -DiskNumber $disk.Number | Where-Object { $_.GptType -eq '{ebd0a0a2-b9e5-4433-87c0-68b6b72699c7}' })
        Assert-Spike ($partition.Count -eq 1) 'unique-windows-partition' "$Path disk=$($disk.Number)"
        if (-not $partition[0].DriveLetter) { $partition[0] | Add-PartitionAccessPath -AssignDriveLetter }
        $partition = Get-Partition -DiskNumber $disk.Number -PartitionNumber $partition[0].PartitionNumber
        $volume = "$($partition.DriveLetter):\"
        Assert-Spike (Test-Path (Join-Path $volume 'Windows')) 'windows-partition-mounted' $volume
        & $Action $volume
    } finally { if ($mounted) { Dismount-VHD -Path $Path } }
}

function New-SpikeIso([string]$Tree, [string]$Path) {
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
    $image.VolumeName = 'CKSETUP'
    $image.Root.AddTree($Tree, $false)
    $result = $image.CreateResultImage()
    [MotorSpikeIso]::Save($result.ImageStream, $Path)
    Assert-Spike ((Get-Item $Path).Length -gt 0) 'seed-iso' ((Get-FileHash $Path).Hash)
}

function Get-SpikeUnixMs { return [long]([DateTime]::UtcNow - [DateTime]'1970-01-01Z').TotalMilliseconds }

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
