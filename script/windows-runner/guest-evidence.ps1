# Shared read-only guest diagnostics used by the installer identity check and
# diagnose-connect.ps1; both use .NET APIs on volume GUID paths.
function Save-Json([string]$Path, $Value) {
    $json = ConvertTo-Json -InputObject $Value -Depth 10
    [IO.File]::WriteAllText($Path, $json, [Text.UTF8Encoding]::new($false))
}
function Test-RunnerVolumeGuidPath([string]$Path) {
    return [regex]::IsMatch($Path, '\A\\\\\?\\Volume\{[0-9A-Fa-f]{8}-(?:[0-9A-Fa-f]{4}-){3}[0-9A-Fa-f]{12}\}\\\z')
}
function Get-RunnerPropertyValue([object]$Object, [string]$Name) {
    if ($null -eq $Object) { return $null }
    $property = $Object.PSObject.Properties[$Name]
    if ($null -eq $property) { return $null }
    return $property.Value
}
function Get-PlainGuestItem([string]$Volume, [string]$Relative) {
    if (-not (Test-RunnerVolumeGuidPath $Volume)) { throw 'Guest volume path is not a volume GUID path' }
    $path = $Volume
    foreach ($part in $Relative.Split('\')) {
        if (-not $part -or $part -in @('.','..') -or $part.Contains(':') -or $part.Contains('/')) { throw 'Unsafe guest diagnostic path' }
        $path = [IO.Path]::Combine($path, $part)
        try { $attributes = [IO.File]::GetAttributes($path) } catch [IO.FileNotFoundException] { return $null } catch [IO.DirectoryNotFoundException] { return $null } catch {
            throw "Guest diagnostic lookup failed at $path : $_"
        }
        if ($attributes -band [IO.FileAttributes]::ReparsePoint) { throw "Guest reparse path refused: $path" }
        $isDirectory = [bool]($attributes -band [IO.FileAttributes]::Directory)
        if ($isDirectory) {
            $info = [IO.DirectoryInfo]::new($path)
            $length = 0
        } else {
            $info = [IO.FileInfo]::new($path)
            $length = $info.Length
        }
        $item = [pscustomobject]@{FullName=$info.FullName; Name=$info.Name; Attributes=$attributes; PSIsContainer=$isDirectory; Length=$length; LastWriteTimeUtc=$info.LastWriteTimeUtc}
    }
    return $item
}
function Get-GuestFileSha256([object]$Item) {
    $stream = [IO.File]::OpenRead($Item.FullName)
    $sha = [Security.Cryptography.SHA256]::Create()
    try { return [BitConverter]::ToString($sha.ComputeHash($stream)).Replace('-', '') } finally {
        $sha.Dispose()
        $stream.Dispose()
    }
}

function Convert-DiagnosticEvent($Event) {
    $message = $null
    try { $message = $Event.Message } catch { $message = "Message resources unavailable: $_" }
    return [ordered]@{utc=$(if ($Event.TimeCreated) { $Event.TimeCreated.ToUniversalTime().ToString('o') } else { $null }); provider=$Event.ProviderName; id=$Event.Id; record_id=$Event.RecordId; message=$message; xml=$Event.ToXml()}
}
function Save-GuestAgentEvents([string]$Source, [string]$Channel, [string]$Destination) {
    New-Item -ItemType Directory -Path $Destination -Force | Out-Null
    $selected = [Collections.Generic.List[object]]::new()
    $stagedSource = Join-Path $Destination "$Channel-source.evtx"
    # Stage with .NET so event tools read a host path, not a PowerShell drive for the mounted guest.
    try {
        [IO.File]::Copy($Source, $stagedSource, $true)
        try {
            $query = '*'
            # Do not filter guest time against host time: integration time sync is off.
            # Match every provider's XML, not localized message text, to retain crash,
            # hang and service records even when host message resources are missing.
            Get-WinEvent -Path $stagedSource -FilterXPath $query -ErrorAction Stop | ForEach-Object {
                $event = $_
                try {
                    if ($event.ToXml() -match '(?i)motor-winagent(?:\.exe)?|Motor Windows job agent') { $selected.Add((Convert-DiagnosticEvent $event)) }
                } finally { $event.Dispose() }
            }
        } catch {
            Set-Content -LiteralPath "$Destination\$Channel-query-status.txt" -Value ($_ | Out-String) -Encoding UTF8
        }
        Save-Json "$Destination\$Channel-agent-events.json" $selected.ToArray()
        for ($offset = 0; $offset -lt $selected.Count; $offset += 128) {
            $chunk = $selected.GetRange($offset, [Math]::Min(128, $selected.Count - $offset))
            $ids = @($chunk | ForEach-Object { "EventRecordID=$($_.record_id)" })
            $filter = '*[System[(' + ($ids -join ' or ') + ')]]'
            $output = Join-Path $Destination ("$Channel-agent-{0:D3}.evtx" -f [int]($offset / 128))
            try {
                $text = & wevtutil.exe epl $stagedSource $output /lf:true /ow:true "/q:$filter" 2>&1 | Out-String
                if ($LASTEXITCODE -ne 0) { throw "wevtutil exit=$LASTEXITCODE $text" }
            } catch { Add-Content -LiteralPath "$Destination\$Channel-export-errors.txt" -Value ($_ | Out-String) -Encoding UTF8 }
        }
        if ($Channel -eq 'System') {
            try {
                $boot = @(Get-WinEvent -Path $stagedSource -FilterXPath '*[System[EventID=12 or EventID=13 or EventID=41 or EventID=6005 or EventID=6006 or EventID=6008]]' -MaxEvents 128 | ForEach-Object {
                    $event = $_
                    try { Convert-DiagnosticEvent $event } finally { $event.Dispose() }
                })
                Save-Json "$Destination\guest-boot-context.json" $boot
            } catch { Set-Content -LiteralPath "$Destination\guest-boot-query-status.txt" -Value ($_ | Out-String) -Encoding UTF8 }
        }
    } finally {
        if ([IO.File]::Exists($stagedSource)) { [IO.File]::Delete($stagedSource) }
    }
}
function Wait-RunnerDiagnosticVolume([int]$DiskNumber, [int]$PartitionNumber, [int]$TimeoutSeconds) {
    # Read the GUID path from the partition; read-only VHD mounts may have no Get-Volume object.
    $clock = [Diagnostics.Stopwatch]::StartNew()
    $lastError = 'partition AccessPaths has not acquired a volume GUID path'
    while ($clock.Elapsed.TotalSeconds -lt $TimeoutSeconds) {
        try {
            $partition = Get-Partition -DiskNumber $DiskNumber -PartitionNumber $PartitionNumber -ErrorAction Stop
            if ($partition) {
                $rawAccessPaths = Get-RunnerPropertyValue $partition 'AccessPaths'
                $accessPaths = @()
                if ($null -ne $rawAccessPaths) { $accessPaths = @($rawAccessPaths) }
                $guidPaths = @($accessPaths | Where-Object { Test-RunnerVolumeGuidPath ([string]$_) })
                if ($guidPaths.Count -eq 1) {
                    return [pscustomobject]@{partition=$partition; volume_path=[string]$guidPaths[0]; access_paths=$accessPaths}
                }
                if ($guidPaths.Count -gt 1) {
                    $lastError = 'partition has several volume GUID access paths'
                } else {
                    $lastError = 'partition AccessPaths has no volume GUID path yet'
                }
            } else {
                $lastError = "partition $PartitionNumber on disk $DiskNumber has not appeared yet"
            }
        } catch { $lastError = [string]$_ }
        Start-Sleep -Milliseconds 100
    }
    throw "Timed out after $TimeoutSeconds seconds waiting for a volume GUID path on disk $DiskNumber partition ${PartitionNumber}: $lastError"
}

function Use-DiagnosticWindowsDisk([string]$Path, [scriptblock]$Action, [string]$EvidenceDirectory, [int]$VolumeWaitSeconds = 15) {
    $identity = [ordered]@{utc=[DateTime]::UtcNow.ToString('o'); requested_path=[IO.Path]::GetFullPath($Path); read_only=$true; matched=$false}
    $mounted = $false
    $diskNumber = $null
    $partitionNumber = $null
    $partition = $null
    try {
        # Keep the sealed image read-only and use its GUID path without assigning a drive letter.
        # Mark the mount before Get-Disk, so a failed disk query still dismounts.
        $image = Mount-VHD -Path $Path -ReadOnly -Passthru
        $mounted = $true
        $imagePath = [string](Get-RunnerPropertyValue $image 'Path')
        $identity['mount_returned_path'] = $imagePath
        $disk = $image | Get-Disk
        $diskNumberValue = Get-RunnerPropertyValue $disk 'Number'
        if ($null -eq $diskNumberValue) { throw 'Mounted VHD returned no disk number' }
        $diskNumber = [int]$diskNumberValue
        $identity['disk_number'] = $diskNumber
        $identity['disk_unique_id'] = [string](Get-RunnerPropertyValue $disk 'UniqueId')
        $identity['disk_location'] = [string](Get-RunnerPropertyValue $disk 'Location')
        $mapped = @(Get-VHD -DiskNumber $diskNumber)
        $identity['mapped_vhd_paths'] = @($mapped | ForEach-Object { Get-RunnerPropertyValue $_ 'Path' })
        if ($mapped.Count -ne 1) { throw 'Mounted disk does not map back to the requested job child' }
        $mappedPath = [string](Get-RunnerPropertyValue $mapped[0] 'Path')
        if ([string]::IsNullOrWhiteSpace($mappedPath) -or [string]::IsNullOrWhiteSpace($imagePath)) { throw 'Mounted disk did not report its VHD path' }
        if (-not [StringComparer]::OrdinalIgnoreCase.Equals($identity.requested_path, [IO.Path]::GetFullPath($mappedPath))) { throw 'Mounted disk does not map back to the requested job child' }
        if (-not [StringComparer]::OrdinalIgnoreCase.Equals($identity.requested_path, [IO.Path]::GetFullPath($imagePath))) { throw 'Mounted disk does not map back to the requested job child' }
        $identity['parent_path'] = [string](Get-RunnerPropertyValue $mapped[0] 'ParentPath')
        $partitions = @(Get-Partition -DiskNumber $diskNumber | Where-Object { (Get-RunnerPropertyValue $_ 'GptType') -eq '{ebd0a0a2-b9e5-4433-87c0-68b6b72699c7}' })
        if ($partitions.Count -ne 1) { throw 'Expected one Windows basic-data partition' }
        $partition = $partitions[0]
        $partitionNumberValue = Get-RunnerPropertyValue $partition 'PartitionNumber'
        if ($null -eq $partitionNumberValue) { throw 'Mounted Windows partition returned no partition number' }
        $partitionNumber = [int]$partitionNumberValue
        $binding = Wait-RunnerDiagnosticVolume -DiskNumber $diskNumber -PartitionNumber $partitionNumber -TimeoutSeconds $VolumeWaitSeconds
        $partition = $binding.partition
        $volume = [string]$binding.volume_path
        $accessPaths = @($binding.access_paths)
        $identity['partition_number'] = $partitionNumber
        $identity['partition_guid'] = [string](Get-RunnerPropertyValue $partition 'Guid')
        $identity['partition_access_paths'] = $accessPaths
        $identity['volume_path'] = $volume
        $identity['volume_guid_path'] = $volume
        $identity['drive_letter'] = [string](Get-RunnerPropertyValue $partition 'DriveLetter')
        $identity['matched'] = $true
        if ($EvidenceDirectory) { Save-Json "$EvidenceDirectory\mounted-disk-identity.json" $identity }
        $windows = Get-PlainGuestItem $volume 'Windows'
        if ($null -eq $windows) { throw 'Mounted partition is not a Windows volume' }
        if (-not $windows.PSIsContainer) { throw 'Mounted partition is not a Windows volume' }
        & $Action $volume
    } catch {
        $identity['error'] = [string]$_
        if ($null -ne $partitionNumber) { $identity['partition_number'] = $partitionNumber }
        if ($EvidenceDirectory) { Save-Json "$EvidenceDirectory\mounted-disk-identity.json" $identity }
        throw
    } finally { if ($mounted) { Dismount-VHD -Path $Path } }
}
function Save-DiagnosticGuestEvidence([string]$Disk, [string]$Destination, [switch]$Lightweight) {
    New-Item -ItemType Directory -Path $Destination -Force | Out-Null
    Use-DiagnosticWindowsDisk -Path $Disk -EvidenceDirectory $Destination -Action {
        param($volume)
        $known = [ordered]@{
            'ProgramData\Motor\hyperv-agent.log' = 'Agent executable SHA/build feature, service startup, nonce/DVD wait, hvsock bind/accept/peer refusal and I/O byte counters. An EOF after this passive probe closes is expected.'
            'ProgramData\Motor\peer-proof.json' = 'Peer-isolation probe results, normally absent for an ordinary agent boot.'
            'build\provision\bootstrap.log' = 'Image setup transcript, agent installation branch and service setup failures.'
            'build\provision\finalize.log' = 'Image finalization/sealing transcript.'
            'build\provision\os-update-first.log' = 'First Windows servicing/update transcript; pending servicing can delay boot.'
            'build\provision\FAILED.txt' = 'Provisioning/finalization failure marker, normally absent.'
            'build\provision\provisioned' = 'Successful provisioning marker.'
            'build\provision\finalized' = 'Successful finalization marker.'
            'build\provision\os-updated' = 'OS update phase completion marker.'
            'build\provision\winagent.sha256' = 'Agent digest recorded by image setup, to compare with the actual baked executable.'
            'build\provision\pwsh-probe.json' = 'PowerShell installation/execution probe result from provisioning.'
            'build\provision\pwsh-probe.stdout.log' = 'Provisioning PowerShell probe stdout.'
            'build\provision\pwsh-probe.stderr.log' = 'Provisioning PowerShell probe stderr.'
            'build\provision\pwsh-host-trace.log' = 'PowerShell host startup trace from provisioning.'
            'build\provision\pwsh-startup-diagnostics.log' = 'PowerShell startup diagnostic details from provisioning.'
        }
        if ($Lightweight) {
            $known = [ordered]@{
                'ProgramData\Motor\hyperv-agent.log' = $known['ProgramData\Motor\hyperv-agent.log']
                'ProgramData\Motor\peer-proof.json' = $known['ProgramData\Motor\peer-proof.json']
            }
        }
        $manifest = [Collections.Generic.List[object]]::new()
        foreach ($relative in $known.Keys) {
            $entry = [ordered]@{path=$relative; expected=$known[$relative]; present=$false}
            try {
                $item = Get-PlainGuestItem $volume $relative
                if ($item) {
                    if ($item.PSIsContainer) { throw 'Expected a regular log file, not a directory' }
                    $target = Join-Path "$Destination\files" $relative
                    New-Item -ItemType Directory -Path ([IO.Path]::GetDirectoryName($target)) -Force | Out-Null
                    [IO.File]::Copy($item.FullName, $target, $true)
                    $entry['present'] = $true
                    $entry['bytes'] = $item.Length
                    $entry['modified_utc'] = $item.LastWriteTimeUtc.ToString('o')
                }
            } catch { $entry['error'] = [string]$_ }
            $manifest.Add($entry)
        }
        Save-Json "$Destination\guest-paths.json" $manifest.ToArray()
        if ($Lightweight) { return }
        try {
            $agent = Get-PlainGuestItem $volume 'ProgramData\Motor\motor-winagent.exe'
            if ($null -eq $agent) {
                Save-Json "$Destination\guest-agent-binary.json" @{present=$false}
            } elseif (-not $agent.PSIsContainer) {
                Save-Json "$Destination\guest-agent-binary.json" @{path='C:\ProgramData\Motor\motor-winagent.exe'; bytes=$agent.Length; sha256=(Get-GuestFileSha256 $agent); modified_utc=$agent.LastWriteTimeUtc.ToString('o')}
            } else {
                throw 'Expected guest-agent executable to be a regular file'
            }
        } catch { Set-Content -LiteralPath "$Destination\guest-agent-error.txt" -Value ($_ | Out-String) -Encoding UTF8 }
        foreach ($channel in @('System','Application')) {
            try {
                $item = Get-PlainGuestItem $volume "Windows\System32\winevt\Logs\$channel.evtx"
                if ($null -eq $item) { Save-Json "$Destination\$channel-log-state.json" @{present=$false} }
                elseif (-not $item.PSIsContainer) { Save-GuestAgentEvents $item.FullName $channel "$Destination\events" }
                else { throw 'Expected a guest event log file, not a directory' }
            } catch { Set-Content -LiteralPath "$Destination\$channel-log-error.txt" -Value ($_ | Out-String) -Encoding UTF8 }
        }
    }
}
