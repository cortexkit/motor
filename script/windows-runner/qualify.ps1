<# Operator-only qualification. run-system.ps1 supplies the SYSTEM token. #>
param([Parameter(Mandatory)][string]$Python, [Parameter(Mandatory)][string]$Evidence)
$ErrorActionPreference = 'Stop'
Set-StrictMode -Version Latest
$Root = 'C:\ProgramData\MotorRunner'
. "$PSScriptRoot\common.ps1"
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
    $written = [regex]::Escape($match.Groups[3].Value)
    if ((Get-Content $disconnectLog -Raw) -notmatch "io read done bytes=0 total=$written(?:\D|$)") { throw 'Guest did not observe the deliberately truncated frame and EOF with the same byte count' }
    Write-Host 'PASS mid-frame-guest-observed-EOF-and-exact-partial-byte-count'
    # Two VMs are used only for peer rejection, not as a two-job throughput claim.
    # The attacker gets 2 GiB; fleet job concurrency remains exactly one.
    $victim = [Guid]::NewGuid().ToString()
    $attacker = [Guid]::NewGuid().ToString()
    $ids = @($victim,$attacker)
    $monitors = @()
    $caches = @()
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
        foreach ($id in $ids) {
            $arguments = @('-NoLogo','-NoProfile','-NonInteractive','-ExecutionPolicy','Bypass','-File',"$Root\bin\hyperv.ps1",'-Operation','Run','-Root',$Root,'-Job',$id)
            $monitors += Start-SpikeNativeProcess 'C:\Windows\System32\WindowsPowerShell\v1.0\powershell.exe' $arguments
        }
        Start-Sleep -Seconds 35
        $foreignVm = (Get-VM -Name "motor-ally-$attacker").Id.ToString('N')
        foreach ($id in $ids) { & "$Root\bin\hyperv.ps1" -Operation Harvest -Root $Root -Job $id }
        $nativeLog = Get-Content "$Root\evidence\$victim-guest\hyperv-agent.log" -Raw
        foreach ($peer in @('e0e16197dd564a1091955ee7a155a838', $foreignVm)) {
            if ($nativeLog -notmatch "hvsock refused peer vm_id=$peer") { throw "Victim has no native refusal for $peer; transport failure alone is not proof" }
        }
        if ($nativeLog -match 'hvsock accepted peer') { throw 'Victim admitted an untrusted partition' }
        foreach ($id in $ids) {
            $proof = @(Get-Content "$Root\evidence\$id-guest\peer-proof.json" -Raw | ConvertFrom-Json)
            if ($proof.Count -ne 1 -or -not $proof[0].transport_connected) { throw 'Peer probe never reached the victim transport' }
        }
        Write-Host 'PASS native-loopback-refused native-foreign-vm-refused no-agent-bytes-disclosed'
    } finally {
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
