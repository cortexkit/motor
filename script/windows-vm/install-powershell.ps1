# Install the pinned ZIP while provisioning the job image as an administrator.
# The job account may execute the tree, but only SYSTEM/Administrators can change it.
param([string]$Archive = "$PSScriptRoot\PowerShell-7.6.7-win-x64.zip")

function Get-MotorNativeExitHex([int]$Code) {
    $unsigned = [BitConverter]::ToUInt32([BitConverter]::GetBytes($Code), 0)
    return ('0x{0:X8}' -f $unsigned)
}

function Invoke-MotorPowerShellProbe([string]$Executable, [string]$Command, [string]$LogDirectory, [int]$TimeoutMs = 60000) {
    $process = New-Object Diagnostics.Process
    $process.StartInfo.FileName = $Executable
    # EncodedCommand preserves the exact script without native quoting ambiguity.
    $encoded = [Convert]::ToBase64String([Text.Encoding]::Unicode.GetBytes($Command))
    $process.StartInfo.Arguments = "-NoLogo -NoProfile -NonInteractive -EncodedCommand $encoded"
    $process.StartInfo.WorkingDirectory = (Get-Location).ProviderPath
    $process.StartInfo.UseShellExecute = $false
    $process.StartInfo.CreateNoWindow = $true
    $process.StartInfo.RedirectStandardOutput = $true
    $process.StartInfo.RedirectStandardError = $true
    $process.StartInfo.StandardOutputEncoding = [Text.UTF8Encoding]::new($false)
    $process.StartInfo.StandardErrorEncoding = [Text.UTF8Encoding]::new($false)
    # Trace only this child. No runtime feature, machine environment, or policy
    # override is installed in the baseline to conceal a startup failure.
    $process.StartInfo.EnvironmentVariables['DOTNET_HOST_TRACE'] = '1'
    $process.StartInfo.EnvironmentVariables['DOTNET_HOST_TRACE_VERBOSITY'] = '4'
    $process.StartInfo.EnvironmentVariables['DOTNET_HOST_TRACEFILE'] = Join-Path $LogDirectory 'pwsh-host-trace.log'
    $started = $false
    try {
        $started = $process.Start()
        if (-not $started) { throw 'PowerShell probe process did not start' }
        $stdout = $process.StandardOutput.ReadToEndAsync()
        $stderr = $process.StandardError.ReadToEndAsync()
        $finished = $process.WaitForExit($TimeoutMs)
        if (-not $finished) { $process.Kill(); $process.WaitForExit() }
        $result = [pscustomobject]@{
            executable=$Executable; arguments=$process.StartInfo.Arguments
            workingDirectory=$process.StartInfo.WorkingDirectory; timedOut=(-not $finished)
            exitCode=$process.ExitCode; exitHex=(Get-MotorNativeExitHex $process.ExitCode)
            stdout=$stdout.Result; stderr=$stderr.Result
        }
        [IO.File]::WriteAllText((Join-Path $LogDirectory 'pwsh-probe.stdout.log'), $result.stdout)
        [IO.File]::WriteAllText((Join-Path $LogDirectory 'pwsh-probe.stderr.log'), $result.stderr)
        $result | ConvertTo-Json -Depth 4 | Set-Content (Join-Path $LogDirectory 'pwsh-probe.json') -Encoding UTF8
        Write-Host "EVIDENCE PowerShell probe exit=$($result.exitCode) hex=$($result.exitHex) timedOut=$($result.timedOut)"
        Write-Host "EVIDENCE PowerShell stdout:`n$($result.stdout)"
        Write-Host "EVIDENCE PowerShell stderr:`n$($result.stderr)"
        return $result
    } finally {
        try {
            if ($started -and -not $process.HasExited) { $process.Kill(); $process.WaitForExit() }
        } finally { $process.Dispose() }
    }
}

function Assert-MotorPowerShellProbe($Probe, [string]$Expected) {
    if ($Probe.timedOut) { throw "PowerShell startup timed out: $($Probe.exitHex)" }
    if ($Probe.exitCode -ne 0) {
        throw "PowerShell startup failed: exit=$($Probe.exitCode) ($($Probe.exitHex)); stderr=$($Probe.stderr)"
    }
    if ($Probe.stdout.Trim() -cne $Expected) { throw "Unexpected PowerShell version: '$($Probe.stdout)' (expected '$Expected')" }
}

function Write-MotorPowerShellStartupDiagnostics([string]$Destination, [string]$Archive, [string]$LogDirectory) {
    & {
        'PowerShell startup failure evidence; no runtime workaround applied'
        "identity=$([Security.Principal.WindowsIdentity]::GetCurrent().Name) process64=$([Environment]::Is64BitProcess) cwd=$((Get-Location).ProviderPath)"
        Get-ItemProperty 'HKLM:\SOFTWARE\Microsoft\Windows NT\CurrentVersion' | Select-Object ProductName,CurrentBuildNumber,UBR | Format-List
        try { Get-HotFix | Select-Object HotFixID,Description,InstalledOn | Format-List }
        catch { "EVIDENCE installed-hotfix inventory unavailable: $_" }
        Get-CimInstance Win32_Processor | Select-Object Name,NumberOfCores,NumberOfLogicalProcessors | Format-List
        foreach ($name in @('SystemRoot','PATH','USERPROFILE','TEMP','TMP','DOTNET_ROOT','DOTNET_ROOT_X64','DOTNET_EnableHWIntrinsic','COMPlus_EnableHWIntrinsic','DOTNET_ReadyToRun','COMPlus_ReadyToRun')) {
            "$name=$([Environment]::GetEnvironmentVariable($name))"
        }
        foreach ($relative in @('pwsh.exe','pwsh.dll','hostfxr.dll','hostpolicy.dll','coreclr.dll','System.Private.CoreLib.dll','pwsh.runtimeconfig.json','pwsh.deps.json')) {
            $file = Join-Path $Destination $relative
            if (Test-Path -LiteralPath $file) {
                Get-Item -LiteralPath $file | Select-Object FullName,Length,@{Name='Version';Expression={$_.VersionInfo.FileVersion}} | Format-List
                Get-FileHash -LiteralPath $file | Format-List
                Get-Acl -LiteralPath $file | Select-Object Path,Sddl | Format-List
            } else { "ABSENT $file" }
        }
        Get-Content (Join-Path $Destination 'pwsh.runtimeconfig.json') -ErrorAction Continue
        foreach ($name in @('vcruntime140.dll','vcruntime140_1.dll','msvcp140.dll','ucrtbase.dll')) {
            foreach ($directory in @($Destination, "$env:SystemRoot\System32")) {
                $file = Join-Path $directory $name
                if (Test-Path -LiteralPath $file) {
                    Get-Item -LiteralPath $file | Select-Object FullName,Length,@{Name='Version';Expression={$_.VersionInfo.FileVersion}} | Format-List
                } else { "ABSENT $file" }
            }
        }
        # Compare extracted bytes to the independently hash-pinned archive, not
        # to another read of the installed file with the same potential damage.
        Add-Type -AssemblyName System.IO.Compression.FileSystem
        $zip = [IO.Compression.ZipFile]::OpenRead($Archive)
        try {
            foreach ($name in @('pwsh.exe','pwsh.dll','hostfxr.dll','hostpolicy.dll','coreclr.dll','System.Private.CoreLib.dll')) {
                $entry = $zip.GetEntry($name)
                $file = Join-Path $Destination $name
                if ($null -eq $entry -or -not (Test-Path -LiteralPath $file)) { "INTEGRITY unavailable $name"; continue }
                $stream = $entry.Open()
                $sha = [Security.Cryptography.SHA256]::Create()
                try { $archiveHash = [BitConverter]::ToString($sha.ComputeHash($stream)).Replace('-', '') }
                finally { $sha.Dispose(); $stream.Dispose() }
                $installedHash = (Get-FileHash -LiteralPath $file).Hash
                "INTEGRITY $name archive=$archiveHash installed=$installedHash match=$($archiveHash -eq $installedHash)"
            }
        } finally { $zip.Dispose() }
        try {
            Get-WinEvent -FilterHashtable @{LogName='Application'; Id=@(1000,1001,1023,1026); StartTime=(Get-Date).AddMinutes(-10)} -MaxEvents 12 | Select-Object TimeCreated,ProviderName,Id,Message | Format-List
        } catch { "EVIDENCE Application events unavailable: $_" }
    } | Out-String -Width 240 | Tee-Object -FilePath (Join-Path $LogDirectory 'pwsh-startup-diagnostics.log') | ForEach-Object { Write-Host $_ }
}

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
try {
    $probe = Invoke-MotorPowerShellProbe "$destination\pwsh.exe" '$PSVersionTable.PSVersion.ToString()' $PSScriptRoot
    Assert-MotorPowerShellProbe $probe '7.6.7'
} catch {
    $failure = $_
    try { Write-MotorPowerShellStartupDiagnostics $destination $Archive $PSScriptRoot }
    catch { Write-Warning "PowerShell failure diagnostics could not finish: $_" }
    throw $failure
}
$version = $probe.stdout.Trim()
# Native tools may rebuild PATH from the registry instead of inheriting the agent's PATH.
$machinePath = [Environment]::GetEnvironmentVariable('Path', 'Machine')
if (($machinePath -split ';' | ForEach-Object { $_.TrimEnd('\') }) -notcontains $destination) {
    [Environment]::SetEnvironmentVariable('Path', "$destination;$machinePath", 'Machine')
}
$env:Path = "$destination;" + $env:Path
Write-Host "Installed PowerShell $version with protected job-user read/execute access"
