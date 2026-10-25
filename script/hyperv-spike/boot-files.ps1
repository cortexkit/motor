# Offline UEFI deployment and BCDBoot comparison; never targets the host ESP.
function Get-SpikeBcdBootArguments([string]$Windows, [char]$Letter, [switch]$Offline) {
    $arguments = @($Windows, '/s', "${Letter}:", '/f', 'UEFI', '/v')
    if ($Offline) { $arguments += '/offline' }
    return $arguments
}

function Assert-SpikeEspState($Partition, $Volume, [int]$DiskNumber) {
    $state = [ordered]@{
        disk=$Partition.DiskNumber; partition=$Partition.PartitionNumber
        gptType=$Partition.GptType; letter=[string]$Partition.DriveLetter
        fileSystem=$Volume.FileSystem; volumeLetter=[string]$Volume.DriveLetter
    }
    $evidence = $state | ConvertTo-Json -Compress
    Assert-Spike ($Partition.DiskNumber -eq $DiskNumber -and $Partition.GptType -eq '{c12a7328-f81f-11d2-ba4b-00a0c93ec93b}') 'esp-partition-identity' $evidence
    Assert-Spike ($Volume.FileSystem -eq 'FAT32') 'esp-fat32' $evidence
    Assert-Spike ([string]$Partition.DriveLetter -match '^[A-Z]$' -and $Partition.DriveLetter -eq $Volume.DriveLetter) 'esp-mounted-letter' $evidence
}

function Get-SpikeFreshEsp([int]$DiskNumber, [int]$PartitionNumber) {
    $partition = Get-Partition -DiskNumber $DiskNumber -PartitionNumber $PartitionNumber
    if ([string]$partition.DriveLetter -notmatch '^[A-Z]$') {
        $partition | Add-PartitionAccessPath -AssignDriveLetter
        $partition = Get-Partition -DiskNumber $DiskNumber -PartitionNumber $PartitionNumber
    }
    $volume = $partition | Get-Volume
    Get-Disk -Number $DiskNumber | Select-Object Number,Path,PartitionStyle,IsOffline,IsReadOnly,OperationalStatus | Format-List | Out-Host
    $partition | Select-Object DiskNumber,PartitionNumber,GptType,DriveLetter,AccessPaths,Size,IsOffline,IsReadOnly | Format-List | Out-Host
    $volume | Select-Object DriveLetter,Path,FileSystem,FileSystemType,HealthStatus,OperationalStatus,Size,SizeRemaining | Format-List | Out-Host
    Assert-SpikeEspState $partition $volume $DiskNumber
    $mapping = Get-Partition -DriveLetter $partition.DriveLetter
    Assert-Spike ($mapping.DiskNumber -eq $DiskNumber -and $mapping.PartitionNumber -eq $PartitionNumber) 'esp-letter-resolves-to-target-disk' "$($partition.DriveLetter): disk=$DiskNumber partition=$PartitionNumber"
    # BCDBoot needs a drive letter, but file probes and writes use the volume GUID path.
    $volumes = @(Get-Volume -Partition $partition)
    Assert-Spike ($volumes.Count -eq 1 -and (Test-SpikeVolumeGuidPath ([string]$volumes[0].Path))) 'esp-volume-guid-path' "$DiskNumber/$PartitionNumber"
    $reverse = @(Get-Partition -Volume $volumes[0])
    Assert-Spike ($reverse.Count -eq 1 -and $reverse[0].DiskNumber -eq $DiskNumber -and $reverse[0].PartitionNumber -eq $PartitionNumber) 'esp-volume-guid-maps-to-partition' $volumes[0].Path
    $root = [string]$volumes[0].Path
    Assert-Spike ([IO.Directory]::Exists($root)) 'esp-root-accessible' $root
    $probe = [IO.Path]::Combine($root, ('write-probe-' + [Guid]::NewGuid().ToString('N') + '.txt'))
    try {
        [IO.File]::WriteAllText($probe, 'motor-esp-write-probe')
        Assert-Spike ([IO.File]::ReadAllText($probe) -eq 'motor-esp-write-probe') 'esp-writable' $probe
    } finally { if ([IO.File]::Exists($probe)) { [IO.File]::Delete($probe) } }
    return [pscustomobject]@{DiskNumber=$partition.DiskNumber; PartitionNumber=$partition.PartitionNumber; DriveLetter=[char]$partition.DriveLetter; VolumeGuidPath=$root}
}

function Invoke-SpikeBcdBoot([string]$Executable, [string]$Windows, [char]$Letter, [string]$VolumeGuidPath, [string]$Label, [string]$Root, [switch]$Offline) {
    $version = [Diagnostics.FileVersionInfo]::GetVersionInfo($Executable)
    Write-Host "EVIDENCE bcdboot-$Label executable=$Executable fileVersion=$($version.FileVersion) productVersion=$($version.ProductVersion) SHA256=$(Get-SpikeFileSha256 $Executable)"
    $arguments = @(Get-SpikeBcdBootArguments $Windows $Letter -Offline:$Offline)
    Write-Host "EVIDENCE bcdboot-$Label arguments=$($arguments | ConvertTo-Json -Compress)"
    $process = New-Object Diagnostics.Process
    $process.StartInfo.FileName = $Executable
    $process.StartInfo.Arguments = (@($arguments | ForEach-Object { '"' + $_ + '"' }) -join ' ')
    $process.StartInfo.UseShellExecute = $false
    $process.StartInfo.CreateNoWindow = $true
    $process.StartInfo.RedirectStandardOutput = $true
    $process.StartInfo.RedirectStandardError = $true
    $started = $false
    try {
        $started = $process.Start()
        Assert-Spike $started 'bcdboot-process-started' $Label
        # Drain both streams concurrently so verbose errors cannot fill a pipe
        # and deadlock the installer. Keep separate raw streams for diagnosis.
        $stdout = $process.StandardOutput.ReadToEndAsync()
        $stderr = $process.StandardError.ReadToEndAsync()
        $finished = $process.WaitForExit(120000)
        if (-not $finished) { $process.Kill(); $process.WaitForExit() }
        [IO.File]::WriteAllText((Join-Path $Root "bcdboot-$Label.stdout.log"), $stdout.Result)
        [IO.File]::WriteAllText((Join-Path $Root "bcdboot-$Label.stderr.log"), $stderr.Result)
        Write-Host "EVIDENCE bcdboot-$Label stdout:`n$($stdout.Result)"
        Write-Host "EVIDENCE bcdboot-$Label stderr:`n$($stderr.Result)"
        Assert-Spike $finished 'bcdboot-finished-within-120s' $Label
        $code = $process.ExitCode
    } finally {
        try {
            if ($started -and -not $process.HasExited) { $process.Kill(); $process.WaitForExit() }
        } finally { $process.Dispose() }
    }
    $status = if ($code -eq 0) { 'PASS' } else { 'FAIL' }
    Write-Host "$status bcdboot-$Label exit=$code target=${Letter}: (comparison failures are evidence, not installer success)"
    [IO.Directory]::EnumerateFiles($VolumeGuidPath, '*', [IO.SearchOption]::AllDirectories) | ForEach-Object {
        $file = [IO.FileInfo]::new($_)
        [pscustomobject]@{FullName=$file.FullName; Length=$file.Length}
    } | Format-Table -AutoSize | Out-Host
    return $code
}

function Invoke-SpikeBcdBootComparison([string]$Executable, [string]$Windows, [string]$Root, [string]$Label, [switch]$Offline) {
    $path = Join-Path $Root "boot-probe-$Label.vhdx"
    Assert-Spike (-not (Test-Path -LiteralPath $path)) 'fresh-bcdboot-probe-path' $path
    $created = $false
    $mounted = $false
    $disk = $null
    try {
        # Each executable gets a fresh ESP, so partial files from a failed run
        # cannot make the next executable appear to repair an unrelated failure.
        New-VHD -Path $path -Dynamic -SizeBytes 300MB | Out-Null
        $created = $true
        $image = Mount-VHD -Path $path -Passthru
        $mounted = $true
        $disk = $image | Get-Disk
        Initialize-Disk -Number $disk.Number -PartitionStyle GPT | Out-Null
        $partition = New-Partition -DiskNumber $disk.Number -Size 260MB -GptType '{c12a7328-f81f-11d2-ba4b-00a0c93ec93b}' -AssignDriveLetter
        $partition | Format-Volume -FileSystem FAT32 -NewFileSystemLabel BootProbe -Confirm:$false | Out-Null
        $partition = Get-SpikeFreshEsp $disk.Number $partition.PartitionNumber
        [void](Invoke-SpikeBcdBoot $Executable $Windows $partition.DriveLetter $partition.VolumeGuidPath $Label $Root -Offline:$Offline)
    } finally {
        if ($mounted) {
            try { if ($null -ne $disk) { Remove-SpikeDiskDriveLetters $disk.Number } } finally { Dismount-VHD -Path $path }
        }
        if ($created) { Remove-Item -LiteralPath $path -Force }
        Assert-Spike (-not (Test-Path -LiteralPath $path)) 'bcdboot-probe-disk-removed' $path
    }
}

function Install-SpikeBootFiles([int]$DiskNumber, [int]$EspPartitionNumber, [string]$WindowsVolumePath, [string]$Root, [char]$WindowsDriveLetter) {
    $hostBcdBoot = [IO.Path]::Combine($env:SystemRoot, 'System32\bcdboot.exe')
    $windowsDriveRoot = "${WindowsDriveLetter}:\Windows"
    $imageBcdBoot = [IO.Path]::Combine($windowsDriveRoot, 'System32\bcdboot.exe')
    foreach ($relative in @('System32\bcdboot.exe','System32\Config\BCD-Template','Boot\EFI\bootmgfw.efi','Boot\EFI\bootmgr.efi')) {
        $item = Get-SpikePlainGuestItem $WindowsVolumePath "Windows\$relative"
        Assert-Spike ($item -and -not $item.PSIsContainer) 'source-boot-file-present' "$relative bytes=$(if ($item) { $item.Length } else { 0 })"
    }
    $bootEx = Get-SpikePlainGuestItem $WindowsVolumePath 'Windows\Boot\EFI_EX'
    Write-Host "EVIDENCE source-bootex-present=$([bool]$bootEx)"
    $partition = Get-SpikeFreshEsp $DiskNumber $EspPartitionNumber
    [void](Invoke-SpikeBcdBoot $hostBcdBoot $windowsDriveRoot $partition.DriveLetter $partition.VolumeGuidPath 'host-default' $Root)
    $version = [Diagnostics.FileVersionInfo]::GetVersionInfo($hostBcdBoot)
    if ($version.FileBuildPart -gt 26100 -or ($version.FileBuildPart -eq 26100 -and $version.FilePrivatePart -ge 8037)) {
        $partition = Get-SpikeFreshEsp $DiskNumber $EspPartitionNumber
        [void](Invoke-SpikeBcdBoot $hostBcdBoot $windowsDriveRoot $partition.DriveLetter $partition.VolumeGuidPath 'host-offline' $Root -Offline)
    } else {
        Write-Host 'SKIP bcdboot-host-offline: /offline is documented only from Windows 11 build 26100.8037 (not a PASS)'
    }
    # Keep the boot-file installer matched to the deployed Server image.
    # /s binds writes to this VHDX and avoids changing the host's NVRAM.
    $partition = Get-SpikeFreshEsp $DiskNumber $EspPartitionNumber
    $code = Invoke-SpikeBcdBoot $imageBcdBoot $windowsDriveRoot $partition.DriveLetter $partition.VolumeGuidPath 'image' $Root
    Assert-Spike ($code -eq 0) 'uefi-boot-files' "image=$imageBcdBoot target=$($partition.DriveLetter): exit=$code"
    foreach ($relative in @('EFI\Microsoft\Boot\BCD','EFI\Microsoft\Boot\bootmgfw.efi','EFI\Boot\bootx64.efi')) {
        $file = Get-SpikePlainGuestItem $partition.VolumeGuidPath $relative
        Assert-Spike ($file -and -not $file.PSIsContainer) 'uefi-boot-output' "$relative SHA256=$(if ($file) { Get-SpikeFileSha256 $file.FullName } else { '' })"
    }
}
