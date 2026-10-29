# The bridge authenticates SCM's process ID, so a pipe planted while the service
# is stopped cannot supply a false idle or ready reply to the updater.
function Get-RunnerUpdateStatus([string]$Root) {
    $info = [Diagnostics.ProcessStartInfo]::new()
    $info.FileName = "$Root\bin\motor-runner.exe"
    $info.Arguments = [string]::Join(' ', [string[]]@(@('connect','--instance','ally','--state-root',"$Root\state") | ForEach-Object { ConvertTo-RunnerNativeArgument $_ }))
    $info.UseShellExecute = $false
    $info.CreateNoWindow = $true
    $info.RedirectStandardInput = $true
    $info.RedirectStandardOutput = $true
    $info.RedirectStandardError = $true
    $process = [Diagnostics.Process]::new()
    $process.StartInfo = $info
    $started = $false
    try {
        $null = $process.Start()
        $started = $true
        $stderr = $process.StandardError.ReadToEndAsync()
        $clock = [Diagnostics.Stopwatch]::StartNew()
        function Send-UpdateFrame($Value) {
            $bytes = [Text.Encoding]::UTF8.GetBytes(($Value | ConvertTo-Json -Compress))
            $length = [BitConverter]::GetBytes([uint32]$bytes.Length)
            [Array]::Reverse($length)
            $process.StandardInput.BaseStream.Write($length, 0, 4)
            $process.StandardInput.BaseStream.Write($bytes, 0, $bytes.Length)
            $process.StandardInput.BaseStream.Flush()
        }
        function Read-UpdateBytes([int]$Length) {
            $bytes = New-Object byte[] $Length
            $offset = 0
            while ($offset -lt $Length) {
                $read = $process.StandardOutput.BaseStream.ReadAsync($bytes, $offset, $Length - $offset)
                $remaining = [Math]::Max(1, 10000 - [int]$clock.ElapsedMilliseconds)
                if ($clock.ElapsedMilliseconds -ge 10000 -or -not $read.Wait($remaining)) { throw 'MotorRunner status timed out after 10 seconds' }
                $size = $read.Result
                if (-not $size) {
                    $detail = if ($stderr.IsCompleted) { $stderr.GetAwaiter().GetResult() } else { 'bridge stderr is still open' }
                    throw "MotorRunner bridge closed before status: $detail"
                }
                $offset += $size
            }
            return ,$bytes
        }
        function Read-UpdateFrame([string]$Kind) {
            while ($true) {
                $length = Read-UpdateBytes 4
                [Array]::Reverse($length)
                $size = [BitConverter]::ToUInt32($length, 0)
                if ($size -lt 1 -or $size -gt 1048576) { throw 'MotorRunner status frame exceeds protocol bounds' }
                $frame = [Text.Encoding]::UTF8.GetString((Read-UpdateBytes $size)) | ConvertFrom-Json
                if ($frame.type -eq $Kind) { return $frame }
                if ($frame.type -ne 'heartbeat') { throw "Unexpected MotorRunner reply: $($frame.type)" }
            }
        }
        Send-UpdateFrame @{type='hello'; protocol_version=1; started=$true; gate_jobs=$true}
        $hello = Read-UpdateFrame 'hello'
        if ($hello.protocol_version -ne 1 -or ($hello.platforms -join ',') -cne 'windows') { throw 'MotorRunner did not negotiate the Windows protocol' }
        Send-UpdateFrame @{type='status'}
        return Read-UpdateFrame 'status_reply'
    } finally {
        if ($started -and -not $process.HasExited) { $process.Kill(); $null = $process.WaitForExit(5000) }
        $process.Dispose()
    }
}

function Assert-RunnerUpdateIdle([string]$Root) {
    $vms = @(Get-VM | Where-Object { $_.Name -like 'motor-ally-*' })
    if ($vms.Count) { throw "Host update blocked by job VM(s): $(($vms | ForEach-Object Name) -join ', '); remove them through the owned runner cleanup first" }
    $service = Get-Service -Name MotorRunner -ErrorAction Stop
    if ($service.Status -eq 'Stopped') { return }
    if ($service.Status -ne 'Running') { throw "Host update blocked by MotorRunner service state $($service.Status)" }
    $status = Get-RunnerUpdateStatus $Root
    if (@($status.running_jobs).Count -or $status.queue_depth -ne 0) {
        throw "Host update blocked by MotorRunner job(s): $(($status.running_jobs | ForEach-Object job_id) -join ', '); queued=$($status.queue_depth)"
    }
}

function Wait-RunnerUpdateReady([string]$Root, [int]$TimeoutSeconds) {
    $clock = [Diagnostics.Stopwatch]::StartNew()
    $reason = 'service has not reported Windows readiness'
    while ($clock.Elapsed.TotalSeconds -lt $TimeoutSeconds) {
        $service = Get-Service -Name MotorRunner -ErrorAction Stop
        if ($service.Status -notin @('Running','StartPending')) { throw "MotorRunner failed to start: $($service.Status)" }
        try {
            $status = Get-RunnerUpdateStatus $Root
            if ($status.health.windows_guest_agent.ready -eq $true) { return }
            $reason = 'Windows guest agent is not ready'
        } catch { $reason = [string]$_ }
        Start-Sleep -Milliseconds 500
    }
    throw "MotorRunner did not report ready within $TimeoutSeconds seconds: $reason"
}

function Stop-RunnerUpdateService {
    $clock = [Diagnostics.Stopwatch]::StartNew()
    $reason = ''
    while ($clock.Elapsed.TotalSeconds -lt 120) {
        $service = Get-Service -Name MotorRunner -ErrorAction Stop
        if ($service.Status -eq 'Stopped') { return }
        try { if ($service.Status -ne 'StopPending') { $service.Stop() } } catch { $reason = [string]$_ }
        Start-Sleep -Milliseconds 250
    }
    throw "MotorRunner did not stop within 120 seconds: $reason"
}

function Get-RunnerUpdateServiceDefinition {
    $key = Get-ItemProperty 'HKLM:\SYSTEM\CurrentControlSet\Services\MotorRunner'
    $start = switch ($key.Start) { 2 { 'auto' } 3 { 'demand' } 4 { 'disabled' } default { throw 'Unexpected MotorRunner service start mode' } }
    return [pscustomobject]@{ binary=$key.ImagePath; display_name=$key.DisplayName; start=$start; dependencies=@($key.DependOnService); environment=@($key.Environment) }
}

function Set-RunnerUpdateServiceDefinition($Definition, [string]$LogPath) {
    # Reconfigure in place: deleting/recreating the service would lose the SSH
    # bridge's query-only service DACL and the existing recovery policy.
    $null = Invoke-RunnerNative -FilePath "$env:SystemRoot\System32\sc.exe" -ArgumentList @('config','MotorRunner','binPath=', $Definition.binary, 'DisplayName=', $Definition.display_name, 'start=', $Definition.start, 'depend=', ($Definition.dependencies -join '/')) -Operation 'configure runner host payload' -LogPath $LogPath
    Set-ItemProperty 'HKLM:\SYSTEM\CurrentControlSet\Services\MotorRunner' -Name Environment -Value ([string[]]$Definition.environment)
}

function Assert-RunnerUpdateTree([string]$Path) {
    $item = Get-Item -LiteralPath $Path -Force
    if ($item.Attributes -band [IO.FileAttributes]::ReparsePoint) { throw "Reparse payload refused: $Path" }
    if ($item.PSIsContainer) {
        foreach ($child in @(Get-ChildItem -LiteralPath $Path -Force)) { Assert-RunnerUpdateTree $child.FullName }
    }
}

function Test-RunnerUpdateManifest([string]$Directory) {
    Assert-RunnerUpdateTree $Directory
    $manifest = Get-Content -LiteralPath "$Directory\manifest.json" -Raw | ConvertFrom-Json
    if ($manifest.schema -ne 1 -or -not @($manifest.files).Count) { throw 'Invalid release manifest' }
    $files = @{}
    foreach ($file in $manifest.files) {
        if ($file.path -notmatch '\A[A-Za-z0-9_.-]+(?:/[A-Za-z0-9_.-]+)*\z' -or $file.path -match '(^|/)\.\.?(?:/|$)' -or $files.ContainsKey($file.path) -or $file.sha256 -notmatch '\A[0-9A-Fa-f]{64}\z') { throw "Unsafe or duplicate release manifest path: $($file.path)" }
        if ((Get-FileHash -LiteralPath (Join-Path $Directory $file.path) -Algorithm SHA256).Hash -ne $file.sha256) { throw "Release hash mismatch: $($file.path)" }
        $files[$file.path] = $file.sha256
    }
    return $files
}

function Set-RunnerUpdatePrivateFile([string]$Path) {
    $acl = [Security.AccessControl.FileSecurity]::new()
    $acl.SetSecurityDescriptorSddlForm('O:BAG:BAD:P(A;;FA;;;SY)(A;;FA;;;BA)')
    Set-Acl -LiteralPath $Path -AclObject $acl
}

function Write-RunnerUpdateManifest([string]$Directory) {
    $files = @(Get-ChildItem -LiteralPath $Directory -Recurse -File | ForEach-Object {
        @{ path=$_.FullName.Substring($Directory.Length + 1).Replace('\','/'); sha256=(Get-FileHash -LiteralPath $_.FullName -Algorithm SHA256).Hash }
    })
    @{schema=1; files=$files} | ConvertTo-Json -Depth 6 | Set-Content -LiteralPath "$Directory\manifest.json" -Encoding UTF8
}

function Invoke-RunnerHostUpdate {
    param([string]$Root, [string]$Bundle, [switch]$Rollback, [int]$ReadyTimeoutSeconds = 300)
    Assert-RunnerPrivateDirectory $Root
    if ((Get-Content -LiteralPath "$Root\.motor-runner-root" -Raw).Trim() -cne 'motor-runner-hyperv-v1') { throw 'Installed runner root marker is missing or invalid' }
    Assert-RunnerUpdateTree "$Root\bin"
    $next = "$Root\payload-next"
    $backup = "$Root\payload-backup"
    $previous = "$Root\payload-previous"
    $retired = "$Root\payload-retired"
    foreach ($path in @($next,$backup,$retired)) {
        if (Test-Path -LiteralPath $path) { throw "Incomplete host update at $path; preserve and inspect it before retrying" }
    }
    if (Test-Path -LiteralPath $previous) { Assert-RunnerPrivateDirectory $previous; Assert-RunnerUpdateTree $previous }
    Import-Module Hyper-V
    Assert-RunnerUpdateIdle $Root
    $config = Get-Content -LiteralPath "$Root\vm.toml" -Raw
    $agent = [regex]::Matches($config, '(?m)^guest_agent_sha256\s*=\s*[''"]([0-9A-Fa-f]{64})[''"]\s*\r?$')
    if ($agent.Count -ne 1) { throw 'Installed parent guest-agent identity is missing; rebuild the parent using the README full uninstall/reinstall path' }
    if ($Rollback) {
        $source = $previous
        $files = Test-RunnerUpdateManifest $source
        $definition = Get-Content -LiteralPath "$source\service.json" -Raw | ConvertFrom-Json
        $payload = @{}
        foreach ($path in $files.Keys) { if ($path -like 'bin/*') { $payload[$path] = $path } }
    } else {
        $source = (Resolve-Path -LiteralPath $Bundle).Path
        $files = Test-RunnerUpdateManifest $source
        $payload = @{}
        foreach ($path in $files.Keys) { if ($path -match '^bin/[^/]+$') { $payload[$path] = $path } }
        foreach ($name in @('hyperv.ps1','parent-identity.ps1','guest-agent-identity.ps1','guest-evidence.ps1','accounts.ps1','qualify.ps1','qualify-peer.ps1','uninstall.ps1','run-system.ps1','verify-sshd-system.ps1','qualify-jobs.py','update.ps1','update-functions.ps1','service-definition.ps1')) {
            $payload["bin/$name"] = "script/windows-runner/$name"
        }
        foreach ($name in @('common.ps1','process.ps1')) { $payload["bin/$name"] = "script/hyperv-spike/$name" }
        foreach ($path in $payload.Values) { if (-not $files.ContainsKey($path)) { throw "Release manifest omits host payload: $path" } }
        . "$source\script\windows-runner\service-definition.ps1"
        $definition = Get-RunnerServiceDefinition $Root
    }
    foreach ($path in @('bin/motor-runner.exe','bin/motor-winagent.exe')) {
        if (-not $payload.ContainsKey($path) -or -not $files.ContainsKey($payload[$path])) { throw "Release manifest omits $path" }
    }
    # This is the same sidecar hash the runner compares at startup. The parent
    # pin was read from its sealed provisioning record by the original install.
    $candidateAgent = $files[$payload['bin/motor-winagent.exe']]
    if ($candidateAgent -ine $agent[0].Groups[1].Value) { throw "Guest agent hash mismatch: release=$candidateAgent parent=$($agent[0].Groups[1].Value); a parent rebuild is required; use the README full uninstall/reinstall path" }
    $oldDefinition = Get-RunnerUpdateServiceDefinition
    $binAcl = Get-Acl -LiteralPath "$Root\bin"
    # All refusal checks above are read-only. Stage on the same volume so each
    # directory rename is atomic, without touching state, the parent or caches.
    New-Item -ItemType Directory -Path $next | Out-Null
    $log = "$next\native.jsonl"
    $movedOld = $false
    $movedNew = $false
    $serviceStopped = $false
    $ready = $false
    $preserveRecovery = $false
    $oldContainer = $backup
    $previousRetired = $false
    try {
        Set-Content -LiteralPath $log -Value '' -Encoding UTF8
        Set-RunnerUpdatePrivateFile $log
        Protect-RunnerPath $next $log
        New-Item -ItemType Directory -Path "$next\bin" | Out-Null
        Protect-RunnerPath "$next\bin" $log
        foreach ($target in $payload.Keys) {
            $destination = Join-Path $next $target
            $null = New-Item -ItemType Directory -Path (Split-Path -Parent $destination) -Force
            Copy-Item -LiteralPath (Join-Path $source $payload[$target]) -Destination $destination
            if ((Get-FileHash -LiteralPath $destination -Algorithm SHA256).Hash -ine $files[$payload[$target]]) { throw "Staged release hash mismatch: $target" }
        }
        # Preserve the existing bridge read/execute grant, but give every newly
        # copied file the trusted owner required on Windows client editions.
        Set-Acl -LiteralPath "$next\bin" -AclObject $binAcl
        $null = Invoke-RunnerNative -FilePath "$env:SystemRoot\System32\icacls.exe" -ArgumentList @("$next\bin", '/setowner', '*S-1-5-32-544', '/T', '/Q') -Operation 'set staged payload owners' -LogPath $log
        Assert-RunnerUpdateIdle $Root
        $serviceStopped = $true
        Stop-RunnerUpdateService
        # Catch a VM created after the last live status probe; never reconcile
        # or delete job resources as part of a host update.
        Assert-RunnerUpdateIdle $Root
        New-Item -ItemType Directory -Path $backup | Out-Null
        Protect-RunnerPath $backup $log
        $oldDefinition | ConvertTo-Json -Depth 6 | Set-Content -LiteralPath "$backup\service.json" -Encoding UTF8
        Set-RunnerUpdatePrivateFile "$backup\service.json"
        Move-Item -LiteralPath "$Root\bin" -Destination "$backup\bin"
        $movedOld = $true
        Move-Item -LiteralPath "$next\bin" -Destination "$Root\bin"
        $movedNew = $true
        Write-RunnerUpdateManifest $backup
        Set-RunnerUpdatePrivateFile "$backup\manifest.json"
        Set-RunnerUpdateServiceDefinition $definition $log
        Start-Service -Name MotorRunner -ErrorAction Stop
        Wait-RunnerUpdateReady $Root $ReadyTimeoutSeconds
        if (Test-Path -LiteralPath $previous) {
            Move-Item -LiteralPath $previous -Destination $retired
            $previousRetired = $true
        }
        Move-Item -LiteralPath $backup -Destination $previous
        $oldContainer = $previous
        $ready = $true
    } catch {
        $reason = [string]$_
        try {
            if ($movedOld) {
                Stop-RunnerUpdateService
                if ($movedNew) { Move-Item -LiteralPath "$Root\bin" -Destination "$next\failed-bin" }
                Move-Item -LiteralPath "$oldContainer\bin" -Destination "$Root\bin"
                Set-RunnerUpdateServiceDefinition $oldDefinition $log
            }
            if ($serviceStopped) {
                Start-Service -Name MotorRunner -ErrorAction Stop
                Wait-RunnerUpdateReady $Root $ReadyTimeoutSeconds
            }
            if (Test-Path -LiteralPath $oldContainer) { Remove-Item -LiteralPath $oldContainer -Recurse -Force }
            if ($previousRetired) { Move-Item -LiteralPath $retired -Destination $previous }
        } catch {
            $preserveRecovery = $true
            throw "Host update failed: $reason; automatic rollback also failed: $_; preserve $next, $oldContainer and $retired for manual recovery"
        }
        throw "Host update failed: $reason; previous payload restored (if swapped) and MotorRunner restarted (if stopped)"
    } finally {
        # Keep failed recovery material rather than removing the only copy of
        # a payload when rollback itself could not finish.
        if (-not $preserveRecovery -and -not $ready -and (Test-Path -LiteralPath $next)) { Remove-Item -LiteralPath $next -Recurse -Force }
    }
    if ($ready) {
        foreach ($path in @($next,$retired)) {
            if (Test-Path -LiteralPath $path) {
                try { Remove-Item -LiteralPath $path -Recurse -Force } catch { Write-Warning "MotorRunner is ready, but scratch cleanup failed at $path; inspect it before the next update: $_" -WarningAction Continue }
            }
        }
        Write-Host "Host payload ready. Previous payload retained at $previous; restore it with update.ps1 -Rollback."
    }
}
