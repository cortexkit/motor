<#
.SYNOPSIS
Build the SSH-free Server 2022 job image on an elevated Windows 11 Pro host.
.EXAMPLE
.\provision.ps1 -ServerIso C:\motor-spike\media\server2022.iso
.NOTES
Requires Hyper-V enabled and rebooted, Python 3.10+, Internet for provisioning,
and 130 GiB free. Uses 8 GiB RAM / 4 vCPU; budget 1-3 hours (updates dominate).
Never reruns over an existing disk. Failure preserves the disk and raw logs.
#>
param(
    [Parameter(Mandatory)][string]$ServerIso,
    [string]$Root = 'C:\motor-spike\hyperv',
    [string]$ProvisionSwitch = 'Default Switch',
    [string]$Python = 'python',
    [string]$GuestAgentPath,
    [string]$PeerRunnerPath,
    [int]$TimeoutMinutes = 180
)
. "$PSScriptRoot\common.ps1"
. "$PSScriptRoot\boot-files.ps1"
$Root = [IO.Path]::GetFullPath($Root)
$ServerIso = (Resolve-Path -LiteralPath $ServerIso).Path
$name = 'motor-hv-provision'
$disk = Join-Path $Root 'parent.vhdx'
$seed = Join-Path $Root 'seed'
$seedIso = Join-Path $Root 'seed.iso'
$lock = $null
$transcript = $false
$mountedIso = $false
$diskMounted = $false
$createdDisk = $false
$sealed = $false
try {
    $lock = Enter-Spike $Root
    Start-Transcript -Path (Join-Path $Root 'provision-host.log') -Append | Out-Host
    $transcript = $true
    Assert-Spike (-not (Test-Path $disk)) 'fresh-parent-path' $disk
    Assert-Spike (-not (Test-Path $seed) -and -not (Test-Path $seedIso)) 'fresh-seed-paths' $seed
    Assert-Spike ((Get-PSDrive -Name $Root.Substring(0,1)).Free -ge 130GB) 'free-disk-at-least-130GiB' (Get-PSDrive -Name $Root.Substring(0,1)).Free
    Assert-Spike ((Get-CimInstance Win32_OperatingSystem).FreePhysicalMemory -ge 10GB/1KB) 'free-memory-at-least-10GiB' (Get-CimInstance Win32_OperatingSystem).FreePhysicalMemory
    Assert-Spike ($null -ne (Get-VMSwitch -Name $ProvisionSwitch)) 'provision-switch' $ProvisionSwitch
    & $Python --version | Out-Host
    Assert-Spike ($LASTEXITCODE -eq 0) 'python-available' $Python
    Write-Host "EVIDENCE preverified-install-media SHA256=$((Get-FileHash $ServerIso).Hash) path=$ServerIso"
    $archive = Join-Path $Root 'PowerShell-7.6.7-win-x64.zip'
    if (-not (Test-Path $archive)) {
        Invoke-WebRequest 'https://github.com/PowerShell/PowerShell/releases/download/v7.6.7/PowerShell-7.6.7-win-x64.zip' -OutFile $archive -UseBasicParsing
    }
    $prebuilt = @()
    if ($GuestAgentPath -or $PeerRunnerPath) {
        if (-not $GuestAgentPath -or -not $PeerRunnerPath) { throw 'Supply both prebuilt hvsock agent and peer runner' }
        $prebuilt = @('--guest-agent', (Resolve-Path $GuestAgentPath).Path, '--peer-runner', (Resolve-Path $PeerRunnerPath).Path)
    }
    & $Python "$PSScriptRoot\seed.py" --destination $seed --powershell-archive $archive @prebuilt | Out-Host
    Assert-Spike ($LASTEXITCODE -eq 0) 'seed-rendered' $seed
    New-SpikeIso $seed $seedIso

    # Apply the same image index as autounattend.xml without a DVD key prompt.
    # Only the newly created, path-bound VHDX is initialized or formatted.
    New-VHD -Path $disk -Dynamic -SizeBytes 100GB | Out-Null
    $createdDisk = $true
    $virtualDiskImage = Mount-VHD -Path $disk -Passthru
    $diskMounted = $true
    $virtualDisk = $virtualDiskImage | Get-Disk
    Initialize-Disk -Number $virtualDisk.Number -PartitionStyle GPT
    $efi = New-Partition -DiskNumber $virtualDisk.Number -Size 260MB -GptType '{c12a7328-f81f-11d2-ba4b-00a0c93ec93b}' -AssignDriveLetter
    $efi | Format-Volume -FileSystem FAT32 -NewFileSystemLabel System -Confirm:$false | Out-Null
    New-Partition -DiskNumber $virtualDisk.Number -Size 16MB -GptType '{e3c9e316-0b5c-4db8-817d-f92df00215ae}' | Out-Null
    $windows = New-Partition -DiskNumber $virtualDisk.Number -UseMaximumSize -AssignDriveLetter
    $windows | Format-Volume -FileSystem NTFS -NewFileSystemLabel Windows -Confirm:$false | Out-Null
    $windows = Get-Partition -DiskNumber $virtualDisk.Number -PartitionNumber $windows.PartitionNumber
    $windowsVolumes = @($windows | Get-Volume)
    Assert-Spike ($windowsVolumes.Count -eq 1 -and (Test-SpikeVolumeGuidPath ([string]$windowsVolumes[0].Path))) 'windows-volume-guid-path' $windowsVolumes
    $windowsReverse = @(Get-Partition -Volume $windowsVolumes[0])
    Assert-Spike ($windowsReverse.Count -eq 1 -and $windowsReverse[0].DiskNumber -eq $virtualDisk.Number -and $windowsReverse[0].PartitionNumber -eq $windows.PartitionNumber) 'windows-volume-maps-to-partition' $windowsVolumes[0].Path
    if (-not (Get-DiskImage -ImagePath $ServerIso).Attached) {
        Mount-DiskImage -ImagePath $ServerIso | Out-Null
        $mountedIso = $true
    }
    $media = (Get-DiskImage -ImagePath $ServerIso | Get-Volume).DriveLetter
    $wim = "${media}:\sources\install.wim"
    $image = Get-WindowsImage -ImagePath $wim -Index 2
    $image | Format-List ImageName,ImageDescription,ImageIndex,Architecture | Out-Host
    Assert-Spike ($image.ImageName -match 'Server.*2022.*(Desktop Experience)' -and $image.Architecture -eq 9) 'server2022-index2-x64-desktop' $image.ImageName
    # DISM requires the assigned letter; subsequent PowerShell file access uses the volume GUID path.
    & dism.exe /Apply-Image "/ImageFile:$wim" /Index:2 "/ApplyDir:$($windows.DriveLetter):\" /CheckIntegrity | Out-Host
    Assert-Spike ($LASTEXITCODE -eq 0) 'offline-image-applied' "$($windows.DriveLetter):"
    $os = [string]$windowsVolumes[0].Path
    $windowsRoot = [IO.Path]::Combine($os, 'Windows')
    [IO.Directory]::CreateDirectory([IO.Path]::Combine($windowsRoot, 'Panther')) | Out-Null
    [IO.File]::Copy((Join-Path $seed 'autounattend.xml'), [IO.Path]::Combine($windowsRoot, 'Panther\unattend.xml'), $true)
    Install-SpikeBootFiles $virtualDisk.Number $efi.PartitionNumber $os $Root ([string]$windows.DriveLetter)
    Remove-SpikeDiskDriveLetters $virtualDisk.Number
    Dismount-VHD -Path $disk
    $diskMounted = $false
    if ($mountedIso) { Dismount-DiskImage -ImagePath $ServerIso; $mountedIso = $false }

    New-SpikeVM $name $disk (Join-Path $Root 'provision-vm')
    Add-VMNetworkAdapter -VMName $name -SwitchName $ProvisionSwitch
    Add-VMDvdDrive -VMName $name -Path $seedIso
    Assert-Spike (@(Get-VMNetworkAdapter -VMName $name).Count -eq 1) 'provision-only-synthetic-nic' $ProvisionSwitch
    $timer = [Diagnostics.Stopwatch]::StartNew()
    Start-VM -Name $name
    Wait-SpikeOff $name ($TimeoutMinutes * 60)
    Write-Host "EVIDENCE provision-and-updates-ms=$($timer.Elapsed.TotalMilliseconds)"
    Remove-SpikeVM $name
    $logs = Join-Path $Root 'guest-logs'
    New-Item $logs -ItemType Directory | Out-Null
    Use-SpikeWindowsDisk $disk {
        param($volume)
        $provision = [IO.Path]::Combine($volume, 'build\provision')
        foreach ($file in @('bootstrap.log','os-update-first.log','os-updated','finalize.log','FAILED.txt','provisioned','finalized','winagent.sha256','pwsh-probe.json','pwsh-probe.stdout.log','pwsh-probe.stderr.log','pwsh-host-trace.log','pwsh-startup-diagnostics.log')) {
            $source = [IO.Path]::Combine($provision, $file)
            if ([IO.File]::Exists($source)) { [IO.File]::Copy($source, (Join-Path $logs $file), $true) }
        }
        foreach ($source in [IO.Directory]::EnumerateFiles($provision, '*-updates-*.json')) { [IO.File]::Copy($source, (Join-Path $logs ([IO.Path]::GetFileName($source))), $true) }
        foreach ($file in @('os-update-first.log','bootstrap.log','finalize.log')) {
            $logPath = Join-Path $logs $file
            if ([IO.File]::Exists($logPath)) { [IO.File]::ReadAllText($logPath) | Out-Host }
        }
        $failureMarker = [IO.Path]::Combine($provision, 'FAILED.txt')
        if ([IO.File]::Exists($failureMarker)) { [IO.File]::ReadAllText($failureMarker) | Out-Host }
        Assert-Spike (-not [IO.File]::Exists($failureMarker)) 'no-failure-marker' $provision
        foreach ($file in @('os-updated','provisioned','finalized')) {
            $marker = [IO.Path]::Combine($provision, $file)
            Assert-Spike ([IO.File]::Exists($marker)) $file $(if ([IO.File]::Exists($marker)) { [IO.File]::ReadAllText($marker) } else { '' })
        }
        $bootstrap = [IO.File]::ReadAllText([IO.Path]::Combine($provision, 'bootstrap.log'))
        foreach ($right in @('SeBatchLogonRight','SeCreateSymbolicLinkPrivilege')) {
            Assert-Spike ($bootstrap -match "Verified $right for motor-job SID S-1-") 'verified-effective-job-right' $right
        }
        $agent = [IO.Path]::Combine($volume, 'ProgramData\Motor\motor-winagent.exe')
        $expected = (([IO.File]::ReadAllText([IO.Path]::Combine($provision, 'winagent.sha256'))) -split '\s+')[0]
        Assert-Spike ((Get-SpikeFileSha256 $agent) -eq $expected) 'installed-agent-hash' $expected
        if (-not $GuestAgentPath) { [IO.File]::Copy([IO.Path]::Combine($provision, 'hyperv-spike.exe'), (Join-Path $Root 'hyperv-spike.exe'), $true) }
        if ($GuestAgentPath) {
            Assert-Spike ((Get-SpikeFileSha256 $agent) -eq (Get-FileHash $GuestAgentPath).Hash) 'sealed-prebuilt-agent' $agent
            Assert-Spike (-not [IO.File]::Exists([IO.Path]::Combine($volume, 'ProgramData\Motor\hyperv-serial-queue.txt'))) 'no-serial-queue-setting' $disk
        }
        # A baseline never carries a job nonce. Each differencing disk gets its own.
        Assert-Spike (-not [IO.File]::Exists([IO.Path]::Combine($volume, 'ProgramData\Motor\hyperv-nonce.txt'))) 'no-baseline-nonce' $disk
    } -ReadOnly
    $expected = ((Get-Content (Join-Path $logs 'winagent.sha256') -Raw) -split '\s+')[0]
    if (-not $GuestAgentPath) {
        & (Join-Path $Root 'hyperv-spike.exe') --version | Out-Host
        Assert-Spike ($LASTEXITCODE -eq 0) 'host-probe-runnable' ((Get-FileHash (Join-Path $Root 'hyperv-spike.exe')).Hash)
    }
    Remove-Item $seed -Recurse -Force
    Remove-Item $seedIso -Force
    Remove-Item (Join-Path $Root 'provision-vm') -Recurse -Force
    (Get-Item $disk).IsReadOnly = $true
    & icacls.exe $disk /inheritance:r /grant:r '*S-1-5-18:R' '*S-1-5-32-544:R' '*S-1-5-83-0:R' | Out-Host
    Assert-Spike ($LASTEXITCODE -eq 0) 'parent-read-only-acl' ((Get-Acl $disk).Sddl)
    $hash = (Get-FileHash $disk).Hash
    @{ parent_sha256=$hash; agent_sha256=$expected; bytes=(Get-Item $disk).Length; created_utc=[DateTime]::UtcNow.ToString('o') } | ConvertTo-Json | Set-Content (Join-Path $Root 'sealed.json') -Encoding ascii
    $sealed = $true
    Assert-Spike ((Get-Item $disk).IsReadOnly) 'sealed-parent' "SHA256=$hash bytes=$((Get-Item $disk).Length)"
} catch {
    Write-Host "FAIL provision $_"
    throw
} finally {
    try {
        if ($diskMounted) {
            try { if ($null -ne $virtualDisk) { Remove-SpikeDiskDriveLetters $virtualDisk.Number } } finally { Dismount-VHD -Path $disk }
        }
        if ($mountedIso) { Dismount-DiskImage -ImagePath $ServerIso }
        if ($lock) {
            Remove-SpikeVM $name
            if ($createdDisk -and -not $sealed) {
                Use-SpikeWindowsDisk $disk {
                    param($volume)
                    $provision = [IO.Path]::Combine($volume, 'build\provision')
                    [IO.Directory]::CreateDirectory($provision) | Out-Null
                    $failureMarker = [IO.Path]::Combine($provision, 'FAILED.txt')
                    if (-not [IO.File]::Exists($failureMarker)) { [IO.File]::WriteAllText($failureMarker, 'Host provisioning failed; see provision-host.log. This disk is not sealed.') }
                }
            }
        }
    } finally {
        if ($transcript) { Stop-Transcript | Out-Host }
        if ($lock) { $lock.Dispose() }
    }
}
