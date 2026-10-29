# Run without Hyper-V or elevation: provider operations are fixture objects,
# while the production ownership, path and removal predicates execute unchanged.
$ErrorActionPreference = 'Stop'
Set-StrictMode -Version Latest
$tokens = $null; $errors = $null
$ast = [Management.Automation.Language.Parser]::ParseFile((Join-Path $PSScriptRoot 'hyperv.ps1'), [ref]$tokens, [ref]$errors)
if ($errors.Count) { throw "Hyper-V script parse failed: $errors" }
$names = @('Job-Path','Owned-VM','Assert-JobDiskBinding','Test-IncompleteJob','Remove-Job')
foreach ($name in $names) {
    $function = $ast.Find({ param($node) $node -is [Management.Automation.Language.FunctionDefinitionAst] -and $node.Name -eq $name }, $true)
    if (-not $function) { throw "Missing production function $name" }
    . ([scriptblock]::Create($function.Extent.Text))
}
$Root = Join-Path $PSScriptRoot ('..\..\target\scratch\slot-cleanup-' + [Guid]::NewGuid())
$Root = [IO.Path]::GetFullPath($Root)
$prefix = 'motor-ally-'; $owner = "MotorRunner:$Root"
$id = [Guid]::NewGuid().ToString()
$directory = Job-Path $id
$script:vm = $null; $script:disks = @(); $script:dvds = @(); $script:harvests = 0; $script:removals = 0
function Get-VM { param($Name, $ErrorAction) return $script:vm }
function Get-VMHardDiskDrive { param($VM) return $script:disks }
function Get-VMDvdDrive { param($VM) return $script:dvds }
function Get-VHD { param($Path) return [pscustomobject]@{Path=$Path; VhdType='Differencing'; ParentPath="$Root\baselines\parent.vhdx"; DiskIdentifier='fixture'} }
function Remove-VM { param($VM, [switch]$Force) $script:removals++; $script:vm = $null }
function Stop-VM { throw 'Fixture must never stop a non-Off incomplete VM' }
function Harvest-Job { param($Id, [switch]$Lightweight) $script:harvests++ }
function Evidence { param($Id, $Event, $Detail) }
function Save-Json { param($Path, $Value) }
function Reset-Fixture {
    New-Item -ItemType Directory -Path $directory -Force | Out-Null
    New-Item -ItemType Directory -Path "$Root\baselines" -Force | Out-Null
    Set-Content -LiteralPath "$directory\os.vhdx" -Value 'fixture child'
    @{parent="$Root\baselines\parent.vhdx"; sha256=('a' * 64); quota=4096} | ConvertTo-Json | Set-Content -LiteralPath "$directory\job.json"
    $script:vm = [pscustomobject]@{State='Off'; Notes="$owner`:$id"; Id=$id}
    $script:disks = @(); $script:dvds = @()
}
function Expect-Refusal([string]$Expected) {
    $failure = $null
    try { Remove-Job $id } catch { $failure = $_.Exception.Message }
    if (-not $failure -or $failure -notmatch $Expected) { throw "Expected $Expected refusal, got: $failure" }
    if (-not $script:vm -or -not (Test-Path -LiteralPath $directory)) { throw 'Refused cleanup deleted resources' }
    Write-Host "PASS incomplete-creation-refuses-$Expected"
}
try {
    Reset-Fixture
    Remove-Job $id
    if ($script:vm -or (Test-Path -LiteralPath $directory) -or $script:harvests -ne 0 -or $script:removals -ne 1) { throw 'Owned zero-disk VM was not safely removed' }
    Write-Host 'PASS owned-Off-zero-disk-creation-is-removed-without-mounting'
    Reset-Fixture; $script:vm.Notes = ''; Expect-Refusal 'Notes'
    Reset-Fixture; Remove-Item -LiteralPath "$directory\job.json"; Expect-Refusal 'job.json'
    Reset-Fixture; $script:vm.State = 'Running'; Expect-Refusal 'not Off'
    Reset-Fixture; $script:dvds = @([pscustomobject]@{Path="$Root\foreign.iso"}); Expect-Refusal 'DVD'
    Reset-Fixture
    @{parent="$Root\foreign\parent.vhdx"; sha256=('a' * 64); quota=4096} | ConvertTo-Json | Set-Content -LiteralPath "$directory\job.json"
    Expect-Refusal 'journal'
    Reset-Fixture; $script:disks = @([pscustomobject]@{ControllerNumber=0; ControllerLocation=0; Path="$Root\foreign.vhdx"}); Expect-Refusal 'does not match'
    Reset-Fixture; $script:disks = @([pscustomobject]@{ControllerNumber=0; ControllerLocation=1; Path="$directory\os.vhdx"}); Expect-Refusal 'exactly one'
    Write-Host 'PASS 8 slot cleanup checks (no VM started)'
} finally {
    Remove-Item -LiteralPath $Root -Recurse -Force
}
