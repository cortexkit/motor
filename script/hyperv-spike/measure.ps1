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
param([string]$Root = 'C:\motor-spike\hyperv', [string]$ClientPath = '', [switch]$DiagnosticOnly)
. "$PSScriptRoot\common.ps1"
. "$PSScriptRoot\diagnostics.ps1"
$Root = [IO.Path]::GetFullPath($Root)
$parent = Join-Path $Root 'parent.vhdx'
$client = if ($ClientPath) { (Resolve-Path -LiteralPath $ClientPath).Path } else { Join-Path $Root 'hyperv-spike.exe' }
$lock = $null
$transcript = $false
$rows = @()
$canary = $null
try {
    $lock = Enter-Spike $Root
    $batch = Join-Path $Root ('measure-' + [DateTime]::UtcNow.ToString('yyyyMMdd-HHmmss') + '-' + [Guid]::NewGuid().ToString('N'))
    New-Item $batch -ItemType Directory | Out-Null
    Start-Transcript -Path (Join-Path $batch 'host.log') | Out-Host
    $transcript = $true
    $seal = Get-Content (Join-Path $Root 'sealed.json') -Raw | ConvertFrom-Json
    Assert-Spike ((Get-Item $parent).IsReadOnly) 'parent-read-only' ((Get-Acl $parent).Sddl)
    Assert-Spike ((Get-FileHash $parent).Hash -eq $seal.parent_sha256) 'parent-hash-before' $seal.parent_sha256
    Assert-Spike ((Get-CimInstance Win32_OperatingSystem).FreePhysicalMemory -ge 10GB/1KB) 'free-memory-at-least-10GiB' (Get-CimInstance Win32_OperatingSystem).FreePhysicalMemory
    Assert-Spike (@(Get-ChildItem $Root -Filter 'job-*.vhdx').Count -eq 0) 'no-orphan-job-disk' $Root
    & $client --version | Out-Host
    Assert-Spike ($LASTEXITCODE -eq 0) 'probe-version' ((Get-FileHash $client).Hash)
    $canary = Join-Path $Root ('host-only-' + [Guid]::NewGuid().ToString('N') + '.txt')
    Set-Content $canary ([Guid]::NewGuid().ToString('N')) -Encoding ascii
    Assert-Spike (Test-Path $canary) 'real-host-canary-exists' "path=$canary SHA256=$((Get-FileHash $canary).Hash) (contents never staged)"
    $runCount = if ($DiagnosticOnly) { 1 } else { 5 }
    Write-Host "EVIDENCE measurement-mode diagnosticOnly=$DiagnosticOnly runs=$runCount client=$client"
    for ($run = 1; $run -le $runCount; $run++) {
        $id = [Guid]::NewGuid().ToString('N')
        $name = "motor-hv-$id"
        $pipe = $name
        $child = Join-Path $Root "job-$id.vhdx"
        $vmDirectory = Join-Path $Root "job-$id"
        $result = Join-Path $batch "run-$run.json"
        $stdout = Join-Path $batch "run-$run.stdout.log"
        $stderr = Join-Path $batch "run-$run.stderr.log"
        $process = $null
        $probe = $null
        $create = [Diagnostics.Stopwatch]::StartNew()
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
            }
            New-SpikeVM $name $child $vmDirectory $pipe
            Assert-Spike (@(Get-VMNetworkAdapter -VMName $name).Count -eq 0) 'no-job-network-adapter' $name
            Assert-Spike (@(Get-VMDvdDrive -VMName $name).Count -eq 0) 'no-job-media' $name
            Assert-Spike (@(Get-VMHardDiskDrive -VMName $name).Count -eq 1) 'only-os-overlay-attached' $child
            Assert-Spike (@(Get-VMIntegrationService -VMName $name | Where-Object Enabled).Count -eq 0) 'no-optional-integration-services' $name
            Assert-Spike ((Get-VMComPort -VMName $name -Number 1).Path -eq "\\.\pipe\$pipe") 'com1-pipe-mapping' $pipe
            $createMs = $create.Elapsed.TotalMilliseconds
            $bootStart = Get-SpikeUnixMs
            Write-Host "EVIDENCE start-vm-begin unix_ms=$bootStart"
            Start-VM -Name $name
            Write-Host "EVIDENCE start-vm-return unix_ms=$(Get-SpikeUnixMs)"
            # Start-Process on Windows joins ArgumentList; quote every value so
            # spaces in the root/canary path remain part of a single argument.
            $clientArgs = @('--pipe', $pipe, '--nonce', $id, '--canary', $canary, '--host', $env:COMPUTERNAME, '--result', $result)
            $quotedArgs = @($clientArgs | ForEach-Object { '"' + $_ + '"' })
            $process = Start-Process -FilePath $client -ArgumentList $quotedArgs -RedirectStandardOutput $stdout -RedirectStandardError $stderr -PassThru -NoNewWindow
            Write-Host "EVIDENCE probe-start pid=$($process.Id) unix_ms=$(Get-SpikeUnixMs)"
            if (-not $process.WaitForExit(300000)) {
                Get-VM -Name $name | Select-Object Name,State,Status,Uptime,CPUUsage,MemoryAssigned | Format-List | Out-Host
                Write-Host "EVIDENCE probe-timeout unix_ms=$(Get-SpikeUnixMs) pid=$($process.Id)"
                $process.Kill(); $process.WaitForExit()
                throw 'FAIL probe-timeout (300s total, includes boot/Hello/command/protocol completion)'
            }
            # Refresh the process handle before reading its exit status in PS 5.1.
            $process.Refresh()
            Get-Content $stdout,$stderr | Out-Host
            Assert-Spike ($process.ExitCode -eq 0) 'probe-exit' "run=$run exit=$($process.ExitCode)"
            $probe = Get-Content $result -Raw | ConvertFrom-Json
            $helloMs = [double]$probe.hello_unix_ms - $bootStart
            Assert-Spike ($helloMs -ge 0 -and $helloMs -le 120000) 'boot-to-validated-Hello' "run=$run ms=$helloMs"
        } finally {
            $teardown = [Diagnostics.Stopwatch]::StartNew()
            try {
                if ($process -and -not $process.HasExited) { $process.Kill(); $process.WaitForExit() }
                if (-not $probe) {
                    foreach ($path in @($stdout,$stderr)) { if (Test-Path -LiteralPath $path) { Get-Content -LiteralPath $path | Out-Host } }
                }
                if ($probe) { Wait-SpikeOff $name 60 }
            } finally {
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
            $teardownMs = $teardown.Elapsed.TotalMilliseconds
            if ($process) { $process.Dispose() }
        }
        Assert-Spike ($cleanupOkay -and $null -ne $probe) 'complete-sample' $run
        $row = [ordered]@{ create_ms=$createMs; boot_hello_ms=$helloMs; command_ms=[double]$probe.command_ms; teardown_ms=$teardownMs }
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
    Write-Host 'DIAGNOSTIC single run completed; five-run measurement not performed and no median reported'
    return
}
& $client --summary (Join-Path $batch 'samples.json') | Tee-Object -FilePath (Join-Path $batch 'summary.log') | Out-Host
Assert-Spike ($LASTEXITCODE -eq 0) 'five-run-summary' $batch
