//! Operator lane: run on the VM host, with ck configured for the installed daemon.
use std::{
    path::PathBuf,
    process::{Command, Output},
    time::{Duration, Instant},
};

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
disks=0
for disk in /dev/rdisk*; do
  name=${{disk#/dev/rdisk}}
  case "$name" in ''|*[!0-9]*) continue;; esac
  disks=$((disks+1))
  if dd if="$disk" of=/dev/null bs=1 count=1 2>/dev/null; then exit 1; fi
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
    required("MOTOR_MACOS_QUALIFICATION_SIBLING");
    successful("set -eu; git diff --exit-code -- bun.lock Cargo.lock; bun install --offline --frozen-lockfile --ignore-scripts; cargo test --offline; git diff --exit-code -- bun.lock Cargo.lock");
    clean();
}
