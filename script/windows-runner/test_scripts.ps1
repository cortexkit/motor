# Portable PowerShell parser and policy-transform fixtures. Never boots a VM.
$ErrorActionPreference = 'Stop'
Set-StrictMode -Version Latest
$files = @(Get-ChildItem $PSScriptRoot -Filter '*.ps1') + @(
    Get-Item "$PSScriptRoot\..\hyperv-spike\common.ps1"
    Get-Item "$PSScriptRoot\..\hyperv-spike\provision.ps1"
    Get-Item "$PSScriptRoot\..\windows-vm\bootstrap.ps1"
    Get-Item "$PSScriptRoot\..\windows-vm\setup-job-baseline.ps1"
)
$count = 0
foreach ($file in $files) {
    $tokens = $null; $errors = $null
    $null = [Management.Automation.Language.Parser]::ParseFile($file.FullName, [ref]$tokens, [ref]$errors)
    if ($errors.Count) { throw "Parse failure $($file.Name): $($errors | Out-String)" }
    Write-Host "PASS powershell-parse-$($file.Name)"
    $count++
}
. "$PSScriptRoot\accounts.ps1"
$root = Join-Path ([IO.Path]::GetTempPath()) ('motor-runner-script-tests-' + [Guid]::NewGuid())
New-Item $root -ItemType Directory | Out-Null
$script:effective = @('[Unicode]','Unicode=yes','[Version]','signature="$CHICAGO$"','Revision=1','[Privilege Rights]','SeBatchLogonRight = *S-1-5-32-544','SeDenyInteractiveLogonRight = *S-1-5-32-546')
# Like the real secedit, the export names a resolvable local account instead of
# listing its SID, so the policy code must resolve names before comparing.
$script:RunnerResolveTrusteeName = { param($name) if ($name -eq 'motor-prep') { 'S-1-5-21-111-222-333-1001' } else { throw "unknown account $name" } }
function global:secedit.exe {
    $cfg = $args[[array]::IndexOf($args,'/cfg') + 1]
    if ($args -contains '/export') { $script:effective | ForEach-Object { $_ -replace '\*S-1-5-21-111-222-333-1001','motor-prep' } | Set-Content -LiteralPath $cfg -Encoding Unicode }
    elseif ($args -contains '/configure') { $script:effective = @(Get-Content -LiteralPath $cfg) }
    else { throw 'Unexpected secedit operation in fixture' }
    $global:LASTEXITCODE = 0
}
if ((Get-Command secedit.exe).CommandType -ne 'Function') { throw 'Policy fixture mock is not installed; refusing to run a real secedit command' }
try {
    $sid = 'S-1-5-21-111-222-333-1001'
    Set-RunnerBatchRights $root $sid
    foreach ($right in @('SeBatchLogonRight','SeDenyInteractiveLogonRight','SeDenyRemoteInteractiveLogonRight','SeDenyNetworkLogonRight','SeDenyServiceLogonRight')) {
        if (@($script:effective | Where-Object { $_ -match "^$right = .*\*$sid" }).Count -ne 1) { throw "Missing exact SID assignment $right" }
    }
    if (($script:effective -join "`n") -notmatch 'SeBatchLogonRight = \*S-1-5-32-544,\*S-1-5-21-111-222-333-1001') { throw 'Existing administrator trustee was lost' }
    Set-RunnerBatchRights $root $sid -Remove
    if (($script:effective -join "`n") -match ([regex]::Escape($sid) + '|motor-prep')) { throw 'Uninstall retained owned SID rights' }
    if (($script:effective -join "`n") -notmatch 'SeBatchLogonRight = \*S-1-5-32-544') { throw 'Uninstall lost unrelated administrator rights' }
    Write-Host 'PASS rights-add-remove-preserves-unrelated-trustees'
    $count++
    $rejected = $false
    try { Set-RunnerBatchRights $root 'S-1-5-32-545' } catch { $rejected = $true }
    if (-not $rejected) { throw 'Policy accepted a broad Users group SID' }
    Write-Host 'PASS rights-rejects-broad-group-sid'
    $count++
} finally {
    Remove-Item Function:\secedit.exe
    Remove-Item $root -Recurse -Force
}
# Parse the actual Prepare/Run guard assignment, not a test-only approximation.
$tokens = $null; $errors = $null
$control = [Management.Automation.Language.Parser]::ParseFile("$PSScriptRoot\hyperv.ps1", [ref]$tokens, [ref]$errors)
$guards = @($control.FindAll({ param($node) $node -is [Management.Automation.Language.AssignmentStatementAst] -and $node.Left.Extent.Text -eq '$parentGuard' -and $node.Right.Extent.Text -match '^Open-RunnerVerifiedParent ' }, $true))
if ($guards.Count -ne 2) { throw 'FAIL parent-identity-production-guards: Prepare and Run must both acquire the verified guard' }
Write-Host 'PASS parent-identity-production-guards'
$count++

& {
    . "$PSScriptRoot\guest-evidence.ps1"
    $path = Join-Path ([IO.Path]::GetTempPath()) ('motor-identity-json-' + [Guid]::NewGuid().ToString('N') + '.json')
    try {
        Save-Json $path @{matched=$true}
        $bytes = [IO.File]::ReadAllBytes($path)
        if ($bytes.Length -lt 2 -or ($bytes.Length -ge 3 -and $bytes[0] -eq 0xef -and $bytes[1] -eq 0xbb -and $bytes[2] -eq 0xbf)) { throw 'FAIL qualification-identity-json-is-bom-free' }
        $value = [Text.Encoding]::UTF8.GetString($bytes) | ConvertFrom-Json
        if (-not $value.matched) { throw 'FAIL qualification-identity-json-is-bom-free' }
        Write-Host 'PASS qualification-identity-json-is-bom-free'
        $script:count++
    } finally { if (Test-Path -LiteralPath $path) { Remove-Item -LiteralPath $path -Force } }
}

& {
    . "$PSScriptRoot\qualify-peer.ps1"
    $nonce = '0123456789abcdef0123456789abcdef'
    $victimVm = [Guid]'01234567-89ab-cdef-0123-456789abcdef'
    $attackerVm = [Guid]'fedcba98-7654-3210-fedc-ba9876543210'
    $proof = @([pscustomobject]@{target=$victimVm.ToString('D'); transport_connected=$false})
    function New-PeerFixtureTimeline {
        return @{victim_start_ms=0; boot_observed_ms=1000; attacker_start_ms=21000; proof_observed_ms=60000; host_connect_ms=60001; hello_ms=60010; settled_ms=120010;
            victim_start_utc='2026-01-01T00:00:00Z'; boot_utc='2026-01-01T00:00:00.500Z'; boot_observed_utc='2026-01-01T00:00:01Z'; attacker_start_utc='2026-01-01T00:00:21Z'; proof_observed_utc='2026-01-01T00:01:00Z'; host_connect_utc='2026-01-01T00:01:00.001Z'; hello_utc='2026-01-01T00:01:00.010Z';
            hello_validated=$true; hello_nonce=$nonce; victim_vm_id=$victimVm.ToString('D'); attacker_vm_id=$attackerVm.ToString('D'); boot_event_id=18601; boot_record_id=123; boot_vm_id=$victimVm.ToString('D')}
    }
    $log = "boot nonce=$nonce`nhvsock listening vm_id=00000000000000000000000000000000`nhvsock refused peer vm_id=e0e16197dd564a1091955ee7a155a838`nhvsock accepted peer vm_id=a42e7cdad03f480c9cc2a4de20abb878 listen=All`n"
    function Assert-PeerFixtureRejected([string]$Name, [scriptblock]$Action, [string]$Expected) {
        $denied = $false
        try { $null = & $Action } catch {
            if ([string]$_ -notlike "*$Expected*") { throw }
            $denied = $true
        }
        if (-not $denied) { throw "FAIL $Name" }
        Write-Host "PASS $Name"
        $script:count++
    }
    $timeline = New-PeerFixtureTimeline
    $result = Assert-RunnerPeerEvidence $nonce $victimVm $attackerVm $proof $timeline $log
    if ($result -ne 'cross-vm-attempt-did-not-consume-single-listener') { throw 'FAIL peer-false-with-final-hello-and-single-parent-accept' }
    Write-Host 'PASS peer-false-with-final-hello-and-single-parent-accept'
    $script:count++
    Assert-PeerFixtureRejected 'peer-host-hello-failure-is-unproven' {
        $changed = New-PeerFixtureTimeline; $changed.hello_validated = $false
        Assert-RunnerPeerEvidence $nonce $victimVm $attackerVm $proof $changed $log
    } 'Victim unavailable; isolation not proven'
    Assert-PeerFixtureRejected 'peer-attacker-before-boot-margin-is-unproven' {
        $changed = New-PeerFixtureTimeline; $changed.attacker_start_ms = 20999
        Assert-RunnerPeerEvidence $nonce $victimVm $attackerVm $proof $changed $log
    } 'Attacker started before victim boot event plus 20-second margin'
    $connected = @([pscustomobject]@{target=$victimVm.ToString('D'); transport_connected=$true})
    Assert-PeerFixtureRejected 'peer-connected-attacker-needs-native-refusal' {
        Assert-RunnerPeerEvidence $nonce $victimVm $attackerVm $connected $timeline $log
    } 'Connected attacker has no native refusal'
    $refusedLog = $log.Replace('hvsock accepted peer', "hvsock refused peer vm_id=$($attackerVm.ToString('N'))`nhvsock accepted peer")
    if ((Assert-RunnerPeerEvidence $nonce $victimVm $attackerVm $connected $timeline $refusedLog) -ne 'native-foreign-vm-refused') { throw 'FAIL peer-connected-attacker-with-native-refusal' }
    Write-Host 'PASS peer-connected-attacker-with-native-refusal'
    $script:count++
    Assert-PeerFixtureRejected 'peer-requires-native-loopback-refusal' {
        Assert-RunnerPeerEvidence $nonce $victimVm $attackerVm $proof $timeline ($log -replace 'hvsock refused peer vm_id=e0e16197dd564a1091955ee7a155a838','unrelated message')
    } 'Parent accept was not preceded by listening and native loopback refusal'
    Assert-PeerFixtureRejected 'peer-rejects-non-parent-accept' {
        Assert-RunnerPeerEvidence $nonce $victimVm $attackerVm $proof $timeline ($log.Replace('a42e7cdad03f480c9cc2a4de20abb878', $attackerVm.ToString('N')))
    } 'Victim accepted a non-parent peer'
    Assert-PeerFixtureRejected 'peer-rejects-stale-or-restarted-listener' {
        Assert-RunnerPeerEvidence $nonce $victimVm $attackerVm $proof $timeline ("boot nonce=$nonce`n" + $log)
    } 'Victim must show one current-nonce listener'
    Assert-PeerFixtureRejected 'peer-requires-full-settle-window' {
        $changed = New-PeerFixtureTimeline; $changed.settled_ms--
        Assert-RunnerPeerEvidence $nonce $victimVm $attackerVm $proof $changed $log
    } 'full 60-second settle window'
    Assert-PeerFixtureRejected 'peer-host-hello-must-follow-completed-attempt' {
        $changed = New-PeerFixtureTimeline; $changed.host_connect_ms = 59999
        Assert-RunnerPeerEvidence $nonce $victimVm $attackerVm $proof $changed $log
    } 'Invalid host peer attempt/Hello ordering'

    # The fixed header independently pins the big-endian Hello wire format.
    $payload = [Text.Encoding]::UTF8.GetBytes('{"kind":"hello","version":4,"nonce":"0123456789abcdef0123456789abcdef"}')
    if ($payload.Length -ne 71) { throw 'Hello wire fixture length changed' }
    $frame = [byte[]]([byte[]]@(0,0,0,71) + $payload)
    if ((Convert-RunnerPeerHello $frame $nonce).nonce -cne $nonce) { throw 'FAIL peer-hello-validates-current-wire-version-and-nonce' }
    Write-Host 'PASS peer-hello-validates-current-wire-version-and-nonce'
    $script:count++
    $badFrames = @($frame.Clone(), $frame.Clone(), $frame.Clone())
    $badFrames[0][3] = 70
    $badFrames[1][30] = [byte][char]'5'
    $badFrames[2][42] = [byte][char]'f'
    foreach ($bad in $badFrames) {
        $denied = $false
        try { $null = Convert-RunnerPeerHello $bad $nonce } catch { $denied = $true }
        if (-not $denied) { throw 'FAIL peer-hello-rejects-bad-length-version-or-nonce' }
    }
    Write-Host 'PASS peer-hello-rejects-bad-length-version-or-nonce'
    $script:count++
    Initialize-RunnerPeerSocket
    Write-Host 'PASS peer-native-socket-helper-compiles-without-dialing'
    $script:count++
}

$switch = $control.Find({ param($node) $node -is [Management.Automation.Language.SwitchStatementAst] }, $true)
$prepare = @($switch.Clauses | Where-Object { $_.Item1 -is [Management.Automation.Language.StringConstantExpressionAst] -and $_.Item1.Value -eq 'Prepare' })
if ($prepare.Count -ne 1) { throw 'FAIL boot-nonce-production-contract: Prepare clause missing' }
$prepareBody = $prepare[0].Item2
$nonceRules = @($prepareBody.FindAll({ param($node) $node -is [Management.Automation.Language.IfStatementAst] -and $node.Extent.Text.StartsWith('if ($Nonce -notmatch') }, $false))
$nonceWrites = @($prepareBody.FindAll({ param($node) $node -is [Management.Automation.Language.InvokeMemberExpressionAst] -and $node.Expression.Extent.Text -eq '[IO.File]' -and $node.Member.Extent.Text -eq 'WriteAllText' -and $node.Arguments[0].Extent.Text -eq '"$directory\nonce\nonce.txt"' }, $false))
$peerFormatter = $control.Find({ param($node) $node -is [Management.Automation.Language.FunctionDefinitionAst] -and $node.Name -eq 'Convert-RunnerPeerTargets' }, $true)
$peerWrites = @($prepareBody.FindAll({ param($node) $node -is [Management.Automation.Language.InvokeMemberExpressionAst] -and $node.Expression.Extent.Text -eq '[IO.File]' -and $node.Member.Extent.Text -eq 'WriteAllText' -and $node.Arguments[0].Extent.Text -eq '"$directory\nonce\peer-targets.json"' }, $false))
if ($nonceRules.Count -ne 1 -or $nonceWrites.Count -ne 1 -or -not $peerFormatter -or $peerWrites.Count -ne 1 -or $nonceRules[0].Clauses[0].Item2.Statements.Count -ne 1 -or $nonceRules[0].Clauses[0].Item2.Statements[0] -isnot [Management.Automation.Language.ThrowStatementAst]) { throw 'FAIL boot-nonce-production-contract: unsafe or missing validation/write fixture' }
& {
    param($rule, $writer, $formatter, $targetWriter)
    $directory = Join-Path ([IO.Path]::GetTempPath()) ('motor-nonce-tests-' + [Guid]::NewGuid().ToString('N'))
    try {
        New-Item -ItemType Directory -Path "$directory\nonce" -Force | Out-Null
        $Nonce = '0123456789abcdef0123456789abcdef'
        . ([scriptblock]::Create($rule))
        . ([scriptblock]::Create($writer))
        $bytes = [IO.File]::ReadAllBytes("$directory\nonce\nonce.txt")
        if ($bytes.Length -ne 32 -or [Text.Encoding]::ASCII.GetString($bytes) -cne $Nonce) { throw 'FAIL boot-nonce-production-contract: media bytes differ from the compact nonce' }
        . ([scriptblock]::Create($formatter))
        foreach ($case in @(
            @{Json='["01234567-89ab-cdef-0123-456789abcdef"]'; Expected='["01234567-89ab-cdef-0123-456789abcdef"]'},
            @{Json='[]'; Expected='[]'},
            @{Json='["01234567-89ab-cdef-0123-456789abcdef","ABCDEF01-2345-4789-ABCD-EF0123456789"]'; Expected='["01234567-89ab-cdef-0123-456789abcdef","abcdef01-2345-4789-abcd-ef0123456789"]'}
        )) {
            $PeerTargetsJson = $case.Json
            . ([scriptblock]::Create($targetWriter))
            $peerBytes = [IO.File]::ReadAllBytes("$directory\nonce\peer-targets.json")
            $actual = [Text.Encoding]::UTF8.GetString($peerBytes)
            if ($actual -cne $case.Expected) { throw "FAIL boot-nonce-production-contract: peer metadata differs; actual=$actual hex=$([BitConverter]::ToString($peerBytes))" }
        }
        foreach ($badPeer in @('"01234567-89ab-cdef-0123-456789abcdef"','[["01234567-89ab-cdef-0123-456789abcdef"]]','[1]','["not-a-guid"]','{"value":["01234567-89ab-cdef-0123-456789abcdef"],"Count":1}')) {
            $PeerTargetsJson = $badPeer
            $rejected = $false
            try { . ([scriptblock]::Create($targetWriter)) } catch { $rejected = $true }
            if (-not $rejected) { throw 'FAIL boot-nonce-production-contract: incompatible peer schema accepted' }
        }
        foreach ($bad in @([Guid]::NewGuid().ToString(), '', ($Nonce + "`n"), ([string][char]0xfeff + $Nonce), ('z' * 32))) {
            $Nonce = $bad
            $rejected = $false
            try { . ([scriptblock]::Create($rule)) } catch { $rejected = $true }
            if (-not $rejected) { throw 'FAIL boot-nonce-production-contract: incompatible nonce accepted' }
        }
        Write-Host 'PASS boot-nonce-production-contract'
        $script:count++
    } finally { if (Test-Path -LiteralPath $directory) { Remove-Item -LiteralPath $directory -Recurse -Force } }
} $nonceRules[0].Extent.Text $nonceWrites[0].Extent.Text $peerFormatter.Extent.Text $peerWrites[0].Extent.Text

$remover = $control.Find({ param($node) $node -is [Management.Automation.Language.FunctionDefinitionAst] -and $node.Name -eq 'Remove-Job' }, $true)
if (-not $remover) { throw 'FAIL removal-harvest-order: production function missing' }
& {
    param($source)
    $Root = Join-Path ([IO.Path]::GetTempPath()) ('motor-remove-tests-' + [Guid]::NewGuid().ToString('N'))
    function Job-Path([string]$Id) { Join-Path $script:RunnerRemovalFixture.root $Id }
    function Owned-VM([string]$Id) { $script:RunnerRemovalFixture.vm }
    function Assert-JobDiskBinding([string]$Id) { $script:RunnerRemovalFixture.calls.Add('binding') }
    function Stop-VM($VM, [switch]$TurnOff, [switch]$Force) {
        $script:RunnerRemovalFixture.calls.Add('stop')
        $script:RunnerRemovalFixture.vm.State = 'Off'
    }
    function Harvest-Job([string]$Id, [switch]$Lightweight) {
        if ($script:RunnerRemovalFixture.vm.State -ne 'Off') { throw 'Fixture attempted a live-disk harvest' }
        if (-not (Test-Path -LiteralPath (Job-Path $Id))) { throw 'Fixture directory was deleted before harvest' }
        $script:RunnerRemovalFixture.calls.Add('harvest')
        if ($Lightweight) { $script:RunnerRemovalFixture.calls.Add('lightweight') }
        else { $script:RunnerRemovalFixture.calls.Add('full'); throw 'Fixture collection error must not skip removal' }
    }
    function Remove-VM($VM, [switch]$Force) {
        $script:RunnerRemovalFixture.calls.Add('remove-vm')
        $script:RunnerRemovalFixture.vm = $null
    }
    function Evidence([string]$Id, [string]$Event, $Detail) { $script:RunnerRemovalFixture.calls.Add($Event) }
    foreach ($name in @('Stop-VM','Remove-VM')) {
        if ((Get-Command $name).CommandType -ne 'Function') { throw 'Removal fixture mock is missing; refusing real Hyper-V control' }
    }
    . ([scriptblock]::Create($source))
    try {
        New-Item -ItemType Directory -Path $Root | Out-Null
        $script:RunnerRemovalFixture = [pscustomobject]@{root=$Root; vm=$null; calls=[Collections.Generic.List[string]]::new()}
        foreach ($lightweight in @($false, $true)) {
            $id = [Guid]::NewGuid().ToString()
            $script:RunnerRemovalFixture.vm = [pscustomobject]@{State='Running'}
            $script:RunnerRemovalFixture.calls.Clear()
            New-Item -ItemType Directory -Path (Job-Path $id) | Out-Null
            Remove-Job $id -LightweightHarvest:$lightweight 3>$null
            $calls = $script:RunnerRemovalFixture.calls.ToArray()
            $stopIndex = [array]::IndexOf($calls, 'stop')
            $harvestIndex = [array]::IndexOf($calls, 'harvest')
            $removeIndex = [array]::IndexOf($calls, 'remove-vm')
            if ($stopIndex -lt 0 -or $removeIndex -lt 0 -or (Test-Path -LiteralPath (Job-Path $id))) { throw 'FAIL removal-harvest-order: cleanup did not complete' }
            if ($harvestIndex -le $stopIndex -or $removeIndex -le $harvestIndex) { throw 'FAIL removal-harvest-order: evidence was not attempted before VM destruction' }
            if ($lightweight) {
                if ($calls -notcontains 'lightweight' -or $calls -contains 'full') { throw 'FAIL known-clean-removal-collects-lightweight-logs' }
                Write-Host 'PASS known-clean-removal-collects-lightweight-logs'
            } else {
                if ($calls -notcontains 'full') { throw 'FAIL failed-removal-requires-full-diagnostics' }
                Write-Host 'PASS failed-removal-harvests-before-destruction-even-if-collection-fails'
            }
            $script:count++
        }
    } finally {
        if (Test-Path -LiteralPath $Root) { Remove-Item -LiteralPath $Root -Recurse -Force }
        Remove-Variable -Name RunnerRemovalFixture -Scope Script -ErrorAction SilentlyContinue
    }
} $remover.Extent.Text

if ([Environment]::OSVersion.Platform -eq [PlatformID]::Win32NT) {
    . "$PSScriptRoot\parent-identity.ps1"
    $fixture = Join-Path ([IO.Path]::GetTempPath()) ('motor-parent-tests-' + [Guid]::NewGuid())
    New-Item $fixture -ItemType Directory | Out-Null
    $Parent = Join-Path $fixture 'parent.vhdx'
    $record = Join-Path $fixture 'parent-verified.json'
    $parentRecord = $record
    $parentGuard = $null
    $lifetimeGuard = $null
    $capability = 'S-1-15-3-1024-2268835264-3721307629-241982045-173645152-1490879176-104643441-2915960892-1612460704'
    $readAcl = [Security.AccessControl.FileSecurity]::new()
    $readSddl = "O:BAG:BAD:P(A;;FR;;;SY)(A;;FR;;;BA)(A;;FR;;;S-1-5-83-0)(A;;FR;;;$capability)"
    $readAcl.SetSecurityDescriptorSddlForm($readSddl)
    $writeAcl = [Security.AccessControl.FileSecurity]::new()
    $writeAcl.SetSecurityDescriptorSddlForm('O:BAG:BAD:P(A;;FA;;;SY)(A;;FA;;;BA)')
    function Set-FixtureParent([string]$Contents, [DateTime]$LastWrite) {
        if (Test-Path -LiteralPath $Parent) {
            Set-Acl -LiteralPath $Parent -AclObject $writeAcl
            [IO.File]::SetAttributes($Parent, [IO.FileAttributes]::Normal)
        }
        [IO.File]::WriteAllText($Parent, $Contents, [Text.UTF8Encoding]::new($false))
        [IO.File]::SetLastWriteTimeUtc($Parent, $LastWrite)
        [IO.File]::SetAttributes($Parent, [IO.FileAttributes]::ReadOnly)
        Set-Acl -LiteralPath $Parent -AclObject $readAcl
    }
    function Assert-FixtureRejected([string]$Name) {
        $rejected = $false
        try {
            $unexpected = Open-RunnerVerifiedParent $Parent $ParentSha256 $record
            $unexpected.Dispose()
        } catch {
            if ([string]$_ -notlike '*parent changed since verification; restart the service to re-verify*') { throw }
            $rejected = $true
        }
        if (-not $rejected) { throw "FAIL $Name : changed parent was accepted" }
        Write-Host "PASS $Name"
        $script:count++
    }
    try {
        $lastWrite = [DateTime]::UtcNow.AddDays(-1)
        Set-FixtureParent 'original' $lastWrite
        $ParentSha256 = (Get-FileHash -LiteralPath $Parent -Algorithm SHA256).Hash
        $hashLog = Join-Path $fixture 'parent-hash.jsonl'
        $verification = Confirm-RunnerParentVerification $Parent $ParentSha256 $record $hashLog
        if ($verification.cache_hit) { throw 'FAIL parent-identity-record-written: missing record did not require verification' }
        if (-not (Test-Path -LiteralPath $record)) { throw 'FAIL parent-identity-record-written' }
        Write-Host 'PASS parent-identity-record-written'
        $count++
        $hashEvent = Get-Content -LiteralPath $hashLog -Raw | ConvertFrom-Json
        if ($hashEvent.event -ne 'parent-sha256' -or -not $hashEvent.hash_matches_configuration -or $hashEvent.hash_duration_ms -lt 0) { throw 'FAIL parent-identity-startup-hash-duration-logged' }
        Write-Host 'PASS parent-identity-startup-hash-duration-logged'
        $count++
        $savedWriter = (Get-Item Function:\Write-RunnerParentVerification).ScriptBlock
        Set-Item Function:\Write-RunnerParentVerification -Value { throw 'FAIL parent-identity-startup-cache-hit-without-hashing: unexpected digest' }
        try {
            $verification = Confirm-RunnerParentVerification $Parent $ParentSha256 $record
            if (-not $verification.cache_hit -or $verification.hash_duration_ms -ne 0) { throw 'FAIL parent-identity-startup-cache-hit-without-hashing' }
        } finally { Set-Item Function:\Write-RunnerParentVerification -Value $savedWriter }
        Write-Host 'PASS parent-identity-startup-cache-hit-without-hashing'
        $count++
        # Run the exact assignment from Prepare against the tiny sealed file.
        . ([scriptblock]::Create($guards[0].Extent.Text))
        $before = Get-RunnerParentIdentity $parentGuard
        $reader = [Motor.ParentFile]::Open($Parent)
        $reader.Dispose()
        $parentGuard.Dispose(); $parentGuard = $null
        Write-Host 'PASS parent-identity-unchanged-and-concurrent-reader'
        $count++

        foreach ($sddl in @($readSddl.Replace("(A;;FR;;;$capability)", "(A;;FRFW;;;$capability)"), ($readSddl + '(A;;FR;;;BU)'))) {
            $badAcl = [Security.AccessControl.FileSecurity]::new()
            $badAcl.SetSecurityDescriptorSddlForm($sddl)
            Set-Acl -LiteralPath $Parent -AclObject $badAcl
            $probe = [Motor.ParentFile]::Open($Parent)
            $rejected = $false
            try { Assert-RunnerFileAcl $probe -ParentFile } catch { $rejected = $true } finally {
                $probe.Dispose()
                Set-Acl -LiteralPath $Parent -AclObject $readAcl
            }
            if (-not $rejected) { throw 'FAIL parent-capability-read-only-and-no-other-grant' }
        }
        Write-Host 'PASS parent-capability-read-only-and-no-other-grant'
        $count++

        # Model the runner's lifetime guard, not just a per-job check. Make the
        # ACL writable so the failure proves sharing rather than access denial.
        $lifetimeGuard = [Motor.ParentFile]::Open($Parent)
        Set-Acl -LiteralPath $Parent -AclObject $writeAcl
        [IO.File]::SetAttributes($Parent, [IO.FileAttributes]::Normal)
        $denied = $false
        try {
            $writer = [IO.File]::Open($Parent, [IO.FileMode]::Open, [IO.FileAccess]::ReadWrite, ([IO.FileShare]::ReadWrite -bor [IO.FileShare]::Delete))
            try {
                $bytes = [Text.Encoding]::UTF8.GetBytes('modified')
                $writer.Write($bytes, 0, $bytes.Length)
            } finally { $writer.Dispose() }
        } catch {
            if (($_.Exception.GetBaseException().HResult -band 0xffff) -ne 32) { throw }
            $denied = $true
        } finally {
            [IO.File]::SetAttributes($Parent, [IO.FileAttributes]::ReadOnly)
            Set-Acl -LiteralPath $Parent -AclObject $readAcl
        }
        if (-not $denied) { throw 'FAIL parent-lifetime-guard-blocks-same-size-content-rewrite' }
        Write-Host 'PASS parent-lifetime-guard-blocks-same-size-content-rewrite'
        $count++
        # The parent is fresh under a GUID-named directory on every run. Do not
        # infer a ChangeTime update from the preceding sharing test's ACL writes:
        # they can finish within one filesystem timestamp tick. Repeatedly flip
        # Hidden to its opposite and back, preserving every original attribute,
        # until the held handle observes the change or the bounded window ends.
        $changeBefore = [Motor.ParentFile]::ChangeTime($lifetimeGuard)
        $changeAfter = $changeBefore
        $originalAttributes = [IO.File]::GetAttributes($Parent)
        $flippedAttributes = [IO.FileAttributes]([int]$originalAttributes -bxor [int][IO.FileAttributes]::Hidden)
        $changeClock = [Diagnostics.Stopwatch]::StartNew()
        try {
            Set-Acl -LiteralPath $Parent -AclObject $writeAcl
            do {
                [IO.File]::SetAttributes($Parent, $flippedAttributes)
                [IO.File]::SetAttributes($Parent, $originalAttributes)
                $changeAfter = [Motor.ParentFile]::ChangeTime($lifetimeGuard)
                if ($changeAfter -gt $changeBefore) { break }
                Start-Sleep -Milliseconds 25
            } while ($changeClock.ElapsedMilliseconds -lt 2000)
        } finally {
            [IO.File]::SetAttributes($Parent, $originalAttributes)
            Set-Acl -LiteralPath $Parent -AclObject $readAcl
        }
        $after = Get-RunnerParentIdentity $lifetimeGuard
        foreach ($key in $before.Keys) {
            if ($before[$key] -cne $after[$key]) { throw "FAIL ChangeTime-only fixture unexpectedly changed $key" }
        }
        if ($changeAfter -le $changeBefore) { throw 'FAIL ChangeTime-only fixture did not advance ChangeTime within 2 seconds of attribute flips' }
        $parentGuard = Open-RunnerVerifiedParent $Parent $ParentSha256 $record
        $parentGuard.Dispose(); $parentGuard = $null
        Write-Host 'PASS parent-identity-accepts-changetime-only-update'
        $count++
        $lifetimeGuard.Dispose(); $lifetimeGuard = $null

        # Without a lifetime guard, LastWrite still detects an ordinary rewrite.
        Set-FixtureParent 'modified' $lastWrite.AddMinutes(1)
        Assert-FixtureRejected 'parent-identity-rejects-same-size-rewrite-with-changed-lastwrite'
        $metadata = @{parent=$Parent; sha256=$ParentSha256}
        $denied = $false
        try {
            . ([scriptblock]::Create($guards[1].Extent.Text))
            $parentGuard.Dispose(); $parentGuard = $null
        } catch {
            if ([string]$_ -notlike '*parent changed since verification; restart the service to re-verify*') { throw }
            $denied = $true
        }
        if (-not $denied) { throw 'FAIL parent-identity-run-rejects-change-before-boot' }
        Write-Host 'PASS parent-identity-run-rejects-change-before-boot'
        $count++
        $denied = $false
        try { $null = Confirm-RunnerParentVerification $Parent $ParentSha256 $record } catch {
            if ([string]$_ -notlike '*parent SHA-256 mismatch*') { throw }
            $denied = $true
        }
        if (-not $denied -or (Test-Path -LiteralPath $record)) { throw 'FAIL parent-identity-startup-rejects-changed-content' }
        Write-Host 'PASS parent-identity-startup-rejects-changed-content'
        $count++

        Set-FixtureParent 'original' $lastWrite
        $null = Write-RunnerParentVerification $Parent $ParentSha256 $record
        Set-Acl -LiteralPath $Parent -AclObject $writeAcl
        [IO.File]::SetAttributes($Parent, [IO.FileAttributes]::Normal)
        [IO.File]::SetLastWriteTimeUtc($Parent, $lastWrite.AddHours(1))
        [IO.File]::SetAttributes($Parent, [IO.FileAttributes]::ReadOnly)
        Set-Acl -LiteralPath $Parent -AclObject $readAcl
        Assert-FixtureRejected 'parent-identity-rejects-timestamp-change'
        $verification = Confirm-RunnerParentVerification $Parent $ParentSha256 $record
        if ($verification.cache_hit) { throw 'FAIL parent-identity-startup-refreshes-changed-identity' }
        $parentGuard = Open-RunnerVerifiedParent $Parent $ParentSha256 $record
        $parentGuard.Dispose(); $parentGuard = $null
        Write-Host 'PASS parent-identity-startup-refreshes-changed-identity'
        $count++

        Set-FixtureParent 'original' $lastWrite
        $null = Write-RunnerParentVerification $Parent $ParentSha256 $record
        $metadata = @{parent=$Parent; sha256=$ParentSha256}
        . ([scriptblock]::Create($guards[1].Extent.Text))
        $parentGuard.Dispose(); $parentGuard = $null
        Write-Host 'PASS parent-identity-run-rechecks-before-boot'
        $count++
        Remove-Item -LiteralPath $record -Force
        Assert-FixtureRejected 'parent-identity-rejects-missing-record'
        $null = Write-RunnerParentVerification $Parent $ParentSha256 $record
        Set-Content -LiteralPath $record -Value '{broken json' -Encoding ascii
        Assert-FixtureRejected 'parent-identity-rejects-unreadable-record'
        $verification = Confirm-RunnerParentVerification $Parent $ParentSha256 $record
        if ($verification.cache_hit) { throw 'FAIL parent-identity-startup-refreshes-unreadable-record' }
        Write-Host 'PASS parent-identity-startup-refreshes-unreadable-record'
        $count++
        $failed = $false
        try { Write-RunnerParentVerification $Parent ('0' * 64) $record } catch { $failed = $true }
        if (-not $failed -or (Test-Path -LiteralPath $record)) { throw 'FAIL parent-identity-failed-validation-invalidates-record' }
        Write-Host 'PASS parent-identity-failed-validation-invalidates-record'
        $count++

        # Use a writable file so a denied write proves sharing, not the sealed ACL.
        $sharingFile = Join-Path $fixture 'sharing.bin'
        [IO.File]::WriteAllText($sharingFile, 'sharing')
        $parentGuard = [Motor.ParentFile]::Open($sharingFile)
        foreach ($access in @([IO.FileAccess]::ReadWrite, [IO.FileAccess]::Write)) {
            $denied = $false
            try {
                $writer = [IO.File]::Open($sharingFile, [IO.FileMode]::Open, $access, [IO.FileShare]::ReadWrite)
                $writer.Dispose()
            } catch { $denied = $true }
            if (-not $denied) { throw 'FAIL parent-guard-denies-write-and-delete: write admitted' }
        }
        $denied = $false
        try { [IO.File]::Delete($sharingFile) } catch { $denied = $true }
        if (-not $denied) { throw 'FAIL parent-guard-denies-write-and-delete: delete admitted' }
        $parentGuard.Dispose(); $parentGuard = $null
        $writer = [IO.File]::Open($sharingFile, [IO.FileMode]::Open, [IO.FileAccess]::ReadWrite, [IO.FileShare]::ReadWrite)
        $denied = $false
        try { $unexpected = [Motor.ParentFile]::Open($sharingFile); $unexpected.Dispose() } catch { $denied = $true } finally { $writer.Dispose() }
        if (-not $denied) { throw 'FAIL parent-guard-refuses-existing-writer' }
        [IO.File]::Delete($sharingFile)
        Write-Host 'PASS parent-guard-denies-write-and-delete-and-refuses-existing-writer'
        $count++
    } finally {
        if ($lifetimeGuard) { $lifetimeGuard.Dispose() }
        if ($parentGuard) { $parentGuard.Dispose() }
        if (Test-Path -LiteralPath $Parent) { Set-Acl -LiteralPath $Parent -AclObject $writeAcl }
        Remove-Item -LiteralPath $fixture -Recurse -Force
    }
} else {
    Write-Host 'SKIP native parent-identity fixtures: Windows and an elevated token are required'
}
Write-Host "PASS Windows runner PowerShell tests: $count checks; PowerShell=$($PSVersionTable.PSVersion)"
