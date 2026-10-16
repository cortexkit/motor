/// A script file avoids Windows PowerShell 5.1 rewriting quoted native arguments.
pub(super) fn command_resolution_script() -> String {
    r#"$ErrorActionPreference='Stop'
$cmd=(Get-Command cmd -ErrorAction Stop).Source
if(-not $cmd.EndsWith('\System32\cmd.exe',[StringComparison]::OrdinalIgnoreCase)){throw "cmd resolves outside System32: $cmd"}
cmd /c exit 7
exit $LASTEXITCODE
"#.into()
}

#[test]
fn windows_command_resolution_script_keeps_literal_cmd_path() {
    let script = command_resolution_script();
    assert!(
        script.contains(r".EndsWith('\System32\cmd.exe',[StringComparison]::OrdinalIgnoreCase)"),
        "{script}"
    );
    assert_eq!(script.lines().last(), Some("exit $LASTEXITCODE"));
}
