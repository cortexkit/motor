# Runs as SYSTEM before tools when requested, or after setup, until updates settle.
# A clean shutdown is not success by itself: sealing also requires the finalized marker.
param([switch]$BeforeToolchain)

$ErrorActionPreference = 'Stop'
$phase = if ($BeforeToolchain) { 'os-update-first' } else { 'finalize' }
$task = if ($BeforeToolchain) { 'MotorUpdateFirst' } else { 'MotorFinalize' }
$log = "C:\build\provision\$phase.log"
$count = "C:\build\provision\$phase.count"
$complete = if ($BeforeToolchain) { 'C:\build\provision\os-updated' } else { 'C:\build\provision\finalized' }
$transcriptOpen = $false
Start-Transcript -Path $log -Append | Out-Null
$transcriptOpen = $true
try {
    if (Test-Path C:\build\provision\FAILED.txt) { throw 'Refusing to update an already failed image' }
    Remove-Item $complete -Force -ErrorAction SilentlyContinue
    $runs = 1 + [int](Get-Content $count -ErrorAction SilentlyContinue)
    Set-Content $count $runs
    "run=$runs at $((Get-Date).ToUniversalTime().ToString('o'))"
    Get-ItemProperty 'HKLM:\SOFTWARE\Microsoft\Windows NT\CurrentVersion' | Select-Object ProductName,CurrentBuildNumber,UBR | Format-List
    $session = New-Object -ComObject Microsoft.Update.Session
    $result = $session.CreateUpdateSearcher().Search("IsInstalled=0 and IsHidden=0 and Type='Software'")
    if ($result.ResultCode -ne 2) { throw "Update search failed: $($result.ResultCode)" }
    "found=$($result.Updates.Count)"
    $inventory = @($result.Updates | ForEach-Object {
        [ordered]@{ title=$_.Title; updateId=$_.Identity.UpdateID; revision=$_.Identity.RevisionNumber; kbArticles=@($_.KBArticleIDs) }
    })
    ConvertTo-Json -InputObject $inventory -Depth 5 | Set-Content "C:\build\provision\${phase}-updates-$runs.json" -Encoding UTF8
    $reboot = $false
    if ($result.Updates.Count -gt 0) {
        $updates = New-Object -ComObject Microsoft.Update.UpdateColl
        foreach ($update in $result.Updates) {
            $update.Title
            if (-not $update.EulaAccepted) { $update.AcceptEula() }
            [void]$updates.Add($update)
        }
        $downloader = $session.CreateUpdateDownloader(); $downloader.Updates = $updates
        $downloaded = $downloader.Download()
        if ($downloaded.ResultCode -ne 2) { throw "Update download failed: $($downloaded.ResultCode)" }
        $installer = $session.CreateUpdateInstaller(); $installer.Updates = $updates
        $installed = $installer.Install()
        "install=$($installed.ResultCode) reboot=$($installed.RebootRequired)"
        if ($installed.ResultCode -ne 2) { throw "Update installation failed: $($installed.ResultCode)" }
        for ($index = 0; $index -lt $updates.Count; $index++) {
            $item = $installed.GetUpdateResult($index)
            if ($item.ResultCode -ne 2 -or $item.HResult -ne 0) { throw "Update $index failed: $($item.HResult)" }
        }
        $reboot = $installed.RebootRequired
    }
    $pending = (New-Object -ComObject Microsoft.Update.SystemInfo).RebootRequired -or (Test-Path 'HKLM:\SOFTWARE\Microsoft\Windows\CurrentVersion\WindowsUpdate\Auto Update\RebootRequired')
    "pending=$pending"
    # Microsoft publishes Defender definition updates several times a day, so a
    # pass can always find a newer one. They install without a reboot and don't
    # change the OS, so they never keep the image from counting as settled.
    $definitionUpdates = 'e0789628-ce08-4437-be74-2495b842f43b'
    $osUpdates = @($result.Updates | Where-Object {
        -not @($_.Categories | Where-Object { $_.CategoryID -eq $definitionUpdates }).Count
    }).Count
    "os_updates=$osUpdates definition_updates=$($result.Updates.Count - $osUpdates)"
    if ($reboot -or $pending -or $osUpdates -gt 0) {
        if ($runs -ge 5) { throw "Updates did not settle within five $phase passes" }
        Stop-Transcript | Out-Null
        $transcriptOpen = $false
        Restart-Computer -Force
        exit
    }
    Unregister-ScheduledTask -TaskName $task -Confirm:$false
    Remove-Item $count
    "$phase complete: found=0 pending=False runs=$runs"
    Set-Content $complete (Get-Date -Format o)
    if ($BeforeToolchain) {
        # Only a settled, reboot-free OS may enter the modern runtime installer.
        # Use the copied scripts, not a CD letter or the temporary builder logon.
        Stop-Transcript | Out-Null
        $transcriptOpen = $false
        & C:\build\provision\bootstrap.ps1 -JobBaseline -ResumeAfterUpdates
        if (-not $? -or (Test-Path C:\build\provision\FAILED.txt)) { throw 'Post-update bootstrap failed' }
        return
    }
} catch {
    if (-not (Test-Path C:\build\provision\FAILED.txt)) { $_ | Out-String | Set-Content C:\build\provision\FAILED.txt }
    Unregister-ScheduledTask -TaskName $task -Confirm:$false -ErrorAction SilentlyContinue
    if ($transcriptOpen) { Stop-Transcript | Out-Null }
    Stop-Computer -Force
    exit 1
}
if ($transcriptOpen) { Stop-Transcript | Out-Null }
Stop-Computer -Force
