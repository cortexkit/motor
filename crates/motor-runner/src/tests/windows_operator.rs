//! Windows VM qualification runs separately from Linux host isolation tests.
//! Use a provisioned, otherwise unused Windows instance and run this lane:
//!
//! ```sh
//! MOTOR_WINDOWS_VM_CONFIG=<vm.toml> MOTOR_RUNNER_BINARY=<release motor-runner> \
//!   cargo nextest run -p motor-runner --lib --locked --run-ignored only -E 'test(/windows_qualification_/)'
//! ```
//!
//! Missing MOTOR_WINDOWS_VM_CONFIG or MOTOR_RUNNER_BINARY fails these tests
//! instead of skipping them. Every qualification test needs the real binary,
//! built from the same commit: trusted preparation runs it as `windows-prepare`,
//! and the runner-crash test kills it. Offline persistence canaries require sudo -n,
//! qemu-nbd, partprobe, kernel ntfs3 and hivexregedit (package libwin-hivex-perl)
//! on the host.

use super::*;
use crate::windows::{VmConfig, WindowsBackend};
use std::io::Read;

// This module is compiled only on Linux (see tests.rs), so no platform check
// is needed here.
fn operator() -> Runner {
    let path = std::env::var_os("MOTOR_WINDOWS_VM_CONFIG")
        .expect("set MOTOR_WINDOWS_VM_CONFIG to a provisioned, unused Windows instance's vm.toml");
    let vm = VmConfig::load(Path::new(&path)).unwrap();
    let instance = vm.layout_root.file_name().unwrap().to_str().unwrap();
    let mut config = Config::new(vm.layout_root.join("state"), instance);
    config.backend = BackendKind::WindowsVm;
    config.vm_config = Some(path.into());
    config.free_space_floor_bytes = 0;
    // Trusted preparation re-executes the runner as `windows-prepare`, which
    // only the real binary implements, never this test harness.
    config.windows_prep_executable = Some(
        std::env::var_os("MOTOR_RUNNER_BINARY")
            .expect("set MOTOR_RUNNER_BINARY to the freshly built motor-runner executable")
            .into(),
    );
    Runner::open(config).unwrap()
}
fn source(f: &Fixture) {
    for repository in [&f.main, &f.sibling] {
        git(repository, &["config", "user.name", "Motor fixture"]);
        git(
            repository,
            &["config", "user.email", "motor-fixture@example.invalid"],
        );
    }
    fs::create_dir_all(f.main.join("src")).unwrap();
    fs::write(
        f.main.join("Cargo.toml"),
        "[package]\nname='motor-qualification'\nversion='0.1.0'\nedition='2021'\n",
    )
    .unwrap();
    fs::write(
        f.main.join("Cargo.lock"),
        "version=3\n[[package]]\nname='motor-qualification'\nversion='0.1.0'\n",
    )
    .unwrap();
    fs::write(
        f.main.join("src/main.rs"),
        "fn main() { println!(\"MSVC binary ran\"); }\n",
    )
    .unwrap();
    git(&f.main, &["add", "."]);
    git(&f.main, &["commit", "-m", "qualification crate"]);
}
fn prepare(runner: &Runner, f: &Fixture, command: &str) -> (Setup, Prepared) {
    let (mut request, _) = f.run(command);
    request.platform = Some(Platform::Windows);
    for (repo, local) in [
        (&request.repository_root, &f.main),
        (&request.siblings[0].canonical_path, &f.sibling),
    ] {
        let mirror = runner.core.trees.ensure_mirror(repo).unwrap();
        git(&mirror, &["fetch", text(local), "HEAD"]);
    }
    let setup = Setup {
        id: request.job_id,
        request,
        cpus: vec![12, 13, 14, 15],
        base: false,
        recorded: BTreeMap::new(),
        snapshots: None,
    };
    let prepared = WindowsBackend
        .materialize(runner.config(), &runner.core.trees, &setup)
        .unwrap();
    (setup, prepared)
}
fn launch(runner: &Runner, f: &Fixture, command: &str) -> (Setup, backend::Launch) {
    let (setup, prepared) = prepare(runner, f, command);
    let mut launch = WindowsBackend
        .launch(runner.config(), &runner.core.trees, &setup, &prepared, 4)
        .unwrap();
    launch.execution =
        super::windows_instance::direct_execution(runner.config(), setup.id, launch.execution);
    drop(prepared);
    launch.execution.release().unwrap();
    (setup, launch)
}
fn run(runner: &Runner, f: &Fixture, command: &str) -> String {
    let (_, launch) = launch(runner, f, command);
    finish(launch)
}
fn finish(launch: backend::Launch) -> String {
    finish_with_context(launch, "")
}
fn finish_with_context(mut launch: backend::Launch, context: &str) -> String {
    finish_execution(launch.execution.as_mut(), context)
}
fn finish_execution(execution: &mut dyn backend::Execution, context: &str) -> String {
    let output = execution.output().unwrap();
    let read = |mut reader: Box<dyn Read + Send>| {
        std::thread::spawn(move || {
            let mut text = String::new();
            reader.read_to_string(&mut text).unwrap();
            text
        })
    };
    let stdout = read(output.stdout);
    let stderr = read(output.stderr);
    let status = execution.wait();
    execution.reap();
    let stdout = stdout.join().unwrap();
    let stderr = stderr.join().unwrap();
    assert!(
        matches!(
            status,
            Ok(backend::ExecutionOutcome::WindowsExit { code: 0 })
        ),
        "{status:?}\n{stdout}\n{stderr}\n{context}"
    );
    format!("{stdout}\n{stderr}")
}
fn powershell(script: &str) -> String {
    use base64::Engine;
    let bytes = script
        .encode_utf16()
        .flat_map(u16::to_le_bytes)
        .collect::<Vec<_>>();
    format!(
        "powershell.exe -NoProfile -EncodedCommand {}",
        base64::engine::general_purpose::STANDARD.encode(bytes)
    )
}

#[test]
fn direct_collection_can_be_read_after_finish_before_owner_cleanup() {
    struct Completed;
    impl backend::Execution for Completed {
        fn release(&mut self) -> io::Result<()> {
            Ok(())
        }
        fn output(&self) -> io::Result<backend::Output> {
            Ok(backend::Output {
                stdout: Box::new(io::empty()),
                stderr: Box::new(io::empty()),
            })
        }
        fn wait(&mut self) -> io::Result<backend::ExecutionOutcome> {
            Ok(backend::ExecutionOutcome::WindowsExit { code: 0 })
        }
        fn reap(&self) {}
    }
    let f = Fixture::new();
    let config = super::windows_instance::config(&f);
    let id = Uuid::now_v7();
    let directory = config.state_dir().join("jobs").join(id.to_string());
    fs::create_dir_all(&directory).unwrap();
    let archive = directory.join("windows-changes.tar");
    fs::write(&archive, b"collected link records").unwrap();
    let mut execution = super::windows_instance::direct_execution(&config, id, Box::new(Completed));
    finish_execution(execution.as_mut(), "");
    assert_eq!(fs::read(&archive).unwrap(), b"collected link records");
    drop(execution);
    assert!(!directory.exists());
}
fn cancel(runner: &Runner, setup: &Setup, hard: bool) {
    let mut job = Job::new(setup.id, Some(setup.request.clone()), 0, 0, 4);
    job.state = JobState::Running;
    job.process = Some(platform::ProcessIdentity {
        pid: 0,
        start: 0,
        unit: Some(setup.id),
    });
    runner
        .core
        .backend
        .cancel(runner.config(), &job, hard)
        .unwrap();
}
fn no_units(runner: &Runner) {
    let pattern = runner.core.backend.unit_pattern(runner.config());
    let output = backend::systemctl(
        runner.config(),
        &["list-units", "--all", "--no-legend", "--plain", &pattern],
    )
    .unwrap();
    assert!(output.status.success());
    assert!(
        output.stdout.is_empty(),
        "{}",
        String::from_utf8_lossy(&output.stdout)
    );
}

/// The standard job user must execute the machine-wide tools but cannot modify
/// PowerShell. A long checkout must build through the short cache mount.
#[test]
#[ignore = "trusted operator: sealed Windows baseline, dedicated accounts, KVM and systemd required"]
fn windows_qualification_baseline_machine_wide_toolchain_and_msvc_build() {
    let runner = operator();
    let mut f = Fixture::new();
    f.key = format!(
        "/windows-qualification/{}/main",
        ["long_checkout_component_01234567890123456789012345"; 4].join("/")
    );
    source(&f);
    let probe = powershell(&baseline_environment_script());
    let output = run(
        &runner,
        &f,
        &format!("set -e; rustc -vV; cargo --version; git --version; pwsh -NoProfile -Command '$PSVersionTable.PSVersion.ToString()'; {probe}; cargo run --offline; test -f target/debug/motor-qualification.exe"),
    );
    assert!(
        output.lines().any(|line| line.trim() == "7.6.7"),
        "{output}"
    );
    assert!(
        output.contains("PowerShell installation is read-only"),
        "{output}"
    );
    assert!(
        output.contains("short target cache is writable"),
        "{output}"
    );
    assert!(output.contains("release: 1.99.0"));
    assert!(output.contains("host: x86_64-pc-windows-msvc"));
    assert!(output.contains("MSVC binary ran"));
    no_units(&runner);
}

fn baseline_environment_script() -> String {
    r#"
$ErrorActionPreference='Stop'
$identity=[Security.Principal.WindowsIdentity]::GetCurrent()
if($identity.Name.Split('\')[-1] -cne 'motor-job'){throw "unexpected job user: $($identity.Name)"}
if(([Security.Principal.WindowsPrincipal]::new($identity)).IsInRole([Security.Principal.WindowsBuiltInRole]::Administrator)){throw 'job user is an administrator'}
$install='C:\Program Files\PowerShell\7'
if((Get-Command pwsh.exe -ErrorAction Stop).Source -cne "$install\pwsh.exe"){throw 'pwsh resolves outside the protected installation'}
$directories=@((Get-Item -LiteralPath $install)) + @(Get-ChildItem -LiteralPath $install -Directory -Recurse -Force -ErrorAction Stop)
foreach($directory in $directories) {
 $probe=Join-Path $directory.FullName ('motor-write-probe-'+[Guid]::NewGuid().ToString())
 $denied=$false
 try {$file=[IO.File]::Open($probe,[IO.FileMode]::CreateNew,[IO.FileAccess]::Write);$file.Dispose()} catch [UnauthorizedAccessException] {$denied=$true}
 if(-not $denied){Remove-Item -LiteralPath $probe;throw "PowerShell directory is writable: $($directory.FullName)"}
}
foreach($file in Get-ChildItem -LiteralPath $install -File -Recurse -Force -ErrorAction Stop) {
 $denied=$false
 try {$handle=[IO.File]::Open($file.FullName,[IO.FileMode]::Open,[IO.FileAccess]::Write,[IO.FileShare]::ReadWrite);$handle.Dispose()} catch [UnauthorizedAccessException] {$denied=$true}
 if(-not $denied){throw "PowerShell file is writable: $($file.FullName)"}
}
Write-Output 'PowerShell installation is read-only'
if($env:CARGO_TARGET_DIR -cne 'C:\t'){throw "unexpected target dir: $env:CARGO_TARGET_DIR"}
$longOutput=Join-Path (Get-Location) 'target\debug\deps\motor_qualification-0123456789abcdef.exe'
if($longOutput.Length -lt 260){throw "checkout fixture is not long enough: $longOutput"}
$probe=Join-Path $env:CARGO_TARGET_DIR ('motor-cache-probe-'+[Guid]::NewGuid().ToString())
[IO.File]::WriteAllText($probe,'same cache')
$checkoutProbe=Join-Path (Join-Path (Get-Location) 'target') ([IO.Path]::GetFileName($probe))
if([IO.File]::ReadAllText($checkoutProbe) -cne 'same cache'){throw 'checkout target does not reach the short cache mount'}
Remove-Item -LiteralPath $probe
Write-Output 'short target cache is writable'
"#.into()
}

/// Standard-user jobs need Windows' executable extensions, writable user folders,
/// and native command lookup. Removing the Windows base block or the provisioned
/// PowerShell machine PATH entry must make this test fail.
#[tokio::test]
#[ignore = "trusted operator: sealed Windows baseline, dedicated accounts, KVM and systemd required"]
async fn windows_qualification_standard_user_environment() {
    let runner = operator();
    let f = Fixture::new();
    source(&f);
    fs::write(
        f.main.join("standard-user-environment.ps1"),
        standard_user_environment_script(),
    )
    .unwrap();
    fs::write(
        f.main.join("standard-user-command-resolution.ps1"),
        super::windows_environment::command_resolution_script(),
    )
    .unwrap();
    git(&f.main, &["add", "."]);
    git(&f.main, &["commit", "-m", "qualification user environment"]);
    let (mut request, bytes) = f.run(
        r#"set -e
cmd //c "echo ok"
pwsh -NoProfile -Command "git --version"
powershell.exe -NoLogo -NoProfile -NonInteractive -File ./standard-user-environment.ps1 -JobPathExt "$PATHEXT"
"#,
    );
    request.platform = Some(Platform::Windows);
    request.env.insert("RUST_TEST_THREADS".into(), "2".into());
    request.env.insert("CI".into(), "request-marker".into());
    f.submit(&runner, &request, &bytes);
    let terminal = operator_terminal(&runner, request.job_id).await;
    let output = replay_output(&runner, request.job_id);
    assert_eq!(
        terminal.outcome,
        Outcome::WindowsExit { code: 0 },
        "{output}"
    );
    assert!(output.lines().any(|line| line.trim() == "ok"), "{output}");
    assert!(output.contains("git version "), "{output}");
    assert!(
        output.contains("normal Windows user environment passed"),
        "{output}"
    );
    no_units(&runner);
}

fn standard_user_environment_script() -> String {
    r#"
param([Parameter(Mandatory=$true)][string]$JobPathExt)
$ErrorActionPreference='Stop'
$identity=[Security.Principal.WindowsIdentity]::GetCurrent()
if($identity.Name.Split('\')[-1] -cne 'motor-job'){throw "unexpected job user: $($identity.Name)"}
if(([Security.Principal.WindowsPrincipal]::new($identity)).IsInRole([Security.Principal.WindowsBuiltInRole]::Administrator)){throw 'job user is an administrator'}
# PowerShell appends .CPL to its own PATHEXT; compare the value captured by Bash.
$expected='.COM;.EXE;.BAT;.CMD;.VBS;.VBE;.JS;.JSE;.WSF;.WSH;.MSC'
$machinePathExt=[Environment]::GetEnvironmentVariable('PATHEXT','Machine')
if($machinePathExt -cne $expected){throw "unexpected machine PATHEXT: $machinePathExt"}
if($JobPathExt -cne $machinePathExt){throw "job PATHEXT differs from the machine: $JobPathExt"}
$install='C:\Program Files\PowerShell\7'
$machinePaths=[Environment]::GetEnvironmentVariable('Path','Machine') -split ';'
if(-not ($machinePaths | Where-Object {$_.Trim().Trim('"').TrimEnd('\') -ieq $install})){throw 'PowerShell 7 is absent from machine PATH'}
$found=@(& where.exe pwsh)
if($LASTEXITCODE -ne 0 -or $found -notcontains "$install\pwsh.exe"){throw "where.exe pwsh failed: $found"}
foreach($name in @('LOCALAPPDATA','APPDATA','TEMP','TMP')) {
 $directory=[Environment]::GetEnvironmentVariable($name,'Process')
 if([string]::IsNullOrWhiteSpace($directory)){throw "$name is unset"}
 if($directory -notmatch '^(?:[A-Za-z]:\\|\\\\)'){throw "$name is not a native Windows path: $directory"}
 if(-not (Test-Path -LiteralPath $directory -PathType Container)){throw "$name directory is missing: $directory"}
 $probe=Join-Path $directory ('motor-env-probe-'+[Guid]::NewGuid().ToString('N'))
 $stream=[IO.File]::Open($probe,[IO.FileMode]::CreateNew,[IO.FileAccess]::Write,[IO.FileShare]::None)
 try {$stream.WriteByte(7)} finally {$stream.Dispose();[IO.File]::Delete($probe)}
 Write-Output "$name is writable: $directory"
}
if($env:RUST_TEST_THREADS -cne '2'){throw "request thread override was lost: $env:RUST_TEST_THREADS"}
if($env:CI -cne 'request-marker'){throw 'request CI marker was lost'}
& pwsh -NoProfile -NonInteractive -File (Join-Path $PSScriptRoot 'standard-user-command-resolution.ps1')
if($LASTEXITCODE -ne 7){throw "pwsh could not resolve cmd or preserve exit 7: $LASTEXITCODE"}
Write-Output 'normal Windows user environment passed'
exit 0
"#.into()
}

#[test]
#[ignore = "trusted operator: sealed Windows baseline, dedicated accounts, KVM and systemd required"]
fn windows_qualification_symlinks_do_not_escape_collection_or_cache() {
    let runner = operator();
    let f = Fixture::new();
    source(&f);
    fs::write(
        f.main.join("symlink-fixture.ps1"),
        symlink_qualification_script(),
    )
    .unwrap();
    git(&f.main, &["add", "."]);
    git(&f.main, &["commit", "-m", "symlink qualification fixture"]);
    let command = "powershell.exe -NoProfile -NonInteractive -File ./symlink-fixture.ps1";
    let (setup, mut job) = launch(&runner, &f, command);
    // DirectExecution removes collection-only job directories on drop. Retain
    // its owner until the archive assertions have consumed the result.
    assert!(finish_execution(job.execution.as_mut(), "").contains("symlink creation passed"));
    let job_dir = runner
        .config()
        .state_dir()
        .join("jobs")
        .join(setup.id.to_string());
    let bytes = fs::read(job_dir.join("windows-changes.tar")).unwrap();
    let prefix = format!("m{}/", setup.request.workspace_key);
    let targets = [
        ("link-file-system", r"C:\Windows\System32\kernel32.dll"),
        ("link-dir-system", r"C:\Windows\System32"),
        ("link-file-agent", r"C:\ProgramData\Motor\job-password.txt"),
        ("link-dir-agent", r"C:\ProgramData\Motor"),
        ("link-unc", r"\\motor-symlink.invalid\share\absent"),
    ];
    let mut found = BTreeMap::new();
    let mut canary_collected = false;
    for entry in tar::Archive::new(&bytes[..]).entries().unwrap() {
        let mut entry = entry.unwrap();
        let name = entry.path().unwrap().to_str().unwrap().to_owned();
        assert!(
            name.starts_with(&prefix),
            "outside workspace member: {name}"
        );
        let name = name.strip_prefix(&prefix).unwrap().to_owned();
        let mut payload = Vec::new();
        entry.read_to_end(&mut payload).unwrap();
        if name == "symlink-collected.txt" {
            canary_collected = true;
            assert!(entry.header().entry_type().is_file());
            assert_eq!(payload, b"ordinary collection canary");
        } else {
            assert!(entry.header().entry_type().is_symlink(), "followed {name}");
            assert!(payload.is_empty(), "target contents returned for {name}");
            let target = String::from_utf8(entry.link_name_bytes().unwrap().into_owned()).unwrap();
            found.insert(name, target);
        }
    }
    assert!(
        canary_collected,
        "ordinary collection canary was not returned"
    );
    assert_eq!(
        found,
        targets
            .into_iter()
            .map(|(name, target)| (name.to_owned(), target.to_owned()))
            .collect()
    );
    assert!(
        !job_dir.join("m").exists(),
        "collection created a Linux source tree"
    );
    let disk = runner
        .config()
        .windows
        .as_ref()
        .unwrap()
        .caches()
        .disk(&setup.request.workspace_key);
    assert!(disk.is_file(), "first job did not retain its clean cache");
    drop(job);
    let output = run(&runner, &f, &format!("{command} -Check"));
    assert!(
        output.contains("only explained cache links persisted"),
        "{output}"
    );
    assert!(disk.is_file());
    no_units(&runner);
}

fn symlink_qualification_script() -> String {
    r#"
param([switch]$Check)
$ErrorActionPreference='Stop'
$identity=[Security.Principal.WindowsIdentity]::GetCurrent()
if($identity.Name.Split('\')[-1] -cne 'motor-job'){throw 'unexpected job account'}
if(([Security.Principal.WindowsPrincipal]::new($identity)).IsInRole([Security.Principal.WindowsBuiltInRole]::Administrator)){throw 'job is an administrator'}
$mode=(Get-ItemProperty -LiteralPath 'HKLM:\SOFTWARE\Microsoft\Windows\CurrentVersion\AppModelUnlock' -ErrorAction SilentlyContinue).AllowDevelopmentWithoutDevLicense
if($mode -eq 1){throw 'Developer Mode is enabled'}
$links=@(
 @('link-file-system','C:\Windows\System32\kernel32.dll',0),
 @('link-dir-system','C:\Windows\System32',1),
 @('link-file-agent','C:\ProgramData\Motor\job-password.txt',0),
 @('link-dir-agent','C:\ProgramData\Motor',1),
 @('link-unc','\\motor-symlink.invalid\share\absent',0)
)
$cache='C:\t\motor-symlink-cache'
$outside=Join-Path $env:TEMP 'motor-symlink-outside'
if($Check) {
 if(Test-Path -LiteralPath $outside){throw 'outside overlay state survived'}
 $workspaceNames=@(Get-ChildItem -LiteralPath (Get-Location) -Force | ForEach-Object {$_.Name})
 foreach($link in $links){if($workspaceNames -contains $link[0]){throw 'workspace link survived'}}
 if([IO.File]::ReadAllText((Join-Path $cache 'marker')) -cne 'expected cache contents'){throw 'cache was not reused'}
 $expected=@('marker') + @($links | ForEach-Object {$_[0]})
 $actual=@(Get-ChildItem -LiteralPath $cache -Force | ForEach-Object {$_.Name})
 if(Compare-Object ($expected | Sort-Object) ($actual | Sort-Object)){throw 'unexplained cache contents'}
 $mismatches=@()
 foreach($link in $links) {
  $item=Get-Item -LiteralPath (Join-Path $cache $link[0]) -Force
  $actual=@($item.Target)
  if(-not ($item.Attributes -band [IO.FileAttributes]::ReparsePoint) -or $actual.Count -ne 1 -or -not (Test-MotorLinkTarget $actual[0] $link[1])) {
   $mismatches += "cache link mismatch: name=<$($link[0])> expected=<$($link[1])> actual=<$($actual -join '|')> attributes=<$($item.Attributes)>"
  }
 }
 foreach($message in $mismatches){Write-Output $message}
 if($mismatches.Count -ne 0){throw "cache link targets changed: $($mismatches.Count) mismatches"}
 $rootNames=@(Get-ChildItem -LiteralPath 'C:\t' -Force | ForEach-Object {$_.Name})
 foreach($name in $rootNames){if($name -notin @('motor-symlink-cache','System Volume Information','$RECYCLE.BIN')){throw "unexplained cache root entry: $name"}}
 Write-Output 'only explained cache links persisted'
 exit 0
}
Add-Type @'
using System;
using System.Runtime.InteropServices;
public static class MotorSymlink {
 [DllImport("kernel32.dll", CharSet=CharSet.Unicode, SetLastError=true)]
 [return: MarshalAs(UnmanagedType.U1)]
 public static extern bool CreateSymbolicLink(string link, string target, uint flags);
}
'@
New-Item -ItemType Directory -Path $cache,$outside | Out-Null
foreach($root in @((Get-Location).Path,$outside,$cache)) {
 foreach($link in $links) {
  if(-not [MotorSymlink]::CreateSymbolicLink((Join-Path $root $link[0]),$link[1],[uint32]$link[2])){throw "CreateSymbolicLink failed: $([Runtime.InteropServices.Marshal]::GetLastWin32Error())"}
 }
}
[IO.File]::WriteAllText((Join-Path $cache 'marker'),'expected cache contents')
[IO.File]::WriteAllText((Join-Path (Get-Location) 'symlink-collected.txt'),'ordinary collection canary')
Write-Output 'symlink creation passed'
"#.replacen("param([switch]$Check)", &format!("param([switch]$Check)\n{}", include_str!("windows_link_target.ps1")), 1)
}

/// Break check: add a NIC to the job VM. Successful external DNS, TCP, UDP or
/// IPv6 routing makes this check fail; guest loopback must remain usable.
#[test]
#[ignore = "trusted operator: sealed Windows baseline, dedicated accounts, KVM and systemd required"]
fn windows_qualification_no_network_but_loopback_works() {
    let runner = operator();
    let f = Fixture::new();
    source(&f);
    run(
        &runner,
        &f,
        &powershell(
            r#"
$ErrorActionPreference='Stop'
$paths=$env:PATH.Split(';') | ForEach-Object {$_.TrimEnd('\')}
foreach($required in @('C:\Windows\System32','C:\Windows\System32\WindowsPowerShell\v1.0')) {
 if($paths -notcontains $required){throw "missing Windows tools directory in PATH: $required; PATH=$env:PATH"}
}
$null=Get-Command powershell.exe,cmd.exe,reg.exe -ErrorAction Stop
$listener=[Net.Sockets.TcpListener]::new([Net.IPAddress]::Loopback,0);$listener.Start()
$c=[Net.Sockets.TcpClient]::new();$c.Connect('127.0.0.1',$listener.LocalEndpoint.Port);$c.Close();$listener.Stop()
$resolved=$false;try {Resolve-DnsName one.one.one.one -Server 1.1.1.1 -DnsOnly -NoHostsFile -QuickTimeout -ErrorAction Stop | Out-Null;$resolved=$true} catch {}
if($resolved){throw 'external DNS succeeded'}
foreach($address in @('1.1.1.1','2606:4700:4700::1111')) {
 $family=if($address.Contains(':')){[Net.Sockets.AddressFamily]::InterNetworkV6}else{[Net.Sockets.AddressFamily]::InterNetwork}
 $tcp=[Net.Sockets.Socket]::new($family,[Net.Sockets.SocketType]::Stream,[Net.Sockets.ProtocolType]::Tcp)
 try {$pending=$tcp.BeginConnect($address,443,$null,$null);if($pending.AsyncWaitHandle.WaitOne(1000)){$tcp.EndConnect($pending);throw 'external TCP succeeded'}} catch {if($_.Exception.Message -eq 'external TCP succeeded'){throw}} finally {$tcp.Dispose()}
 $udp=[Net.Sockets.Socket]::new($family,[Net.Sockets.SocketType]::Dgram,[Net.Sockets.ProtocolType]::Udp)
 $sent=$false;try {$udp.SendTo([byte[]]@(1),[Net.IPEndPoint]::new([Net.IPAddress]::Parse($address),53))|Out-Null;$sent=$true} catch {} finally {$udp.Dispose()}
 if($sent){throw 'external UDP routed'}
}
Write-Output 'network qualification passed'
"#,
        ),
    );
    no_units(&runner);
}

/// Break check: run QEMU as the runner account. Query the live unit and verify
/// motor-qemu can read only its baseline/socket, not state or Linux checkouts.
#[test]
#[ignore = "trusted operator: sealed Windows baseline, dedicated accounts, KVM and systemd required"]
fn windows_qualification_qemu_and_preparation_accounts_are_confined() {
    let runner = operator();
    let f = Fixture::new();
    source(&f);
    let (setup, mut launch) = launch(&runner, &f, "sleep 300");
    let unit = runner.core.backend.unit_name(runner.config(), setup.id);
    let properties = backend::systemctl(
        runner.config(),
        &["show", "--property=User", "--value", &unit],
    )
    .unwrap();
    assert!(properties.status.success());
    assert_eq!(
        String::from_utf8_lossy(&properties.stdout).trim(),
        "motor-qemu"
    );
    let vm = runner.config().windows.as_ref().unwrap();
    for (account, path, allowed) in [
        ("motor-qemu", vm.baseline_path(), true),
        ("motor-qemu", runner.config().state_dir(), false),
        (
            "motor-prep",
            runner.config().state_dir().join("jobs"),
            false,
        ),
    ] {
        assert!(path.exists(), "access probe target is missing: {path:?}");
        // Open the target rather than using access(2), which can disagree with
        // the kernel's ACL enforcement. Denied directory probes must fail at
        // open, not at a subsequent read that would reject any directory.
        let probe = r#"import os, sys
try:
    fd = os.open(sys.argv[1], os.O_RDONLY)
except PermissionError:
    sys.exit(13)
if sys.argv[2] == 'read':
    sys.stdout.buffer.write(os.read(fd, 1))
os.close(fd)
"#;
        let output = std::process::Command::new("sudo")
            .args(["-n", "-u", account, "/usr/bin/python3", "-c", probe])
            .arg(&path)
            .arg(if allowed { "read" } else { "open" })
            .output()
            .unwrap();
        assert_eq!(
            output.status.code(),
            Some(if allowed { 0 } else { 13 }),
            "{account} access to {path:?}: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        if allowed {
            assert_eq!(output.stdout, b"Q", "baseline was not actually read");
        }
    }
    cancel(&runner, &setup, true);
    assert!(launch.execution.wait().is_err());
    launch.execution.reap();
    no_units(&runner);
}

/// Break check: remove flock from qemu_args. Holding the shared slot file must
/// refuse materialization with exactly windows_slot_busy and load no job unit.
#[test]
#[ignore = "trusted operator: sealed Windows baseline, dedicated accounts, KVM and systemd required"]
fn windows_qualification_one_vm_slot_lock_refuses_without_boot() {
    use fs2::FileExt;
    let runner = operator();
    let f = Fixture::new();
    source(&f);
    let vm = runner.config().windows.as_ref().unwrap();
    let lock = fs::File::open(vm.layout_root.parent().unwrap().join("slot.lock")).unwrap();
    lock.lock_exclusive().unwrap();
    let (mut request, _) = f.run("true");
    request.platform = Some(Platform::Windows);
    for (repo, local) in [
        (&request.repository_root, &f.main),
        (&request.siblings[0].canonical_path, &f.sibling),
    ] {
        let mirror = runner.core.trees.ensure_mirror(repo).unwrap();
        git(&mirror, &["fetch", text(local), "HEAD"]);
    }
    let setup = Setup {
        id: request.job_id,
        request,
        cpus: vec![12, 13, 14, 15],
        base: false,
        recorded: BTreeMap::new(),
        snapshots: None,
    };
    let error = WindowsBackend
        .materialize(runner.config(), &runner.core.trees, &setup)
        .err()
        .expect("VM booted while slot was held");
    assert_eq!(error.to_string(), "windows_slot_busy");
    no_units(&runner);
}

/// Break check: share a cache disk or keep it after hard cancel.
/// This fails if a hard cancel leaves the first job's cache disk behind, or if a
/// later job for another workspace can read its marker. The test checks that the
/// disk is gone before starting the second workspace's job.
#[test]
#[ignore = "trusted operator: sealed Windows baseline, dedicated accounts, KVM and systemd required"]
fn windows_qualification_cache_isolation_and_hard_cancel_discards_disk() {
    let runner = operator();
    let f = Fixture::new();
    source(&f);
    let (setup, mut launch) = launch(
        &runner,
        &f,
        "set -e; printf 'cache canary' > target/motor-canary; printf 'cache canary ready\\n'; sleep 300",
    );
    let mut output = launch.execution.output().unwrap();
    let stderr = std::thread::spawn(move || {
        let mut text = String::new();
        output.stderr.read_to_string(&mut text).unwrap();
        text
    });
    let (tx, rx) = std::sync::mpsc::sync_channel(1);
    std::thread::spawn(move || {
        let mut reader = std::io::BufReader::new(&mut output.stdout);
        let mut line = String::new();
        let result = std::io::BufRead::read_line(&mut reader, &mut line).map(|_| line);
        let _ = tx.send(result);
    });
    let line = rx
        .recv_timeout(Duration::from_secs(60))
        .expect("cache canary was not written")
        .unwrap();
    let disk = runner
        .config()
        .windows
        .as_ref()
        .unwrap()
        .caches()
        .disk(&setup.request.workspace_key);
    assert!(disk.exists());
    cancel(&runner, &setup, true);
    let status = launch.execution.wait();
    launch.execution.reap();
    let stderr = stderr.join().unwrap();
    assert!(
        line.contains("cache canary ready"),
        "first stdout line: {line:?}\nstderr:\n{stderr}"
    );
    assert!(status.is_err(), "{status:?}\nstderr:\n{stderr}");
    assert!(!disk.exists(), "hard cancellation retained {disk:?}");
    run(&runner, &f, "test ! -e target/motor-canary");
    let other = Fixture::new();
    source(&other);
    run(&runner, &other, "test ! -e target/motor-canary");
    no_units(&runner);
}

/// Break check: terminate only Git Bash instead of its entire Job Object.
/// This fails if cancellation kills Git Bash but leaves its detached PowerShell
/// child running. That child holds a file open on the cache disk, so the guest
/// cannot unmount the disk cleanly until cancellation also kills the child.
#[test]
#[ignore = "trusted operator: sealed Windows baseline, dedicated accounts, KVM and systemd required"]
fn windows_qualification_cancel_kills_detached_grandchild() {
    let runner = operator();
    let f = Fixture::new();
    source(&f);
    let script = r#"$ErrorActionPreference='Stop';$child='$f=[IO.File]::Open(''target\held'',[IO.FileMode]::OpenOrCreate,[IO.FileAccess]::ReadWrite,[IO.FileShare]::ReadWrite);Write-Output ready;Start-Sleep 300';$encoded=[Convert]::ToBase64String([Text.Encoding]::Unicode.GetBytes($child));Start-Process powershell.exe -ArgumentList @('-NoProfile','-EncodedCommand',$encoded) -NoNewWindow;Start-Sleep 300"#;
    let (setup, mut launch) = launch(&runner, &f, &powershell(script));
    let mut output = launch.execution.output().unwrap();
    let (tx, rx) = std::sync::mpsc::sync_channel(1);
    std::thread::spawn(move || {
        let mut reader = std::io::BufReader::new(&mut output.stdout);
        let mut line = String::new();
        let result = std::io::BufRead::read_line(&mut reader, &mut line).map(|_| line);
        let _ = tx.send(result);
    });
    let line = rx
        .recv_timeout(Duration::from_secs(60))
        .expect("detached child did not open cache handle")
        .unwrap();
    assert!(
        line.contains("ready"),
        "detached child did not report readiness; first stdout line: {line:?}"
    );
    cancel(&runner, &setup, false);
    assert!(matches!(
        launch.execution.wait(),
        Ok(backend::ExecutionOutcome::WindowsExit { .. })
    ));
    launch.execution.reap();
    no_units(&runner);
}

// Probe partition nodes, not just the NBD disk: they can appear after connection
// returns. This fragment is also exercised without privileged disk access below.
const OFFLINE_OS_PARTITION_SCRIPT: &str = r#"
partprobe "$device"
udevadm settle --timeout=30
shopt -s nullglob
partitions=()
for ((attempt = 0; attempt < 30; attempt++)); do
    partitions=("${device}"p*)
    (( ${#partitions[@]} > 0 )) && break
    sleep 1
done
os_partition=
candidates=
if (( ${#partitions[@]} == 0 )); then
    candidates="(none; timed out waiting for ${device}p* partition nodes)"$'\n'
fi
# Windows also has a recovery NTFS partition. Inspect read-only before selecting
# the OS volume, rather than assuming its partition number on every baseline.
for partition in "${partitions[@]}"; do
    # Probe each device directly; udev's cached FSTYPE can still be empty.
    filesystem=$(blkid -o value -s TYPE "$partition") || filesystem=
    candidates+="$partition ${filesystem:-unknown}"$'\n'
    [[ "$filesystem" == ntfs ]] || continue
    mount -t ntfs3 -o ro,nosuid,nodev,noexec "$partition" "$mountpoint"
    mounted=1
    if [[ -f "$mountpoint/Windows/System32/config/SOFTWARE" &&
          -d "$mountpoint/ProgramData" ]]; then
        os_partition=$partition
    fi
    umount "$mountpoint"
    mounted=0
    [[ -z "$os_partition" ]] || break
done
[[ -n "$os_partition" ]] || {
    printf 'Windows OS NTFS volume not found; probed candidates:\n%s' "$candidates" >&2
    exit 1
}
"#;

// Keep all privileged disk access in one child with an EXIT trap. The hook must
// return with no mount or NBD attachment before the backend can boot the VM,
// including when planting or hive editing fails. No administrator login or saved
// guest credential is needed: discarding an overlay removes every writer's state.
fn plant_overlay_canaries(overlay: &Path, marker: &str) -> std::io::Result<()> {
    let script = [
        r#"
set -euo pipefail
overlay=$1
marker=$2
mountpoint="${overlay}.qualification-ntfs"
device=
connected=0
mounted=0
cleanup() {
    local status=$?
    trap - EXIT
    if (( mounted )); then
        umount "$mountpoint" || status=1
    fi
    if (( connected )); then
        qemu-nbd --disconnect "$device" || status=1
    fi
    rmdir "$mountpoint" || status=1
    exit "$status"
}
mkdir -m 700 "$mountpoint"
trap cleanup EXIT
trap 'exit 1' HUP INT TERM
modprobe nbd max_part=16
[[ $(< /sys/module/nbd/parameters/max_part) -gt 0 ]] || {
    echo 'NBD needs partition support; load the unused module with max_part=16' >&2
    exit 1
}
# A lock prevents concurrent qualification helpers from choosing the same free
# device. A device already attached by another host user is never disconnected.
for candidate in /sys/class/block/nbd*; do
    [[ -d "$candidate" ]] || continue
    exec 9>"/run/lock/motor-qualification-${candidate##*/}.lock"
    flock -n 9 || continue
    [[ ! -e "$candidate/pid" ]] || continue
    device="/dev/${candidate##*/}"
    break
done
[[ -n "$device" ]] || { echo 'no free NBD device' >&2; exit 1; }
# qcow2 writes go only to the newly created overlay; its backing file stays sealed.
qemu-nbd --format=qcow2 --connect="$device" "$overlay"
connected=1
"#,
        OFFLINE_OS_PARTITION_SCRIPT,
        r#"
mount -t ntfs3 -o rw,nosuid,nodev,noexec,windows_names "$os_partition" "$mountpoint"
mounted=1
# A never-logged-on job account has no profile in the sealed baseline. Plant in
# its conventional profile location and other inherited user-readable folders.
# Offline-owned Windows Temp files are not readable by the job user.
mkdir -p "$mountpoint/ProgramData/$marker" "$mountpoint/Users/Public" "$mountpoint/Users/motor-job"
printf '%s' "$marker" > "$mountpoint/ProgramData/$marker/canary"
for directory in Users/Public Users/motor-job; do
    printf '%s' "$marker" > "$mountpoint/$directory/$marker"
done
# Use a harmless HKLM key, not an autostart entry, to check hive persistence
# without changing what Windows runs when job A boots.
hivexregedit --merge --encoding ASCII --prefix 'HKEY_LOCAL_MACHINE\SOFTWARE' \
    "$mountpoint/Windows/System32/config/SOFTWARE" <<REG
Windows Registry Editor Version 5.00

[HKEY_LOCAL_MACHINE\\SOFTWARE\\${marker}]
"Canary"="$marker"
REG
sync -f "$mountpoint"
"#,
    ]
    .concat();
    let output = std::process::Command::new("/usr/bin/sudo")
        .args(["-n", "/bin/bash", "-c", &script, "motor-offline-canaries"])
        .arg(overlay)
        .arg(marker)
        .env_clear()
        .env("PATH", "/usr/sbin:/usr/bin:/sbin:/bin")
        .output()?;
    if !output.status.success() {
        return Err(std::io::Error::other(format!(
            "offline overlay planting or cleanup failed: {}\n{}\n{}",
            output.status,
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        )));
    }
    Ok(())
}

/// Break check: retain job A's overlay and reuse it for job B instead of creating
/// a fresh overlay from the sealed baseline. The test must fail on retained
/// overlays or on job B's persisted canaries.
/// Job A reads the exact host-planted contents in inherited user-readable folders
/// (ProgramData, Users\Public and the motor-job profile) and HKLM SOFTWARE. It
/// also writes and reads its own Windows Temp file and HKCU value: another
/// account's offline-owned Temp file need not be readable by a standard user.
/// Job B accepts only not-found results, never access denial or other read errors.
#[test]
#[ignore = "trusted operator: sealed Windows baseline, sudo -n, qemu-nbd, ntfs3 and hivexregedit required"]
fn windows_qualification_no_persistence_offline_overlay_canaries() {
    let mut config = operator().config().clone();
    let marker = format!("motor-qualification-{}", Uuid::now_v7());
    let overlays = Arc::new(std::sync::Mutex::new(Vec::<PathBuf>::new()));
    let created = overlays.clone();
    let canary = marker.clone();
    let baseline = config.windows.as_ref().unwrap().baseline_path();
    // The existing test-only command hook runs the real backend qemu-img create
    // first, then plants only in its first overlay while Resources is not booted.
    // This avoids a second overlay builder or a new production lifecycle hook.
    config.windows_command = Some(Arc::new(move |program, args| {
        let output = std::process::Command::new(program).args(args).output()?;
        if output.status.success()
            && program == "/usr/bin/qemu-img"
            && args.first().is_some_and(|arg| arg == "create")
            && args.get(5).is_some_and(|arg| arg == "-b")
        {
            if args.len() != 8 || Path::new(&args[6]) != baseline {
                return Err(std::io::Error::other("unexpected job overlay backing file"));
            }
            let overlay = PathBuf::from(&args[7]);
            let mut created = created.lock().unwrap();
            if created.is_empty() {
                plant_overlay_canaries(&overlay, &canary)?;
            }
            created.push(overlay);
        }
        Ok(output)
    }));
    let runner = Runner::open(config).unwrap();
    let f = Fixture::new();
    source(&f);
    let check = overlay_canary_check(&marker, true);
    let (_, job_a) = launch(&runner, &f, &powershell(&check));
    let output = finish_with_context(job_a, &format!("job A rendered PowerShell:\n{check}"));
    assert!(
        output.contains("job A saw all offline and job-owned canaries"),
        "{output}\njob A rendered PowerShell:\n{check}"
    );
    {
        let created = overlays.lock().unwrap();
        assert_eq!(created.len(), 1, "job A did not use the overlay hook");
        assert!(!created[0].exists(), "job A overlay was not discarded");
    }
    let absent = overlay_canary_check(&marker, false);
    let (_, job_b) = launch(&runner, &f, &powershell(&absent));
    let output = finish_with_context(job_b, &format!("job B rendered PowerShell:\n{absent}"));
    assert!(
        output.contains("job B saw no offline or job-owned canaries"),
        "{output}\njob B rendered PowerShell:\n{absent}"
    );
    let created = overlays.lock().unwrap();
    assert_eq!(created.len(), 2, "both jobs must create a baseline overlay");
    assert_ne!(created[0], created[1], "jobs reused an overlay path");
    assert!(created.iter().all(|path| !path.exists()));
    no_units(&runner);
}

// Generate both phases of the overlay-persistence check: the first job verifies
// offline markers and writes its own; a second job on a fresh overlay must find
// all those markers absent. Actual reads keep access failures from passing as
// absence. Unwrap file exceptions and enumerate registry names before declaring
// them absent; a null registry open alone is not an absence diagnosis. Keep the
// encoded invocation compact enough for a conservative command budget.
fn overlay_canary_check(marker: &str, present: bool) -> String {
    let mut script =
        format!("$ErrorActionPreference='Stop';$marker='{marker}';$present=${present};\n");
    script.push_str(
        r#"
$files=@("C:\ProgramData\$marker\canary","C:\Users\Public\$marker","C:\Users\motor-job\$marker")
$tempCanary="C:\Windows\Temp\$marker"
function Assert-FileCanary([string]$path,[bool]$expected){
try{$value=[IO.File]::ReadAllText($path)}catch{
$cause=$_.Exception.GetBaseException()
if(-not $expected -and ($cause -is [IO.FileNotFoundException] -or $cause -is [IO.DirectoryNotFoundException])){return}
throw "read '$path': $cause"
}
if(-not $expected){throw "persisted '$path'"}
if($value -cne $marker){throw "incorrect '$path': '$value'"}
}
function Assert-RegistryCanary([Microsoft.Win32.RegistryKey]$hive,[bool]$expected){
$software=$null;$key=$null;$location="$($hive.Name)\SOFTWARE\$marker"
try{
if($hive.GetSubKeyNames() -notcontains 'SOFTWARE'){
if($expected){throw 'missing SOFTWARE key'}
return
}
$software=$hive.OpenSubKey('SOFTWARE',$false)
if($null -eq $software){throw 'cannot open enumerated SOFTWARE key'}
if($software.GetSubKeyNames() -notcontains $marker){
if($expected){throw 'missing canary key'}
return
}
$key=$software.OpenSubKey($marker,$false)
if($null -eq $key){throw 'cannot open enumerated canary key'}
if(-not $expected){throw 'persisted canary key'}
$value=$key.GetValue('Canary',$null,[Microsoft.Win32.RegistryValueOptions]::DoNotExpandEnvironmentNames)
if($value -isnot [string] -or $value -cne $marker){throw "missing or incorrect value '$value'"}
}catch{throw "registry '$location': $_"}finally{
if($null -ne $key){$key.Dispose()}
if($null -ne $software){$software.Dispose()}
}
}
foreach($path in $files){Assert-FileCanary $path $present}
Assert-RegistryCanary ([Microsoft.Win32.Registry]::LocalMachine) $present
if($present){
try{[IO.File]::WriteAllText($tempCanary,$marker)}catch{throw "write '$tempCanary': $_"}
$key=$null
try{
$key=[Microsoft.Win32.Registry]::CurrentUser.CreateSubKey("SOFTWARE\$marker")
if($null -eq $key){throw 'CreateSubKey returned null'}
$key.SetValue('Canary',$marker,[Microsoft.Win32.RegistryValueKind]::String)
}catch{throw "write 'HKCU\SOFTWARE\$marker': $_"}finally{if($null -ne $key){$key.Dispose()}}
}
Assert-FileCanary $tempCanary $present
Assert-RegistryCanary ([Microsoft.Win32.Registry]::CurrentUser) $present
if($present){Write-Output 'job A saw all offline and job-owned canaries'}
else{Write-Output 'job B saw no offline or job-owned canaries'}
exit 0
"#,
    );
    script
}

/// Keeps the test's runner child process so it can be killed and waited for when
/// the test exits, including after a failed assertion. These helpers do not kill
/// the guest agent or QEMU. For that separate manual test, kill motor-winagent or
/// QEMU after a job starts; it fails if the job does not end OutcomeUnknown or a VM
/// unit survives. Stopping the guest service requires an administrator console.
struct Daemon(std::process::Child);
impl Drop for Daemon {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}
fn daemon(config: &Config) -> Daemon {
    let binary = std::env::var_os("MOTOR_RUNNER_BINARY")
        .expect("set MOTOR_RUNNER_BINARY to the freshly built motor-runner executable");
    assert!(Path::new(&binary).is_absolute());
    Daemon(
        std::process::Command::new(binary)
            .args([
                "serve",
                "--backend",
                "windows-vm",
                "--instance",
                &config.instance,
                "--state-root",
                text(&config.state_root),
                "--vm-config",
                text(config.vm_config.as_ref().unwrap()),
                "--free-space-floor-bytes",
                "0",
            ])
            .stdin(std::process::Stdio::null())
            .spawn()
            .unwrap(),
    )
}
fn peer(config: &Config, child: &mut Daemon) -> std::os::unix::net::UnixStream {
    let began = std::time::Instant::now();
    loop {
        assert!(
            child.0.try_wait().unwrap().is_none(),
            "runner child exited before greeting"
        );
        assert!(
            began.elapsed() < Duration::from_secs(120),
            "runner greeting deadline exceeded"
        );
        if let Ok(mut peer) = std::os::unix::net::UnixStream::connect(config.socket_path()) {
            peer.set_read_timeout(Some(Duration::from_secs(2))).unwrap();
            motor_protocol::write_frame(
                &mut peer,
                &Frame::Hello {
                    return_paths: false,
                    return_chunked: false,
                    protocol_version: PROTOCOL_VERSION,
                    started: true,
                    default_branch_tip: false,
                    gate_jobs: false,
                    platforms: Vec::new(),
                },
            )
            .unwrap();
            if matches!(
                motor_protocol::read_frame(&mut peer),
                Ok(Some(Frame::Hello { .. }))
            ) {
                return peer;
            }
        }
        std::thread::park_timeout(Duration::from_millis(20));
    }
}
fn next(peer: &mut std::os::unix::net::UnixStream) -> Frame {
    loop {
        match motor_protocol::read_frame(peer).unwrap().unwrap() {
            Frame::Heartbeat { .. } => {}
            frame => return frame,
        }
    }
}

/// Break check: stop only units named in saved job records.
/// This fails if a restarted runner stops only VM units named in saved job
/// records: a runner killed during VM boot has not saved the unit's identity yet.
/// The test kills its runner child during boot and again after the Started record
/// is saved, then restarts it and checks that the VM units and job files are gone.
/// The booting job must end lost_to_restart; the started job must end
/// OutcomeUnknown because the runner cannot confirm what the command did.
/// MOTOR_RUNNER_BINARY must name the freshly built binary. A missing binary or
/// required kill phase fails the test instead of silently skipping it.
#[test]
#[ignore = "trusted operator: dedicated Windows instance, KVM, MOTOR_RUNNER_BINARY and MOTOR_WINDOWS_VM_CONFIG required"]
fn windows_qualification_runner_sigkill_reconciles_boot_and_started_jobs() {
    for started in [false, true] {
        let runner = operator();
        let config = runner.config().clone();
        drop(runner);
        let f = Fixture::new();
        source(&f);
        let (mut request, bytes) = f.run("sleep 300");
        request.platform = Some(Platform::Windows);
        let mut child = daemon(&config);
        let mut stream = peer(&config, &mut child);
        motor_protocol::write_frame(&mut stream, &Frame::Run(request.clone())).unwrap();
        assert!(matches!(next(&mut stream), Frame::Accepted(_)));
        for (index, data) in bytes.into_iter().enumerate() {
            motor_protocol::write_frame(
                &mut stream,
                &Frame::BundleChunk {
                    index: index as u32,
                    offset: 0,
                    data: BytePayload(data),
                },
            )
            .unwrap();
        }
        let record = config
            .state_dir()
            .join("jobs")
            .join(request.job_id.to_string())
            .join("record.json");
        let began = std::time::Instant::now();
        loop {
            assert!(
                began.elapsed() < Duration::from_secs(120),
                "required kill phase not reached"
            );
            let job: Job = read_json(&record).unwrap();
            assert!(job.terminal.is_none(), "job ended before kill phase");
            if started && job.started_ms.is_some() {
                break;
            }
            if !started {
                assert!(
                    job.started_ms.is_none(),
                    "boot window missed; rerun rather than qualifying the wrong phase"
                );
                let unit = format!(
                    "motor-windows-{}-{}.service",
                    config.instance, request.job_id
                );
                let state = backend::systemctl(
                    &config,
                    &["show", "--property=ActiveState", "--value", &unit],
                )
                .unwrap();
                if state.status.success()
                    && String::from_utf8_lossy(&state.stdout).trim() == "active"
                {
                    break;
                }
            }
            std::thread::park_timeout(Duration::from_millis(20));
        }
        child.0.kill().unwrap();
        child.0.wait().unwrap();
        drop(stream);
        drop(child);
        let mut restarted = daemon(&config);
        let mut attach = peer(&config, &mut restarted);
        motor_protocol::write_frame(
            &mut attach,
            &Frame::Attach(AttachRequest::new(request.job_id, 0)),
        )
        .unwrap();
        let terminal = loop {
            if let Frame::Terminal(record) = next(&mut attach) {
                break record;
            }
        };
        if started {
            assert_eq!(terminal.outcome, Outcome::OutcomeUnknown);
        } else {
            assert!(matches!(
                terminal.outcome,
                Outcome::RefusedBeforeStart {
                    reason: RefusalReason::LostToRestart
                }
            ));
        }
        let vm = VmConfig::load(config.vm_config.as_ref().unwrap()).unwrap();
        assert!(!vm
            .layout_root
            .join("vm/overlays")
            .join(format!("{}.qcow2", request.job_id))
            .exists());
        assert!(!vm
            .layout_root
            .join("staging")
            .join(request.job_id.to_string())
            .exists());
        assert!(!vm.caches().disk(&request.workspace_key).exists());
        let pattern = format!("motor-windows-{}-*", config.instance);
        let units = backend::systemctl(
            &config,
            &["list-units", "--all", "--no-legend", "--plain", &pattern],
        )
        .unwrap();
        assert!(units.status.success());
        assert!(units.stdout.is_empty());
        drop(restarted);
    }
}

/// Break check: ignore RUST_TEST_THREADS, allow CARGO_NET_OFFLINE=false, or
/// drop the path mapping while persisting Started or forwarding replay frames.
#[tokio::test]
#[ignore = "trusted operator: sealed Windows baseline, dedicated accounts, KVM and systemd required"]
async fn windows_qualification_build_environment_and_status_round_trip() {
    let runner = operator();
    let f = Fixture::new();
    source(&f);
    fs::write(
        f.main.join("src/main.rs"),
        r#"
fn main() { println!("MSVC binary ran"); }
#[test]
fn requested_threads_and_enforced_offline() {
    assert_eq!(std::env::var("RUST_TEST_THREADS").unwrap(), "1");
    assert_eq!(std::env::var("CARGO_NET_OFFLINE").unwrap(), "true");
}
"#,
    )
    .unwrap();
    git(&f.main, &["add", "."]);
    git(&f.main, &["commit", "-m", "qualification environment test"]);
    let (mut request, bytes) = f.run("set -e; cargo test --offline; cargo run --offline; pwd -W");
    request.platform = Some(Platform::Windows);
    request.env.insert("RUST_TEST_THREADS".into(), "1".into());
    request
        .env
        .insert("CARGO_NET_OFFLINE".into(), "false".into());
    f.submit(&runner, &request, &bytes);
    let terminal = operator_terminal(&runner, request.job_id).await;
    assert_eq!(terminal.outcome, Outcome::WindowsExit { code: 0 });
    let replay = runner.replay(request.job_id, 0).unwrap();
    assert!(
        matches!(&replay[0], Frame::Started { path_mapping: Some(mapping), .. }
        if mapping == &PathMapping::new("/", r"C:\m"))
    );
    // Read stdout alone: cargo writes its progress lines to stderr, and joining
    // both streams can split the test harness's "... ok" line in two.
    let output: String = replay
        .iter()
        .filter_map(|frame| match frame {
            Frame::Output(output) if output.stream == OutputStream::Stdout => {
                Some(String::from_utf8_lossy(&output.bytes.0))
            }
            _ => None,
        })
        .collect();
    assert!(
        output.contains("requested_threads_and_enforced_offline ... ok"),
        "{output}"
    );
    assert!(output.contains("MSVC binary ran"), "{output}");
    assert!(output.replace('\\', "/").contains("C:/m/"), "{output}");
    let stored: Job = read_json(&runner.record_path(request.job_id)).unwrap();
    assert_eq!(stored.terminal.as_ref().unwrap(), &terminal);
    assert_eq!(
        stored.started.as_ref().unwrap().path_mapping,
        Some(PathMapping::new("/", r"C:\m"))
    );
    for frame in &replay {
        let mut wire = Vec::new();
        motor_protocol::write_frame(&mut wire, frame).unwrap();
        assert_eq!(
            motor_protocol::read_frame(&mut &wire[..]).unwrap().as_ref(),
            Some(frame)
        );
    }
    no_units(&runner);
}

/// The agent must report GetExitCodeProcess's entire unsigned 32-bit exit status;
/// narrowing to a byte or interpreting it as signed must fail this test.
/// PowerShell terminates the agent's original Git Bash with an exact 32-bit
/// Windows status, including NTSTATUS values such as 0xC0000005. The runner reads
/// the root's exit code from its retained process handle the same way whether the
/// root crashed or was terminated, so these codes prove the full-width round trip.
/// Git Bash's /proc supplies its Windows PID before it forks a native child;
/// PowerShell's immediate parent can be an intermediate shell, not the job root.
#[tokio::test]
#[ignore = "trusted operator: sealed Windows baseline, dedicated accounts, KVM and systemd required"]
async fn windows_qualification_full_width_and_ntstatus_crash_round_trip() {
    let runner = operator();
    let f = Fixture::new();
    source(&f);
    for code in [256_u32, 0x7FFF_FFFF, 0xFFFF_FFFF, 0xC000_0005] {
        let script = format!(
            r#"
$ErrorActionPreference='Stop'
Add-Type -TypeDefinition 'using System; using System.Runtime.InteropServices; public class KillBash {{ [DllImport("kernel32.dll", SetLastError=true)] public static extern IntPtr OpenProcess(uint rights, bool inherit, uint pid); [DllImport("kernel32.dll", SetLastError=true)] public static extern bool TerminateProcess(IntPtr process, uint code); }}'
if($env:MOTOR_QUALIFICATION_BASH_PID -notmatch '^[1-9][0-9]*$') {{ throw 'missing original Git Bash Windows PID' }}
$bash=[uint32]$env:MOTOR_QUALIFICATION_BASH_PID
$handle=[KillBash]::OpenProcess(0x0001,$false,$bash)
if($handle -eq [IntPtr]::Zero) {{ throw 'cannot open original Git Bash' }}
Write-Output "terminating original Git Bash Windows PID $bash with status {code}"
if(![KillBash]::TerminateProcess($handle,[uint32]{code})) {{ throw 'cannot terminate original Git Bash' }}
"#
        );
        let (mut request, bytes) = f.run(&format!(
            "set -e; bash_winpid=$(cat /proc/$$/winpid); export MOTOR_QUALIFICATION_BASH_PID=\"$bash_winpid\"; {}; sleep 300",
            powershell(&script)
        ));
        request.platform = Some(Platform::Windows);
        request.timeout = Some(120);
        f.submit(&runner, &request, &bytes);
        let terminal = operator_terminal(&runner, request.job_id).await;
        assert_eq!(
            terminal.outcome,
            Outcome::WindowsExit { code },
            "{terminal:?}\n{}",
            replay_output(&runner, request.job_id)
        );
        let stored: Job = read_json(&runner.record_path(request.job_id)).unwrap();
        assert_eq!(
            stored.terminal.as_ref().unwrap().outcome,
            Outcome::WindowsExit { code }
        );
        let replay = runner.replay(request.job_id, 0).unwrap();
        assert!(
            matches!(replay.last(), Some(Frame::Terminal(t)) if t.outcome == Outcome::WindowsExit { code })
        );
        let mut wire = Vec::new();
        motor_protocol::write_frame(&mut wire, replay.last().unwrap()).unwrap();
        assert_eq!(
            motor_protocol::read_frame(&mut &wire[..]).unwrap(),
            replay.last().cloned()
        );
        no_units(&runner);
    }
}

async fn operator_terminal(runner: &Runner, id: Uuid) -> TerminalRecord {
    tokio::time::timeout(Duration::from_secs(600), async {
        loop {
            if let Some(terminal) = runner.job_status(id).unwrap().terminal {
                return terminal;
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
    })
    .await
    .expect("Windows qualification job did not finish within ten minutes")
}

/// Break check: remove private git routing to the verified mirror, discard a
/// clean target cache, or let preparation accept a missing lock or outside path.
#[tokio::test]
#[ignore = "trusted operator: sealed Windows baseline, dedicated accounts, KVM and systemd required"]
async fn windows_qualification_representative_build_and_dependency_cache() {
    let runner = operator();
    let f = Fixture::new();
    source(&f);
    fs::create_dir_all(f.sibling.join("src")).unwrap();
    fs::write(
        f.sibling.join("Cargo.toml"),
        "[package]\nname='commons'\nversion='0.1.0'\nedition='2021'\n",
    )
    .unwrap();
    fs::write(
        f.sibling.join("src/lib.rs"),
        "pub fn helper() -> u32 { 42 }\n",
    )
    .unwrap();
    git(&f.sibling, &["add", "."]);
    git(&f.sibling, &["commit", "-m", "qualification sibling"]);
    let dependency = f.scratch.join("private");
    fs::create_dir_all(dependency.join("src")).unwrap();
    git(&dependency, &["init"]);
    git(&dependency, &["config", "user.name", "Motor fixture"]);
    git(
        &dependency,
        &["config", "user.email", "motor-fixture@example.invalid"],
    );
    fs::write(
        dependency.join("Cargo.toml"),
        "[package]\nname='private'\nversion='0.1.0'\nedition='2021'\n",
    )
    .unwrap();
    fs::write(
        dependency.join("src/lib.rs"),
        "pub fn helper() -> u32 { 42 }\n",
    )
    .unwrap();
    git(&dependency, &["add", "."]);
    git(
        &dependency,
        &["commit", "-m", "qualification private dependency"],
    );
    let commit = git(&dependency, &["rev-parse", "HEAD"]);
    let reference = format!("refs/ck-motor/dep/{commit}");
    git(&dependency, &["update-ref", &reference, &commit]);
    let bundle_path = f.scratch.join("private.bundle");
    git(
        &dependency,
        &["bundle", "create", text(&bundle_path), &reference],
    );
    let bundle_bytes = fs::read(bundle_path).unwrap();
    let url = format!("https://unroutable.invalid/{}.git", Uuid::now_v7());
    fs::write(f.main.join("Cargo.toml"), format!("[package]\nname='motor-qualification'\nversion='0.1.0'\nedition='2021'\n[dependencies]\ncommons={{path='../../source/commons'}}\nprivate={{git='{url}',rev='{commit}'}}\nitoa='=1.0.18'\n")).unwrap();
    fs::write(f.main.join("Cargo.lock"), format!("version=3\n[[package]]\nname='commons'\nversion='0.1.0'\n[[package]]\nname='motor-qualification'\nversion='0.1.0'\ndependencies=['commons','private','itoa']\n[[package]]\nname='private'\nversion='0.1.0'\nsource='git+{url}?rev={commit}#{commit}'\n[[package]]\nname='itoa'\nversion='1.0.18'\nsource='registry+https://github.com/rust-lang/crates.io-index'\nchecksum='8f42a60cbdf9a97f5d2305f08a87dc4e09308d1276d28c869c684d7777685682'\n")).unwrap();
    let long_path = format!(
        "src/{}/value.rs",
        ["long_component_012345678901234567890123456789"; 7].join("/")
    );
    fs::create_dir_all(f.main.join(&long_path).parent().unwrap()).unwrap();
    fs::write(f.main.join(&long_path), "pub fn helper() -> u32 { 42 }\n").unwrap();
    fs::write(f.main.join("src/main.rs"), format!(r#"
#[path = "{}"] mod long;
fn main() {{ assert_eq!(commons::helper(), 42); assert_eq!(private::helper(), 42); assert_eq!(long::helper(), 42); println!("{{}}", itoa::Buffer::new().format(42)); }}
#[test] fn dependencies_work() {{ main(); }}
"#, long_path.strip_prefix("src/").unwrap())).unwrap();
    git(&f.main, &["add", "."]);
    git(
        &f.main,
        &["commit", "-m", "representative Windows qualification"],
    );
    for with_bundle in [true, false] {
        let writable = powershell(
            r#"$ErrorActionPreference='Stop';$probe=Join-Path (Get-Location) 'target\motor-write-probe';[IO.File]::WriteAllText($probe,'writable');if([IO.File]::ReadAllText($probe) -cne 'writable'){throw 'target cache write probe failed'};Remove-Item -LiteralPath $probe;Write-Output 'target cache is writable'"#,
        );
        let (mut request, bytes) = f.run(&format!("set -e; {writable}; cargo build --locked --offline -v; cargo test --locked --offline -v; cargo run --locked --offline"));
        request.platform = Some(Platform::Windows);
        request.git_dependencies = vec![GitDependency {
            url: url.clone(),
            commits: vec![commit.clone()],
            bundle: with_bundle.then(|| BundleManifest {
                index: 2,
                repository: url.clone(),
                length: bundle_bytes.len() as u64,
                sha256: format!("{:x}", Sha256::digest(&bundle_bytes)),
            }),
        }];
        submit_dependency(
            &runner,
            &request,
            &bytes,
            with_bundle.then_some(bundle_bytes.as_slice()),
        );
        let terminal = operator_terminal(&runner, request.job_id).await;
        let output = replay_output(&runner, request.job_id);
        assert_eq!(
            terminal.outcome,
            Outcome::WindowsExit { code: 0 },
            "representative build job stdout/stderr:\n{output}"
        );
        assert!(output.contains("target cache is writable"), "{output}");
        assert!(
            !output.contains("motor-winagent: Cache::mount"),
            "service timings leaked into command output: {output}"
        );
        let timing = runner
            .config()
            .windows_runtime
            .cache_timings
            .lock()
            .unwrap()
            .get(&request.job_id)
            .cloned()
            .expect("missing cache mount timing frame");
        eprintln!(
            "representative build cache mount ({}): {timing:?}",
            if with_bundle {
                "fresh disk"
            } else {
                "reused disk"
            }
        );
        assert!(
            timing.disk_selection_ms < 2_000,
            "slow cache disk selection: {timing:?}"
        );
        if !with_bundle {
            assert_eq!(
                timing.powershell_ms, 0,
                "reused cache launched PowerShell: {timing:?}"
            );
            assert_eq!(
                timing.set_disk_ms, 0,
                "reused cache used Set-Disk: {timing:?}"
            );
            assert!(
                timing.elapsed_ms < 3_000,
                "slow reused cache mount: {timing:?}"
            );
            assert!(
                !timing.volume_acl_updated,
                "reused cache repeated its ACL propagation: {timing:?}"
            );
        }
        // The test harness writes its "... ok" line to stdout, while cargo and
        // PowerShell progress go to stderr and can land between its halves in
        // the combined output, so match it in stdout alone.
        let stdout = replay_stdout(&runner, request.job_id);
        assert!(
            stdout.contains("dependencies_work ... ok"),
            "{stdout}\n--- combined:\n{output}"
        );
        if !with_bundle {
            for fresh in ["Fresh itoa v1.0.18", "Fresh private v0.1.0"] {
                assert!(output.contains(fresh), "missing {fresh}: {output}");
            }
            for compiled in ["Compiling itoa", "Compiling private"] {
                assert!(
                    !output.contains(compiled),
                    "unexpected {compiled}: {output}"
                );
            }
            for compiled in ["Compiling commons", "Compiling motor-qualification"] {
                assert!(output.contains(compiled), "missing {compiled}: {output}");
            }
        }
        no_units(&runner);
    }
    let (mut missing, bytes) = f.run("touch must-not-run");
    missing.platform = Some(Platform::Windows);
    missing.git_dependencies = vec![GitDependency {
        url,
        commits: vec!["0000000000000000000000000000000000000001".into()],
        bundle: None,
    }];
    f.submit(&runner, &missing, &bytes);
    assert_operator_refusal(&runner, missing.job_id).await;
    for missing_lock in [true, false] {
        let bad = Fixture::new();
        source(&bad);
        if missing_lock {
            fs::remove_file(bad.main.join("Cargo.lock")).unwrap();
        } else {
            fs::write(bad.main.join("Cargo.toml"), "[package]\nname='motor-qualification'\nversion='0.1.0'\n[dependencies]\noutside={path='../../../../outside'}\n").unwrap();
        }
        git(&bad.main, &["add", "."]);
        git(&bad.main, &["commit", "-m", "qualification refusal"]);
        let (mut request, bytes) = bad.run("touch must-not-run");
        request.platform = Some(Platform::Windows);
        bad.submit(&runner, &request, &bytes);
        assert_operator_refusal(&runner, request.job_id).await;
    }
    no_units(&runner);
}

fn submit_dependency(runner: &Runner, request: &Run, bytes: &[Vec<u8>], dependency: Option<&[u8]>) {
    assert!(matches!(
        runner.accept(request.clone()).unwrap(),
        Admission::Accepted(_)
    ));
    let mut manifests = request.bundles.clone();
    if let Some(bundle) = request.git_dependencies[0].bundle.clone() {
        manifests.push(bundle);
    }
    let mut upload = Upload::new(runner.spool_dir(request.job_id), &manifests).unwrap();
    for (index, data) in bytes.iter().enumerate() {
        upload
            .chunk(Frame::BundleChunk {
                index: index as u32,
                offset: 0,
                data: BytePayload(data.clone()),
            })
            .unwrap();
    }
    if let Some(dependency) = dependency {
        upload
            .chunk(Frame::BundleChunk {
                index: 2,
                offset: 0,
                data: BytePayload(dependency.to_vec()),
            })
            .unwrap();
    }
    runner.fetch_run(request, &upload.directory, true).unwrap();
}
fn replay_stdout(runner: &Runner, id: Uuid) -> String {
    runner
        .replay(id, 0)
        .unwrap()
        .iter()
        .filter_map(|frame| match frame {
            Frame::Output(output) if output.stream == OutputStream::Stdout => {
                Some(String::from_utf8_lossy(&output.bytes.0))
            }
            _ => None,
        })
        .collect()
}
fn replay_output(runner: &Runner, id: Uuid) -> String {
    runner
        .replay(id, 0)
        .unwrap()
        .iter()
        .filter_map(|frame| match frame {
            Frame::Output(output) => Some(String::from_utf8_lossy(&output.bytes.0)),
            _ => None,
        })
        .collect()
}
async fn assert_operator_refusal(runner: &Runner, id: Uuid) {
    let terminal = operator_terminal(runner, id).await;
    assert!(matches!(
        terminal.outcome,
        Outcome::RefusedBeforeStart { .. }
    ));
    assert_eq!(terminal.ran, Some(Ran::None));
    assert!(runner
        .replay(id, 0)
        .unwrap()
        .iter()
        .all(|frame| !matches!(frame, Frame::Started { .. })));
    no_units(runner);
}

// Run the production discovery fragment against delayed synthetic nodes. Shell
// functions replace privileged commands; no disk is attached or mounted here.
fn offline_partition_probe(scenario: &str) -> (std::process::Output, String) {
    let fixture = Fixture::new();
    let mountpoint = fixture.scratch.join("mount");
    fs::create_dir(&mountpoint).unwrap();
    let trace = fixture.scratch.join("probe-trace");
    let script = [
        r#"
set -euo pipefail
device=$1
mountpoint=$2
trace=$3
scenario=$4
mounted=0
probed=0
waits=0
record() { printf '%s\n' "$1" >> "$trace"; }
partprobe() {
    [[ "$#" == 1 && "$1" == "$device" ]]
    probed=1
    record partprobe
    if [[ "$scenario" == no_os ]]; then
        touch "${device}p1" "${device}p2"
    fi
}
udevadm() {
    [[ "$probed" == 1 && "$*" == 'settle --timeout=30' ]]
    record udevadm
}
sleep() {
    [[ "$probed" == 1 && "$*" == 1 ]]
    record sleep
    waits=$((waits + 1))
    if [[ "$scenario" == delayed && "$waits" == 2 ]]; then
        touch "${device}p1" "${device}p2" "${device}p3"
    fi
}
blkid() {
    [[ "$#" == 5 && "$1 $2 $3 $4" == '-o value -s TYPE' && -e "$5" ]]
    record "blkid:${5##*/}"
    if [[ "$scenario" == no_os && "$5" == "${device}p2" ]]; then
        return 2
    fi
    case "$5" in
        "${device}p1") echo vfat ;;
        "${device}p2"|"${device}p3") echo ntfs ;;
        *) return 2 ;;
    esac
}
mount() {
    [[ "$#" == 6 && "$1 $2 $3 $4" == '-t ntfs3 -o ro,nosuid,nodev,noexec' && "$6" == "$mountpoint" ]]
    record "mount:${5##*/}"
    if [[ "$5" == "${device}p3" ]]; then
        mkdir -p "$mountpoint/Windows/System32/config" "$mountpoint/ProgramData"
        touch "$mountpoint/Windows/System32/config/SOFTWARE"
    fi
}
umount() {
    [[ "$#" == 1 && "$1" == "$mountpoint" ]]
    record umount
    rm -rf "$mountpoint/Windows" "$mountpoint/ProgramData"
}
"#,
        OFFLINE_OS_PARTITION_SCRIPT,
        r#"
[[ "$mounted" == 0 ]]
printf 'selected=%s\n' "${os_partition##*/}"
"#,
    ]
    .concat();
    let output = std::process::Command::new("/bin/bash")
        .args(["-c", &script, "motor-partition-probe-test"])
        .arg(fixture.scratch.join("nbd0"))
        .arg(mountpoint)
        .arg(&trace)
        .arg(scenario)
        .output()
        .unwrap();
    (output, fs::read_to_string(trace).unwrap())
}

#[test]
fn offline_partition_probe_waits_for_nodes_and_selects_os_volume() {
    let (output, trace) = offline_partition_probe("delayed");
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert_eq!(String::from_utf8_lossy(&output.stdout), "selected=nbd0p3\n");
    assert_eq!(
        trace,
        "partprobe\nudevadm\nsleep\nsleep\nblkid:nbd0p1\nblkid:nbd0p2\nmount:nbd0p2\numount\nblkid:nbd0p3\nmount:nbd0p3\numount\n"
    );
}

#[test]
fn offline_partition_probe_bounds_node_wait_and_reports_no_candidates() {
    let (output, trace) = offline_partition_probe("timeout");
    assert!(!output.status.success());
    assert_eq!(
        trace,
        format!("partprobe\nudevadm\n{}", "sleep\n".repeat(30))
    );
    let error = String::from_utf8_lossy(&output.stderr);
    assert!(
        error.contains("Windows OS NTFS volume not found; probed candidates:"),
        "{error}"
    );
    assert!(error.contains("timed out waiting for"), "{error}");
    assert!(error.contains("nbd0p* partition nodes"), "{error}");
}

#[test]
fn offline_partition_probe_reports_probed_filesystems_when_os_is_missing() {
    let (output, trace) = offline_partition_probe("no_os");
    assert!(!output.status.success());
    assert_eq!(trace, "partprobe\nudevadm\nblkid:nbd0p1\nblkid:nbd0p2\n");
    let error = String::from_utf8_lossy(&output.stderr);
    assert!(
        error.contains("Windows OS NTFS volume not found; probed candidates:"),
        "{error}"
    );
    assert!(error.contains("nbd0p1 vfat\n"), "{error}");
    assert!(error.contains("nbd0p2 unknown\n"), "{error}");
}

// This scanner covers the quoting used by the fixture, not the full PowerShell
// grammar. It catches dropped block delimiters before an operator boots a VM.
fn powershell_brace_balance(script: &str) -> Result<(), String> {
    let mut chars = script.chars().peekable();
    let mut quote = None;
    let mut braces = Vec::new();
    let mut line = 1;
    while let Some(character) = chars.next() {
        if character == '\n' {
            line += 1;
        }
        if let Some(delimiter) = quote {
            if delimiter == '"' && character == '`' {
                if chars.next() == Some('\n') {
                    line += 1;
                }
            } else if character == delimiter {
                if delimiter == '\'' && chars.peek() == Some(&'\'') {
                    chars.next();
                } else {
                    quote = None;
                }
            }
            continue;
        }
        match character {
            '`' => {
                if chars.next() == Some('\n') {
                    line += 1;
                }
            }
            '\'' | '"' => quote = Some(character),
            '#' => {
                for comment in chars.by_ref() {
                    if comment == '\n' {
                        line += 1;
                        break;
                    }
                }
            }
            '{' => braces.push(line),
            '}' if braces.pop().is_none() => {
                return Err(format!("unmatched closing brace on line {line}"));
            }
            _ => {}
        }
    }
    if let Some(delimiter) = quote {
        return Err(format!("unterminated {delimiter} string at line {line}"));
    }
    if let Some(opened) = braces.last() {
        return Err(format!("unclosed brace from line {opened}"));
    }
    Ok(())
}

#[test]
fn offline_canary_generated_powershell_has_balanced_braces() {
    for present in [true, false] {
        let script = overlay_canary_check(
            "motor-qualification-00000000-0000-7000-8000-000000000000",
            present,
        );
        let command = powershell(&script);
        eprintln!(
            "present={present}, script={} bytes, encoded command={} bytes",
            script.len(),
            command.len()
        );
        assert_eq!(powershell_brace_balance(&script), Ok(()), "{script}");
        assert!(
            command.len() < 8000,
            "encoded canary invocation is {} bytes",
            command.len()
        );
    }
}

#[test]
fn baseline_environment_generated_powershell_has_balanced_braces() {
    let script = baseline_environment_script();
    assert_eq!(powershell_brace_balance(&script), Ok(()), "{script}");
    assert!(powershell(&script).len() < 8000);
}

#[test]
fn symlink_qualification_generated_powershell_has_balanced_braces() {
    let script = symlink_qualification_script();
    assert_eq!(powershell_brace_balance(&script), Ok(()), "{script}");
    assert!(script.contains("[switch]$Check"));
    assert!(script.contains("CreateSymbolicLink"));
}

#[test]
fn standard_user_environment_generated_powershell_has_balanced_braces() {
    let script = standard_user_environment_script();
    assert_eq!(powershell_brace_balance(&script), Ok(()), "{script}");
}

#[test]
fn standard_user_command_resolution_generated_powershell_has_balanced_braces() {
    let script = super::windows_environment::command_resolution_script();
    assert_eq!(powershell_brace_balance(&script), Ok(()), "{script}");
}

#[test]
fn powershell_brace_scanner_ignores_quoted_literals_and_comments() {
    assert_eq!(
        powershell_brace_balance("if ($true) { '{''}' ; \"`\"{\"; # } ' \"\n }"),
        Ok(())
    );
    assert!(powershell_brace_balance("if ($true) { '}'").is_err());
    assert!(powershell_brace_balance("'{' }").is_err());
    assert!(powershell_brace_balance("{ 'unterminated }").is_err());
}
