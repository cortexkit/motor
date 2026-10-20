$ErrorActionPreference = 'Stop'
function Assert-MotorIsolation($Evidence) {
    if ($Evidence.user -notmatch '\\motor-job$' -or $Evidence.admin -or $Evidence.groups -contains 'S-1-5-32-544') { throw 'FAIL standard-account' }
    Write-Output 'PASS standard-account'
    if ($Evidence.adapters.Count -ne 0 -or $Evidence.routes.Count -ne 0) { throw 'FAIL no-network-route' }
    Write-Output 'PASS no-network-route'
    if ($Evidence.localCanary -or $Evidence.uncCanary -or $Evidence.secretReadable) { throw 'FAIL host-files-inaccessible' }
    Write-Output 'PASS host-files-inaccessible'
}
$id = [Security.Principal.WindowsIdentity]::GetCurrent()
$principal = [Security.Principal.WindowsPrincipal]::new($id)
$admin = $principal.IsInRole([Security.Principal.WindowsBuiltInRole]::Administrator)
$groups = @($id.Groups | ForEach-Object { $_.Value })
$adapters = @(Get-NetAdapter -IncludeHidden | Where-Object { $_.Status -eq 'Up' })
$routes = @(Get-NetRoute | Where-Object { $_.InterfaceAlias -notlike '*Loopback*' })
$local = Test-Path -LiteralPath __CANARY__
$unc = Test-Path -LiteralPath __UNC__ -ErrorAction SilentlyContinue
$secretReadable = $false
try { [void][IO.File]::ReadAllText('C:\ProgramData\Motor\job-password.txt'); $secretReadable = $true } catch [UnauthorizedAccessException] {}
$evidence = [ordered]@{ user=$id.Name; admin=$admin; groups=$groups; adapters=@($adapters | Select-Object Name,InterfaceDescription,Status); routes=@($routes | Select-Object DestinationPrefix,InterfaceAlias,NextHop); localCanary=$local; uncCanary=$unc; secretReadable=$secretReadable }
Write-Output ('EVIDENCE ' + ($evidence | ConvertTo-Json -Depth 6 -Compress))
& "$env:SystemRoot\System32\whoami.exe" /all
if ($LASTEXITCODE -ne 0) { throw 'whoami failed' }
Assert-MotorIsolation $evidence
Write-Output ('PASS trivial-command ' + (2 + 2))
