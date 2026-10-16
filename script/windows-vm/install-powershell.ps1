# Install the pinned ZIP while provisioning the job image as an administrator.
# The job account may execute the tree, but only SYSTEM/Administrators can change it.
param([string]$Archive = "$PSScriptRoot\PowerShell-7.6.7-win-x64.zip")

$ErrorActionPreference = 'Stop'
if (-not ([Security.Principal.WindowsPrincipal] [Security.Principal.WindowsIdentity]::GetCurrent()).IsInRole([Security.Principal.WindowsBuiltInRole]::Administrator)) {
    throw 'PowerShell installation must run elevated'
}
$expected = 'd2f952ebdbf24386cb8923553aa58e41a846db35a6155ea04c58142a4b16dbb7'
if ((Get-FileHash -LiteralPath $Archive -Algorithm SHA256).Hash.ToLowerInvariant() -cne $expected) {
    throw 'PowerShell archive SHA-256 mismatch'
}
$destination = 'C:\Program Files\PowerShell\7'
if (Test-Path -LiteralPath $destination) { throw 'PowerShell destination already exists; use a fresh provisioning image' }
New-Item $destination -ItemType Directory | Out-Null
# Protect the root first; descendants then inherit the same read/execute policy.
& icacls $destination /inheritance:r /grant:r '*S-1-5-18:(OI)(CI)F' '*S-1-5-32-544:(OI)(CI)F' '*S-1-5-32-545:(OI)(CI)RX'
if ($LASTEXITCODE -ne 0) { throw 'PowerShell root ACL setup failed' }
Expand-Archive -LiteralPath $Archive -DestinationPath $destination
& icacls "$destination\*" /reset /T /C /L /Q
if ($LASTEXITCODE -ne 0) { throw 'PowerShell ACL inheritance reset failed' }
& icacls $destination /setowner '*S-1-5-32-544' /T /C /L /Q
if ($LASTEXITCODE -ne 0) { throw 'PowerShell ownership setup failed' }
$items = @((Get-Item -LiteralPath $destination)) + @(Get-ChildItem -LiteralPath $destination -Recurse -Force)
foreach ($item in $items) {
    $rules = (Get-Acl -LiteralPath $item.FullName).GetAccessRules($true, $true, [Security.Principal.SecurityIdentifier])
    $seen = @{}
    foreach ($rule in $rules) {
        $sid = $rule.IdentityReference.Value
        $rights = switch ($sid) {
            'S-1-5-18' { [Security.AccessControl.FileSystemRights]::FullControl }
            'S-1-5-32-544' { [Security.AccessControl.FileSystemRights]::FullControl }
            'S-1-5-32-545' { [Security.AccessControl.FileSystemRights]::ReadAndExecute -bor [Security.AccessControl.FileSystemRights]::Synchronize }
            default { throw "Unexpected PowerShell ACL principal $sid on $($item.FullName)" }
        }
        if ($rule.AccessControlType -ne [Security.AccessControl.AccessControlType]::Allow -or $rule.FileSystemRights -ne $rights) {
            throw "Unexpected PowerShell access rule on $($item.FullName): $rule"
        }
        $seen[$sid] = $true
    }
    if ($seen.Count -ne 3) { throw "Incomplete PowerShell ACL on $($item.FullName)" }
}
$version = & "$destination\pwsh.exe" -NoProfile -Command '$PSVersionTable.PSVersion.ToString()'
if ($LASTEXITCODE -ne 0 -or $version -cne '7.6.7') { throw "Unexpected PowerShell version: $version" }
# Native tools may rebuild PATH from the registry instead of inheriting the agent's PATH.
$machinePath = [Environment]::GetEnvironmentVariable('Path', 'Machine')
if (($machinePath -split ';' | ForEach-Object { $_.TrimEnd('\') }) -notcontains $destination) {
    [Environment]::SetEnvironmentVariable('Path', "$destination;$machinePath", 'Machine')
}
$env:Path = "$destination;" + $env:Path
Write-Host "Installed PowerShell $version with protected job-user read/execute access"
