# Managed native processes keep exit handles and treat stderr as data, not PS errors.
function Convert-SpikeNativeArgument([string]$Value) {
    if ($Value.Contains([char]0)) { throw 'Native argument contains NUL' }
    $result = [Text.StringBuilder]::new()
    [void]$result.Append('"')
    $slashes = 0
    foreach ($character in $Value.ToCharArray()) {
        if ($character -eq '\') { $slashes++; continue }
        if ($character -eq '"') {
            [void]$result.Append(('\' * (2 * $slashes + 1)))
        } else { [void]$result.Append(('\' * $slashes)) }
        [void]$result.Append($character)
        $slashes = 0
    }
    [void]$result.Append(('\' * (2 * $slashes)))
    [void]$result.Append('"')
    return $result.ToString()
}

function Start-SpikeNativeProcess([string]$Executable, [string[]]$Arguments) {
    $process = New-Object Diagnostics.Process
    $process.StartInfo.FileName = $Executable
    $process.StartInfo.Arguments = (@($Arguments | ForEach-Object { Convert-SpikeNativeArgument $_ }) -join ' ')
    $process.StartInfo.WorkingDirectory = (Get-Location).ProviderPath
    $process.StartInfo.UseShellExecute = $false
    $process.StartInfo.CreateNoWindow = $true
    $process.StartInfo.RedirectStandardOutput = $true
    $process.StartInfo.RedirectStandardError = $true
    $process.StartInfo.StandardOutputEncoding = [Text.UTF8Encoding]::new($false)
    $process.StartInfo.StandardErrorEncoding = [Text.UTF8Encoding]::new($false)
    $started = $false
    try {
        $start = [DateTimeOffset]::UtcNow.ToUnixTimeMilliseconds()
        $started = $process.Start()
        if (-not $started) { throw "Native process did not start: $Executable" }
        # Retain the original process handle until after ExitCode is captured.
        # Reopening an already exited process by PID loses its status on PS 5.1.
        $null = $process.Handle
        return [pscustomobject]@{ process=$process; stdout=$process.StandardOutput.ReadToEndAsync(); stderr=$process.StandardError.ReadToEndAsync(); started_unix_ms=$start }
    } catch {
        if ($started -and -not $process.HasExited) { $process.Kill(); $process.WaitForExit() }
        $process.Dispose()
        throw
    }
}

function Complete-SpikeNativeProcess($Running, [string]$StdoutPath, [string]$StderrPath) {
    if (-not $Running.process.HasExited) { throw 'Native process has not exited' }
    $Running.process.WaitForExit()
    $rawCode = $Running.process.ExitCode
    if ($null -eq $rawCode) { throw 'Native process exit status is missing' }
    $code = [int]$rawCode
    if (-not $Running.stdout.Wait(10000) -or -not $Running.stderr.Wait(10000)) { throw 'Native output pipes did not close after process exit' }
    $output = [pscustomobject]@{ exitCode=$code; stdout=$Running.stdout.Result; stderr=$Running.stderr.Result }
    if ($StdoutPath) { [IO.File]::WriteAllText($StdoutPath, $output.stdout) }
    if ($StderrPath) { [IO.File]::WriteAllText($StderrPath, $output.stderr) }
    return $output
}

function Close-SpikeNativeProcess($Running) {
    if ($null -eq $Running) { return }
    try {
        if (-not $Running.process.HasExited) { $Running.process.Kill(); $Running.process.WaitForExit() }
    } finally { $Running.process.Dispose() }
}

function Invoke-SpikeNative([string]$Executable, [string[]]$Arguments, [string]$LogDirectory,
    [ValidatePattern('^[a-z0-9-]+$')][string]$Label, [int]$TimeoutMs = 600000) {
    $running = Start-SpikeNativeProcess $Executable $Arguments
    try {
        $finished = $running.process.WaitForExit($TimeoutMs)
        if (-not $finished) { $running.process.Kill(); $running.process.WaitForExit() }
        $output = Complete-SpikeNativeProcess $running (Join-Path $LogDirectory "$Label.stdout.log") (Join-Path $LogDirectory "$Label.stderr.log")
        Write-Host "EVIDENCE native-$Label exit=$($output.exitCode) stdout:`n$($output.stdout)"
        Write-Host "EVIDENCE native-$Label stderr (data):`n$($output.stderr)"
        if (-not $finished) { throw "Native process timed out: $Label" }
        return $output
    } finally { Close-SpikeNativeProcess $running }
}
