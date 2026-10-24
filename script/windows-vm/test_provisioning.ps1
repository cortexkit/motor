# Portable parser/control-flow checks. Real Windows baseline qualification is separate.
param([string]$Scripts = $PSScriptRoot)
$ErrorActionPreference = 'Stop'

function Assert($condition, [string]$message) {
    if (-not $condition) { throw $message }
}

$checks = 0
foreach ($name in @('bootstrap.ps1', 'setup-guest.ps1', 'setup-job-baseline.ps1', 'install-powershell.ps1', 'register-job-bootstrap.ps1', 'finalize-job-baseline.ps1')) {
    $tokens = $null
    $errors = $null
    $null = [Management.Automation.Language.Parser]::ParseFile((Join-Path $Scripts $name), [ref]$tokens, [ref]$errors)
    Assert ($errors.Count -eq 0) "$name parse failed: $errors"
    $checks++
    Write-Host "PASS syntax_$name"
}

# These assertions defend the provisioning inputs, not Windows installer behavior.
$baseline = Get-Content (Join-Path $Scripts 'setup-job-baseline.ps1') -Raw
$ast = [Management.Automation.Language.Parser]::ParseInput($baseline, [ref]$null, [ref]$null)
foreach ($commandText in @('& $rustup component add clippy rustfmt --toolchain $toolchain', '& $rustup target add aarch64-pc-windows-msvc --toolchain $toolchain')) {
    $commands = @($ast.FindAll({param($node) $node -is [Management.Automation.Language.CommandAst] -and $node.Extent.Text -eq $commandText}, $true))
    Assert ($commands.Count -eq 1) "expected one unconditional $commandText"
    $parent = $commands[0].Parent
    while ($parent -ne $ast) {
        Assert ($parent -is [Management.Automation.Language.PipelineAst] -or $parent -is [Management.Automation.Language.NamedBlockAst]) "conditional Rust installation: $commandText"
        $parent = $parent.Parent
    }
}
$checks++
Write-Host 'PASS rust_components_and_arm64_target_unconditional'

# Execute the real inventory guards with complete and incomplete rustup output.
$hostTriple = 'x86_64-pc-windows-msvc'
foreach ($inventory in @('components', 'targets')) {
    $variable = if ($inventory -eq 'components') { 'component' } else { 'target' }
    $guard = $ast.Find({param($node) $node -is [Management.Automation.Language.ForEachStatementAst] -and $node.Variable.VariablePath.UserPath -eq $variable}, $true)
    Assert ($null -ne $guard) "missing $inventory inventory guard"
    $complete = if ($inventory -eq 'components') { @("clippy-$hostTriple", "rustfmt-$hostTriple") } else { @($hostTriple, 'aarch64-pc-windows-msvc') }
    foreach ($missing in @('', $complete[0], $complete[1])) {
        Set-Variable -Name $inventory -Value @($complete | Where-Object { $_ -ne $missing })
        $threw = $false
        try { Invoke-Expression $guard.Extent.Text } catch {
            Assert ($_.ToString().Contains('Missing Rust')) "unexpected inventory-check error: $_"
            $threw = $true
        }
        Assert ($threw -eq ($missing -ne '')) "$inventory guard accepted missing '$missing' or rejected complete output"
    }
    $checks++
    Write-Host "PASS rust_${inventory}_inventory_rejects_missing_entries"
}
$definition = $ast.Find({param($node) $node -is [Management.Automation.Language.FunctionDefinitionAst] -and $node.Name -eq 'Resolve-MotorTrusteeSid'}, $true)
Invoke-Expression $definition.Extent.Text
$resolver = {param($name) if ($name -in @('motor-job','HOST\motor-job')) { 'S-1-5-21-123-1001' } else { throw 'unknown account' }}
foreach ($export in @('SeBatchLogonRight = motor-job,*S-1-5-32-544', 'SeBatchLogonRight = *S-1-5-21-123-1001,*S-1-5-32-544')) {
    $sids = @((($export -split '=',2)[1] -split ',') | ForEach-Object { Resolve-MotorTrusteeSid $_ $resolver })
    Assert ($sids -contains 'S-1-5-21-123-1001') 'exported job trustee did not resolve'
    Assert ($sids.Count -eq 2) 'trustee normalization duplicated a grant'
}
$rejected = $false
try { Resolve-MotorTrusteeSid 'missing-user' $resolver } catch { $rejected = $_.ToString().Contains('missing-user') }
Assert $rejected 'unresolvable trustee did not fail closed with its name'
$checks++
Write-Host 'PASS policy_named_and_sid_trustees'
$definition = $ast.Find({param($node) $node -is [Management.Automation.Language.FunctionDefinitionAst] -and $node.Name -eq 'Assert-MotorDeveloperModeDisabled'}, $true)
Invoke-Expression $definition.Extent.Text
foreach ($state in @('key-absent','value-absent','zero','one')) {
    $readKey = {
        switch ($state) {
            'key-absent' { return $null }
            'value-absent' { return [pscustomobject]@{} }
            'zero' { return [pscustomobject]@{AllowDevelopmentWithoutDevLicense=0} }
            'one' { return [pscustomobject]@{AllowDevelopmentWithoutDevLicense=1} }
        }
    }
    $threw = $false
    try { Assert-MotorDeveloperModeDisabled $readKey } catch {
        Assert ($_.ToString().Contains('must not enable Developer Mode')) "unexpected registry-check error: $_"
        $threw = $true
    }
    Assert ($threw -eq ($state -eq 'one')) "wrong Developer Mode result for $state"
}
$checks++
Write-Host 'PASS developer_mode_absent_key_absent_value_zero_and_one'
$items = [ordered]@{
    machine_toolchain = @("`$toolchain = '1.99.0'", "`$hostTriple = 'x86_64-pc-windows-msvc'", "`$env:RUSTUP_HOME = 'C:\rust\rustup'", "`$env:CARGO_HOME = 'C:\rust\cargo'", '--default-toolchain $toolchain --default-host $hostTriple --no-modify-path', "SetEnvironmentVariable('RUSTUP_HOME'", "SetEnvironmentVariable('Path'", "'C:\rust' /inheritance:r", ':(OI)(CI)RX')
    virtio_serial = @('vioserial\2k22\amd64\vioser.inf', '& pnputil /add-driver', '/install')
    standard_job_user = @("`$jobUser = 'motor-job'", 'New-LocalUser $jobUser', 'Remove-LocalGroupMember $admins', 'SeBatchLogonRight', 'SeCreateSymbolicLinkPrivilege', 'Job SID missing from $right', 'Job baseline must not enable Developer Mode', 'CreateProfile($jobSid', 'job-password.txt', '$credential /inheritance:r')
    service = @('build --release --locked', 'New-Service -Name motor-winagent', '-StartupType Automatic', "StartName -ne 'LocalSystem'", 'Get-FileHash', 'SHA256')
    long_paths = @('config --system core.longpaths true', 'LongPathsEnabled -PropertyType DWord -Value 1')
    defender = @("@('C:\m', 'C:\rust', `$jobCargo)", 'Add-MpPreference -ExclusionPath $path')
    no_ssh = @('Stop-Service sshd', 'Remove-WindowsCapability -Online -Name OpenSSH.Server', "'C:\ProgramData\ssh'", 'authorized_keys', "Join-Path `$_.FullName '.ssh'")
}
foreach ($item in $items.GetEnumerator()) {
    foreach ($input in $item.Value) { Assert ($baseline.Contains($input)) "baseline_$($item.Key) missing $input" }
    $checks++
    Write-Host "PASS baseline_$($item.Key)"
}

if ($env:OS -ne 'Windows_NT') {
    $temp = Join-Path ([IO.Path]::GetTempPath()) ([Guid]::NewGuid().ToString())
    New-Item "$temp/c","$temp/s" -ItemType Directory -Force | Out-Null
    New-PSDrive -Name C -PSProvider FileSystem -Root "$temp/c" | Out-Null
    New-PSDrive -Name S -PSProvider FileSystem -Root "$temp/s" | Out-Null
    try {
        Copy-Item (Join-Path $Scripts 'bootstrap.ps1') S:\
        New-Item S:\motor-winagent\src -ItemType Directory -Force | Out-Null
        Set-Content S:\motor-winagent\Cargo.lock 'source lock'
        Set-Content S:\motor-winagent\src\main.rs 'source main'
        Set-Content S:\setup-guest.ps1 'param([switch]$JobBaseline); if (-not $JobBaseline) { throw "mode not forwarded" }; Set-Content C:\build\provision\guest-called yes'
        Set-Content S:\PowerShell-7.6.7-win-x64.zip 'media fixture'
        Set-Content S:\finalize-job-baseline.ps1 'finalizer fixture'
        Set-Content S:\setup-job-baseline.ps1 'Set-Content C:\build\provision\baseline-called yes'
        # No authorized_keys exists. Entering any of the SSH setup block is a failure.
        function Get-Volume { [pscustomobject]@{ DriveLetter = 'S' } }
        function Get-NetAdapter { [pscustomobject]@{ InterfaceDescription = 'VirtIO'; Status = 'Up' } }
        function Set-LocalUser {}
        function Disable-LocalUser {}
        function Unregister-ScheduledTask {}
        function New-ScheduledTaskAction {}
        function New-ScheduledTaskTrigger {}
        function Register-ScheduledTask { Set-Content C:\build\provision\finalizer-registered yes }
        function Start-ScheduledTask { Set-Content C:\build\provision\finalizer-started yes }
        function cscript.exe { $global:LASTEXITCODE = 0 }
        function Set-ItemProperty {}
        function Remove-ItemProperty {}
        function New-ItemProperty {}
        function Add-WindowsCapability { throw 'SSH setup executed in job-baseline mode' }
        foreach ($mode in @('marker', 'switch')) {
            try {
            if ($mode -eq 'marker') {
                New-Item S:\job-baseline -ItemType File | Out-Null
                & S:\bootstrap.ps1
                Remove-Item S:\job-baseline
            } else {
                & S:\bootstrap.ps1 -JobBaseline
            }
            Assert (Test-Path C:\build\provision\guest-called) "bootstrap_$mode did not call shared setup"
            Assert (Test-Path C:\build\provision\baseline-called) "bootstrap_$mode did not call baseline setup"
            Assert (Test-Path C:\build\provision\motor-winagent\Cargo.lock) "bootstrap_$mode lost agent lockfile"
            Assert (Test-Path C:\build\provision\PowerShell-7.6.7-win-x64.zip) "bootstrap_$mode lost PowerShell media"
            Assert (Test-Path C:\build\provision\finalize-job-baseline.ps1) "bootstrap_$mode lost finalizer"
            Assert (Test-Path C:\build\provision\finalizer-registered) "bootstrap_$mode did not register finalizer"
            Assert (Test-Path C:\build\provision\finalizer-started) "bootstrap_$mode did not start finalizer"
            Assert (-not (Test-Path C:\build\provision\ssh-ready)) "bootstrap_$mode enabled SSH"
            Remove-Item C:\build\provision -Recurse -Force
            $checks++
            Write-Host "PASS bootstrap_job_baseline_$mode"
            } catch {
                throw "FAIL bootstrap_job_baseline_${mode}: $_"
            }
        }
        Set-Content S:\setup-job-baseline.ps1 'throw "baseline failure fixture"'
        function Stop-Computer { Set-Content C:\build\provision\shutdown-requested yes; throw 'shutdown observed' }
        $shutdown = $false
        try { & S:\bootstrap.ps1 -JobBaseline } catch { $shutdown = $_.ToString().Contains('shutdown observed') }
        Assert $shutdown 'failed job bootstrap did not request shutdown'
        Assert (Test-Path C:\build\provision\shutdown-requested) 'shutdown was not dispatched'
        Assert ((Get-Content C:\build\provision\FAILED.txt -Raw).Contains('baseline failure fixture')) 'bootstrap lost original failure'
        $checks++
        Write-Host 'PASS bootstrap_failure_records_error_and_shuts_down'
        # Substitute only the Windows elevation query so the real setup flow can run portably.
        $guest = Get-Content (Join-Path $Scripts 'setup-guest.ps1') -Raw
        $adminQuery = '$IsAdmin = ([Security.Principal.WindowsPrincipal] [Security.Principal.WindowsIdentity]::GetCurrent()).IsInRole([Security.Principal.WindowsBuiltInRole] "Administrator")'
        Assert ($guest.Contains($adminQuery)) 'portable setup adapter no longer matches the elevation query'
        Set-Content "$temp/setup-guest.ps1" ($guest.Replace($adminQuery, '$IsAdmin = $true'))
        function Add-MpPreference {}
        function Get-Command { [pscustomobject]@{ Name = 'installed' } }
        function winget { $global:LASTEXITCODE = 0 }
        function git { $global:LASTEXITCODE = 0; 'git version fixture' }
        function bun { $global:LASTEXITCODE = 0; 'bun version fixture' }
        function cargo { throw 'per-user Cargo was accessed in job-baseline mode' }
        function rustc { throw 'per-user rustc was accessed in job-baseline mode' }
        function Invoke-WebRequest { throw 'per-user Rust installer was downloaded' }
        try {
            & "$temp/setup-guest.ps1" -JobBaseline
        } catch {
            throw "FAIL setup_guest_skips_per_user_rust: $_"
        }
        $checks++
        Write-Host 'PASS setup_guest_skips_per_user_rust'
    } finally {
        Remove-PSDrive C,S
        Remove-Item $temp -Recurse -Force
    }
} else {
    Write-Host 'SKIP portable bootstrap mocks: do not replace Windows C: drive'
}
Write-Host "Provisioning checks: $checks passed"
