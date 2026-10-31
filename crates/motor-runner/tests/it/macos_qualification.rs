//! Operator lane: run on the VM host, with ck configured for the installed daemon.
use super::qualification_support as support;
use std::{
    path::{Path, PathBuf},
    process::{Command, Output},
    time::{Duration, Instant},
};
static QUALIFICATION_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());
fn qualification_lock() -> std::sync::MutexGuard<'static, ()> {
    assert_eq!(
        host("/usr/bin/id", &["-un"]).trim(),
        "motor",
        "qualification process inspection must run as motor, never root"
    );
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
    let evidence = evidence_job("job");
    job_at(
        command,
        env,
        &required("MOTOR_MACOS_QUALIFICATION_WORKSPACE"),
        &evidence,
        None,
    )
}
fn evidence_job(label: &str) -> PathBuf {
    use std::os::unix::fs::DirBuilderExt;
    let root = std::fs::canonicalize(required("MOTOR_MACOS_QUALIFICATION_EVIDENCE")).unwrap();
    let path = root.join(format!("{label}-{}", uuid::Uuid::now_v7()));
    std::fs::DirBuilder::new()
        .mode(0o700)
        .create(&path)
        .unwrap();
    path
}
fn accepted_id(evidence: &Path) -> Option<uuid::Uuid> {
    let bytes = std::fs::read_to_string(evidence.join("records.jsonl")).ok()?;
    bytes
        .lines()
        .filter_map(|line| serde_json::from_str::<serde_json::Value>(line).ok())
        .find(|row| row["type"] == "accepted")?
        .get("job_id")?
        .as_str()?
        .parse()
        .ok()
}
fn job_at(
    command: &str,
    env: &[&str],
    workspace: &str,
    evidence: &Path,
    capture: Option<&str>,
) -> Output {
    job_at_with_returns(command, env, workspace, evidence, capture, &[])
}
fn job_at_with_returns(
    command: &str,
    env: &[&str],
    workspace: &str,
    evidence: &Path,
    capture: Option<&str>,
    returns: &[&str],
) -> Output {
    let normalized = std::fs::canonicalize(workspace).unwrap();
    assert_eq!(
        Path::new(workspace),
        normalized,
        "operator cwd must be normalized and have no symlink ancestors"
    );
    assert!(normalized.starts_with("/Users"));
    // Use the installed client and its configured SSH/socket route, never a
    // test-owned runner or a host shell standing in for the daemon.
    let mut client =
        Command::new(std::env::var("MOTOR_MACOS_QUALIFICATION_CK").unwrap_or_else(|_| "ck".into()));
    client.args(["motor", "run", "--platform", "macos"]);
    client
        .arg("--record-file")
        .arg(evidence.join("records.jsonl"));
    client.args(["--cwd", workspace]);
    for path in returns {
        client.args(["--return-path", path]);
    }
    if let Ok(sibling) = std::env::var("MOTOR_MACOS_QUALIFICATION_SIBLING") {
        client.args(["--sibling", &sibling]);
    }
    for value in env {
        client.args(["--env", value]);
    }
    client.args(["--", command]).current_dir(workspace);
    let (tx, rx) = std::sync::mpsc::channel();
    let child = client
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .expect("installed client");
    let identity = motor_macvm::process_identity(child.id() as i32).unwrap();
    std::thread::spawn(move || {
        let _ = tx.send(child.wait_with_output().expect("installed client output"));
    });
    let deadline = Instant::now() + Duration::from_secs(1200);
    let root = PathBuf::from(required("MOTOR_MACOS_QUALIFICATION_STATE_ROOT"));
    let mut collected = false;
    let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| loop {
        helper_records();
        if let (Some(capture), Some(id)) = (capture, accepted_id(evidence)) {
            let source = PathBuf::from("jobs")
                .join(id.to_string())
                .join(workspace.trim_start_matches('/'))
                .join(capture);
            if !collected {
                collected = support::collect_capture(&root, &source, evidence).unwrap();
            }
        }
        match rx.recv_timeout(Duration::from_millis(50)) {
            Ok(output) => break output,
            Err(std::sync::mpsc::RecvTimeoutError::Timeout) => {
                assert!(Instant::now() < deadline, "installed job timed out")
            }
            Err(error) => panic!("installed client worker: {error}"),
        }
    }));
    let output = match result {
        Ok(output) => output,
        Err(panic) => {
            if let Some(id) = accepted_id(evidence) {
                if let Err(error) = cancel_installed(id) {
                    eprintln!("failed to cancel {id} after qualification failure: {error}");
                }
            }
            if motor_macvm::process::identity_alive(identity).unwrap() {
                unsafe {
                    libc::kill(identity.pid, libc::SIGTERM);
                }
            }
            let _ = rx.recv_timeout(Duration::from_secs(120));
            std::panic::resume_unwind(panic);
        }
    };
    std::fs::write(evidence.join("cli.stdout"), &output.stdout).unwrap();
    std::fs::write(evidence.join("cli.stderr"), &output.stderr).unwrap();
    std::fs::write(
        evidence.join("cli.status"),
        output
            .status
            .code()
            .expect("CLI terminated by signal")
            .to_string(),
    )
    .unwrap();
    eprintln!("job evidence: {}", evidence.display());
    if output.status.success() {
        let id = accepted_id(evidence).expect("successful job must have Accepted evidence");
        assert!(
            OBSERVED
                .lock()
                .unwrap()
                .iter()
                .any(|record| record.id == id),
            "no durable record for accepted job: wrong state root or missing observations"
        );
    }
    if capture.is_some() {
        assert!(collected, "independent guest capture was not collected");
        let result = Command::new("python3")
            .arg("script/macos-runner/qualification.py")
            .arg(evidence)
            .output()
            .unwrap();
        assert!(
            result.status.success(),
            "{}",
            String::from_utf8_lossy(&result.stderr)
        );
        eprintln!("{}", String::from_utf8_lossy(&result.stdout));
    }
    output
}
fn successful(command: &str) -> String {
    let output = job(command);
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    eprintln!(
        "guest stdout:\n{}\nguest/CLI stderr:\n{}",
        String::from_utf8_lossy(&output.stdout),
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
        let mut remaining: Vec<_> = [
            "jobs",
            "prep",
            "clones",
            "nonces",
            "macvm-records",
            "quarantine",
        ]
        .iter()
        .flat_map(|name| support::entries(&root.join(name)).expect("readable cleanup inventory"))
        .collect();
        remaining.extend(
            support::entries(&root.join("targets"))
                .unwrap()
                .into_iter()
                .filter(|path| {
                    path.file_name()
                        .unwrap()
                        .to_string_lossy()
                        .ends_with(".tmp")
                }),
        );
        eprintln!("cleanup inventory: {remaining:?}");
        if remaining.is_empty() {
            break;
        }
        assert!(Instant::now() < deadline, "resources remain: {remaining:?}");
        std::thread::park_timeout(Duration::from_millis(50));
    }
    for record in OBSERVED.lock().unwrap().iter() {
        for identity in [record.helper, record.service].into_iter().flatten() {
            assert!(
                !motor_macvm::process::identity_alive(identity).unwrap(),
                "recorded process remains: {identity:?}"
            );
        }
        for path in record.paths.all() {
            assert!(
                matches!(std::fs::symlink_metadata(path), Err(error) if error.kind() == std::io::ErrorKind::NotFound),
                "recorded resource remains or is unreadable: {path:?}"
            );
        }
    }
}

#[test]
#[ignore = "operator: installed launchd daemon, sealed image, SYNAPSE active, two cold offline builds"]
fn macos_qualification_concurrent() {
    let _lane = qualification_lock();
    let name = required("MOTOR_MACOS_QUALIFICATION_CRATE");
    assert!(
        !name.is_empty()
            && name
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || b"_-".contains(&b))
    );
    let swap = swap_used();
    eprintln!(
        "ambient VM inventory (not qualification jobs): {:?}",
        vm_service_snapshot()
    );
    let started = Instant::now();
    let command = format!("cargo test -p {name} --offline");
    let workspaces = [
        required("MOTOR_MACOS_QUALIFICATION_WORKSPACE"),
        required("MOTOR_MACOS_QUALIFICATION_WORKSPACE_SECOND"),
    ];
    support::distinct_workspaces(Path::new(&workspaces[0]), Path::new(&workspaces[1])).unwrap();
    let evidence = [evidence_job("concurrent-a"), evidence_job("concurrent-b")];
    let marker = format!(".motor-qualification-{}", uuid::Uuid::now_v7());
    let guest_capture = include_str!("../../../../script/macos-runner/guest_capture.py");
    let command = format!(
        "python3 - '{command}' '{marker}' <<'MOTOR_CAPTURE'\n{guest_capture}\nMOTOR_CAPTURE\n"
    );
    let (tx, rx) = std::sync::mpsc::channel();
    for index in 0..2 {
        let tx = tx.clone();
        let command = command.clone();
        let workspace = workspaces[index].clone();
        let evidence = evidence[index].clone();
        let marker = marker.clone();
        std::thread::spawn(move || {
            tx.send(job_at(&command, &[], &workspace, &evidence, Some(&marker)))
                .unwrap()
        });
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
        let ids: Vec<_> = evidence
            .iter()
            .filter_map(|path| accepted_id(path))
            .collect();
        let records = helper_records();
        let candidates: Vec<_> = records
            .iter()
            .map(|r| (r.id, r.service, r.gate_open))
            .collect();
        let vms = support::attributed_services(&ids, &candidates, |identity| {
            motor_macvm::process::identity_alive(identity).unwrap()
        });
        for service in &vms {
            let record = records
                .iter()
                .find(|r| r.service == Some(*service))
                .unwrap();
            let helper = record.helper.unwrap();
            let identified = motor_macvm::process::identify_service(&record.paths.clone, helper);
            let attached = motor_macvm::process::disk_services(&record.paths.clone);
            if !motor_macvm::process::identity_alive(*service).unwrap()
                || !motor_macvm::process::identity_alive(helper).unwrap()
            {
                continue;
            }
            assert_eq!(identified.unwrap(), *service);
            assert_eq!(
                attached.unwrap(),
                vec![*service],
                "extra service attached to a qualification clone"
            );
            let owner = Command::new("/bin/ps")
                .args(["-p", &service.pid.to_string(), "-o", "user="])
                .output()
                .unwrap();
            if motor_macvm::process::identity_alive(*service).unwrap() {
                assert!(owner.status.success());
                assert_eq!(String::from_utf8(owner.stdout).unwrap().trim(), "motor");
            }
        }
        assert!(vms.len() <= 2);
        peak = peak.max(vms.len());
        let memory = host("/usr/bin/vm_stat", &[]);
        eprintln!(
            "vm_stat:\n{memory}\nVM identities: {:?}",
            vm_service_snapshot()
        );
        let free_pages: u64 = memory
            .lines()
            .find_map(|line| line.strip_prefix("Pages free:"))
            .unwrap()
            .trim()
            .trim_end_matches('.')
            .parse()
            .unwrap();
        minimum_free = minimum_free.min(free_pages * page_size);
        let sample_swap = swap_used();
        eprintln!("sample elapsed={:?} ids={ids:?} attributed={vms:?} free_bytes={} swap_used={sample_swap}", started.elapsed(), free_pages * page_size);
        assert_eq!(sample_swap, swap, "swap grew during concurrent VM builds");
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
    assert!(accepted_id(&evidence[0]).is_some() && accepted_id(&evidence[1]).is_some());
    assert_ne!(accepted_id(&evidence[0]), accepted_id(&evidence[1]));
    eprintln!("two-client interval (prep/boot/build/cleanup): {:?}; minimum free memory: {minimum_free} bytes; swap used before/after: {swap}; service owners: motor",started.elapsed());
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
#[ignore = "operator: installed daemon/client with macOS returned-file support"]
fn macos_qualification_returned_file() {
    let _lane = qualification_lock();
    let workspace = required("MOTOR_MACOS_QUALIFICATION_WORKSPACE");
    let name = format!("motor-return-{}.bin", uuid::Uuid::now_v7());
    let path = Path::new(&workspace).join(&name);
    assert!(!path.exists(), "artifact must exist only in the VM share");
    let evidence = evidence_job("returned-file");
    let output = job_at_with_returns(
        &format!("printf returned-from-guest > '{name}'; printf output-before-returns"),
        &[],
        &workspace,
        &evidence,
        None,
        &[path.to_str().unwrap()],
    );
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert_eq!(std::fs::read(&path).unwrap(), b"returned-from-guest");
    std::fs::remove_file(&path).unwrap();
    let records = std::fs::read_to_string(evidence.join("records.jsonl")).unwrap();
    let rows: Vec<serde_json::Value> = records
        .lines()
        .map(|line| serde_json::from_str(line).unwrap())
        .collect();
    let position = |kind| rows.iter().position(|row| row["type"] == kind).unwrap();
    assert!(position("output") < position("return_chunk"));
    assert!(position("return_chunk") < position("return_manifest"));
    assert!(position("return_manifest") < position("terminal"));
    clean();
}
#[test]
#[ignore = "operator: cold Cargo/Bun caches and locked sibling/platform-package fixture"]
fn macos_qualification_bun() {
    let _lane = qualification_lock();
    required("MOTOR_MACOS_QUALIFICATION_SIBLING");
    successful("set -eu; git ls-files --error-unmatch bun.lock Cargo.lock; shasum -a 256 bun.lock Cargo.lock > \"$TMPDIR/locks.before\"; cat \"$TMPDIR/locks.before\"; bun install --offline --frozen-lockfile --ignore-scripts; bun verify.js; cargo test --locked --offline; shasum -a 256 bun.lock Cargo.lock > \"$TMPDIR/locks.after\"; cat \"$TMPDIR/locks.after\"; cmp \"$TMPDIR/locks.before\" \"$TMPDIR/locks.after\"; git diff --exit-code -- bun.lock Cargo.lock");
    clean();
}

#[test]
#[ignore = "operator: generated private fixtures, isolated client Cargo cache, host prep network capture"]
fn macos_qualification_private_bundle() {
    let _lane = qualification_lock();
    let good = required("MOTOR_MACOS_QUALIFICATION_PRIVATE_GOOD");
    let missing = required("MOTOR_MACOS_QUALIFICATION_PRIVATE_MISSING");
    let url = required("MOTOR_MACOS_QUALIFICATION_PRIVATE_URL");
    let positive = job_at(
        "cargo test --locked --offline",
        &[],
        &good,
        &evidence_job("private-good"),
        None,
    );
    assert!(
        positive.status.success(),
        "{}",
        String::from_utf8_lossy(&positive.stderr)
    );
    let evidence = evidence_job("private-missing");
    let negative = job_at(
        "cargo test --locked --offline",
        &[],
        &missing,
        &evidence,
        None,
    );
    assert!(
        negative.status.code() == Some(200),
        "missing private commit must be refused before execution"
    );
    let records = std::fs::read_to_string(evidence.join("records.jsonl")).unwrap();
    assert!(
        !records.lines().any(
            |line| serde_json::from_str::<serde_json::Value>(line).unwrap()["type"] == "started"
        ),
        "negative fixture reached guest execution"
    );
    let diagnostics = String::from_utf8_lossy(&negative.stderr);
    assert!(
        diagnostics.contains(&url),
        "missing dependency refusal must name its source: {diagnostics}"
    );
    clean();
}

// Identity and SIGKILL checks observe the installed daemon's helper/service.
// The gate primitive drives that exact signed helper directly, so daemon
// escalation cannot mask a failure of EOF itself to stop the VM.
#[derive(serde::Deserialize, serde::Serialize, Clone, PartialEq, Eq)]
struct HelperRecord {
    id: uuid::Uuid,
    helper: Option<motor_macvm::ProcessIdentity>,
    service: Option<motor_macvm::ProcessIdentity>,
    gate_open: bool,
    paths: HelperPaths,
}
#[derive(serde::Deserialize, serde::Serialize, Clone, PartialEq, Eq)]
struct HelperPaths {
    clone: PathBuf,
    nonce: PathBuf,
    tree: PathBuf,
    cargo: PathBuf,
    prep: PathBuf,
    target_tmp: PathBuf,
    quarantine: PathBuf,
}
impl HelperPaths {
    fn all(&self) -> [&Path; 7] {
        [
            &self.clone,
            &self.nonce,
            &self.tree,
            &self.cargo,
            &self.prep,
            &self.target_tmp,
            &self.quarantine,
        ]
    }
}
static OBSERVED: std::sync::Mutex<Vec<HelperRecord>> = std::sync::Mutex::new(Vec::new());
fn process_arguments(identity: motor_macvm::ProcessIdentity) -> std::io::Result<Vec<Vec<u8>>> {
    if motor_macvm::process_identity(identity.pid)? != identity {
        return Err(std::io::Error::other(
            "process identity changed before argv read",
        ));
    }
    let mut mib = [libc::CTL_KERN, libc::KERN_PROCARGS2, identity.pid];
    let mut bytes = vec![0u8; 1024 * 1024];
    let mut size = bytes.len();
    if unsafe {
        libc::sysctl(
            mib.as_mut_ptr(),
            3,
            bytes.as_mut_ptr().cast(),
            &mut size,
            std::ptr::null_mut(),
            0,
        )
    } != 0
    {
        return Err(std::io::Error::last_os_error());
    }
    bytes.truncate(size);
    if motor_macvm::process_identity(identity.pid)? != identity {
        return Err(std::io::Error::other(
            "process identity changed during argv read",
        ));
    }
    support::procargs(&bytes)
}
fn helper_records() -> Vec<HelperRecord> {
    let root = PathBuf::from(required("MOTOR_MACOS_QUALIFICATION_STATE_ROOT"));
    let _ = std::fs::read_dir(&root).expect("readable qualification state root");
    let records: Vec<HelperRecord> = support::entries(&root.join("macvm-records"))
        .unwrap()
        .into_iter()
        .filter(|path| support::final_record(path))
        .filter_map(|path| match std::fs::read(&path) {
            Ok(bytes) => Some(serde_json::from_slice(&bytes).expect("valid durable helper record")),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => None,
            Err(error) => panic!("{}: {error}", path.display()),
        })
        .collect();
    let mut observed = OBSERVED.lock().unwrap();
    for record in &records {
        if !observed.contains(record) {
            use std::io::Write;
            let path = PathBuf::from(required("MOTOR_MACOS_QUALIFICATION_EVIDENCE"))
                .join("helper-records.jsonl");
            let mut file = std::fs::OpenOptions::new()
                .create(true)
                .append(true)
                .open(path)
                .unwrap();
            writeln!(file, "{}", serde_json::to_string(record).unwrap()).unwrap();
            if let Some(helper) = record.helper {
                match process_arguments(helper) {
                    Ok(args) => {
                        assert_eq!(args.get(1).map(Vec::as_slice), Some(b"job".as_slice()));
                        let request: motor_macvm::JobVmRequest =
                            serde_json::from_slice(args.get(2).unwrap()).unwrap();
                        assert_eq!(request.clone, record.paths.clone);
                        assert_eq!(request.nonce_disk, record.paths.nonce);
                        assert_eq!(request.cargo_home.host, record.paths.cargo);
                        for share in std::iter::once(&request.workspace).chain(&request.siblings) {
                            assert_eq!(
                                share.host,
                                record
                                    .paths
                                    .tree
                                    .join(share.guest.strip_prefix("/").unwrap_or(&share.guest))
                            );
                        }
                        let path = PathBuf::from(required("MOTOR_MACOS_QUALIFICATION_EVIDENCE"))
                            .join(format!("{}.vm-request.json", record.id));
                        std::fs::write(path, serde_json::to_vec_pretty(&request).unwrap()).unwrap();
                    }
                    Err(error) => assert!(
                        !motor_macvm::process::identity_alive(helper).unwrap(),
                        "live helper argv unavailable: {error}"
                    ),
                }
            }
            observed.push(record.clone());
        }
    }
    records
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
            sibling_paths: false,
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
    let evidence = evidence_job("helper");
    let workspace = required("MOTOR_MACOS_QUALIFICATION_WORKSPACE");
    let worker_evidence = evidence.clone();
    let worker =
        std::thread::spawn(move || job_at("sleep 30", &[], &workspace, &worker_evidence, None));
    let deadline = Instant::now() + Duration::from_secs(120);
    let mut candidate = None;
    let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        loop {
            if let Some(record) = helper_records()
                .into_iter()
                .find(|r| Some(r.id) == accepted_id(&evidence))
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
        eprintln!(
            "SIGKILL at {:?}: helper={helper:?} service={service:?}",
            std::time::SystemTime::now()
        );
        while motor_macvm::process::identity_alive(helper).unwrap()
            || motor_macvm::process::identity_alive(service).unwrap()
        {
            assert!(
                now.elapsed() < Duration::from_secs(2),
                "helper SIGKILL left a VM service alive"
            );
            std::thread::park_timeout(Duration::from_millis(5));
        }
        eprintln!("helper and exact service absent after {:?}", now.elapsed());
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
    host("/bin/ps", &["-axo", "user=,pid=,lstart=,comm="])
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
        let daemon = motor_macvm::process_identity(parent.trim().parse().unwrap()).unwrap();
        let arguments = process_arguments(daemon).unwrap();
        let config_path =
            std::fs::canonicalize(support::vm_config_argument(&arguments).unwrap()).unwrap();
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
        eprintln!(
            "gate EOF at {:?}: reason={reason:?} helper exit after {:?}; reports={:?}",
            std::time::SystemTime::now(),
            closed.elapsed(),
            self.reports
        );
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
