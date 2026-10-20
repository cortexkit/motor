# Selected evidence from a stopped disposable overlay, never from host secrets.
function Get-SpikeClientProgress([string]$Text) {
    $progress = [ordered]@{}
    foreach ($line in ($Text -split '\r?\n')) {
        if (-not $line.StartsWith('EVIDENCE transport ')) { continue }
        try { $event = $line.Substring('EVIDENCE transport '.Length) | ConvertFrom-Json }
        catch { Write-Host 'EVIDENCE incomplete transport trace record'; continue }
        $progress['last_transport_event'] = $event.event
        $progress['last_transport_event_unix_ms'] = $event.unix_ms
        switch ($event.event) {
            'first-com-byte' { $progress['first_com_byte_unix_ms'] = $event.detail.first_com_byte_unix_ms }
            'hello-validated' { $progress['hello_unix_ms'] = $event.detail.hello_unix_ms }
            'await-frame' { $progress['last_protocol_wait'] = $event.detail.state }
            'send-receive-tree-done' { $progress['tree_send_done_unix_ms'] = $event.unix_ms }
            'send-tree-end-done' { $progress['tree_end_send_done_unix_ms'] = $event.unix_ms }
        }
    }
    return $progress
}

function Get-SpikePlainGuestItem([string]$Volume, [string]$Relative, [scriptblock]$ReadItem = {
    param($Path) Get-Item -LiteralPath $Path -Force -ErrorAction SilentlyContinue
}) {
    $path = $Volume
    foreach ($component in $Relative.Split('\')) {
        if (-not $component -or $component -in @('.','..') -or $component.Contains(':')) { throw 'Invalid guest diagnostic path' }
        $path = Join-Path $path $component
        $item = & $ReadItem $path
        if (-not $item) { return $null }
        # The VM is stopped, so checking each ancestor prevents a guest junction
        # from redirecting privileged host inspection to a host or UNC target.
        if ($item.Attributes -band [IO.FileAttributes]::ReparsePoint) {
            Write-Host "EVIDENCE skipped-guest-reparse-path $path"
            return $null
        }
    }
    return $item
}

function Save-SpikeGuestEvidence([string]$Disk, [string]$Directory) {
    New-Item $Directory -ItemType Directory -Force | Out-Null
    Use-SpikeWindowsDisk $Disk {
        param($volume)
        $agent = Get-SpikePlainGuestItem $volume 'ProgramData\Motor\motor-winagent.exe'
        if ($agent) {
            Get-FileHash -LiteralPath $agent.FullName | Format-List | Out-String | Set-Content (Join-Path $Directory 'agent-hash.txt') -Encoding UTF8
        }
        $serialLog = Get-SpikePlainGuestItem $volume 'ProgramData\Motor\hyperv-agent.log'
        if ($serialLog) {
            Copy-Item -LiteralPath $serialLog.FullName -Destination (Join-Path $Directory 'hyperv-agent.log')
            [ordered]@{ present=$true; bytes=$serialLog.Length; path=$serialLog.FullName } | ConvertTo-Json | Set-Content (Join-Path $Directory 'agent-log-state.json') -Encoding UTF8
        } else {
            [ordered]@{ present=$false; reason='absent or reparse path' } | ConvertTo-Json | Set-Content (Join-Path $Directory 'agent-log-state.json') -Encoding UTF8
        }
        $files = @()
        $users = Get-SpikePlainGuestItem $volume 'Users'
        $profiles = if ($users) { @(Get-ChildItem -LiteralPath $users.FullName -Directory | Where-Object Name -Like 'motor-job*') } else { @() }
        foreach ($profile in $profiles) {
            $relative = "Users\$($profile.Name)\AppData\Local\Temp\motor-tree.tar"
            $item = Get-SpikePlainGuestItem $volume $relative
            if ($item) {
                $files += [ordered]@{ path=$item.FullName; bytes=$item.Length; modifiedUtc=$item.LastWriteTimeUtc.ToString('o'); sha256=(Get-FileHash -LiteralPath $item.FullName).Hash }
            } else {
                $files += [ordered]@{ path=(Join-Path $volume $relative); absentOrReparse=$true }
            }
        }
        foreach ($relative in @('m\spike','m\spike\check.ps1','m\spike\target','t')) {
            $path = Join-Path $volume $relative
            $item = Get-SpikePlainGuestItem $volume $relative
            if ($item) {
                $files += [ordered]@{ path=$path; attributes=[string]$item.Attributes; modifiedUtc=$item.LastWriteTimeUtc.ToString('o') }
            } else { $files += [ordered]@{ path=$path; absentOrReparse=$true } }
        }
        ConvertTo-Json -InputObject $files -Depth 5 | Set-Content (Join-Path $Directory 'staging-files.json') -Encoding UTF8
        foreach ($log in @('System','Application','Microsoft-Windows-Kernel-Boot%4Operational')) {
            $item = Get-SpikePlainGuestItem $volume "Windows\System32\winevt\Logs\$log.evtx"
            if (-not $item) { continue }
            $path = $item.FullName
            try {
                # Boot and service/error records distinguish an exited service
                # from a live agent waiting for bytes; retain XML if text is missing.
                $events = @(Get-WinEvent -Path $path -FilterXPath '*[System[(EventID=12 or EventID=13 or EventID=6005 or EventID=6006 or EventID=7000 or EventID=7009 or EventID=7011 or EventID=7022 or EventID=7023 or EventID=7024 or EventID=7031 or EventID=7034 or EventID=7036 or EventID=1000 or EventID=1001 or EventID=1023) or Provider[@Name="Microsoft-Windows-Kernel-Boot"]]]' -MaxEvents 100 | ForEach-Object {
                    [ordered]@{ time=$_.TimeCreated.ToUniversalTime().ToString('o'); provider=$_.ProviderName; id=$_.Id; message=$_.Message; xml=$_.ToXml() }
                })
                ConvertTo-Json -InputObject $events -Depth 6 | Set-Content (Join-Path $Directory "$log-events.json") -Encoding UTF8
            } catch {
                Set-Content (Join-Path $Directory "$log-events-error.txt") ($_ | Out-String)
            }
        }
    } -ReadOnly
    Write-Host "EVIDENCE stopped-guest-diagnostics $Directory (no job password or disk retained)"
}

# Copy only the agent's diagnostic log from a stopped overlay. Used on every
# hvsock run so both sides' byte counters are available, without the slower
# event-log export the failure harvest does.
function Save-SpikeAgentLog([string]$Disk, [string]$Directory) {
    New-Item $Directory -ItemType Directory -Force | Out-Null
    Use-SpikeWindowsDisk $Disk {
        param($volume)
        $log = Get-SpikePlainGuestItem $volume 'ProgramData\Motor\hyperv-agent.log'
        if ($log) { Copy-Item -LiteralPath $log.FullName -Destination (Join-Path $Directory 'hyperv-agent.log') }
        else { Write-Host 'EVIDENCE agent-log absent or reparse path' }
    } -ReadOnly
}

# Compare the host client's trace with the guest agent's counters. Prints the
# accounting JSON and returns its exit code; never throws, because it runs while
# a VM is being cleaned up. -1 means the guest log was not available.
function Invoke-SpikeByteAccounting([string]$Python, [string]$Batch, [int]$Run, [string]$HostTrace, [string]$GuestLog) {
    if (-not $Python -or -not (Test-Path -LiteralPath $HostTrace) -or -not (Test-Path -LiteralPath $GuestLog)) {
        Write-Host "FAIL byte-accounting-inputs run=$Run host=$HostTrace guest=$GuestLog"
        return -1
    }
    try {
        $result = Invoke-SpikeNative $Python @((Join-Path $PSScriptRoot 'account-serial.py'), '--host-trace', $HostTrace, '--guest-log', $GuestLog) $Batch "accounting-run-$Run"
        return $result.exitCode
    } catch {
        Write-Host "FAIL byte-accounting run=$Run $_"
        return -1
    }
}
