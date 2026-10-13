# Adapted from the AFT Windows VM setup script for a Rust/Bun builder.
# One-time x64 Windows Server builder setup; run elevated via bootstrap.ps1.
# Server has no winget. Direct official installers replace its Git/VS fallback.
# Node, OpenCode, aimock and PowerShell 7 are not needed for a Rust/Bun builder.

param([switch]$JobBaseline)

$ErrorActionPreference = "Stop"

Write-Host "============================================"
Write-Host "  Windows VM one-time setup"
Write-Host "============================================"
Write-Host ""

# Sanity check -- must run as admin for Defender exclusions and system installers.
$IsAdmin = ([Security.Principal.WindowsPrincipal] [Security.Principal.WindowsIdentity]::GetCurrent()).IsInRole([Security.Principal.WindowsBuiltInRole] "Administrator")
if (-not $IsAdmin) {
    Write-Host "ERROR: This script must run as Administrator." -ForegroundColor Red
    Write-Host "       Right-click PowerShell -> Run as administrator, then re-run." -ForegroundColor Red
    exit 1
}

# -----------------------------------------------------------------------------
# Defender exclusions FIRST -- every step below benefits from these.
# -----------------------------------------------------------------------------
Write-Host "-- Configuring Windows Defender exclusions --"
$Exclusions = @(
    "C:\build",
    "$env:USERPROFILE\.cargo",
    "$env:USERPROFILE\.rustup",
    "$env:USERPROFILE\.bun"
)
foreach ($path in $Exclusions) {
    Add-MpPreference -ExclusionPath $path -ErrorAction Stop
    Write-Host "  + $path"
}
Write-Host ""

# -----------------------------------------------------------------------------
# winget: prefer when available; fall back to direct downloads otherwise.
# -----------------------------------------------------------------------------
$HasWinget = $null -ne (Get-Command winget -ErrorAction SilentlyContinue)

# -----------------------------------------------------------------------------
# Git
# -----------------------------------------------------------------------------
if ($null -eq (Get-Command git -ErrorAction SilentlyContinue)) {
    Write-Host "-- Installing Git --"
    if ($HasWinget) {
        & winget install --id Git.Git -e --source winget --accept-source-agreements --accept-package-agreements
    } else {
        $release = Invoke-RestMethod 'https://api.github.com/repos/git-for-windows/git/releases/latest'
        $asset = $release.assets | Where-Object { $_.name -match '^Git-.*-64-bit\.exe$' } | Select-Object -First 1
        if (-not $asset) { throw 'No official Git for Windows x64 installer found' }
        Invoke-WebRequest $asset.browser_download_url -OutFile "$env:TEMP\git-install.exe" -UseBasicParsing
        $p = Start-Process "$env:TEMP\git-install.exe" -ArgumentList '/VERYSILENT /NORESTART /SP- /SUPPRESSMSGBOXES' -Wait -PassThru
        if ($p.ExitCode -ne 0) { throw "Git install failed: $($p.ExitCode)" }
        $env:Path = [Environment]::GetEnvironmentVariable('Path','Machine') + ';' + [Environment]::GetEnvironmentVariable('Path','User')
    }
} else {
    Write-Host "Git already installed: $((& git --version))"
}
Write-Host ""


# -----------------------------------------------------------------------------
# Bun (PowerShell installer is the official path)
# -----------------------------------------------------------------------------
if ($null -eq (Get-Command bun -ErrorAction SilentlyContinue)) {
    Write-Host "-- Installing Bun --"
    Invoke-RestMethod -Uri "https://bun.sh/install.ps1" -OutFile "$env:TEMP\install-bun.ps1"
    & powershell -ExecutionPolicy Bypass -File "$env:TEMP\install-bun.ps1"
    if ($LASTEXITCODE -ne 0) { throw "Bun installer failed: $LASTEXITCODE" }
    # Bun installs to $env:USERPROFILE\.bun\bin\bun.exe
    $BunPath = Join-Path $env:USERPROFILE ".bun\bin"
    if (Test-Path (Join-Path $BunPath "bun.exe")) {
        # Add to user PATH permanently
        $UserPath = [System.Environment]::GetEnvironmentVariable("Path", "User")
        if ($UserPath -notlike "*$BunPath*") {
            [System.Environment]::SetEnvironmentVariable("Path", "$UserPath;$BunPath", "User")
        }
        $env:Path = "$env:Path;$BunPath"
        Write-Host "  Bun installed to $BunPath"
    } else {
        throw 'Bun installer returned without bun.exe'
    }
} else {
    Write-Host "Bun already installed: $((& bun --version))"
}
Write-Host ""

# -----------------------------------------------------------------------------
# Visual Studio Build Tools (C++ workload)
#
# Required for the Rust MSVC toolchain. Without it, cargo build fails at the
# linker step with:
#   "error: linker `link.exe` not found ... please ensure that Visual Studio
#    2017 or later, or Build Tools for Visual Studio were installed with the
#    Visual C++ option"
# This is a 3-7 GB install but it's the canonical Rust-on-Windows setup;
# alternatives (rustup gnu toolchain) produce non-MSVC binaries that don't
# match the MSVC binaries this builder produces.
#
# Detection: Visual Studio Locator (vswhere.exe) ships with VS2017+ Build
# Tools and is the official way to detect installed VS instances.
# -----------------------------------------------------------------------------
$VSWherePath = "${env:ProgramFiles(x86)}\Microsoft Visual Studio\Installer\vswhere.exe"
$HasVCTools = $false
if (Test-Path $VSWherePath) {
    # -requires Microsoft.VisualStudio.Component.VC.Tools.* checks for the
    # C++ build tools workload specifically (any architecture variant).
    $vsInstance = & $VSWherePath -latest -products * `
        -requires Microsoft.VisualStudio.Workload.VCTools `
        -property installationPath 2>$null
    if ($vsInstance) {
        $HasVCTools = $true
        Write-Host "Visual Studio Build Tools (VC++) already installed at: $vsInstance"
    }
}

if (-not $HasVCTools) {
    Write-Host "-- Installing Visual Studio 2022 Build Tools (VC++ workload) --"
    Write-Host "   This is large (~3-7 GB) and may take 10-20 minutes."
    if ($HasWinget) {
        # --override is the documented way to pass arguments through winget to
        # the VS installer. We use --quiet --wait so winget doesn't return
        # before the install actually finishes (otherwise rustup runs against
        # a half-installed toolchain and we hit the linker error anyway).
        #
        # IMPORTANT: on ARM64 Windows, Microsoft's --includeRecommended for the
        # VCTools workload installs ONLY the x64-host C++ toolchain, NOT the
        # ARM64-native compiler. That's enough to satisfy vswhere ("VCTools
        # workload installed") but NOT to actually link an aarch64-msvc Rust
        # build, because cargo defaults to the host triple
        # (aarch64-pc-windows-msvc) and looks for Hostarm64\arm64\link.exe,
        # which only ships with the explicit VC.Tools.ARM64 component.
        # We add it unconditionally; on x64 hosts it's a small no-op
        # extra, on ARM64 hosts it's the difference between cargo working
        # and the "linker `link.exe` not found" error.
        $vsArgs = @(
            "--quiet"
            "--wait"
            "--add"
            "Microsoft.VisualStudio.Workload.VCTools"
            "--add"
            "Microsoft.VisualStudio.Component.VC.Tools.ARM64"
            "--includeRecommended"
        ) -join " "
        & winget install --id Microsoft.VisualStudio.2022.BuildTools -e `
            --source winget `
            --accept-source-agreements --accept-package-agreements `
            --override $vsArgs
        if ($LASTEXITCODE -ne 0) {
            Write-Host "  VS Build Tools install failed (exit $LASTEXITCODE)" -ForegroundColor Red
            Write-Host "  Cargo builds will fail at the linker step until this is resolved." -ForegroundColor Red
            exit 1
        }
    } else {
        Invoke-WebRequest 'https://aka.ms/vs/17/release/vs_buildtools.exe' -OutFile "$env:TEMP\vs_buildtools.exe" -UseBasicParsing
        $p = Start-Process "$env:TEMP\vs_buildtools.exe" -ArgumentList '--quiet --wait --norestart --nocache --installPath C:\BuildTools --add Microsoft.VisualStudio.Workload.VCTools --includeRecommended' -Wait -PassThru
        if ($p.ExitCode -notin @(0,3010)) { throw "VS install failed: $($p.ExitCode)" }
        $vsInstance = & $VSWherePath -latest -products * -requires Microsoft.VisualStudio.Workload.VCTools -property installationPath
        if (-not $vsInstance) { throw 'VS installer returned without a VC++ workload' }
    }
    Write-Host "  VS Build Tools installed."
}
Write-Host ""

# -----------------------------------------------------------------------------
# Rust toolchain via rustup
# -----------------------------------------------------------------------------
if ($JobBaseline) {
    Write-Host 'Job baseline: machine-wide pinned Rust is installed by setup-job-baseline.ps1.'
} elseif ($null -eq (Get-Command cargo -ErrorAction SilentlyContinue)) {
    Write-Host "-- Installing Rust (rustup) --"
    # Pick the right rustup-init based on the actual VM architecture.
    # Apple Silicon hosts will run ARM64 Windows; Intel hosts run x64.
    $rustupArch = if ($env:PROCESSOR_ARCHITECTURE -eq "ARM64") { "aarch64" } else { "x86_64" }
    $rustupUrl = "https://win.rustup.rs/$rustupArch"
    Write-Host "  detected arch: $env:PROCESSOR_ARCHITECTURE -> rustup-init for $rustupArch"
    Invoke-WebRequest -Uri $rustupUrl -OutFile "$env:TEMP\rustup-init.exe"
    # -y = non-interactive, --default-toolchain stable
    & "$env:TEMP\rustup-init.exe" -y --default-toolchain stable --default-host x86_64-pc-windows-msvc --no-modify-path
    if ($LASTEXITCODE -ne 0) { throw "rustup failed: $LASTEXITCODE" }
    # Update PATH for current session
    $CargoPath = Join-Path $env:USERPROFILE ".cargo\bin"
    $env:Path = "$env:Path;$CargoPath"
    # Persist to user PATH
    $UserPath = [System.Environment]::GetEnvironmentVariable("Path", "User")
    if ($UserPath -notlike "*$CargoPath*") {
        [System.Environment]::SetEnvironmentVariable("Path", "$UserPath;$CargoPath", "User")
    }
    Write-Host "  Rust installed: $((& cargo --version))"
} else {
    Write-Host "Rust already installed: $((& cargo --version))"
}
Write-Host ""

# Verify the actual binaries rather than treating installer success as proof.
$verifyTools = if ($JobBaseline) { @('git', 'bun') } else { @('git', 'bun', 'cargo') }
foreach ($tool in $verifyTools) {
    & $tool --version
    if ($LASTEXITCODE -ne 0) { throw "$tool verification failed" }
}
if (-not $JobBaseline) {
    & rustc -vV
    if ($LASTEXITCODE -ne 0) { throw 'rustc verification failed' }
}
Write-Host 'Builder toolchain setup complete.'
