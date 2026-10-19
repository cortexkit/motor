<# Build only the trusted host probe with an already installed Rust/MSVC toolchain.
No guest image, guest agent, toolchain installation or Windows policy is changed. #>
param([string]$Root = 'C:\motor-spike\hyperv', [string]$Cargo = 'cargo')
. "$PSScriptRoot\common.ps1"
$Root = [IO.Path]::GetFullPath($Root)
$lock = $null
try {
    $lock = Enter-Spike $Root
    Assert-Spike (Test-Path (Join-Path $Root 'sealed.json')) 'existing-sealed-baseline' $Root
    $manifest = (Resolve-Path "$PSScriptRoot\..\..\crates\motor-winagent\Cargo.toml").Path
    & $Cargo --version | Out-Host
    Assert-Spike ($LASTEXITCODE -eq 0) 'existing-host-cargo' $Cargo
    $oldWrapper = $env:RUSTC_WRAPPER
    $oldBuildWrapper = $env:CARGO_BUILD_RUSTC_WRAPPER
    try {
        $env:RUSTC_WRAPPER = ''; $env:CARGO_BUILD_RUSTC_WRAPPER = ''
        & $Cargo build --release --locked --manifest-path $manifest --target x86_64-pc-windows-msvc --example hyperv-spike --target-dir (Join-Path $Root 'probe-build') -j 2 | Out-Host
        Assert-Spike ($LASTEXITCODE -eq 0) 'host-probe-built' $manifest
    } finally {
        $env:RUSTC_WRAPPER = $oldWrapper; $env:CARGO_BUILD_RUSTC_WRAPPER = $oldBuildWrapper
    }
    $destination = Join-Path $Root 'hyperv-spike-trace.exe'
    Copy-Item (Join-Path $Root 'probe-build\x86_64-pc-windows-msvc\release\examples\hyperv-spike.exe') $destination -Force
    $version = & $destination --version
    Assert-Spike ($LASTEXITCODE -eq 0 -and $version -match 'protocol 4 trace=1') 'traced-host-probe' "$version SHA256=$((Get-FileHash $destination).Hash)"
    Write-Host "PASS measurement-only-client path=$destination (parent and guest agent untouched)"
} finally { if ($lock) { $lock.Dispose() } }
