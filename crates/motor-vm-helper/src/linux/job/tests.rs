use super::*;
use std::sync::atomic::{AtomicU64, Ordering};

mod exit_tests;

fn start() -> Start {
    Start {
        job_id: "00000000-0000-4000-8000-000000000001".into(),
        account: "account-1".into(),
        workspace_disk_id: "disk-1".into(),
        image_generation_id: "generation-1".into(),
        vcpus: 7,
        memory_mib: 1536,
        burst_mib: Some(512),
    }
}

fn record() -> Record {
    Record {
        start: start(),
        uid: unsafe { libc::geteuid() },
        disk_device: 8,
        disk_inode: 99,
        released: false,
        oom_kill: None,
        exit_acknowledged: false,
    }
}

fn scratch() -> PathBuf {
    static NEXT: AtomicU64 = AtomicU64::new(0);
    let path = cortexkit_test_support::scratch_root().join(format!(
        "helper-job-{}-{}",
        std::process::id(),
        NEXT.fetch_add(1, Ordering::Relaxed)
    ));
    std::fs::create_dir_all(&path).unwrap();
    path
}

fn machine(base: &Path) -> LinuxJobs {
    LinuxJobs {
        settings: None,
        groups: base.join("groups"),
        records_path: base.join("records"),
        jail: base.join("jail"),
        records: BTreeMap::new(),
        children: BTreeMap::new(),
        next_prune: None,
        exit_fault: None,
        remove_test_group: Some(|path| std::fs::remove_dir_all(path)),
    }
}

struct Vmm {
    child: Child,
    base: PathBuf,
}
impl Vmm {
    fn new() -> Self {
        let base = scratch();
        let binary = base.join("firecracker");
        std::fs::copy("/bin/sleep", &binary).unwrap();
        let child = Command::new(binary)
            .arg0("sleep")
            .arg("300")
            .spawn()
            .unwrap();
        // spawn can return while the child is still inside execve: the kernel
        // wakes a vfork-style parent before it renames the child, so `comm`
        // briefly still shows the test's name. The helper itself checks the
        // name only after the VMM's API socket is up.
        let comm = format!("/proc/{}/comm", child.id());
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
        while std::fs::read_to_string(&comm).unwrap().trim() != "firecracker" {
            assert!(
                std::time::Instant::now() < deadline,
                "fixture never became firecracker"
            );
            std::thread::sleep(std::time::Duration::from_millis(5));
        }
        Self { child, base }
    }
}
impl Drop for Vmm {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
        let _ = std::fs::remove_dir_all(&self.base);
    }
}

#[test]
fn cold_boot_uses_request_size_and_exactly_the_three_job_images() {
    let requests = boot_requests(&start());
    let paths: Vec<_> = requests.iter().map(|r| r.0).collect();
    assert_eq!(
        paths,
        [
            "/machine-config",
            "/boot-source",
            "/drives/root",
            "/pmem/toolchain",
            "/drives/workspace",
            "/vsock",
            "/balloon",
            "/actions"
        ]
    );
    assert_eq!(
        requests[0].1,
        json!({"vcpu_count":7,"mem_size_mib":1536,"smt":false})
    );
    assert_eq!(
        requests[2].1,
        json!({"drive_id":"root","path_on_host":"/rootfs.ext4","is_root_device":true,"is_read_only":true})
    );
    assert_eq!(
        requests[3].1,
        json!({"id":"toolchain","path_on_host":"/toolchain.ext4","read_only":true})
    );
    assert_eq!(
        requests[4].1,
        json!({"drive_id":"workspace","path_on_host":"/workspace.img","is_root_device":false,"is_read_only":false})
    );
    assert_eq!(
        requests[6].1,
        json!({"amount_mib":0,"deflate_on_oom":true,"free_page_reporting":true,"stats_polling_interval_s":0})
    );
    assert_eq!(requests[7].1, json!({"action_type":"InstanceStart"}));
    let mut different = start();
    different.vcpus = 2;
    different.memory_mib = 8192;
    assert_eq!(
        boot_requests(&different)[0].1,
        json!({"vcpu_count":2,"mem_size_mib":8192,"smt":false})
    );
}

#[test]
fn attached_disk_is_refused_by_identity_and_inode_until_released() {
    let base = scratch();
    let mut jobs = machine(&base);
    jobs.settings = Some(JobVms {
        uid_base: 500000,
        uid_count: 1,
        memory_max_bytes: 1048576,
    });
    jobs.adopt_record(record()).unwrap();
    let mut next = start();
    next.job_id = "00000000-0000-4000-8000-000000000002".into();
    let error = jobs
        .handle(
            Operation::Start(next),
            Peer {
                uid: unsafe { libc::geteuid() },
                gid: unsafe { libc::getegid() },
            },
        )
        .err()
        .unwrap();
    assert!(error.0.contains("already attached"), "{}", error.0);
    assert!(jobs
        .disk_available(&start(), 10, 11)
        .unwrap_err()
        .0
        .contains("already attached"));
    let mut alias = start();
    alias.workspace_disk_id = "alias".into();
    alias.account = "other".into();
    assert!(jobs.disk_available(&alias, 8, 99).is_err());
    jobs.disk_available(&alias, 8, 100).unwrap();
    jobs.records.get_mut(&start().job_id).unwrap().released = true;
    jobs.disk_available(&start(), 8, 99).unwrap();
    std::fs::remove_dir_all(base).unwrap();
}

#[test]
fn helper_restart_re_adopts_a_live_vmm_without_killing_it() {
    let mut vmm = Vmm::new();
    let mut old = machine(&vmm.base);
    let group = old.group(&start().job_id);
    std::fs::create_dir_all(&group).unwrap();
    std::fs::write(group.join("cgroup.procs"), vmm.child.id().to_string()).unwrap();
    old.adopt_record(record()).unwrap();
    let durable = serde_json::to_vec(old.records.get(&start().job_id).unwrap()).unwrap();
    // Dropping the old helper's child handles must not signal their processes.
    let child = Command::new("/bin/sleep").arg("300").spawn().unwrap();
    let child_pid = child.id();
    old.children.insert(start().job_id, child);
    drop(old);
    let mut restarted = machine(&vmm.base);
    restarted
        .adopt_record(serde_json::from_slice(&durable).unwrap())
        .unwrap();
    assert!(matches!(restarted.status(&start().job_id).unwrap().reply,
        Reply::Job { running: true, vmm_pid: Some(pid), exit_report: None } if pid == vmm.child.id()));
    assert!(vmm.child.try_wait().unwrap().is_none());
    assert_eq!(unsafe { libc::kill(child_pid as i32, 0) }, 0);
    unsafe {
        libc::kill(child_pid as i32, libc::SIGKILL);
        libc::waitpid(child_pid as i32, std::ptr::null_mut(), 0);
    }
}

#[test]
fn adoption_refuses_a_cgroup_with_the_wrong_uid() {
    let vmm = Vmm::new();
    let mut jobs = machine(&vmm.base);
    let group = jobs.group(&start().job_id);
    std::fs::create_dir_all(&group).unwrap();
    std::fs::write(group.join("cgroup.procs"), vmm.child.id().to_string()).unwrap();
    let mut wrong = record();
    wrong.uid += 1;
    assert!(jobs
        .adopt_record(wrong)
        .unwrap_err()
        .0
        .contains("identity mismatch"));
}

#[test]
fn job_vmm_oom_score_is_read_back_above_host_and_helper() {
    let vmm = Vmm::new();
    let host = read_oom_score(std::process::id()).unwrap();
    let helper = read_oom_score(std::process::id()).unwrap();
    set_job_oom_score(vmm.child.id(), host, helper).unwrap();
    let actual = std::fs::read_to_string(format!("/proc/{}/oom_score_adj", vmm.child.id()))
        .unwrap()
        .trim()
        .parse::<i32>()
        .unwrap();
    assert!(actual > host && actual > helper);
    assert!(check_oom_order(1000, helper).is_err());
    assert!(check_oom_order(host, 1000).is_err());
}

#[test]
fn job_child_is_placed_before_exec() {
    let base = scratch();
    let path = base.join("procs-written-before-exec");
    let procs = OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&path)
        .unwrap();
    let mut command = Command::new("/bin/sleep");
    command
        .arg("300")
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    unsafe {
        command.pre_exec(move || super::super::raw_place(procs.as_raw_fd()));
    }
    let child = command.spawn().unwrap();
    let _vmm = Vmm { child, base };
    // spawn returns only after the exec handshake. Placement must have happened
    // before the guest executable could process even one request.
    assert_eq!(std::fs::read_to_string(&path).unwrap(), "0");
}

#[test]
fn final_oom_evidence_is_not_invented() {
    let base = scratch();
    assert!(read_oom_kill(&base).is_err());
    std::fs::write(base.join("memory.events"), "oom 7\noom_kill 3\n").unwrap();
    assert_eq!(read_oom_kill(&base).unwrap(), 3);
    std::fs::write(base.join("memory.events"), "oom 7\n").unwrap();
    assert!(read_oom_kill(&base).is_err());
    std::fs::remove_dir_all(base).unwrap();
}

#[cfg(feature = "linux-service-tests")]
struct ServiceResources {
    base: PathBuf,
    group: PathBuf,
}

#[cfg(feature = "linux-service-tests")]
impl Drop for ServiceResources {
    fn drop(&mut self) {
        if self.group.exists() {
            let _ = std::fs::write(self.group.join("cgroup.kill"), "1");
            let _ = tenant::wait_empty(&self.group);
            let _ = std::fs::remove_dir(&self.group);
        }
        let _ = Command::new("systemctl")
            .args(["stop", "motorvmtest.slice"])
            .status();
        let _ = std::fs::remove_dir_all(&self.base);
    }
}

#[cfg(feature = "linux-service-tests")]
fn slice_state(point: &str) {
    let output = Command::new("systemctl")
        .args([
            "show",
            "motorvmtest.slice",
            "-p",
            "ActiveState,SubState,Delegate",
        ])
        .output()
        .unwrap();
    eprintln!(
        "{point}: systemctl show motorvmtest.slice: status={}\n{}{}",
        output.status,
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
}

#[cfg(feature = "linux-service-tests")]
fn child_placement(point: &str, pid: u32) {
    eprintln!(
        "{point}: pid={pid} cgroup={:?}",
        std::fs::read_to_string(format!("/proc/{pid}/cgroup"))
    );
}

#[cfg(feature = "linux-service-tests")]
fn wait_for_file(path: &Path) {
    let deadline = std::time::Instant::now() + Duration::from_secs(15);
    while !path.exists() {
        assert!(
            std::time::Instant::now() < deadline,
            "timed out waiting for {}",
            path.display()
        );
        std::thread::sleep(Duration::from_millis(10));
    }
}

/// Runs outside remote jobs as root: it creates a disposable systemd service
/// with the shipped helper's kill/delegation settings and a separate test slice.
/// The shared scratch root and its ancestors must be root-owned, non-writable
/// by other users, and traversable by the VM uid in 600000..601000; do not use
/// a directory below /root. On a Linux build server with systemd 254+ as PID 1,
/// a writable cgroup v2 hierarchy and a C compiler, run:
///
/// ```sh
/// mkdir -p target/scratch
/// cc -O2 -Wall -Wextra -Werror crates/motor-vm-helper/fixtures/job-firecracker.c -o target/scratch/job-firecracker
/// TMPDIR=/var/lib/motor-root-tests \
/// MOTOR_VM_TEST_JOB_FIRECRACKER="$PWD/target/scratch/job-firecracker" \
/// cargo nextest run -p motor-vm-helper --features linux-service-tests \
///   -E 'test(=linux::job::tests::isolated_job_vmm_survives_helper_service_restart)' --no-capture
/// ```
#[test]
#[cfg(feature = "linux-service-tests")]
fn isolated_job_vmm_survives_helper_service_restart() {
    assert_eq!(unsafe { libc::geteuid() }, 0);
    const TEST: &str = "linux::job::tests::isolated_job_vmm_survives_helper_service_restart";
    const SLICE: &str = "/sys/fs/cgroup/motorvmtest.slice";
    if let Ok(phase) = std::env::var("MOTOR_VM_JOB_TEST_PHASE") {
        let base = PathBuf::from(std::env::var_os("MOTOR_VM_JOB_TEST_ROOT").unwrap());
        let mut jobs = machine(&base);
        jobs.remove_test_group = None;
        jobs.groups = SLICE.into();
        jobs.settings = Some(JobVms {
            uid_base: 600000,
            uid_count: 1000,
            memory_max_bytes: 268435456,
        });
        if phase == "start" {
            trusted_directory(&jobs.records_path).unwrap();
            let uid = jobs.allocate_uid(jobs.settings.as_ref().unwrap()).unwrap();
            let mut record = record();
            record.uid = uid;
            let group = jobs.group(&record.start.job_id);
            cgroup::create_job(&group, 1).unwrap();
            let procs = OpenOptions::new()
                .write(true)
                .open(group.join("cgroup.procs"))
                .unwrap();
            let binary = base.join("firecracker");
            let mut command = Command::new(binary);
            command
                .arg("300")
                .stdin(Stdio::null())
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .env_clear();
            unsafe {
                command.pre_exec(move || {
                    super::super::raw_place(procs.as_raw_fd())?;
                    if libc::setgroups(0, std::ptr::null()) != 0
                        || libc::setgid(uid) != 0
                        || libc::setuid(uid) != 0
                    {
                        return Err(std::io::Error::last_os_error());
                    }
                    Ok(())
                });
            }
            // This no-KVM fixture has the same lifetime, uid and placement as
            // jailer's Firecracker child; the test needs no guest image.
            jobs.save(&record).unwrap();
            let child = command.spawn().unwrap();
            child_placement("inner immediately after move", child.id());
            set_job_oom_score(
                child.id(),
                read_oom_score(std::process::id()).unwrap(),
                read_oom_score(std::process::id()).unwrap(),
            )
            .unwrap();
            tenant::durable_write(&base.join("pid"), child.id().to_string().as_bytes()).unwrap();
            let placement =
                std::fs::read_to_string(format!("/proc/{}/cgroup", child.id())).unwrap();
            assert!(placement.contains("/motorvmtest.slice/job-"), "{placement}");
            assert!(
                !placement.contains("motor-vm-helper-job-test.service"),
                "{placement}"
            );
            jobs.children.insert(record.start.job_id.clone(), child);
            jobs.adopt_record(record).unwrap();
            assert_eq!(
                std::fs::read_to_string(group.join("memory.max"))
                    .unwrap()
                    .trim(),
                "max"
            );
            assert_eq!(
                std::fs::read_to_string(group.join("memory.high"))
                    .unwrap()
                    .trim(),
                "max"
            );
            assert!(std::fs::read_to_string("/proc/self/cgroup")
                .unwrap()
                .contains("/motor-vm-helper-job-test.service/helper"));
            wait_for_file(&base.join("allow-helper-exit"));
            let child = jobs.children.get_mut(&start().job_id).unwrap();
            let status = child.try_wait().unwrap();
            eprintln!("inner before exit: child try_wait={status:?}");
            assert!(
                status.is_none(),
                "job VMM fixture exited before the helper: {status:?}"
            );
            slice_state("inner before service exit");
            child_placement("inner immediately before service exit", child.id());
        } else {
            jobs.reconcile().unwrap();
            let expected: u32 = std::fs::read_to_string(base.join("pid"))
                .unwrap()
                .parse()
                .unwrap();
            assert!(matches!(jobs.status(&start().job_id).unwrap().reply,
                Reply::Job { running: true, vmm_pid: Some(pid), exit_report: None } if pid == expected));
            assert_eq!(read_oom_score(expected).unwrap(), JOB_OOM_SCORE);
        }
        return;
    }
    let base = scratch();
    // Ancestors of helper-owned records must already be root-controlled. The
    // operator must set a root-controlled, uid-traversable shared scratch root
    // such as TMPDIR=/var/lib/motor-root-tests for this service restart test.
    // A path below /root prevents the dropped-uid child from executing.
    for ancestor in base.ancestors() {
        fs::trusted(ancestor, &std::fs::symlink_metadata(ancestor).unwrap()).unwrap();
    }
    std::fs::set_permissions(&base, std::fs::Permissions::from_mode(0o755)).unwrap();
    // Compile fixtures/job-firecracker.c on the build server and name its path.
    // Copying a multicall /bin/sleep under a different name can exit immediately
    // because its applet dispatch uses argv[0], not the supplied sleep interval.
    let fixture = std::env::var_os("MOTOR_VM_TEST_JOB_FIRECRACKER").expect(
        "set MOTOR_VM_TEST_JOB_FIRECRACKER to the compiled fixtures/job-firecracker.c executable",
    );
    std::fs::copy(fixture, base.join("firecracker")).unwrap();
    let run = |args: &[&str]| {
        assert!(Command::new("systemctl")
            .args(args)
            .status()
            .unwrap()
            .success());
    };
    run(&["start", "motorvmtest.slice"]);
    run(&[
        "set-property",
        "--runtime",
        "motorvmtest.slice",
        "MemoryMax=268435456",
    ]);
    cgroup::enable(Path::new(SLICE)).unwrap();
    let group = Path::new(SLICE).join(format!("job-{}", start().job_id));
    assert!(!group.exists(), "leftover job service-test cgroup");
    let _resources = ServiceResources {
        base: base.clone(),
        group,
    };
    let unit = include_str!("../../../systemd/motor-vm-helper.service");
    let properties: Vec<_> = ["Delegate=", "DelegateSubgroup=", "KillMode="]
        .iter()
        .map(|prefix| {
            format!(
                "--property={}",
                unit.lines().find(|line| line.starts_with(prefix)).unwrap()
            )
        })
        .collect();
    for phase in ["start", "adopt"] {
        slice_state(&format!("outer before {phase} service"));
        let mut service = Command::new("systemd-run")
            .args([
                "--wait",
                "--pipe",
                "--collect",
                "--unit=motor-vm-helper-job-test",
            ])
            .args(&properties)
            .arg(format!("--setenv=MOTOR_VM_JOB_TEST_PHASE={phase}"))
            .arg(format!(
                "--setenv=MOTOR_VM_JOB_TEST_ROOT={}",
                base.display()
            ))
            .arg(std::env::current_exe().unwrap())
            .args(["--exact", TEST, "--nocapture"])
            .spawn()
            .unwrap();
        wait_for_file(&base.join("pid"));
        let pid: u32 = std::fs::read_to_string(base.join("pid"))
            .unwrap()
            .parse()
            .unwrap();
        // A pidfd observes exit without confusing a reused pid for the VMM.
        // waitid may report ECHILD because systemd, not this test, is its reaper.
        use std::os::fd::FromRawFd;
        let pidfd = unsafe { libc::syscall(libc::SYS_pidfd_open, pid, 0) as i32 };
        assert!(
            pidfd >= 0,
            "pidfd_open: {}",
            std::io::Error::last_os_error()
        );
        let pidfd = unsafe { File::from_raw_fd(pidfd) };
        if phase == "start" {
            child_placement("outer while inner helper is alive", pid);
            slice_state("outer immediately before allowing inner helper exit");
            std::fs::write(base.join("allow-helper-exit"), "1").unwrap();
        }
        assert!(service.wait().unwrap().success());
        slice_state(&format!("outer after {phase} service exit"));
        child_placement("outer after service exit", pid);
        let mut event = libc::pollfd {
            fd: pidfd.as_raw_fd(),
            events: libc::POLLIN,
            revents: 0,
        };
        assert!(unsafe { libc::poll(&mut event, 1, 0) } >= 0);
        eprintln!("outer pidfd exit readiness: {}", event.revents);
        if event.revents != 0 {
            let mut info: libc::siginfo_t = unsafe { std::mem::zeroed() };
            let rc = unsafe {
                libc::waitid(
                    libc::P_PIDFD,
                    pidfd.as_raw_fd() as u32,
                    &mut info,
                    libc::WEXITED | libc::WNOHANG | libc::WNOWAIT,
                )
            };
            if rc == 0 {
                eprintln!(
                    "outer child exit: code={} status={}",
                    info.si_code,
                    unsafe { info.si_status() }
                );
            } else {
                eprintln!(
                    "outer cannot reap child exit status: {}",
                    std::io::Error::last_os_error()
                );
            }
            let journal = Command::new("journalctl")
                .args([
                    "--no-pager",
                    "-n",
                    "30",
                    "-u",
                    "motorvmtest.slice",
                    "-u",
                    "motor-vm-helper-job-test.service",
                ])
                .output()
                .unwrap();
            eprintln!(
                "job test unit journal:\n{}",
                String::from_utf8_lossy(&journal.stdout)
            );
            eprintln!(
                "job cgroup memory.events after exit: {:?}",
                std::fs::read_to_string(
                    Path::new(SLICE).join(format!("job-{}/memory.events", start().job_id))
                )
            );
        }
        assert_eq!(unsafe { libc::kill(pid as i32, 0) }, 0);
        let placement = std::fs::read_to_string(format!("/proc/{pid}/cgroup")).unwrap();
        assert!(placement.contains("/motorvmtest.slice/job-"));
        assert!(!placement.contains("motor-vm-helper-job-test.service"));
    }
    let group = Path::new(SLICE).join(format!("job-{}", start().job_id));
    std::fs::write(group.join("cgroup.kill"), "1").unwrap();
    tenant::wait_empty(&group).unwrap();
    // The helper was absent at exit. Startup must recover the real cgroup's
    // evidence, persist it with root ownership, and replay it until acked.
    let mut restarted = machine(&base);
    restarted.groups = SLICE.into();
    restarted.remove_test_group = None;
    restarted.settings = Some(JobVms {
        uid_base: 600000,
        uid_count: 1000,
        memory_max_bytes: 268435456,
    });
    restarted.reconcile().unwrap();
    assert!(!group.exists());
    let report = ExitReport {
        job_id: start().job_id,
        oom_kill: 0,
    };
    assert!(
        matches!(restarted.handle(Operation::Exits, Peer { uid: 0, gid: 0 }).unwrap().reply,
        Reply::JobExits { exits } if exits == [report.clone()])
    );
    let path = restarted
        .records_path
        .join(format!("{}.json", start().job_id));
    let metadata = std::fs::metadata(&path).unwrap();
    assert_eq!(metadata.uid(), 0);
    assert_eq!(metadata.mode() & 0o777, 0o600);
    restarted.acknowledge_exit(report.clone()).unwrap();
    restarted.records.clear();
    restarted.reconcile().unwrap();
    assert!(
        matches!(restarted.handle(Operation::Exits, Peer { uid: 0, gid: 0 }).unwrap().reply,
        Reply::JobExits { exits } if exits.is_empty())
    );
    restarted.acknowledge_exit(report).unwrap();
    run(&["stop", "motorvmtest.slice"]);
    std::fs::remove_dir_all(base).unwrap();
}
