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

$nextestInstall = @($ast.EndBlock.Statements | Where-Object {
    $_ -is [Management.Automation.Language.TryStatementAst] -and $_.Extent.Text.Contains('-OutFile $nextestArchive')
})
Assert ($nextestInstall.Count -eq 1) 'nextest must install unconditionally for QEMU and Hyper-V images'
$nextestSetup = $nextestInstall[0].Extent.Text
Assert ($nextestSetup.IndexOf('-OutFile $nextestArchive') -lt $nextestSetup.IndexOf('Get-FileHash -LiteralPath $nextestArchive')) 'nextest hash must follow download'
Assert ($nextestSetup.IndexOf("throw 'cargo-nextest archive SHA-256 mismatch'") -lt $nextestSetup.IndexOf('-xzf $nextestArchive')) 'nextest hash must be verified before extraction'
Assert ($nextestInstall[0].Extent.EndOffset -lt $baseline.IndexOf("# Freeze toolchain files")) 'nextest must install before the toolchain is made read-only'
$checks++
Write-Host 'PASS nextest_unconditional_download_verify_extract_before_freeze'

# Execute the installer's guards without downloading or running Windows tools.
$nextestHashGuard = $ast.Find({param($node) $node -is [Management.Automation.Language.IfStatementAst] -and $node.Clauses[0].Item1.Extent.Text.Contains('Get-FileHash -LiteralPath $nextestArchive')}, $true)
Assert ($null -ne $nextestHashGuard) 'missing nextest archive hash guard'
$nextestArchive = 'nextest-fixture.tar.gz'
$nextestSha256 = 'e4606198dd067299f16863dcf5b9be007b9cae041e468c417884fc224760c0f4'
function Get-FileHash {
    param($LiteralPath, $Algorithm)
    Assert ($LiteralPath -eq $nextestArchive -and $Algorithm -eq 'SHA256') 'nextest hashed the wrong archive or algorithm'
    [pscustomobject]@{ Hash = $fixtureHash }
}
try {
    foreach ($fixtureHash in @($nextestSha256.ToUpperInvariant(), ('0' * 64))) {
        $threw = $false
        try { Invoke-Expression $nextestHashGuard.Extent.Text } catch {
            Assert ($_.ToString().Contains('cargo-nextest archive SHA-256 mismatch')) "unexpected nextest hash error: $_"
            $threw = $true
        }
        Assert ($threw -eq ($fixtureHash -eq ('0' * 64))) 'nextest accepted a wrong hash or rejected the pinned hash'
    }
} finally {
    Remove-Item Function:\Get-FileHash
}
$checks++
Write-Host 'PASS nextest_hash_guard_rejects_mismatch'

$nextestVersionGuard = $ast.Find({param($node) $node -is [Management.Automation.Language.IfStatementAst] -and $node.Clauses[0].Item1.Extent.Text.Contains('$nextestOutput | Select-Object -First 1) -notmatch')}, $true)
Assert ($null -ne $nextestVersionGuard) 'missing nextest version guard'
$nextestVersion = '0.9.138'
# The first case is nextest 0.9.138's real `--version` output: five lines.
$realOutput = @('cargo-nextest 0.9.138 (fc97e97bb 2026-06-21)', 'release: 0.9.138', 'commit-hash: fc97e97bbe0a3927482a694247da00c099f4269e', 'commit-date: 2026-06-21', 'host: x86_64-pc-windows-msvc')
foreach ($case in @(
    @{Version=$realOutput; Code=0; Reject=$false},
    @{Version=@('cargo-nextest 0.9.138 (fc97e97bb 2026-06-21)'); Code=0; Reject=$false},
    @{Version=@("cargo-nextest 0.9.138 (fc97e97bb 2026-06-21)`r"); Code=0; Reject=$false},
    @{Version=@('cargo-nextest 0.9.137 (old)', 'release: 0.9.137'); Code=0; Reject=$true},
    @{Version=@('release: 0.9.138', 'cargo-nextest 0.9.138 (fc97e97bb 2026-06-21)'); Code=0; Reject=$true},
    @{Version=@('cargo-nextest 0.9.1380 (wrong)'); Code=0; Reject=$true},
    @{Version=@(); Code=127; Reject=$true},
    @{Version=$realOutput; Code=1; Reject=$true}
)) {
    $nextestOutput = $case.Version
    $LASTEXITCODE = $case.Code
    $threw = $false
    try { Invoke-Expression $nextestVersionGuard.Extent.Text } catch {
        Assert ($_.ToString().Contains('Machine-wide cargo-nextest version verification failed')) "unexpected nextest version error: $_"
        $threw = $true
    }
    Assert ($threw -eq $case.Reject) "wrong nextest version guard result: $($case.Version -join ' | '), exit $($case.Code)"
}
$checks++
Write-Host 'PASS nextest_version_guard_rejects_wrong_missing_and_failed_tools'

$pythonInstall = @($ast.EndBlock.Statements | Where-Object {
    $_ -is [Management.Automation.Language.TryStatementAst] -and $_.Extent.Text.Contains('-OutFile $pythonInstaller')
})
Assert ($pythonInstall.Count -eq 1) 'Python must install unconditionally for QEMU and Hyper-V images'
$pythonSetup = $pythonInstall[0].Extent.Text
Assert ($pythonSetup.IndexOf('-OutFile $pythonInstaller') -lt $pythonSetup.IndexOf('Get-FileHash -LiteralPath $pythonInstaller')) 'Python hash must follow download'
Assert ($pythonSetup.IndexOf("throw 'Python installer SHA-256 mismatch'") -lt $pythonSetup.IndexOf('Start-Process -FilePath $pythonInstaller')) 'Python hash must be verified before execution'
Assert ($pythonSetup.IndexOf('Start-Process -FilePath $pythonInstaller') -lt $pythonSetup.IndexOf('Copy-Item -LiteralPath "$pythonInstall\python.exe"')) 'python3 must come from the installed interpreter'
$checks++
Write-Host 'PASS python_unconditional_download_verify_install_and_alias'

$pythonHashGuard = $ast.Find({param($node) $node -is [Management.Automation.Language.IfStatementAst] -and $node.Clauses[0].Item1.Extent.Text.Contains('Get-FileHash -LiteralPath $pythonInstaller')}, $true)
Assert ($null -ne $pythonHashGuard) 'missing Python installer hash guard'
$pythonInstaller = 'python-fixture.exe'
$pythonSha256 = '5da012101ba2e17113a9a1cd901e9ce1441086f2b7ecb5de06ec94163899a190'
function Get-FileHash {
    param($LiteralPath, $Algorithm)
    Assert ($LiteralPath -eq $pythonInstaller -and $Algorithm -eq 'SHA256') 'Python hashed the wrong installer or algorithm'
    [pscustomobject]@{ Hash = $fixtureHash }
}
try {
    foreach ($fixtureHash in @($pythonSha256.ToUpperInvariant(), ('0' * 64))) {
        $threw = $false
        try { Invoke-Expression $pythonHashGuard.Extent.Text } catch {
            Assert ($_.ToString().Contains('Python installer SHA-256 mismatch')) "unexpected Python hash error: $_"
            $threw = $true
        }
        Assert ($threw -eq ($fixtureHash -eq ('0' * 64))) 'Python accepted a wrong hash or rejected the pinned hash'
    }
} finally {
    Remove-Item Function:\Get-FileHash
}
$checks++
Write-Host 'PASS python_hash_guard_rejects_mismatch'

$pythonExitGuard = $ast.Find({param($node) $node -is [Management.Automation.Language.IfStatementAst] -and $node.Clauses[0].Item1.Extent.Text.Contains('$pythonProcess.ExitCode')}, $true)
Assert ($null -ne $pythonExitGuard) 'missing Python installer exit code guard'
foreach ($code in @(0, 3010, 1, 1603)) {
    $pythonProcess = [pscustomobject]@{ExitCode = $code}
    $threw = $false
    try { Invoke-Expression $pythonExitGuard.Extent.Text } catch {
        Assert ($_.ToString().Contains('Machine-wide Python installation failed')) "unexpected Python installer error: $_"
        $threw = $true
    }
    Assert ($threw -eq ($code -notin @(0,3010))) "wrong Python installer exit code result: $code"
}
$checks++
Write-Host 'PASS python_installer_exit_guard_rejects_failure'

$pythonVersionGuard = $ast.Find({param($node) $node -is [Management.Automation.Language.IfStatementAst] -and $node.Clauses[0].Item1.Extent.Text.Contains('$installedPythonVersion -cne')}, $true)
Assert ($null -ne $pythonVersionGuard) 'missing Python version guard'
$pythonVersion = '3.14.8'
$command = 'python3.exe'
foreach ($case in @(
    @{Version='Python 3.14.8'; Code=0; Reject=$false},
    @{Version='Python 3.14.7'; Code=0; Reject=$true},
    @{Version='Python 3.14.80'; Code=0; Reject=$true},
    @{Version=''; Code=127; Reject=$true},
    @{Version='Python 3.14.8'; Code=1; Reject=$true}
)) {
    $installedPythonVersion = $case.Version
    $LASTEXITCODE = $case.Code
    $threw = $false
    try { Invoke-Expression $pythonVersionGuard.Extent.Text } catch {
        Assert ($_.ToString().Contains('Machine-wide Python version verification failed')) "unexpected Python version error: $_"
        $threw = $true
    }
    Assert ($threw -eq $case.Reject) "wrong Python version guard result: $($case.Version), exit $($case.Code)"
}
$checks++
Write-Host 'PASS python_version_guard_rejects_wrong_missing_and_failed_tools'

$pythonStdlibGuard = $ast.Find({param($node) $node -is [Management.Automation.Language.IfStatementAst] -and $node.Extent.Text.Contains("throw 'Machine-wide Python stdlib verification failed'")}, $true)
Assert ($null -ne $pythonStdlibGuard) 'missing Python stdlib guard'
foreach ($code in @(0, 1)) {
    $LASTEXITCODE = $code
    $threw = $false
    try { Invoke-Expression $pythonStdlibGuard.Extent.Text } catch {
        Assert ($_.ToString().Contains('Machine-wide Python stdlib verification failed')) "unexpected Python stdlib error: $_"
        $threw = $true
    }
    Assert ($threw -eq ($code -ne 0)) "wrong Python stdlib guard result: $code"
}
$checks++
Write-Host 'PASS python_stdlib_guard_rejects_failure'

$natsInstall = @($ast.EndBlock.Statements | Where-Object {
    $_ -is [Management.Automation.Language.TryStatementAst] -and $_.Extent.Text.Contains('-OutFile $natsArchive')
})
Assert ($natsInstall.Count -eq 1) 'NATS must install unconditionally for QEMU and Hyper-V images'
$natsSetup = $natsInstall[0].Extent.Text
Assert ($natsSetup.IndexOf('-OutFile $natsArchive') -lt $natsSetup.IndexOf('Get-FileHash -LiteralPath $natsArchive')) 'NATS hash must follow download'
Assert ($natsSetup.IndexOf("throw 'nats-server archive SHA-256 mismatch'") -lt $natsSetup.IndexOf('Expand-Archive -LiteralPath $natsArchive')) 'NATS hash must be verified before extraction'
Assert ($natsSetup.IndexOf('& icacls $natsInstall /inheritance:r') -lt $natsSetup.IndexOf('Expand-Archive -LiteralPath $natsArchive')) 'NATS root must be protected before extraction'
$checks++
Write-Host 'PASS nats_unconditional_download_verify_extract'

$natsHashGuard = $ast.Find({param($node) $node -is [Management.Automation.Language.IfStatementAst] -and $node.Clauses[0].Item1.Extent.Text.Contains('Get-FileHash -LiteralPath $natsArchive')}, $true)
Assert ($null -ne $natsHashGuard) 'missing NATS archive hash guard'
$natsArchive = 'nats-fixture.zip'
$natsSha256 = '2d0861dc2ca3567d17f7d19611d7b346cc2ccbd7086a0a77cc957a2c71663200'
function Get-FileHash {
    param($LiteralPath, $Algorithm)
    Assert ($LiteralPath -eq $natsArchive -and $Algorithm -eq 'SHA256') 'NATS hashed the wrong archive or algorithm'
    [pscustomobject]@{ Hash = $fixtureHash }
}
try {
    foreach ($fixtureHash in @($natsSha256.ToUpperInvariant(), ('0' * 64))) {
        $threw = $false
        try { Invoke-Expression $natsHashGuard.Extent.Text } catch {
            Assert ($_.ToString().Contains('nats-server archive SHA-256 mismatch')) "unexpected NATS hash error: $_"
            $threw = $true
        }
        Assert ($threw -eq ($fixtureHash -eq ('0' * 64))) 'NATS accepted a wrong hash or rejected the pinned hash'
    }
} finally {
    Remove-Item Function:\Get-FileHash
}
$checks++
Write-Host 'PASS nats_hash_guard_rejects_mismatch'

$natsVersionGuard = $ast.Find({param($node) $node -is [Management.Automation.Language.IfStatementAst] -and $node.Clauses[0].Item1.Extent.Text.Contains('$installedNatsVersion -cne')}, $true)
Assert ($null -ne $natsVersionGuard) 'missing NATS version guard'
$natsVersion = '2.15.0'
foreach ($case in @(
    @{Version='nats-server: v2.15.0'; Code=0; Reject=$false},
    @{Version='nats-server: v2.14.0'; Code=0; Reject=$true},
    @{Version='nats-server: v2.15.00'; Code=0; Reject=$true},
    @{Version=''; Code=127; Reject=$true},
    @{Version='nats-server: v2.15.0'; Code=1; Reject=$true}
)) {
    $installedNatsVersion = $case.Version
    $LASTEXITCODE = $case.Code
    $threw = $false
    try { Invoke-Expression $natsVersionGuard.Extent.Text } catch {
        Assert ($_.ToString().Contains('Machine-wide nats-server version verification failed')) "unexpected NATS version error: $_"
        $threw = $true
    }
    Assert ($threw -eq $case.Reject) "wrong NATS version guard result: $($case.Version), exit $($case.Code)"
}
$checks++
Write-Host 'PASS nats_version_guard_rejects_wrong_missing_and_failed_tools'

$stableCopyText = 'Copy-Item -LiteralPath "$env:RUSTUP_HOME\toolchains\$toolchain-$hostTriple" -Destination $stableInstall -Recurse'
$stableCopy = @($ast.EndBlock.Statements | Where-Object { $_.Extent.Text -eq $stableCopyText })
Assert ($stableCopy.Count -eq 1) 'stable must be a complete unconditional copy of the numbered Rust toolchain'
Assert ($baseline.IndexOf('target add aarch64-pc-windows-msvc') -lt $stableCopy[0].Extent.StartOffset) 'stable must include the installed ARM64 target'
Assert ($baseline.IndexOf('component add clippy rustfmt') -lt $stableCopy[0].Extent.StartOffset) 'stable must include the installed Rust components'
Assert ($stableCopy[0].Extent.EndOffset -lt $baseline.IndexOf('# Freeze toolchain files')) 'stable must inherit the read-only toolchain ACL'
$checks++
Write-Host 'PASS rust_stable_complete_copy_before_freeze'

$stableVersionGuard = $ast.Find({param($node) $node -is [Management.Automation.Language.IfStatementAst] -and $node.Clauses[0].Item1.Extent.Text.Contains('$stableRustc -notcontains')}, $true)
Assert ($null -ne $stableVersionGuard) 'missing stable version guard'
$toolchain = '1.99.0'
$hostTriple = 'x86_64-pc-windows-msvc'
foreach ($case in @(
    @{Version=@('release: 1.99.0', "host: $hostTriple"); Code=0; Reject=$false},
    @{Version=@('release: 1.98.0', "host: $hostTriple"); Code=0; Reject=$true},
    @{Version=@('release: 1.99.00', "host: $hostTriple"); Code=0; Reject=$true},
    @{Version=@('release: 1.99.0', 'host: aarch64-pc-windows-msvc'); Code=0; Reject=$true},
    @{Version=@(); Code=127; Reject=$true},
    @{Version=@('release: 1.99.0', "host: $hostTriple"); Code=1; Reject=$true}
)) {
    $stableRustc = $case.Version
    $LASTEXITCODE = $case.Code
    $threw = $false
    try { Invoke-Expression $stableVersionGuard.Extent.Text } catch {
        Assert ($_.ToString().Contains('Machine-wide Rust stable version/host verification failed')) "unexpected stable version error: $_"
        $threw = $true
    }
    Assert ($threw -eq $case.Reject) "wrong stable version guard result: $($case.Version -join ' | '), exit $($case.Code)"
}
$checks++
Write-Host 'PASS rust_stable_version_guard_rejects_wrong_missing_and_failed_tools'

foreach ($inventory in @('components', 'targets')) {
    $guard = $ast.Find({param($node) $node -is [Management.Automation.Language.IfStatementAst] -and $node.Extent.Text.Contains("throw 'Rust stable $inventory differ from pinned toolchain'")}, $true)
    Assert ($null -ne $guard) "missing stable $inventory guard"
    $complete = if ($inventory -eq 'components') { @("clippy-$hostTriple", "rustfmt-$hostTriple") } else { @($hostTriple, 'aarch64-pc-windows-msvc') }
    Set-Variable -Name $inventory -Value $complete
    foreach ($case in @(
        @{Entries=$complete; Code=0; Reject=$false},
        @{Entries=@($complete[0]); Code=0; Reject=$true},
        @{Entries=@($complete[1]); Code=0; Reject=$true},
        @{Entries=@($complete + 'unexpected'); Code=0; Reject=$true},
        @{Entries=$complete; Code=1; Reject=$true}
    )) {
        Set-Variable -Name "stable$inventory" -Value $case.Entries
        $LASTEXITCODE = $case.Code
        $threw = $false
        try { Invoke-Expression $guard.Extent.Text } catch {
            Assert ($_.ToString().Contains("Rust stable $inventory differ from pinned toolchain")) "unexpected stable inventory error: $_"
            $threw = $true
        }
        Assert ($threw -eq $case.Reject) "stable $inventory guard accepted mismatched output or rejected matching output"
    }
    $checks++
    Write-Host "PASS rust_stable_${inventory}_guard_rejects_mismatch_and_failure"
}

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
    machine_bun = @("`$bunVersion = '1.4.2'", 'bun-v1.4.2/bun-windows-x64.zip', 'bun-v1.4.2/SHASUMS256.txt', 'ce4c17497b2f29712a99d3d53f028de28cd42e3bacb8589599e7f000e49b6405', 'Get-FileHash -LiteralPath $bunArchive -Algorithm SHA256', 'throw ''Bun archive SHA-256 mismatch''', "`$bunInstall = 'C:\Program Files\Bun'", '& icacls $bunInstall /inheritance:r', '*S-1-5-32-545:(OI)(CI)RX', '& icacls $bunInstall /setowner', 'SetEnvironmentVariable(''Path'', "$bunInstall;$machinePath", ''Machine'')', '& "$bunInstall\bun.exe" --version', '$installedBunVersion -cne $bunVersion')
    machine_nextest = @("`$nextestVersion = '0.9.138'", 'cargo-nextest-0.9.138/cargo-nextest-0.9.138-x86_64-pc-windows-msvc.tar.gz', 'e4606198dd067299f16863dcf5b9be007b9cae041e468c417884fc224760c0f4', '-xzf $nextestArchive -C ''C:\rust\cargo\bin'' cargo-nextest.exe', "if (`$LASTEXITCODE -ne 0) { throw 'cargo-nextest extraction failed' }", 'Remove-Item -LiteralPath $nextestArchive -Force', '& ''C:\rust\cargo\bin\cargo.exe'' nextest --version')
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
        function bun { throw 'per-user Bun was accessed in job-baseline mode' }
        function cargo { throw 'per-user Cargo was accessed in job-baseline mode' }
        function rustc { throw 'per-user rustc was accessed in job-baseline mode' }
        function Invoke-WebRequest { throw 'per-user installer was downloaded' }
        function Invoke-RestMethod { throw 'per-user Bun installer was downloaded' }
        try {
            & "$temp/setup-guest.ps1" -JobBaseline
        } catch {
            throw "FAIL setup_guest_skips_per_user_bun_and_rust: $_"
        }
        $checks++
        Write-Host 'PASS setup_guest_skips_per_user_bun_and_rust'
    } finally {
        Remove-PSDrive C,S
        Remove-Item $temp -Recurse -Force
    }
} else {
    Write-Host 'SKIP portable bootstrap mocks: do not replace Windows C: drive'
}
Write-Host "Provisioning checks: $checks passed"
