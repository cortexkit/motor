//! Operator lane: run on the VM host, with ck configured for the installed daemon.
use std::{
    path::PathBuf,
    process::{Command, Output},
    time::{Duration, Instant},
};
static QUALIFICATION_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());
fn qualification_lock() -> std::sync::MutexGuard<'static, ()> {
    QUALIFICATION_LOCK
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}

fn required(name: &str) -> String {
    std::env::var(name).unwrap_or_else(|_| panic!("operator must set {name}"))
}
fn job(command: &str) -> Output {
    job_with_env(command, &[])
}
fn job_with_env(command: &str, env: &[&str]) -> Output {
    // Use the installed client and its configured SSH/socket route, never a
    // test-owned runner or a host shell standing in for the daemon.
    let mut client =
        Command::new(std::env::var("MOTOR_MACOS_QUALIFICATION_CK").unwrap_or_else(|_| "ck".into()));
    client.args(["motor", "run", "--platform", "macos"]);
    if let Ok(sibling) = std::env::var("MOTOR_MACOS_QUALIFICATION_SIBLING") {
        client.args(["--sibling", &sibling]);
    }
    for value in env {
        client.args(["--env", value]);
    }
    client
        .args(["--", command])
        .current_dir(required("MOTOR_MACOS_QUALIFICATION_WORKSPACE"))
        .output()
        .expect("installed client")
}
fn successful(command: &str) -> String {
    let output = job(command);
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    String::from_utf8(output.stdout).unwrap()
}
fn host(tool: &str, args: &[&str]) -> String {
    let output = Command::new(tool).args(args).output().unwrap();
    assert!(output.status.success());
    String::from_utf8(output.stdout).unwrap()
}
fn swap_used() -> String {
    host("/usr/sbin/sysctl", &["-n", "vm.swapusage"])
        .split("used = ")
        .nth(1)
        .unwrap()
        .split_whitespace()
        .next()
        .unwrap()
        .to_string()
}
fn clean() {
    let root = PathBuf::from(required("MOTOR_MACOS_QUALIFICATION_STATE_ROOT"));
    let deadline = Instant::now() + Duration::from_secs(30);
    loop {
        let remaining: Vec<_> = ["jobs", "prep", "clones", "nonces", "macvm-records"]
            .iter()
            .flat_map(|name| {
                std::fs::read_dir(root.join(name))
                    .into_iter()
                    .flatten()
                    .filter_map(Result::ok)
                    .map(|e| e.path())
            })
            .collect();
        if remaining.is_empty() {
            break;
        }
        assert!(Instant::now() < deadline, "resources remain: {remaining:?}");
        std::thread::park_timeout(Duration::from_millis(50));
    }
    let processes = host("/bin/ps", &["-axo", "user=,comm="]);
    assert!(
        !processes
            .lines()
            .any(|line| line.split_whitespace().next() == Some("motor")
                && (line.contains("motor-macvm")
                    || line.contains("com.apple.Virtualization.VirtualMachine"))),
        "{processes}"
    );
}

#[test]
#[ignore = "operator: installed launchd daemon, sealed image, SYNAPSE active, two cold offline builds"]
fn macos_qualification_concurrent() {
    let _lane = qualification_lock();
    let name = required("MOTOR_MACOS_QUALIFICATION_CRATE");
    assert!(name
        .bytes()
        .all(|b| b.is_ascii_alphanumeric() || b"_-".contains(&b)));
    let swap = swap_used();
    let started = Instant::now();
    let command = format!("cargo test -p {name} --offline");
    let (tx, rx) = std::sync::mpsc::channel();
    for _ in 0..2 {
        let tx = tx.clone();
        let command = command.clone();
        std::thread::spawn(move || tx.send(job(&command)).unwrap());
    }
    drop(tx);
    let mut completed = Vec::new();
    let mut peak = 0;
    let mut minimum_free = u64::MAX;
    let page_size: u64 = host("/usr/sbin/sysctl", &["-n", "hw.pagesize"])
        .trim()
        .parse()
        .unwrap();
    while completed.len() < 2 {
        assert!(
            started.elapsed() < Duration::from_secs(1200),
            "installed daemon qualification timed out"
        );
        completed.extend(rx.try_iter());
        let processes = host("/bin/ps", &["-axo", "user=,comm="]);
        let vms: Vec<_> = processes
            .lines()
            .filter(|line| line.contains("com.apple.Virtualization.VirtualMachine"))
            .collect();
        assert!(vms.len() <= 2, "{vms:?}");
        for vm in &vms {
            assert_eq!(vm.split_whitespace().next(), Some("motor"));
        }
        peak = peak.max(vms.len());
        let memory = host("/usr/bin/vm_stat", &[]);
        let free_pages: u64 = memory
            .lines()
            .find_map(|line| line.strip_prefix("Pages free:"))
            .unwrap()
            .trim()
            .trim_end_matches('.')
            .parse()
            .unwrap();
        minimum_free = minimum_free.min(free_pages * page_size);
        std::thread::park_timeout(Duration::from_millis(100));
    }
    assert_eq!(peak, 2, "jobs did not overlap in two VMs");
    for output in completed {
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
    }
    assert_eq!(swap_used(), swap, "swap grew during concurrent VM builds");
    eprintln!("cold virtiofs build wall time: {:?}; minimum free memory: {minimum_free} bytes; swap used before/after: {swap}; service owners: motor",started.elapsed());
    clean();
}
#[test]
#[ignore = "operator: installed launchd daemon and disposable sealed image"]
fn macos_qualification_isolation() {
    let _lane = qualification_lock();
    let sibling = required("MOTOR_MACOS_QUALIFICATION_SIBLING");
    assert!(
        sibling.starts_with("/Users/")
            && sibling
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || b"/_-.".contains(&b))
    );
    let command = format!(
        r#"set -eu
test "$(ifconfig -l)" = lo0
test "$(id -un)" = motorjob
git log -1 >/dev/null
test ! -e .git/objects/info/alternates
printf 'cwd=%s\n' "$PWD"
mount -t virtiofs
if touch '{sibling}/.motor-write-must-fail'; then exit 1; fi
cat > "$TMPDIR/device-probe.c" <<'C'
#include <fcntl.h>
#include <unistd.h>
#include <errno.h>
#include <stdlib.h>
int main(int argc,char **argv) {{
  if(argc!=2) return 2;
  void *bytes=0; if(posix_memalign(&bytes,4096,4096)) return 2;
  int fd=open(argv[1],O_RDONLY|O_NOFOLLOW);
  if(fd<0) return errno==EACCES || errno==EPERM ? 0 : 1;
  ssize_t count=read(fd,bytes,4096); int error=errno;
  close(fd); free(bytes);
  return count<0 && (error==EACCES || error==EPERM) ? 0 : 1;
}}
C
cc "$TMPDIR/device-probe.c" -o "$TMPDIR/device-probe"
disks=0
for disk in /dev/rdisk*; do
  name=${{disk#/dev/rdisk}}
  case "$name" in ''|*[!0-9]*) continue;; esac
  disks=$((disks+1))
  "$TMPDIR/device-probe" "$disk"
done
test "$disks" -ge 2
diskutil list -plist physical > "$TMPDIR/disks.plist"
/usr/bin/python3 -c 'import plistlib,sys; disks=plistlib.load(open(sys.argv[1],"rb"))["AllDisksAndPartitions"]; assert len(disks)==2, disks' "$TMPDIR/disks.plist"
cat > "$TMPDIR/vsock-probe.c" <<'C'
#include <sys/socket.h>
#include <sys/vsock.h>
#include <sys/time.h>
#include <errno.h>
#include <unistd.h>
int main(void) {{
  int fd=socket(AF_VSOCK,SOCK_STREAM,0);
  if(fd<0) return errno==EPERM || errno==EACCES ? 0 : 1;
  struct sockaddr_vm addr={{0}};
  addr.svm_len=sizeof(addr); addr.svm_family=AF_VSOCK;
  addr.svm_cid=VMADDR_CID_HOST; addr.svm_port=5000;
  if(connect(fd,(struct sockaddr*)&addr,sizeof(addr))<0)
    return errno==ECONNREFUSED || errno==ECONNRESET || errno==EPERM || errno==EACCES ? 0 : 1;
  struct timeval timeout={{2,0}}; setsockopt(fd,SOL_SOCKET,SO_RCVTIMEO,&timeout,sizeof(timeout));
  char byte; int n=read(fd,&byte,1);
  return n==0 || (n<0 && (errno==ECONNRESET || errno==ENOTCONN)) ? 0 : 1;
}}
C
cc "$TMPDIR/vsock-probe.c" -o "$TMPDIR/vsock-probe"
"$TMPDIR/vsock-probe"
"#
    );
    let output = successful(&command);
    let workspace = required("MOTOR_MACOS_QUALIFICATION_WORKSPACE");
    assert!(output.contains(&format!("cwd={workspace}")));
    let mounts: Vec<_> = output
        .lines()
        .filter(|line| line.contains("(virtiofs"))
        .collect();
    assert_eq!(mounts.len(), 3, "{output}");
    for (tag, path, read_only) in [
        ("workspace", workspace.as_str(), false),
        ("sibling-0", sibling.as_str(), true),
        ("cargo-home", "/opt/motor/job/cargo-home", false),
    ] {
        let line = mounts
            .iter()
            .find(|line| line.starts_with(&format!("{tag} on {path} (")))
            .unwrap_or_else(|| panic!("missing {tag} on {path}: {output}"));
        assert_eq!(line.contains("read-only"), read_only, "{line}");
        assert!(
            line.contains("nosuid") && line.contains("nodev"),
            "unsafe share flags: {line}"
        );
    }
    let marker = format!(".motor-qualification-{}", uuid::Uuid::now_v7().simple());
    let guest_command = format!("printf visible > '{marker}'; sleep 5");
    let worker = std::thread::spawn(move || successful(&guest_command));
    let root = PathBuf::from(required("MOTOR_MACOS_QUALIFICATION_STATE_ROOT"));
    let deadline = Instant::now() + Duration::from_secs(120);
    loop {
        let found = std::fs::read_dir(root.join("jobs"))
            .into_iter()
            .flatten()
            .filter_map(Result::ok)
            .map(|entry| {
                entry
                    .path()
                    .join(workspace.trim_start_matches('/'))
                    .join(&marker)
            })
            .find(|path| path.is_file());
        if let Some(path) = found {
            use std::os::unix::fs::MetadataExt;
            let motor = unsafe { libc::getpwnam(c"motor".as_ptr()) };
            assert!(!motor.is_null());
            assert_eq!(std::fs::metadata(path).unwrap().uid(), unsafe {
                (*motor).pw_uid
            });
            break;
        }
        assert!(
            !worker.is_finished() && Instant::now() < deadline,
            "guest-written host file was never visible"
        );
        std::thread::park_timeout(Duration::from_millis(20));
    }
    worker.join().unwrap();
    clean();
}
#[test]
#[ignore = "operator: installed launchd daemon and macOS parity manifest in the job snapshot"]
fn macos_qualification_env() {
    let _lane = qualification_lock();
    let result=job_with_env("set -eu; env; printf 'shell=%s\\n' \"$0\"; python3 script/runner/toolchain-parity.py --check --role macos --job --manifest script/macos-runner/toolchain.json",&["HOME=/poison","TMPDIR=/poison","PATH=/poison","RUSTUP_HOME=/poison","CARGO_HOME=/poison","CARGO_TARGET_DIR=/poison"]);
    assert!(
        result.status.success(),
        "{}",
        String::from_utf8_lossy(&result.stderr)
    );
    let output = String::from_utf8(result.stdout).unwrap();
    for expected in [
        "HOME=/Users/motorjob",
        "RUSTUP_HOME=/opt/motor/rustup",
        "CARGO_HOME=/opt/motor/job/cargo-home",
        "shell=/opt/motor/bin/bash",
    ] {
        assert!(
            output.lines().any(|line| line == expected),
            "missing {expected}: {output}"
        );
    }
    assert!(!output
        .lines()
        .any(|line| line.starts_with("CARGO_TARGET_DIR=")));
    clean();
}
#[test]
#[ignore = "operator: cold Cargo/Bun caches and locked sibling/platform-package fixture"]
fn macos_qualification_bun() {
    let _lane = qualification_lock();
    required("MOTOR_MACOS_QUALIFICATION_SIBLING");
    successful("set -eu; git diff --exit-code -- bun.lock Cargo.lock; bun install --offline --frozen-lockfile --ignore-scripts; cargo test --offline; git diff --exit-code -- bun.lock Cargo.lock");
    clean();
}

// Identity and SIGKILL checks observe the installed daemon's helper/service.
// The gate primitive drives that exact signed helper directly, so daemon
// escalation cannot mask a failure of EOF itself to stop the VM.
#[derive(serde::Deserialize, Clone)]
struct HelperRecord {
    id: uuid::Uuid,
    helper: Option<motor_macvm::ProcessIdentity>,
    service: Option<motor_macvm::ProcessIdentity>,
    gate_open: bool,
    paths: HelperPaths,
}
#[derive(serde::Deserialize, Clone)]
struct HelperPaths {
    clone: PathBuf,
    nonce: PathBuf,
}
fn helper_records() -> Vec<HelperRecord> {
    let root = PathBuf::from(required("MOTOR_MACOS_QUALIFICATION_STATE_ROOT"));
    std::fs::read_dir(root.join("macvm-records"))
        .into_iter()
        .flatten()
        .filter_map(Result::ok)
        .filter_map(|entry| std::fs::read(entry.path()).ok())
        .filter_map(|bytes| serde_json::from_slice(&bytes).ok())
        .collect()
}
fn cancel_installed(id: uuid::Uuid) -> std::io::Result<()> {
    use motor_protocol::{read_frame, write_frame, CancelRequest, Frame, PROTOCOL_VERSION};
    let root = PathBuf::from(required("MOTOR_MACOS_QUALIFICATION_STATE_ROOT"));
    let socket = root
        .join(required("MOTOR_MACOS_QUALIFICATION_INSTANCE"))
        .join("runner.sock");
    let mut stream = std::os::unix::net::UnixStream::connect(socket)?;
    stream.set_read_timeout(Some(Duration::from_secs(2)))?;
    stream.set_write_timeout(Some(Duration::from_secs(2)))?;
    write_frame(
        &mut stream,
        &Frame::Hello {
            protocol_version: PROTOCOL_VERSION,
            started: true,
            default_branch_tip: false,
            gate_jobs: false,
            return_paths: false,
            return_chunked: false,
            platforms: vec!["macos".into()],
        },
    )?;
    loop {
        match read_frame(&mut stream)? {
            Some(Frame::Hello { .. }) => break,
            Some(Frame::Heartbeat { .. }) => {}
            _ => {
                return Err(std::io::Error::other(
                    "installed daemon refused qualification Hello",
                ))
            }
        }
    }
    write_frame(&mut stream, &Frame::Cancel(CancelRequest::new(id)))?;
    loop {
        match read_frame(&mut stream)? {
            Some(Frame::CancelReply(_)) => break,
            Some(Frame::Heartbeat { .. }) => {}
            _ => {
                return Err(std::io::Error::other(
                    "installed daemon closed before cancel reply",
                ))
            }
        }
    }
    Ok(())
}
fn with_live_helper(check: impl FnOnce(&HelperRecord)) {
    let previous: std::collections::BTreeSet<_> =
        helper_records().into_iter().map(|r| r.id).collect();
    let worker = std::thread::spawn(|| job("sleep 30"));
    let deadline = Instant::now() + Duration::from_secs(120);
    let mut candidate = None;
    let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        loop {
            if let Some(record) = helper_records()
                .into_iter()
                .find(|r| !previous.contains(&r.id))
            {
                let ready = record.gate_open && record.helper.is_some() && record.service.is_some();
                candidate = Some(record);
                if ready {
                    break;
                }
            }
            assert!(
                Instant::now() < deadline && !worker.is_finished(),
                "installed job never reported its helper/service"
            );
            std::thread::park_timeout(Duration::from_millis(10));
        }
        check(candidate.as_ref().unwrap());
    }));
    if let Some(record) = candidate {
        cancel_installed(record.id).unwrap();
    }
    let _ = worker.join();
    clean();
    if let Err(panic) = result {
        std::panic::resume_unwind(panic);
    }
}
#[test]
#[ignore = "operator: installed daemon, real signed VM helper, dedicated motor account"]
fn macos_qualification_helper_identity() {
    let _lane = qualification_lock();
    with_live_helper(|record| {
        let helper = record.helper.unwrap();
        let service = record.service.unwrap();
        assert!(motor_macvm::process::identity_alive(helper).unwrap());
        assert_eq!(
            motor_macvm::process::identify_service(&record.paths.clone, helper).unwrap(),
            service
        );
        assert!(motor_macvm::process::disk_services(&record.paths.clone)
            .unwrap()
            .contains(&service));
        assert!(!motor_macvm::process::release_proven(
            Some(service),
            &record.paths.clone,
            &record.paths.nonce
        )
        .unwrap());
    });
}
#[test]
#[ignore = "operator: installed daemon; deliberately SIGKILL its job helper"]
fn macos_qualification_helper_sigkill() {
    let _lane = qualification_lock();
    with_live_helper(|record| {
        let helper = record.helper.unwrap();
        let service = record.service.unwrap();
        // Numeric-PID signals retain the documented Darwin reuse window; the
        // qualification requires no other jobs or process churn in this lane.
        assert_eq!(motor_macvm::process_identity(helper.pid).unwrap(), helper);
        let now = Instant::now();
        assert_eq!(unsafe { libc::kill(helper.pid, libc::SIGKILL) }, 0);
        while motor_macvm::process::identity_alive(helper).unwrap()
            || motor_macvm::process::identity_alive(service).unwrap()
        {
            assert!(
                now.elapsed() < Duration::from_secs(2),
                "helper SIGKILL left a VM service alive"
            );
            std::thread::park_timeout(Duration::from_millis(5));
        }
        if record.paths.clone.exists() && record.paths.nonce.exists() {
            match motor_macvm::process::release_proven(
                Some(service),
                &record.paths.clone,
                &record.paths.nonce,
            ) {
                Ok(released) => assert!(released),
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
                Err(error) => panic!("disk release scan: {error}"),
            }
        }
    });
}
#[test]
#[ignore = "operator: exact installed signed helper, sealed image, both gate EOF phases"]
fn macos_qualification_helper_gate() {
    let _lane = qualification_lock();
    let (binary, image) = signed_daemon_helper_configuration();
    for open in [false, true] {
        let before = vm_service_snapshot();
        assert!(
            before.len() < 2,
            "gate primitive needs a free VM capacity slot"
        );
        let mut fixture = GateFixture::new();
        let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            fixture
                .prepare(&binary, open.then_some(image.as_path()))
                .unwrap();
            if open {
                fixture.opened = true;
                fixture.helper.as_mut().unwrap().open_gate().unwrap();
                let deadline = Instant::now() + Duration::from_secs(120);
                let service = loop {
                    fixture.collect_reports().unwrap();
                    if let Some(service) = fixture.reports.iter().find_map(|report| match report {
                        motor_macvm::HelperReport::Started { service, .. } => Some(*service),
                        _ => None,
                    }) {
                        break service;
                    }
                    assert!(
                        fixture
                            .helper
                            .as_mut()
                            .unwrap()
                            .child
                            .try_wait()
                            .unwrap()
                            .is_none(),
                        "helper exited before VM start: {:?}",
                        fixture.reports
                    );
                    assert!(
                        Instant::now() < deadline,
                        "signed helper did not start its VM"
                    );
                    std::thread::park_timeout(Duration::from_millis(10));
                };
                assert!(motor_macvm::process::identity_alive(service).unwrap());
                let helper = motor_macvm::process_identity(
                    fixture.helper.as_ref().unwrap().child.id() as i32,
                )
                .unwrap();
                assert_eq!(
                    motor_macvm::process::identify_service(
                        &fixture.plan.as_ref().unwrap().request().clone,
                        helper
                    )
                    .unwrap(),
                    service
                );
                let closed = fixture.assert_eof_exit(motor_macvm::HelperStopReason::GateEof);
                let plan = fixture.plan.as_ref().unwrap();
                while motor_macvm::process::identity_alive(service).unwrap()
                    || !motor_macvm::process::release_proven(
                        Some(service),
                        &plan.request().clone,
                        &plan.request().nonce_disk,
                    )
                    .unwrap()
                {
                    assert!(
                        closed.elapsed() < Duration::from_secs(2),
                        "EOF did not release the VM before any escalation"
                    );
                    std::thread::park_timeout(Duration::from_millis(5));
                }
            } else {
                std::thread::park_timeout(Duration::from_millis(100));
                assert!(fixture
                    .helper
                    .as_mut()
                    .unwrap()
                    .child
                    .try_wait()
                    .unwrap()
                    .is_none());
                fixture.assert_eof_exit(motor_macvm::HelperStopReason::GateEofBeforeStart);
                assert_eq!(
                    vm_service_snapshot(),
                    before,
                    "an unopened gate created a VM service"
                );
            }
        }));
        let cleanup = fixture.finish();
        if let Err(error) = &cleanup {
            eprintln!(
                "gate qualification cleanup failed; inventory retained at {}: {error}",
                fixture.root.display()
            );
        }
        if let Err(panic) = result {
            std::panic::resume_unwind(panic);
        }
        cleanup.unwrap();
    }
}
fn vm_service_snapshot() -> std::collections::BTreeSet<String> {
    host("/bin/ps", &["-axo", "pid=,comm="])
        .lines()
        .filter(|line| line.contains("com.apple.Virtualization.VirtualMachine"))
        .map(str::to_string)
        .collect()
}

fn signed_daemon_helper_configuration() -> (PathBuf, PathBuf) {
    use std::io::Write;
    let mut configuration = None;
    with_live_helper(|record| {
        let helper = record.helper.unwrap();
        let parent = host("/bin/ps", &["-p", &helper.pid.to_string(), "-o", "ppid="]);
        let command = host("/bin/ps", &["-ww", "-p", parent.trim(), "-o", "command="]);
        let arguments: Vec<_> = command.split_whitespace().collect();
        let index = arguments
            .iter()
            .position(|arg| *arg == "--vm-config")
            .expect("daemon --vm-config argument");
        let config_path = std::fs::canonicalize(arguments[index + 1]).unwrap();
        let expected_config =
            std::fs::canonicalize(required("MOTOR_MACOS_QUALIFICATION_VM_CONFIG")).unwrap();
        assert_eq!(config_path, expected_config);
        let value: toml::Value =
            toml::from_str(&std::fs::read_to_string(&config_path).unwrap()).unwrap();
        let binary =
            std::fs::canonicalize(value.get("helper").and_then(toml::Value::as_str).unwrap())
                .unwrap();
        let image =
            std::fs::canonicalize(value.get("image").and_then(toml::Value::as_str).unwrap())
                .unwrap();
        assert_eq!(
            binary,
            std::fs::canonicalize(required("MOTOR_MACOS_QUALIFICATION_HELPER")).unwrap()
        );
        let running = host(
            "/bin/ps",
            &["-ww", "-p", &helper.pid.to_string(), "-o", "comm="],
        );
        assert_eq!(
            binary,
            std::fs::canonicalize(running.trim()).unwrap(),
            "test must use the daemon's exact installed helper"
        );
        assert!(
            !binary
                .components()
                .any(|part| part.as_os_str() == "target" || part.as_os_str() == "deps"),
            "build-tree helper is not an installed qualification binary"
        );
        host(
            "/usr/bin/codesign",
            &["--verify", "--strict", binary.to_str().unwrap()],
        );
        let entitlements = Command::new("/usr/bin/codesign")
            .args(["-d", "--entitlements", ":-"])
            .arg(&binary)
            .output()
            .unwrap();
        assert!(entitlements.status.success());
        let mut convert = Command::new("/usr/bin/plutil")
            .args(["-convert", "json", "-o", "-", "-"])
            .stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::piped())
            .spawn()
            .unwrap();
        convert
            .stdin
            .take()
            .unwrap()
            .write_all(&entitlements.stdout)
            .unwrap();
        let converted = convert.wait_with_output().unwrap();
        assert!(converted.status.success());
        let entitlements: serde_json::Value = serde_json::from_slice(&converted.stdout).unwrap();
        assert_eq!(
            entitlements
                .get("com.apple.security.virtualization")
                .and_then(serde_json::Value::as_bool),
            Some(true)
        );
        configuration = Some((binary, image));
    });
    configuration.unwrap()
}
struct GateFixture {
    root: PathBuf,
    owned: bool,
    opened: bool,
    image_ready: bool,
    plan: Option<motor_macvm::JobVmConfig>,
    helper: Option<motor_macvm::JobHelper>,
    reports: Vec<motor_macvm::HelperReport>,
    report_bytes: Vec<u8>,
}
impl GateFixture {
    fn new() -> Self {
        Self {
            root: PathBuf::from(required("MOTOR_MACOS_QUALIFICATION_STATE_ROOT"))
                .join("qualification")
                .join(format!("gate-{}", uuid::Uuid::now_v7())),
            owned: false,
            opened: false,
            image_ready: false,
            plan: None,
            helper: None,
            reports: vec![],
            report_bytes: vec![],
        }
    }
    fn prepare(
        &mut self,
        binary: &std::path::Path,
        image: Option<&std::path::Path>,
    ) -> std::io::Result<()> {
        use std::{
            io::Write,
            os::{fd::AsRawFd, unix::fs::DirBuilderExt},
        };
        std::fs::create_dir_all(self.root.parent().unwrap())?;
        std::fs::DirBuilder::new().mode(0o700).create(&self.root)?;
        self.owned = true;
        let share = |tag: &str, guest: &str| motor_macvm::ShareLocation {
            host: self.root.join(tag),
            tag: tag.into(),
            guest: guest.into(),
        };
        let plan = motor_macvm::JobVmConfig::new(motor_macvm::JobVmRequest {
            clone: self.root.join("clone"),
            nonce_disk: self.root.join("nonce"),
            workspace: share("workspace", "/Users/motor/gate"),
            siblings: vec![],
            cargo_home: share("cargo-home", motor_macvm::CARGO_HOME),
        })?;
        let mut inventory = std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(self.root.join("inventory.json"))?;
        inventory.write_all(&serde_json::to_vec(plan.request())?)?;
        inventory.sync_all()?;
        std::fs::File::open(&self.root)?.sync_all()?;
        if let Some(image) = image {
            motor_macvm::clone_image(image, &plan.request().clone)?;
            motor_macvm::create_nonce_disk(
                &plan.request().nonce_disk,
                &uuid::Uuid::now_v7().simple().to_string(),
                &plan.mounts(),
            )?;
            std::fs::create_dir(self.root.join("workspace"))?;
            std::fs::create_dir(self.root.join("cargo-home"))?;
            self.image_ready = true;
        }
        let slot = std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(self.root.join("slot.lock"))?;
        fs2::FileExt::lock_exclusive(&slot)?;
        let helper = motor_macvm::spawn_job_helper(binary, &plan, &slot)?;
        self.plan = Some(plan);
        self.helper = Some(helper);
        if unsafe {
            libc::fcntl(
                self.helper.as_ref().unwrap().reports.as_raw_fd(),
                libc::F_SETFL,
                libc::O_NONBLOCK,
            )
        } < 0
        {
            return Err(std::io::Error::last_os_error());
        }
        Ok(())
    }
    fn collect_reports(&mut self) -> std::io::Result<()> {
        use std::io::Read;
        let Some(helper) = self.helper.as_mut() else {
            return Ok(());
        };
        let mut bytes = [0; 4096];
        loop {
            match helper.reports.read(&mut bytes) {
                Ok(0) => break,
                Ok(n) => self.report_bytes.extend_from_slice(&bytes[..n]),
                Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => break,
                Err(e) if e.kind() == std::io::ErrorKind::Interrupted => {}
                Err(e) => return Err(e),
            }
        }
        while let Some(end) = self.report_bytes.iter().position(|b| *b == b'\n') {
            self.reports
                .push(serde_json::from_slice(&self.report_bytes[..end])?);
            drop(self.report_bytes.drain(..=end));
        }
        Ok(())
    }
    fn assert_eof_exit(&mut self, reason: motor_macvm::HelperStopReason) -> Instant {
        self.helper.as_mut().unwrap().close_gate();
        let closed = Instant::now();
        // There is no escalation task or timer in this proof phase. Any fallback
        // kill is confined to finish(), after this assertion has passed or failed.
        loop {
            self.collect_reports().unwrap();
            if let Some(status) = self.helper.as_mut().unwrap().child.try_wait().unwrap() {
                assert!(status.success(), "helper did not exit from EOF normally");
                break;
            }
            assert!(
                closed.elapsed() < Duration::from_secs(2),
                "helper did not stop on gate EOF before cleanup escalation"
            );
            std::thread::park_timeout(Duration::from_millis(5));
        }
        self.collect_reports().unwrap();
        assert!(
            !self
                .reports
                .iter()
                .any(|report| matches!(report, motor_macvm::HelperReport::Failed { .. })),
            "helper reported failure during EOF proof: {:?}",
            self.reports
        );
        assert!(self.reports.iter().any(|report|matches!(report,motor_macvm::HelperReport::Stopped{reason:observed} if *observed==reason)),"helper did not attribute its stop to EOF: {:?}",self.reports);
        closed
    }
    fn finish(&mut self) -> std::io::Result<()> {
        if let Some(helper) = self.helper.as_mut() {
            helper.close_gate();
            let deadline = Instant::now() + Duration::from_secs(2);
            while helper.child.try_wait()?.is_none() && Instant::now() < deadline {
                std::thread::park_timeout(Duration::from_millis(5));
            }
            if helper.child.try_wait()?.is_none() {
                helper.child.kill()?;
            }
            let deadline = Instant::now() + Duration::from_secs(2);
            while helper.child.try_wait()?.is_none() {
                if Instant::now() >= deadline {
                    return Err(std::io::Error::other(
                        "qualification helper did not exit; inventory retained",
                    ));
                }
                std::thread::park_timeout(Duration::from_millis(5));
            }
        }
        if self.opened || (self.image_ready && self.helper.is_some()) {
            let plan = self.plan.as_ref().unwrap();
            for service in motor_macvm::process::disk_services(&plan.request().clone)? {
                if motor_macvm::process::identity_alive(service)?
                    && unsafe { libc::kill(service.pid, libc::SIGKILL) } != 0
                {
                    return Err(std::io::Error::last_os_error());
                }
            }
            let deadline = Instant::now() + Duration::from_secs(2);
            while !motor_macvm::process::release_proven(
                None,
                &plan.request().clone,
                &plan.request().nonce_disk,
            )? {
                if Instant::now() >= deadline {
                    return Err(std::io::Error::other(
                        "qualification disk references remain; inventory retained",
                    ));
                }
                std::thread::park_timeout(Duration::from_millis(5));
            }
        }
        if self.owned {
            std::fs::remove_dir_all(&self.root)?;
            self.owned = false;
        }
        Ok(())
    }
}
