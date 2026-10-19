<# Build trusted spike executables with an already installed Rust/MSVC toolchain.
The optional agent is an overlay override; no sealed image or Windows policy is changed. #>
param([string]$Root = 'C:\motor-spike\hyperv', [string]$Cargo = 'cargo', [switch]$WithAgent)
. "$PSScriptRoot\common.ps1"
$Root = [IO.Path]::GetFullPath($Root)
$lock = $null
try {
    $lock = Enter-Spike $Root
    Assert-Spike (Test-Path (Join-Path $Root 'sealed.json')) 'existing-sealed-baseline' $Root
    $manifest = (Resolve-Path "$PSScriptRoot\..\..\crates\motor-winagent\Cargo.toml").Path
    $version = Invoke-SpikeNative $Cargo @('--version') $Root 'cargo-version'
    Assert-Spike ($version.exitCode -eq 0) 'existing-host-cargo' $Cargo
    $oldWrapper = $env:RUSTC_WRAPPER
    $oldBuildWrapper = $env:CARGO_BUILD_RUSTC_WRAPPER
    try {
        $env:RUSTC_WRAPPER = ''; $env:CARGO_BUILD_RUSTC_WRAPPER = ''
        $built = Invoke-SpikeNative $Cargo @('build','--release','--locked','--manifest-path',$manifest,'--target','x86_64-pc-windows-msvc','--example','hyperv-spike','--target-dir',(Join-Path $Root 'probe-build'),'-j','2') $Root 'cargo-build'
        Assert-Spike ($built.exitCode -eq 0) 'host-probe-built' $manifest
        if ($WithAgent) {
            $agentBuild = Invoke-SpikeNative $Cargo @('build','--release','--locked','--manifest-path',$manifest,'--target','x86_64-pc-windows-msvc','--features','hyperv-spike','--bin','motor-winagent','--target-dir',(Join-Path $Root 'probe-build'),'-j','2') $Root 'agent-build'
            Assert-Spike ($agentBuild.exitCode -eq 0) 'guest-agent-override-built' $manifest
            $agent = Join-Path $Root 'motor-winagent-hyperv.exe'
            Copy-Item (Join-Path $Root 'probe-build\x86_64-pc-windows-msvc\release\motor-winagent.exe') $agent -Force
            $identity = Invoke-SpikeNative $agent @('--build-info') $Root 'agent-build-info'
            Assert-Spike ($identity.exitCode -eq 0 -and $identity.stdout -match 'hyperv-spike agent_diag=3') 'guest-agent-diagnostic-feature' $identity.stdout.Trim()
            Write-Host "EVIDENCE guest-agent-override path=$agent SHA256=$((Get-FileHash $agent).Hash) (not installed in parent)"
        }
    } finally {
        $env:RUSTC_WRAPPER = $oldWrapper; $env:CARGO_BUILD_RUSTC_WRAPPER = $oldBuildWrapper
    }
    $destination = Join-Path $Root 'hyperv-spike-trace.exe'
    Copy-Item (Join-Path $Root 'probe-build\x86_64-pc-windows-msvc\release\examples\hyperv-spike.exe') $destination -Force
    $version = Invoke-SpikeNative $destination @('--version') $Root 'probe-version'
    Assert-Spike ($version.exitCode -eq 0 -and $version.stdout -match 'protocol 4 trace=3') 'traced-host-probe' "$($version.stdout.Trim()) SHA256=$((Get-FileHash $destination).Hash)"
    Write-Host "PASS measurement-only-client path=$destination (parent and guest agent untouched)"
} finally { if ($lock) { $lock.Dispose() } }
