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
. "$PSScriptRoot\guest-evidence.ps1"
. "$PSScriptRoot\guest-agent-identity.ps1"
$Root = 'C:\ProgramData\MotorRunner'
$nativeLogPath = Join-Path $Root 'installation-native.jsonl'
$identity = [Security.Principal.WindowsIdentity]::GetCurrent()
$admin = ([Security.Principal.WindowsPrincipal]$identity).IsInRole([Security.Principal.WindowsBuiltInRole]::Administrator)
if ($identity.User.Value -eq 'S-1-5-18') { throw 'Run install.ps1 from an elevated administrator shell, not a SYSTEM scheduled task; setting the parent owner requires that context' }
if (-not $admin -or -not $LicenseAcknowledged) { throw 'Elevation and an explicit image licence acknowledgement are required' }
if (Test-Path -LiteralPath $Root) { throw 'Root already exists; use the documented uninstall before reinstalling, preserving wanted evidence first' }
foreach ($name in @('motor-prep','motor-ssh')) { if (Get-LocalUser $name -ErrorAction SilentlyContinue) { throw "Refusing to adopt pre-existing account $name; run the owned uninstaller first" } }
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
Protect-RunnerPath $Root $nativeLogPath
Set-Content "$Root\.motor-runner-root" 'motor-runner-hyperv-v1' -Encoding ascii
$installation = @{ schema=1; root=$Root; accounts=@{}; enhanced_session_before=(Get-VMHost).EnableEnhancedSessionMode; created_utc=[DateTime]::UtcNow.ToString('o') }
function Save-Installation { $installation | ConvertTo-Json -Depth 6 | Set-Content "$Root\installation.json" -Encoding UTF8 }
Save-Installation
foreach ($directory in @('bin','baselines','vm\jobs','vm\caches','vm\cache-locks','staging','state\ally','state\ally\local-registry-cache','evidence','service-home')) { New-Item "$Root\$directory" -ItemType Directory -Force | Out-Null }
# `state` is created implicitly as the parent of state\ally, so it gets its
# own protection: the runner checks its owner and ACL like the others.
Protect-RunnerPath "$Root\state" $nativeLogPath
Protect-RunnerPath "$Root\state\ally" $nativeLogPath
Protect-RunnerPath "$Root\state\ally\local-registry-cache" $nativeLogPath
Protect-RunnerPath "$Root\staging" $nativeLogPath
Copy-Item "$Bundle\bin\*" "$Root\bin" -Recurse
Copy-Item "$Bundle\prep-toolchain" $Root -Recurse
foreach ($name in @('hyperv.ps1','parent-identity.ps1','guest-evidence.ps1','accounts.ps1','qualify.ps1','qualify-peer.ps1','uninstall.ps1','run-system.ps1','verify-sshd-system.ps1','qualify-jobs.py')) { Copy-Item "$PSScriptRoot\$name" "$Root\bin" }
foreach ($name in @('common.ps1','process.ps1')) { Copy-Item "$PSScriptRoot\..\hyperv-spike\$name" "$Root\bin" }
$parent = "$Root\baselines\ally-hvsock.vhdx"
Copy-Item -LiteralPath $ParentPath -Destination $parent
(Get-Item -LiteralPath $parent).IsReadOnly = $true
$null = Invoke-RunnerNative -FilePath "$env:SystemRoot\System32\icacls.exe" -ArgumentList @($parent, '/inheritance:r', '/grant:r', '*S-1-5-18:R', '*S-1-5-32-544:R', '*S-1-5-83-0:R', '*S-1-15-3-1024-2268835264-3721307629-241982045-173645152-1490879176-104643441-2915960892-1612460704:R', '/Q') -Operation 'seal parent ACL' -LogPath $nativeLogPath
try {
    $null = Invoke-RunnerNative -FilePath "$env:SystemRoot\System32\icacls.exe" -ArgumentList @($parent, '/setowner', '*S-1-5-32-544', '/Q') -Operation 'set parent owner' -LogPath $nativeLogPath
} catch {
    throw "Cannot set trusted parent owner; run install.ps1 from an elevated administrator shell, not a SYSTEM scheduled task: $_"
}
$parentHash = (Get-FileHash -LiteralPath $parent -Algorithm SHA256).Hash
if ($AdoptProvisioningRoot) {
    $installation.provision_root = 'C:\motor-spike\hyperv-ally'
    $installation.provision_parent_sha256 = $parentHash
    Save-Installation
}
# The SSH bridge is the only runner-owned local account. Cargo preparation runs as SYSTEM.
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

function Invoke-RunnerSshPolicyAsSystem([string]$ConfigPath) {
    $operation = 'verify restricted SSH policy as SYSTEM'
    $resultPath = Join-Path "$Root\evidence" ("sshd-policy-system-" + [Guid]::NewGuid().ToString('N') + '.json')
    $taskName = 'MotorRunnerSshPolicy-' + [Guid]::NewGuid().ToString('N')
    $helperPath = Join-Path "$Root\bin" 'verify-sshd-system.ps1'
    $powershell = "$env:SystemRoot\System32\WindowsPowerShell\v1.0\powershell.exe"
    $taskArguments = @('-NoProfile','-NonInteractive','-ExecutionPolicy','Bypass','-File',$helperPath,'-ConfigPath',$ConfigPath,'-ResultPath',$resultPath,'-NativeLogPath',$nativeLogPath)
    $arguments = [string]::Join(' ', [string[]]@($taskArguments | ForEach-Object { ConvertTo-RunnerNativeArgument $_ }))
    $action = New-ScheduledTaskAction -Execute $powershell -Argument $arguments
    $settings = New-ScheduledTaskSettingsSet -ExecutionTimeLimit (New-TimeSpan -Seconds 60) -AllowStartIfOnBatteries -DontStopIfGoingOnBatteries
    $registered = $false
    try {
        Register-ScheduledTask -TaskName $taskName -Action $action -Settings $settings -User 'SYSTEM' -RunLevel Highest -Force | Out-Null
        $registered = $true
        Start-ScheduledTask -TaskName $taskName
        $deadline = [DateTime]::UtcNow.AddSeconds(65)
        while (-not (Test-Path -LiteralPath $resultPath)) {
            if ([DateTime]::UtcNow -ge $deadline) {
                try { Stop-ScheduledTask -TaskName $taskName -ErrorAction SilentlyContinue } catch { }
                throw "$operation timed out after 65 seconds; see $nativeLogPath"
            }
            Start-Sleep -Milliseconds 250
        }
        $result = Get-Content -LiteralPath $resultPath -Raw | ConvertFrom-Json
        if (-not $result.success) { throw "$operation failed: $($result.error); see $nativeLogPath" }
        return [string]$result.stdout
    } finally {
        if ($registered) { Unregister-ScheduledTask -TaskName $taskName -Confirm:$false -ErrorAction SilentlyContinue }
    }
}

$null = New-OwnedUser 'motor-ssh'
$sshSid = $installation.accounts['motor-ssh']
$resolvedSshSid = Resolve-RunnerTrusteeSid 'motor-ssh'
$localResolvedSshSid = Resolve-RunnerTrusteeSid "$env:COMPUTERNAME\motor-ssh"
if ($resolvedSshSid -cne $sshSid -or $localResolvedSshSid -cne $sshSid) { throw "motor-ssh name resolution returned short=$resolvedSshSid local=$localResolvedSshSid instead of $sshSid; SSH configuration was not changed" }
Set-Content "$Root\ssh-sid.txt" $sshSid -Encoding ascii
# Only the dedicated SSH entry can open the pipe. It can read the bridge binary,
# but the forced command cannot launch a shell or read service state/secrets.
$null = Invoke-RunnerNative -FilePath "$env:SystemRoot\System32\icacls.exe" -ArgumentList @("$Root\bin", '/grant:r', "*${sshSid}:(OI)(CI)RX", '/Q') -Operation 'grant bridge binary read access' -LogPath $nativeLogPath
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
$null = Invoke-RunnerNative -FilePath "$env:SystemRoot\System32\icacls.exe" -ArgumentList @($sshHome, '/inheritance:r', '/grant:r', '*S-1-5-18:(OI)(CI)F', '*S-1-5-32-544:(OI)(CI)F', "*${sshSid}:(OI)(CI)RX", '/Q') -Operation 'protect SSH home' -LogPath $nativeLogPath
$null = Invoke-RunnerNative -FilePath "$env:SystemRoot\System32\icacls.exe" -ArgumentList @($sshHome, '/setowner', '*S-1-5-18', '/Q') -Operation 'set SSH home owner' -LogPath $nativeLogPath
if (Test-Path -LiteralPath "$sshHome\.ssh") { throw 'Unexpected SSH state in the newly created profile; preserving it for review' }
New-Item "$sshHome\.ssh" -ItemType Directory | Out-Null
[IO.File]::WriteAllText("$sshHome\.ssh\authorized_keys", $keyLine + "`r`n", [Text.UTF8Encoding]::new($false))
$null = Invoke-RunnerNative -FilePath "$env:SystemRoot\System32\icacls.exe" -ArgumentList @("$sshHome\.ssh", '/setowner', '*S-1-5-18', '/T', '/L', '/Q') -Operation 'set SSH key directory owner' -LogPath $nativeLogPath
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
try {
    $null = Invoke-RunnerNative -FilePath "$env:SystemRoot\System32\OpenSSH\sshd.exe" -ArgumentList @('-t', '-f', $sshdConfig) -Operation 'validate SSH configuration' -LogPath $nativeLogPath
} catch {
    [IO.File]::WriteAllText($sshdConfig, $before, [Text.UTF8Encoding]::new($false))
    throw "OpenSSH rejected the restricted entry configuration; original restored: $_"
}
try {
    $effectiveOutput = Invoke-RunnerSshPolicyAsSystem $sshdConfig
    $effective = @($effectiveOutput -split '\r?\n' | Where-Object { $_ })
    $policy = $effective -join "`n"
    foreach ($rule in @('(?m)^authenticationmethods publickey$','(?m)^passwordauthentication no$','(?m)^allowtcpforwarding no$','(?m)^allowagentforwarding no$','(?m)^permittty no$','(?m)^forcecommand C:/ProgramData/MotorRunner/bin/motor-runner.exe connect --instance ally --state-root C:/ProgramData/MotorRunner/state$')) {
        if ($policy -notmatch $rule) { throw "Effective SSH restriction missing: $rule" }
    }
    $effective | Set-Content "$Root\evidence\sshd-effective-motor-ssh.txt" -Encoding ascii
} catch {
    [IO.File]::WriteAllText($sshdConfig, $before, [Text.UTF8Encoding]::new($false))
    throw "OpenSSH did not accept the restricted motor-ssh context; original config restored: $_"
}
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
$powerResult = Invoke-RunnerNative -FilePath "$env:SystemRoot\System32\powercfg.exe" -ArgumentList @('/getactivescheme') -Operation 'read active power scheme' -LogPath $nativeLogPath
$power = $powerResult.stdout -split '\r?\n'
$scheme = [regex]::Match(($power -join ' '), '[0-9a-fA-F]{8}-(?:[0-9a-fA-F]{4}-){3}[0-9a-fA-F]{12}').Value
if (-not $scheme) { throw 'Cannot identify active power scheme' }
$settings = @('29f6c1db-86da-48c5-9fdb-f2b67b1f44da','9d7815a6-7ee4-497e-8888-515a05f02364')
$installation.power = @{scheme=$scheme; ac=@{}}
foreach ($setting in $settings) {
    $queryResult = Invoke-RunnerNative -FilePath "$env:SystemRoot\System32\powercfg.exe" -ArgumentList @('/query', $scheme, '238c9fa8-0aad-41ed-83f4-97be242c8f20', $setting) -Operation "read power setting $setting" -LogPath $nativeLogPath
    $values = $queryResult.stdout -split '\r?\n'
    $indexes = [regex]::Matches(($values -join "`n"), '0x[0-9a-fA-F]+')
    if ($indexes.Count -lt 2) { throw 'Cannot parse AC/DC power indexes' }
    $installation.power.ac[$setting] = [Convert]::ToUInt32($indexes[$indexes.Count-2].Value.Substring(2),16)
    Save-Installation
    $null = Invoke-RunnerNative -FilePath "$env:SystemRoot\System32\powercfg.exe" -ArgumentList @('/setacvalueindex', $scheme, '238c9fa8-0aad-41ed-83f4-97be242c8f20', $setting, '0') -Operation "disable AC sleep for $setting" -LogPath $nativeLogPath
}
$null = Invoke-RunnerNative -FilePath "$env:SystemRoot\System32\powercfg.exe" -ArgumentList @('/setactive', $scheme) -Operation 'apply power policy' -LogPath $nativeLogPath
Save-Installation
$cargoHash = (Get-FileHash "$Root\prep-toolchain\bin\cargo.exe" -Algorithm SHA256).Hash
$parentAgentHash = ''
try {
    $sealedAgentHash = Get-RunnerSealedAgentHash $parent
    if ($sealedAgentHash) {
        $parentAgentHash = $sealedAgentHash
    } else {
        Write-Warning 'Sealed parent has no valid provisioning agent identity; Windows jobs will remain not-ready'
    }
} catch {
    Write-Warning "Cannot read sealed guest-agent provisioning record; Windows jobs will remain not-ready: $_"
}
@"
layout_root = 'C:\ProgramData\MotorRunner'
baseline = 'ally-hvsock.vhdx'
parent_sha256 = '$parentHash'
prep_cargo_sha256 = '$cargoHash'
guest_agent_sha256 = '$parentAgentHash'
memory_mib = 8192
cpu_set = [0, 1, 2, 3]
concurrent_jobs = 1
toolchain_version = 'rustc 1.99.0 (Windows MSVC)'
"@ | Set-Content "$Root\vm.toml" -Encoding ascii
$binary = '"C:\ProgramData\MotorRunner\bin\motor-runner.exe" service --instance ally --state-root C:\ProgramData\MotorRunner\state --backend windows-vm --vm-config C:\ProgramData\MotorRunner\vm.toml'
New-Service -Name MotorRunner -BinaryPathName $binary -DisplayName 'Motor offline Hyper-V runner' -StartupType Automatic -DependsOn vmms | Out-Null
# The bridge authenticates the pipe against SCM's registered process ID. Grant
# only SERVICE_QUERY_STATUS to its SID, never service control/configuration.
$sddlResult = Invoke-RunnerNative -FilePath "$env:SystemRoot\System32\sc.exe" -ArgumentList @('sdshow', 'MotorRunner') -Operation 'read runner service security descriptor' -LogPath $nativeLogPath
$sddl = ($sddlResult.stdout -split '\r?\n' | Where-Object { $_ -match '^[DOGS]:' }) -join ''
if (-not $sddl) { throw 'Cannot read runner service security descriptor' }
$descriptor = [Security.AccessControl.RawSecurityDescriptor]::new($sddl)
if ($null -eq $descriptor.DiscretionaryAcl) { throw 'Runner service has an unexpected null DACL' }
$ace = [Security.AccessControl.CommonAce]::new([Security.AccessControl.AceFlags]::None, [Security.AccessControl.AceQualifier]::AccessAllowed, 4, [Security.Principal.SecurityIdentifier]::new($sshSid), $false, $null)
$descriptor.DiscretionaryAcl.InsertAce($descriptor.DiscretionaryAcl.Count, $ace)
$updatedSddl = $descriptor.GetSddlForm([Security.AccessControl.AccessControlSections]::All)
$null = Invoke-RunnerNative -FilePath "$env:SystemRoot\System32\sc.exe" -ArgumentList @('sdset', 'MotorRunner', $updatedSddl) -Operation 'set runner service security descriptor' -LogPath $nativeLogPath
New-ItemProperty 'HKLM:\SYSTEM\CurrentControlSet\Services\MotorRunner' -Name Environment -PropertyType MultiString -Value @('HOME=C:\ProgramData\MotorRunner\service-home','PATH=C:\Windows\System32;C:\Windows;C:\Program Files\Git\cmd;C:\Program Files\Git\bin') -Force | Out-Null
$null = Invoke-RunnerNative -FilePath "$env:SystemRoot\System32\sc.exe" -ArgumentList @('failure', 'MotorRunner', 'reset=', '86400', 'actions=', 'restart/5000/restart/15000/restart/60000') -Operation 'configure runner service recovery' -LogPath $nativeLogPath
$null = Invoke-RunnerNative -FilePath "$env:SystemRoot\System32\sc.exe" -ArgumentList @('failureflag', 'MotorRunner', '1') -Operation 'enable runner recovery on service failure' -LogPath $nativeLogPath
Start-Service MotorRunner
Write-Host "EVIDENCE sealed-parent SHA256=$parentHash path=$parent"
Write-Host 'Installed with one slot, AC-only admission. Fleet routing remains disabled until qualification evidence is reviewed.'
Get-Content 'C:\ProgramData\ssh\ssh_host_ed25519_key.pub'
Write-Host 'Use the host public key line above as the pinned Ally sshd key; the dedicated SSH username is motor-ssh.'
