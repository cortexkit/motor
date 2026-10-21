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
function Set-RunnerBatchRights([string]$Root, [string]$Sid, [switch]$Remove) {
    if ($Sid -notmatch '^S-1-5-21-(\d+-){3}\d+$') { throw 'Expected a local account SID' }
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
            if ($entry.Count) { $grants = @((($entry[0] -split '=',2)[1] -split ',') | ForEach-Object { $_.Trim() } | Where-Object { $_ -and (Resolve-RunnerTrusteeSid $_) -ne $Sid }) }
            if (-not $Remove) { $grants += "*$Sid" }
            if ($entry.Count) {
                $lines = @($lines | ForEach-Object { if ($_ -match $pattern) { "$right = " + ($grants -join ',') } else { $_ } })
            } elseif (-not $Remove) {
                $lines = @($lines | ForEach-Object { $_; if ($_ -eq '[Privilege Rights]') { "$right = *$Sid" } })
            }
        }
        $lines | Set-Content -LiteralPath $policy -Encoding Unicode
        & secedit.exe /configure /db $database /cfg $policy /areas USER_RIGHTS /quiet
        if ($LASTEXITCODE -ne 0) { throw 'Cannot configure local user rights' }
        & secedit.exe /export /cfg $policy /areas USER_RIGHTS /quiet
        if ($LASTEXITCODE -ne 0) { throw 'Cannot verify local user rights' }
        foreach ($right in $rights) {
            $line = @(Get-Content -LiteralPath $policy | Where-Object { $_ -match ('^' + $right + '\s*=') })
            $present = $line.Count -eq 1 -and @((($line[0] -split '=',2)[1] -split ',') | Where-Object { $_.Trim() -and (Resolve-RunnerTrusteeSid $_) -eq $Sid }).Count -eq 1
            if ($present -eq [bool]$Remove) { throw "Effective $right does not match requested SID policy" }
        }
    } finally { Remove-Item $policy,$database -Force -ErrorAction SilentlyContinue }
}
function Protect-RunnerPath([string]$Path) {
    & icacls.exe $Path /inheritance:r /grant:r '*S-1-5-18:(OI)(CI)F' '*S-1-5-32-544:(OI)(CI)F' /Q | Out-Null
    if ($LASTEXITCODE -ne 0) { throw "Cannot protect $Path" }
}
