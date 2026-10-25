<#
Run elevated Windows PowerShell 5.1 on an installed native runner. No new binary
or source checkout is needed. Installed Prepare supplies the exact production VM,
differencing disk, nonce DVD, cache and isolation settings. The embedded probe
uses the same endpoint and 250-ms connect option as the Rust host, sends nothing,
and receives only a bounded Hello if connected. Evidence is retained externally;
the disposable VM/disks are removed only after stopped-disk log collection.
#>
param([string]$Evidence = ('C:\motor-runner-evidence\connect-' + (Get-Date -Format 'yyyyMMdd-HHmmss')))
$ErrorActionPreference = 'Stop'
Set-StrictMode -Version Latest
$Root = 'C:\ProgramData\MotorRunner'
$principal = [Security.Principal.WindowsPrincipal][Security.Principal.WindowsIdentity]::GetCurrent()
if (-not $principal.IsInRole([Security.Principal.WindowsBuiltInRole]::Administrator)) { throw 'Use an elevated administrator PowerShell shell' }
if ($Evidence -notmatch '^C:\\motor-runner-evidence\\[A-Za-z0-9-]+$') { throw 'Use a fresh direct child of C:\motor-runner-evidence' }
if (Test-Path -LiteralPath $Evidence) { throw 'Evidence directory already exists; use a fresh name' }
if (-not (Test-Path -LiteralPath "$Root\.motor-runner-root")) { throw 'Installed runner root marker is missing' }
foreach ($path in @('C:\ProgramData', $Root, "$Root\bin", "$Root\state", "$Root\baselines", "$Root\vm", "$Root\vm\jobs", "$Root\vm\caches", 'C:\motor-runner-evidence')) {
    if ((Test-Path -LiteralPath $path) -and ((Get-Item -LiteralPath $path -Force).Attributes -band [IO.FileAttributes]::ReparsePoint)) { throw "Reparse directory refused: $path" }
}
Import-Module Hyper-V
. "$Root\bin\parent-identity.ps1"
New-Item -ItemType Directory -Path $Evidence -Force | Out-Null
& icacls.exe $Evidence /inheritance:r /grant:r '*S-1-5-18:(OI)(CI)F' '*S-1-5-32-544:(OI)(CI)F' /Q | Out-Null
if ($LASTEXITCODE -ne 0) { throw 'Cannot protect diagnostic evidence directory' }

if (-not ('Motor.ConnectDiagnostic' -as [type])) {
    Add-Type -TypeDefinition @'
using System;
using System.ComponentModel;
using System.Diagnostics;
using System.IO;
using System.Runtime.InteropServices;

namespace Motor {
    public sealed class ConnectDiagnosticResult {
        public bool Connected;
        public int Error;
        public string Stage;
        public double ElapsedMs;
        public int ReceiveError;
        public byte[] Hello = new byte[0];
    }
    public static class ConnectDiagnostic {
        [StructLayout(LayoutKind.Sequential)]
        struct Address { public ushort Family, Reserved; public Guid VmId, ServiceId; }
        [DllImport("ws2_32.dll", ExactSpelling = true)]
        static extern int WSAStartup(ushort version, IntPtr data);
        [DllImport("ws2_32.dll", ExactSpelling = true)]
        static extern int WSACleanup();
        [DllImport("ws2_32.dll", ExactSpelling = true)]
        static extern IntPtr socket(int family, int type, int protocol);
        [DllImport("ws2_32.dll", ExactSpelling = true)]
        static extern int setsockopt(IntPtr socket, int level, int option, ref uint value, int length);
        [DllImport("ws2_32.dll", ExactSpelling = true)]
        static extern int connect(IntPtr socket, ref Address address, int length);
        [DllImport("ws2_32.dll", ExactSpelling = true)]
        static extern int recv(IntPtr socket, [Out] byte[] buffer, int length, int flags);
        [DllImport("ws2_32.dll", ExactSpelling = true)]
        static extern int closesocket(IntPtr socket);
        [DllImport("ws2_32.dll", ExactSpelling = true)]
        static extern int WSAGetLastError();

        public static ConnectDiagnosticResult Attempt(Guid vm, Guid service, uint timeoutMs) {
            var result = new ConnectDiagnosticResult();
            var timer = Stopwatch.StartNew();
            var data = Marshal.AllocHGlobal(512); // WSADATA fits on Windows x86 and x64.
            bool started = false;
            IntPtr handle = new IntPtr(-1);
            try {
                result.Stage = "WSAStartup";
                result.Error = WSAStartup(0x0202, data);
                if (result.Error != 0) return result;
                started = true;
                result.Stage = "socket";
                // AF_HYPERV=34, SOCK_STREAM=1, HV_PROTOCOL_RAW=1. No creation flags,
                // matching crates/motor-runner/src/windows/hvsock.rs::connect.
                handle = socket(34, 1, 1);
                if (handle == new IntPtr(-1)) { result.Error = WSAGetLastError(); return result; }
                result.Stage = "setsockopt-connect-timeout";
                // HV_PROTOCOL_RAW level, HVSOCKET_CONNECT_TIMEOUT option: both 1.
                if (setsockopt(handle, 1, 1, ref timeoutMs, 4) != 0) {
                    result.Error = WSAGetLastError(); return result;
                }
                var address = new Address { Family = 34, Reserved = 0, VmId = vm, ServiceId = service };
                result.Stage = "connect";
                if (connect(handle, ref address, Marshal.SizeOf(typeof(Address))) != 0) {
                    result.Error = WSAGetLastError(); return result;
                }
                result.Connected = true;
                result.Stage = "connected";
                // Passive receive only: never send a job frame or execute a guest command.
                uint receiveTimeout = 2000;
                if (setsockopt(handle, 0xffff, 0x1006, ref receiveTimeout, 4) != 0) {
                    result.ReceiveError = WSAGetLastError(); return result;
                }
                var receiveTimer = Stopwatch.StartNew();
                using (var bytes = new MemoryStream()) {
                    var buffer = new byte[4096];
                    while (receiveTimer.ElapsedMilliseconds < 2000 && bytes.Length < 65536) {
                        receiveTimeout = (uint)Math.Max(1, 2000 - receiveTimer.ElapsedMilliseconds);
                        if (setsockopt(handle, 0xffff, 0x1006, ref receiveTimeout, 4) != 0) {
                            result.ReceiveError = WSAGetLastError(); break;
                        }
                        int count = recv(handle, buffer, (int)Math.Min(buffer.Length, 65536 - bytes.Length), 0);
                        if (count < 0) { result.ReceiveError = WSAGetLastError(); break; }
                        if (count == 0) break;
                        bytes.Write(buffer, 0, count);
                        if (bytes.Length >= 4) {
                            var header = bytes.GetBuffer();
                            uint length = ((uint)header[0] << 24) | ((uint)header[1] << 16) | ((uint)header[2] << 8) | header[3];
                            if (length > 65532) break;
                            if (bytes.Length >= length + 4) { bytes.SetLength(length + 4); break; }
                        }
                    }
                    result.Hello = bytes.ToArray();
                }
                return result;
            } finally {
                if (handle != new IntPtr(-1)) closesocket(handle);
                if (started) WSACleanup();
                Marshal.FreeHGlobal(data);
                timer.Stop();
                result.ElapsedMs = timer.Elapsed.TotalMilliseconds;
            }
        }
        public static string ErrorText(int code) {
            return code == 0 ? "" : new Win32Exception(code).Message;
        }
    }
}
'@
}

. "$PSScriptRoot\guest-evidence.ps1"
function Save-HostVmEvents([string]$VmId, [DateTime]$Started) {
    try {
        $events = @()
        $logs = @(Get-WinEvent -ListLog '*Hyper-V*' -ErrorAction SilentlyContinue | Where-Object { $_.IsEnabled -and $_.LogName -match 'Hyper-V-(Worker|VMMS)' })
        foreach ($log in $logs) {
            try {
                $events += @(Get-WinEvent -FilterHashtable @{LogName=$log.LogName; StartTime=$Started.AddSeconds(-10)} -ErrorAction Stop | Where-Object { $_.ToXml() -match [regex]::Escape($VmId) } | ForEach-Object { Convert-DiagnosticEvent $_ })
            } catch { Add-Content -LiteralPath "$Evidence\host-event-query-status.txt" -Value "$($log.LogName): $_" -Encoding UTF8 }
        }
        Save-Json "$Evidence\host-vm-events.json" $events
    } catch { Set-Content -LiteralPath "$Evidence\host-event-error.txt" -Value ($_ | Out-String) -Encoding UTF8 }
}

$job = [Guid]::NewGuid().ToString()
$nonce = ([Guid]$job).ToString('N')
$parent = "$Root\baselines\ally-hvsock.vhdx"
$serviceId = [Guid]'92c268b0-cddd-40a4-a664-f32e50ad35ea'
$sha = [Security.Cryptography.SHA256]::Create()
try { $cacheKey = -join ($sha.ComputeHash([Text.Encoding]::ASCII.GetBytes($job)) | ForEach-Object { $_.ToString('x2') }) } finally { $sha.Dispose() }
$cache = "$Root\vm\caches\$cacheKey.vhdx"
$directory = "$Root\vm\jobs\$job"
$vmName = "motor-ally-$job"
$vmId = $null
$parentGuard = $null
$prepared = $false
$cacheOwned = $false
$stoppedService = $false
$restartedService = $false
$connected = $false
$helloBytes = 0
$receiveError = $null
$harvested = $false
$removed = $false
$attempts = 0
$issues = [Collections.Generic.List[string]]::new()
$startedUtc = [DateTime]::UtcNow
$transcript = $false
try {
    Start-Transcript -Path "$Evidence\host-transcript.log" | Out-Null
    $transcript = $true
    @'
host-attempts.jsonl: every bounded connect attempt, native stage/error, elapsed time,
  and passive Hello receive result. No job frame is sent. Hello EOF is expected.
host-hello.bin / host-hello.txt: bounded raw first frame, if any; compare protocol,
  nonce and build information with the baked guest's logs.
vm-settings.json: exact production Prepare configuration and attached disk/DVD paths.
host-vm-events.json: this VM's Worker/VMMS lifecycle events since the diagnostic began.
guest/guest-paths.json: explicit log/marker paths, presence, expected meaning and any
  refused reparse or copy error. An absent agent log is evidence, not silently skipped.
guest/mounted-disk-identity.json: requested/mapped VHD paths, disk number and reverse
  partition/drive-letter/volume identities; a mismatch refuses guest inspection.
guest/guest-agent-binary.json: actual baked agent digest, never executed on the host.
host-agent-binary.json: installed release's agent digest for comparison.
guest/files/ProgramData/Motor/hyperv-agent.log: earliest agent identity and startup,
  nonce/DVD and socket bind/accept/peer/I/O progress; absence may mean the service
  never entered its diagnostic path, the wrong build is installed, or startup failed.
guest/files/build/provision/*: bootstrap/finalization/update/probe logs and markers;
  each path's expected purpose is recorded in guest-paths.json.
guest/events/*-agent-events.json and *-agent-*.evtx: System SCM and Application
  crash/WER records whose XML mentions motor-winagent or its display name. Raw XML
  is retained even when host message resources cannot render guest messages.
guest/events/guest-boot-context.json: separate recent boot/shutdown context, not
  misrepresented as agent-only events. Guest clocks are not assumed synchronized.
summary.json: connection, harvest and cleanup outcomes, with every lifecycle error.
No guest password/DPAPI files, registry hives, memory dumps or complete disk are copied.
'@ | Set-Content -LiteralPath "$Evidence\EXPECTED-LOGS.txt" -Encoding UTF8
    if (@(Get-VM -Name 'motor-ally-*' -ErrorAction SilentlyContinue).Count -or @(Get-ChildItem -LiteralPath "$Root\vm\jobs" -Force).Count) { throw 'Existing runner VM/job resources found; drain and clean them before this diagnostic' }
    $service = Get-Service MotorRunner
    if ($service.Status -ne 'Stopped') {
        $stoppedService = $true
        Stop-Service MotorRunner
        $service.WaitForStatus('Stopped', [TimeSpan]::FromSeconds(40))
    }
    if (Test-Path -LiteralPath $cache) { throw 'Fresh diagnostic cache path already exists' }
    $cacheOwned = $true
    $config = Get-Content -LiteralPath "$Root\vm.toml" -Raw
    $hashMatch = [regex]::Match($config, '(?m)^\s*parent_sha256\s*=\s*[''"]([0-9a-fA-F]{64})[''"]\s*$')
    if (-not $hashMatch.Success) { throw 'Cannot read the configured parent SHA-256' }
    $parentHash = $hashMatch.Groups[1].Value
    $parentGuard = Open-RunnerVerifiedParent $parent $parentHash "$Root\state\parent-verified.json"
    Save-Json "$Evidence\diagnostic.json" @{job=$job; vm_name=$vmName; parent=$parent; parent_sha256=$parentHash; nonce=$nonce; cache=$cache; service_id=$serviceId.ToString(); connect_attempt_ms=250; boot_deadline_seconds=120; started_utc=$startedUtc.ToString('o')}
    Save-Json "$Evidence\host-agent-binary.json" @{path="$Root\bin\motor-winagent.exe"; sha256=(Get-FileHash -LiteralPath "$Root\bin\motor-winagent.exe" -Algorithm SHA256).Hash}
    # Use installed production setup rather than duplicating its isolation settings.
    $prepared = $true
    & "$Root\bin\hyperv.ps1" -Operation Prepare -Root $Root -Job $job -Parent $parent -ParentSha256 $parentHash -Cache $cache -Nonce $nonce
    $vm = Get-VM -Name $vmName
    $vmId = $vm.Id.ToString()
    Save-Json "$Evidence\vm-settings.json" @{
        vm=($vm | Select-Object Name,Id,Generation,State,ProcessorCount,AutomaticStartAction,AutomaticStopAction,CheckpointType,AutomaticCheckpointsEnabled)
        memory=(Get-VMMemory -VM $vm | Select-Object DynamicMemoryEnabled,Startup,Minimum,Maximum)
        firmware=(Get-VMFirmware -VM $vm | Select-Object SecureBoot,SecureBootTemplate)
        disks=@(Get-VMHardDiskDrive -VM $vm | Select-Object ControllerType,ControllerNumber,ControllerLocation,Path)
        dvd=@(Get-VMDvdDrive -VM $vm | Select-Object Path,ControllerNumber,ControllerLocation)
        network_adapters=@(Get-VMNetworkAdapter -VM $vm | Select-Object Name,SwitchName)
        integration_services=@(Get-VMIntegrationService -VM $vm | Select-Object Name,Enabled)
        com_ports=@(Get-VMComPort -VMName $vmName | Select-Object Name,Path)
    }
    Start-VM -VM $vm
    $clock = [Diagnostics.Stopwatch]::StartNew()
    while ($clock.Elapsed.TotalSeconds -lt 120) {
        $attempts++
        $remaining = [Math]::Max(1, [Math]::Min(250, 120000 - $clock.ElapsedMilliseconds))
        $reply = [Motor.ConnectDiagnostic]::Attempt([Guid]$vmId, $serviceId, [uint32]$remaining)
        @{utc=[DateTime]::UtcNow.ToString('o'); attempt=$attempts; since_start_ms=$clock.Elapsed.TotalMilliseconds; connected=$reply.Connected; stage=$reply.Stage; error=$reply.Error; message=[Motor.ConnectDiagnostic]::ErrorText($reply.Error); elapsed_ms=$reply.ElapsedMs; receive_error=$reply.ReceiveError; hello_bytes=$reply.Hello.Length} | ConvertTo-Json -Compress | Add-Content -LiteralPath "$Evidence\host-attempts.jsonl" -Encoding UTF8
        if ($reply.Connected) {
            $connected = $true
            $helloBytes = $reply.Hello.Length
            $receiveError = $reply.ReceiveError
            [IO.File]::WriteAllBytes("$Evidence\host-hello.bin", $reply.Hello)
            $textOffset = if ($reply.Hello.Length -ge 4) { 4 } else { 0 }
            [IO.File]::WriteAllText("$Evidence\host-hello.txt", [Text.Encoding]::UTF8.GetString($reply.Hello, $textOffset, $reply.Hello.Length - $textOffset), [Text.UTF8Encoding]::new($false))
            break
        }
        Start-Sleep -Milliseconds 100
    }
} catch {
    $issues.Add([string]$_)
    $_ | Out-String | Set-Content -LiteralPath "$Evidence\host-error.txt" -Encoding UTF8
} finally {
    # Harvest the stopped child before Remove can delete its only guest logs.
    try {
        $vm = Get-VM -Name $vmName -ErrorAction SilentlyContinue
        if ($vm -and -not $vmId) { $vmId = $vm.Id.ToString() }
        if ($vm -and $vm.State -ne 'Off') { Stop-VM -VM $vm -TurnOff -Force }
        $vm = Get-VM -Name $vmName -ErrorAction SilentlyContinue
        if ($vm -and $vm.State -ne 'Off') { throw 'VM is not Off; refusing to mount a running guest disk' }
        if (Test-Path -LiteralPath "$directory\os.vhdx") {
            Save-DiagnosticGuestEvidence "$directory\os.vhdx" "$Evidence\guest"
            $harvested = $true
        } else { Save-Json "$Evidence\guest-disk-state.json" @{present=$false; path="$directory\os.vhdx"} }
    } catch {
        $issues.Add("guest harvest: $_")
        $_ | Out-String | Set-Content -LiteralPath "$Evidence\guest-harvest-error.txt" -Encoding UTF8
    }
    try {
        if ($prepared) { & "$Root\bin\hyperv.ps1" -Operation Remove -Root $Root -Job $job }
        if ($cacheOwned -and (Test-Path -LiteralPath $cache)) { Remove-Item -LiteralPath $cache -Force }
        $removed = -not ((Get-VM -Name $vmName -ErrorAction SilentlyContinue) -or (Test-Path -LiteralPath $directory) -or ($cacheOwned -and (Test-Path -LiteralPath $cache)))
    } catch {
        $issues.Add("cleanup: $_")
        $_ | Out-String | Set-Content -LiteralPath "$Evidence\cleanup-error.txt" -Encoding UTF8
    }
    if ($vmId) { Save-HostVmEvents $vmId $startedUtc }
    if ($parentGuard) { $parentGuard.Dispose() }
    if ($stoppedService -and $removed) {
        try { Start-Service MotorRunner; $restartedService = $true } catch { $issues.Add("service restart: $_") }
    }
    Save-Json "$Evidence\summary.json" @{job=$job; vm_id=$vmId; connected=$connected; hello_bytes=$helloBytes; receive_error=$receiveError; attempts=$attempts; guest_harvest_attempt_succeeded=$harvested; resources_removed=$removed; restarted_previous_service=$restartedService; ended_utc=[DateTime]::UtcNow.ToString('o'); issues=$issues.ToArray(); evidence=$Evidence}
    if ($transcript) { Stop-Transcript | Out-Null }
}
Write-Host "Diagnostic evidence: $Evidence (connected=$connected harvested=$harvested removed=$removed)"
if (-not $connected -or -not $harvested -or -not $removed -or $issues.Count) { throw "Diagnostic did not fully succeed; return the complete $Evidence directory, including summary.json" }
