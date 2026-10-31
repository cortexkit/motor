use super::*;
use crate::test_helpers::CanonicalScratchDir as ScratchDir;
use std::sync::Barrier;

#[derive(Default)]
struct Fake {
    services: Mutex<Vec<Service>>,
    processes: Mutex<BTreeMap<i32, ProcessIdentity>>,
    killed: Mutex<Vec<ProcessIdentity>>,
    stop_failure: AtomicBool,
    delete_failure: AtomicBool,
    delete_errors: Mutex<Vec<(PathBuf, String)>>,
    disk_held: AtomicBool,
    scans: std::sync::atomic::AtomicUsize,
    orphan: Mutex<Option<ProcessIdentity>>,
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
    fn release_proven(&self, record: &Record) -> io::Result<bool> {
        release_after_gate(record, || {
            self.scans.fetch_add(1, Ordering::AcqRel);
            Ok(!self.disk_held.load(Ordering::Acquire))
        })
    }
    fn stop_unrecorded(&self, _: &Record) -> io::Result<()> {
        let process = self.orphan.lock().unwrap().take();
        if let Some(process) = process {
            self.kill(process)?;
        }
        Ok(())
    }
    fn delete(&self, path: &Path) -> io::Result<()> {
        if self.delete_failure.load(Ordering::Acquire) {
            return Err(invalid("injected delete failure"));
        }
        let result = remove(path);
        if let Err(error) = &result {
            self.delete_errors
                .lock()
                .unwrap()
                .push((path.to_owned(), error.to_string()));
        }
        result
    }
}
fn new_runtime(host: Arc<Fake>) -> Arc<Runtime> {
    Arc::new(Runtime {
        host,
        jobs: Mutex::default(),
        cancelled: Mutex::default(),
        changes: Mutex::default(),
        returns: Mutex::default(),
        workspace_reads: Mutex::default(),
        runner_lock: Mutex::default(),
        grace: Duration::from_millis(30),
        stop_timeout: Duration::from_millis(120),
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
fn returned_files_wait_for_stop_and_saved_proof_then_survive_cleanup() {
    for failure in ["none", "stop", "disk", "proof-save"] {
        for chunked in [false, true] {
            let scratch = ScratchDir::new("macos-return-stop");
            let root = scratch.join("state");
            let host = Arc::new(Fake::default());
            let runtime = new_runtime(host.clone());
            let mut config = crate::engine::Config::new(&root, "macos");
            config.macos_runtime = runtime.clone();
            let entry = begin(&runtime, &root, 0);
            let record = entry.owned.lock().unwrap().record.clone();
            boot(&runtime, &host, &root, &entry, 100);
            let tree = record.paths.tree.clone();
            let workspace = tree.join("workspace");
            std::fs::create_dir_all(&workspace).unwrap();
            std::fs::write(workspace.join("artifact"), b"x").unwrap();
            let trees = crate::workspace::Trees {
                state: root.join("macos"),
                mirrors: root.join("mirrors"),
                compacting: Default::default(),
                mirror_locks: Default::default(),
            };
            let staged = super::super::Prepared {
                cargo_home: tree.join("cargo-home"),
                tree,
                workspace,
                siblings: vec![],
                prepared_javascript_directories: vec![],
            };
            let motor_protocol::Frame::Run(mut request) = serde_json::from_str(include_str!(
                "../../../../test-vectors/exec-remote-v1/frames/run.json"
            ))
            .unwrap() else {
                unreachable!()
            };
            request.job_id = record.id;
            request.workspace_key = record.workspace.clone();
            request.return_paths = vec![format!("{}/artifact", record.workspace)];
            request.return_chunked = chunked;
            match failure {
                "stop" => host.stop_failure.store(true, Ordering::Release),
                "disk" => host.disk_held.store(true, Ordering::Release),
                "proof-save" => {
                    let path = Runtime::record_path(&root, record.id);
                    std::fs::remove_file(&path).unwrap();
                    std::fs::create_dir(&path).unwrap();
                }
                _ => {}
            }
            super::super::execution::capture_completion(&config, &trees, &entry, &request, &staged);
            assert!(
                entry.active.load(Ordering::Acquire),
                "ownership retained through captures"
            );
            if failure == "none" {
                let saved: Record =
                    crate::storage::read_json(&Runtime::record_path(&root, record.id)).unwrap();
                assert!(saved.stop_proven);
                assert!(host.scans.load(Ordering::Acquire) > 0);
                assert!(!runtime.host.alive(identity(100)).unwrap());
                assert_eq!(
                    *runtime.workspace_reads.lock().unwrap(),
                    vec![(record.id, "changes"), (record.id, "returns")]
                );
                runtime.finish(&root, &entry);
                assert!(!staged.workspace.exists());
            } else {
                assert!(!entry.owned.lock().unwrap().record.stop_proven);
                assert!(
                    runtime.workspace_reads.lock().unwrap().is_empty(),
                    "failed stop must perform zero workspace reads"
                );
                assert!(runtime.take_changes(record.id).is_none());
                assert!(staged.workspace.exists());
            }
            let collected = runtime.take_returns(record.id).unwrap();
            assert!(runtime.take_returns(record.id).is_none());
            match collected {
                crate::return_paths::Collection::Legacy(result) if failure == "none" => {
                    assert_eq!(result.returned_files.unwrap()[0].data, "eA==");
                }
                crate::return_paths::Collection::Legacy(result) => {
                    assert_eq!(
                        result.return_paths_failed.unwrap().cause,
                        motor_protocol::ReturnFailureCause::ReadError
                    );
                    assert!(result.returned_files.is_none());
                }
                crate::return_paths::Collection::Chunked(collection) if failure == "none" => {
                    assert_eq!(
                        std::fs::read(
                            trees
                                .state
                                .join("jobs")
                                .join(record.id.to_string())
                                .join(collection.directory)
                                .join("chunk-0")
                        )
                        .unwrap(),
                        b"x"
                    );
                }
                crate::return_paths::Collection::Chunked(collection) => {
                    assert!(collection.chunks.is_empty());
                    assert_eq!(
                        collection.files[0].cause(),
                        Some(&motor_protocol::ReturnPathsFailureCause::ReadError)
                    );
                }
            }
        }
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
fn stop_failure_retains_record_slot_and_degraded_across_restart() {
    {
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
        host.stop_failure.store(true, Ordering::Release);
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

#[test]
fn permanent_delete_failure_quarantines_without_holding_vm_capacity_across_restart() {
    let scratch = ScratchDir::new("macos-quarantine");
    let root = scratch.join("state");
    let host = Arc::new(Fake::default());
    let runtime = new_runtime(host.clone());
    let entry = begin(&runtime, &root, 0);
    runtime.workspace(&root, &entry).unwrap();
    assert!(runtime.admit(&root, &entry).unwrap());
    make_paths(&entry);
    boot(&runtime, &host, &root, &entry, 100);
    host.delete_failure.store(true, Ordering::Release);
    runtime.finish(&root, &entry);
    let record = entry.owned.lock().unwrap().record.clone();
    assert!(record.quarantined && record.stop_proven);
    assert!(record.slot.is_none());
    assert!(record.paths.quarantine.is_dir());
    assert!(!record.paths.tree.exists());
    assert!(!runtime.workspace_busy(&record.workspace, Uuid::now_v7()));
    drop(entry);
    drop(runtime);
    let runtime = new_runtime(host.clone());
    runtime.recover(&root).unwrap();
    let entry = runtime.entry(record.id).unwrap();
    assert!(entry.owned.lock().unwrap().record.slot.is_none());
    assert!(!runtime.cleanup_pending(record.id));
    assert_eq!(
        runtime
            .begin(&root, record.id, &record.workspace)
            .err()
            .unwrap()
            .kind(),
        io::ErrorKind::AlreadyExists
    );
    let next = begin(&runtime, &root, 1);
    assert!(runtime.admit(&root, &next).unwrap());
    host.delete_failure.store(false, Ordering::Release);
    runtime.retry_cleanups(&Config::new(&root, "macos"));
    assert!(runtime.entry(record.id).is_none());
    assert_gone(&entry);
}
#[test]
fn nonroot_cleanup_repairs_guest_modes_without_following_links_across_restart() {
    use std::os::unix::fs::{symlink, PermissionsExt};
    assert_ne!(
        unsafe { libc::geteuid() },
        0,
        "permission regression must run as the real non-root host account"
    );
    let scratch = ScratchDir::new("macos-permission-cleanup");
    let root = scratch.join("state");
    let sentinel = scratch.join("sentinel");
    std::fs::create_dir(&sentinel).unwrap();
    std::fs::write(sentinel.join("whole"), "safe").unwrap();
    std::fs::set_permissions(&sentinel, std::fs::Permissions::from_mode(0o500)).unwrap();
    let host = Arc::new(Fake::default());
    let runtime = new_runtime(host.clone());
    let entry = begin(&runtime, &root, 0);
    assert!(runtime.admit(&root, &entry).unwrap());
    make_paths(&entry);
    boot(&runtime, &host, &root, &entry, 100);
    let paths = entry.owned.lock().unwrap().record.paths.clone();
    let empty = paths.tree.join("empty");
    let locked = paths.tree.join("locked");
    std::fs::create_dir(&empty).unwrap();
    std::fs::create_dir_all(locked.join("nonempty")).unwrap();
    std::fs::write(locked.join("nonempty/file"), "owned").unwrap();
    symlink(&sentinel, paths.tree.join("outside-link")).unwrap();
    std::fs::set_permissions(&empty, std::fs::Permissions::from_mode(0o000)).unwrap();
    std::fs::set_permissions(&locked, std::fs::Permissions::from_mode(0o500)).unwrap();
    assert_eq!(
        std::fs::read_dir(&empty).unwrap_err().kind(),
        io::ErrorKind::PermissionDenied
    );
    assert_eq!(
        std::fs::remove_dir(locked.join("nonempty"))
            .unwrap_err()
            .kind(),
        io::ErrorKind::PermissionDenied
    );
    drop(entry);
    drop(runtime);
    let runtime = new_runtime(host);
    runtime.recover(&root).unwrap();
    assert!(!runtime.degraded());
    assert!(!paths.tree.exists());
    assert_eq!(
        std::fs::read_to_string(sentinel.join("whole")).unwrap(),
        "safe"
    );
    assert_eq!(
        std::fs::metadata(&sentinel).unwrap().permissions().mode() & 0o777,
        0o500
    );
    std::fs::set_permissions(&sentinel, std::fs::Permissions::from_mode(0o700)).unwrap();
}
#[test]
fn deep_guest_tree_is_bounded_and_quarantined_without_slot_loss_after_restart() {
    assert_ne!(unsafe { libc::geteuid() }, 0);
    let scratch = ScratchDir::new("macos-deep-cleanup");
    let root = scratch.join("state");
    let host = Arc::new(Fake::default());
    let runtime = new_runtime(host.clone());
    let entry = begin(&runtime, &root, 0);
    assert!(runtime.admit(&root, &entry).unwrap());
    make_paths(&entry);
    let record = entry.owned.lock().unwrap().record.clone();
    let mut dir = Dir::open(&record.paths.tree, false).unwrap();
    for _ in 0..(super::super::fs::MAX_TREE_DEPTH + 64) {
        dir = dir.child("d".as_ref(), true).unwrap();
    }
    drop(dir);
    drop(entry);
    drop(runtime);
    let runtime = new_runtime(host.clone());
    runtime.recover(&root).unwrap();
    // Assert the traversal's work bound, not recovery speed on a busy disk.
    // Quarantine alone could also result from an unrelated cleanup failure.
    assert_eq!(
        *host.delete_errors.lock().unwrap(),
        vec![(
            record.paths.tree.clone(),
            "guest tree depth limit exceeded".into()
        )]
    );
    let record = runtime
        .entry(record.id)
        .unwrap()
        .owned
        .lock()
        .unwrap()
        .record
        .clone();
    assert!(record.quarantined);
    assert!(record.slot.is_none());
    assert!(record.paths.quarantine.is_dir());
    let next = begin(&runtime, &root, 1);
    assert!(runtime.admit(&root, &next).unwrap());
    drop(next);
    drop(runtime);
    let runtime = new_runtime(host);
    runtime.recover(&root).unwrap();
    assert!(runtime
        .entry(record.id)
        .unwrap()
        .owned
        .lock()
        .unwrap()
        .record
        .slot
        .is_none());
}
#[test]
fn opened_gate_without_service_identity_still_requires_disk_holder_scan() {
    let scratch = ScratchDir::new("macos-no-service-proof");
    let root = scratch.join("state");
    let host = Arc::new(Fake::default());
    let runtime = new_runtime(host.clone());
    let entry = begin(&runtime, &root, 0);
    assert!(runtime.admit(&root, &entry).unwrap());
    make_paths(&entry);
    {
        let mut owned = entry.owned.lock().unwrap();
        owned.record.gate_open = true;
        owned.record.stop_proven = true;
        owned.record.proof_version = 0;
        Runtime::save(&root, &owned.record).unwrap();
    }
    host.disk_held.store(true, Ordering::Release);
    drop(entry);
    drop(runtime);
    let runtime = new_runtime(host.clone());
    runtime.recover(&root).unwrap();
    assert!(host.scans.load(Ordering::Acquire) > 0);
    assert!(runtime.degraded());
    let entry = runtime
        .jobs
        .lock()
        .unwrap()
        .values()
        .next()
        .unwrap()
        .clone();
    assert!(entry.owned.lock().unwrap().record.slot.is_some());
    host.disk_held.store(false, Ordering::Release);
    runtime.retry_cleanups(&Config::new(&root, "macos"));
    assert!(runtime.jobs.lock().unwrap().is_empty());
}
#[test]
fn helper_orphan_is_stopped_by_disk_association_without_killing_foreign_service() {
    let scratch = ScratchDir::new("macos-orphan-stop");
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
    host.processes.lock().unwrap().insert(700, identity(700));
    host.services.lock().unwrap().push(Service {
        identity: identity(700),
        owned: true,
    });
    *host.orphan.lock().unwrap() = Some(identity(700));
    host.processes.lock().unwrap().insert(800, identity(800));
    host.services.lock().unwrap().push(Service {
        identity: identity(800),
        owned: false,
    });
    runtime.finish(&root, &entry);
    assert!(!runtime.degraded());
    assert_eq!(*host.killed.lock().unwrap(), vec![identity(700)]);
    assert!(host.alive(identity(800)).unwrap());
}
#[test]
fn pid_replacement_between_observations_is_not_signalled() {
    let mut checks = 0;
    let signalled = AtomicBool::new(false);
    signal_if_current(
        identity(42),
        || {
            checks += 1;
            Ok(checks == 1)
        },
        || {
            signalled.store(true, Ordering::Release);
            Ok(())
        },
    )
    .unwrap();
    assert_eq!(checks, 2);
    assert!(!signalled.load(Ordering::Acquire));
}
#[test]
fn recovery_skips_temporary_and_stray_record_files() {
    let scratch = ScratchDir::new("macos-record-strays");
    let root = scratch.join("state");
    let host = Arc::new(Fake::default());
    let runtime = new_runtime(host.clone());
    let entry = begin(&runtime, &root, 0);
    let id = entry.owned.lock().unwrap().record.id;
    for name in ["record.tmp", "README", "not-a-uuid.json", ".hidden.json"] {
        std::fs::write(root.join("macvm-records").join(name), "not a record").unwrap();
    }
    let copy = entry.owned.lock().unwrap().record.clone();
    std::fs::write(
        root.join("macvm-records")
            .join(format!("{}.json", Uuid::now_v7())),
        serde_json::to_vec(&copy).unwrap(),
    )
    .unwrap();
    std::fs::write(
        root.join("macvm-records")
            .join(format!("{}.json", Uuid::now_v7())),
        "malformed stray",
    )
    .unwrap();
    drop(entry);
    drop(runtime);
    let runtime = new_runtime(host);
    runtime.recover(&root).unwrap();
    assert!(runtime.entry(id).is_none());
    assert!(root.join("macvm-records/record.tmp").exists());
}

#[test]
fn helper_gets_gate_eof_grace_before_sigkill_escalation() {
    use std::{
        io::Read,
        os::{
            fd::AsRawFd,
            unix::{fs::PermissionsExt, process::ExitStatusExt},
        },
    };
    for ignore_eof in [false, true] {
        let scratch = ScratchDir::new("macos-helper-grace");
        let root = scratch.join("state");
        let host = Arc::new(Fake::default());
        let mut value = Runtime {
            host,
            jobs: Mutex::default(),
            cancelled: Mutex::default(),
            changes: Mutex::default(),
            returns: Mutex::default(),
            workspace_reads: Mutex::default(),
            runner_lock: Mutex::default(),
            grace: Duration::from_millis(100),
            stop_timeout: Duration::from_secs(1),
        };
        let runtime = &mut value;
        let entry = begin(runtime, &root, 0);
        assert!(runtime.admit(&root, &entry).unwrap());
        make_paths(&entry);
        let script = scratch.join("helper");
        let marker = scratch.join("graceful");
        std::fs::write(
            &script,
            if ignore_eof {
                "#!/bin/sh\nprintf 'ready\\n'\nwhile :; do :; done\n".to_string()
            } else {
                format!(
                    "#!/bin/sh\nprintf 'ready\\n'\nIFS= read -r gate\nprintf stopped > '{}'\nexit 0\n",
                    marker.display()
                )
            },
        )
        .unwrap();
        std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o700)).unwrap();
        let share = |tag: &str, guest: &str| motor_macvm::ShareLocation {
            host: scratch.join(tag),
            tag: tag.into(),
            guest: guest.into(),
        };
        let paths = entry.owned.lock().unwrap().record.paths.clone();
        let plan = motor_macvm::JobVmConfig::new(motor_macvm::JobVmRequest {
            clone: paths.clone,
            nonce_disk: paths.nonce,
            workspace: share("workspace", "/Users/test/repo"),
            siblings: vec![],
            cargo_home: share("cargo-home", motor_macvm::CARGO_HOME),
        })
        .unwrap();
        {
            let mut owned = entry.owned.lock().unwrap();
            let mut helper =
                motor_macvm::spawn_job_helper(&script, &plan, owned.slot.as_ref().unwrap())
                    .unwrap();
            // Measure gate shutdown, not the host's time to launch the shell.
            // Bound readiness as well so a broken fixture cannot hang the suite.
            let mut poll = libc::pollfd {
                fd: helper.reports.as_raw_fd(),
                events: libc::POLLIN,
                revents: 0,
            };
            let mut ready = [0; 6];
            let readable = unsafe { libc::poll(&mut poll, 1, 10_000) };
            if readable != 1
                || helper.reports.read_exact(&mut ready).is_err()
                || &ready != b"ready\n"
            {
                let _ = helper.child.kill();
                let status = helper.child.wait();
                panic!("helper did not become ready: poll={readable}, status={status:?}");
            }
            owned.helper = Some(helper);
        }
        let now = Instant::now();
        runtime.finish(&root, &entry);
        assert!(!runtime.degraded());
        let status = entry
            .owned
            .lock()
            .unwrap()
            .helper
            .as_mut()
            .unwrap()
            .child
            .try_wait()
            .unwrap()
            .unwrap();
        if ignore_eof {
            assert_eq!(status.signal(), Some(libc::SIGKILL));
            assert!(now.elapsed() >= Duration::from_millis(100));
        } else {
            assert!(status.success(), "EOF-aware helper exited with {status:?}");
            assert!(marker.exists());
        }
    }
}

#[test]
fn controller_and_proven_slot_locks_release_while_child_is_still_before_exec() {
    use std::os::fd::AsRawFd;
    let scratch = ScratchDir::new("macos-forked-locks");
    let root = scratch.join("state");
    let host = Arc::new(Fake::default());
    let runtime = new_runtime(host.clone());
    runtime.recover(&root).unwrap();
    let entry = begin(&runtime, &root, 0);
    runtime.workspace(&root, &entry).unwrap();
    assert!(runtime.admit(&root, &entry).unwrap());
    boot(&runtime, &host, &root, &entry, 100);
    let control = control(Cancellation::default());
    let admission_path = root.join("macvm-locks/admission.lock");
    let prep_path = root.join("prep.lock");
    let canonical_path = root.join("canonical-locks/test.lock");
    let admission = control.lock(&admission_path).unwrap();
    let prep = control.lock(&prep_path).unwrap();
    let canonical = control.lock(&canonical_path).unwrap();
    let (slot_path, workspace_path) = {
        let owned = entry.owned.lock().unwrap();
        let slot = owned.record.slot.unwrap();
        for fd in [
            owned.slot.as_ref().unwrap().as_raw_fd(),
            owned.workspace_lock.as_ref().unwrap().as_raw_fd(),
            runtime
                .runner_lock
                .lock()
                .unwrap()
                .as_ref()
                .unwrap()
                .as_raw_fd(),
            admission.as_raw_fd(),
            prep.as_raw_fd(),
            canonical.as_raw_fd(),
        ] {
            let flags = unsafe { libc::fcntl(fd, libc::F_GETFD) };
            assert!(
                flags >= 0 && flags & libc::FD_CLOEXEC != 0,
                "controller descriptor is not close-on-exec"
            );
        }
        (
            root.join("macvm-locks").join(format!("slot-{slot}.lock")),
            root.join("macvm-locks")
                .join(format!("workspace-{}.lock", key(&owned.record.workspace))),
        )
    };
    // The child deliberately retains every descriptor without exec: CLOEXEC
    // cannot rescue either controller ownership or the eventual slot release.
    let forked = crate::test_helpers::fork_before_exec().unwrap();
    host.stop_failure.store(true, Ordering::Release);
    runtime.finish(&root, &entry);
    let probe = lock_file(&slot_path).unwrap();
    assert!(fs2::FileExt::try_lock_exclusive(&probe).is_err());
    drop(probe);
    host.stop_failure.store(false, Ordering::Release);
    runtime.finish(&root, &entry);
    assert!(!runtime.degraded());
    drop(admission);
    drop(prep);
    drop(canonical);
    drop(entry);
    drop(runtime);
    for path in [
        root.join("macvm-locks/runner.lock"),
        admission_path,
        workspace_path,
        prep_path,
        canonical_path,
        slot_path,
    ] {
        let probe = lock_file(&path).unwrap();
        fs2::FileExt::try_lock_exclusive(&probe).unwrap_or_else(|error| {
            panic!(
                "{} remained held through a forked copy: {error}",
                path.display()
            )
        });
        fs2::FileExt::unlock(&probe).unwrap();
    }
    let restarted = new_runtime(host);
    restarted.recover(&root).unwrap();
    drop(restarted);
    drop(forked);
}

/// macOS lists system accounts such as nobody with negative uids; the first
/// live macOS job failed to start because one such row didn't parse.
#[test]
fn vm_service_rows_accept_negative_system_uids() {
    assert_eq!(
        super::vm_service_row("45958    -2 /usr/libexec/dhcp6d").unwrap(),
        None
    );
    assert_eq!(
        super::vm_service_row(
            "  812   501 /System/Library/Frameworks/Virtualization.framework/Versions/A/XPCServices/com.apple.Virtualization.VirtualMachine"
        )
        .unwrap(),
        Some((812, 501))
    );
    assert!(super::vm_service_row("812 x /bin/sh").is_err());
}
