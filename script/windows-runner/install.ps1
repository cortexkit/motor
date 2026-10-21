<# Install from a verified release bundle in elevated Windows PowerShell 5.1. #>
param(
    [Parameter(Mandatory)][string]$Bundle,
    [Parameter(Mandatory)][string]$ParentPath,
    [Parameter(Mandatory)][string]$VaultPublicKeyFile,
    [Parameter(Mandatory)][switch]$LicenseAcknowledged,
    [switch]$AdoptSpikeRegistration,
    [switch]$AdoptProvisioningRoot
)
$ErrorActionPreference = 'Stop'
Set-StrictMode -Version Latest
. "$PSScriptRoot\accounts.ps1"
$Root = 'C:\ProgramData\MotorRunner'
$identity = [Security.Principal.WindowsIdentity]::GetCurrent()
$admin = ([Security.Principal.WindowsPrincipal]$identity).IsInRole([Security.Principal.WindowsBuiltInRole]::Administrator)
if ($identity.User.Value -eq 'S-1-5-18') { throw 'Run install.ps1 from an elevated administrator shell, not a SYSTEM scheduled task; setting the parent owner requires that context' }
if (-not $admin -or -not $LicenseAcknowledged) { throw 'Elevation and an explicit image licence acknowledgement are required' }
if (Test-Path -LiteralPath $Root) { throw 'Root already exists; use the documented uninstall before reinstalling, preserving wanted evidence first' }
foreach ($name in @('motor-prep','motor-ssh')) { if (Get-LocalUser $name -ErrorAction SilentlyContinue) { throw "Refusing to adopt pre-existing account $name" } }
if (Get-Service MotorRunner -ErrorAction SilentlyContinue) { throw 'MotorRunner service already exists' }
$Bundle = (Resolve-Path -LiteralPath $Bundle).Path
$ParentPath = (Resolve-Path -LiteralPath $ParentPath).Path
if ($AdoptProvisioningRoot) {
    if ($ParentPath -cne 'C:\motor-spike\hyperv-ally\parent.vhdx' -or -not (Test-Path -LiteralPath 'C:\motor-spike\hyperv-ally\.hyperv-spike-root')) { throw 'Only the explicitly marked fresh provisioning root C:\motor-spike\hyperv-ally can be adopted for removal' }
}
if (-not (Get-Item -LiteralPath $ParentPath).IsReadOnly) { throw 'Parent is not sealed read-only' }
$keyLine = (Get-Content -LiteralPath $VaultPublicKeyFile -Raw).Trim()
if ($keyLine -notmatch '^ssh-ed25519 [A-Za-z0-9+/]+={0,2}(?: [^\r\n]+)?$') { throw 'Supply exactly one vault Ed25519 public key line, not a private key or authorized_keys options' }
$manifest = Get-Content -LiteralPath "$Bundle\manifest.json" -Raw | ConvertFrom-Json
foreach ($file in $manifest.files) {
    if ($file.path -match '(^|[/\\])\.\.($|[/\\])' -or [IO.Path]::IsPathRooted($file.path)) { throw 'Unsafe release manifest path' }
    if ((Get-FileHash -LiteralPath (Join-Path $Bundle $file.path) -Algorithm SHA256).Hash -ne $file.sha256) { throw "Release hash mismatch: $($file.path)" }
}
Import-Module Hyper-V
if (-not (Test-Path 'C:\Program Files\Git\cmd\git.exe')) { throw 'Machine-wide Git for Windows is required; installer does not bootstrap rustup or build anything on this host' }
if ((Get-Service sshd).Status -ne 'Running') { throw 'Install/configure Windows OpenSSH server first; installer only adds its owned Match block' }
New-Item $Root -ItemType Directory | Out-Null
Protect-RunnerPath $Root
Set-Content "$Root\.motor-runner-root" 'motor-runner-hyperv-v1' -Encoding ascii
$installation = @{ schema=1; root=$Root; accounts=@{}; enhanced_session_before=(Get-VMHost).EnableEnhancedSessionMode; created_utc=[DateTime]::UtcNow.ToString('o') }
function Save-Installation { $installation | ConvertTo-Json -Depth 6 | Set-Content "$Root\installation.json" -Encoding UTF8 }
Save-Installation
foreach ($directory in @('bin','baselines','vm\jobs','vm\caches','vm\cache-locks','staging','state\ally','evidence','service-home')) { New-Item "$Root\$directory" -ItemType Directory -Force | Out-Null }
Copy-Item "$Bundle\bin\*" "$Root\bin" -Recurse
Copy-Item "$Bundle\prep-toolchain" $Root -Recurse
foreach ($name in @('hyperv.ps1','parent-identity.ps1','guest-evidence.ps1','prep-acl.ps1','prep-access-check.ps1','accounts.ps1','qualify.ps1','qualify-peer.ps1','loader-snaps.ps1','uninstall.ps1','run-system.ps1','qualify-jobs.py')) { Copy-Item "$PSScriptRoot\$name" "$Root\bin" }
foreach ($name in @('common.ps1','process.ps1')) { Copy-Item "$PSScriptRoot\..\hyperv-spike\$name" "$Root\bin" }
$parent = "$Root\baselines\ally-hvsock.vhdx"
Copy-Item -LiteralPath $ParentPath -Destination $parent
(Get-Item -LiteralPath $parent).IsReadOnly = $true
& icacls.exe $parent /inheritance:r /grant:r '*S-1-5-18:R' '*S-1-5-32-544:R' '*S-1-5-83-0:R' '*S-1-15-3-1024-2268835264-3721307629-241982045-173645152-1490879176-104643441-2915960892-1612460704:R' /Q | Out-Null
if ($LASTEXITCODE -ne 0) { throw 'Cannot seal parent ACL for Hyper-V workers' }
& icacls.exe $parent /setowner '*S-1-5-32-544' /Q | Out-Null
if ($LASTEXITCODE -ne 0) { throw 'Cannot set trusted parent owner; run install.ps1 from an elevated administrator shell, not a SYSTEM scheduled task' }
$parentHash = (Get-FileHash -LiteralPath $parent -Algorithm SHA256).Hash
if ($AdoptProvisioningRoot) {
    $installation.provision_root = 'C:\motor-spike\hyperv-ally'
    $installation.provision_parent_sha256 = $parentHash
    Save-Installation
}
# Host passwords are independent of the guest's standard-job account.
function New-OwnedUser([string]$Name) {
    $bytes = New-Object byte[] 48
    $rng = [Security.Cryptography.RandomNumberGenerator]::Create()
    try { $rng.GetBytes($bytes) } finally { $rng.Dispose() }
    $secret = 'Aa1!' + (($bytes | ForEach-Object { $_.ToString('x2') }) -join '')
    New-LocalUser -Name $Name -Password (ConvertTo-SecureString $secret -AsPlainText -Force) -PasswordNeverExpires -UserMayNotChangePassword -AccountNeverExpires -Description 'Owned by MotorRunner; remove with its uninstall' | Out-Null
    $user = Get-LocalUser $Name
    $users = Get-LocalGroup -SID 'S-1-5-32-545'
    if (-not (Get-LocalGroupMember $users | Where-Object SID -eq $user.SID)) { Add-LocalGroupMember $users -Member $Name }
    $installation.accounts[$Name] = $user.SID.Value
    Save-Installation
    return $secret
}
$secret = New-OwnedUser 'motor-prep'
Set-RunnerBatchRights $Root $installation.accounts['motor-prep']
Add-Type -AssemblyName System.Security
[IO.File]::WriteAllBytes("$Root\prep-secret.dpapi", [Security.Cryptography.ProtectedData]::Protect([Text.Encoding]::ASCII.GetBytes($secret), $null, [Security.Cryptography.DataProtectionScope]::LocalMachine))
$secret = $null
$null = New-OwnedUser 'motor-ssh'
$sshSid = $installation.accounts['motor-ssh']
Set-Content "$Root\ssh-sid.txt" $sshSid -Encoding ascii
# Only the dedicated SSH entry can open the pipe. It can read the bridge binary,
# but the forced command cannot launch a shell or read service state/secrets.
& icacls.exe "$Root\bin" /grant:r "*${sshSid}:(OI)(CI)RX" /Q | Out-Null
if ($LASTEXITCODE -ne 0) { throw 'SSH bridge executable ACL failed' }
$profileKey = "HKLM:\SOFTWARE\Microsoft\Windows NT\CurrentVersion\ProfileList\$sshSid"
if (-not (Test-Path -LiteralPath $profileKey)) {
    Add-Type @'
using System;
using System.Runtime.InteropServices;
using System.Text;
public static class MotorSshProfile {
    [DllImport("userenv.dll", CharSet = CharSet.Unicode)]
    public static extern int CreateProfile(string sid, string user, StringBuilder path, uint length);
}
'@
    $buffer = New-Object Text.StringBuilder 260
    $result = [MotorSshProfile]::CreateProfile($sshSid, 'motor-ssh', $buffer, 260)
    if ($result -ne 0) { throw "Creating the dedicated SSH profile failed: $result" }
}
$sshHome = [Environment]::ExpandEnvironmentVariables((Get-ItemProperty -LiteralPath $profileKey).ProfileImagePath)
if ($sshHome -notmatch '^C:\\Users\\motor-ssh(?:\.[^\\]+)?$') { throw 'Unexpected SSH profile path; preserving it for review' }
$installation.ssh_home = $sshHome
Save-Installation
& icacls.exe $sshHome /inheritance:r /grant:r '*S-1-5-18:(OI)(CI)F' '*S-1-5-32-544:(OI)(CI)F' "*${sshSid}:(OI)(CI)RX" /Q | Out-Null
if ($LASTEXITCODE -ne 0) { throw 'SSH entry home ACL failed' }
& icacls.exe $sshHome /setowner '*S-1-5-18' /Q | Out-Null
if ($LASTEXITCODE -ne 0) { throw 'SSH profile ownership failed' }
if (Test-Path -LiteralPath "$sshHome\.ssh") { throw 'Unexpected SSH state in the newly created profile; preserving it for review' }
New-Item "$sshHome\.ssh" -ItemType Directory | Out-Null
[IO.File]::WriteAllText("$sshHome\.ssh\authorized_keys", $keyLine + "`r`n", [Text.UTF8Encoding]::new($false))
& icacls.exe "$sshHome\.ssh" /setowner '*S-1-5-18' /T /L /Q | Out-Null
if ($LASTEXITCODE -ne 0) { throw 'SSH authorized-key ownership failed' }
$sshdConfig = 'C:\ProgramData\ssh\sshd_config'
$before = [IO.File]::ReadAllText($sshdConfig)
if ($before -match '(?m)^# BEGIN MOTOR RUNNER') { throw 'Existing Motor SSH block requires removal first' }
[IO.File]::WriteAllText("$Root\sshd_config.before", $before, [Text.UTF8Encoding]::new($false))
$block = @'
# BEGIN MOTOR RUNNER
Match User motor-ssh
    AuthenticationMethods publickey
    PasswordAuthentication no
    PubkeyAuthentication yes
    AuthorizedKeysFile .ssh/authorized_keys
    AllowTcpForwarding no
    AllowAgentForwarding no
    X11Forwarding no
    PermitTTY no
    ForceCommand C:/ProgramData/MotorRunner/bin/motor-runner.exe connect --instance ally --state-root C:/ProgramData/MotorRunner/state
Match all
# END MOTOR RUNNER
'@
[IO.File]::WriteAllText($sshdConfig, $before.TrimEnd() + "`r`n" + $block + "`r`n", [Text.UTF8Encoding]::new($false))
& 'C:\Windows\System32\OpenSSH\sshd.exe' -t -f $sshdConfig
if ($LASTEXITCODE -ne 0) { [IO.File]::WriteAllText($sshdConfig, $before); throw 'OpenSSH rejected the restricted entry configuration; original restored' }
$effective = & 'C:\Windows\System32\OpenSSH\sshd.exe' -T -f $sshdConfig -C 'user=motor-ssh,host=localhost,addr=127.0.0.1'
if ($LASTEXITCODE -ne 0) { throw 'Cannot verify the effective restricted SSH policy' }
$policy = $effective -join "`n"
foreach ($rule in @('(?m)^authenticationmethods publickey$','(?m)^passwordauthentication no$','(?m)^allowtcpforwarding no$','(?m)^allowagentforwarding no$','(?m)^permittty no$','(?m)^forcecommand C:/ProgramData/MotorRunner/bin/motor-runner.exe connect --instance ally --state-root C:/ProgramData/MotorRunner/state$')) {
    if ($policy -notmatch $rule) { throw "Effective SSH restriction missing: $rule" }
}
$effective | Set-Content "$Root\evidence\sshd-effective-motor-ssh.txt" -Encoding ascii
$installation.ssh_block = $true
Save-Installation
Restart-Service sshd
$keyPath = 'HKLM:\SOFTWARE\Microsoft\Windows NT\CurrentVersion\Virtualization\GuestCommunicationServices\92c268b0-cddd-40a4-a664-f32e50ad35ea'
if (Test-Path -LiteralPath $keyPath) {
    $existing = Get-ItemProperty -LiteralPath $keyPath
    if (-not $AdoptSpikeRegistration -or -not $existing.PSObject.Properties['MotorOwner'] -or $existing.MotorOwner -ne 'motor-hyperv-spike') { throw 'Existing hvsock registration not adopted; use -AdoptSpikeRegistration only for the owned spike key' }
}
New-Item $keyPath -Force | Out-Null
New-ItemProperty $keyPath -Name ElementName -Value 'Motor Windows runner' -PropertyType String -Force | Out-Null
New-ItemProperty $keyPath -Name MotorOwner -Value 'motor-runner-hyperv' -PropertyType String -Force | Out-Null
New-ItemProperty $keyPath -Name MotorRoot -Value $Root -PropertyType String -Force | Out-Null
$installation.hvsock_owned = $true
Set-VMHost -EnableEnhancedSessionMode $false
$power = & powercfg.exe /getactivescheme
if ($LASTEXITCODE -ne 0) { throw 'Cannot read active power scheme' }
$scheme = [regex]::Match(($power -join ' '), '[0-9a-fA-F]{8}-(?:[0-9a-fA-F]{4}-){3}[0-9a-fA-F]{12}').Value
if (-not $scheme) { throw 'Cannot identify active power scheme' }
$settings = @('29f6c1db-86da-48c5-9fdb-f2b67b1f44da','9d7815a6-7ee4-497e-8888-515a05f02364')
$installation.power = @{scheme=$scheme; ac=@{}}
foreach ($setting in $settings) {
    $values = & powercfg.exe /query $scheme '238c9fa8-0aad-41ed-83f4-97be242c8f20' $setting
    if ($LASTEXITCODE -ne 0) { throw 'Cannot read AC sleep/hibernate timeout' }
    $indexes = [regex]::Matches(($values -join "`n"), '0x[0-9a-fA-F]+')
    if ($indexes.Count -lt 2) { throw 'Cannot parse AC/DC power indexes' }
    $installation.power.ac[$setting] = [Convert]::ToUInt32($indexes[$indexes.Count-2].Value.Substring(2),16)
    Save-Installation
    & powercfg.exe /setacvalueindex $scheme '238c9fa8-0aad-41ed-83f4-97be242c8f20' $setting 0
    if ($LASTEXITCODE -ne 0) { throw 'Cannot disable AC sleep/hibernate timeout' }
}
& powercfg.exe /setactive $scheme
if ($LASTEXITCODE -ne 0) { throw 'Cannot apply power policy' }
Save-Installation
$cargoHash = (Get-FileHash "$Root\prep-toolchain\bin\cargo.exe" -Algorithm SHA256).Hash
@"
layout_root = 'C:\ProgramData\MotorRunner'
baseline = 'ally-hvsock.vhdx'
parent_sha256 = '$parentHash'
prep_cargo_sha256 = '$cargoHash'
memory_mib = 8192
cpu_set = [0, 1, 2, 3]
concurrent_jobs = 1
toolchain_version = 'rustc 1.99.0 (Windows MSVC)'
"@ | Set-Content "$Root\vm.toml" -Encoding ascii
$binary = '"C:\ProgramData\MotorRunner\bin\motor-runner.exe" service --instance ally --state-root C:\ProgramData\MotorRunner\state --backend windows-vm --vm-config C:\ProgramData\MotorRunner\vm.toml'
New-Service -Name MotorRunner -BinaryPathName $binary -DisplayName 'Motor offline Hyper-V runner' -StartupType Automatic -DependsOn vmms | Out-Null
# The bridge authenticates the pipe against SCM's registered process ID. Grant
# only SERVICE_QUERY_STATUS to its SID, never service control/configuration.
$sddl = (& sc.exe sdshow MotorRunner | Where-Object { $_ -match '^[DOGS]:' }) -join ''
if ($LASTEXITCODE -ne 0 -or -not $sddl) { throw 'Cannot read runner service security descriptor' }
$descriptor = [Security.AccessControl.RawSecurityDescriptor]::new($sddl)
if ($null -eq $descriptor.DiscretionaryAcl) { throw 'Runner service has an unexpected null DACL' }
$ace = [Security.AccessControl.CommonAce]::new([Security.AccessControl.AceFlags]::None, [Security.AccessControl.AceQualifier]::AccessAllowed, 4, [Security.Principal.SecurityIdentifier]::new($sshSid), $false, $null)
$descriptor.DiscretionaryAcl.InsertAce($descriptor.DiscretionaryAcl.Count, $ace)
$updatedSddl = $descriptor.GetSddlForm([Security.AccessControl.AccessControlSections]::All)
& sc.exe sdset MotorRunner $updatedSddl | Out-Host
if ($LASTEXITCODE -ne 0) { throw 'Cannot grant bridge access to runner service identity' }
New-ItemProperty 'HKLM:\SYSTEM\CurrentControlSet\Services\MotorRunner' -Name Environment -PropertyType MultiString -Value @('HOME=C:\ProgramData\MotorRunner\service-home','PATH=C:\Windows\System32;C:\Windows;C:\Program Files\Git\cmd;C:\Program Files\Git\bin') -Force | Out-Null
& sc.exe failure MotorRunner reset= 86400 actions= restart/5000/restart/15000/restart/60000 | Out-Host
if ($LASTEXITCODE -ne 0) { throw 'Cannot configure runner service recovery' }
& sc.exe failureflag MotorRunner 1 | Out-Host
if ($LASTEXITCODE -ne 0) { throw 'Cannot enable recovery after a reported service failure' }
Start-Service MotorRunner
Write-Host "EVIDENCE sealed-parent SHA256=$parentHash path=$parent"
Write-Host 'Installed with one slot, AC-only admission. Fleet routing remains disabled until qualification evidence is reviewed.'
Get-Content 'C:\ProgramData\ssh\ssh_host_ed25519_key.pub'
Write-Host 'Use the host public key line above as the pinned Ally sshd key; the dedicated SSH username is motor-ssh.'
