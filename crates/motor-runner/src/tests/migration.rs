use super::*;

struct LegacyFixture {
    checkout: PathBuf,
    unrecorded: PathBuf,
    sibling: PathBuf,
    tip: String,
}

async fn seed_legacy(f: &Fixture, config: &Config) -> LegacyFixture {
    fs::write(f.sibling.join("keep"), "sibling output").unwrap();
    commit(&f.sibling, "tracked sibling output\n");
    let runner = Runner::open(config.clone()).unwrap();
    warm(f, &runner).await;
    let tip = git(&f.main, &["rev-parse", "HEAD"]);
    let checkout = Path::new(&f.key).parent().unwrap().join(format!(
        ".motor-base-{}-{}",
        config.instance,
        workspace::name(&f.root)
    ));
    let unrecorded = checkout.with_file_name(".motor-base-e2e-unrecorded");
    let sibling = PathBuf::from(&f.canonical);
    for path in [&checkout, &unrecorded] {
        fs::create_dir_all(path.join("target")).unwrap();
        fs::write(path.join("target/keep"), "legacy output").unwrap();
    }
    fs::write(Path::new(&f.key).join("target/keep"), "foreground output").unwrap();
    fs::write(sibling.join("keep"), "sibling output").unwrap();
    let generations = config.state_dir().join("generations/published");
    fs::create_dir_all(&generations).unwrap();
    fs::write(generations.join("artifact"), vec![1u8; 1024 * 1024]).unwrap();
    let mut catalog = serde_json::to_value(&runner.core.inner.lock().unwrap().catalog).unwrap();
    catalog["generation_migration_complete"] = false.into();
    catalog
        .as_object_mut()
        .unwrap()
        .remove("legacy_generations_pending");
    catalog["repositories"][&f.root] = serde_json::json!({
        "published": "published", "pending": "interrupted", "allocated_bytes": 1048576,
        "fresh_baseline_bytes": 1048576, "commit": tip, "result": "ok", "finished_ms": runner.now(),
        "desired": {"tip": tip, "workspace": f.key, "siblings": []}
    });
    atomic_json(&config.state_dir().join("catalog.json"), &catalog).unwrap();
    drop(runner);
    LegacyFixture {
        checkout,
        unrecorded,
        sibling,
        tip,
    }
}

fn assert_retained(f: &Fixture, legacy: &LegacyFixture) {
    assert_eq!(
        fs::read(Path::new(&f.key).join("target/keep")).unwrap(),
        b"foreground output"
    );
    assert_eq!(
        fs::read(legacy.sibling.join("keep")).unwrap(),
        b"sibling output"
    );
    assert_eq!(
        fs::read(legacy.unrecorded.join("target/keep")).unwrap(),
        b"legacy output"
    );
}

async fn trash_empty(runner: &Runner) {
    until_with_diagnostics(
        || fs::read_dir(runner.trash()).unwrap().next().is_none(),
        || {
            format!(
                "waiting for migration trash purge at {}; {}",
                runner.trash().display(),
                runner_diagnostics(runner)
            )
        },
    )
    .await;
}

#[tokio::test]
async fn legacy_catalog_checkout_is_retired_and_generation_trash_is_resumed() {
    let f = Fixture::new();
    let mut config = f.config();
    config.disable_warm_builds = true;
    let legacy = seed_legacy(&f, &config).await;
    let interrupted = config
        .state_dir()
        .join("trash/interrupted-generations/nested");
    fs::create_dir_all(&interrupted).unwrap();
    fs::write(interrupted.join("artifact"), "remaining deletion").unwrap();
    let runner = reopen(config);
    assert_retained(&f, &legacy);
    assert!(!legacy.checkout.exists());
    assert!(!runner.config().state_dir().join("generations").exists());
    trash_empty(&runner).await;
    let catalog: serde_json::Value =
        read_json(&runner.config().state_dir().join("catalog.json")).unwrap();
    for field in [
        "published",
        "pending",
        "allocated_bytes",
        "fresh_baseline_bytes",
    ] {
        assert!(
            catalog["repositories"][&f.root].get(field).is_none(),
            "{field} survived migration"
        );
    }
    assert!(runner
        .core
        .inner
        .lock()
        .unwrap()
        .catalog
        .legacy_checkouts
        .is_empty());

    let config = runner.config().clone();
    drop(runner);
    let runner = reopen(config);
    assert!(!legacy.checkout.exists());
    assert!(!runner.config().state_dir().join("generations").exists());
    assert_retained(&f, &legacy);
}

#[tokio::test]
async fn unavailable_writer_still_retires_legacy_storage_and_serves_jobs() {
    let f = Fixture::new();
    let mut config = f.config();
    config.disable_warm_builds = true;
    let legacy = seed_legacy(&f, &config).await;
    config.writer_unavailable = true;
    let runner = reopen(config);
    assert!(!runner.config().writer_enabled());
    assert!(!legacy.checkout.exists());
    assert!(!runner.config().state_dir().join("generations").exists());
    assert!(runner
        .core
        .inner
        .lock()
        .unwrap()
        .catalog
        .legacy_checkouts
        .is_empty());
    assert_retained(&f, &legacy);
    let (run, bytes) = f.run("test -f target/keep");
    f.submit(&runner, &run, &bytes);
    assert_eq!(
        terminal(&runner, run.job_id).await.outcome,
        Outcome::Exit { code: 0 }
    );
    assert!(!runner
        .core
        .inner
        .lock()
        .unwrap()
        .jobs
        .values()
        .any(|job| job.base));
    trash_empty(&runner).await;
}

#[tokio::test]
async fn legacy_live_unit_is_stopped_and_closed_before_foreground_recovery_and_storage_removal() {
    use std::os::unix::process::ExitStatusExt;
    let f = Fixture::new();
    let mut config = f.config();
    config.disable_warm_builds = true;
    let legacy = seed_legacy(&f, &config).await;
    let (mut request, _) = f.run("true");
    request.job_id = Uuid::now_v7();
    request.cwd = text(&legacy.checkout).into();
    request.workspace_key = format!("base:{}", f.root);
    let mut base = Job::new(
        request.job_id,
        Some(request),
        f.clock.load(Ordering::SeqCst),
        99,
        4,
    );
    base.base = true;
    base.state = JobState::Running;
    base.started_ms = Some(f.clock.load(Ordering::SeqCst));
    base.process = Some(platform::ProcessIdentity {
        pid: 0,
        start: 0,
        unit: Some(base.id),
    });
    let base_record = config
        .state_dir()
        .join("jobs")
        .join(base.id.to_string())
        .join("record.json");
    fs::create_dir_all(base_record.parent().unwrap()).unwrap();
    atomic_json(&base_record, &base).unwrap();
    let (request, _) = f.run("true");
    let mut foreground = Job::new(
        request.job_id,
        Some(request),
        f.clock.load(Ordering::SeqCst),
        100,
        2,
    );
    foreground.state = JobState::Running;
    foreground.started_ms = Some(f.clock.load(Ordering::SeqCst));
    foreground.process = Some(platform::ProcessIdentity {
        pid: 0,
        start: 0,
        unit: Some(foreground.id),
    });
    let foreground_record = config
        .state_dir()
        .join("jobs")
        .join(foreground.id.to_string())
        .join("record.json");
    fs::create_dir_all(foreground_record.parent().unwrap()).unwrap();
    atomic_json(&foreground_record, &foreground).unwrap();
    let stopped = Arc::new(AtomicBool::new(false));
    let release = Arc::new(AtomicBool::new(false));
    config.systemd_command = Some({
        let stopped = stopped.clone();
        let release = release.clone();
        let checkout = legacy.checkout.clone();
        let generations = config.state_dir().join("generations");
        let base_unit = backend::host_unit_name(base.id);
        Arc::new(move |args| {
            let is_base = args.iter().any(|arg| *arg == base_unit);
            let mut stdout = Vec::new();
            if args[0] == "stop" && is_base {
                assert!(checkout.join("target/keep").exists());
                assert!(generations.join("published/artifact").exists());
                stopped.store(true, Ordering::SeqCst);
            } else if args[0] == "show" {
                if is_base {
                    stdout = b"ActiveState=inactive\n".to_vec();
                } else {
                    assert!(stopped.load(Ordering::SeqCst));
                    let record: Job = read_json(&base_record).unwrap();
                    assert!(
                        record.terminal.is_some(),
                        "legacy base must be closed before foreground recovery"
                    );
                    stdout = if release.load(Ordering::SeqCst) {
                        b"ActiveState=inactive\n".to_vec()
                    } else {
                        b"ActiveState=active\n".to_vec()
                    };
                }
            }
            Ok(std::process::Output {
                status: std::process::ExitStatus::from_raw(0),
                stdout,
                stderr: Vec::new(),
            })
        })
    });
    let runner = reopen(config);
    assert!(stopped.load(Ordering::SeqCst));
    assert_eq!(
        runner.core.inner.lock().unwrap().jobs[&foreground.id].state,
        JobState::Running
    );
    assert!(runner.core.inner.lock().unwrap().jobs[&base.id]
        .terminal
        .is_some());
    assert!(!legacy.checkout.exists());
    assert!(!runner.config().state_dir().join("generations").exists());
    assert_retained(&f, &legacy);
    release.store(true, Ordering::SeqCst);
    terminal(&runner, foreground.id).await;
    trash_empty(&runner).await;
}

#[cfg(target_os = "linux")]
#[tokio::test]
#[ignore = "requires the trusted build-server isolation gate; see motor-runner README"]
async fn isolated_legacy_checkout_migration_retires_source_and_first_fill_creates_writer_owned_checkout(
) {
    let f = Fixture::new();
    fs::create_dir_all(f.main.join("src")).unwrap();
    fs::write(
        f.main.join("Cargo.toml"),
        "[package]\nname='lazy-writer'\nversion='0.1.0'\nedition='2021'\n",
    )
    .unwrap();
    fs::write(
        f.main.join("Cargo.lock"),
        "version=4\n[[package]]\nname='lazy-writer'\nversion='0.1.0'\n",
    )
    .unwrap();
    fs::write(f.main.join("src/lib.rs"), "pub fn value() {}\n").unwrap();
    commit(&f.main, "lazy writer fixture\n");
    let mut config = f.config();
    config.disable_warm_builds = true;
    let legacy = seed_legacy(&f, &config).await;
    let (mut request, _) = f.run("true");
    request.cwd = text(&legacy.checkout).into();
    request.workspace_key = format!("base:{}", f.root);
    let mut base = Job::new(
        request.job_id,
        Some(request),
        f.clock.load(Ordering::SeqCst),
        99,
        4,
    );
    base.base = true;
    base.state = JobState::Running;
    base.started_ms = Some(f.clock.load(Ordering::SeqCst));
    let sandbox = crate::isolation::Sandbox::prepare(
        base.id,
        &config.state_dir(),
        &legacy.checkout,
        &legacy.checkout.join("target"),
        &[],
        None,
        &config.build_slot_root,
        4,
    )
    .unwrap();
    let job_uid = as_account("motor-job", "/usr/bin/id", &["-u"]).unwrap();
    assert_eq!(
        as_account(
            "motor-job",
            "/usr/bin/stat",
            &["-c", "%u", text(&legacy.checkout)]
        )
        .unwrap()
        .trim(),
        job_uid.trim()
    );
    as_account("motor-job", "/bin/sh", &[
        "-c", "mkdir -p \"$1/private/sticky\"; printf private > \"$1/private/sticky/keep\"; chmod 700 \"$1/private\"; chmod 1777 \"$1/private/sticky\"; chmod 600 \"$1/private/sticky/keep\"",
        "private-fixture", text(&legacy.checkout.join("target")),
    ]).unwrap();
    assert_ne!(
        unsafe { libc::geteuid() },
        0,
        "run the ACL precondition as a non-root operator with sudo"
    );
    let runner_acl = format!("u:{}:rwx,m::r-x", unsafe { libc::geteuid() });
    let masked = Command::new("/usr/bin/sudo")
        .args(["-n", "/usr/bin/setfacl", "-m", &runner_acl, "--"])
        .arg(&legacy.checkout)
        .output()
        .unwrap();
    assert!(
        masked.status.success(),
        "{}",
        String::from_utf8_lossy(&masked.stderr)
    );
    let trash = config.state_dir().join("trash");
    fs::create_dir_all(&trash).unwrap();
    let error = fs::rename(&legacy.checkout, trash.join("unrepaired-rename-probe")).unwrap_err();
    eprintln!(
        "unrepaired legacy rename: errno={:?}: {error}",
        error.raw_os_error()
    );
    assert_eq!(error.raw_os_error(), Some(libc::EACCES));
    use std::os::unix::fs::MetadataExt;
    let legacy_inode = fs::symlink_metadata(&legacy.checkout).unwrap().ino();
    let mut live = sandbox.launch_durable(
        "trap 'if test -f target/keep; then echo storage-present-at-stop; fi; exit 0' TERM; echo live; while :; do sleep 0.1; done",
        &legacy.checkout, &[], &[], base.weight).unwrap();
    base.process = Some(live.identity);
    base.durable_output = live.durable_output;
    let record = config
        .state_dir()
        .join("jobs")
        .join(base.id.to_string())
        .join("record.json");
    fs::create_dir_all(record.parent().unwrap()).unwrap();
    atomic_json(&record, &base).unwrap();
    live.release().unwrap();
    let stdout = config
        .state_dir()
        .join("sandboxes")
        .join(base.id.to_string())
        .join("output/stdout");
    // Durable output is runner control-plane storage, not a host path the job
    // account may traverse outside its service's mount namespace.
    until_with_diagnostics(|| {
        fs::read_to_string(&stdout)
            .is_ok_and(|output| output.contains("live"))
    }, || format!("waiting for pre-migration legacy service to emit live: process={:?} unit_alive={:?} stdout_path={} stdout={:?} state_permissions={:?}",
        base.process, platform::unit_alive(base.process), stdout.display(), fs::read_to_string(&stdout), fs::metadata(config.state_dir()).map(|m| m.permissions())))
    .await;
    assert_eq!(platform::unit_alive(base.process).unwrap(), Some(true));
    config.isolate_jobs = true;
    config.disable_warm_builds = false;
    config.base_commands = vec!["printf lazy-owned > lazy-owned-marker".into()];
    let purge_hold = PurgeHold::new();
    config.long_work = Some(purge_hold.hook());
    let work = crate::test_cleanup::CheckoutWorkCapture::begin(&f.scratch);
    let runner = reopen(config);
    assert_no_recursive_startup_checkout_work(&work);
    assert_eq!(
        work.startup_thread_events()
            .iter()
            .filter(|operation| **operation == "restore_runner_checkout_root")
            .count(),
        1
    );
    drop(work);
    assert!(!legacy.checkout.exists());
    let retired = fs::read_dir(&trash)
        .unwrap()
        .map(|entry| entry.unwrap().path())
        .find(|path| fs::symlink_metadata(path).unwrap().ino() == legacy_inode)
        .expect("legacy directory was renamed into trash");
    // purge_hold blocks the background deleter before any files are removed.
    // While deletion is paused, inspect this same inode under a fixture parent
    // the job account can traverse, then return it to the queued purge path.
    // No socket-parent or descendant permissions are changed by inspection.
    let inspection = f.scratch.join("remote/retired-owner-inspection");
    fs::rename(&retired, &inspection).unwrap();
    let private = inspection.join("target/private/sticky/keep");
    let inspected_inode = fs::symlink_metadata(&inspection).unwrap().ino();
    let owner = as_account("motor-job", "/usr/bin/stat", &["-c", "%u", text(&private)]);
    fs::rename(&inspection, &retired).unwrap();
    let owner = owner.unwrap();
    assert_eq!(
        inspected_inode, legacy_inode,
        "inspection must use the retired checkout itself"
    );
    // The state directory is private to the daemon. Its ACL deliberately lets the
    // job and cache-writer accounts traverse it (the group bits then show the ACL
    // mask, --x), but nobody else may read or write it.
    assert_eq!(
        fs::metadata(runner.config().state_dir()).unwrap().mode() & 0o067,
        0,
        "inspection must not open the socket parent beyond traversal"
    );
    assert_eq!(
        owner.trim(),
        job_uid.trim(),
        "startup changed a descendant's ownership"
    );
    assert_eq!(platform::unit_alive(base.process).unwrap(), Some(false));
    assert!(runner.core.inner.lock().unwrap().jobs[&base.id]
        .terminal
        .is_some());
    assert!(fs::read_to_string(&stdout)
        .unwrap()
        .contains("storage-present-at-stop"));
    live.wait().unwrap();
    live.reap();
    let account = runner
        .config()
        .writer_account
        .expect("validated writer account");
    assert_retained(&f, &legacy);
    drop(purge_hold);
    trash_empty(&runner).await;
    assert!(
        !retired.exists(),
        "background purge left the retired directory"
    );
    let requested_tip = commit(&f.main, "fresh integration tip\n").0;
    warm(&f, &runner).await;
    assert_eq!(
        runner.status().repositories[0].last_rebuild_result,
        Some(RebuildResult::Ok),
        "first post-migration fill failed; {}",
        runner_diagnostics(&runner)
    );
    drop(live);
    drop(sandbox);
    let core = Arc::downgrade(&runner.core);
    runner.wait_for_background_deletes(Duration::from_secs(30));
    let trash = runner.trash();
    drop(runner);
    until_with_diagnostics(
        || core.strong_count() == 0,
        || {
            format!(
                "waiting for migrated runner shutdown: strong_count={} trash={:?}",
                core.strong_count(),
                fs::read_dir(&trash)
                    .map(|entries| entries.flatten().map(|e| e.path()).collect::<Vec<_>>())
            )
        },
    )
    .await;
    assert!(fs::read_dir(trash).unwrap().next().is_none());
    let uid = account.uid.to_string();
    for path in [
        &legacy.checkout,
        &legacy.checkout.join(".git"),
        &legacy.checkout.join("lazy-owned-marker"),
    ] {
        let owner = as_account(
            crate::isolation::WRITER_USER,
            "/usr/bin/stat",
            &["-c", "%u", text(path)],
        )
        .unwrap();
        assert_eq!(owner.trim(), uid);
    }
    let tip = as_account(
        crate::isolation::WRITER_USER,
        "/usr/bin/git",
        &["-C", text(&legacy.checkout), "rev-parse", "HEAD"],
    )
    .unwrap();
    assert_eq!(tip.trim(), requested_tip);
    assert_eq!(
        as_account(
            crate::isolation::WRITER_USER,
            "/usr/bin/cat",
            &[text(&legacy.checkout.join("lazy-owned-marker"))]
        )
        .unwrap(),
        "lazy-owned"
    );
    assert_eq!(
        as_account(
            "motor-job",
            "/usr/bin/cat",
            &[text(&Path::new(&f.key).join("target/keep"))]
        )
        .unwrap(),
        "foreground output"
    );
    assert_eq!(
        fs::read(legacy.sibling.join("keep")).unwrap(),
        b"sibling output"
    );
    assert_eq!(
        fs::read(legacy.unrecorded.join("target/keep")).unwrap(),
        b"legacy output"
    );
    crate::isolation::restore_runner_access(&f.scratch).unwrap();
}

#[cfg(target_os = "linux")]
fn as_account(user: &str, program: &str, args: &[&str]) -> io::Result<String> {
    let output = Command::new("/usr/bin/sudo")
        .args(["-n", "-u", user, "--", program])
        .args(args)
        .output()?;
    if !output.status.success() {
        return Err(io::Error::other(format!(
            "{user} {program} {args:?}: {}",
            String::from_utf8_lossy(&output.stderr)
        )));
    }
    Ok(String::from_utf8_lossy(&output.stdout).into_owned())
}

#[tokio::test]
async fn missing_legacy_tip_does_not_block_startup_or_foreground_job() {
    let f = Fixture::new();
    let mut config = f.config();
    config.disable_warm_builds = true;
    let legacy = seed_legacy(&f, &config).await;
    let path = config.state_dir().join("catalog.json");
    let mut catalog: serde_json::Value = read_json(&path).unwrap();
    let missing = "0".repeat(40);
    catalog["repositories"][&f.root]["commit"] = missing.clone().into();
    catalog["repositories"][&f.root]["desired"]["tip"] = missing.into();
    atomic_json(&path, &catalog).unwrap();
    let runner = reopen(config);
    assert!(!legacy.checkout.exists());
    let (run, bytes) = f.run("test -f target/keep && printf served");
    f.submit(&runner, &run, &bytes);
    assert_eq!(
        terminal(&runner, run.job_id).await.outcome,
        Outcome::Exit { code: 0 }
    );
    assert_retained(&f, &legacy);
    trash_empty(&runner).await;
}

#[tokio::test]
async fn many_legacy_checkouts_retire_without_sync_or_recursive_ownership_during_open() {
    let f = Fixture::new();
    let mut config = f.config();
    config.disable_warm_builds = true;
    let legacy = seed_legacy(&f, &config).await;
    let path = config.state_dir().join("catalog.json");
    let mut catalog: Catalog = read_json(&path).unwrap();
    let mut checkouts = vec![legacy.checkout.clone()];
    for index in 0..32 {
        let checkout = legacy
            .checkout
            .with_file_name(format!(".motor-base-e2e-recorded-{index}"));
        fs::create_dir_all(checkout.join("target/private/deep")).unwrap();
        fs::write(
            checkout.join("target/private/deep/keep"),
            "old build output",
        )
        .unwrap();
        catalog.legacy_checkouts.insert(
            text(&checkout).into(),
            LegacyCheckout {
                repository: f.root.clone(),
                tip: Some(legacy.tip.clone()),
            },
        );
        checkouts.push(checkout);
    }
    atomic_json(&path, &catalog).unwrap();
    let work = crate::test_cleanup::CheckoutWorkCapture::begin(&f.scratch);
    let runner = reopen(config);
    assert_no_recursive_startup_checkout_work(&work);
    drop(work);
    assert!(checkouts.iter().all(|path| !path.exists()));
    assert!(runner
        .core
        .inner
        .lock()
        .unwrap()
        .catalog
        .legacy_checkouts
        .is_empty());
    assert_retained(&f, &legacy);
    trash_empty(&runner).await;
}

#[tokio::test]
async fn retirement_errors_keep_records_and_retry_without_blocking_jobs() {
    let f = Fixture::new();
    let mut config = f.config();
    config.disable_warm_builds = true;
    let legacy = seed_legacy(&f, &config).await;
    let trash = config.state_dir().join("trash");
    fs::write(&trash, "blocks trash directory creation").unwrap();
    let runner = reopen(config.clone());
    assert!(legacy.checkout.exists());
    assert!(runner
        .core
        .inner
        .lock()
        .unwrap()
        .catalog
        .legacy_checkouts
        .contains_key(text(&legacy.checkout)));
    assert!(
        runner
            .core
            .inner
            .lock()
            .unwrap()
            .catalog
            .legacy_generations_pending
    );
    assert!(config.state_dir().join("generations").exists());
    let (run, bytes) = f.run("true");
    f.submit(&runner, &run, &bytes);
    assert_eq!(
        terminal(&runner, run.job_id).await.outcome,
        Outcome::Exit { code: 0 }
    );
    assert_retained(&f, &legacy);
    drop(runner);
    fs::remove_file(trash).unwrap();
    let runner = reopen(config);
    assert!(!legacy.checkout.exists());
    assert!(
        !runner
            .core
            .inner
            .lock()
            .unwrap()
            .catalog
            .legacy_generations_pending
    );
    assert!(!runner.config().state_dir().join("generations").exists());
    assert_retained(&f, &legacy);
    trash_empty(&runner).await;
}

#[tokio::test]
async fn first_fill_materializes_its_checkout_only_after_migration() {
    let f = Fixture::new();
    let mut config = f.config();
    config.disable_warm_builds = true;
    let legacy = seed_legacy(&f, &config).await;
    config.disable_warm_builds = false;
    config.base_commands = vec!["printf lazy-owned > lazy-owned-marker".into()];
    let runner = reopen(config);
    assert!(!legacy.checkout.exists());
    let requested_tip = commit(&f.main, "fresh integration tip\n").0;
    warm(&f, &runner).await;
    assert_eq!(git(&legacy.checkout, &["rev-parse", "HEAD"]), requested_tip);
    assert_eq!(
        fs::read(legacy.checkout.join("lazy-owned-marker")).unwrap(),
        b"lazy-owned"
    );
    assert_eq!(
        runner.status().repositories[0].last_rebuild_result,
        Some(RebuildResult::Ok)
    );
    assert_retained(&f, &legacy);
}

fn assert_no_recursive_startup_checkout_work(work: &crate::test_cleanup::CheckoutWorkCapture) {
    assert!(
        work.startup_thread_events()
            .iter()
            .all(|operation| *operation == "restore_runner_checkout_root"),
        "startup performed recursive checkout work: {:?}",
        work.startup_thread_events()
    );
    // Rename can need ownership repair on the top-level directory. Repairing
    // permissions throughout the old tree belongs to background deletion.
    // Only a newly requested dependency-cache build may recreate source or
    // transfer its ownership to the writer account.
    assert!(
        !work
            .events()
            .iter()
            .any(|operation| matches!(*operation, "sync_workspace" | "prepare_writer_tree")),
        "startup recreated a checkout: {:?}",
        work.events()
    );
}

#[cfg(target_os = "linux")]
struct PurgeHold(Arc<(Mutex<bool>, std::sync::Condvar)>);
#[cfg(target_os = "linux")]
impl PurgeHold {
    fn new() -> Self {
        Self(Arc::new((Mutex::new(false), std::sync::Condvar::new())))
    }
    fn hook(&self) -> Arc<dyn Fn(&'static str) + Send + Sync> {
        let gate = self.0.clone();
        Arc::new(move |operation| {
            if operation == "remove" {
                let (lock, wake) = &*gate;
                let mut released = lock.lock().unwrap();
                while !*released {
                    released = wake.wait(released).unwrap();
                }
            }
        })
    }
}
#[cfg(target_os = "linux")]
impl Drop for PurgeHold {
    fn drop(&mut self) {
        let (lock, wake) = &*self.0;
        *lock.lock().unwrap() = true;
        wake.notify_all();
    }
}

#[cfg(target_os = "linux")]
#[tokio::test]
async fn background_delete_waiter_blocks_until_purge_finishes_and_reports_pending_count() {
    let f = Fixture::new();
    let runner = Runner::open(f.config()).unwrap();
    let path = f.scratch.join("retired-tree");
    fs::create_dir_all(&path).unwrap();
    let hold = PurgeHold::new();
    // Use a fresh instance so every purge runs through the held deletion hook.
    let mut config = runner.config().clone();
    drop(runner);
    config.long_work = Some(hold.hook());
    let runner = reopen(config);
    runner.discard(&path).unwrap();
    assert_eq!(runner.core.background_deletes.load(Ordering::SeqCst), 1);
    let timeout = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        runner.wait_for_background_deletes(Duration::from_millis(20));
    }))
    .unwrap_err();
    let message = timeout.downcast_ref::<String>().unwrap();
    assert!(message.contains("1 background deletions still in flight"));
    drop(hold);
    runner.wait_for_background_deletes(Duration::from_secs(10));
    assert!(fs::read_dir(runner.trash()).unwrap().next().is_none());
    let core = Arc::downgrade(&runner.core);
    drop(runner);
    until(|| core.strong_count() == 0).await;
}
