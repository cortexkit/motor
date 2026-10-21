# Qualification only: never connect before the sibling probe. The baked agent
# consumes its one listener on the first host accept, so a readiness dial would
# manufacture the very transport failure this check must distinguish.
function Convert-RunnerPeerHello([byte[]]$Bytes, [string]$Nonce) {
    if ($Bytes.Length -lt 4) { throw 'Missing peer qualification Hello header' }
    $length = [long]$Bytes[0] * 16777216 + [long]$Bytes[1] * 65536 + [long]$Bytes[2] * 256 + [long]$Bytes[3]
    if ($length -lt 1 -or $length -gt 65532 -or $Bytes.Length -ne $length + 4) { throw 'Invalid peer qualification Hello length' }
    $text = [Text.UTF8Encoding]::new($false, $true).GetString($Bytes, 4, [int]$length)
    $hello = ConvertFrom-Json -InputObject $text
    $keys = @($hello.PSObject.Properties.Name | Sort-Object)
    if (($keys -join ',') -cne 'kind,nonce,version' -or $hello.kind -cne 'hello' -or ($hello.version -isnot [int] -and $hello.version -isnot [long]) -or $hello.version -ne 4 -or $hello.nonce -cne $Nonce) { throw 'Invalid peer qualification Hello version, shape or nonce' }
    return $hello
}

function Assert-RunnerPeerEvidence([string]$Nonce, [Guid]$VictimVm, [Guid]$AttackerVm, $Proof, [Collections.IDictionary]$Timeline, [string]$Log) {
    if (-not $Timeline.Contains('hello_validated') -or $Timeline.hello_validated -ne $true) { throw 'Victim unavailable; isolation not proven' }
    foreach ($key in @('victim_start_ms','boot_observed_ms','attacker_start_ms','proof_observed_ms','host_connect_ms','hello_ms','settled_ms','victim_start_utc','boot_utc','boot_observed_utc','attacker_start_utc','proof_observed_utc','host_connect_utc','hello_utc')) {
        if (-not $Timeline.Contains($key)) { throw "Missing host peer ordering evidence: $key" }
    }
    if ($Timeline.boot_event_id -ne 18601 -or $Timeline.boot_record_id -le 0 -or $Timeline.boot_vm_id -cne $VictimVm.ToString('D')) { throw 'Missing VM-matched host OS-boot event' }
    if ($Timeline.boot_observed_ms -lt $Timeline.victim_start_ms -or $Timeline.attacker_start_ms -lt $Timeline.boot_observed_ms + 20000 -or [DateTimeOffset]::Parse($Timeline.boot_utc) -lt [DateTimeOffset]::Parse($Timeline.victim_start_utc).AddSeconds(-2) -or [DateTimeOffset]::Parse($Timeline.boot_observed_utc) -lt [DateTimeOffset]::Parse($Timeline.boot_utc) -or [DateTimeOffset]::Parse($Timeline.attacker_start_utc) -lt [DateTimeOffset]::Parse($Timeline.boot_observed_utc).AddSeconds(20)) { throw 'Attacker started before victim boot event plus 20-second margin' }
    $ordered = @('attacker_start','proof_observed','host_connect','hello')
    for ($i = 1; $i -lt $ordered.Count; $i++) {
        if ($Timeline[($ordered[$i] + '_ms')] -lt $Timeline[($ordered[$i-1] + '_ms')] -or [DateTimeOffset]::Parse($Timeline[($ordered[$i] + '_utc')]) -lt [DateTimeOffset]::Parse($Timeline[($ordered[$i-1] + '_utc')])) { throw 'Invalid host peer attempt/Hello ordering' }
    }
    if ($Timeline.settled_ms -lt $Timeline.hello_ms + 60000) { throw 'Peer qualification did not retain the full 60-second settle window' }
    if ($Timeline.hello_nonce -cne $Nonce -or $Timeline.victim_vm_id -cne $VictimVm.ToString('D') -or $Timeline.attacker_vm_id -cne $AttackerVm.ToString('D')) { throw 'Peer evidence belongs to another VM or nonce' }
    $entries = @($Proof)
    if ($entries.Count -ne 1 -or $entries[0].target -isnot [string] -or [Guid]::Parse($entries[0].target) -ne $VictimVm -or $entries[0].transport_connected -isnot [bool]) { throw 'Missing completed attacker proof for the current victim' }
    $current = $false; $boots = 0; $listens = 0; $loopback = $false; $foreignRefused = $false; $accepts = 0
    foreach ($line in ($Log -split '\r?\n')) {
        if ($line -match '\bboot nonce=(\S+)\s*$') {
            $current = $Matches[1] -ceq $Nonce
            if ($current) { $boots++ } else { $boots = 0; $listens = 0; $loopback = $false; $foreignRefused = $false; $accepts = 0 }
        } elseif ($current) {
            if ($line -match '\bhvsock listening\b') { $listens++ }
            if ($line -match '\bhvsock refused peer vm_id=([0-9a-f]{32})(?:\s|$)') {
                if ($listens -ne 1 -or $accepts -ne 0) { throw 'Peer refusal is outside the single-listener window' }
                if ($Matches[1] -ceq 'e0e16197dd564a1091955ee7a155a838') { $loopback = $true }
                if ($Matches[1] -ceq $AttackerVm.ToString('N')) { $foreignRefused = $true }
            }
            if ($line -match '\bhvsock accepted peer vm_id=([0-9a-f]{32})(?:\s|$)') {
                if ($Matches[1] -cne 'a42e7cdad03f480c9cc2a4de20abb878') { throw 'Victim accepted a non-parent peer' }
                if ($listens -ne 1 -or -not $loopback) { throw 'Parent accept was not preceded by listening and native loopback refusal' }
                $accepts++
            }
        }
    }
    if (-not $current -or $boots -ne 1 -or $listens -ne 1 -or -not $loopback -or $accepts -ne 1) { throw 'Victim must show one current-nonce listener, native loopback refusal, and exactly one parent accept' }
    if ($entries[0].transport_connected -and -not $foreignRefused) { throw 'Connected attacker has no native refusal in the victim log' }
    # A failed sibling connect is not a native refusal. Boot-plus-margin, the
    # completed offline attacker proof, and the one subsequent host Hello show
    # that this attempt did not consume or reach the single listener. No guest
    # timestamp is compared with a host clock, and no sibling-routing policy is assumed.
    if ($entries[0].transport_connected) { return 'native-foreign-vm-refused' }
    return 'cross-vm-attempt-did-not-consume-single-listener'
}

function Save-RunnerPeerJson([string]$Path, $Value) {
    [IO.File]::WriteAllText($Path, (ConvertTo-Json -InputObject $Value -Depth 10), [Text.UTF8Encoding]::new($false))
}

function Get-RunnerPeerBootMatch([string]$EventXml, [Guid]$VmId, [DateTime]$Since) {
    $Since = $Since.ToUniversalTime()
    $xml = [xml]$EventXml
    $system = $xml.SelectSingleNode("/*[local-name()='Event']/*[local-name()='System']")
    $id = $system.SelectSingleNode("*[local-name()='EventID']")
    $channel = $system.SelectSingleNode("*[local-name()='Channel']")
    if (-not $id -or $id.InnerText -ne '18601' -or -not $channel -or $channel.InnerText -cne 'Microsoft-Windows-Hyper-V-Worker-Admin') { return $null }
    # Chipset OS-boot events use namespaced UserData, not EventData/Properties.
    # GUID parsing handles uppercase and braces without localized message matching.
    $node = $xml.SelectSingleNode("/*[local-name()='Event']/*[local-name()='UserData']/*[local-name()='VmlEventLog']/*[local-name()='VmId']")
    $actual = [Guid]::Empty
    if (-not $node -or -not [Guid]::TryParse($node.InnerText, [ref]$actual) -or $actual -ne $VmId) { return $null }
    $created = $system.SelectSingleNode("*[local-name()='TimeCreated']")
    $utc = [DateTimeOffset]::Parse($created.GetAttribute('SystemTime')).UtcDateTime
    # Allow event timestamps up to two seconds before the recorded host start;
    # only this fresh VM's GUID can match. The 20s service margin is unchanged.
    if ($utc -lt $Since.AddSeconds(-2)) { return $null }
    $record = $system.SelectSingleNode("*[local-name()='EventRecordID']")
    return @{utc=$utc.ToString('o'); record_id=[long]$record.InnerText; vm_id=$actual.ToString('D')}
}

function Wait-RunnerPeerBoot([Guid]$VmId, [DateTime]$Since, [string]$EvidencePath) {
    $Since = $Since.ToUniversalTime()
    $clock = [Diagnostics.Stopwatch]::StartNew()
    $queries = [Collections.Generic.List[object]]::new()
    $candidates = @{}
    $waitError = $null
    try {
        while ($clock.Elapsed.TotalSeconds -lt 90) {
            $query = @{utc=[DateTime]::UtcNow.ToString('o'); count=0; error=$null}
            $queries.Add($query)
            try {
                $events = @(Get-WinEvent -FilterHashtable @{LogName='Microsoft-Windows-Hyper-V-Worker-Admin'; Id=18601; StartTime=$Since.AddSeconds(-2).ToLocalTime()} -ErrorAction Stop)
            } catch {
                $query.error = [string]$_
                if ($_.FullyQualifiedErrorId -notlike 'NoMatchingEventsFound*') { throw "Hyper-V OS-boot event query failed: $_" }
                $events = @()
            }
            $query.count = $events.Count
            foreach ($event in $events) {
                $text = $event.ToXml()
                $candidates[[string]$event.RecordId] = $text
                $match = Get-RunnerPeerBootMatch $text $VmId $Since
                if ($match) {
                    [IO.File]::WriteAllText($EvidencePath, $text, [Text.UTF8Encoding]::new($false))
                    return $match
                }
            }
            Start-Sleep -Milliseconds 250
        }
        throw "No current VM-matched Hyper-V Worker/Admin 18601 OS-boot event for $VmId within 90 seconds"
    } catch {
        $waitError = $_
        throw
    } finally {
        try { Save-RunnerPeerJson ($EvidencePath + '.queries.json') @{vm_id=$VmId.ToString('D'); since_utc=$Since.ToString('o'); clock_tolerance_seconds=2; queries=$queries.ToArray(); candidates=$candidates} } catch {
            [Console]::Error.WriteLine("Boot query evidence write failed: $_")
            if ($null -eq $waitError) { throw }
        }
    }
}

function Initialize-RunnerPeerSocket {
    if ('Motor.QualificationSocket' -as [type]) { return }
    Add-Type -TypeDefinition @'
using System;
using System.Diagnostics;
using System.IO;
using System.Runtime.InteropServices;
namespace Motor {
    public sealed class PeerHelloResult {
        public bool Connected;
        public int Error;
        public int ReceiveError;
        public byte[] Hello = new byte[0];
    }
    public static class QualificationSocket {
        [StructLayout(LayoutKind.Sequential)]
        struct Address { public ushort Family, Reserved; public Guid VmId, ServiceId; }
        [DllImport("ws2_32.dll", ExactSpelling=true)] static extern int WSAStartup(ushort version, IntPtr data);
        [DllImport("ws2_32.dll", ExactSpelling=true)] static extern int WSACleanup();
        [DllImport("ws2_32.dll", ExactSpelling=true)] static extern IntPtr socket(int family, int type, int protocol);
        [DllImport("ws2_32.dll", ExactSpelling=true)] static extern int setsockopt(IntPtr socket, int level, int option, ref uint value, int length);
        [DllImport("ws2_32.dll", ExactSpelling=true)] static extern int connect(IntPtr socket, ref Address address, int length);
        [DllImport("ws2_32.dll", ExactSpelling=true)] static extern int recv(IntPtr socket, [Out] byte[] buffer, int length, int flags);
        [DllImport("ws2_32.dll", ExactSpelling=true)] static extern int closesocket(IntPtr socket);
        [DllImport("ws2_32.dll", ExactSpelling=true)] static extern int WSAGetLastError();
        public static PeerHelloResult Attempt(Guid vm) {
            var result = new PeerHelloResult();
            var data = Marshal.AllocHGlobal(512);
            bool started = false;
            IntPtr handle = new IntPtr(-1);
            try {
                result.Error = WSAStartup(0x0202, data);
                if (result.Error != 0) return result;
                started = true;
                // Match the Rust host: AF_HYPERV, SOCK_STREAM, HV_PROTOCOL_RAW.
                handle = socket(34, 1, 1);
                if (handle == new IntPtr(-1)) { result.Error = WSAGetLastError(); return result; }
                uint timeout = 250;
                if (setsockopt(handle, 1, 1, ref timeout, 4) != 0) { result.Error = WSAGetLastError(); return result; }
                var address = new Address { Family=34, VmId=vm, ServiceId=new Guid("92c268b0-cddd-40a4-a664-f32e50ad35ea") };
                if (connect(handle, ref address, Marshal.SizeOf(typeof(Address))) != 0) { result.Error = WSAGetLastError(); return result; }
                result.Connected = true;
                var timer = Stopwatch.StartNew();
                using (var bytes = new MemoryStream()) {
                    var buffer = new byte[4096];
                    while (timer.ElapsedMilliseconds < 2000 && bytes.Length < 65536) {
                        timeout = (uint)Math.Max(1, 2000 - timer.ElapsedMilliseconds);
                        if (setsockopt(handle, 0xffff, 0x1006, ref timeout, 4) != 0) { result.ReceiveError = WSAGetLastError(); break; }
                        int count = recv(handle, buffer, (int)Math.Min(buffer.Length, 65536 - bytes.Length), 0);
                        if (count < 0) { result.ReceiveError = WSAGetLastError(); break; }
                        if (count == 0) break;
                        bytes.Write(buffer, 0, count);
                        if (bytes.Length >= 4) {
                            var header = bytes.GetBuffer();
                            uint length = ((uint)header[0] << 24) | ((uint)header[1] << 16) | ((uint)header[2] << 8) | header[3];
                            if (length == 0 || length > 65532) break;
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
            }
        }
    }
}
'@
}

function Receive-RunnerPeerHello([Guid]$VmId, [string]$Nonce, [string]$EvidencePath) {
    Initialize-RunnerPeerSocket
    $attempts = [Collections.Generic.List[object]]::new()
    $clock = [Diagnostics.Stopwatch]::StartNew()
    try {
        do {
            $started = [DateTime]::UtcNow.ToString('o')
            $result = [Motor.QualificationSocket]::Attempt($VmId)
            $attempts.Add(@{started_utc=$started; completed_utc=[DateTime]::UtcNow.ToString('o'); connected=$result.Connected; error=$result.Error; receive_error=$result.ReceiveError; hello_base64=[Convert]::ToBase64String($result.Hello)})
            if ($result.Connected) {
                # Do not retry after an accepted socket: the guest has only one
                # host session. A malformed/missing Hello is an unavailable victim.
                if ($result.Error -ne 0 -or $result.ReceiveError -ne 0) { throw 'Incomplete Hello' }
                return Convert-RunnerPeerHello $result.Hello $Nonce
            }
            Start-Sleep -Milliseconds 100
        } while ($clock.Elapsed.TotalSeconds -lt 15)
        throw 'Host could not connect after completed sibling probe'
    } catch { throw "Victim unavailable; isolation not proven: $_" } finally {
        $json = ConvertTo-Json -InputObject @($attempts.ToArray()) -Depth 5
        [IO.File]::WriteAllText($EvidencePath, $json, [Text.UTF8Encoding]::new($false))
    }
}

function Stop-RunnerPeerMonitor($Monitor, [string]$Evidence, [string]$PrimaryError) {
    $prefix = Join-Path $Evidence "peer-monitor-$($Monitor.process.Id)"
    $status = [ordered]@{job=$Monitor.job; pid=$Monitor.process.Id; forced=$false; terminated=$false; exit_code=$null; reason='Waiting for Run monitor exit'; failure=$null}
    try {
        if (-not $Monitor.process.WaitForExit(5000)) {
            $status.forced = $true
            $status.reason = if ($PrimaryError) { "Qualification teardown after primary failure: $PrimaryError" } else { 'Run monitor did not exit after successful peer proof; forcing termination' }
            # A killed child cannot flush an explanation. Record the parent's
            # intent before Kill, then retain it beside the captured child output.
            Save-RunnerPeerJson ($prefix + '.status.json') $status
            $Monitor.process.Kill()
            if (-not $Monitor.process.WaitForExit(5000)) { throw 'Peer monitor did not terminate; preserving VM disks rather than inspecting a live guest' }
        }
        $status.terminated = $true
        $result = Complete-SpikeNativeProcess $Monitor ($prefix + '.stdout') ($prefix + '.stderr')
        $status.exit_code = $result.exitCode
        if ($status.forced) {
            if (-not $PrimaryError) { $status.failure = $status.reason }
        } elseif ($result.exitCode -ne 0) {
            $status.reason = "Run monitor exited nonzero: exit=$($result.exitCode); stderr=$($result.stderr); stdout=$($result.stdout)"
            $status.failure = $status.reason
        } else { $status.reason = 'Run monitor exited normally after VM off' }
    } catch {
        $status.reason = "Run monitor collection/termination failed: $_"
        $status.failure = $status.reason
    } finally {
        [Console]::Error.WriteLine("Peer monitor job=$($status.job) pid=$($status.pid): $($status.reason)")
        try {
            [IO.File]::AppendAllText(($prefix + '.stderr'), "`r`nParent monitor disposition: $($status.reason)`r`n", [Text.UTF8Encoding]::new($false))
            Save-RunnerPeerJson ($prefix + '.status.json') $status
        } catch {
            [Console]::Error.WriteLine("Could not write peer monitor disposition: $_")
            $status.failure = "Could not write peer monitor disposition: $_"
        }
    }
    return [pscustomobject]$status
}

function Assert-RunnerPeerCleanup($PrimaryFailure, [string[]]$Failures) {
    # Cleanup can report additional failures but must not replace the operation
    # that caused teardown, especially an intentional monitor kill after timeout.
    if ($null -ne $PrimaryFailure) { throw $PrimaryFailure }
    if ($null -ne $Failures -and $Failures.Count) { throw ('Peer qualification monitor/cleanup failure: ' + ($Failures -join '; ')) }
}
