$ErrorActionPreference = 'Stop'
$log = 'C:\build\provision\finalize.log'
$count = 'C:\build\provision\finalize.count'
Start-Transcript -Path $log -Append | Out-Null
$runs = 1 + [int](Get-Content $count -ErrorAction SilentlyContinue)
Set-Content $count $runs
"run=$runs at $((Get-Date).ToUniversalTime().ToString('o'))"
$session = New-Object -ComObject Microsoft.Update.Session
$result = $session.CreateUpdateSearcher().Search("IsInstalled=0 and IsHidden=0 and Type='Software'")
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
    "download=$($downloader.Download().ResultCode)"
    $installer = $session.CreateUpdateInstaller(); $installer.Updates = $updates
    $installed = $installer.Install()
    "install=$($installed.ResultCode) reboot=$($installed.RebootRequired)"
    $reboot = $installed.RebootRequired
}
$pending = Test-Path 'HKLM:\SOFTWARE\Microsoft\Windows\CurrentVersion\WindowsUpdate\Auto Update\RebootRequired'
"pending=$pending"
if (($reboot -or $pending -or $result.Updates.Count -gt 0) -and $runs -lt 5) {
    Stop-Transcript | Out-Null
    Restart-Computer -Force
    exit
}
Unregister-ScheduledTask -TaskName MotorFinalize -Confirm:$false
Remove-Item $count
"finalized: found=$($result.Updates.Count) pending=$pending runs=$runs"
Stop-Transcript | Out-Null
Stop-Computer -Force
