# Execute the same comparison helper embedded in the qualification fixture.
$ErrorActionPreference = 'Stop'
. "$PSScriptRoot/../../crates/motor-runner/src/tests/windows_link_target.ps1"
$cases = @(
    @('C:\Windows\System32','C:\Windows\System32',$true),
    @('c:\WINDOWS\system32','C:\Windows\System32',$true),
    @('UNC\motor-symlink.invalid\share\absent','\\motor-symlink.invalid\share\absent',$true),
    @('\\motor-symlink.invalid\share\absent','\\motor-symlink.invalid\share\absent',$true),
    @('UNC\other\share\absent','\\motor-symlink.invalid\share\absent',$false),
    @('C:\Windows\System32\other','C:\Windows\System32',$false),
    @('C:\Windows\System32 ','C:\Windows\System32',$false),
    @('C:\Windows\foo\..\System32','C:\Windows\System32',$false),
    @('\\?\C:\Windows\System32','C:\Windows\System32',$false),
    @('relative','RELATIVE',$false),
    @('UNC\server\share','UNC\server\share',$true)
)
foreach ($case in $cases) {
    $actual = Test-MotorLinkTarget $case[0] $case[1]
    if ($actual -ne $case[2]) { throw "comparison mismatch: actual=<$($case[0])> expected=<$($case[1])> result=$actual" }
}
Write-Host "Link target comparison checks: $($cases.Count) passed"
