use super::*;
use std::io::{BufRead, BufReader, Read, Write};
use std::os::unix::net::UnixListener;

const TEST: &str = "linux::job::tests::start_service_tests::isolated_start_api_survives_helper_sigkill_and_releases_disk_after_empty";
const SLICE: &str = "/sys/fs/cgroup/motorvmtest.slice";

fn jobs(base: &Path) -> LinuxJobs {
    let mut jobs = machine(base);
    jobs.remove_test_group = None;
    jobs.groups = SLICE.into();
    jobs.settings = Some(JobVms {
        workspace_disk_max_bytes: Some(1 << 30),
        fixed_path_repositories: Default::default(),
        uid_base: 600000,
        uid_count: 254,
        memory_max_bytes: 268435456,
    });
    jobs
}

fn read_record(jobs: &LinuxJobs, id: &str) -> Record {
    serde_json::from_reader(File::open(jobs.records_path.join(format!("{id}.json"))).unwrap())
        .unwrap()
}

fn start_phase(base: &Path, jobs: &LinuxJobs, start: Start) {
    trusted_directory(&jobs.records_path).unwrap();
    let admission = jobs.admission.lock();
    let uid = jobs.allocate_uid(jobs.settings.as_ref().unwrap()).unwrap();
    let intent = start_record(jobs, start.clone(), uid, None);
    let group = jobs.group(&start.job_id);
    let disk_path = jobs
        .workspaces
        .join(&start.workspace_disk_id)
        .join("workspace.img");
    let marker = base.join("api-paused");
    jobs.pause_exit_at(ExitStep::Api, move || {
        tenant::durable_write(&marker, std::process::id().to_string().as_bytes()).unwrap();
        // The operator kills this helper while launch holds the job-state lock.
        // No return path may publish Running before the simulated crash.
        loop {
            std::thread::park();
        }
    });
    jobs.launch_record(admission, intent, |state| {
        cgroup::create_job(&group, 1)?;
        let procs = OpenOptions::new()
            .write(true)
            .open(group.join("cgroup.procs"))?;
        let mut command = Command::new(base.join("firecracker"));
        command
            .arg("300")
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .env_clear();
        unsafe {
            command.pre_exec(move || {
                super::super::super::raw_place(procs.as_raw_fd())?;
                if libc::setgroups(0, std::ptr::null()) != 0
                    || libc::setgid(uid) != 0
                    || libc::setuid(uid) != 0
                {
                    return Err(std::io::Error::last_os_error());
                }
                Ok(())
            });
        }
        let child = command.spawn()?;
        let pid = child.id();
        state.child = Some(child);
        state.pidfd = Some(std::sync::Arc::new(memory::pin_pidfd(pid)?));
        set_job_oom_score(
            pid,
            read_oom_score(std::process::id())?,
            read_oom_score(std::process::id())?,
        )?;
        fs::chown(&File::open(&disk_path)?, uid)?;
        tenant::durable_write(&base.join("pid"), pid.to_string().as_bytes())?;

        // Only the Firecracker response is a fixture: the real API client and
        // post-InstanceStart pause run inside the production launch boundary.
        let directory = File::open(base)?;
        let root = PathBuf::from(format!("/proc/self/fd/{}", directory.as_raw_fd()));
        let listener = UnixListener::bind(root.join("api.sock"))?;
        let api = std::thread::spawn(move || {
            let (stream, _) = listener.accept().unwrap();
            stream
                .set_read_timeout(Some(Duration::from_secs(5)))
                .unwrap();
            let mut reader = BufReader::new(stream);
            let mut length = 0;
            let mut request = String::new();
            loop {
                let mut line = String::new();
                assert!(reader.read_line(&mut line).unwrap() > 0);
                if line == "\r\n" {
                    break;
                }
                if let Some(value) = line.to_ascii_lowercase().strip_prefix("content-length:") {
                    length = value.trim().parse::<usize>().unwrap();
                }
                request.push_str(&line);
            }
            let mut body = vec![0; length];
            reader.read_exact(&mut body).unwrap();
            assert!(request.starts_with("PUT /actions "), "{request}");
            assert_eq!(
                serde_json::from_slice::<Value>(&body).unwrap(),
                json!({"action_type":"InstanceStart"})
            );
            reader
                .get_mut()
                .write_all(b"HTTP/1.1 204 No Content\r\nContent-Length: 0\r\n\r\n")
                .unwrap();
        });
        jobs.boot_api_requests(
            &root,
            vec![("/actions", json!({"action_type":"InstanceStart"}))],
        )?;
        api.join().unwrap();
        panic!("API pause returned before the helper was killed");
    })
    .unwrap();
}

fn adopt_and_stop(base: &Path, jobs: &LinuxJobs, start: Start) {
    jobs.reconcile().unwrap();
    let expected: u32 = std::fs::read_to_string(base.join("pid"))
        .unwrap()
        .parse()
        .unwrap();
    assert!(
        matches!(jobs.handle(Operation::Adopt(start.job_id.clone()), Peer { uid: 0, gid: 0 }).unwrap().reply,
        Reply::Job { known: true, running: true, vmm_pid: Some(pid), exit_report: None, .. } if pid == expected)
    );
    let record = read_record(jobs, &start.job_id);
    assert!(!record.released);
    assert!(jobs
        .disk_available(&start, record.disk_device, record.disk_inode)
        .is_err());
    let group = jobs.group(&start.job_id);
    assert!(!std::fs::read_to_string(group.join("cgroup.procs"))
        .unwrap()
        .trim()
        .is_empty());
    let disk = jobs
        .workspaces
        .join(&start.workspace_disk_id)
        .join("workspace.img");
    assert_eq!(std::fs::metadata(&disk).unwrap().uid(), record.uid);
    let path = jobs.records_path.join(format!("{}.json", start.job_id));
    let disk_before_release = disk.clone();
    let group_before_release = group.clone();
    jobs.pause_exit_at(ExitStep::Captured, move || {
        // This checkpoint follows stop's wait_empty and precedes both evidence
        // publication and disk release. A live group must never release a disk.
        assert!(
            std::fs::read_to_string(group_before_release.join("cgroup.procs"))
                .unwrap()
                .trim()
                .is_empty()
        );
        let record: Record = serde_json::from_reader(File::open(&path).unwrap()).unwrap();
        assert!(!record.released);
        assert_eq!(
            std::fs::metadata(&disk_before_release).unwrap().uid(),
            record.uid
        );
    });
    jobs.handle(
        Operation::Stop(start.job_id.clone()),
        Peer { uid: 0, gid: 0 },
    )
    .unwrap();
    assert!(read_record(jobs, &start.job_id).released);
    assert!(!group.exists());
    assert_eq!(std::fs::metadata(disk).unwrap().uid(), 0);
    jobs.disk_available(&start, record.disk_device, record.disk_inode)
        .unwrap();
}

// Run as root outside remote jobs, with the same scratch/fixture prerequisites
// as isolated_job_vmm_survives_helper_service_restart. No KVM image is needed.
#[test]
fn isolated_start_api_survives_helper_sigkill_and_releases_disk_after_empty() {
    assert_eq!(unsafe { libc::geteuid() }, 0);
    if let Ok(phase) = std::env::var("MOTOR_VM_START_CRASH_PHASE") {
        let base = PathBuf::from(std::env::var_os("MOTOR_VM_START_CRASH_ROOT").unwrap());
        let jobs = jobs(&base);
        if phase == "start" {
            start_phase(&base, &jobs, restart_start(&base));
        } else {
            assert_eq!(phase, "adopt");
            adopt_and_stop(&base, &jobs, restart_start(&base));
        }
        return;
    }
    let base = scratch();
    for ancestor in base.ancestors() {
        fs::trusted(ancestor, &std::fs::symlink_metadata(ancestor).unwrap()).unwrap();
    }
    std::fs::set_permissions(&base, std::fs::Permissions::from_mode(0o755)).unwrap();
    tenant::durable_write(
        &base.join("start.json"),
        &serde_json::to_vec(&start()).unwrap(),
    )
    .unwrap();
    let fixture = std::env::var_os("MOTOR_VM_TEST_JOB_FIRECRACKER")
        .expect("set MOTOR_VM_TEST_JOB_FIRECRACKER to compiled fixtures/job-firecracker.c");
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
    let start = restart_start(&base);
    let group = Path::new(SLICE).join(format!("job-{}", start.job_id));
    assert!(!group.exists(), "leftover service-test cgroup");
    let _resources = ServiceResources::new(base.clone(), group.clone());
    let unit = include_str!("../../../../systemd/motor-vm-helper.service");
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
        let mut service = Command::new("systemd-run")
            .args([
                "--wait",
                "--pipe",
                "--collect",
                "--unit=motor-vm-helper-job-test",
            ])
            .args(&properties)
            .arg(format!("--setenv=MOTOR_VM_START_CRASH_PHASE={phase}"))
            .arg(format!(
                "--setenv=MOTOR_VM_START_CRASH_ROOT={}",
                base.display()
            ))
            .arg(std::env::current_exe().unwrap())
            .args(["--exact", TEST, "--nocapture"])
            .spawn()
            .unwrap();
        if phase == "start" {
            wait_for_file(&base.join("api-paused"));
            let helper: i32 = std::fs::read_to_string(base.join("api-paused"))
                .unwrap()
                .parse()
                .unwrap();
            let jobs = jobs(&base);
            let record = read_record(&jobs, &start.job_id);
            assert!(!record.released);
            assert!(!std::fs::read_to_string(group.join("cgroup.procs"))
                .unwrap()
                .trim()
                .is_empty());
            assert_eq!(unsafe { libc::kill(helper, libc::SIGKILL) }, 0);
            assert!(
                !service.wait().unwrap().success(),
                "SIGKILL helper unexpectedly succeeded"
            );
            assert!(!read_record(&jobs, &start.job_id).released);
            assert!(!std::fs::read_to_string(group.join("cgroup.procs"))
                .unwrap()
                .trim()
                .is_empty());
        } else {
            assert!(service.wait().unwrap().success());
            assert!(read_record(&jobs(&base), &start.job_id).released);
            assert!(!group.exists());
        }
    }
}
