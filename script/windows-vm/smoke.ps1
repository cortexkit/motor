# Run through SSH on the guest; exercises native x64 MSVC compilation and Git clone.
$ErrorActionPreference = 'Stop'
$ProgressPreference = 'SilentlyContinue'
Start-Transcript C:\build\provision\smoke.log -Append
try {
    $all = [Diagnostics.Stopwatch]::StartNew()
    foreach ($tool in @('cargo','bun','git')) {
        & $tool --version
        if ($LASTEXITCODE -ne 0) { throw "$tool failed" }
    }
    & rustc -vV
    if ($LASTEXITCODE -ne 0) { throw 'rustc failed' }
    . C:\build\provision\dev-shell.ps1
    $work = 'C:\build\vm-smoke'
    if (Test-Path $work) { throw 'Smoke directory already exists; inspect before removing it' }
    New-Item "$work\source\src" -ItemType Directory -Force | Out-Null
    'int main(void) { return 0; }' | Set-Content "$work\smoke.c" -Encoding ascii
    & cl "$work\smoke.c" "/Fe$work\smoke.exe" "/Fo$work\smoke.obj" "/Fd$work\smoke.pdb"
    if ($LASTEXITCODE -ne 0) { throw 'cl compilation failed' }
    & "$work\smoke.exe"
    if ($LASTEXITCODE -ne 0) { throw 'MSVC executable failed' }
    @'
[package]
name = "windows-vm-smoke"
version = "0.1.0"
edition = "2021"
'@ | Set-Content "$work\source\Cargo.toml" -Encoding ascii
    @'
#[cfg(test)]
mod tests {
    #[test]
    fn native_windows_x64_runtime() {
        assert_eq!(std::env::consts::OS, "windows");
        assert_eq!(std::env::consts::ARCH, "x86_64");
        assert_eq!(std::mem::size_of::<usize>(), 8);
    }
    #[test]
    fn windows_temp_file_roundtrip() {
        let path = std::env::temp_dir().join(format!("vm-smoke-{}.txt", std::process::id()));
        std::fs::write(&path, "x64 MSVC guest").unwrap();
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "x64 MSVC guest");
        std::fs::remove_file(path).unwrap();
    }
}
'@ | Set-Content "$work\source\src\lib.rs" -Encoding ascii
    & git -C "$work\source" init
    if ($LASTEXITCODE -ne 0) { throw 'git init failed' }
    & git -C "$work\source" add Cargo.toml src/lib.rs
    if ($LASTEXITCODE -ne 0) { throw 'git add failed' }
    & git -C "$work\source" -c user.name='VM smoke' -c user.email='vm-smoke@localhost' commit -m 'Local Windows smoke fixture'
    if ($LASTEXITCODE -ne 0) { throw 'git commit failed' }
    $clone = [Diagnostics.Stopwatch]::StartNew()
    & git clone "$work\source" "$work\clone"
    if ($LASTEXITCODE -ne 0) { throw 'git clone failed' }
    Write-Host ('Git clone elapsed: {0:N3} s' -f $clone.Elapsed.TotalSeconds)
    $test = [Diagnostics.Stopwatch]::StartNew()
    Push-Location "$work\clone"
    try {
        & cargo test --offline
        if ($LASTEXITCODE -ne 0) { throw 'cargo test failed' }
    } finally { Pop-Location }
    Write-Host ('cargo test elapsed: {0:N3} s' -f $test.Elapsed.TotalSeconds)
    Write-Host ('Total guest smoke elapsed: {0:N3} s' -f $all.Elapsed.TotalSeconds)
    Remove-Item $work -Recurse -Force
} finally { Stop-Transcript }
