use super::*;

pub(super) struct Artifacts {
    pub binaries: PathBuf,
    pub images: Vec<(PathBuf, &'static str)>,
    pub memory_mib: u32,
}

impl Artifacts {
    pub fn new(base: &Path) -> Self {
        let memory_mib: u32 = std::env::var("MOTOR_VM_TEST_JOB_MEMORY_MIB")
            .expect(
                "set MOTOR_VM_TEST_JOB_MEMORY_MIB to the image's smallest bootable RAM size in MiB",
            )
            .parse()
            .expect("MOTOR_VM_TEST_JOB_MEMORY_MIB must be a positive integer");
        assert!(memory_mib > 0);
        let binaries = base.join("bin");
        std::fs::create_dir(&binaries).unwrap();
        for (name, variable) in [
            ("firecracker", "MOTOR_VM_TEST_FIRECRACKER"),
            ("jailer", "MOTOR_VM_TEST_JAILER"),
        ] {
            let source = fixture::artifact(variable);
            fs::trusted_file(&source).unwrap();
            let version = Command::new(&source).arg("--version").output().unwrap();
            assert!(
                version.status.success()
                    && String::from_utf8_lossy(&version.stdout).contains("1.17.0"),
                "{variable} must be version 1.17.0"
            );
            std::fs::copy(source, binaries.join(name)).unwrap();
            std::fs::set_permissions(binaries.join(name), std::fs::Permissions::from_mode(0o755))
                .unwrap();
        }
        let images = [
            ("MOTOR_VM_TEST_JOB_KERNEL", "kernel"),
            ("MOTOR_VM_TEST_JOB_ROOTFS", "rootfs.ext4"),
            ("MOTOR_VM_TEST_JOB_TOOLCHAIN", "toolchain.ext4"),
        ]
        .into_iter()
        .map(|(variable, name)| (fixture::artifact(variable), name))
        .collect();
        Self {
            binaries,
            images,
            memory_mib,
        }
    }
}

pub(super) fn boot(jobs: &LinuxJobs, artifacts: &Artifacts, mut start: Start) -> Result<Response> {
    start.vcpus = 2;
    start.memory_mib = u64::from(artifacts.memory_mib);
    start.burst_mib = None;
    start.cache_write_blocked = true;
    start.validate_fresh_at(SystemTime::now())?;
    jobs.create_workspace(job::CreateWorkspace {
        cache_write_job_id: None,
        account: start.account.clone(),
        workspace_disk_id: start.workspace_disk_id.clone(),
        size_bytes: 4 << 30,
    })?;
    let _admission = jobs.admission.lock();
    let uid = jobs.allocate_uid(jobs.settings.as_ref().unwrap())?;
    assert!(
        unsafe { libc::getpwuid(uid) }.is_null(),
        "fixture UID must not name a host account"
    );
    let (registry, disk) =
        workspace::open_disk(&jobs.workspaces, &start.account, &start.workspace_disk_id)?;
    let meta = disk.metadata()?;
    let index = network::allocate(&[], &jobs.records())?;
    let record = Record::new(start.clone(), uid, meta.dev(), meta.ino(), Some(index));
    jobs.launch_record(_admission, record, |state| {
        let id = &start.job_id;
        let tap = Tap {
            tap_index: index,
            uid,
        };
        let netns = jobs.network.create_namespace(id)?;
        jobs.prepare_network(id, tap)?;
        let boundary = jobs.boundary(id);
        let root = jobs.root(id);
        let group = jobs.group(id);
        cgroup::create_job(&group, 2)?;
        std::fs::write(
            group.join("memory.max"),
            ((start.memory_mib + 128) << 20).to_string(),
        )?;
        std::fs::write(group.join("memory.swap.max"), "0")?;
        crate::linux::create_boundary(&boundary)?;
        let boundary_fd = File::open(&boundary)?;
        let (_, requests) = stage_boot(
            &jobs.cache_store,
            &start,
            &registry,
            jobs.settings.as_ref().unwrap(),
            &boundary_fd,
            uid,
            Some(index),
            meta.len(),
            None,
        )?;
        std::fs::create_dir_all(&root)?;
        std::fs::set_permissions(&root, std::fs::Permissions::from_mode(0o700))?;
        fs::chown(&File::open(&root)?, uid)?;
        for (source, name) in &artifacts.images {
            tenant::bind(&fs::trusted_file(source)?, &root.join(name), true)?;
        }
        fs::chown(&disk, uid)?;
        tenant::bind(&disk, &root.join("workspace.img"), false)?;
        let procs = OpenOptions::new()
            .write(true)
            .open(group.join("cgroup.procs"))?;
        let console = File::create(boundary.join(CONSOLE_LOG))?;
        let mut command = jailer_command(&artifacts.binaries, &boundary, &netns, id, uid);
        command.stdout(console.try_clone()?).stderr(console);
        unsafe {
            command.pre_exec(move || crate::linux::raw_place(procs.as_raw_fd()));
        }
        let child = command.spawn()?;
        let pid = child.id();
        state.child = Some(child);
        state.pidfd = Some(Arc::new(memory::pin_pidfd(pid)?));
        set_job_oom_score(
            pid,
            read_oom_score(std::process::id())?,
            read_oom_score(std::process::id())?,
        )?;
        let root_fd = File::open(&root)?;
        let short_root = PathBuf::from(format!("/proc/self/fd/{}", root_fd.as_raw_fd()));
        drop(crate::linux::wait_socket(
            &short_root.join("api.sock"),
            Duration::from_secs(10),
        )?);
        jobs.boot_api_requests(&short_root, requests)?;
        Ok(())
    })
}

pub(super) fn guest_ready(jobs: &LinuxJobs, id: &str) {
    let root = File::open(jobs.root(id)).unwrap();
    let path = PathBuf::from(format!("/proc/self/fd/{}", root.as_raw_fd()));
    let uid = jobs.view(id).unwrap().record.uid;
    // A successful real guest vsock handshake proves more than InstanceStart:
    // the production agent has booted. No guest job or live host is contacted.
    let deadline = Instant::now() + Duration::from_secs(90);
    loop {
        match tenant::connect_job_relay(&path, uid, 54) {
            Ok(fd) => {
                drop(fd);
                return;
            }
            Err(error) => {
                assert!(
                    Instant::now() < deadline,
                    "real guest control agent must boot: {error}"
                );
                std::thread::sleep(Duration::from_millis(50));
            }
        }
    }
}
