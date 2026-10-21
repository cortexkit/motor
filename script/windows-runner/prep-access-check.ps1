# Read-only access screening, with only newly created fixture objects. Production
# and fixtures call New-RunnerPrepAcl; this lane never invokes GrantPrep/Set-Acl,
# and only rewrites a fixture's own ACL when a plain delete of it fails.
. "$PSScriptRoot\prep-acl.ps1"
function Save-RunnerAccessFixture([string]$Path, $State) {
    [IO.File]::WriteAllText($Path, (ConvertTo-Json -InputObject $State -Depth 10), [Text.UTF8Encoding]::new($false))
}
function Remove-RunnerAccessFixture([string]$Path) {
    if (-not (Test-Path -LiteralPath $Path)) { return 'absent' }
    $first = $null
    try { Remove-Item -LiteralPath $Path -Recurse -Force -ErrorAction Stop; return 'removed' } catch { $first = [string]$_ }
    # The descriptor under test may deny the caller list or delete rights on
    # the fixture. Only then reset the fixture's own ACLs and delete again.
    Reset-RunnerAccessFixtureAcl $Path 0
    try { Remove-Item -LiteralPath $Path -Recurse -Force -ErrorAction Stop }
    catch { throw "Fixture cleanup failed at $Path even after the ACL reset: $_ (first attempt: $first)" }
    if (Test-Path -LiteralPath $Path) { throw "Fixture remained after cleanup: $Path" }
    return 'removed-after-acl-reset'
}
function Reset-RunnerAccessFixtureAcl([string]$Path, [int]$Depth) {
    # Top-down, so each directory becomes listable before its children are
    # visited: make Administrators the owner (an owner can always rewrite a
    # DACL) and grant SYSTEM and Administrators inheritable full control.
    # Reparse points are not followed, so nothing outside the fixture changes.
    & takeown.exe /F $Path /A | Out-Null
    & icacls.exe $Path /grant '*S-1-5-18:(OI)(CI)F' '*S-1-5-32-544:(OI)(CI)F' /C /Q | Out-Null
    if ($Depth -ge 32 -or -not (Test-Path -LiteralPath $Path -PathType Container)) { return }
    foreach ($child in @(Get-ChildItem -LiteralPath $Path -Force -ErrorAction SilentlyContinue)) {
        if (-not ($child.Attributes -band [IO.FileAttributes]::ReparsePoint)) { Reset-RunnerAccessFixtureAcl $child.FullName ($Depth + 1) }
    }
}
function Remove-RunnerAccessLeftovers([string]$Root, [string]$EvidenceRoot) {
    # Earlier diagnostic runs that could not clean up recorded their fixture in
    # access-fixture-state.json. Remove only GUID-named staging children named
    # there, plus descriptor probes, which nothing in production creates.
    $staging = "$Root\staging"
    $targets = @(Get-ChildItem -LiteralPath $staging -Directory -Force -Filter '.motor-acl-probe-*' -ErrorAction Stop | ForEach-Object { $_.FullName })
    foreach ($old in @(Get-ChildItem -LiteralPath $EvidenceRoot -Directory -ErrorAction Stop)) {
        $file = Join-Path $old.FullName 'access-fixture-state.json'
        if (-not (Test-Path -LiteralPath $file)) { continue }
        $prior = Get-Content -LiteralPath $file -Raw | ConvertFrom-Json
        $guid = [Guid]::Empty
        if ($prior.fixture -is [string] -and (Split-Path -Parent $prior.fixture) -eq $staging -and [Guid]::TryParse((Split-Path -Leaf $prior.fixture), [ref]$guid) -and (Test-Path -LiteralPath $prior.fixture)) { $targets += $prior.fixture }
    }
    foreach ($target in @($targets | Sort-Object -Unique)) { [ordered]@{path=$target; method=(Remove-RunnerAccessFixture $target)} }
}
function Invoke-RunnerPrepAccessMatrix([string]$Root, [string]$Executable, [string]$Evidence) {
    $identity = [Security.Principal.WindowsIdentity]::GetCurrent().User
    if ($identity.Value -ne 'S-1-5-18') { throw 'Read-only prep access matrix requires SYSTEM' }
    $sid = (Get-LocalUser motor-prep -ErrorAction Stop).SID.Value
    $fixture = Join-Path "$Root\staging" ([Guid]::NewGuid().ToString('D'))
    $statePath = Join-Path $Evidence 'access-fixture-state.json'
    $state = [ordered]@{fixture=$fixture; created=$false; removed=$false; existing_acl_changes=$false; objects=@(); error=$null; cleanup_error=$null; cleanup_method=$null; leftovers=@();
        descriptor_builder='New-RunnerPrepAcl (also used by production GrantPrep)'; builder_sha256=(Get-FileHash -LiteralPath "$PSScriptRoot\prep-acl.ps1" -Algorithm SHA256).Hash}
    $failure = $null
    try {
        $state.leftovers = @(Remove-RunnerAccessLeftovers $Root (Split-Path -Parent $Evidence))
        # The owner, group and inherited rules come from an ordinarily created
        # sibling in the real staging directory, as for a production job directory.
        $requested = New-RunnerPrepDirectory $fixture $sid Stage
        $state.created = $true
        $state.objects += @{path=$fixture; role='Stage'; requested_sddl=$requested; actual_sddl=(Get-Acl -LiteralPath $fixture).GetSecurityDescriptorSddlForm([Security.AccessControl.AccessControlSections]::All)}
        Save-RunnerAccessFixture $statePath $state
        $manifests = Join-Path $fixture 'fetch-manifests'
        $requested = New-RunnerPrepDirectory $manifests $sid Read
        $state.objects += @{path=$manifests; role='Read'; requested_sddl=$requested; actual_sddl=(Get-Acl -LiteralPath $manifests).GetSecurityDescriptorSddlForm([Security.AccessControl.AccessControlSections]::All)}
        $manifest = Join-Path $manifests 'Cargo.toml'
        [IO.File]::WriteAllText($manifest, '[workspace]', [Text.UTF8Encoding]::new($false))
        $state.objects += @{path=$manifest; role='Inherited Read'; actual_sddl=(Get-Acl -LiteralPath $manifest).GetSecurityDescriptorSddlForm([Security.AccessControl.AccessControlSections]::All)}
        Save-RunnerAccessFixture $statePath $state
        $running = Start-SpikeNativeProcess $Executable @('prep-access-check',$Root,$fixture,(Join-Path $Evidence 'ksecdd-access-matrix.json'))
        $finished = $running.process.WaitForExit(180000)
        if (-not $finished) { $running.process.Kill(); if (-not $running.process.WaitForExit(5000)) { throw 'Access matrix child did not terminate after timeout' } }
        $result = Complete-SpikeNativeProcess $running (Join-Path $Evidence 'access-check.stdout') (Join-Path $Evidence 'access-check.stderr')
        Write-Host $result.stdout
        if ($result.stderr) { [Console]::Error.WriteLine($result.stderr) }
        if (-not $finished) { throw 'Read-only descriptor access matrix timed out after 180 seconds' }
        if ($result.exitCode -ne 0) { throw "Read-only descriptor collection failed exit=$($result.exitCode); retain the report and stderr" }
    } catch {
        $failure = $_; $state.error = [string]$_; throw
    } finally {
        # The fixture path is a fresh GUID, so anything at it was made by this run.
        if ($state.created -or (Test-Path -LiteralPath $fixture)) {
            try {
                $state.cleanup_method = Remove-RunnerAccessFixture $fixture
                $state.removed = -not (Test-Path -LiteralPath $fixture)
                if (-not $state.removed) { throw 'Temporary access fixture remained after cleanup' }
            } catch { $state.cleanup_error = [string]$_; [Console]::Error.WriteLine("FAIL access-fixture-cleanup: $_") }
        }
        try { Save-RunnerAccessFixture $statePath $state } catch { [Console]::Error.WriteLine("Access fixture state write failed: $_"); if ($null -eq $failure) { throw } }
        if ($state.cleanup_error -and $null -eq $failure) { throw $state.cleanup_error }
    }
}
