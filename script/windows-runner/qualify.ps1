<# Operator-only qualification. run-system.ps1 supplies the SYSTEM token. #>
param([Parameter(Mandatory)][string]$Python, [Parameter(Mandatory)][string]$Evidence)
$ErrorActionPreference = 'Stop'
Set-StrictMode -Version Latest
$Root = 'C:\ProgramData\MotorRunner'
. "$PSScriptRoot\common.ps1"
. "$PSScriptRoot\qualify-peer.ps1"
if ([Security.Principal.WindowsIdentity]::GetCurrent().User.Value -ne 'S-1-5-18') { throw 'Use run-system.ps1; restricted process creation requires SYSTEM privileges' }
$env:HOME = "$Root\service-home"
$env:Path = 'C:\Windows\System32;C:\Windows;C:\Program Files\Git\cmd;C:\Program Files\Git\bin'
$exe = "$Root\bin\motor-runner.exe"
$config = @('--instance','ally','--state-root',"$Root\state",'--backend','windows-vm','--vm-config',"$Root\vm.toml")
function Runner-Check([string]$Operation) {
    $process = Start-SpikeNativeProcess $exe (@($Operation) + $config)
    if (-not $process.process.WaitForExit(180000)) { $process.process.Kill(); throw "$Operation timed out" }
    $result = Complete-SpikeNativeProcess $process "$Evidence\$Operation.stdout" "$Evidence\$Operation.stderr"
    Get-Content "$Evidence\$Operation.stdout","$Evidence\$Operation.stderr" | Out-Host
    if ($result.exitCode -ne 0) { throw "$Operation failed exit=$($result.exitCode)" }
    return $result.stdout
}
function Copy-QualificationEvidence {
    New-Item -ItemType Directory -Path "$Evidence\host-evidence" -Force | Out-Null
    Get-ChildItem -LiteralPath "$Root\evidence" -Force | Copy-Item -Destination "$Evidence\host-evidence" -Recurse -Force -ErrorAction Stop
    Copy-Item -LiteralPath "$Root\vm.toml" -Destination "$Evidence\vm.toml" -Force -ErrorAction Stop
}
$parent = "$Root\baselines\ally-hvsock.vhdx"
$parentHash = (Get-FileHash -LiteralPath $parent -Algorithm SHA256).Hash
$service = Get-Service MotorRunner
if ($service.Status -ne 'Stopped') { Stop-Service MotorRunner; $service.WaitForStatus('Stopped',[TimeSpan]::FromSeconds(40)) }
$evidenceCopied = $false
try {
    $null = Runner-Check 'qualify-prep'
    $disconnect = Runner-Check 'qualify-disconnect'
    $match = [regex]::Match($disconnect, 'job=([0-9a-f-]{36}) announced=(\d+) written=(\d+)')
    if (-not $match.Success -or [int]$match.Groups[3].Value -ge [int]$match.Groups[2].Value) { throw 'Disconnect did not stop in the middle of an announced agent frame' }
    $disconnectLog = "$Root\evidence\$($match.Groups[1].Value)-guest\hyperv-agent.log"
    if (-not (Test-Path -LiteralPath $disconnectLog)) { throw 'Verified guest disconnect log is missing' }
    $proofs = @(Get-Content "$Root\evidence\$($match.Groups[1].Value)-qualification.jsonl" -Encoding UTF8 | ForEach-Object { $_ | ConvertFrom-Json } | Where-Object event -eq 'guest-terminal-read-verified')
    if ($proofs.Count -ne 1 -or $proofs[0].detail.read_total -ne [int]$match.Groups[3].Value -or $proofs[0].detail.written -ne [int]$match.Groups[3].Value -or $proofs[0].detail.announced -ne [int]$match.Groups[2].Value -or $proofs[0].detail.termination -notin @('clean-eof','reader-aborted-10053','reader-reset-10054') -or -not $proofs[0].detail.no_frame_processed -or $proofs[0].detail.successful_reply_bytes_after_partial -ne 0) { throw 'Guest did not prove exact partial read, terminal disconnect and no processed frame/reply' }
    Write-Host 'PASS mid-frame-guest-observed-terminal-disconnect-and-exact-partial-byte-count-no-frame-processed'
    # Two VMs are used only for peer rejection, not as a two-job throughput claim.
    # The attacker gets 2 GiB; fleet job concurrency remains exactly one.
    $victim = [Guid]::NewGuid().ToString()
    $attacker = [Guid]::NewGuid().ToString()
    $ids = @($victim,$attacker)
    $monitors = @()
    $caches = @()
    $peerClock = [Diagnostics.Stopwatch]::StartNew()
    $timeline = [ordered]@{schema=1; hello_validated=$false}
    try {
        foreach ($id in $ids) {
            $sha = [Security.Cryptography.SHA256]::Create()
            try { $key = (($sha.ComputeHash([Text.Encoding]::ASCII.GetBytes($id)) | ForEach-Object { $_.ToString('x2') }) -join '') } finally { $sha.Dispose() }
            $cache = "$Root\vm\caches\$key.vhdx"
            $caches += $cache
            $target = if ($id -eq $victim) { 'e0e16197-dd56-4a10-9195-5ee7a155a838' } else { (Get-VM -Name "motor-ally-$victim").Id.Guid }
            $targets = '["' + ([Guid]::Parse([string]$target)).ToString('D') + '"]'
            $bootNonce = ([Guid]$id).ToString('N')
            & "$Root\bin\hyperv.ps1" -Operation Prepare -Root $Root -Job $id -Parent $parent -ParentSha256 $parentHash -Cache $cache -Nonce $bootNonce -PeerTargetsJson $targets
            if ($id -eq $attacker) { Set-VM -Name "motor-ally-$id" -ProcessorCount 2; Set-VMMemory -VMName "motor-ally-$id" -StartupBytes 2GB }
        }
        $victimVm = [Guid](Get-VM -Name "motor-ally-$victim").Id
        $attackerVm = [Guid](Get-VM -Name "motor-ally-$attacker").Id
        $timeline.victim_vm_id = $victimVm.ToString('D')
        $timeline.attacker_vm_id = $attackerVm.ToString('D')
        $timeline.victim_nonce = ([Guid]$victim).ToString('N')
        $timeline.attacker_nonce = ([Guid]$attacker).ToString('N')
        $victimStart = [DateTime]::UtcNow
        $timeline.victim_start_utc = $victimStart.ToString('o')
        $timeline.victim_start_ms = $peerClock.ElapsedMilliseconds
        $arguments = @('-NoLogo','-NoProfile','-NonInteractive','-ExecutionPolicy','Bypass','-File',"$Root\bin\hyperv.ps1",'-Operation','Run','-Root',$Root,'-Job',$victim)
        $monitors += Start-SpikeNativeProcess 'C:\Windows\System32\WindowsPowerShell\v1.0\powershell.exe' $arguments
        $boot = Wait-RunnerPeerBoot $victimVm $victimStart "$Evidence\peer-victim-boot.xml"
        $timeline.boot_utc = $boot.utc
        $timeline.boot_record_id = $boot.record_id
        $timeline.boot_event_id = 18601
        $timeline.boot_vm_id = $boot.vm_id
        $timeline.boot_observed_utc = [DateTime]::UtcNow.ToString('o')
        $timeline.boot_observed_ms = $peerClock.ElapsedMilliseconds
        # A host readiness connection would consume the guest's single listener.
        # Instead use a VM-matched host OS-boot event plus a fixed service-start margin.
        Start-Sleep -Seconds 20
        $attackerStart = [DateTime]::UtcNow
        $timeline.attacker_start_utc = $attackerStart.ToString('o')
        $timeline.attacker_start_ms = $peerClock.ElapsedMilliseconds
        $arguments = @('-NoLogo','-NoProfile','-NonInteractive','-ExecutionPolicy','Bypass','-File',"$Root\bin\hyperv.ps1",'-Operation','Run','-Root',$Root,'-Job',$attacker)
        $monitors += Start-SpikeNativeProcess 'C:\Windows\System32\WindowsPowerShell\v1.0\powershell.exe' $arguments
        $attackerBoot = Wait-RunnerPeerBoot $attackerVm $attackerStart "$Evidence\peer-attacker-boot.xml"
        $timeline.attacker_boot_utc = $attackerBoot.utc
        $timeline.attacker_boot_record_id = $attackerBoot.record_id
        # Retain a full qualification-only write-back window for the attacker
        # too. Its completed, synced proof is still mandatory after power-off;
        # elapsed time alone is never treated as probe completion.
        Start-Sleep -Seconds 60
        $timeline.attacker_settled_ms = $peerClock.ElapsedMilliseconds
        # Read completion only from the stopped attacker child; never mount a
        # running disk. The victim stays running and has had no host connection.
        & "$Root\bin\hyperv.ps1" -Operation Harvest -Root $Root -Job $attacker
        $attackerLog = [IO.File]::ReadAllText("$Root\evidence\$attacker-guest\hyperv-agent.log")
        if ($attackerLog -notmatch ("\bboot nonce=" + $timeline.attacker_nonce + '(?:\s|$)')) { throw 'Attacker proof has no current-nonce boot log' }
        $proof = ConvertFrom-Json -InputObject ([IO.File]::ReadAllText("$Root\evidence\$attacker-guest\peer-proof.json"))
        if (@($proof).Count -ne 1 -or [Guid]::Parse($proof[0].target) -ne $victimVm -or $proof[0].transport_connected -isnot [bool]) { throw 'Missing completed attacker proof for the current victim' }
        $timeline.proof_observed_utc = [DateTime]::UtcNow.ToString('o')
        $timeline.proof_observed_ms = $peerClock.ElapsedMilliseconds
        $timeline.attacker_transport_connected = $proof[0].transport_connected
        $timeline.host_connect_utc = [DateTime]::UtcNow.ToString('o')
        $timeline.host_connect_ms = $peerClock.ElapsedMilliseconds
        $hello = Receive-RunnerPeerHello $victimVm $timeline.victim_nonce "$Evidence\peer-host-hello.json"
        $timeline.hello_utc = [DateTime]::UtcNow.ToString('o')
        $timeline.hello_ms = $peerClock.ElapsedMilliseconds
        $timeline.hello_nonce = $hello.nonce
        $timeline.hello_validated = $true
        # Qualification only: as with the disconnect oracle, retain the full
        # window for guest log write-back before victim power-off. This is not
        # a durability guarantee and production never waits on persisted logs.
        Start-Sleep -Seconds 60
        $timeline.settled_ms = $peerClock.ElapsedMilliseconds
        & "$Root\bin\hyperv.ps1" -Operation Harvest -Root $Root -Job $victim
        $nativeLog = [IO.File]::ReadAllText("$Root\evidence\$victim-guest\hyperv-agent.log")
        $timeline.result = Assert-RunnerPeerEvidence $timeline.victim_nonce $victimVm $attackerVm $proof $timeline $nativeLog
        Write-Host "PASS native-loopback-refused $($timeline.result) no-agent-bytes-disclosed"
    } finally {
        [IO.File]::WriteAllText("$Evidence\peer-timeline.json", (ConvertTo-Json -InputObject $timeline -Depth 5), [Text.UTF8Encoding]::new($false))
        # A late-starting monitor must not boot a VM during offline collection.
        # Normal monitors have already exited after the explicit Harvest above.
        foreach ($monitor in $monitors) {
            if (-not $monitor.process.WaitForExit(5000)) {
                $monitor.process.Kill()
                if (-not $monitor.process.WaitForExit(5000)) { throw 'Peer monitor did not terminate; preserving VM disks rather than inspecting a live guest' }
            }
        }
        foreach ($id in $ids) { & "$Root\bin\hyperv.ps1" -Operation Remove -Root $Root -Job $id }
        foreach ($monitor in $monitors) {
            if (-not $monitor.process.WaitForExit(120000)) { $monitor.process.Kill(); throw 'Peer VM monitor did not exit' }
            $result = Complete-SpikeNativeProcess $monitor "$Evidence\peer-monitor-$($monitor.process.Id).stdout" "$Evidence\peer-monitor-$($monitor.process.Id).stderr"
            if ($result.exitCode -ne 0) { throw 'Host isolation monitor failed during peer qualification' }
        }
        foreach ($cache in $caches) { if (Test-Path -LiteralPath $cache) { Remove-Item -LiteralPath $cache -Force } }
    }
    Start-Service MotorRunner
    & $Python "$Root\bin\qualify-jobs.py" --root $Root --evidence "$Evidence\real-jobs"
    if ($LASTEXITCODE -ne 0) { throw 'Real runner protocol/cache/output qualification failed' }
    if (@(Get-VM -Name 'motor-ally-*' -ErrorAction SilentlyContinue).Count -or @(Get-ChildItem "$Root\vm\jobs" -Force).Count -or @(Get-ChildItem "$Root\vm\caches" -Filter '*.attached').Count) { throw 'Qualification left a job VM, disk directory or attachment marker' }
    if ((Get-FileHash -LiteralPath $parent -Algorithm SHA256).Hash -ne $parentHash) { throw 'Sealed parent changed during qualification' }
    Write-Host "PASS final-nothing-left-behind parent-sha256=$parentHash"
    Copy-QualificationEvidence
    $evidenceCopied = $true
    Set-Content "$Evidence\QUALIFIED-SINGLE-SLOT.txt" "Single slot only; parent SHA256=$parentHash; review raw evidence before enabling fleet routing" -Encoding ascii
} catch {
    Stop-Service MotorRunner -ErrorAction SilentlyContinue
    throw
} finally {
    # Failed checks need the same external guest/host evidence as a passing run.
    if (-not $evidenceCopied) {
        try { Copy-QualificationEvidence } catch {
            # Evidence storage may itself be unavailable; preserve the check's
            # original exception even if recording the copy failure also fails.
            try { $_ | Out-String | Set-Content -LiteralPath "$Evidence\host-evidence-copy-error.txt" -Encoding UTF8 } catch {}
            Write-Warning 'Host evidence copy failed; inspect the installed runner evidence directory' -WarningAction Continue
        }
    }
}
