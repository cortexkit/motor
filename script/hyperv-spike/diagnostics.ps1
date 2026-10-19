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
        if ($serialLog) { Copy-Item -LiteralPath $serialLog.FullName -Destination (Join-Path $Directory 'hyperv-agent.log') }
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
