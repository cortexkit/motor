# Runs as SYSTEM after job-image setup and again at startup until updates settle.
# A clean shutdown is not success by itself: sealing also requires the finalized marker.
$ErrorActionPreference = 'Stop'
$log = 'C:\build\provision\finalize.log'
$count = 'C:\build\provision\finalize.count'
$complete = 'C:\build\provision\finalized'
Start-Transcript -Path $log -Append | Out-Null
try {
    Remove-Item $complete -Force -ErrorAction SilentlyContinue
    $runs = 1 + [int](Get-Content $count -ErrorAction SilentlyContinue)
    Set-Content $count $runs
    "run=$runs at $((Get-Date).ToUniversalTime().ToString('o'))"
    $session = New-Object -ComObject Microsoft.Update.Session
    $result = $session.CreateUpdateSearcher().Search("IsInstalled=0 and IsHidden=0 and Type='Software'")
    if ($result.ResultCode -ne 2) { throw "Update search failed: $($result.ResultCode)" }
    "found=$($result.Updates.Count)"
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
    if ($reboot -or $pending -or $result.Updates.Count -gt 0) {
        if ($runs -ge 5) { throw 'Updates did not settle within five finalization passes' }
        Stop-Transcript | Out-Null
        Restart-Computer -Force
        exit
    }
    Unregister-ScheduledTask -TaskName MotorFinalize -Confirm:$false
    Remove-Item $count
    "finalized: found=0 pending=False runs=$runs"
    Set-Content $complete (Get-Date -Format o)
} catch {
    $_ | Out-String | Set-Content C:\build\provision\FAILED.txt
    Unregister-ScheduledTask -TaskName MotorFinalize -Confirm:$false -ErrorAction SilentlyContinue
    Stop-Transcript | Out-Null
    Stop-Computer -Force
    exit 1
}
Stop-Transcript | Out-Null
Stop-Computer -Force
