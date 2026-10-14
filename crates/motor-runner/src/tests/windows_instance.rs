use super::*;

#[cfg(target_os = "linux")]
#[test]
fn startup_keeps_named_state_traversal_acl_effective_for_each_backend() {
    use std::os::unix::fs::PermissionsExt;

    for windows in [false, true] {
        let f = Fixture::new();
        let config = if windows { config(&f) } else { f.config() };
        let state = config.state_dir();
        fs::create_dir_all(&state).unwrap();
        let seeded = Command::new("/usr/bin/setfacl")
            .args(["-m", "u:123456:--x,g::rwx,o::rwx,m::rwx", "--"])
            .arg(&state)
            .output()
            .unwrap();
        assert!(
            seeded.status.success(),
            "{}",
            String::from_utf8_lossy(&seeded.stderr)
        );
        let runner = if windows {
            Runner::open_with_backend(config, Box::new(InProcessWindows)).unwrap()
        } else {
            Runner::open(config).unwrap()
        };
        let output = Command::new("/usr/bin/getfacl")
            .args(["-cpn", "--"])
            .arg(&state)
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
        let acl = String::from_utf8(output.stdout).unwrap();
        for entry in [
            "user::rwx",
            "user:123456:--x",
            "group::---",
            "mask::--x",
            "other::---",
        ] {
            assert!(
                acl.lines().any(|line| line == entry),
                "backend windows={windows}: missing {entry}:\n{acl}"
            );
        }
        assert_eq!(
            fs::metadata(&state).unwrap().permissions().mode() & 0o777,
            0o710
        );
        drop(runner);
    }
}
use crate::windows::{VmConfig, WindowsBackend};

#[cfg(target_os = "linux")]
#[test]
fn windows_startup_quarantine_preserves_private_state_tree_traversal_acls() {
    let f = Fixture::new();
    let config = config(&f);
    let state = config.state_dir();
    for name in ["", "jobs", "mirrors", "spools"] {
        let path = state.join(name);
        fs::create_dir_all(&path).unwrap();
        let output = Command::new("/usr/bin/setfacl")
            .args(["-m", "u:123456:r-x,g::rwx,o::rwx,m::rwx", "--"])
            .arg(&path)
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
    }
    let id = Uuid::now_v7();
    atomic_json_private(
        &state.join("quarantine").join(format!("{id}.json")),
        &QuarantinedJob {
            id,
            request: None,
            process: None,
            observed_ms: 1,
        },
    )
    .unwrap();
    let runner = Runner::open_with_backend(config, Box::new(InProcessWindows)).unwrap();
    for name in ["", "jobs", "mirrors", "spools"] {
        let path = state.join(name);
        let output = Command::new("/usr/bin/getfacl")
            .args(["-cpn", "--"])
            .arg(&path)
            .output()
            .unwrap();
        assert!(output.status.success());
        let acl = String::from_utf8(output.stdout).unwrap();
        for entry in [
            "user::rwx",
            "user:123456:r-x",
            "group::---",
            "mask::r-x",
            "other::---",
        ] {
            assert!(
                acl.lines().any(|line| line == entry),
                "{}: missing {entry}:\n{acl}",
                path.display()
            );
        }
    }
    drop(runner);
}
use std::os::unix::{
    fs::{MetadataExt, PermissionsExt},
    process::ExitStatusExt,
};

pub(super) fn config(f: &Fixture) -> Config {
    let root = f.scratch.join("windows-parent/windows");
    for name in [
        "baselines",
        "vm/overlays",
        "vm/caches",
        "vm/sockets",
        "staging",
        "state/windows",
    ] {
        fs::create_dir_all(root.join(name)).unwrap();
    }
    let root = root.canonicalize().unwrap();
    let baseline = root.join("baselines/job-1.qcow2");
    if !baseline.exists() {
        fs::write(&baseline, b"sealed fixture").unwrap();
    }
    fs::set_permissions(&baseline, fs::Permissions::from_mode(0o440)).unwrap();
    let vm = root.join("vm.toml");
    fs::write(&vm, format!("layout_root = {:?}\nbaseline = \"job-1.qcow2\"\nmemory_mib = 16384\ncpu_set = [12, 13, 14, 15]\ntoolchain_version = \"1.99.0\"\n", root.to_str().unwrap())).unwrap();
    let mut config = f.config();
    config.instance = "windows".into();
    config.state_root = root.join("state");
    config.backend = BackendKind::WindowsVm;
    config.vm_config = Some(vm);
    config.systemd_command = Some(Arc::new(|_| Ok(output(""))));
    config
}
fn output(text: &str) -> std::process::Output {
    std::process::Output {
        status: std::process::ExitStatus::from_raw(0),
        stdout: text.as_bytes().to_vec(),
        stderr: Vec::new(),
    }
}

pub(super) fn direct_execution(
    config: &Config,
    id: Uuid,
    inner: Box<dyn backend::Execution>,
) -> Box<dyn backend::Execution> {
    Box::new(DirectExecution {
        inner: Some(inner),
        collection: config.state_dir().join("jobs").join(id.to_string()),
    })
}

struct DirectExecution {
    inner: Option<Box<dyn backend::Execution>>,
    collection: PathBuf,
}
impl backend::Execution for DirectExecution {
    fn release(&mut self) -> io::Result<()> {
        self.inner.as_mut().unwrap().release()
    }
    fn output(&self) -> io::Result<backend::Output> {
        self.inner.as_ref().unwrap().output()
    }
    fn abort(&self) -> io::Result<()> {
        self.inner.as_ref().unwrap().abort()
    }
    fn wait(&mut self) -> io::Result<backend::ExecutionOutcome> {
        self.inner.as_mut().unwrap().wait()
    }
    fn reap(&self) {
        self.inner.as_ref().unwrap().reap();
    }
}
impl Drop for DirectExecution {
    fn drop(&mut self) {
        // Direct backend tests have no accepted Job record. Join teardown before
        // removing their collection-only directory from the durable job tree.
        drop(self.inner.take());
        if fs::symlink_metadata(self.collection.join("record.json"))
            .is_err_and(|error| error.kind() == io::ErrorKind::NotFound)
        {
            match fs::remove_dir_all(&self.collection) {
                Ok(()) => {}
                Err(error) if error.kind() == io::ErrorKind::NotFound => {}
                Err(error) => panic!(
                    "remove direct backend collection {}: {error}",
                    self.collection.display()
                ),
            }
        }
    }
}

#[test]
fn direct_collection_cleanup_preserves_existing_job_records() {
    let f = Fixture::new();
    let config = config(&f);
    let id = Uuid::now_v7();
    let directory = config.state_dir().join("jobs").join(id.to_string());
    fs::create_dir_all(&directory).unwrap();
    fs::write(directory.join("record.json"), b"existing record evidence").unwrap();
    drop(direct_execution(&config, id, Box::new(FakeExecution)));
    assert_eq!(
        fs::read(directory.join("record.json")).unwrap(),
        b"existing record evidence"
    );
}

// A portable execution fake creates only runner-owned staging. It deliberately
// never calls HostBackend, even when the request names an existing host checkout.
struct InProcessWindows;
impl Backend for InProcessWindows {
    fn materialize(&self, config: &Config, _trees: &Trees, setup: &Setup) -> io::Result<Prepared> {
        assert_eq!(setup.cpus, [12, 13, 14, 15]);
        let cwd = config
            .windows
            .as_ref()
            .unwrap()
            .layout_root
            .join("staging")
            .join(setup.id.to_string());
        fs::create_dir_all(&cwd)?;
        Ok(Prepared {
            target: cwd.join("target"),
            cwd,
            wrapper: None,
            sandbox: None,
            windows: None,
        })
    }
    fn launch(
        &self,
        _config: &Config,
        _trees: &Trees,
        _setup: &Setup,
        prepared: &Prepared,
        weight: u32,
    ) -> io::Result<backend::Launch> {
        assert_eq!(weight, 4);
        fs::remove_dir_all(&prepared.cwd)?;
        Ok(backend::Launch {
            process: None,
            durable_output: false,
            execution: Box::new(FakeExecution),
        })
    }
    fn cancel(&self, config: &Config, job: &Job, hard: bool) -> io::Result<()> {
        WindowsBackend.cancel(config, job, hard)
    }
    fn unit_alive(
        &self,
        config: &Config,
        process: Option<platform::ProcessIdentity>,
    ) -> io::Result<Option<bool>> {
        WindowsBackend.unit_alive(config, process)
    }
    fn collect_changes(&self, trees: &Trees, job: &Job) -> io::Result<Option<Vec<String>>> {
        WindowsBackend.collect_changes(trees, job)
    }
    fn reconcile(&self, config: &Config, jobs: &[Job]) -> io::Result<Vec<Uuid>> {
        WindowsBackend.reconcile(config, jobs)
    }
}
struct FakeExecution;
impl backend::Execution for FakeExecution {
    fn release(&mut self) -> io::Result<()> {
        Ok(())
    }
    fn output(&self) -> io::Result<backend::Output> {
        Ok(backend::Output {
            stdout: Box::new(io::Cursor::new(b"fake Windows job\n")),
            stderr: Box::new(io::empty()),
        })
    }
    fn wait(&mut self) -> io::Result<backend::ExecutionOutcome> {
        Ok(backend::ExecutionOutcome::Unix(
            std::process::ExitStatus::from_raw(0),
        ))
    }
    fn reap(&self) {}
}

#[tokio::test]
async fn windows_started_unit_backed_job_keeps_drain_waiting() {
    let f = Fixture::new();
    let runner = Runner::open_with_backend(config(&f), Box::new(InProcessWindows)).unwrap();
    let (mut request, _) = f.run("must not run on host");
    request.platform = Some(Platform::Windows);
    let id = request.job_id;
    let mut job = Job::new(id, Some(request), runner.now(), 1, 4);
    job.state = JobState::Running;
    job.started_ms = Some(runner.now());
    job.started = Some(Started::new(0, 0, runner.now()));
    job.process = Some(platform::ProcessIdentity {
        pid: 42,
        start: 1,
        unit: Some(id),
    });
    job.durable_output = true;
    runner.save(&job).unwrap();
    runner.core.inner.lock().unwrap().jobs.insert(id, job);
    runner.begin_drain(Duration::from_secs(30), false);
    assert_eq!(
        runner.drain_result(),
        None,
        "Windows cannot reattach even a recorded unit"
    );
    {
        let mut inner = runner.core.inner.lock().unwrap();
        let job = inner.jobs.get_mut(&id).unwrap();
        job.terminal(Outcome::Exit { code: 0 }, runner.now());
        runner.save(job).unwrap();
    }
    // drain_result reports "not decided yet" while a background thread holds
    // the job table, so poll as the other drain tests do instead of reading once.
    wait_for(|| runner.drain_result() == Some(false)).await;
}

#[test]
fn windows_open_requires_vm_config_and_a_sealed_regular_baseline() {
    let f = Fixture::new();
    let mut c = f.config();
    c.backend = BackendKind::WindowsVm;
    assert!(Runner::open(c)
        .err()
        .unwrap()
        .to_string()
        .contains("--vm-config"));
    let c = config(&f);
    let vm = VmConfig::load(c.vm_config.as_ref().unwrap()).unwrap();
    assert_eq!(vm.overlay_growth_quota_bytes, 40 << 30);
    fs::remove_file(vm.baseline_path()).unwrap();
    assert!(Runner::open(c.clone())
        .err()
        .unwrap()
        .to_string()
        .contains("baseline"));
    fs::write(vm.baseline_path(), b"wrong mode").unwrap();
    fs::set_permissions(vm.baseline_path(), fs::Permissions::from_mode(0o444)).unwrap();
    assert!(Runner::open(c.clone())
        .err()
        .unwrap()
        .to_string()
        .contains("0440"));
    fs::remove_file(vm.baseline_path()).unwrap();
    std::os::unix::fs::symlink(&f.main, vm.baseline_path()).unwrap();
    assert!(Runner::open(c)
        .err()
        .unwrap()
        .to_string()
        .contains("regular file"));
    assert!(!f.config().state_dir().exists());
}

#[tokio::test]
async fn windows_weights_are_fixed_and_warm_builds_and_caller_catalog_are_disabled() {
    let f = Fixture::new();
    fs::create_dir_all(&f.key).unwrap();
    let canary = Path::new(&f.key).join("untouched");
    fs::write(&canary, b"caller files").unwrap();
    let before = fs::metadata(&canary).unwrap();
    let mut c = config(&f);
    let free = Arc::new(AtomicU64::new(1000));
    let probe = free.clone();
    c.free_space_floor_bytes = 100;
    c.free_space_probe = Arc::new(move |_| Ok(probe.load(Ordering::SeqCst)));
    let runner = Runner::open_with_backend(c, Box::new(InProcessWindows)).unwrap();
    assert_eq!(runner.config().max_weight, 4);
    assert!(runner.config().workspace_roots.is_empty());
    assert_eq!(runner.config().served_platform(), Platform::Windows);
    for hint in [None, Some(1), Some(8)] {
        let (mut request, bytes) = f.run("must not run on host");
        request.platform = Some(Platform::Windows);
        request.weight_hint = hint;
        f.submit(&runner, &request, &bytes);
        assert_eq!(
            terminal(&runner, request.job_id).await.outcome,
            Outcome::Exit { code: 0 }
        );
        let inner = runner.core.inner.lock().unwrap();
        let job = &inner.jobs[&request.job_id];
        assert_eq!(job.weight, 4);
        assert_eq!(job.request.as_ref().unwrap().weight_hint, Some(4));
        assert_eq!(job.cpus, [12, 13, 14, 15]);
        assert!(inner.jobs.values().all(|j| !j.base));
        assert!(inner.catalog.workspaces.is_empty());
        assert!(inner.catalog.repositories.is_empty());
    }
    f.clock.fetch_add(RETENTION_MS + 1, Ordering::SeqCst);
    runner.maintenance().unwrap();
    free.store(0, Ordering::SeqCst);
    assert!(runner.ensure_disk_space().unwrap().is_some());
    assert_eq!(fs::read(&canary).unwrap(), b"caller files");
    let after = fs::metadata(canary).unwrap();
    assert_eq!(
        (
            before.uid(),
            before.gid(),
            before.mode(),
            before.mtime(),
            before.mtime_nsec()
        ),
        (
            after.uid(),
            after.gid(),
            after.mode(),
            after.mtime(),
            after.mtime_nsec()
        )
    );
}

#[test]
fn windows_prepare_is_refused_before_validation_reservation_or_filesystem_work() {
    let f = Fixture::new();
    fs::create_dir_all(&f.key).unwrap();
    let sentinel = Path::new(&f.key).join("sentinel");
    fs::write(&sentinel, b"untouched").unwrap();
    let before = fs::metadata(&sentinel).unwrap();
    let mut c = config(&f);
    c.free_space_floor_bytes = 1;
    c.free_space_probe = Arc::new(|_| panic!("Prepare must not probe the filesystem"));
    let runner = Runner::open_with_backend(c, Box::new(InProcessWindows)).unwrap();
    let mut request = Prepare {
        transfer_id: Uuid::now_v7(),
        workspace_key: f.key.clone(),
        workspace_kind: None,
        repository_root: f.root.clone(),
        base_commit: "invalid".into(),
        integration_tip: "invalid".into(),
        siblings: Vec::new(),
        bundles: Vec::new(),
        git_dependencies: Vec::new(),
    };
    for key in [f.key.as_str(), "invalid relative path"] {
        request.workspace_key = key.into();
        let refusal = runner
            .admit_prepare(&request)
            .unwrap()
            .err()
            .expect("refused before upload");
        assert_eq!(refusal.reason, RefusalReason::RequestRejected);
        assert_eq!(refusal.detail.as_deref(), Some("prepare_unsupported: Prepare is unsupported by the Windows instance; caller workspaces are never materialized on the host"));
        assert_eq!(refusal.retry_after_ms, None);
        assert!(runner
            .prepare(&request, true)
            .unwrap_err()
            .to_string()
            .contains("Prepare"));
        assert!(runner
            .fetch_prepare(&request, Path::new("missing-upload"))
            .unwrap_err()
            .to_string()
            .contains("Prepare"));
    }
    let inner = runner.core.inner.lock().unwrap();
    assert!(inner.reserved.is_empty() && inner.preparing.is_empty() && inner.transfers.is_empty());
    assert!(inner.catalog.workspaces.is_empty());
    assert!(!runner
        .core
        .trees
        .state
        .join("spools")
        .read_dir()
        .unwrap()
        .any(|_| true));
    assert_eq!(fs::read(&sentinel).unwrap(), b"untouched");
    let after = fs::metadata(sentinel).unwrap();
    assert_eq!(
        (
            before.uid(),
            before.gid(),
            before.mode(),
            before.mtime(),
            before.mtime_nsec()
        ),
        (
            after.uid(),
            after.gid(),
            after.mode(),
            after.mtime(),
            after.mtime_nsec()
        )
    );
}

#[test]
fn windows_health_and_status_use_the_instance_units_and_baseline_toolchain() {
    let f = Fixture::new();
    let mut c = config(&f);
    c.systemd_command = Some(Arc::new(|args| {
        if args.contains(&"--state=failed") {
            assert_eq!(args.last(), Some(&"motor-windows-windows-*"));
            Ok(output("motor-job-linux.service loaded failed failed Linux\nmotor-windows-other-a.service loaded failed failed other\nmotor-windows-windows-a.service loaded failed failed Windows\n"))
        } else {
            Ok(output(""))
        }
    }));
    let runner = Runner::open_with_backend(c, Box::new(InProcessWindows)).unwrap();
    let status = runner.runner_status();
    assert_eq!(status.status.rustc_version, "1.99.0");
    assert_eq!(status.health.as_ref().unwrap().failed_job_units, Some(1));
    let shape = serde_json::to_value(status.health.unwrap()).unwrap();
    assert_eq!(shape.as_object().unwrap().len(), 8);
    assert!(shape.get("rustc_version").is_none());
    let mut linux = f.config();
    linux.systemd_command = Some(Arc::new(|args| {
        assert_eq!(args.last(), Some(&"motor-job-*"));
        Ok(output("motor-job-a.service loaded failed failed Linux\nmotor-windows-windows-a.service loaded failed failed Windows\n"))
    }));
    assert_eq!(
        Runner::open(linux)
            .unwrap()
            .runner_status()
            .health
            .unwrap()
            .failed_job_units,
        Some(1)
    );
}

fn seed_disk(vm: &VmConfig, key: &str, last_used: u64, marked: bool) -> PathBuf {
    let caches = vm.caches();
    let disk = caches.disk(key);
    fs::write(&disk, b"fake qcow2 allocated bytes").unwrap();
    caches.attach(key, last_used).unwrap();
    if !marked {
        caches.complete(key, last_used, true, true, true).unwrap();
    }
    disk
}

#[test]
fn windows_reconcile_discards_marked_disks_but_keeps_unattached_disks() {
    let f = Fixture::new();
    let c = config(&f);
    let vm = VmConfig::load(c.vm_config.as_ref().unwrap()).unwrap();
    let dirty = seed_disk(&vm, "dirty", 1, true);
    let clean = seed_disk(&vm, "clean", 1, false);
    let runner = Runner::open_with_backend(c, Box::new(InProcessWindows)).unwrap();
    assert!(!dirty.exists());
    assert!(!vm.caches().marker(&dirty).exists());
    assert!(!dirty.with_extension("json").exists());
    assert!(clean.exists());
    drop(runner);
}

#[test]
fn windows_cache_key_includes_baseline_name_and_toolchain() {
    let f = Fixture::new();
    let c = config(&f);
    let vm_path = c.vm_config.as_ref().unwrap();
    let mut vm = VmConfig::load(vm_path).unwrap();
    let initial = vm.caches().disk(&f.key);
    assert_eq!(initial, vm.caches().disk(&f.key));
    assert_ne!(initial, vm.caches().disk("another workspace"));
    let changed_baseline = vm.layout_root.join("baselines/job-2.qcow2");
    fs::write(&changed_baseline, b"new sealed baseline").unwrap();
    fs::set_permissions(changed_baseline, fs::Permissions::from_mode(0o440)).unwrap();
    let contents = fs::read_to_string(vm_path)
        .unwrap()
        .replace("job-1.qcow2", "job-2.qcow2");
    fs::write(vm_path, &contents).unwrap();
    vm = VmConfig::load(vm_path).unwrap();
    let new_baseline = vm.caches().disk(&f.key);
    assert_ne!(initial, new_baseline);
    fs::write(vm_path, contents.replace("1.99.0", "1.100.0")).unwrap();
    vm = VmConfig::load(vm_path).unwrap();
    assert_ne!(new_baseline, vm.caches().disk(&f.key));
}

#[test]
fn windows_cache_reuse_requires_clean_dismount_normal_exit_and_known_outcome() {
    let f = Fixture::new();
    let c = config(&f);
    let vm = VmConfig::load(c.vm_config.as_ref().unwrap()).unwrap();
    for (known, clean, normal) in [
        (true, true, true),
        (false, true, true),
        (true, false, true),
        (true, true, false),
    ] {
        let key = format!("{known}-{clean}-{normal}");
        let disk = seed_disk(&vm, &key, 1, true);
        assert!(vm.caches().marker(&disk).exists());
        vm.caches().complete(&key, 9, known, clean, normal).unwrap();
        assert_eq!(disk.exists(), known && clean && normal);
        assert!(!vm.caches().marker(&disk).exists());
        if disk.exists() {
            assert_eq!(vm.caches().last_used(&disk).unwrap(), 9);
        }
    }
}

#[test]
fn windows_pressure_is_lru_and_never_evicts_marked_recent_or_owned_disks() {
    let f = Fixture::new();
    let mut c = config(&f);
    let vm = VmConfig::load(c.vm_config.as_ref().unwrap()).unwrap();
    let now = f.clock.load(Ordering::SeqCst);
    let free = Arc::new(AtomicU64::new(1000));
    let free_probe = free.clone();
    c.free_space_floor_bytes = 100;
    let oldest = vm.caches().disk("oldest");
    let probe_oldest = oldest.clone();
    c.free_space_probe = Arc::new(move |_| {
        let free = free_probe.load(Ordering::SeqCst);
        Ok(if free == 1 && !probe_oldest.exists() {
            200
        } else {
            free
        })
    });
    let runner = Runner::open_with_backend(c, Box::new(InProcessWindows)).unwrap();
    assert_eq!(seed_disk(&vm, "oldest", now - 2_000_000, false), oldest);
    let younger = seed_disk(&vm, "younger", now - 1_800_001, false);
    let marked = seed_disk(&vm, "marked", now - 2_000_000, true);
    let recent = seed_disk(&vm, "recent", now - 1_800_000, false);
    let (mut request, _) = f.run("not queued");
    request.platform = Some(Platform::Windows);
    assert!(matches!(
        runner.accept(request.clone()).unwrap(),
        Admission::Accepted(_)
    ));
    let live = seed_disk(&vm, &request.workspace_key, now - 2_000_000, false);
    let admitting = seed_disk(&vm, "admitting", now - 2_000_000, false);
    let mut pending = request.clone();
    pending.workspace_key = "admitting".into();
    let admission = runner.protect_admission(&mut runner.core.inner.lock().unwrap(), &pending);
    free.store(1, Ordering::SeqCst);
    assert!(runner.ensure_disk_space().unwrap().is_none());
    assert!(!oldest.exists());
    assert!(younger.exists());
    assert_eq!(
        runner
            .runner_status()
            .disk
            .unwrap()
            .last_eviction
            .unwrap()
            .path,
        oldest.to_string_lossy()
    );
    free.store(0, Ordering::SeqCst);
    assert!(runner.ensure_disk_space().unwrap().is_some());
    assert!(!younger.exists());
    for disk in [marked, recent, live, admitting] {
        assert!(disk.exists(), "{} must be protected", disk.display());
    }
    drop(admission);
}

#[test]
fn windows_reconcile_stops_orphan_units_and_uses_windows_names_for_status() {
    let f = Fixture::new();
    let mut c = config(&f);
    let unit_id = Uuid::now_v7();
    let name = format!("motor-windows-windows-{unit_id}.service");
    let events = Arc::new(Mutex::new(Vec::new()));
    let log = events.clone();
    let orphan_name = format!("motor-windows-windows-{}.service", Uuid::now_v7());
    let orphan = orphan_name.clone();
    c.systemd_command = Some(Arc::new(move |args| {
        log.lock()
            .unwrap()
            .push(args.iter().map(|s| s.to_string()).collect::<Vec<_>>());
        Ok(if args[0] == "list-units" {
            output(&format!("{orphan} loaded active running orphan\nmotor-job-linux.service loaded active running Linux\n"))
        } else if args[0] == "show" {
            output("LoadState=loaded\nActiveState=active\nSubState=running\n")
        } else {
            output("")
        })
    }));
    let now = (c.clock)();
    let mut booting = Job::new(Uuid::now_v7(), None, now, 0, 4);
    booting.setup_ms = Some(now);
    let mut started = Job::new(Uuid::now_v7(), None, now, 1, 4);
    started.state = JobState::Running;
    started.started_ms = Some(now);
    started.process = Some(platform::ProcessIdentity {
        pid: 0,
        start: 0,
        unit: Some(unit_id),
    });
    for job in [&booting, &started] {
        atomic_json(
            &c.state_dir()
                .join("jobs")
                .join(job.id.to_string())
                .join("record.json"),
            job,
        )
        .unwrap();
    }
    let runner = Runner::open_with_backend(c, Box::new(InProcessWindows)).unwrap();
    let inner = runner.core.inner.lock().unwrap();
    assert!(matches!(
        inner.jobs[&booting.id].terminal.as_ref().unwrap().outcome,
        Outcome::RefusedBeforeStart {
            reason: RefusalReason::LostToRestart
        }
    ));
    assert_eq!(
        inner.jobs[&started.id].terminal.as_ref().unwrap().outcome,
        Outcome::OutcomeUnknown
    );
    drop(inner);
    let status = runner.job_status(started.id).unwrap();
    assert_eq!(status.unit, Some(name.clone()));
    assert_eq!(status.unit_alive, Some(true));
    WindowsBackend
        .cancel(runner.config(), &started, true)
        .unwrap();
    let events = events.lock().unwrap();
    assert_eq!(events[0].last().unwrap(), "motor-windows-windows-*");
    assert_eq!(events[1], ["stop", &orphan_name]);
    assert!(events[2].iter().any(|arg| arg == &name));
    assert!(!events
        .iter()
        .any(|args| args.iter().any(|s| s == "motor-job-linux.service")));
    assert!(runner.replay(started.id, 0).unwrap().iter().any(|frame| matches!(frame, Frame::Output(out) if String::from_utf8_lossy(&out.bytes.0).contains(&name))));
}

fn host_fixture(f: &Fixture, fault: &'static str) -> Config {
    let mut c = config(f);
    let vm = VmConfig::load(c.vm_config.as_ref().unwrap()).unwrap();
    let root = vm.layout_root.clone();
    fs::write(root.parent().unwrap().join("slot.lock"), b"").unwrap();
    for path in [
        root.parent().unwrap().to_path_buf(),
        root.clone(),
        root.join("vm"),
        root.join("vm/sockets"),
    ] {
        fs::set_permissions(path, fs::Permissions::from_mode(0o711)).unwrap();
    }
    for path in [
        root.join("state"),
        root.join("state/windows"),
        root.join("staging"),
        root.join("vm/overlays"),
        root.join("vm/caches"),
    ] {
        fs::set_permissions(path, fs::Permissions::from_mode(0o710)).unwrap();
    }
    fs::set_permissions(root.join("baselines"), fs::Permissions::from_mode(0o750)).unwrap();
    fs::set_permissions(
        root.parent().unwrap().join("slot.lock"),
        fs::Permissions::from_mode(0o660),
    )
    .unwrap();
    c.windows_command = Some(Arc::new(move |program, args| {
        if program == "/usr/bin/getent" {
            let name = &args[1];
            if fault == "missing_account" && name == "motor-qemu" {
                return Ok(std::process::Output {
                    status: std::process::ExitStatus::from_raw(256),
                    stdout: Vec::new(),
                    stderr: b"missing motor-qemu".to_vec(),
                });
            }
            let uid = if name == "motor-qemu" || fault == "shared_account" {
                60001
            } else {
                60002
            };
            let shell = if fault == "login_shell" {
                "/bin/bash"
            } else {
                "/usr/sbin/nologin"
            };
            return Ok(output(&format!(
                "{name}:x:{uid}:{uid}::/nonexistent:{shell}\n"
            )));
        }
        if program == "/usr/bin/id" {
            return Ok(output(if args[0] == "-u" {
                "60003"
            } else if fault == "admin_group" {
                "motor-prep sudo"
            } else {
                "motor-qemu kvm"
            }));
        }
        if program == "/usr/bin/sudo" {
            if args.contains(&"-U".to_string()) {
                return Ok(if fault == "sudo_grant" {
                    output("(ALL) NOPASSWD: ALL")
                } else {
                    std::process::Output {
                        status: std::process::ExitStatus::from_raw(256),
                        stdout: b"account is not allowed to run sudo".to_vec(),
                        stderr: Vec::new(),
                    }
                });
            }
            if fault == "runner_sudo"
                || (fault == "mirror_acl" && args.contains(&"/usr/bin/setfacl".to_string()))
            {
                return Ok(std::process::Output {
                    status: std::process::ExitStatus::from_raw(256),
                    stdout: Vec::new(),
                    stderr: b"sudo denied".to_vec(),
                });
            }
            return Ok(output(""));
        }
        if program == "/usr/bin/getfacl" {
            let path = Path::new(args.last().unwrap());
            let acl = if path == vm.baseline_path() {
                "user::r--\nuser:60001:r--\ngroup::r--\nmask::r--\nother::---\n"
            } else if path == root.join("baselines") {
                if fault == "missing_acl" {
                    "user::rwx\ngroup::r-x\nother::---\n"
                } else {
                    "user::rwx\nuser:60001:r-x\ngroup::r-x\nmask::r-x\nother::---\n"
                }
            } else if path == root.join("vm/overlays") || path == root.join("vm/caches") {
                "user::rwx\nuser:60001:--x\ngroup::---\nmask::--x\nother::---\n"
            } else if path == root.join("state")
                || path == root.join("state/windows")
                || path == root.join("staging")
            {
                if fault == "extra_acl" {
                    "user::rwx\nuser:60002:--x\nuser:60001:rwx\ngroup::---\nmask::rwx\nother::---\n"
                } else {
                    "user::rwx\nuser:60002:--x\ngroup::---\nmask::--x\nother::---\n"
                }
            } else if path == root.join("state/windows/jobs") {
                "user::rwx\ngroup::---\nother::---\n"
            } else if path.ends_with("slot.lock") {
                "user::rw-\nuser:60001:rw-\ngroup::---\nmask::rw-\nother::---\n"
            } else {
                "user::rwx\ngroup::--x\nother::--x\n"
            };
            return Ok(output(acl));
        }
        assert_eq!(program, "/usr/bin/find");
        assert!(args.contains(&"-P".to_string()));
        assert!(args.contains(&"u:motor-prep:rX".to_string()));
        Ok(output(""))
    }));
    c
}

#[test]
fn windows_startup_fails_closed_on_accounts_sudo_and_host_layout_acls() {
    for fault in [
        "missing_account",
        "shared_account",
        "login_shell",
        "admin_group",
        "sudo_grant",
        "runner_sudo",
        "missing_acl",
        "extra_acl",
        "mirror_acl",
    ] {
        let f = Fixture::new();
        let c = host_fixture(&f, fault);
        let error = Runner::open(c).err().expect("unsafe startup must fail");
        let expected = match fault {
            "missing_account" => "missing motor-qemu",
            "shared_account" => "distinct accounts",
            "login_shell" => "non-login shell",
            "admin_group" => "administrative group",
            "sudo_grant" => "no sudo grants",
            "runner_sudo" | "mirror_acl" => "sudo denied",
            "missing_acl" | "extra_acl" => "host-layout ACL",
            _ => unreachable!(),
        };
        assert!(error.to_string().contains(expected), "{fault}: {error}");
    }
    let f = Fixture::new();
    let runner = Runner::open(host_fixture(&f, "none")).unwrap();
    assert_eq!(runner.config().max_weight, 4);
    assert_eq!(runner.status().rustc_version, "1.99.0");
    assert!(runner.config().workspace_roots.is_empty());
}

#[test]
fn windows_startup_rejects_symlinked_layout_roots_and_different_state_roots() {
    let f = Fixture::new();
    let mut c = host_fixture(&f, "none");
    c.state_root = f.scratch.join("elsewhere");
    assert!(Runner::open(c)
        .err()
        .unwrap()
        .to_string()
        .contains("--state-root"));
    let c = host_fixture(&f, "none");
    let vm = VmConfig::load(c.vm_config.as_ref().unwrap()).unwrap();
    fs::remove_dir(vm.layout_root.join("vm/caches")).unwrap();
    std::os::unix::fs::symlink(
        vm.layout_root.join("vm/overlays"),
        vm.layout_root.join("vm/caches"),
    )
    .unwrap();
    assert!(Runner::open(c)
        .err()
        .unwrap()
        .to_string()
        .contains("unsafe Windows host-layout path"));
}
