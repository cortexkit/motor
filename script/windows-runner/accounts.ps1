# SID-based local policy edits. Preserve every unrelated trustee and assignment.

# secedit /export lists a local account by its name, not its SID, so a trustee
# is resolved to a SID before any comparison. Tests replace the name lookup.
$script:RunnerResolveTrusteeName = {
    param($name)
    ([Security.Principal.NTAccount]::new($name)).Translate([Security.Principal.SecurityIdentifier]).Value
}
function Resolve-RunnerTrusteeSid([string]$Trustee) {
    $Trustee = $Trustee.Trim()
    if ($Trustee -match '^\*?(S-\d+(?:-\d+)+)$') { return $Matches[1] }
    try { $sid = & $script:RunnerResolveTrusteeName $Trustee } catch { throw "Cannot resolve policy trustee '$Trustee': $_" }
    if ($sid -notmatch '^S-\d+(?:-\d+)+$') { throw "Cannot resolve policy trustee '$Trustee'" }
    $sid
}
function Remove-RunnerBatchRights([string]$Root, [string]$Sid) {
    if ($Sid -notmatch '^S-1-5-21-(\d+-){3}\d+$') { throw 'Expected the owned local account SID' }
    $policy = "$Root\rights.inf"
    $database = "$Root\rights.sdb"
    $rights = @('SeBatchLogonRight','SeDenyInteractiveLogonRight','SeDenyRemoteInteractiveLogonRight','SeDenyNetworkLogonRight','SeDenyServiceLogonRight')
    try {
        & secedit.exe /export /cfg $policy /areas USER_RIGHTS /quiet
        if ($LASTEXITCODE -ne 0) { throw 'Cannot export local user rights' }
        $lines = @(Get-Content -LiteralPath $policy)
        foreach ($right in $rights) {
            $pattern = '^' + $right + '\s*='
            $entry = @($lines | Where-Object { $_ -match $pattern })
            if ($entry.Count -gt 1) { throw "Duplicate assignment $right" }
            $grants = @()
            if (-not $entry.Count) { continue }
            $grants = @(($entry[0] -split '=',2)[1] -split ',' | ForEach-Object { $_.Trim() } | Where-Object { $_ -and (Resolve-RunnerTrusteeSid $_) -ne $Sid })
            $lines = @($lines | ForEach-Object { if ($_ -match $pattern) { "$right = " + ($grants -join ',') } else { $_ } })
        }
        $lines | Set-Content -LiteralPath $policy -Encoding Unicode
        & secedit.exe /configure /db $database /cfg $policy /areas USER_RIGHTS /quiet
        if ($LASTEXITCODE -ne 0) { throw 'Cannot remove obsolete local account rights' }
        & secedit.exe /export /cfg $policy /areas USER_RIGHTS /quiet
        if ($LASTEXITCODE -ne 0) { throw 'Cannot verify local user rights cleanup' }
        foreach ($right in $rights) {
            $line = @(Get-Content -LiteralPath $policy | Where-Object { $_ -match ('^' + $right + '\s*=') })
            $present = $line.Count -eq 1 -and @((($line[0] -split '=',2)[1] -split ',') | Where-Object { $_.Trim() -and (Resolve-RunnerTrusteeSid $_) -eq $Sid }).Count -eq 1
            if ($present) { throw "Effective $right still contains obsolete SID" }
        }
    } finally { Remove-Item $policy,$database -Force -ErrorAction SilentlyContinue }
}
function Remove-RunnerPrepAccount([string]$Root, [string]$Sid) {
    if ($Sid -notmatch '^S-1-5-21-(\d+-){3}\d+$') { throw 'Expected the owned motor-prep SID' }
    $account = Get-LocalUser motor-prep -ErrorAction SilentlyContinue
    if ($account -and $account.SID.Value -ne $Sid) { throw 'motor-prep SID changed; refusing legacy account cleanup' }
    Remove-RunnerBatchRights $Root $Sid
    $profiles = @(Get-CimInstance Win32_UserProfile | Where-Object SID -eq $Sid)
    if ($profiles.Count -gt 1) { throw 'Ambiguous motor-prep profile ownership' }
    if ($profiles.Count -eq 1) {
        $profile = $profiles[0]
        if ($profile.LocalPath -notmatch '^C:\\Users\\motor-prep(?:\.[^\\]+)?$' -or $profile.Loaded) {
            throw 'motor-prep profile path changed or is still loaded; preserving it'
        }
        $path = [string]$profile.LocalPath
        if (Test-Path -LiteralPath $path -and ((Get-Item -LiteralPath $path -Force).Attributes -band [IO.FileAttributes]::ReparsePoint)) {
            throw 'Reparse motor-prep profile refused'
        }
        $profile | Remove-CimInstance
        if (Test-Path -LiteralPath $path) { throw 'motor-prep profile cleanup did not complete' }
    }
    if (Test-Path -LiteralPath "$Root\prep-secret.dpapi") {
        Remove-Item -LiteralPath "$Root\prep-secret.dpapi" -Force
    }
    if ($account) { Remove-LocalUser motor-prep }
}
function Protect-RunnerPath([string]$Path) {
    & icacls.exe $Path /inheritance:r /grant:r '*S-1-5-18:(OI)(CI)F' '*S-1-5-32-544:(OI)(CI)F' /Q | Out-Null
    if ($LASTEXITCODE -ne 0) { throw "Cannot protect $Path" }
}
