# Run inside a disposable job VM as the standard job account, never on the host.
$ErrorActionPreference = 'Stop'
$identity = [Security.Principal.WindowsIdentity]::GetCurrent()
$principal = [Security.Principal.WindowsPrincipal]::new($identity)
if ($identity.Name -notmatch '\\motor-job$' -or $principal.IsInRole([Security.Principal.WindowsBuiltInRole]::Administrator)) { throw 'Crash dump probe requires the standard job account' }
$name = 'motor-crash-' + [Guid]::NewGuid().ToString('N')
# Assembly forces a real null write, not a Rust precondition panic or optimized-away store.
Set-Content -LiteralPath "$name.rs" -Encoding ascii -Value 'fn main() { unsafe { core::arch::asm!("mov byte ptr [{p}], 1", p = in(reg) 0usize, options(nostack)); } }'
& rustc --edition=2021 -C debuginfo=1 "$name.rs" -o "$name.exe"
if ($LASTEXITCODE -ne 0) { throw 'Crash dump probe compilation failed' }
$process = Start-Process -FilePath (Join-Path $PWD "$name.exe") -PassThru
if (-not $process.WaitForExit(30000)) { throw 'Crash dump probe did not exit within 30 seconds' }
if ($process.ExitCode -ne -1073741819) { throw "Expected 0xC0000005, got $($process.ExitCode)" }
# WER writes asynchronously; a unique executable name prevents stale dumps passing.
$deadline = [DateTime]::UtcNow.AddSeconds(30)
do {
    $dumps = @(Get-ChildItem -LiteralPath 'C:\CrashDumps' -Filter "$name.exe.$($process.Id).dmp" -File)
    foreach ($dump in $dumps) {
        $stream = $null
        try {
            $stream = [IO.File]::Open($dump.FullName, 'Open', 'Read', 'ReadWrite')
            if ($stream.Length -gt 0 -and $stream.ReadByte() -ge 0) {
                Write-Output "PASS job-crash-dump readable non-empty minidump: $($dump.FullName) bytes=$($stream.Length)"
                exit 0
            }
        } catch [IO.IOException] {
            # WER may still have the file open; retry within the same deadline.
        } finally {
            if ($stream) { $stream.Dispose() }
        }
    }
    Start-Sleep -Milliseconds 200
} while ([DateTime]::UtcNow -lt $deadline)
throw 'No readable non-empty crash dump appeared within 30 seconds'
