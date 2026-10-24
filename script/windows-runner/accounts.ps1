# SID-based local policy edits. Preserve every unrelated trustee and assignment.

# secedit /export lists a local account by its name, not its SID, so a trustee
# is resolved to a SID before any comparison. Tests replace the name lookup.
$script:RunnerResolveTrusteeName = {
    param($name)
    ([Security.Principal.NTAccount]::new($name)).Translate([Security.Principal.SecurityIdentifier]).Value
}
function Resolve-RunnerTrusteeSid([string]$Trustee) {
    $Trustee = $Trustee.Trim()
    if ($Trustee -match '^\*?(S-\d+(?:-\d+)+)$') { return $Matches[1] }
    try { $sid = & $script:RunnerResolveTrusteeName $Trustee } catch { throw "Cannot resolve policy trustee '$Trustee': $_" }
    if ($sid -notmatch '^S-\d+(?:-\d+)+$') { throw "Cannot resolve policy trustee '$Trustee'" }
    $sid
}
function Remove-RunnerBatchRights([string]$Root, [string]$Sid) {
    if ($Sid -notmatch '^S-1-5-21-(\d+-){3}\d+$') { throw 'Expected the owned local account SID' }
    $policy = "$Root\rights.inf"
    $database = "$Root\rights.sdb"
    $rights = @('SeBatchLogonRight','SeDenyInteractiveLogonRight','SeDenyRemoteInteractiveLogonRight','SeDenyNetworkLogonRight','SeDenyServiceLogonRight')
    try {
        & secedit.exe /export /cfg $policy /areas USER_RIGHTS /quiet
        if ($LASTEXITCODE -ne 0) { throw 'Cannot export local user rights' }
        $lines = @(Get-Content -LiteralPath $policy)
        foreach ($right in $rights) {
            $pattern = '^' + $right + '\s*='
            $entry = @($lines | Where-Object { $_ -match $pattern })
            if ($entry.Count -gt 1) { throw "Duplicate assignment $right" }
            $grants = @()
            if (-not $entry.Count) { continue }
            $grants = @(($entry[0] -split '=',2)[1] -split ',' | ForEach-Object { $_.Trim() } | Where-Object { $_ -and (Resolve-RunnerTrusteeSid $_) -ne $Sid })
            $lines = @($lines | ForEach-Object { if ($_ -match $pattern) { "$right = " + ($grants -join ',') } else { $_ } })
        }
        $lines | Set-Content -LiteralPath $policy -Encoding Unicode
        & secedit.exe /configure /db $database /cfg $policy /areas USER_RIGHTS /quiet
        if ($LASTEXITCODE -ne 0) { throw 'Cannot remove obsolete local account rights' }
        & secedit.exe /export /cfg $policy /areas USER_RIGHTS /quiet
        if ($LASTEXITCODE -ne 0) { throw 'Cannot verify local user rights cleanup' }
        foreach ($right in $rights) {
            $line = @(Get-Content -LiteralPath $policy | Where-Object { $_ -match ('^' + $right + '\s*=') })
            $present = $line.Count -eq 1 -and @((($line[0] -split '=',2)[1] -split ',') | Where-Object { $_.Trim() -and (Resolve-RunnerTrusteeSid $_) -eq $Sid }).Count -eq 1
            if ($present) { throw "Effective $right still contains obsolete SID" }
        }
    } finally { Remove-Item $policy,$database -Force -ErrorAction SilentlyContinue }
}
function Remove-RunnerPrepAccount([string]$Root, [string]$Sid) {
    if ($Sid -notmatch '^S-1-5-21-(\d+-){3}\d+$') { throw 'Expected the owned motor-prep SID' }
    $account = Get-LocalUser motor-prep -ErrorAction SilentlyContinue
    if ($account -and $account.SID.Value -ne $Sid) { throw 'motor-prep SID changed; refusing legacy account cleanup' }
    Remove-RunnerBatchRights $Root $Sid
    $profiles = @(Get-CimInstance Win32_UserProfile | Where-Object SID -eq $Sid)
    if ($profiles.Count -gt 1) { throw 'Ambiguous motor-prep profile ownership' }
    if ($profiles.Count -eq 1) {
        $profile = $profiles[0]
        if ($profile.LocalPath -notmatch '^C:\\Users\\motor-prep(?:\.[^\\]+)?$' -or $profile.Loaded) {
            throw 'motor-prep profile path changed or is still loaded; preserving it'
        }
        $path = [string]$profile.LocalPath
        if ((Test-Path -LiteralPath $path) -and ((Get-Item -LiteralPath $path -Force).Attributes -band [IO.FileAttributes]::ReparsePoint)) {
            throw 'Reparse motor-prep profile refused'
        }
        $profile | Remove-CimInstance
        if (Test-Path -LiteralPath $path) { throw 'motor-prep profile cleanup did not complete' }
    }
    if (Test-Path -LiteralPath "$Root\prep-secret.dpapi") {
        Remove-Item -LiteralPath "$Root\prep-secret.dpapi" -Force
    }
    if ($account) { Remove-LocalUser motor-prep }
}
function Protect-RunnerPath {
    param([string]$Path, [string]$LogPath)
    if (-not $LogPath) { $LogPath = Join-Path $Path 'native-command.jsonl' }
    $null = Invoke-RunnerNative -FilePath "$env:SystemRoot\System32\icacls.exe" -ArgumentList @(
        $Path, '/inheritance:r', '/grant:r', '*S-1-5-18:(OI)(CI)F', '*S-1-5-32-544:(OI)(CI)F', '/Q'
    ) -Operation "protect runner path $Path" -LogPath $LogPath
    # On Windows client editions a directory an elevated administrator creates
    # is owned by that user, not by Administrators, and the runner refuses a
    # private directory with any other owner. icacls can't set the owner in the
    # same call as the grants.
    $null = Invoke-RunnerNative -FilePath "$env:SystemRoot\System32\icacls.exe" -ArgumentList @(
        $Path, '/setowner', '*S-1-5-32-544', '/Q'
    ) -Operation "set runner path owner $Path" -LogPath $LogPath
}

function ConvertTo-RunnerNativeArgument {
    param([AllowNull()][string]$Argument)
    if ($null -eq $Argument -or $Argument.Length -eq 0) { return '""' }
    if ($Argument -notmatch '[\s"]') { return $Argument }
    $builder = [Text.StringBuilder]::new()
    [void]$builder.Append([char]34)
    $backslashes = 0
    foreach ($character in $Argument.ToCharArray()) {
        if ($character -eq [char]92) { $backslashes++; continue }
        if ($character -eq [char]34) {
            for ($index = 0; $index -lt ($backslashes * 2 + 1); $index++) { [void]$builder.Append([char]92) }
            [void]$builder.Append([char]34)
        } else {
            for ($index = 0; $index -lt $backslashes; $index++) { [void]$builder.Append([char]92) }
            [void]$builder.Append($character)
        }
        $backslashes = 0
    }
    for ($index = 0; $index -lt ($backslashes * 2); $index++) { [void]$builder.Append([char]92) }
    [void]$builder.Append([char]34)
    return $builder.ToString()
}

function Invoke-RunnerNative {
    param(
        [Parameter(Mandatory)][string]$FilePath,
        [string[]]$ArgumentList = @(),
        [Parameter(Mandatory)][string]$Operation,
        [Parameter(Mandatory)][string]$LogPath,
        [ValidateRange(1, 3600)][int]$TimeoutSeconds = 180
    )
    $quotedArguments = @($ArgumentList | ForEach-Object { ConvertTo-RunnerNativeArgument $_ })
    $startInfo = [Diagnostics.ProcessStartInfo]::new()
    $startInfo.FileName = $FilePath
    $startInfo.Arguments = [string]::Join(' ', [string[]]$quotedArguments)
    $startInfo.UseShellExecute = $false
    $startInfo.CreateNoWindow = $true
    $startInfo.RedirectStandardInput = $true
    $startInfo.RedirectStandardOutput = $true
    $startInfo.RedirectStandardError = $true
    $process = [Diagnostics.Process]::new()
    $process.StartInfo = $startInfo
    $timedOut = $false
    $terminated = $true
    $killError = $null
    $processId = $null
    $exitCode = $null
    $stdout = ''
    $stderr = ''
    try {
        if (-not $process.Start()) { throw "Cannot start native command $Operation" }
        $processId = $process.Id
        $stdoutTask = $process.StandardOutput.ReadToEndAsync()
        $stderrTask = $process.StandardError.ReadToEndAsync()
        # Native diagnostics are noninteractive; EOF prevents commands waiting for console input.
        $process.StandardInput.Close()
        $finished = $process.WaitForExit([int]([TimeSpan]::FromSeconds($TimeoutSeconds).TotalMilliseconds))
        if (-not $finished) {
            $timedOut = $true
            try { $process.Kill() } catch { $killError = [string]$_ }
            $terminated = $process.WaitForExit(5000)
        }
        if ($stdoutTask.Wait(5000)) { $stdout = $stdoutTask.GetAwaiter().GetResult() }
        if ($stderrTask.Wait(5000)) { $stderr = $stderrTask.GetAwaiter().GetResult() }
        if ($process.HasExited) { $exitCode = $process.ExitCode }
    } finally {
        $process.Dispose()
    }
    $entry = [ordered]@{
        utc = [DateTime]::UtcNow.ToString('o')
        operation = $Operation
        executable = $FilePath
        arguments = [string]::Join(' ', [string[]]$quotedArguments)
        process_id = $processId
        timeout_seconds = $TimeoutSeconds
        timed_out = $timedOut
        termination_confirmed = $terminated
        kill_error = $killError
        exit_code = $exitCode
        stderr = $stderr
    }
    [IO.File]::AppendAllText($LogPath, (ConvertTo-Json -InputObject $entry -Compress) + "`r`n", [Text.UTF8Encoding]::new($false))
    if ($timedOut) { throw "Native command $Operation timed out after $TimeoutSeconds seconds; process_id=$processId; terminated=$terminated; see $LogPath" }
    if ($exitCode -ne 0) { throw "Native command $Operation failed exit=$exitCode; see $LogPath" }
    return [pscustomobject]@{ exit_code=$exitCode; stdout=$stdout; stderr=$stderr }
}

function Remove-RunnerSshConfigBlock {
    param(
        [Parameter(Mandatory)][string]$ConfigPath,
        [Parameter(Mandatory)][string]$LogPath,
        [string]$SshdPath,
        [string[]]$SshdArguments
    )
    $config = [IO.File]::ReadAllText($ConfigPath)
    $pattern = '(?ms)^# BEGIN MOTOR RUNNER\r?\n.*?^# END MOTOR RUNNER\r?\n?'
    $matches = [regex]::Matches($config, $pattern)
    if ($matches.Count -gt 1) { throw 'Multiple SSH ownership blocks; manual reconciliation required' }
    if ($matches.Count -eq 0) { return $false }
    $updated = [regex]::Replace($config, $pattern, '')
    [IO.File]::WriteAllText($ConfigPath, $updated, [Text.UTF8Encoding]::new($false))
    if (-not $SshdPath) { $SshdPath = "$env:SystemRoot\System32\OpenSSH\sshd.exe" }
    if (-not $PSBoundParameters.ContainsKey('SshdArguments')) { $SshdArguments = @('-t', '-f', $ConfigPath) }
    try {
        $null = Invoke-RunnerNative -FilePath $SshdPath -ArgumentList $SshdArguments -Operation 'validate SSH config after runner block removal' -LogPath $LogPath
    } catch {
        [IO.File]::WriteAllText($ConfigPath, $config, [Text.UTF8Encoding]::new($false))
        throw "SSH configuration removal failed validation; original restored: $_"
    }
    return $true
}


function Remove-RunnerSshInstallState {
    param(
        [Parameter(Mandatory)]$Installation,
        [Parameter(Mandatory)][string]$SshdConfig,
        [Parameter(Mandatory)][string]$LogPath,
        [string]$SshdPath,
        [string[]]$SshdArguments
    )
    $sshValidation = @{ConfigPath=$SshdConfig; LogPath=$LogPath}
    if ($SshdPath) { $sshValidation.SshdPath = $SshdPath }
    if ($PSBoundParameters.ContainsKey('SshdArguments')) { $sshValidation.SshdArguments = $SshdArguments }
    $removedBlock = Remove-RunnerSshConfigBlock @sshValidation
    if ($removedBlock) { Restart-Service sshd }
    if ($Installation.PSObject.Properties['ssh_home']) {
        if ($Installation.ssh_home -notmatch '^C:\\Users\\motor-ssh(?:\.[^\\]+)?$') { throw 'Unexpected SSH home path' }
        $sidRecord = $Installation.accounts.PSObject.Properties['motor-ssh']
        if (-not $sidRecord) { throw 'SSH profile lacks an owned account SID' }
        $sid = $sidRecord.Value
        $profiles = @(Get-CimInstance Win32_UserProfile | Where-Object SID -eq $sid)
        if ($profiles.Count -gt 1) { throw 'Ambiguous SSH profile ownership' }
        if ($profiles.Count -eq 1) {
            if ($profiles[0].LocalPath -ine $Installation.ssh_home -or $profiles[0].Loaded) { throw 'SSH profile path changed or is still loaded; close its sessions and retry removal' }
            $profiles[0] | Remove-CimInstance
        }
        if (Test-Path -LiteralPath $Installation.ssh_home) {
            if ((Get-Item -LiteralPath $Installation.ssh_home -Force).Attributes -band [IO.FileAttributes]::ReparsePoint) { throw 'Refusing a reparse SSH home' }
            Remove-Item -LiteralPath $Installation.ssh_home -Recurse -Force
        }
    }
    $sshAccount = $Installation.accounts.PSObject.Properties['motor-ssh']
    if ($sshAccount) {
        $account = Get-LocalUser motor-ssh -ErrorAction SilentlyContinue
        if ($account -and $account.SID.Value -ne $sshAccount.Value) { throw 'Account SID changed: motor-ssh; refusing deletion' }
        if ($account) { Remove-LocalUser motor-ssh }
    }
    return $removedBlock
}
