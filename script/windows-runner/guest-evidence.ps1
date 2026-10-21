# Read-only guest diagnostics collected before job-disk removal.
# diagnose-connect.ps1 embeds these functions so its collector
# needs no sidecar file; source-level tests keep the function bodies aligned.
function Save-Json([string]$Path, $Value) {
    $json = ConvertTo-Json -InputObject $Value -Depth 10
    [IO.File]::WriteAllText($Path, $json, [Text.UTF8Encoding]::new($false))
}
function Get-PlainGuestItem([string]$Volume, [string]$Relative) {
    $path = $Volume
    foreach ($part in $Relative.Split('\')) {
        if (-not $part -or $part -in @('.','..') -or $part.Contains(':')) { throw 'Unsafe guest diagnostic path' }
        $path = Join-Path $path $part
        try { $item = Get-Item -LiteralPath $path -Force -ErrorAction Stop } catch {
            if ($_.CategoryInfo.Category -eq 'ObjectNotFound') { return $null }
            throw "Guest diagnostic lookup failed at $path : $_"
        }
        if ($item.Attributes -band [IO.FileAttributes]::ReparsePoint) { throw "Guest reparse path refused: $path" }
    }
    return $item
}

function Convert-DiagnosticEvent($Event) {
    $message = $null
    try { $message = $Event.Message } catch { $message = "Message resources unavailable: $_" }
    return [ordered]@{utc=$(if ($Event.TimeCreated) { $Event.TimeCreated.ToUniversalTime().ToString('o') } else { $null }); provider=$Event.ProviderName; id=$Event.Id; record_id=$Event.RecordId; message=$message; xml=$Event.ToXml()}
}
function Save-GuestAgentEvents([string]$Source, [string]$Channel, [string]$Destination) {
    New-Item -ItemType Directory -Path $Destination -Force | Out-Null
    $selected = [Collections.Generic.List[object]]::new()
    try {
        $query = '*'
        # Do not filter guest time against host time: integration time sync is off.
        # Match every provider's XML, not localized message text, to retain crash,
        # hang and service records even when host message resources are missing.
        Get-WinEvent -Path $Source -FilterXPath $query -ErrorAction Stop | ForEach-Object {
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
            $text = & wevtutil.exe epl $Source $output /lf:true /ow:true "/q:$filter" 2>&1 | Out-String
            if ($LASTEXITCODE -ne 0) { throw "wevtutil exit=$LASTEXITCODE $text" }
        } catch { Add-Content -LiteralPath "$Destination\$Channel-export-errors.txt" -Value ($_ | Out-String) -Encoding UTF8 }
    }
    if ($Channel -eq 'System') {
        try {
            $boot = @(Get-WinEvent -Path $Source -FilterXPath '*[System[EventID=12 or EventID=13 or EventID=41 or EventID=6005 or EventID=6006 or EventID=6008]]' -MaxEvents 128 | ForEach-Object {
                $event = $_
                try { Convert-DiagnosticEvent $event } finally { $event.Dispose() }
            })
            Save-Json "$Destination\guest-boot-context.json" $boot
        } catch { Set-Content -LiteralPath "$Destination\guest-boot-query-status.txt" -Value ($_ | Out-String) -Encoding UTF8 }
    }
}
function Use-DiagnosticWindowsDisk([string]$Path, [scriptblock]$Action, [string]$EvidenceDirectory) {
    $identity = [ordered]@{utc=[DateTime]::UtcNow.ToString('o'); requested_path=[IO.Path]::GetFullPath($Path); read_only=$true; matched=$false}
    $mounted = $false
    $assigned = $false
    $partition = $null
    try {
        # Mark the mount before Get-Disk, so a failed disk query still dismounts.
        $image = Mount-VHD -Path $Path -ReadOnly -Passthru
        $mounted = $true
        $identity['mount_returned_path'] = $image.Path
        $disk = $image | Get-Disk
        $identity['disk_number'] = $disk.Number
        $identity['disk_unique_id'] = [string]$disk.UniqueId
        $identity['disk_location'] = [string]$disk.Location
        $mapped = @(Get-VHD -DiskNumber $disk.Number)
        $identity['mapped_vhd_paths'] = @($mapped | ForEach-Object Path)
        if ($mapped.Count -ne 1 -or -not [StringComparer]::OrdinalIgnoreCase.Equals($identity.requested_path, [IO.Path]::GetFullPath($mapped[0].Path)) -or -not [StringComparer]::OrdinalIgnoreCase.Equals($identity.requested_path, [IO.Path]::GetFullPath($image.Path))) { throw 'Mounted disk does not map back to the requested job child' }
        $identity['parent_path'] = $mapped[0].ParentPath
        $partitions = @(Get-Partition -DiskNumber $disk.Number | Where-Object { $_.GptType -eq '{ebd0a0a2-b9e5-4433-87c0-68b6b72699c7}' })
        if ($partitions.Count -ne 1) { throw 'Expected one Windows basic-data partition' }
        $partition = $partitions[0]
        if (-not $partition.DriveLetter) {
            $partition | Add-PartitionAccessPath -AssignDriveLetter
            $assigned = $true
            $partition = Get-Partition -DiskNumber $disk.Number -PartitionNumber $partition.PartitionNumber
        }
        $volume = "$($partition.DriveLetter):\"
        $reverse = @(Get-Partition -DriveLetter $partition.DriveLetter)
        if ($reverse.Count -ne 1 -or $reverse[0].DiskNumber -ne $disk.Number -or $reverse[0].PartitionNumber -ne $partition.PartitionNumber) { throw 'Guest drive letter resolves to a different disk or partition' }
        $volumeInfo = Get-Volume -DriveLetter $partition.DriveLetter
        $identity['partition_number'] = $partition.PartitionNumber
        $identity['partition_guid'] = [string]$partition.Guid
        $identity['volume_path'] = $volume
        $identity['volume_unique_id'] = [string]$volumeInfo.UniqueId
        $identity['volume_label'] = [string]$volumeInfo.FileSystemLabel
        $identity['matched'] = $true
        if ($EvidenceDirectory) { Save-Json "$EvidenceDirectory\mounted-disk-identity.json" $identity }
        $windows = Get-PlainGuestItem $volume 'Windows'
        if (-not $windows -or -not $windows.PSIsContainer) { throw 'Mounted partition is not a Windows volume' }
        & $Action $volume
    } catch {
        $identity['error'] = [string]$_
        if ($EvidenceDirectory) { Save-Json "$EvidenceDirectory\mounted-disk-identity.json" $identity }
        throw
    } finally {
        try {
            if ($assigned -and $partition -and $partition.DriveLetter) { $partition | Remove-PartitionAccessPath -AccessPath "$($partition.DriveLetter):\" }
        } finally { if ($mounted) { Dismount-VHD -Path $Path } }
    }
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
                    Copy-Item -LiteralPath $item.FullName -Destination $target
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
            if ($agent -and -not $agent.PSIsContainer) {
                Save-Json "$Destination\guest-agent-binary.json" @{path='C:\ProgramData\Motor\motor-winagent.exe'; bytes=$agent.Length; sha256=(Get-FileHash -LiteralPath $agent.FullName -Algorithm SHA256).Hash; modified_utc=$agent.LastWriteTimeUtc.ToString('o')}
            } else { Save-Json "$Destination\guest-agent-binary.json" @{present=$false} }
        } catch { Set-Content -LiteralPath "$Destination\guest-agent-error.txt" -Value ($_ | Out-String) -Encoding UTF8 }
        foreach ($channel in @('System','Application')) {
            try {
                $item = Get-PlainGuestItem $volume "Windows\System32\winevt\Logs\$channel.evtx"
                if ($item -and -not $item.PSIsContainer) { Save-GuestAgentEvents $item.FullName $channel "$Destination\events" }
                else { Save-Json "$Destination\$channel-log-state.json" @{present=$false} }
            } catch { Set-Content -LiteralPath "$Destination\$channel-log-error.txt" -Value ($_ | Out-String) -Encoding UTF8 }
        }
    }
}
