use motor_protocol::{BytePayload, Frame, Outcome};
use motor_vm_helper::{
    job::Start,
    linux::{cgroup, fs as helper_fs, tenant},
};
use serde_json::json;
use sha2::{Digest, Sha256};
use std::{
    fs::{self, File, OpenOptions},
    io::{Read, Write},
    os::{
        fd::AsRawFd,
        unix::{fs::PermissionsExt, net::UnixStream, process::CommandExt},
    },
    path::{Path, PathBuf},
    process::{Child, Command, Stdio},
    sync::mpsc,
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};

const PROBE: &str = include_str!("../../../../script/runner/job-sandbox-probe.py");
const LIMIT: Duration = Duration::from_secs(180);

#[cfg(feature = "job-cache-vm-tests")]
mod cache_write;

enum Probe {
    Sandbox,
    Fill,
    #[cfg(feature = "job-cache-vm-tests")]
    Cache(std::rc::Rc<cache_write::Prepared>),
}

fn artifact(name: &str) -> PathBuf {
    let path = PathBuf::from(std::env::var_os(name).unwrap_or_else(|| panic!("missing {name}")));
    assert!(
        path.is_absolute(),
        "{name} must be absolute: {}",
        path.display()
    );
    let file = File::open(&path).unwrap_or_else(|error| panic!("unreadable {name}: {error}"));
    assert!(
        file.metadata().unwrap().is_file(),
        "{name} must name a regular file"
    );
    path
}

fn command(command: &mut Command) -> String {
    let output = command
        .output()
        .unwrap_or_else(|e| panic!("{command:?}: {e}"));
    assert!(
        output.status.success(),
        "{command:?}: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    String::from_utf8(output.stdout).unwrap().trim().to_owned()
}

fn wait_for(mut ready: impl FnMut() -> bool) {
    let deadline = Instant::now() + Duration::from_secs(15);
    while !ready() {
        assert!(Instant::now() < deadline, "VM fixture setup deadline");
        std::thread::sleep(Duration::from_millis(50));
    }
}

struct Resources {
    scratch: Option<tempfile::TempDir>,
    group: PathBuf,
    mounts: Vec<PathBuf>,
    children: Vec<Child>,
    watchdog: Option<(mpsc::Sender<()>, std::thread::JoinHandle<()>)>,
}

impl Resources {
    fn cleanup(&mut self) -> anyhow::Result<()> {
        if let Some((stop, thread)) = self.watchdog.take() {
            let _ = stop.send(());
            thread.join().expect("VM deadline watchdog");
        }
        if self.group.exists() {
            fs::write(self.group.join("cgroup.kill"), "1")?;
        }
        for child in &mut self.children {
            let pid = child.id();
            child.wait()?;
            anyhow::ensure!(
                !Path::new(&format!("/proc/{pid}/ns/net")).exists(),
                "namespace keeper or VMM survived cleanup"
            );
        }
        self.children.clear();
        while let Some(path) = self.mounts.last() {
            helper_fs::unmount(path)?;
            self.mounts.pop();
        }
        if self.group.exists() {
            fs::remove_dir(&self.group)?;
        }
        anyhow::ensure!(!self.group.exists(), "test cgroup survived cleanup");
        if let Some(scratch) = self.scratch.take() {
            let root = scratch.path().to_owned();
            anyhow::ensure!(
                !fs::read_to_string("/proc/self/mountinfo")?
                    .lines()
                    .any(|line| line
                        .split_whitespace()
                        .nth(4)
                        .is_some_and(|p| p.starts_with(root.to_str().unwrap()))),
                "test mount survived cleanup"
            );
            scratch.close()?;
            anyhow::ensure!(!root.exists(), "test jail/disks survived cleanup");
        }
        Ok(())
    }

    fn spawn(&mut self, command: &mut Command) {
        let procs = OpenOptions::new()
            .write(true)
            .open(self.group.join("cgroup.procs"))
            .unwrap();
        // The descriptor is opened before fork; the child performs only write(2).
        unsafe {
            command.pre_exec(move || {
                if libc::write(procs.as_raw_fd(), b"0".as_ptr().cast(), 1) != 1 {
                    return Err(std::io::Error::last_os_error());
                }
                Ok(())
            });
        }
        self.children.push(command.spawn().unwrap());
    }
}

impl Drop for Resources {
    fn drop(&mut self) {
        if let Err(error) = self.cleanup() {
            eprintln!("job contract fixture cleanup failed: {error:#}");
        }
    }
}

fn relay(root: &Path, port: u32) -> UnixStream {
    let deadline = Instant::now() + Duration::from_secs(60);
    loop {
        let attempt = || -> std::io::Result<UnixStream> {
            let mut socket = UnixStream::connect(root.join("vsock.sock"))?;
            socket.set_read_timeout(Some(Duration::from_secs(2)))?;
            socket.set_write_timeout(Some(Duration::from_secs(2)))?;
            writeln!(socket, "CONNECT {port}")?;
            // Do not buffer past the handshake: the guest may immediately
            // send a boot-refusal message on the same connection.
            let mut answer = Vec::new();
            loop {
                let mut byte = [0];
                socket.read_exact(&mut byte)?;
                answer.push(byte[0]);
                if byte[0] == b'\n' || answer.len() == 128 {
                    break;
                }
            }
            if !answer.starts_with(b"OK ") || !answer.ends_with(b"\n") {
                return Err(std::io::Error::other(format!(
                    "vsock port {port}: {answer:?}"
                )));
            }
            socket.set_read_timeout(None)?;
            socket.set_write_timeout(None)?;
            Ok(socket)
        };
        match attempt() {
            Ok(socket) => return socket,
            Err(error) => assert!(
                Instant::now() < deadline,
                "guest port {port} unavailable: {error}"
            ),
        }
        std::thread::sleep(Duration::from_millis(100));
    }
}

fn git(path: &Path, args: &[&str]) -> String {
    command(
        Command::new("git")
            .current_dir(path)
            .env("GIT_CONFIG_NOSYSTEM", "1")
            .args(args),
    )
}

async fn run_probe(root: &Path, scratch: &Path, id: uuid::Uuid, source: &str, verdict: &str) {
    let repo = scratch.join("repo");
    fs::create_dir(&repo).unwrap();
    git(&repo, &["init", "--quiet"]);
    fs::write(repo.join("probe.py"), source).unwrap();
    git(&repo, &["add", "probe.py"]);
    git(
        &repo,
        &[
            "-c",
            "user.name=Contract",
            "-c",
            "user.email=contract@example.invalid",
            "-c",
            "commit.gpgsign=false",
            "commit",
            "--quiet",
            "-m",
            "contract probe",
        ],
    );
    let commit = git(&repo, &["rev-parse", "HEAD"]);
    let tree = git(&repo, &["rev-parse", "HEAD^{tree}"]);
    let reference = format!("refs/ck-motor/{id}/snapshot");
    git(&repo, &["update-ref", &reference, &commit]);
    let bundle = scratch.join("probe.bundle");
    git(
        &repo,
        &["bundle", "create", bundle.to_str().unwrap(), &reference],
    );
    let bytes = fs::read(bundle).unwrap();
    let socket = relay(root, 52);
    socket.set_nonblocking(true).unwrap();
    let mut socket = tokio::net::UnixStream::from_std(socket).unwrap();
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_secs();
    let lease = json!({"type":"verified_lease", "jti":id, "purpose":"run", "sub":"acct:01ARZ3NDEKTSV4RRFFQ69G5FAV", "vcpus":2, "mem_mib":4096, "platform":"linux-x64", "net":false, "max_s":60, "exp":now+120});
    let write = motor_protocol::write_frame_async;
    write(&mut socket, &serde_json::from_value(lease).unwrap())
        .await
        .unwrap();
    write(&mut socket, &serde_json::from_value(json!({"type":"hello", "protocol_version":1, "started":true, "default_branch_tip":true})).unwrap()).await.unwrap();
    assert!(matches!(next(&mut socket).await, Frame::Hello { .. }));
    let workspace = "/home/motor-contract";
    let request = json!({"type":"run", "job_id":id, "workspace_key":workspace, "repository_root":workspace, "cwd":workspace,
        "command":"python3 -c 'import os; print(\"fixed cwd hex: \" + os.getcwd().encode().hex())' && python3 probe.py", "env":{}, "weight_hint":2, "timeout":60, "queue_wait_limit_s":10,
        "snapshot_commit":commit, "integration_tip":commit, "tree_hash":tree, "siblings":[],
        "bundles":[{"index":0, "repository":workspace, "length":bytes.len(), "sha256":format!("{:x}", Sha256::digest(&bytes))}]});
    write(&mut socket, &serde_json::from_value(request).unwrap())
        .await
        .unwrap();
    let admitted = next(&mut socket).await;
    assert!(
        matches!(admitted, Frame::Accepted(_)),
        "run not accepted: {admitted:?}"
    );
    write(
        &mut socket,
        &Frame::BundleChunk {
            index: 0,
            offset: 0,
            data: BytePayload(bytes),
        },
    )
    .await
    .unwrap();
    let mut output = String::new();
    let mut started = false;
    loop {
        match next(&mut socket).await {
            Frame::Started { .. } => started = true,
            Frame::Output(chunk) => output.push_str(&String::from_utf8_lossy(&chunk.bytes.0)),
            Frame::Terminal(terminal) => {
                assert!(started, "probe did not start: {terminal:?}; {output}");
                assert_eq!(terminal.outcome, Outcome::Exit { code: 0 }, "{output}");
                assert!(output.contains(verdict), "missing probe verdict: {output}");
                let fixed = format!(
                    "/w/acct-01ARZ3NDEKTSV4RRFFQ69G5FAV/{:x}/motor-contract",
                    Sha256::digest(workspace.as_bytes())
                );
                // Encoding keeps diagnostic path rewriting from hiding the actual cwd.
                let encoded: String = fixed.bytes().map(|byte| format!("{byte:02x}")).collect();
                assert!(
                    output.contains(&format!("fixed cwd hex: {encoded}")),
                    "missing fixed checkout path: {output}"
                );
                println!("real job VM contract: {output}");
                let refused = crate::helper::exchange_control(
                    relay(root, 54),
                    "contract",
                    &json!({"op":"fill","source":{"repository_root":workspace,"integration_tip":commit,"bundles":[]}}),
                    Duration::from_secs(10),
                ).unwrap();
                assert_eq!(refused["ok"], false);
                assert!(
                    refused["error"].as_str().unwrap().contains("boot kind"),
                    "{refused}"
                );
                let mut export = relay(root, 54);
                export
                    .set_read_timeout(Some(Duration::from_secs(30)))
                    .unwrap();
                crate::helper::write_json(&mut export, &json!({"op":"export_tip","job_id":id,"max_bytes":motor_protocol::FILL_SOURCE_MAX_BYTES})).unwrap();
                let reply = crate::helper::read_json(&mut export).unwrap();
                assert_eq!(reply["ok"], true, "{reply}");
                let source: motor_protocol::FillSource =
                    serde_json::from_value(reply["source"].clone()).unwrap();
                assert_eq!(source.repository_root, workspace);
                assert_eq!(source.integration_tip, commit);
                let mut incoming =
                    crate::jobs::fill_source::Incoming::new(scratch.join("exported-tip"), source)
                        .unwrap();
                while !incoming.complete() {
                    incoming
                        .chunk(
                            motor_protocol::read_frame(&mut export)
                                .unwrap()
                                .expect("complete exported tip"),
                        )
                        .unwrap();
                }
                write(&mut socket, &Frame::TerminalStored { jti: id })
                    .await
                    .unwrap();
                return;
            }
            other => panic!("unexpected probe frame: {other:?}; {output}"),
        }
    }
}

async fn next(socket: &mut tokio::net::UnixStream) -> Frame {
    loop {
        let frame = motor_protocol::read_frame_async(socket)
            .await
            .unwrap()
            .expect("guest runner EOF");
        if !matches!(frame, Frame::Heartbeat { .. }) {
            return frame;
        }
    }
}

/// Root-only qualification of docs/job-isolation.md 4.4.2 in one real job VM.
/// Set MOTOR_VM_TEST_FIRECRACKER, MOTOR_VM_TEST_JAILER and
/// MOTOR_VM_TEST_JOB_{KERNEL,ROOTFS,TOOLCHAIN} to the absolute kernel,
/// rootfs.ext4 and toolchain.ext4 emitted by script/tenant-image/build.py build
/// under /var/lib/motor-vm/job-image-staging/<generation>. No image edits or
/// separately provisioned argument files are needed.
/// Uses the helper's production boot requests and the image's production guest
/// agent/runner. A disconnected TAP exists only in a private network namespace
/// because guest attach requires eth0; this does not qualify network enforcement.
/// No live helper, registry, fleet slice or host network rules are modified.
#[test]
#[ignore = "requires root, KVM, cgroup v2, iproute2, jailer/Firecracker 1.17.0 and job image artifacts"]
fn isolated_job_vm_preserves_full_sandbox_contract() {
    isolated_vm(Probe::Sandbox);
}

#[test]
#[ignore = "requires root, KVM, cgroup v2, iproute2, jailer/Firecracker 1.17.0 and job image artifacts"]
fn isolated_fill_vm_refuses_run_admission() {
    isolated_vm(Probe::Fill);
}

fn isolated_vm(probe: Probe) {
    let fill = matches!(probe, Probe::Fill);
    assert_eq!(unsafe { libc::geteuid() }, 0, "run as the root operator");
    let firecracker = artifact("MOTOR_VM_TEST_FIRECRACKER");
    let jailer = artifact("MOTOR_VM_TEST_JAILER");
    let inputs = [
        ("kernel", artifact("MOTOR_VM_TEST_JOB_KERNEL")),
        ("rootfs.ext4", artifact("MOTOR_VM_TEST_JOB_ROOTFS")),
        ("toolchain.ext4", artifact("MOTOR_VM_TEST_JOB_TOOLCHAIN")),
    ];
    OpenOptions::new()
        .read(true)
        .write(true)
        .open("/dev/kvm")
        .expect("read/write /dev/kvm required");
    assert!(
        Path::new("/sys/fs/cgroup/cgroup.controllers").exists(),
        "cgroup v2 required"
    );
    let scratch = tempfile::Builder::new()
        .prefix("job-contract-")
        .tempdir_in(cortexkit_test_support::scratch_root())
        .unwrap();
    let base = scratch.path().to_owned();
    // Unix sockets use descriptor aliases below, but mountinfo checks need an
    // unescaped absolute path to verify that all our mounts have disappeared.
    assert!(!base.to_str().unwrap().chars().any(char::is_whitespace));
    let id = match &probe {
        #[cfg(feature = "job-cache-vm-tests")]
        Probe::Cache(prepared) => prepared.id,
        _ => uuid::Uuid::now_v7(),
    };
    let mut resources = Resources {
        scratch: Some(scratch),
        group: PathBuf::from(format!("/sys/fs/cgroup/motor-contract-{id}")),
        mounts: Vec::new(),
        children: Vec::new(),
        watchdog: None,
    };
    let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        cgroup::create_job(&resources.group, 2).expect("private CPU-controlled cgroup required");
        fs::write(resources.group.join("memory.max"), (4u64 << 30).to_string()).unwrap();
        fs::write(resources.group.join("memory.swap.max"), "0").unwrap();
        let (send, receive) = mpsc::channel();
        let group = resources.group.clone();
        resources.watchdog = Some((
            send,
            std::thread::spawn(move || {
                if receive.recv_timeout(LIMIT) == Err(mpsc::RecvTimeoutError::Timeout) {
                    // Bound even a blocking Firecracker API or guest-agent call.
                    fs::write(group.join("cgroup.kill"), "1")
                        .expect("deadline must kill test VM and namespace keeper");
                }
            }),
        ));
        fs::set_permissions(&base, fs::Permissions::from_mode(0o755)).unwrap();
        resources.spawn(Command::new("unshare").args(["--net", "--", "sleep", "180"]));
        let keeper = resources.children[0].id();
        let netns = PathBuf::from(format!("/proc/{keeper}/ns/net"));
        let original = fs::read_link("/proc/self/ns/net").unwrap();
        wait_for(|| fs::read_link(&netns).is_ok_and(|ns| ns != original));
        let tap = (rand::random::<u8>() % 254) + 1;
        let tap_name = format!("mc{}", &id.simple().to_string()[20..]);
        command(
            Command::new("nsenter")
                .arg(format!("--net={}", netns.display()))
                .args(["ip", "tuntap", "add", "dev", &tap_name, "mode", "tap"]),
        );
        command(
            Command::new("nsenter")
                .arg(format!("--net={}", netns.display()))
                .args(["ip", "link", "set", &tap_name, "up"]),
        );
        let uid = 900_000 + u32::from(rand::random::<u16>());
        assert!(
            unsafe { libc::getpwuid(uid) }.is_null(),
            "test jail uid must not name a host account"
        );
        let boundary = base.join("jail");
        let root = boundary.join(format!("firecracker/job-{id}/root"));
        fs::create_dir_all(&root).unwrap();
        helper_fs::chown(&File::open(&root).unwrap(), uid).unwrap();
        for (name, input) in &inputs {
            let target = root.join(name);
            File::create(&target).unwrap();
            // Record before mounting so failure during read-only remount is
            // still cleaned up by the same teardown path.
            resources.mounts.push(target.clone());
            helper_fs::bind_readonly(&File::open(input).unwrap(), &target).unwrap();
        }
        let workspace = root.join("workspace.img");
        File::create(&workspace)
            .unwrap()
            .set_len(4u64 << 30)
            .unwrap();
        helper_fs::chown(&File::open(&workspace).unwrap(), uid).unwrap();
        fs::set_permissions(&workspace, fs::Permissions::from_mode(0o600)).unwrap();
        let binaries = base.join("bin");
        fs::create_dir(&binaries).unwrap();
        let binary = binaries.join("firecracker");
        fs::copy(&firecracker, &binary).unwrap();
        fs::set_permissions(&binary, fs::Permissions::from_mode(0o755)).unwrap();
        let console = File::create(base.join("console.log")).unwrap();
        resources.spawn(
            Command::new(&jailer)
                .args(["--id", &format!("job-{id}"), "--exec-file"])
                .arg(&binary)
                .args([
                    "--uid",
                    &uid.to_string(),
                    "--gid",
                    &uid.to_string(),
                    "--chroot-base-dir",
                ])
                .arg(&boundary)
                .arg("--netns")
                .arg(&netns)
                .args(["--", "--api-sock", "/api.sock"])
                .env_clear()
                .stdin(Stdio::null())
                .stdout(console.try_clone().unwrap())
                .stderr(console),
        );
        let root_fd = File::open(&root).unwrap();
        let short_root = PathBuf::from(format!("/proc/self/fd/{}", root_fd.as_raw_fd()));
        wait_for(|| short_root.join("api.sock").exists());
        let start = Start {
            cache_write_blocked: false,
            kind: if fill {
                motor_vm_helper::job::VmKind::Fill
            } else {
                Default::default()
            },
            job_id: id.to_string(),
            account: "acct:01ARZ3NDEKTSV4RRFFQ69G5FAV".into(),
            workspace_disk_id: "contract".into(),
            image_generation_id: "contract".into(),
            vcpus: 2,
            memory_mib: 3072,
            burst_mib: None,
        };
        let registry = motor_vm_helper::job::Workspace {
            workspace_key_hash: None,
            account: start.account.clone(),
            workspace_disk_id: start.workspace_disk_id.clone(),
            role: if fill {
                motor_vm_helper::job::WorkspaceRole::Fill
            } else {
                motor_vm_helper::job::WorkspaceRole::Job
            },
        };
        let settings = motor_vm_helper::config::JobVms {
            uid_base: uid,
            uid_count: 1,
            memory_max_bytes: 4 << 30,
            workspace_disk_max_bytes: Some(4 << 30),
            fixed_path_repositories: std::collections::BTreeMap::from([(
                start.account.clone(),
                vec!["/home/motor-contract".into()],
            )]),
        };
        let requests = motor_vm_helper::linux::job::recorded_boot_requests(
            &start,
            Some(tap),
            None,
            File::open(&workspace).unwrap().metadata().unwrap().len(),
            &registry,
            &settings,
        )
        .unwrap();
        #[cfg(feature = "job-cache-vm-tests")]
        let requests = if let Probe::Cache(prepared) = &probe {
            let (disk, requests) = prepared.boot(&start, &root, uid, tap);
            resources.mounts.push(workspace.clone());
            command(
                Command::new("mount")
                    .arg("--bind")
                    .arg(format!(
                        "/proc/{}/fd/{}",
                        std::process::id(),
                        disk.as_raw_fd()
                    ))
                    .arg(&workspace),
            );
            helper_fs::chown(&disk, uid).unwrap();
            requests
        } else {
            requests
        };
        for (path, mut body) in requests {
            // Only the host-side device name is fixture-specific; the guest
            // interface and every boot/drive/vsock request stay production's.
            if path == "/network-interfaces/eth0" {
                body["host_dev_name"] = json!(tap_name);
            }
            tenant::api(&short_root, "PUT", path, &body).unwrap();
        }
        let init = motor_protocol::TenantAgentRequest::AttachTenant {
            generation: 1,
            recovery_required: false,
            recovery_budget_ms: None,
            unix_ms: SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_millis() as u64,
            seed: motor_lease::base64url(&rand::random::<[u8; 32]>()),
            address: format!("10.200.{tap}.2/30"),
            gateway: format!("10.200.{tap}.1"),
            resolvers: vec!["1.1.1.1".into()],
            hostname: "motor-contract".into(),
            machine_id: id.simple().to_string(),
        };
        let reply = crate::helper::exchange_control(
            relay(&short_root, 54),
            "contract",
            &serde_json::to_value(init).unwrap(),
            Duration::from_secs(60),
        )
        .unwrap();
        assert_eq!(
            reply["ok"], true,
            "production guest initialization: {reply}; production boot arguments must install root-owned /run/motor/runner-args (0600) and job artifacts must provide /toolchain/usr/local/lib/motor/cargo/{{bin,registry,git}}; check MOTOR_VM_TEST_JOB_ROOTFS and MOTOR_VM_TEST_JOB_TOOLCHAIN"
        );
        tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap()
            .block_on(async {
                let probe = async {
                    match &probe {
                        Probe::Fill => refuse_run_probe(&short_root, id).await,
                        #[cfg(feature = "job-cache-vm-tests")]
                        Probe::Cache(prepared) => {
                            run_probe(&short_root, &base, id, &prepared.probe(), "PASS job cache:")
                                .await;
                        }
                        _ => run_probe(&short_root, &base, id, PROBE, "PASS job sandbox:").await,
                    }
                };
                tokio::time::timeout(Duration::from_secs(75), probe)
                    .await
                    .expect("job contract deadline");
            });
    }));
    if result.is_err() {
        if let Ok(console) = fs::read_to_string(base.join("console.log")) {
            eprintln!("VM console: {console}");
        }
    }
    let cleanup = resources.cleanup();
    cleanup.expect("all VM, namespace/TAP, cgroup, mounts and scratch resources must be removed");
    if let Err(error) = result {
        std::panic::resume_unwind(error);
    }
}

async fn refuse_run_probe(root: &Path, id: uuid::Uuid) {
    let socket = relay(root, 52);
    socket.set_nonblocking(true).unwrap();
    let mut socket = tokio::net::UnixStream::from_std(socket).unwrap();
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_secs();
    // A Run that would be admitted on a Job VM (its weight matches the lease's
    // vCPUs), so the refusal below can only come from the Fill boot kind.
    let lease = json!({"type":"verified_lease","jti":id,"purpose":"run","sub":"acct:01ARZ3NDEKTSV4RRFFQ69G5FAV","vcpus":2,"mem_mib":4096,"platform":"linux-x64","net":false,"max_s":60,"exp":now+120});
    motor_protocol::write_frame_async(&mut socket, &serde_json::from_value(lease).unwrap())
        .await
        .unwrap();
    motor_protocol::write_frame_async(
        &mut socket,
        &serde_json::from_value(json!({"type":"hello","protocol_version":1,"started":true}))
            .unwrap(),
    )
    .await
    .unwrap();
    assert!(matches!(next(&mut socket).await, Frame::Hello { .. }));
    let run = json!({"type":"run","job_id":id,"workspace_key":"/home/motor-contract","repository_root":"/home/motor-contract","cwd":"/home/motor-contract","command":"true","env":{},"weight_hint":2,"timeout":60,"queue_wait_limit_s":10,"snapshot_commit":"a".repeat(40),"integration_tip":"a".repeat(40),"tree_hash":"b".repeat(40),"siblings":[],"bundles":[]});
    motor_protocol::write_frame_async(&mut socket, &serde_json::from_value(run).unwrap())
        .await
        .unwrap();
    let Frame::Refusal(refusal) = next(&mut socket).await else {
        panic!("Fill VM admitted a Run")
    };
    assert!(
        refusal
            .detail
            .as_deref()
            .unwrap()
            .contains("Fill VM refuses job admission"),
        "{refusal:?}"
    );
}

#[test]
fn full_contract_probe_rejects_incomplete_or_weakened_observations() {
    let checks = r#"
import copy
import sys
ns = {'__name__': 'contract_checks'}
exec(sys.stdin.read(), ns)
path = '/sys/fs/cgroup/motor.slice/motor-jobs.slice/motor-job-01234567-89ab-cdef-0123-456789abcdef.service'
root = dict(uid=0, gid=0, mode=0o755)
job = dict(uid=123, gid=456, mode=0o644)
good = dict(
    cgroup='0::' + path.removeprefix('/sys/fs/cgroup') + '\n',
    directories={p: dict(root) for p in (path, path.rsplit('/', 1)[0], path.rsplit('/', 2)[0])},
    files={'cgroup.procs': dict(root), 'memory.max': dict(root), 'memory.pressure': dict(job)},
    identity=dict(name='motor-job', uid=123, euid=123, gid=456, egid=456, account_uid=123, account_gid=456),
    system_pressure='some avg10=0.00', job_pressure='some avg10=0.00', pressure_write=dict(errno=30),
    pid1=dict(comm='bash', uids=[123]*4, gids=[456]*4))
def judge(value):
    return ns['judge'](ns['json'].dumps(value))
judge(good)
root_pressure = copy.deepcopy(good)
root_pressure['files']['memory.pressure'] = dict(uid=0, gid=0, mode=0o644)
judge(root_pressure)
cases = []
for key in good:
    value = copy.deepcopy(good)
    del value[key]
    cases.append(value)
for bad in ('0::/motor.slice/motor-jobs.slice/motor-job-not-a-uuid.service\n',
            good['cgroup'].replace('motor-jobs.slice', 'wrong.slice'),
            good['cgroup'].replace('motor-job-', 'other-'),
            good['cgroup'].replace('.service', '.scope'),
            good['cgroup'].replace('abcdef', 'ABCDEF'), good['cgroup'] + '0::/extra\n'):
    value = copy.deepcopy(good)
    value['cgroup'] = bad
    cases.append(value)
for directory in good['directories']:
    value = copy.deepcopy(good)
    value['directories'][directory]['uid'] = 123
    cases.append(value)
for field, bad in (('mode', 0o666), ('uid', 789), ('gid', 789)):
    value = copy.deepcopy(good)
    value['files']['memory.pressure'][field] = bad
    cases.append(value)
for change in ('control-owner', 'pressure-absent', 'writable', 'wrong-errno', 'pid1', 'pid1-owner', 'account', 'euid'):
    value = copy.deepcopy(good)
    if change == 'control-owner': value['files']['memory.max']['uid'] = 123
    if change == 'pressure-absent': del value['files']['memory.pressure']
    if change == 'writable': value['pressure_write'] = dict(wrote=True)
    if change == 'wrong-errno': value['pressure_write'] = dict(errno=13)
    if change == 'pid1': value['pid1']['comm'] = 'systemd'
    if change == 'pid1-owner': value['pid1']['uids'] = [0]*4
    if change == 'account': value['identity']['name'] = 'root'
    if change == 'euid': value['identity']['euid'] = 0
    cases.append(value)
for value in cases:
    try:
        judge(value)
    except RuntimeError:
        continue
    raise AssertionError('weakened observation accepted: ' + repr(value))
print(sys.version.split()[0] + ': 2 valid and ' + str(len(cases)) + ' rejected contract observations')
"#;
    let mut child = Command::new("python3")
        .args(["-c", checks])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("python3 required for the shared contract probe");
    child
        .stdin
        .take()
        .unwrap()
        .write_all(PROBE.as_bytes())
        .unwrap();
    let output = child.wait_with_output().unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    println!("{}", String::from_utf8(output.stdout).unwrap());
}
