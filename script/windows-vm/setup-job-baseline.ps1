# Runs elevated after setup-guest.ps1 -JobBaseline has installed Git and MSVC.
# This image is for disposable job VMs, never for the standing SSH build VM.
param([string]$AgentSource = "$PSScriptRoot\motor-winagent", [switch]$HyperVSpike)

$ErrorActionPreference = 'Stop'
$ProgressPreference = 'SilentlyContinue'
[Net.ServicePointManager]::SecurityProtocol = [Net.SecurityProtocolType]::Tls12
if (-not ([Security.Principal.WindowsPrincipal] [Security.Principal.WindowsIdentity]::GetCurrent()).IsInRole([Security.Principal.WindowsBuiltInRole]::Administrator)) {
    throw 'Job baseline setup must run elevated'
}
if ($env:PROCESSOR_ARCHITECTURE -ne 'AMD64') { throw 'Job baseline requires x64 Windows' }
& "$PSScriptRoot\install-powershell.ps1"
if (-not $?) { throw 'Machine-wide PowerShell installation failed' }

$toolchain = '1.99.0'
$hostTriple = 'x86_64-pc-windows-msvc'
$motor = 'C:\ProgramData\Motor'
$jobUser = 'motor-job'
$credential = "$motor\job-password.txt"
New-Item $motor -ItemType Directory -Force | Out-Null
# Limit C:\ProgramData\Motor to SYSTEM and Administrators before the agent
# binary and the job account's password file are written into it, so neither
# is ever readable by a job.
& icacls $motor /inheritance:r /grant:r '*S-1-5-18:(OI)(CI)F' '*S-1-5-32-544:(OI)(CI)F'
if ($LASTEXITCODE -ne 0) { throw 'Motor directory ACL setup failed' }

$bytes = New-Object byte[] 48
$rng = [Security.Cryptography.RandomNumberGenerator]::Create()
$rng.GetBytes($bytes)
$rng.Dispose()
$password = 'Aa1!' + [Convert]::ToBase64String($bytes)
$securePassword = ConvertTo-SecureString $password -AsPlainText -Force
$service = Get-Service motor-winagent -ErrorAction SilentlyContinue
if ($service -and $service.Status -ne 'Stopped') { Stop-Service motor-winagent -Force }
if (Get-LocalUser $jobUser -ErrorAction SilentlyContinue) {
    Set-LocalUser $jobUser -Password $securePassword -PasswordNeverExpires $true -UserMayChangePassword $false -AccountNeverExpires
    Enable-LocalUser $jobUser
} else {
    New-LocalUser $jobUser -Password $securePassword -PasswordNeverExpires -UserMayNotChangePassword -AccountNeverExpires | Out-Null
}
$jobSid = (Get-LocalUser $jobUser).SID.Value
$users = Get-LocalGroup -SID 'S-1-5-32-545'
$admins = Get-LocalGroup -SID 'S-1-5-32-544'
if (-not (Get-LocalGroupMember $users | Where-Object { $_.SID.Value -eq $jobSid })) {
    Add-LocalGroupMember $users -Member $jobUser
}
if (Get-LocalGroupMember $admins | Where-Object { $_.SID.Value -eq $jobSid }) {
    Remove-LocalGroupMember $admins -Member $jobUser
}
[IO.File]::WriteAllText($credential, $password, (New-Object Text.UTF8Encoding $false))
& icacls $credential /inheritance:r /grant:r '*S-1-5-18:F' '*S-1-5-32-544:F'
if ($LASTEXITCODE -ne 0) { throw 'Job credential ACL setup failed' }
$password = $null

# Export all USER_RIGHTS and rewrite only the two job assignments, preserving
# existing trustees. Add only the job SID, never a broad user group.
# SeCreateSymbolicLinkPrivilege replaces the need for machine-wide Developer Mode.
function Resolve-MotorTrusteeSid {
    param([string]$Trustee, [scriptblock]$ResolveName = {
        param($name)
        ([Security.Principal.NTAccount]::new($name)).Translate([Security.Principal.SecurityIdentifier]).Value
    })
    $Trustee = $Trustee.Trim()
    if ($Trustee -match '^\*?(S-\d+(?:-\d+)+)$') { return $Matches[1] }
    try {
        $sid = & $ResolveName $Trustee
        if ($sid -notmatch '^S-\d+(?:-\d+)+$') { throw 'Invalid resolved SID' }
        return $sid
    } catch {
        throw "Cannot resolve policy trustee '$Trustee': $_"
    }
}

function Assert-MotorDeveloperModeDisabled {
    param([scriptblock]$ReadKey = {
        Get-ItemProperty -LiteralPath 'HKLM:\SOFTWARE\Microsoft\Windows\CurrentVersion\AppModelUnlock' -ErrorAction SilentlyContinue
    })
    # Fresh Server images may have neither the key nor the value. Only an
    # explicitly enabled value is Developer Mode, not an absent setting.
    $developerMode = (& $ReadKey).AllowDevelopmentWithoutDevLicense
    if ($developerMode -eq 1) { throw 'Job baseline must not enable Developer Mode' }
}

$policy = Join-Path $env:TEMP 'motor-job-rights.inf'
$policyDb = Join-Path $env:TEMP 'motor-job-rights.sdb'
try {
    & secedit /export /cfg $policy /areas USER_RIGHTS /quiet
    if ($LASTEXITCODE -ne 0) { throw 'Exporting local logon rights failed' }
    $lines = @(Get-Content $policy)
    foreach ($right in @('SeBatchLogonRight', 'SeCreateSymbolicLinkPrivilege')) {
        $pattern = '^' + $right + '\s*='
        $entry = @($lines | Where-Object { $_ -match $pattern })
        if ($entry.Count -gt 1) { throw "Duplicate policy entries for $right" }
        if ($entry) {
            $grants = @((($entry[0] -split '=', 2)[1] -split ',') | ForEach-Object { $_.Trim() } | Where-Object { $_ })
            $grantSids = @($grants | ForEach-Object { Resolve-MotorTrusteeSid $_ })
            if ($grantSids -notcontains $jobSid) { $grants += "*$jobSid" }
            $lines = $lines | ForEach-Object {
                if ($_ -match $pattern) { "$right = " + ($grants -join ',') } else { $_ }
            }
        } else {
            $lines = $lines | ForEach-Object {
                $_
                if ($_ -eq '[Privilege Rights]') { "$right = *$jobSid" }
            }
        }
    }
    $lines | Set-Content $policy -Encoding Unicode
    & secedit /configure /db $policyDb /cfg $policy /areas USER_RIGHTS /quiet
    if ($LASTEXITCODE -ne 0) { throw 'Granting job account rights failed' }
    # Re-export the configured assignments, not the template just written.
    # Qualification after reboot separately checks the job token's privileges.
    Remove-Item $policy -Force
    & secedit /export /cfg $policy /areas USER_RIGHTS /quiet
    if ($LASTEXITCODE -ne 0) { throw 'Verifying job account rights failed' }
    foreach ($right in @('SeBatchLogonRight', 'SeCreateSymbolicLinkPrivilege')) {
        $entry = @(Get-Content $policy | Where-Object { $_ -match ('^' + $right + '\s*=') })
        if ($entry.Count -ne 1) { throw "Expected one policy entry for $right" }
        $grants = @((($entry[0] -split '=', 2)[1] -split ',') | ForEach-Object { $_.Trim() } | Where-Object { $_ })
        $grantSids = @($grants | ForEach-Object { Resolve-MotorTrusteeSid $_ })
        if ($grantSids -notcontains $jobSid) { throw "Job SID missing from $right" }
        Write-Host "Verified $right for motor-job SID $jobSid"
    }
    Assert-MotorDeveloperModeDisabled
} finally {
    Remove-Item $policy,$policyDb -Force -ErrorAction SilentlyContinue
}

# Create the real profile now so exclusions use its actual path, including any suffix.
$profileKey = "HKLM:\SOFTWARE\Microsoft\Windows NT\CurrentVersion\ProfileList\$jobSid"
if (-not (Test-Path $profileKey)) {
    Add-Type @'
using System;
using System.Runtime.InteropServices;
using System.Text;
public static class MotorProfile {
    [DllImport("userenv.dll", CharSet = CharSet.Unicode)]
    public static extern int CreateProfile(string sid, string user, StringBuilder path, uint length);
}
'@
    $profileBuffer = New-Object Text.StringBuilder 260
    $result = [MotorProfile]::CreateProfile($jobSid, $jobUser, $profileBuffer, 260)
    if ($result -ne 0) { throw "Creating job profile failed: $result" }
}
$jobProfile = [Environment]::ExpandEnvironmentVariables((Get-ItemProperty $profileKey).ProfileImagePath)
$jobCargo = Join-Path $jobProfile '.cargo'
New-Item $jobCargo,'C:\m','C:\t' -ItemType Directory -Force | Out-Null
& icacls 'C:\m' /inheritance:r /grant:r '*S-1-5-18:(OI)(CI)F' '*S-1-5-32-544:(OI)(CI)F' "*${jobSid}:(OI)(CI)M"
if ($LASTEXITCODE -ne 0) { throw 'Job source directory ACL setup failed' }
& icacls $jobCargo /inheritance:r /grant:r '*S-1-5-18:(OI)(CI)F' '*S-1-5-32-544:(OI)(CI)F' "*${jobSid}:(OI)(CI)M"
if ($LASTEXITCODE -ne 0) { throw 'Job cargo home ACL setup failed' }

if (-not $HyperVSpike) {
    $drivers = Get-Volume | Where-Object { $_.DriveLetter -and (Test-Path "$($_.DriveLetter):\vioserial\2k22\amd64\vioser.inf") } | Select-Object -First 1
    if (-not $drivers) { throw 'Official virtio-win CD with the Server 2022 serial driver not found' }
    & pnputil /add-driver "$($drivers.DriveLetter):\vioserial\2k22\amd64\vioser.inf" /install
    if ($LASTEXITCODE -notin @(0,3010)) { throw "Virtio-serial driver installation failed: $LASTEXITCODE" }
}

$env:RUSTUP_HOME = 'C:\rust\rustup'
$env:CARGO_HOME = 'C:\rust\cargo'
New-Item $env:RUSTUP_HOME,$env:CARGO_HOME -ItemType Directory -Force | Out-Null
Invoke-WebRequest 'https://win.rustup.rs/x86_64' -OutFile "$env:TEMP\rustup-init.exe" -UseBasicParsing
& "$env:TEMP\rustup-init.exe" -y --profile minimal --default-toolchain $toolchain --default-host $hostTriple --no-modify-path
if ($LASTEXITCODE -ne 0) { throw "Machine-wide rustup installation failed: $LASTEXITCODE" }
[Environment]::SetEnvironmentVariable('RUSTUP_HOME', $env:RUSTUP_HOME, 'Machine')
$machinePath = [Environment]::GetEnvironmentVariable('Path', 'Machine')
if (($machinePath -split ';') -notcontains 'C:\rust\cargo\bin') {
    [Environment]::SetEnvironmentVariable('Path', "C:\rust\cargo\bin;$machinePath", 'Machine')
}
$env:Path = 'C:\rust\cargo\bin;' + $env:Path
$rustc = & 'C:\rust\cargo\bin\rustc.exe' -vV
if ($LASTEXITCODE -ne 0 -or $rustc -notcontains "release: $toolchain" -or $rustc -notcontains "host: $hostTriple") {
    throw 'Machine-wide Rust version/host verification failed'
}
$rustc | Write-Host
& 'C:\rust\cargo\bin\cargo.exe' --version
if ($LASTEXITCODE -ne 0) { throw 'Machine-wide Cargo verification failed' }
$git = 'C:\Program Files\Git\cmd\git.exe'
if (-not (Test-Path 'C:\Program Files\Git\bin\bash.exe')) { throw 'Git Bash is missing' }
& $git --version
if ($LASTEXITCODE -ne 0) { throw 'Machine-wide Git verification failed' }
& $git config --system core.longpaths true
if ($LASTEXITCODE -ne 0) { throw 'Git long-path configuration failed' }
New-ItemProperty 'HKLM:\SYSTEM\CurrentControlSet\Control\FileSystem' -Name LongPathsEnabled -PropertyType DWord -Value 1 -Force | Out-Null
foreach ($path in @('C:\m', 'C:\rust', $jobCargo)) { Add-MpPreference -ExclusionPath $path }

foreach ($name in @('Cargo.toml', 'Cargo.lock', 'src')) {
    if (-not (Test-Path (Join-Path $AgentSource $name))) { throw "Standalone agent source is missing $name" }
}
# Build with the pinned toolchain and lockfile inside this MSVC-equipped seed VM.
$agentOptions = @()
if ($HyperVSpike) { $agentOptions = @('--features', 'hyperv-spike') }
& 'C:\rust\cargo\bin\cargo.exe' build --release --locked --manifest-path "$AgentSource\Cargo.toml" --target $hostTriple @agentOptions
if ($LASTEXITCODE -ne 0) { throw "Agent release build failed: $LASTEXITCODE" }
Copy-Item "$AgentSource\target\$hostTriple\release\motor-winagent.exe" "$motor\motor-winagent.exe" -Force
$agentHash = (Get-FileHash "$motor\motor-winagent.exe" -Algorithm SHA256).Hash
Set-Content "$PSScriptRoot\winagent.sha256" "$agentHash  motor-winagent.exe" -Encoding ascii
Write-Host "Installed motor-winagent SHA-256: $agentHash"
if ($HyperVSpike) {
    & 'C:\rust\cargo\bin\cargo.exe' build --release --locked --manifest-path "$AgentSource\Cargo.toml" --target $hostTriple --example hyperv-spike
    if ($LASTEXITCODE -ne 0) { throw "Spike host client build failed: $LASTEXITCODE" }
    Copy-Item "$AgentSource\target\$hostTriple\release\examples\hyperv-spike.exe" "$PSScriptRoot\hyperv-spike.exe"
}
# Freeze toolchain files after building; jobs use their own writable Cargo home.
# Set the protected ACL on C:\rust only, then make every descendant inherit it.
# Granting (OI)(CI) entries recursively with /inheritance:r leaves each file
# with an empty DACL, because inheritance flags don't apply to files and the
# inherited entries are stripped; nobody, administrators included, can then
# run rustc. /L resets rustup's symlinked proxies themselves, not only their target.
& icacls 'C:\rust' /inheritance:r /grant:r '*S-1-5-18:(OI)(CI)F' '*S-1-5-32-544:(OI)(CI)F' "*${jobSid}:(OI)(CI)RX"
if ($LASTEXITCODE -ne 0) { throw 'Read-only job toolchain ACL setup failed' }
& icacls 'C:\rust\*' /reset /T /C /L /Q
if ($LASTEXITCODE -ne 0) { throw 'Toolchain ACL inheritance reset failed' }
# The freeze must leave the toolchain runnable; verify it after the ACL change.
$rustc = & 'C:\rust\cargo\bin\rustc.exe' -vV
if ($LASTEXITCODE -ne 0 -or $rustc -notcontains "release: $toolchain") {
    throw 'Rust is not runnable after the toolchain ACL change'
}
& 'C:\rust\cargo\bin\cargo.exe' --version
if ($LASTEXITCODE -ne 0) { throw 'Cargo is not runnable after the toolchain ACL change' }

# A rerun must also remove SSH state, not merely skip installing the server.
$sshd = Get-Service sshd -ErrorAction SilentlyContinue
if ($sshd) { Stop-Service sshd -Force -ErrorAction Stop }
$sshServer = Get-WindowsCapability -Online -Name OpenSSH.Server~~~~0.0.1.0
if ($sshServer.State -eq 'Installed') {
    Remove-WindowsCapability -Online -Name OpenSSH.Server~~~~0.0.1.0 | Out-Null
}
foreach ($rule in @('OpenSSH-Server-In-TCP', 'WindowsBuilderSSH')) {
    Get-NetFirewallRule -Name $rule -ErrorAction SilentlyContinue | Remove-NetFirewallRule
}
foreach ($path in @('C:\ProgramData\ssh', "$PSScriptRoot\authorized_keys", "$PSScriptRoot\ssh-ready")) {
    if (Test-Path $path) { Remove-Item $path -Recurse -Force }
}
Get-ChildItem C:\Users -Directory -Force | ForEach-Object {
    $sshHome = Join-Path $_.FullName '.ssh'
    if (Test-Path $sshHome) { Remove-Item $sshHome -Recurse -Force }
}
if ((Get-WindowsCapability -Online -Name OpenSSH.Server~~~~0.0.1.0).State -eq 'Installed') {
    throw 'Job baseline still has an SSH server'
}

if ($service) {
    & sc.exe config motor-winagent binPath= "$motor\motor-winagent.exe" start= auto obj= LocalSystem
    if ($LASTEXITCODE -ne 0) { throw 'Updating agent service failed' }
} else {
    New-Service -Name motor-winagent -BinaryPathName "$motor\motor-winagent.exe" -StartupType Automatic -DisplayName 'Motor Windows job agent' | Out-Null
}
# Do not start on the provisioning boot: only a job VM supplies the port and nonce.
$agentService = Get-CimInstance Win32_Service -Filter "Name='motor-winagent'"
if ($agentService.StartName -ne 'LocalSystem' -or $agentService.StartMode -ne 'Auto') {
    throw 'Agent must be an auto-start LocalSystem service'
}
Write-Host 'Job baseline setup complete; reboot and verify as motor-job through the agent before sealing.'
