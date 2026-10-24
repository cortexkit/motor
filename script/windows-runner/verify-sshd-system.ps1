<# Runs the effective SSH policy check as SYSTEM so Win32-OpenSSH can query a local user's groups. #>
param(
    [Parameter(Mandatory)][string]$ConfigPath,
    [Parameter(Mandatory)][string]$ResultPath,
    [Parameter(Mandatory)][string]$NativeLogPath
)
$ErrorActionPreference = 'Stop'
Set-StrictMode -Version Latest
. "$PSScriptRoot\accounts.ps1"
$result = [ordered]@{success=$false; stdout=''; error=$null}
try {
    $native = Invoke-RunnerNative -FilePath "$env:SystemRoot\System32\OpenSSH\sshd.exe" `
        -ArgumentList @('-T', '-f', $ConfigPath, '-C', 'user=motor-ssh,host=localhost,addr=127.0.0.1') `
        -Operation 'verify restricted SSH policy as SYSTEM' -LogPath $NativeLogPath -TimeoutSeconds 30
    $result.stdout = $native.stdout
    $result.success = $true
} catch {
    $result.error = [string]$_
}
[IO.File]::WriteAllText($ResultPath, (ConvertTo-Json -InputObject $result -Depth 4 -Compress), [Text.UTF8Encoding]::new($false))
if (-not $result.success) { exit 1 }
exit 0
