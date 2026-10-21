# Read-only access screening, with only newly created fixture objects. Production
# and fixtures call New-RunnerPrepAcl; this lane never invokes GrantPrep/Set-Acl.
. "$PSScriptRoot\prep-acl.ps1"
function Save-RunnerAccessFixture([string]$Path, $State) {
    [IO.File]::WriteAllText($Path, (ConvertTo-Json -InputObject $State -Depth 10), [Text.UTF8Encoding]::new($false))
}
function Invoke-RunnerPrepAccessMatrix([string]$Root, [string]$Executable, [string]$Evidence) {
    $identity = [Security.Principal.WindowsIdentity]::GetCurrent().User
    if ($identity.Value -ne 'S-1-5-18') { throw 'Read-only prep access matrix requires SYSTEM' }
    $sid = (Get-LocalUser motor-prep -ErrorAction Stop).SID.Value
    $fixture = Join-Path "$Root\staging" ([Guid]::NewGuid().ToString('D'))
    $statePath = Join-Path $Evidence 'access-fixture-state.json'
    $state = [ordered]@{fixture=$fixture; created=$false; removed=$false; existing_acl_changes=$false; objects=@(); error=$null; cleanup_error=$null;
        descriptor_builder='New-RunnerPrepAcl (also used by production GrantPrep)'; builder_sha256=(Get-FileHash -LiteralPath "$PSScriptRoot\prep-acl.ps1" -Algorithm SHA256).Hash}
    $failure = $null
    try {
        $base = Get-Acl -LiteralPath "$Root\staging" -ErrorAction Stop
        # A production job directory is created by SYSTEM before its prep grant.
        # Set creator metadata only in this not-yet-applied descriptor, not on an
        # existing object. The DACL comes from the real staging parent.
        $base.SetOwner($identity); $base.SetGroup($identity)
        $requested = New-RunnerPrepDirectory $fixture $base $sid Stage
        $state.created = $true
        $state.objects += @{path=$fixture; role='Stage'; requested_sddl=$requested; actual_sddl=(Get-Acl -LiteralPath $fixture).GetSecurityDescriptorSddlForm([Security.AccessControl.AccessControlSections]::All)}
        Save-RunnerAccessFixture $statePath $state
        $manifests = Join-Path $fixture 'fetch-manifests'
        $requested = New-RunnerPrepDirectory $manifests (Get-Acl -LiteralPath $fixture) $sid Read
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
        if ($state.created) {
            try {
                Remove-Item -LiteralPath $fixture -Recurse -Force -ErrorAction Stop
                $state.removed = -not (Test-Path -LiteralPath $fixture)
                if (-not $state.removed) { throw 'Temporary access fixture remained after cleanup' }
            } catch { $state.cleanup_error = [string]$_; [Console]::Error.WriteLine("Access fixture cleanup failed: $_") }
        }
        try { Save-RunnerAccessFixture $statePath $state } catch { [Console]::Error.WriteLine("Access fixture state write failed: $_"); if ($null -eq $failure) { throw } }
        if ($state.cleanup_error -and $null -eq $failure) { throw $state.cleanup_error }
    }
}
