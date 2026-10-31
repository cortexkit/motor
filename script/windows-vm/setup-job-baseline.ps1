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

# Jobs are offline and cannot use the provisioning administrator's per-user Bun.
# SHA-256 source: https://github.com/oven-sh/bun/releases/download/bun-v1.4.4/SHASUMS256.txt
$bunVersion = '1.4.4'
$bunSha256 = 'cd2c0fccc78f3ede1f50ca827a15f063eda13d4d150a3d0674f4f6e323d864a5'
$bunArchive = Join-Path $motor 'bun-windows-x64.zip'
$bunInstall = 'C:\Program Files\Bun'
try {
    Invoke-WebRequest 'https://github.com/oven-sh/bun/releases/download/bun-v1.4.4/bun-windows-x64.zip' -OutFile $bunArchive -UseBasicParsing
    if ((Get-FileHash -LiteralPath $bunArchive -Algorithm SHA256).Hash.ToLowerInvariant() -cne $bunSha256) {
        throw 'Bun archive SHA-256 mismatch'
    }
    if (Test-Path -LiteralPath $bunInstall) { throw 'Bun destination already exists; use a fresh provisioning image' }
    New-Item $bunInstall -ItemType Directory | Out-Null
    # Protect the root before extraction; reset descendants to inherit its policy.
    & icacls $bunInstall /inheritance:r /grant:r '*S-1-5-18:(OI)(CI)F' '*S-1-5-32-544:(OI)(CI)F' '*S-1-5-32-545:(OI)(CI)RX'
    if ($LASTEXITCODE -ne 0) { throw 'Bun root ACL setup failed' }
    Expand-Archive -LiteralPath $bunArchive -DestinationPath $bunInstall
    Move-Item -LiteralPath "$bunInstall\bun-windows-x64\bun.exe" -Destination "$bunInstall\bun.exe"
    Remove-Item -LiteralPath "$bunInstall\bun-windows-x64" -Recurse -Force
    & icacls "$bunInstall\*" /reset /T /C /L /Q
    if ($LASTEXITCODE -ne 0) { throw 'Bun ACL inheritance reset failed' }
    & icacls $bunInstall /setowner '*S-1-5-32-544' /T /C /L /Q
    if ($LASTEXITCODE -ne 0) { throw 'Bun ownership setup failed' }
} finally {
    Remove-Item -LiteralPath $bunArchive -Force -ErrorAction SilentlyContinue
}
$machinePath = [Environment]::GetEnvironmentVariable('Path', 'Machine')
if (($machinePath -split ';' | ForEach-Object { $_.TrimEnd('\') }) -notcontains $bunInstall) {
    [Environment]::SetEnvironmentVariable('Path', "$bunInstall;$machinePath", 'Machine')
}
$env:Path = "$bunInstall;" + $env:Path
$installedBunVersion = & "$bunInstall\bun.exe" --version
if ($LASTEXITCODE -ne 0 -or $installedBunVersion -cne $bunVersion) { throw 'Machine-wide Bun version verification failed' }
Write-Host "Verified machine-wide Bun $installedBunVersion with protected user read/execute access"

# Offline jobs need the full standard library, so use the official all-users
# installer rather than the embeddable distribution with its restricted site path.
# Archive probes need Python 3.14's compression.zstd module.
# SHA-256 source: https://www.python.org/ftp/python/3.14.8/python-3.14.8-amd64.exe.spdx.json
$pythonVersion = '3.14.8'
$pythonSha256 = '759be887b96e736a3ca886daf8d575f18fcae1a09efab6902f42d59e8999f8ef'
$pythonInstaller = Join-Path $motor 'python-3.14.8-amd64.exe'
$pythonInstall = 'C:\Python314'
try {
    Invoke-WebRequest 'https://www.python.org/ftp/python/3.14.8/python-3.14.8-amd64.exe' -OutFile $pythonInstaller -UseBasicParsing
    if ((Get-FileHash -LiteralPath $pythonInstaller -Algorithm SHA256).Hash.ToLowerInvariant() -cne $pythonSha256) {
        throw 'Python installer SHA-256 mismatch'
    }
    if (Test-Path -LiteralPath $pythonInstall) { throw 'Python destination already exists; use a fresh provisioning image' }
    New-Item $pythonInstall -ItemType Directory | Out-Null
    & icacls $pythonInstall /inheritance:r /grant:r '*S-1-5-18:(OI)(CI)F' '*S-1-5-32-544:(OI)(CI)F' '*S-1-5-32-545:(OI)(CI)RX'
    if ($LASTEXITCODE -ne 0) { throw 'Python root ACL setup failed' }
    $pythonArguments = @('/quiet', 'InstallAllUsers=1', 'PrependPath=0', 'Include_launcher=0', 'Include_test=0', "TargetDir=$pythonInstall")
    $pythonProcess = Start-Process -FilePath $pythonInstaller -ArgumentList $pythonArguments -Wait -PassThru
    if ($pythonProcess.ExitCode -notin @(0,3010)) { throw "Machine-wide Python installation failed: $($pythonProcess.ExitCode)" }
    # The installer provides python.exe only. An adjacent copy gives Git Bash
    # python3 without a user alias, launcher or Windows Store execution stub.
    Copy-Item -LiteralPath "$pythonInstall\python.exe" -Destination "$pythonInstall\python3.exe"
    & icacls "$pythonInstall\*" /reset /T /C /L /Q
    if ($LASTEXITCODE -ne 0) { throw 'Python ACL inheritance reset failed' }
    & icacls $pythonInstall /setowner '*S-1-5-32-544' /T /C /L /Q
    if ($LASTEXITCODE -ne 0) { throw 'Python ownership setup failed' }
} finally {
    Remove-Item -LiteralPath $pythonInstaller -Force -ErrorAction SilentlyContinue
}
$machinePath = [Environment]::GetEnvironmentVariable('Path', 'Machine')
if (($machinePath -split ';' | ForEach-Object { $_.TrimEnd('\') }) -notcontains $pythonInstall) {
    [Environment]::SetEnvironmentVariable('Path', "$pythonInstall;$machinePath", 'Machine')
}
$env:Path = "$pythonInstall;" + $env:Path
foreach ($command in @('python3.exe', 'python.exe')) {
    $installedPythonVersion = & "$pythonInstall\$command" --version
    if ($LASTEXITCODE -ne 0 -or $installedPythonVersion -cne "Python $pythonVersion") { throw "Machine-wide Python version verification failed for ${command}: $installedPythonVersion" }
}
Write-Host "Verified machine-wide Python $pythonVersion with protected user read/execute access"
& "$pythonInstall\python3.exe" -c "import json, hashlib, subprocess, tomllib, compression.zstd"
if ($LASTEXITCODE -ne 0) { throw 'Machine-wide Python stdlib verification failed' }
Write-Host 'Verified machine-wide Python stdlib imports: json, hashlib, subprocess, tomllib, compression.zstd'

# Offline tests start a real NATS server, so put the verified release on the
# machine PATH rather than relying on a job to download its own executable.
# SHA-256 source: https://github.com/nats-io/nats-server/releases/download/v2.15.0/SHA256SUMS
$natsVersion = '2.15.0'
$natsSha256 = '2d0861dc2ca3567d17f7d19611d7b346cc2ccbd7086a0a77cc957a2c71663200'
$natsArchive = Join-Path $motor 'nats-server-v2.15.0-windows-amd64.zip'
$natsInstall = 'C:\Program Files\NATS'
try {
    Invoke-WebRequest 'https://github.com/nats-io/nats-server/releases/download/v2.15.0/nats-server-v2.15.0-windows-amd64.zip' -OutFile $natsArchive -UseBasicParsing
    if ((Get-FileHash -LiteralPath $natsArchive -Algorithm SHA256).Hash.ToLowerInvariant() -cne $natsSha256) {
        throw 'nats-server archive SHA-256 mismatch'
    }
    if (Test-Path -LiteralPath $natsInstall) { throw 'NATS destination already exists; use a fresh provisioning image' }
    New-Item $natsInstall -ItemType Directory | Out-Null
    & icacls $natsInstall /inheritance:r /grant:r '*S-1-5-18:(OI)(CI)F' '*S-1-5-32-544:(OI)(CI)F' '*S-1-5-32-545:(OI)(CI)RX'
    if ($LASTEXITCODE -ne 0) { throw 'NATS root ACL setup failed' }
    Expand-Archive -LiteralPath $natsArchive -DestinationPath $natsInstall
    Move-Item -LiteralPath "$natsInstall\nats-server-v$natsVersion-windows-amd64\nats-server.exe" -Destination "$natsInstall\nats-server.exe"
    Remove-Item -LiteralPath "$natsInstall\nats-server-v$natsVersion-windows-amd64" -Recurse -Force
    & icacls "$natsInstall\*" /reset /T /C /L /Q
    if ($LASTEXITCODE -ne 0) { throw 'NATS ACL inheritance reset failed' }
    & icacls $natsInstall /setowner '*S-1-5-32-544' /T /C /L /Q
    if ($LASTEXITCODE -ne 0) { throw 'NATS ownership setup failed' }
} finally {
    Remove-Item -LiteralPath $natsArchive -Force -ErrorAction SilentlyContinue
}
$machinePath = [Environment]::GetEnvironmentVariable('Path', 'Machine')
if (($machinePath -split ';' | ForEach-Object { $_.TrimEnd('\') }) -notcontains $natsInstall) {
    [Environment]::SetEnvironmentVariable('Path', "$natsInstall;$machinePath", 'Machine')
}
$env:Path = "$natsInstall;" + $env:Path
$installedNatsVersion = & "$natsInstall\nats-server.exe" --version
if ($LASTEXITCODE -ne 0 -or $installedNatsVersion -cne "nats-server: v$natsVersion") { throw 'Machine-wide nats-server version verification failed' }
Write-Host "Verified machine-wide nats-server v$natsVersion with protected user read/execute access"

# Bindgen needs the shared libclang library in the same machine-wide layout as
# GitHub's Windows images; offline jobs cannot download LLVM themselves.
# SHA-256 source: https://api.github.com/repos/llvm/llvm-project/releases/assets/329799346 (published digest for LLVM-21.1.8-win64.exe)
# Installer signature: https://github.com/llvm/llvm-project/releases/download/llvmorg-21.1.8/LLVM-21.1.8-win64.exe.sig
$llvmVersion = '21.1.8'
$llvmSha256 = '7a5386c26497db1691f320121e5b113364dd0274b98e55f15f4dbc00c0450113'
$llvmInstaller = Join-Path $motor 'LLVM-21.1.8-win64.exe'
$llvmInstall = 'C:\Program Files\LLVM'
try {
    Invoke-WebRequest 'https://github.com/llvm/llvm-project/releases/download/llvmorg-21.1.8/LLVM-21.1.8-win64.exe' -OutFile $llvmInstaller -UseBasicParsing
    if ((Get-FileHash -LiteralPath $llvmInstaller -Algorithm SHA256).Hash.ToLowerInvariant() -cne $llvmSha256) {
        throw 'LLVM installer SHA-256 mismatch'
    }
    if (Test-Path -LiteralPath $llvmInstall) { throw 'LLVM destination already exists; use a fresh provisioning image' }
    New-Item $llvmInstall -ItemType Directory | Out-Null
    & icacls $llvmInstall /inheritance:r /grant:r '*S-1-5-18:(OI)(CI)F' '*S-1-5-32-544:(OI)(CI)F' '*S-1-5-32-545:(OI)(CI)RX'
    if ($LASTEXITCODE -ne 0) { throw 'LLVM root ACL setup failed' }
    # NSIS requires /D to be last and unquoted, even when the path has spaces.
    $llvmProcess = Start-Process -FilePath $llvmInstaller -ArgumentList "/S /D=$llvmInstall" -Wait -PassThru
    if ($llvmProcess.ExitCode -ne 0) { throw "Machine-wide LLVM installation failed: $($llvmProcess.ExitCode)" }
    & icacls "$llvmInstall\*" /reset /T /C /L /Q
    if ($LASTEXITCODE -ne 0) { throw 'LLVM ACL inheritance reset failed' }
    & icacls $llvmInstall /setowner '*S-1-5-32-544' /T /C /L /Q
    if ($LASTEXITCODE -ne 0) { throw 'LLVM ownership setup failed' }
} finally {
    Remove-Item -LiteralPath $llvmInstaller -Force -ErrorAction SilentlyContinue
}
$llvmBin = "$llvmInstall\bin"
$machinePath = [Environment]::GetEnvironmentVariable('Path', 'Machine')
if (($machinePath -split ';' | ForEach-Object { $_.TrimEnd('\') }) -notcontains $llvmBin) {
    [Environment]::SetEnvironmentVariable('Path', "$llvmBin;$machinePath", 'Machine')
}
[Environment]::SetEnvironmentVariable('LIBCLANG_PATH', $llvmBin, 'Machine')
$env:LIBCLANG_PATH = $llvmBin
$env:Path = "$llvmBin;" + $env:Path
if (-not (Test-Path -LiteralPath "$llvmBin\libclang.dll" -PathType Leaf)) { throw 'Machine-wide LLVM libclang.dll is missing' }
$installedClangVersion = & "$llvmBin\clang.exe" --version
if ($LASTEXITCODE -ne 0 -or ($installedClangVersion | Select-Object -First 1) -cne "clang version $llvmVersion") { throw 'Machine-wide LLVM version verification failed' }
Write-Host "Verified machine-wide LLVM $llvmVersion and libclang.dll with protected user read/execute access"

# CMake sub-builds need Ninja to avoid the default MSBuild generator's parallel
# build races. Offline jobs must find both tools without downloading them.
# SHA-256 source: https://github.com/Kitware/CMake/releases/download/v4.4.5/cmake-4.4.5-SHA-256.txt
$cmakeVersion = '4.4.5'
$cmakeSha256 = '326c826486d46a23cf9d5a1c9ee3a567d427bdd02c4a34071937a2a675cff004'
$cmakeArchive = Join-Path $motor 'cmake-4.4.5-windows-x86_64.zip'
$cmakeInstall = 'C:\Program Files\CMake'
try {
    Invoke-WebRequest 'https://github.com/Kitware/CMake/releases/download/v4.4.5/cmake-4.4.5-windows-x86_64.zip' -OutFile $cmakeArchive -UseBasicParsing
    if ((Get-FileHash -LiteralPath $cmakeArchive -Algorithm SHA256).Hash.ToLowerInvariant() -cne $cmakeSha256) {
        throw 'CMake archive SHA-256 mismatch'
    }
    if (Test-Path -LiteralPath $cmakeInstall) { throw 'CMake destination already exists; use a fresh provisioning image' }
    New-Item $cmakeInstall -ItemType Directory | Out-Null
    & icacls $cmakeInstall /inheritance:r /grant:r '*S-1-5-18:(OI)(CI)F' '*S-1-5-32-544:(OI)(CI)F' '*S-1-5-32-545:(OI)(CI)RX'
    if ($LASTEXITCODE -ne 0) { throw 'CMake root ACL setup failed' }
    Expand-Archive -LiteralPath $cmakeArchive -DestinationPath $cmakeInstall
    # Keep CMake's modules and templates beside bin, not only cmake.exe.
    Get-ChildItem -LiteralPath "$cmakeInstall\cmake-$cmakeVersion-windows-x86_64" -Force | Move-Item -Destination $cmakeInstall
    Remove-Item -LiteralPath "$cmakeInstall\cmake-$cmakeVersion-windows-x86_64" -Force
    & icacls "$cmakeInstall\*" /reset /T /C /L /Q
    if ($LASTEXITCODE -ne 0) { throw 'CMake ACL inheritance reset failed' }
    & icacls $cmakeInstall /setowner '*S-1-5-32-544' /T /C /L /Q
    if ($LASTEXITCODE -ne 0) { throw 'CMake ownership setup failed' }
} finally {
    Remove-Item -LiteralPath $cmakeArchive -Force -ErrorAction SilentlyContinue
}
$cmakeBin = "$cmakeInstall\bin"
$machinePath = [Environment]::GetEnvironmentVariable('Path', 'Machine')
if (($machinePath -split ';' | ForEach-Object { $_.TrimEnd('\') }) -notcontains $cmakeBin) {
    [Environment]::SetEnvironmentVariable('Path', "$cmakeBin;$machinePath", 'Machine')
}
$env:Path = "$cmakeBin;" + $env:Path
$installedCmakeVersion = @(& cmake.exe --version)
if ($LASTEXITCODE -ne 0 -or ($installedCmakeVersion | Select-Object -First 1) -cne "cmake version $cmakeVersion") { throw 'Machine-wide CMake version verification failed' }
Write-Host "Verified machine-wide CMake $cmakeVersion with protected user read/execute access"

# Ninja publishes no checksum file. This hash was computed from ninja-win.zip
# and cross-checked against the GitHub API asset digest for release v1.13.2:
# https://api.github.com/repos/ninja-build/ninja/releases/assets/318863577
$ninjaVersion = '1.13.2'
$ninjaSha256 = '07fc8261b42b20e71d1720b39068c2e14ffcee6396b76fb7a795fb460b78dc65'
$ninjaArchive = Join-Path $motor 'ninja-win.zip'
$ninjaInstall = 'C:\Program Files\Ninja'
try {
    Invoke-WebRequest 'https://github.com/ninja-build/ninja/releases/download/v1.13.2/ninja-win.zip' -OutFile $ninjaArchive -UseBasicParsing
    if ((Get-FileHash -LiteralPath $ninjaArchive -Algorithm SHA256).Hash.ToLowerInvariant() -cne $ninjaSha256) {
        throw 'Ninja archive SHA-256 mismatch'
    }
    if (Test-Path -LiteralPath $ninjaInstall) { throw 'Ninja destination already exists; use a fresh provisioning image' }
    New-Item $ninjaInstall -ItemType Directory | Out-Null
    & icacls $ninjaInstall /inheritance:r /grant:r '*S-1-5-18:(OI)(CI)F' '*S-1-5-32-544:(OI)(CI)F' '*S-1-5-32-545:(OI)(CI)RX'
    if ($LASTEXITCODE -ne 0) { throw 'Ninja root ACL setup failed' }
    Expand-Archive -LiteralPath $ninjaArchive -DestinationPath $ninjaInstall
    & icacls "$ninjaInstall\*" /reset /T /C /L /Q
    if ($LASTEXITCODE -ne 0) { throw 'Ninja ACL inheritance reset failed' }
    & icacls $ninjaInstall /setowner '*S-1-5-32-544' /T /C /L /Q
    if ($LASTEXITCODE -ne 0) { throw 'Ninja ownership setup failed' }
} finally {
    Remove-Item -LiteralPath $ninjaArchive -Force -ErrorAction SilentlyContinue
}
$machinePath = [Environment]::GetEnvironmentVariable('Path', 'Machine')
if (($machinePath -split ';' | ForEach-Object { $_.TrimEnd('\') }) -notcontains $ninjaInstall) {
    [Environment]::SetEnvironmentVariable('Path', "$ninjaInstall;$machinePath", 'Machine')
}
$env:Path = "$ninjaInstall;" + $env:Path
$installedNinjaVersion = & ninja.exe --version
if ($LASTEXITCODE -ne 0 -or $installedNinjaVersion -cne $ninjaVersion) { throw 'Machine-wide Ninja version verification failed' }
Write-Host "Verified machine-wide Ninja $ninjaVersion with protected user read/execute access"

# Native children need jq on machine PATH, not a Git Bash-only shim.
# SHA-256 source: https://raw.githubusercontent.com/jqlang/jq/master/sig/v1.8.2/sha256sum.txt
$jqVersion = '1.8.2'
$jqSha256 = 'a6fc67fedaf9128a3309a1e2ebb8b986aeccf70122ee46d2cb4849e423f0c627'
$jqArchive = Join-Path $motor 'jq-windows-amd64.exe'
$jqInstall = 'C:\Program Files\jq'
try {
    Invoke-WebRequest 'https://github.com/jqlang/jq/releases/download/jq-1.8.2/jq-windows-amd64.exe' -OutFile $jqArchive -UseBasicParsing
    if ((Get-FileHash -LiteralPath $jqArchive -Algorithm SHA256).Hash.ToLowerInvariant() -cne $jqSha256) {
        throw 'jq archive SHA-256 mismatch'
    }
    if (Test-Path -LiteralPath $jqInstall) { throw 'jq destination already exists; use a fresh provisioning image' }
    New-Item $jqInstall -ItemType Directory | Out-Null
    & icacls $jqInstall /inheritance:r /grant:r '*S-1-5-18:(OI)(CI)F' '*S-1-5-32-544:(OI)(CI)F' '*S-1-5-32-545:(OI)(CI)RX'
    if ($LASTEXITCODE -ne 0) { throw 'jq root ACL setup failed' }
    Move-Item -LiteralPath $jqArchive -Destination "$jqInstall\jq.exe"
    & icacls "$jqInstall\*" /reset /T /C /L /Q
    if ($LASTEXITCODE -ne 0) { throw 'jq ACL inheritance reset failed' }
    & icacls $jqInstall /setowner '*S-1-5-32-544' /T /C /L /Q
    if ($LASTEXITCODE -ne 0) { throw 'jq ownership setup failed' }
} finally {
    Remove-Item -LiteralPath $jqArchive -Force -ErrorAction SilentlyContinue
}
$machinePath = [Environment]::GetEnvironmentVariable('Path', 'Machine')
if (($machinePath -split ';' | ForEach-Object { $_.TrimEnd('\') }) -notcontains $jqInstall) {
    [Environment]::SetEnvironmentVariable('Path', "$jqInstall;$machinePath", 'Machine')
}
$env:Path = "$jqInstall;" + $env:Path
$installedJqVersion = & jq.exe --version
if ($LASTEXITCODE -ne 0 -or $installedJqVersion -cne "jq-$jqVersion") { throw 'Machine-wide jq version verification failed' }
Write-Host "Verified machine-wide jq $jqVersion with protected user read/execute access"

# The official Node zip carries npm as well as node.exe for offline JS gates.
# SHA-256 source: https://nodejs.org/dist/v24.22.2/SHASUMS256.txt
# Its SHASUMS256.txt.sig was verified with the nodejs/release-keys keyring.
$nodeVersion = '24.22.2'
$npmVersion = '11.19.1'
$nodeSha256 = 'ca1bff5d8ead13c9f2ea404a176db46bd430704d5b5113734049fa365e7799e2'
$nodeArchive = Join-Path $motor 'node-v24.22.2-win-x64.zip'
$nodeInstall = 'C:\Program Files\nodejs'
try {
    Invoke-WebRequest 'https://nodejs.org/dist/v24.22.2/node-v24.22.2-win-x64.zip' -OutFile $nodeArchive -UseBasicParsing
    if ((Get-FileHash -LiteralPath $nodeArchive -Algorithm SHA256).Hash.ToLowerInvariant() -cne $nodeSha256) {
        throw 'Node archive SHA-256 mismatch'
    }
    if (Test-Path -LiteralPath $nodeInstall) { throw 'Node destination already exists; use a fresh provisioning image' }
    New-Item $nodeInstall -ItemType Directory | Out-Null
    & icacls $nodeInstall /inheritance:r /grant:r '*S-1-5-18:(OI)(CI)F' '*S-1-5-32-544:(OI)(CI)F' '*S-1-5-32-545:(OI)(CI)RX'
    if ($LASTEXITCODE -ne 0) { throw 'Node root ACL setup failed' }
    Expand-Archive -LiteralPath $nodeArchive -DestinationPath $nodeInstall
    Get-ChildItem -LiteralPath "$nodeInstall\node-v$nodeVersion-win-x64" -Force | Move-Item -Destination $nodeInstall
    Remove-Item -LiteralPath "$nodeInstall\node-v$nodeVersion-win-x64" -Force
    & icacls "$nodeInstall\*" /reset /T /C /L /Q
    if ($LASTEXITCODE -ne 0) { throw 'Node ACL inheritance reset failed' }
    & icacls $nodeInstall /setowner '*S-1-5-32-544' /T /C /L /Q
    if ($LASTEXITCODE -ne 0) { throw 'Node ownership setup failed' }
} finally {
    Remove-Item -LiteralPath $nodeArchive -Force -ErrorAction SilentlyContinue
}
$machinePath = [Environment]::GetEnvironmentVariable('Path', 'Machine')
if (($machinePath -split ';' | ForEach-Object { $_.TrimEnd('\') }) -notcontains $nodeInstall) {
    [Environment]::SetEnvironmentVariable('Path', "$nodeInstall;$machinePath", 'Machine')
}
$env:Path = "$nodeInstall;" + $env:Path
$installedNodeVersion = & node.exe --version
if ($LASTEXITCODE -ne 0 -or $installedNodeVersion -cne "v$nodeVersion") { throw 'Machine-wide Node version verification failed' }
# Select the cmd shim explicitly so PowerShell execution policy cannot hide npm.
$installedNpmVersion = & npm.cmd --version
if ($LASTEXITCODE -ne 0 -or $installedNpmVersion -cne $npmVersion) { throw 'Machine-wide npm version verification failed' }
Write-Host "Verified machine-wide Node $nodeVersion and npm $npmVersion with protected user read/execute access"

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
New-Item 'C:\motor-bun-seed','C:\motor-job' -ItemType Directory -Force | Out-Null
# Seeds inherit read-only access for the job; Bun writes only its disposable copy.
& icacls 'C:\motor-bun-seed' /inheritance:r /grant:r '*S-1-5-18:(OI)(CI)F' '*S-1-5-32-544:(OI)(CI)F' "*${jobSid}:(OI)(CI)RX"
if ($LASTEXITCODE -ne 0) { throw 'Bun seed ACL setup failed' }
& icacls 'C:\motor-bun-seed' /setowner '*S-1-5-32-544'
if ($LASTEXITCODE -ne 0) { throw 'Bun seed ownership setup failed' }
& icacls 'C:\motor-job' /inheritance:r /grant:r '*S-1-5-18:(OI)(CI)F' '*S-1-5-32-544:(OI)(CI)F' "*${jobSid}:(OI)(CI)M"
if ($LASTEXITCODE -ne 0) { throw 'Bun disposable cache ACL setup failed' }
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
$rustup = 'C:\rust\cargo\bin\rustup.exe'
& $rustup component add clippy rustfmt --toolchain $toolchain
if ($LASTEXITCODE -ne 0) { throw 'Job image Rust components failed to install' }
& $rustup target add aarch64-pc-windows-msvc --toolchain $toolchain
if ($LASTEXITCODE -ne 0) { throw 'Job image ARM64 Rust target failed to install' }
$components = & $rustup component list --installed --toolchain $toolchain
if ($LASTEXITCODE -ne 0) { throw 'Listing installed Rust components failed' }
foreach ($component in @('clippy', 'rustfmt')) {
    if ($components -notcontains "$component-$hostTriple") { throw "Missing Rust component: $component" }
}
$targets = & $rustup target list --installed --toolchain $toolchain
if ($LASTEXITCODE -ne 0) { throw 'Listing installed Rust targets failed' }
foreach ($target in @($hostTriple, 'aarch64-pc-windows-msvc')) {
    if ($targets -notcontains $target) { throw "Missing Rust target: $target" }
}
$components | Write-Host
$targets | Write-Host

# rustup reserves the stable name for official toolchains, so a custom link
# cannot provide it. Copy the complete pinned toolchain, including its component
# metadata and ARM64 target, so stable selection needs no channel download.
$stableInstall = Join-Path $env:RUSTUP_HOME "toolchains\stable-$hostTriple"
if (Test-Path -LiteralPath $stableInstall) { throw 'Rust stable destination already exists; use a fresh provisioning image' }
Copy-Item -LiteralPath "$env:RUSTUP_HOME\toolchains\$toolchain-$hostTriple" -Destination $stableInstall -Recurse
$stableRustc = & 'C:\rust\cargo\bin\rustc.exe' +stable -vV
if ($LASTEXITCODE -ne 0 -or $stableRustc -notcontains "release: $toolchain" -or $stableRustc -notcontains "host: $hostTriple") {
    throw 'Machine-wide Rust stable version/host verification failed'
}
$stableComponents = & $rustup component list --installed --toolchain stable
if ($LASTEXITCODE -ne 0 -or (Compare-Object $components $stableComponents)) { throw 'Rust stable components differ from pinned toolchain' }
$stableTargets = & $rustup target list --installed --toolchain stable
if ($LASTEXITCODE -ne 0 -or (Compare-Object $targets $stableTargets)) { throw 'Rust stable targets differ from pinned toolchain' }
Write-Host "Verified machine-wide Rust stable $toolchain with matching components and targets"

# Jobs are offline; install the verified Windows release beside Cargo before
# the toolchain tree is made read-only for the job user.
# SHA-256 source: https://github.com/cortexkit/aft/blob/main/.github/actions/setup-nextest/action.yml
$nextestVersion = '0.9.138'
$nextestSha256 = 'e4606198dd067299f16863dcf5b9be007b9cae041e468c417884fc224760c0f4'
$nextestArchive = Join-Path $motor 'cargo-nextest-x86_64-pc-windows-msvc.tar.gz'
try {
    Invoke-WebRequest 'https://github.com/nextest-rs/nextest/releases/download/cargo-nextest-0.9.138/cargo-nextest-0.9.138-x86_64-pc-windows-msvc.tar.gz' -OutFile $nextestArchive -UseBasicParsing
    if ((Get-FileHash -LiteralPath $nextestArchive -Algorithm SHA256).Hash.ToLowerInvariant() -cne $nextestSha256) {
        throw 'cargo-nextest archive SHA-256 mismatch'
    }
    & "$env:SystemRoot\System32\tar.exe" -xzf $nextestArchive -C 'C:\rust\cargo\bin' cargo-nextest.exe
    if ($LASTEXITCODE -ne 0) { throw 'cargo-nextest extraction failed' }
} finally {
    Remove-Item -LiteralPath $nextestArchive -Force -ErrorAction SilentlyContinue
}
# `cargo nextest --version` prints five lines (version, release, commit hash,
# commit date, host). PowerShell's -notmatch on that array returns the four
# non-matching lines, which is truthy, so only the first line is checked.
$nextestOutput = @(& 'C:\rust\cargo\bin\cargo.exe' nextest --version)
if ($LASTEXITCODE -ne 0 -or [string]($nextestOutput | Select-Object -First 1) -notmatch ('^cargo-nextest ' + [regex]::Escape($nextestVersion) + '(?:\s|$)')) {
    throw "Machine-wide cargo-nextest version verification failed: $($nextestOutput -join ' | ')"
}
Write-Host "Verified machine-wide $($nextestOutput | Select-Object -First 1)"

$vswhere = "${env:ProgramFiles(x86)}\Microsoft Visual Studio\Installer\vswhere.exe"
if (-not (Test-Path $vswhere)) { throw 'Visual Studio locator is missing' }
$vsInstall = & $vswhere -latest -products * -requires Microsoft.VisualStudio.Workload.VCTools Microsoft.VisualStudio.Component.VC.Tools.ARM64 -property installationPath
if ($LASTEXITCODE -ne 0 -or -not $vsInstall) { throw 'Visual Studio ARM64 tools are missing' }
$arm64Linker = Get-ChildItem "$vsInstall\VC\Tools\MSVC\*\bin\Hostx64\arm64\link.exe" -File | Select-Object -First 1
if (-not $arm64Linker) { throw 'MSVC Hostx64 ARM64 link.exe is missing' }
Write-Host "Verified ARM64 linker: $($arm64Linker.FullName)"

# A dependency-free executable exercises the ARM64 standard library, linker and
# Windows SDK without fetching crates or running an ARM64 binary on the x64 host.
$arm64Probe = Join-Path $env:TEMP 'motor-arm64-toolchain-probe'
if (Test-Path $arm64Probe) { Remove-Item $arm64Probe -Recurse -Force }
New-Item "$arm64Probe\src" -ItemType Directory -Force | Out-Null
try {
    Set-Content "$arm64Probe\Cargo.toml" @'
[package]
name = "motor-arm64-toolchain-probe"
version = "0.0.0"
edition = "2021"

[workspace]
'@ -Encoding ascii
    Set-Content "$arm64Probe\src\main.rs" 'fn main() { println!("ARM64 toolchain verified"); }' -Encoding ascii
    Push-Location $arm64Probe
    try {
        & 'C:\rust\cargo\bin\cargo.exe' "+$toolchain" build --offline --target aarch64-pc-windows-msvc --target-dir "$arm64Probe\target"
        if ($LASTEXITCODE -ne 0) { throw "Offline ARM64 Cargo build failed: $LASTEXITCODE" }
        if (-not (Test-Path "$arm64Probe\target\aarch64-pc-windows-msvc\debug\motor-arm64-toolchain-probe.exe")) {
            throw 'Offline ARM64 Cargo build did not produce an executable'
        }
    } finally {
        Pop-Location
    }
} finally {
    Remove-Item $arm64Probe -Recurse -Force
}
$git = 'C:\Program Files\Git\cmd\git.exe'
if (-not (Test-Path 'C:\Program Files\Git\bin\bash.exe')) { throw 'Git Bash is missing' }
& $git --version
if ($LASTEXITCODE -ne 0) { throw 'Machine-wide Git verification failed' }
& $git config --system core.longpaths true
if ($LASTEXITCODE -ne 0) { throw 'Git long-path configuration failed' }
New-ItemProperty 'HKLM:\SYSTEM\CurrentControlSet\Control\FileSystem' -Name LongPathsEnabled -PropertyType DWord -Value 1 -Force | Out-Null
foreach ($path in @('C:\m', 'C:\rust', $jobCargo)) { Add-MpPreference -ExclusionPath $path }

$prebuiltHvsock = Test-Path "$PSScriptRoot\hyperv-runner"
if ($prebuiltHvsock) {
    if (-not $HyperVSpike) { throw 'Prebuilt hvsock agent requires Hyper-V provisioning mode' }
    $prebuilt = "$PSScriptRoot\motor-winagent-prebuilt.exe"
    $info = & $prebuilt --build-info
    if ($LASTEXITCODE -ne 0 -or $info -notmatch 'transport=hvsock' -or $info -notmatch 'nonce=dvd') { throw 'Prebuilt agent is not a sealed-runner hvsock/DVD-nonce build' }
    Copy-Item $prebuilt "$motor\motor-winagent.exe" -Force
    Copy-Item "$PSScriptRoot\motor-runner-prebuilt.exe" "$motor\motor-peer.exe" -Force
    # This task performs no action on normal job media. Qualification media asks
    # it to contact a victim listener before any parent connection is admitted.
    $action = New-ScheduledTaskAction -Execute "$motor\motor-peer.exe" -Argument 'probe-peers'
    Register-ScheduledTask -TaskName MotorPeerQualification -Action $action -Trigger (New-ScheduledTaskTrigger -AtStartup) -User SYSTEM -RunLevel Highest -Force | Out-Null
} else {
foreach ($name in @('Cargo.toml', 'Cargo.lock', 'src')) {
    if (-not (Test-Path (Join-Path $AgentSource $name))) { throw "Standalone agent source is missing $name" }
}
# Build with the pinned toolchain and lockfile inside this MSVC-equipped seed VM.
$agentOptions = @()
if ($HyperVSpike) { $agentOptions = @('--features', 'hyperv-spike') }
& 'C:\rust\cargo\bin\cargo.exe' build --release --locked --manifest-path "$AgentSource\Cargo.toml" --target $hostTriple @agentOptions
if ($LASTEXITCODE -ne 0) { throw "Agent release build failed: $LASTEXITCODE" }
Copy-Item "$AgentSource\target\$hostTriple\release\motor-winagent.exe" "$motor\motor-winagent.exe" -Force
}
$agentHash = (Get-FileHash "$motor\motor-winagent.exe" -Algorithm SHA256).Hash
Set-Content "$PSScriptRoot\winagent.sha256" "$agentHash  motor-winagent.exe" -Encoding ascii
Write-Host "Installed motor-winagent SHA-256: $agentHash"
if ($HyperVSpike -and -not $prebuiltHvsock) {
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
