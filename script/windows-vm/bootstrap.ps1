# Runs elevated once from the unattended first logon. No fleet credentials are used.
param([switch]$JobBaseline)

$ErrorActionPreference = 'Stop'
$ProgressPreference = 'SilentlyContinue'
[Net.ServicePointManager]::SecurityProtocol = [Net.SecurityProtocolType]::Tls12
$seed = Get-Volume | Where-Object { $_.DriveLetter -and (Test-Path "$($_.DriveLetter):\bootstrap.ps1") } | Select-Object -First 1
if (-not $seed) { throw 'Provisioning CD not found' }
$JobBaseline = $JobBaseline -or (Test-Path "$($seed.DriveLetter):\job-baseline")
New-Item C:\build\provision -ItemType Directory -Force | Out-Null
Copy-Item "$($seed.DriveLetter):\*.ps1" C:\build\provision -Force
if ($JobBaseline) {
    Copy-Item "$($seed.DriveLetter):\motor-winagent" C:\build\provision -Recurse -Force
} else {
    Copy-Item "$($seed.DriveLetter):\authorized_keys" C:\build\provision -Force
}
# Files copied from a CD retain its read-only attribute; allow later maintenance.
Get-ChildItem C:\build\provision -File -Recurse | ForEach-Object { $_.IsReadOnly = $false }
Start-Transcript C:\build\provision\bootstrap.log -Append
Remove-Item C:\build\provision\FAILED.txt -Force -ErrorAction SilentlyContinue
try {
    # Provisioning needs a NIC for Windows Update and official tool installers.
    $nic = Get-NetAdapter | Where-Object { $_.InterfaceDescription -like '*VirtIO*' -and $_.Status -eq 'Up' }
    if (-not $nic) {
        $drivers = Get-Volume | Where-Object { $_.DriveLetter -and (Test-Path "$($_.DriveLetter):\NetKVM\2k22\amd64\netkvm.inf") } | Select-Object -First 1
        if (-not $drivers) { throw 'Official virtio-win driver CD not found' }
        & pnputil /add-driver "$($drivers.DriveLetter):\NetKVM\2k22\amd64\netkvm.inf" /install
        # Already-staged drivers can return a nonzero status despite a working NIC.
        $nic = Get-NetAdapter | Where-Object { $_.InterfaceDescription -like '*VirtIO*' -and $_.Status -eq 'Up' }
        if (-not $nic) { throw "VirtIO network unavailable after pnputil: $LASTEXITCODE" }
    }

    # Clear autologon and replace the temporary setup passwords with random,
    # guest-local values. Neither password is saved or used for remote access.
    $winlogon = 'HKLM:\SOFTWARE\Microsoft\Windows NT\CurrentVersion\Winlogon'
    Set-ItemProperty $winlogon AutoAdminLogon '0'
    Remove-ItemProperty $winlogon DefaultPassword -ErrorAction SilentlyContinue
    Remove-ItemProperty $winlogon AutoLogonCount -ErrorAction SilentlyContinue
    foreach ($account in @('builder','Administrator')) {
        $bytes = New-Object byte[] 48
        $rng = [Security.Cryptography.RandomNumberGenerator]::Create()
        $rng.GetBytes($bytes)
        $rng.Dispose()
        $password = ConvertTo-SecureString ('Aa1!' + [Convert]::ToBase64String($bytes)) -AsPlainText -Force
        Set-LocalUser $account -Password $password -PasswordNeverExpires $true
    }
    Disable-LocalUser Administrator
    # Server otherwise ignores the host's ACPI power button when nobody is logged on.
    # The console and QMP socket remain private to the trusted host administrators.
    New-ItemProperty 'HKLM:\SOFTWARE\Microsoft\Windows\CurrentVersion\Policies\System' -Name ShutdownWithoutLogon -PropertyType DWord -Value 1 -Force | Out-Null
    Remove-Item C:\Windows\Panther\unattend.xml,C:\Windows\Panther\Unattend\unattend.xml,C:\Windows\System32\Sysprep\unattend.xml -Force -ErrorAction SilentlyContinue

    if (-not $JobBaseline) {
    Add-WindowsCapability -Online -Name OpenSSH.Server~~~~0.0.1.0 | Out-Null
    $ssh = "$env:ProgramData\ssh"
    New-Item $ssh -ItemType Directory -Force | Out-Null
    Copy-Item C:\build\provision\authorized_keys "$ssh\administrators_authorized_keys" -Force
    & icacls "$ssh\administrators_authorized_keys" /inheritance:r /grant:r '*S-1-5-18:F' '*S-1-5-32-544:F'
    if ($LASTEXITCODE -ne 0) { throw 'SSH key ACL setup failed' }
    @'
Port 22
PubkeyAuthentication yes
PasswordAuthentication no
KbdInteractiveAuthentication no
ChallengeResponseAuthentication no
AuthenticationMethods publickey
AllowUsers builder
AllowAgentForwarding no
AllowTcpForwarding no
X11Forwarding no
Subsystem sftp sftp-server.exe
Match Group administrators
    AuthorizedKeysFile __PROGRAMDATA__/ssh/administrators_authorized_keys
'@ | Set-Content "$ssh\sshd_config" -Encoding ascii
    New-Item HKLM:\SOFTWARE\OpenSSH -Force | Out-Null
    Set-ItemProperty HKLM:\SOFTWARE\OpenSSH DefaultShell 'C:\Windows\System32\WindowsPowerShell\v1.0\powershell.exe'
    Set-ItemProperty HKLM:\SOFTWARE\OpenSSH DefaultShellCommandOption '-c'
    Get-NetFirewallRule -Name 'OpenSSH-Server-In-TCP' -ErrorAction SilentlyContinue | Disable-NetFirewallRule
    Get-NetFirewallRule -Name 'WindowsBuilderSSH' -ErrorAction SilentlyContinue | Remove-NetFirewallRule
    New-NetFirewallRule -Name 'WindowsBuilderSSH' -DisplayName 'SSH from QEMU NAT host only' -Direction Inbound -Action Allow -Protocol TCP -LocalPort 22 -RemoteAddress 10.0.2.2 -Profile Any | Out-Null
    # The service normally creates host keys on first start; validation runs first.
    & C:\Windows\System32\OpenSSH\ssh-keygen.exe -A
    if ($LASTEXITCODE -ne 0) { throw 'SSH host key generation failed' }
    # The SYSTEM service rejects host keys writable by the bootstrap user,
    # even if that user belongs to Administrators. Repair owner and grants.
    & icacls $ssh /setowner '*S-1-5-32-544' /T
    if ($LASTEXITCODE -ne 0) { throw 'SSH file ownership setup failed' }
    & icacls $ssh /remove:g builder /T
    if ($LASTEXITCODE -ne 0) { throw 'SSH bootstrap-user ACL cleanup failed' }
    & C:\Windows\System32\OpenSSH\sshd.exe -t
    if ($LASTEXITCODE -ne 0) { throw 'Invalid sshd configuration' }
    Set-Service sshd -StartupType Automatic
    if ((Get-Service sshd).Status -ne 'Running') { Start-Service sshd }
    New-Item C:\build\provision\ssh-ready -ItemType File -Force | Out-Null
    }

    & C:\build\provision\setup-guest.ps1 -JobBaseline:$JobBaseline
    if (-not $?) { throw 'Guest toolchain setup failed' }
    if ($JobBaseline) {
        & C:\build\provision\setup-job-baseline.ps1
        if (-not $?) { throw 'Job baseline setup failed' }
    }
    Set-Content C:\build\provision\provisioned (Get-Date -Format o)
} catch {
    $_ | Out-String | Set-Content C:\build\provision\FAILED.txt
    throw
} finally {
    Stop-Transcript
}
