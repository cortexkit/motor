use super::*;
use cortexkit_test_support::ScratchDir;
use std::sync::Barrier;

#[derive(Default)]
struct Fake {
    services: Mutex<Vec<Service>>,
    processes: Mutex<BTreeMap<i32, ProcessIdentity>>,
    killed: Mutex<Vec<ProcessIdentity>>,
    stop_failure: AtomicBool,
    delete_failure: AtomicBool,
}
fn identity(pid: i32) -> ProcessIdentity {
    ProcessIdentity {
        pid,
        start_seconds: 10,
        start_microseconds: 20,
    }
}
impl Host for Fake {
    fn services(&self) -> io::Result<Vec<Service>> {
        Ok(self.services.lock().unwrap().clone())
    }
    fn alive(&self, process: ProcessIdentity) -> io::Result<bool> {
        Ok(self.processes.lock().unwrap().get(&process.pid) == Some(&process))
    }
    fn kill(&self, process: ProcessIdentity) -> io::Result<()> {
        if self.stop_failure.load(Ordering::Acquire) {
            return Err(invalid("injected VM stop failure"));
        }
        if self.alive(process)? {
            self.killed.lock().unwrap().push(process);
            self.processes.lock().unwrap().remove(&process.pid);
            self.services
                .lock()
                .unwrap()
                .retain(|s| s.identity != process);
        }
        Ok(())
    }
    fn release_proven(&self, _: &Record) -> io::Result<bool> {
        Ok(true)
    }
    fn delete(&self, path: &Path) -> io::Result<()> {
        if self.delete_failure.load(Ordering::Acquire) {
            return Err(invalid("injected delete failure"));
        }
        remove(path)
    }
}
fn new_runtime(host: Arc<Fake>) -> Arc<Runtime> {
    Arc::new(Runtime {
        host,
        jobs: Mutex::default(),
        cancelled: Mutex::default(),
        runner_lock: Mutex::default(),
    })
}
fn begin(runtime: &Runtime, root: &Path, index: usize) -> Arc<Entry> {
    runtime
        .begin(root, Uuid::now_v7(), &format!("/Users/motor/repo{index}"))
        .unwrap()
}
fn boot(runtime: &Runtime, host: &Fake, root: &Path, entry: &Entry, pid: i32) {
    let process = identity(pid);
    host.processes.lock().unwrap().insert(pid, process);
    host.services.lock().unwrap().push(Service {
        identity: process,
        owned: true,
    });
    let mut owned = entry.owned.lock().unwrap();
    owned.record.gate_open = true;
    owned.record.service = Some(process);
    Runtime::save(root, &owned.record).unwrap();
    assert!(runtime.host.alive(process).unwrap());
}
fn make_paths(entry: &Entry) {
    let paths = entry.owned.lock().unwrap().record.paths.clone();
    for path in [
        &paths.clone,
        &paths.tree,
        &paths.cargo,
        &paths.prep,
        &paths.target_tmp,
    ] {
        std::fs::create_dir_all(path).unwrap();
        std::fs::write(path.join("resource"), "owned").unwrap();
    }
    std::fs::create_dir_all(paths.nonce.parent().unwrap()).unwrap();
    std::fs::write(paths.nonce, "secret").unwrap();
}
fn assert_gone(entry: &Entry) {
    for path in entry.owned.lock().unwrap().record.paths.all() {
        assert!(!path.exists(), "{} survives", path.display());
    }
}

#[test]
fn four_jobs_hold_at_most_two_vm_slots() {
    let scratch = ScratchDir::new("macos-two-vms");
    let root = scratch.join("state");
    let host = Arc::new(Fake::default());
    let runtime = new_runtime(host.clone());
    let entries: Vec<_> = (0..4).map(|i| begin(&runtime, &root, i)).collect();
    let barrier = Arc::new(Barrier::new(4));
    let handles: Vec<_> = entries
        .iter()
        .cloned()
        .map(|entry| {
            let runtime = runtime.clone();
            let root = root.clone();
            let barrier = barrier.clone();
            std::thread::spawn(move || {
                barrier.wait();
                runtime.admit(&root, &entry).unwrap()
            })
        })
        .collect();
    let admitted: Vec<_> = handles.into_iter().map(|h| h.join().unwrap()).collect();
    assert_eq!(admitted.iter().filter(|yes| **yes).count(), 2);
    for (i, entry) in entries.iter().enumerate().filter(|(i, _)| admitted[*i]) {
        boot(&runtime, &host, &root, entry, 100 + i as i32);
    }
    assert_eq!(host.services.lock().unwrap().len(), 2);
    for (i, entry) in entries.iter().enumerate().filter(|(i, _)| admitted[*i]) {
        runtime.finish(&root, entry);
        assert!(!runtime.cleanup_pending(entry.owned.lock().unwrap().record.id));
        assert!(host.services.lock().unwrap().len() <= 2, "job {i}");
    }
    for entry in entries
        .iter()
        .enumerate()
        .filter(|(i, _)| !admitted[*i])
        .map(|(_, e)| e)
    {
        assert!(runtime.admit(&root, entry).unwrap());
    }
    assert_eq!(
        runtime
            .jobs
            .lock()
            .unwrap()
            .values()
            .filter(|e| e.owned.lock().unwrap().record.slot.is_some())
            .count(),
        2
    );
}
#[test]
fn concurrent_admission_counts_one_foreign_vm_only_once_under_lock() {
    let scratch = ScratchDir::new("macos-foreign-race");
    let root = scratch.join("state");
    let host = Arc::new(Fake::default());
    host.services.lock().unwrap().push(Service {
        identity: identity(99),
        owned: false,
    });
    let runtime = new_runtime(host);
    let barrier = Arc::new(Barrier::new(2));
    let handles: Vec<_> = (0..2)
        .map(|i| {
            let entry = begin(&runtime, &root, i);
            let runtime = runtime.clone();
            let barrier = barrier.clone();
            let root = root.clone();
            std::thread::spawn(move || {
                barrier.wait();
                runtime.admit(&root, &entry).unwrap()
            })
        })
        .collect();
    assert_eq!(
        handles
            .into_iter()
            .map(|h| h.join().unwrap())
            .filter(|yes| *yes)
            .count(),
        1
    );
}
#[test]
fn foreign_vm_and_cancel_frees_queued_slot_within_two_seconds() {
    let scratch = ScratchDir::new("macos-foreign-cancel");
    let root = scratch.join("state");
    let host = Arc::new(Fake::default());
    host.services.lock().unwrap().push(Service {
        identity: identity(99),
        owned: false,
    });
    let runtime = new_runtime(host.clone());
    let first = begin(&runtime, &root, 0);
    let queued = begin(&runtime, &root, 1);
    assert!(runtime.admit(&root, &first).unwrap());
    boot(&runtime, &host, &root, &first, 100);
    assert!(!runtime.admit(&root, &queued).unwrap());
    let started = Instant::now();
    runtime.cancel(first.owned.lock().unwrap().record.id);
    runtime.finish(&root, &first);
    // The caller's Entry can outlive cleanup; successful cleanup must release its lock too.
    assert!(runtime.admit(&root, &queued).unwrap());
    assert!(started.elapsed() < Duration::from_secs(2));
}
#[test]
fn record_precedes_first_resource_and_recovery_removes_only_derived_paths() {
    for window in ["first-resource", "helper-before-gate", "copy-back"] {
        let scratch = ScratchDir::new(window);
        let root = scratch.join("state");
        let outside = scratch.join("sentinel");
        std::fs::write(&outside, "unchanged").unwrap();
        let host = Arc::new(Fake::default());
        let runtime = new_runtime(host.clone());
        let entry = begin(&runtime, &root, 0);
        let mut record = entry.owned.lock().unwrap().record.clone();
        assert!(Runtime::record_path(&root, record.id).is_file());
        assert!(!record.paths.tree.exists());
        make_paths(&entry);
        if window == "helper-before-gate" {
            record.helper = Some(identity(700));
            host.processes.lock().unwrap().insert(700, identity(700));
        }
        if window == "copy-back" {
            record.stop_proven = true;
            let target = root.join("targets").join(key(&record.workspace));
            std::fs::create_dir_all(&target).unwrap();
            std::fs::write(target.join("whole"), "old store").unwrap();
        }
        Runtime::save(&root, &record).unwrap();
        drop(entry);
        drop(runtime);
        let recovered = new_runtime(host.clone());
        recovered.recover(&root).unwrap();
        assert!(recovered.jobs.lock().unwrap().is_empty());
        assert!(!host.alive(identity(700)).unwrap());
        for path in record.paths.all() {
            assert!(!path.exists(), "{window}: {}", path.display());
        }
        if window == "copy-back" {
            assert_eq!(
                std::fs::read_to_string(
                    root.join("targets")
                        .join(key(&record.workspace))
                        .join("whole")
                )
                .unwrap(),
                "old store"
            );
        }
        assert_eq!(std::fs::read_to_string(&outside).unwrap(), "unchanged");
    }
}
#[test]
fn stop_and_delete_failures_retain_records_slots_and_degraded_across_restart() {
    for stop in [true, false] {
        let scratch = ScratchDir::new("macos-cleanup-failure");
        let root = scratch.join("state");
        let sentinel = scratch.join("sentinel");
        std::fs::write(&sentinel, "safe").unwrap();
        let host = Arc::new(Fake::default());
        let runtime = new_runtime(host.clone());
        let entry = begin(&runtime, &root, 0);
        assert!(runtime.admit(&root, &entry).unwrap());
        make_paths(&entry);
        boot(&runtime, &host, &root, &entry, 100);
        host.stop_failure.store(stop, Ordering::Release);
        host.delete_failure.store(!stop, Ordering::Release);
        runtime.finish(&root, &entry);
        assert!(runtime.degraded());
        let record = entry.owned.lock().unwrap().record.clone();
        assert!(Runtime::record_path(&root, record.id).exists());
        assert!(record.slot.is_some());
        drop(entry);
        drop(runtime);
        let runtime = new_runtime(host.clone());
        runtime.recover(&root).unwrap();
        assert!(runtime.degraded());
        let entry = runtime.entry(record.id).unwrap();
        let slot = lock_file(
            &root
                .join("macvm-locks")
                .join(format!("slot-{}.lock", record.slot.unwrap())),
        )
        .unwrap();
        assert!(fs2::FileExt::try_lock_exclusive(&slot).is_err());
        assert_eq!(std::fs::read_to_string(&sentinel).unwrap(), "safe");
        host.stop_failure.store(false, Ordering::Release);
        host.delete_failure.store(false, Ordering::Release);
        let config = Config::new(&root, "macos");
        runtime.retry_cleanups(&config);
        assert!(!runtime.degraded());
        assert_gone(&entry);
        assert!(!Runtime::record_path(&root, record.id).exists());
    }
}
#[test]
fn unproven_stop_retries_without_operator_when_motor_owned_unrecorded_service_disappears() {
    let scratch = ScratchDir::new("macos-unproven");
    let root = scratch.join("state");
    let host = Arc::new(Fake::default());
    let runtime = new_runtime(host.clone());
    let entry = begin(&runtime, &root, 0);
    assert!(runtime.admit(&root, &entry).unwrap());
    make_paths(&entry);
    {
        let mut owned = entry.owned.lock().unwrap();
        owned.record.gate_open = true;
        Runtime::save(&root, &owned.record).unwrap();
    }
    host.services.lock().unwrap().push(Service {
        identity: identity(900),
        owned: true,
    });
    runtime.finish(&root, &entry);
    assert!(runtime.degraded());
    let id = entry.owned.lock().unwrap().record.id;
    drop(entry);
    drop(runtime);
    let runtime = new_runtime(host.clone());
    runtime.recover(&root).unwrap();
    assert!(runtime.degraded());
    assert!(runtime
        .entry(id)
        .unwrap()
        .owned
        .lock()
        .unwrap()
        .record
        .paths
        .clone
        .exists());
    host.services.lock().unwrap().clear();
    runtime.retry_cleanups(&Config::new(&root, "macos"));
    assert!(!runtime.degraded());
    assert!(runtime.entry(id).is_none());
    assert!(!root.join("jobs").join(id.to_string()).exists());
}
#[test]
fn recovery_never_kills_reused_pid_with_different_start_time() {
    let scratch = ScratchDir::new("macos-pid-reuse");
    let root = scratch.join("state");
    let host = Arc::new(Fake::default());
    let runtime = new_runtime(host.clone());
    let entry = begin(&runtime, &root, 0);
    {
        let mut owned = entry.owned.lock().unwrap();
        owned.record.helper = Some(identity(7));
        Runtime::save(&root, &owned.record).unwrap();
    }
    let reused = ProcessIdentity {
        start_seconds: 11,
        ..identity(7)
    };
    host.processes.lock().unwrap().insert(7, reused);
    drop(entry);
    drop(runtime);
    let runtime = new_runtime(host.clone());
    runtime.recover(&root).unwrap();
    assert!(host.alive(reused).unwrap());
    assert!(host.killed.lock().unwrap().is_empty());
}
#[test]
fn cleanup_refuses_a_record_with_any_unowned_path() {
    let scratch = ScratchDir::new("macos-record-path");
    let root = scratch.join("state");
    let sentinel = scratch.join("sentinel");
    std::fs::write(&sentinel, "safe").unwrap();
    let host = Arc::new(Fake::default());
    let runtime = new_runtime(host);
    let entry = begin(&runtime, &root, 0);
    entry.owned.lock().unwrap().record.paths.nonce = sentinel.clone();
    runtime.finish(&root, &entry);
    assert!(runtime.degraded());
    assert_eq!(std::fs::read_to_string(sentinel).unwrap(), "safe");
}

#[cfg(target_os = "linux")]
#[test]
#[ignore = "subprocess fixture invoked only by runner_sigkill_crash_windows_recover_resources_and_whole_target"]
fn crash_process_fixture() {
    use std::os::unix::fs::PermissionsExt;
    let root = PathBuf::from(std::env::var_os("MOTOR_MACOS_CRASH_ROOT").expect("fixture root"));
    let ready = PathBuf::from(std::env::var_os("MOTOR_MACOS_CRASH_READY").expect("fixture ready"));
    let window = std::env::var("MOTOR_MACOS_CRASH_WINDOW").unwrap();
    let runtime = Runtime::default();
    let id = Uuid::now_v7();
    let entry = runtime.begin(&root, id, "/Users/test/repo").unwrap();
    let paths = entry.owned.lock().unwrap().record.paths.clone();
    std::fs::create_dir(&paths.clone).unwrap();
    let ready_now = |helper: u32| {
        std::fs::write(&ready, format!("{id} {helper}")).unwrap();
        loop {
            std::thread::park();
        }
    };
    if window == "first-resource" {
        ready_now(0);
    }
    if window == "helper-before-gate" {
        let slot = lock_file(&root.join("macvm-locks/slot-0.lock")).unwrap();
        fs2::FileExt::lock_exclusive(&slot).unwrap();
        {
            let mut owned = entry.owned.lock().unwrap();
            owned.record.slot = Some(0);
            Runtime::save(&root, &owned.record).unwrap();
        }
        let script = root.join("fixture-helper");
        std::fs::write(&script, "#!/bin/sh\nIFS= read -r gate\nexit 0\n").unwrap();
        std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o700)).unwrap();
        let share = |tag: &str, path: &str| motor_macvm::ShareLocation {
            host: root.join(tag),
            tag: tag.into(),
            guest: path.into(),
        };
        let plan = motor_macvm::JobVmConfig::new(motor_macvm::JobVmRequest {
            clone: paths.clone,
            nonce_disk: paths.nonce,
            workspace: share("workspace", "/Users/test/repo"),
            siblings: vec![],
            cargo_home: share("cargo-home", motor_macvm::CARGO_HOME),
        })
        .unwrap();
        let helper = motor_macvm::spawn_job_helper(&script, &plan, &slot).unwrap();
        ready_now(helper.child.id());
    }
    assert_eq!(window, "copy-back");
    let workspace = paths.tree.join("Users/test/repo");
    std::fs::create_dir_all(workspace.join("target")).unwrap();
    std::fs::write(workspace.join("target/whole"), "new").unwrap();
    let store = root.join("targets").join(key("/Users/test/repo"));
    std::fs::create_dir(&store).unwrap();
    std::fs::write(store.join("whole"), "old").unwrap();
    {
        let mut owned = entry.owned.lock().unwrap();
        owned.record.stop_proven = true;
        Runtime::save(&root, &owned.record).unwrap();
    }
    let mut config = Config::new(&root, "macos");
    config.long_work = Some(Arc::new(move |phase| {
        if phase == "macos-target-before-publish" {
            std::fs::write(&ready, format!("{id} 0")).unwrap();
            loop {
                std::thread::park();
            }
        }
    }));
    super::super::execution::target_out(&config, &entry, &workspace).unwrap();
    panic!("copy-back crash hook not reached");
}
#[cfg(target_os = "linux")]
#[test]
fn runner_sigkill_crash_windows_recover_resources_and_whole_target() {
    for window in ["first-resource", "helper-before-gate", "copy-back"] {
        let scratch = ScratchDir::new("macos-sigkill");
        let root = scratch.join("state");
        let ready = scratch.join("ready");
        let outside = scratch.join("outside");
        std::fs::write(&outside, "unchanged").unwrap();
        let mut child = std::process::Command::new(std::env::current_exe().unwrap())
            .args([
                "--exact",
                "macos::lifecycle::tests::crash_process_fixture",
                "--ignored",
                "--nocapture",
            ])
            .env("MOTOR_MACOS_CRASH_ROOT", &root)
            .env("MOTOR_MACOS_CRASH_READY", &ready)
            .env("MOTOR_MACOS_CRASH_WINDOW", window)
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::piped())
            .spawn()
            .unwrap();
        let deadline = Instant::now() + Duration::from_secs(10);
        while !ready.exists() {
            if child.try_wait().unwrap().is_some() {
                let output = child.wait_with_output().unwrap();
                panic!(
                    "crash fixture exited: {}",
                    String::from_utf8_lossy(&output.stderr)
                );
            }
            assert!(
                Instant::now() < deadline,
                "crash fixture never reached {window}"
            );
            std::thread::park_timeout(Duration::from_millis(5));
        }
        let text = std::fs::read_to_string(&ready).unwrap();
        let mut fields = text.split_whitespace();
        let id: Uuid = fields.next().unwrap().parse().unwrap();
        let helper: i32 = fields.next().unwrap().parse().unwrap();
        child.kill().unwrap();
        child.wait().unwrap();
        let runtime = new_runtime(Arc::new(Fake::default()));
        runtime.recover(&root).unwrap();
        assert!(!runtime.degraded());
        for path in Paths::new(&root, id).all() {
            assert!(!path.exists(), "{window}: {}", path.display());
        }
        if helper != 0 {
            let deadline = Instant::now() + Duration::from_secs(2);
            loop {
                let live = std::fs::read_to_string(format!("/proc/{helper}/stat"))
                    .ok()
                    .is_some_and(|text| {
                        text.rsplit_once(") ")
                            .is_some_and(|(_, fields)| !fields.starts_with('Z'))
                    });
                if !live {
                    break;
                }
                assert!(
                    Instant::now() < deadline,
                    "gate-blocked helper survived runner SIGKILL"
                );
                std::thread::park_timeout(Duration::from_millis(5));
            }
            let slot = lock_file(&root.join("macvm-locks/slot-0.lock")).unwrap();
            assert!(fs2::FileExt::try_lock_exclusive(&slot).is_ok());
        }
        if window == "copy-back" {
            assert_eq!(
                std::fs::read_to_string(
                    root.join("targets")
                        .join(key("/Users/test/repo"))
                        .join("whole")
                )
                .unwrap(),
                "old"
            );
        }
        assert_eq!(std::fs::read_to_string(&outside).unwrap(), "unchanged");
    }
}
