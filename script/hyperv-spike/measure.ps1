<#
.SYNOPSIS
Measure five fresh offline jobs, or one diagnostic run that reports no median.
.EXAMPLE
.\measure.ps1 -Root C:\motor-spike\hyperv
.NOTES
Run elevated after provision.ps1 succeeds. Parent hashing is outside the phase
timings and may dominate wall time; each client has a 300 s cap. Uses at most one
8 GiB / 4 vCPU VM and one disposable differencing VHDX. Keeps raw logs and JSON.
#>
param([string]$Root = 'C:\motor-spike\hyperv', [string]$ClientPath = '', [switch]$DiagnosticOnly,
    [string]$GuestAgentPath = '', [ValidateSet(0,2097152)][int]$SerialQueueBytes = 2097152,
    [ValidateSet(0,65536)][int]$StressBytes = 0, [ValidateRange(1,300)][int]$ProbeTimeoutSeconds = 300,
    [switch]$CompareSerialQueues)
. "$PSScriptRoot\common.ps1"
. "$PSScriptRoot\diagnostics.ps1"
$Root = [IO.Path]::GetFullPath($Root)
$parent = Join-Path $Root 'parent.vhdx'
$client = if ($ClientPath) { (Resolve-Path -LiteralPath $ClientPath).Path } else { Join-Path $Root 'hyperv-spike.exe' }
$lock = $null
$transcript = $false
$rows = @()
$comparison = @()
$canary = $null
try {
    $lock = Enter-Spike $Root
    if (-not $DiagnosticOnly -and ($StressBytes -ne 0 -or $SerialQueueBytes -eq 0 -or $ProbeTimeoutSeconds -ne 300)) { throw 'Serial control/stress/short-timeout options require DiagnosticOnly' }
    if ($SerialQueueBytes -eq 0 -and -not $GuestAgentPath) { throw 'Serial queue control requires a diagnostic agent override' }
    if ($CompareSerialQueues -and (-not $DiagnosticOnly -or -not $GuestAgentPath -or $StressBytes -ne 65536 -or $SerialQueueBytes -ne 2097152)) { throw 'Queue comparison requires DiagnosticOnly, GuestAgentPath, StressBytes 65536 and the 2097152-byte fix' }
    $agentHash = if ($GuestAgentPath) { $GuestAgentPath = (Resolve-Path -LiteralPath $GuestAgentPath).Path; (Get-FileHash -LiteralPath $GuestAgentPath).Hash } else { '' }
    $batch = Join-Path $Root ('measure-' + [DateTime]::UtcNow.ToString('yyyyMMdd-HHmmss') + '-' + [Guid]::NewGuid().ToString('N'))
    New-Item $batch -ItemType Directory | Out-Null
    Start-Transcript -Path (Join-Path $batch 'host.log') | Out-Host
    $transcript = $true
    $seal = Get-Content (Join-Path $Root 'sealed.json') -Raw | ConvertFrom-Json
    Assert-Spike ((Get-Item $parent).IsReadOnly) 'parent-read-only' ((Get-Acl $parent).Sddl)
    Assert-Spike ((Get-FileHash $parent).Hash -eq $seal.parent_sha256) 'parent-hash-before' $seal.parent_sha256
    Assert-Spike ((Get-CimInstance Win32_OperatingSystem).FreePhysicalMemory -ge 10GB/1KB) 'free-memory-at-least-10GiB' (Get-CimInstance Win32_OperatingSystem).FreePhysicalMemory
    Assert-Spike (@(Get-ChildItem $Root -Filter 'job-*.vhdx').Count -eq 0) 'no-orphan-job-disk' $Root
    $version = Invoke-SpikeNative $client @('--version') $batch 'client-version'
    Assert-Spike ($version.exitCode -eq 0 -and $version.stdout -match 'protocol 4 trace=3') 'probe-version' ((Get-FileHash $client).Hash)
    $canary = Join-Path $Root ('host-only-' + [Guid]::NewGuid().ToString('N') + '.txt')
    Set-Content $canary ([Guid]::NewGuid().ToString('N')) -Encoding ascii
    Assert-Spike (Test-Path $canary) 'real-host-canary-exists' "path=$canary SHA256=$((Get-FileHash $canary).Hash) (contents never staged)"
    $runCount = if ($CompareSerialQueues) { 2 } elseif ($DiagnosticOnly) { 1 } else { 5 }
    $queueEvidence = if ($GuestAgentPath) { [string]$SerialQueueBytes } else { 'baseline-not-overridden' }
    Write-Host "EVIDENCE measurement-mode diagnosticOnly=$DiagnosticOnly runs=$runCount client=$client agent=$agentHash serialQueue=$queueEvidence compare=$CompareSerialQueues stressBytes=$StressBytes capSeconds=$ProbeTimeoutSeconds"
    for ($run = 1; $run -le $runCount; $run++) {
        $activeQueueBytes = if ($CompareSerialQueues -and $run -eq 1) { 0 } else { $SerialQueueBytes }
        $controlFailure = $null
        $id = [Guid]::NewGuid().ToString('N')
        $name = "motor-hv-$id"
        $pipe = $name
        $child = Join-Path $Root "job-$id.vhdx"
        $vmDirectory = Join-Path $Root "job-$id"
        $result = Join-Path $batch "run-$run.json"
        $stdout = Join-Path $batch "run-$run.stdout.log"
        $stderr = Join-Path $batch "run-$run.stderr.log"
        $running = $null
        $output = $null
        $logsDisplayed = $false
        $probe = $null
        $create = [Diagnostics.Stopwatch]::StartNew()
        $createBegin = Get-SpikeUnixMs
        $timeline = [ordered]@{ create_begin_unix_ms=$createBegin }
        Write-Host "EVIDENCE vm-create-begin unix_ms=$createBegin"
        $createMs = $null
        $helloMs = $null
        $cleanupOkay = $false
        try {
            Assert-Spike (Test-SpikePipeAbsent $pipe) 'pipe-absent-before' $pipe
            New-VHD -Path $child -Differencing -ParentPath $parent | Out-Null
            $vhd = Get-VHD $child
            Assert-Spike ($vhd.VhdType -eq 'Differencing' -and $vhd.ParentPath -eq $parent -and (Test-VHD $child)) 'differencing-disk' ($vhd | Select-Object Path,ParentPath,VhdType | Out-String)
            Use-SpikeWindowsDisk $child {
                param($volume)
                $nonce = Join-Path $volume 'ProgramData\Motor\hyperv-nonce.txt'
                [IO.File]::WriteAllText($nonce, $id, [Text.UTF8Encoding]::new($false))
                Assert-Spike ((Get-Acl $nonce).AreAccessRulesProtected -eq $false) 'nonce-inherits-protected-motor-acl' ((Get-Acl $nonce).Sddl)
                if ($GuestAgentPath) {
                    $agent = Get-SpikePlainGuestItem $volume 'ProgramData\Motor\motor-winagent.exe'
                    if (-not $agent) { throw 'Baseline agent path is missing or reparse' }
                    $beforeAcl = (Get-Acl -LiteralPath $agent.FullName).Sddl
                    [IO.File]::WriteAllBytes($agent.FullName, [IO.File]::ReadAllBytes($GuestAgentPath))
                    Assert-Spike ((Get-FileHash -LiteralPath $agent.FullName).Hash -eq $agentHash) 'overlay-agent-hash' $agentHash
                    Assert-Spike ((Get-Acl -LiteralPath $agent.FullName).Sddl -eq $beforeAcl) 'overlay-agent-acl-preserved' $beforeAcl
                    [IO.File]::WriteAllText((Join-Path $volume 'ProgramData\Motor\hyperv-serial-queue.txt'), [string]$activeQueueBytes, [Text.UTF8Encoding]::new($false))
                    Write-Host "EVIDENCE overlay-serial-queue run=$run bytes=$activeQueueBytes"
                }
            }
            $timeline['define_vm_begin_unix_ms'] = Get-SpikeUnixMs
            New-SpikeVM $name $child $vmDirectory $pipe
            $timeline['define_vm_done_unix_ms'] = Get-SpikeUnixMs
            Assert-Spike (@(Get-VMNetworkAdapter -VMName $name).Count -eq 0) 'no-job-network-adapter' $name
            Assert-Spike (@(Get-VMDvdDrive -VMName $name).Count -eq 0) 'no-job-media' $name
            Assert-Spike (@(Get-VMHardDiskDrive -VMName $name).Count -eq 1) 'only-os-overlay-attached' $child
            Assert-Spike (@(Get-VMIntegrationService -VMName $name | Where-Object Enabled).Count -eq 0) 'no-optional-integration-services' $name
            Assert-Spike ((Get-VMComPort -VMName $name -Number 1).Path -eq "\\.\pipe\$pipe") 'com1-pipe-mapping' $pipe
            $createMs = $create.Elapsed.TotalMilliseconds
            $createDone = Get-SpikeUnixMs
            $timeline['create_done_unix_ms'] = $createDone
            Write-Host "EVIDENCE vm-create-done unix_ms=$createDone"
            $bootStart = Get-SpikeUnixMs
            $timeline['start_vm_begin_unix_ms'] = $bootStart
            Write-Host "EVIDENCE start-vm-begin unix_ms=$bootStart"
            Start-VM -Name $name
            $startReturned = Get-SpikeUnixMs
            $timeline['start_vm_return_unix_ms'] = $startReturned
            Write-Host "EVIDENCE start-vm-return unix_ms=$startReturned"
            $clientArgs = @('--pipe', $pipe, '--nonce', $id, '--canary', $canary, '--host', $env:COMPUTERNAME, '--result', $result)
            if ($StressBytes) { $clientArgs += @('--stress-bytes',[string]$StressBytes) }
            $running = Start-SpikeNativeProcess $client $clientArgs
            $timeline['client_start_unix_ms'] = $running.started_unix_ms
            Write-Host "EVIDENCE probe-start pid=$($running.process.Id) unix_ms=$($running.started_unix_ms)"
            $finished = $running.process.WaitForExit($ProbeTimeoutSeconds * 1000)
            if (-not $finished) {
                Get-VM -Name $name | Select-Object Name,State,Status,Uptime,CPUUsage,MemoryAssigned | Format-List | Out-Host
                Write-Host "EVIDENCE probe-timeout unix_ms=$(Get-SpikeUnixMs) pid=$($running.process.Id)"
                $running.process.Kill(); $running.process.WaitForExit()
            }
            $output = Complete-SpikeNativeProcess $running $stdout $stderr
            $timeline['client_exit_unix_ms'] = Get-SpikeUnixMs
            $timeline['client_exit_code'] = $output.exitCode
            $progress = Get-SpikeClientProgress $output.stderr
            foreach ($key in $progress.Keys) { $timeline[$key] = $progress[$key] }
            if (-not $finished) {
                throw "FAIL probe-timeout (${ProbeTimeoutSeconds}s total, includes boot/Hello/command/protocol completion)"
            }
            Get-Content $stdout,$stderr | Out-Host
            $logsDisplayed = $true
            Assert-Spike ($output.exitCode -eq 0) 'probe-exit' "run=$run exit=$($output.exitCode)"
            $probe = Get-Content $result -Raw | ConvertFrom-Json
            $helloMs = [double]$probe.hello_unix_ms - $bootStart
            $firstByte = [long]$probe.first_com_byte_unix_ms
            Assert-Spike ($firstByte -ge $bootStart -and $firstByte -le $probe.hello_unix_ms) 'first-com-byte-clock-order' "first=$firstByte start=$bootStart hello=$($probe.hello_unix_ms)"
            $startCallMs = [double]$startReturned - $bootStart
            $startToFirstMs = [double]$firstByte - $bootStart
            $firstToHelloMs = [double]$probe.hello_unix_ms - $firstByte
            $timeline['first_com_byte_unix_ms'] = $firstByte
            $timeline['hello_unix_ms'] = $probe.hello_unix_ms
            $timeline['command_start_unix_ms'] = $probe.command_start_unix_ms
            $timeline['command_exit_unix_ms'] = $probe.command_exit_unix_ms
            $timeline['protocol_complete_unix_ms'] = $probe.protocol_complete_unix_ms
            Write-Host "EVIDENCE boot-timeline $($timeline | ConvertTo-Json -Compress)"
            Assert-Spike ($helloMs -ge 0 -and $helloMs -le 120000) 'boot-to-validated-Hello' "run=$run ms=$helloMs"
        } catch {
            if ($CompareSerialQueues -and $run -eq 1 -and $_.Exception.Message -match 'FAIL probe-(timeout|exit)') {
                $controlFailure = $_.Exception.Message
                Write-Host "DIAGNOSTIC default-queue control failed: $controlFailure (not an accepted sample)"
            } else { throw }
        } finally {
            $teardown = [Diagnostics.Stopwatch]::StartNew()
            $timeline['teardown_begin_unix_ms'] = Get-SpikeUnixMs
            try {
                try {
                    if ($running -and -not $running.process.HasExited) { $running.process.Kill(); $running.process.WaitForExit() }
                    if ($running -and -not $output) { $output = Complete-SpikeNativeProcess $running $stdout $stderr }
                    if (-not $logsDisplayed) {
                        foreach ($path in @($stdout,$stderr)) { if (Test-Path -LiteralPath $path) { Get-Content -LiteralPath $path | Out-Host } }
                    }
                    if ($probe) { Wait-SpikeOff $name 60 }
                } finally {
                    try { Close-SpikeNativeProcess $running } finally {
                        Remove-SpikeVM $name
                        if (-not $probe -and (Test-Path -LiteralPath $child)) {
                            try { Save-SpikeGuestEvidence $child (Join-Path $batch "run-$run-guest") }
                            catch { Write-Host "FAIL diagnostic-harvest $_ (cleanup still required)" }
                        }
                        if (Test-Path $child) { Remove-Item $child -Force }
                        if (Test-Path $vmDirectory) { Remove-Item $vmDirectory -Recurse -Force }
                        Assert-Spike (-not (Test-Path $child) -and -not (Test-Path $vmDirectory)) 'disk-and-vm-directory-removed' "$child $vmDirectory"
                        Assert-Spike (Test-SpikePipeAbsent $pipe) 'pipe-removed' "\\.\pipe\$pipe"
                        $cleanupOkay = $true
                    }
                }
            } finally {
                $teardownMs = $teardown.Elapsed.TotalMilliseconds
                $timeline['teardown_done_unix_ms'] = Get-SpikeUnixMs
                $timeline | ConvertTo-Json | Set-Content (Join-Path $batch "run-$run-timeline.json") -Encoding UTF8
                Write-Host "EVIDENCE full-timeline $($timeline | ConvertTo-Json -Compress)"
            }
        }
        if ($CompareSerialQueues) {
            $comparison += [ordered]@{ run=$run; queue_bytes=$activeQueueBytes; completed=($null -eq $controlFailure -and $null -ne $probe); control_failure=$controlFailure; cleanup=$cleanupOkay }
            ConvertTo-Json -InputObject $comparison -Depth 5 | Set-Content (Join-Path $batch 'serial-comparison.json') -Encoding UTF8
        }
        if ($controlFailure) {
            Assert-Spike $cleanupOkay 'failed-control-cleanup-complete' $run
            continue
        }
        Assert-Spike ($cleanupOkay -and $null -ne $probe) 'complete-sample' $run
        $row = [ordered]@{ create_ms=$createMs; boot_hello_ms=$helloMs; command_ms=[double]$probe.command_ms; teardown_ms=$teardownMs; start_vm_call_ms=$startCallMs; start_to_first_byte_ms=$startToFirstMs; first_byte_to_hello_ms=$firstToHelloMs }
        Write-Host "PASS phases run=$run $($row | ConvertTo-Json -Compress)"
        $rows += $row
        ConvertTo-Json -InputObject $rows -Depth 5 | Set-Content (Join-Path $batch 'samples.json') -Encoding ascii
    }
} catch {
    Write-Host "FAIL measurement $_ (no median is valid unless all five runs and cleanup passed)"
    throw
} finally {
    try {
        if ($lock) {
            if ($canary -and (Test-Path $canary)) { Remove-Item $canary -Force }
            Assert-Spike (@(Get-VM -Name 'motor-hv-*' -ErrorAction SilentlyContinue).Count -eq 0) 'final-no-spike-vm' 'prefix=motor-hv-'
            Assert-Spike (@(Get-ChildItem $Root -Filter 'job-*').Count -eq 0) 'final-no-job-disk-or-config' $Root
            Assert-Spike (@([IO.Directory]::GetFiles('\\.\pipe\') | Where-Object { $_ -like '\\.\pipe\motor-hv-*' }).Count -eq 0) 'final-no-spike-pipe' 'prefix=motor-hv-'
            if (Test-Path (Join-Path $Root 'sealed.json')) {
                $expected = (Get-Content (Join-Path $Root 'sealed.json') -Raw | ConvertFrom-Json).parent_sha256
                Assert-Spike ((Get-Item $parent).IsReadOnly -and (Get-FileHash $parent).Hash -eq $expected) 'parent-unchanged-after' $expected
            }
        }
    } finally {
        if ($transcript) { Stop-Transcript | Out-Host }
        if ($lock) { $lock.Dispose() }
    }
}
if ($DiagnosticOnly) {
    Write-Host 'DIAGNOSTIC run(s) completed; five-run measurement not performed and no median reported'
    return
}
$summary = Invoke-SpikeNative $client @('--summary', (Join-Path $batch 'samples.json')) $batch 'summary'
Set-Content (Join-Path $batch 'summary.log') $summary.stdout -Encoding UTF8
Assert-Spike ($summary.exitCode -eq 0) 'five-run-summary' $batch
