# Portable mocked control-flow tests; no Windows Update or registry calls.
# Private fixture drives leave native C: intact, including SYSTEM's temp paths.
$ErrorActionPreference = 'Stop'
$repo = (Resolve-Path "$PSScriptRoot/../..").Path
$bootstrapText = Get-Content (Join-Path $repo 'script/windows-vm/bootstrap.ps1') -Raw
$finalizerText = Get-Content (Join-Path $repo 'script/windows-vm/finalize-job-baseline.ps1') -Raw
$passed = 0
$failed = 0
Write-Host "EVIDENCE servicing test temp=$([IO.Path]::GetTempPath()); native C: is not rebound"
function Assert-Service($Condition, [string]$Message) { if (-not $Condition) { throw $Message } }
function Test-Service([string]$Name, [scriptblock]$Check) {
    try { & $Check; Write-Host "PASS $Name"; $script:passed++ }
    catch { Write-Host "FAIL $Name $_"; $script:failed++ }
}

function Convert-ServiceFixturePath([string]$Path, [string]$PhysicalRoot) {
    $comparison = [StringComparison]::OrdinalIgnoreCase
    if (-not $Path.StartsWith('C:\', $comparison)) { return $Path }
    if ($Path.Equals($PhysicalRoot.TrimEnd('\'), $comparison) -or $Path.StartsWith($PhysicalRoot.TrimEnd('\') + '\', $comparison)) { return $Path }
    return 'MotorGuest:\' + $Path.Substring(3)
}

function Convert-ServiceFixtureSource([string]$Source, [string]$PhysicalRoot) {
    $tokens = $null; $errors = $null
    $ast = [Management.Automation.Language.Parser]::ParseInput($Source, [ref]$tokens, [ref]$errors)
    if ($errors.Count -ne 0) { throw ($errors | Out-String) }
    $paths = $ast.FindAll({ param($node)
        ($node -is [Management.Automation.Language.StringConstantExpressionAst] -or $node -is [Management.Automation.Language.ExpandableStringExpressionAst]) -and $node.Value.StartsWith('C:\', [StringComparison]::OrdinalIgnoreCase)
    }, $true)
    # Rewrite logical guest path literals once. Physical paths returned by file
    # APIs and $PSScriptRoot still use the real C: drive, never a rebound C: root.
    foreach ($node in @($paths | Sort-Object { $_.Extent.StartOffset } -Descending)) {
        if ((Convert-ServiceFixturePath $node.Value $PhysicalRoot) -eq $node.Value) { continue }
        $offset = $node.Extent.StartOffset + $node.Extent.Text.IndexOf('C:\', [StringComparison]::OrdinalIgnoreCase)
        $Source = $Source.Remove($offset, 3).Insert($offset, 'MotorGuest:\')
    }
    return $Source
}

function New-ServiceFixture {
    $root = Join-Path ([IO.Path]::GetTempPath()) ('motor-servicing-' + [Guid]::NewGuid().ToString('N'))
    [void][IO.Directory]::CreateDirectory((Join-Path $root 'c'))
    [void][IO.Directory]::CreateDirectory((Join-Path $root 's'))
    $shell = [PowerShell]::Create()
    $setup = {
        param($Root, $Bootstrap, $Finalizer)
        # Use private names so native absolute paths are never resolved twice.
        Set-Location Function:\
        Remove-PSDrive MotorGuest,S -Force -ErrorAction SilentlyContinue
        New-PSDrive MotorGuest FileSystem (Join-Path $Root 'c') -Scope Global | Out-Null
        New-PSDrive S FileSystem (Join-Path $Root 's') -Scope Global | Out-Null
        if ((Get-PSDrive MotorGuest).Root -ne (Join-Path $Root 'c')) { throw 'Fixture drive isolation failed' }
        Set-Location MotorGuest:\
        $global:f = @{ rotations=0; reboots=0; stops=0; tasks=@{}; started=@(); seed=$true; searchCode=2; downloadCode=2; installCode=2; itemCode=2; pending=$false; offered=$true; calls=@() }
        Set-Content S:\bootstrap.ps1 $Bootstrap
        Set-Content S:\finalize-job-baseline.ps1 $Finalizer
        New-Item S:\job-baseline,S:\hyperv-spike -ItemType File | Out-Null
        New-Item S:\motor-winagent\src -ItemType Directory -Force | Out-Null
        Set-Content S:\motor-winagent\Cargo.lock 'fixture lock'
        Set-Content S:\PowerShell-7.6.7-win-x64.zip 'fixture zip'
        Set-Content S:\setup-guest.ps1 'param([switch]$JobBaseline); if (-not $JobBaseline) { throw "mode lost" }; $global:f.calls += "guest"'
        Set-Content S:\setup-job-baseline.ps1 'param([switch]$HyperVSpike); if (-not $HyperVSpike) { throw "Hyper-V mode lost" }; $global:f.calls += "baseline"'
        function global:Get-Volume { if (-not $global:f.seed) { throw 'Seed reread during resume' }; [pscustomobject]@{DriveLetter='S'} }
        function global:Get-NetAdapter { [pscustomobject]@{InterfaceDescription='Hyper-V'; Status='Up'} }
        function global:Set-LocalUser { $global:f.rotations++ }
        function global:Disable-LocalUser {}
        function global:Set-ItemProperty {}
        function global:Remove-ItemProperty {}
        function global:New-ItemProperty {}
        function global:Start-Transcript {}
        function global:Stop-Transcript {}
        function global:New-ScheduledTaskAction { param($Execute,$Argument) [pscustomobject]@{Execute=$Execute; Arguments=$Argument} }
        function global:New-ScheduledTaskTrigger { [pscustomobject]@{Delay=''} }
        function global:Register-ScheduledTask { param($TaskName,$Action,$Trigger,$User,$RunLevel) $global:f.tasks[$TaskName] = @{action=$Action; trigger=$Trigger; user=$User; level=$RunLevel} }
        function global:Unregister-ScheduledTask { param($TaskName) [void]$global:f.tasks.Remove($TaskName) }
        function global:Start-ScheduledTask { param($TaskName) $global:f.started += $TaskName }
        function global:Restart-Computer { $global:f.reboots++ }
        function global:Stop-Computer { $global:f.stops++ }
        function global:cscript.exe { $global:LASTEXITCODE=0 }
        function global:Get-ItemProperty { [pscustomobject]@{ProductName='Fixture'; CurrentBuildNumber=20348; UBR=4000} }
        function global:Test-Path {
            [CmdletBinding()]
            param($Path,$LiteralPath,$PathType)
            $value = if ($LiteralPath) { $LiteralPath } else { $Path }
            if ([string]$value -like 'HKLM:*') { return $false }
            Microsoft.PowerShell.Management\Test-Path @PSBoundParameters
        }
        function global:Check($Condition, [string]$Message) { if (-not $Condition) { throw $Message } }
        $global:update = [pscustomobject]@{Title='Fixture cumulative update'; EulaAccepted=$false; Identity=[pscustomobject]@{UpdateID='11111111-1111-1111-1111-111111111111'; RevisionNumber=7}; KBArticleIDs=@('KBfixture')}
        $global:update | Add-Member ScriptMethod AcceptEula { $this.EulaAccepted=$true }
        $global:searcher = [pscustomobject]@{}
        $global:searcher | Add-Member ScriptMethod Search { param($Criteria) [pscustomobject]@{ResultCode=$global:f.searchCode; Updates=@(if ($global:f.offered) { $global:update })} }
        $global:downloader = [pscustomobject]@{Updates=$null}
        $global:downloader | Add-Member ScriptMethod Download { [pscustomobject]@{ResultCode=$global:f.downloadCode} }
        $global:installer = [pscustomobject]@{Updates=$null}
        $global:installer | Add-Member ScriptMethod Install {
            $r=[pscustomobject]@{ResultCode=$global:f.installCode; RebootRequired=$false}
            $r | Add-Member ScriptMethod GetUpdateResult { param($Index) [pscustomobject]@{ResultCode=$global:f.itemCode; HResult=$(if ($global:f.itemCode -eq 2) { 0 } else { -1 })} }
            return $r
        }
        $global:session = [pscustomobject]@{}
        $global:session | Add-Member ScriptMethod CreateUpdateSearcher { $global:searcher }
        $global:session | Add-Member ScriptMethod CreateUpdateDownloader { $global:downloader }
        $global:session | Add-Member ScriptMethod CreateUpdateInstaller { $global:installer }
        $global:collection = [pscustomobject]@{Count=0}
        $global:collection | Add-Member ScriptMethod Add { param($Update) $this.Count++; return ($this.Count - 1) }
        function global:New-Object {
            param($ComObject)
            switch ($ComObject) {
                'Microsoft.Update.Session' { return $global:session }
                'Microsoft.Update.UpdateColl' { return $global:collection }
                'Microsoft.Update.SystemInfo' { return [pscustomobject]@{RebootRequired=$global:f.pending} }
                default { Microsoft.PowerShell.Utility\New-Object $ComObject @args }
            }
        }
        & S:\bootstrap.ps1 -JobBaseline
    }
    try {
        $physicalRoot = Join-Path $root 'c'
        [void]$shell.AddScript($setup.ToString()).AddArgument($root).AddArgument((Convert-ServiceFixtureSource $bootstrapText $physicalRoot)).AddArgument((Convert-ServiceFixtureSource $finalizerText $physicalRoot))
        $null = $shell.Invoke()
        $shell.Commands.Clear()
        [void]$shell.AddScript('Check (-not (Test-Path MotorGuest:\build\provision\FAILED.txt)) "Initial bootstrap failed"; Check ($global:f.tasks.ContainsKey("MotorUpdateFirst")) "Initial bootstrap did not schedule OS servicing"')
        $null = $shell.Invoke()
        if ($shell.InvocationStateInfo.State -ne 'Completed') { throw ($shell.Streams.Error | Out-String) }
        return @{root=$root; shell=$shell}
    } catch { $shell.Dispose(); Remove-Item $root -Recurse -Force; throw }
}

function Invoke-ServiceFixture($Fixture, [string]$Code) {
    $shell = $Fixture.shell
    $shell.Commands.Clear()
    $shell.Streams.Error.Clear()
    [void]$shell.AddScript((Convert-ServiceFixtureSource $Code (Join-Path $Fixture.root 'c')))
    $result = @($shell.Invoke())
    if ($shell.InvocationStateInfo.State -ne 'Completed') { throw ($shell.Streams.Error | Out-String) }
    return $result
}
function Close-ServiceFixture($Fixture) { $Fixture.shell.Dispose(); Remove-Item $Fixture.root -Recurse -Force }

Test-Service 'servicing-fixture-redirection-is-idempotent-with-native-c-temp-root' {
    $root = 'C:\WINDOWS\SystemTemp\motor-servicing-fixture\c'
    $native = "$root\build\provision\FAILED.txt"
    $source = @"
Get-Content C:\build\provision\FAILED.txt
Get-Content '$native'
`$log = "C:\build\provision\`$phase.log"
"@
    $once = Convert-ServiceFixtureSource $source $root
    $twice = Convert-ServiceFixtureSource $once $root
    Assert-Service ($once -ceq $twice) 'Fixture source was redirected twice'
    Assert-Service ($once.Contains('Get-Content MotorGuest:\build\provision\FAILED.txt')) 'Logical guest path was not redirected'
    Assert-Service ($once.Contains("Get-Content '$native'")) 'Already physical C: fixture path was redirected'
    Assert-Service ($once.Contains('"MotorGuest:\build\provision\$phase.log"')) 'Interpolated guest path was not redirected'
    Assert-Service ((Convert-ServiceFixturePath $native $root) -ceq $native) 'Physical C: path is not idempotent'
}
Test-Service 'servicing-is-scheduled-before-any-toolchain-and-uses-system-resume' {
    $f = New-ServiceFixture
    try {
        $null = Invoke-ServiceFixture $f @'
Check ($global:f.calls.Count -eq 0) 'Toolchain ran before OS servicing'
Check ($global:f.rotations -eq 2) 'One-time passwords were not rotated'
Check ($global:f.tasks.MotorUpdateFirst.user -eq 'SYSTEM') 'Update task must use SYSTEM'
Check ($global:f.tasks.MotorUpdateFirst.level -eq 'Highest') 'Update task must be elevated'
Check ($global:f.tasks.MotorUpdateFirst.action.Arguments -like '*-BeforeToolchain') 'Wrong updater mode'
Check ($global:f.tasks.MotorUpdateFirst.trigger.Delay -eq 'PT30S') 'Startup network settling delay missing'
Check (-not (Test-Path C:\build\provision\provisioned)) 'Early provisioned marker'
Check (-not (Test-Path C:\build\provision\finalized)) 'Early finalized marker'
Check ($global:f.stops -eq 0) 'Early poweroff'
'@
    } finally { Close-ServiceFixture $f }
}
Test-Service 'servicing-reboot-resumes-without-seed-password-rerun-or-early-shutdown' {
    $f = New-ServiceFixture
    try {
        $null = Invoke-ServiceFixture $f '& C:\build\provision\finalize-job-baseline.ps1 -BeforeToolchain'
        $null = Invoke-ServiceFixture $f @'
Check ($global:f.reboots -eq 1 -and $global:f.stops -eq 0) 'Update pass must reboot, not power off'
Check ($global:collection.Count -eq 1) 'COM collection count must not depend on implicit scalar Count'
Check ($global:f.calls.Count -eq 0) 'Installer ran before reboot settled'
Check (-not (Test-Path C:\build\provision\os-updated)) 'Early servicing marker'
$inventory = Get-Content C:\build\provision\os-update-first-updates-1.json -Raw | ConvertFrom-Json
Check ($inventory[0].updateId -eq '11111111-1111-1111-1111-111111111111') 'Update identity not recorded'
$global:f.offered=$false; $global:f.seed=$false
'@
        $null = Invoke-ServiceFixture $f '& C:\build\provision\finalize-job-baseline.ps1 -BeforeToolchain'
        $null = Invoke-ServiceFixture $f @'
Check (Test-Path C:\build\provision\os-updated) 'Settled OS marker missing'
Check (($global:f.calls -join ',') -eq 'guest,baseline') 'Toolchain continuation not executed once'
Check ($global:f.rotations -eq 2) 'Passwords rotated again on resume'
Check (-not $global:f.tasks.ContainsKey('MotorUpdateFirst')) 'Pre-update task left behind'
Check ($global:f.tasks.ContainsKey('MotorFinalize')) 'Final update task not registered'
Check (Test-Path C:\build\provision\provisioned) 'Provisioned marker missing after tools'
Check (-not (Test-Path C:\build\provision\finalized)) 'Finalization falsely completed before final pass'
Check ($global:f.stops -eq 0) 'Pre-update completion powered off instead of continuing'
'@
        $null = Invoke-ServiceFixture $f '& C:\build\provision\finalize-job-baseline.ps1'
        $null = Invoke-ServiceFixture $f @'
Check (Test-Path C:\build\provision\finalized) 'Final marker missing'
Check ($global:f.stops -eq 1) 'Final pass did not power off'
Check (-not $global:f.tasks.ContainsKey('MotorFinalize')) 'Final task left behind'
'@
    } finally { Close-ServiceFixture $f }
}
foreach ($failure in @('searchCode','downloadCode','installCode','itemCode')) {
    Test-Service "servicing-$failure-failure-never-runs-runtime-installer" {
        $f = New-ServiceFixture
        try {
            $null = Invoke-ServiceFixture $f ('$global:f.' + $failure + '=3; & C:\build\provision\finalize-job-baseline.ps1 -BeforeToolchain')
            $null = Invoke-ServiceFixture $f @'
Check (Test-Path C:\build\provision\FAILED.txt) 'Update failure marker missing'
Check ($global:f.calls.Count -eq 0) 'Runtime installer ran after failed update'
Check (-not (Test-Path C:\build\provision\os-updated)) 'Failed update published OS marker'
Check (-not (Test-Path C:\build\provision\finalized)) 'Failed update published final marker'
Check ($global:f.stops -eq 1) 'Failed update did not power off'
Check (-not $global:f.tasks.ContainsKey('MotorUpdateFirst')) 'Failed updater task not removed'
'@
        } finally { Close-ServiceFixture $f }
    }
}
Test-Service 'servicing-pending-reboot-and-five-pass-bound-refuse-early-continuation' {
    $f = New-ServiceFixture
    try {
        $null = Invoke-ServiceFixture $f '$global:f.offered=$false; $global:f.pending=$true; & C:\build\provision\finalize-job-baseline.ps1 -BeforeToolchain'
        $null = Invoke-ServiceFixture $f @'
Check ($global:f.reboots -eq 1) 'Pending reboot was ignored'
Check ($global:f.calls.Count -eq 0) 'Pending reboot opened installer gate'
Set-Content C:\build\provision\os-update-first.count 4
'@
        $null = Invoke-ServiceFixture $f '& C:\build\provision\finalize-job-baseline.ps1 -BeforeToolchain'
        $null = Invoke-ServiceFixture $f @'
Check ((Get-Content C:\build\provision\FAILED.txt -Raw) -like '*within five*') 'Pass bound not enforced'
Check ($global:f.reboots -eq 1 -and $global:f.stops -eq 1) 'Unbounded reboot instead of failure shutdown'
Check ($global:f.calls.Count -eq 0) 'Unsettled OS reached installer'
'@
    } finally { Close-ServiceFixture $f }
}
Test-Service 'servicing-resume-requires-complete-marker-and-preserves-first-failure' {
    $f = New-ServiceFixture
    try {
        $null = Invoke-ServiceFixture $f '& C:\build\provision\bootstrap.ps1 -JobBaseline -ResumeAfterUpdates'
        $null = Invoke-ServiceFixture $f @'
Check ((Get-Content C:\build\provision\FAILED.txt -Raw) -like '*OS servicing must complete*') 'Missing marker was accepted'
Check ($global:f.calls.Count -eq 0) 'Unserviced OS reached installer'
Set-Content C:\build\provision\FAILED.txt 'original native update failure'
'@
        $null = Invoke-ServiceFixture $f '& C:\build\provision\finalize-job-baseline.ps1 -BeforeToolchain'
        $null = Invoke-ServiceFixture $f @'
Check ((Get-Content C:\build\provision\FAILED.txt -Raw).Trim() -eq 'original native update failure') 'Original failure was overwritten'
Check ($global:f.calls.Count -eq 0) 'Failed image resumed'
'@
    } finally { Close-ServiceFixture $f }
}
Write-Host "Servicing checks: $passed passed, $failed failed, 0 skipped"
if ($failed -ne 0) { throw 'Servicing control-flow checks failed' }
