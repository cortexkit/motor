use super::*;
use std::os::unix::fs::{MetadataExt, PermissionsExt};

async fn deployment_canary(f: &Fixture, runner: &Runner) -> std::process::Output {
    let (socket, server) = listener(f, runner);
    let state = f.scratch.join("canary-client/e2e");
    fs::create_dir_all(&state).unwrap();
    std::os::unix::fs::symlink(&socket.0, state.join("runner.sock")).unwrap();
    let result = tokio::process::Command::new("python3")
        .arg(Path::new(env!("CARGO_MANIFEST_DIR")).join("../../script/runner/deploy_canary.py"))
        // Relative socket names fit sun_path even with a long fixture scratch root.
        .args(["--state-root", "canary-client", "--instance", "e2e"])
        .arg("--workspace-root")
        .arg(f.scratch.join("remote/worktrees"))
        .args(["--timeout-s", "20"])
        .current_dir(f.scratch.join("."))
        .output()
        .await
        .unwrap();
    server.abort();
    result
}

#[tokio::test]
async fn deployment_canary_reaches_a_real_runner_job_start() {
    let f = Fixture::new();
    let runner = Runner::open(f.config()).unwrap();
    let result = deployment_canary(&f, &runner).await;
    let jobs = runner
        .core
        .inner
        .lock()
        .unwrap()
        .jobs
        .values()
        .filter(|job| !job.base)
        .cloned()
        .collect::<Vec<_>>();
    assert_eq!(jobs.len(), 1, "{}", String::from_utf8_lossy(&result.stderr));
    let job = &jobs[0];
    assert!(
        job.started_ms.is_some(),
        "{}",
        String::from_utf8_lossy(&result.stderr)
    );
    let outcome = &job.terminal.as_ref().unwrap().outcome;
    assert!(matches!(outcome, Outcome::Exit { code: 0 | 1 }));
    // A portable fixture runs as the test account; only an isolated fixture is
    // guaranteed to satisfy the canary's motor-job identity assertion.
    assert_eq!(
        result.status.success(),
        *outcome == Outcome::Exit { code: 0 }
    );
}

#[tokio::test]
#[ignore = "requires the trusted build-server isolation gate; see motor-runner README"]
async fn isolated_symlinked_state_root_runs_job_through_permissions_helper() {
    let f = Fixture::new();
    let mut config = f.config();
    fs::create_dir_all(&config.state_root).unwrap();
    let alias = f.scratch.join("operator-state-link");
    std::os::unix::fs::symlink(&config.state_root, &alias).unwrap();
    config.state_root = alias;
    config.isolate_jobs = true;
    let runner = Runner::open(config).unwrap();
    let result = deployment_canary(&f, &runner).await;
    assert!(
        result.status.success(),
        "{}",
        String::from_utf8_lossy(&result.stderr)
    );
    crate::isolation::restore_runner_access(&f.scratch).unwrap();
}

#[test]
#[ignore = "requires the trusted build-server isolation gate; see motor-runner README"]
fn isolated_startup_reclaims_job_owned_trash_entries() {
    let f = Fixture::new();
    let mut config = f.config();
    config.isolate_jobs = true;
    let state = config.state_dir();
    let stale = state.join("trash/previous-job/nested");
    fs::create_dir_all(&stale).unwrap();
    let file = stale.join("private-output");
    fs::write(&file, "created by the previous job\n").unwrap();
    fs::set_permissions(&stale, fs::Permissions::from_mode(0o700)).unwrap();
    fs::set_permissions(&file, fs::Permissions::from_mode(0o600)).unwrap();
    let chown = Command::new("/usr/bin/sudo")
        .args(["-n", "/usr/bin/chown", "-R", "motor-job:motor-job", "--"])
        .arg(stale.parent().unwrap())
        .output()
        .unwrap();
    assert!(
        chown.status.success(),
        "could not create job-owned trash fixture: {}",
        String::from_utf8_lossy(&chown.stderr)
    );

    let _runner = Runner::open(config).unwrap();
    let trash = state.join("trash");
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        let pending = fs::read_dir(&trash).unwrap().next().is_some();
        if !pending {
            break;
        }
        assert!(
            Instant::now() < deadline,
            "startup left job-owned trash behind"
        );
        std::thread::sleep(Duration::from_millis(10));
    }
}

/// Prepare retains private Cargo outputs owned by motor-job in an existing
/// workspace, while a new workspace gets no artifacts copied from another one.
#[tokio::test]
#[ignore = "requires the trusted build-server isolation gate; see motor-runner README"]
async fn isolated_workspace_prepare_preserves_job_owned_target_and_starts_new_target_empty() {
    let f = Fixture::new();
    let mut config = f.config();
    config.isolate_jobs = true;
    let runner = Runner::open(config).unwrap();
    let (run, bytes) = f.run("mkdir -p target/debug/incremental; printf retained > target/debug/incremental/s.lock; chmod 600 target/debug/incremental/s.lock");
    f.submit(&runner, &run, &bytes);
    assert_eq!(
        terminal(&runner, run.job_id).await.outcome,
        Outcome::Exit { code: 0 }
    );
    base_done(&runner).await;
    let mut prepare = prepare_for(&run);
    runner.prepare(&prepare, true).unwrap();
    let retained = Path::new(&run.workspace_key).join("target/debug/incremental/s.lock");
    let contents = Command::new("/usr/bin/sudo")
        .args(["-n", "-u", "motor-job", "--", "/usr/bin/cat"])
        .arg(&retained)
        .output()
        .unwrap();
    assert!(contents.status.success());
    assert_eq!(contents.stdout, b"retained");
    prepare.workspace_key = text(&f.scratch.join("remote/worktrees/new-empty")).into();
    runner.prepare(&prepare, true).unwrap();
    assert_eq!(
        fs::read_dir(Path::new(&prepare.workspace_key).join("target"))
            .unwrap()
            .count(),
        0
    );
    assert!(!runner.config().state_dir().join("generations").exists());
    crate::isolation::restore_runner_access(&f.scratch).unwrap();
}

/// A sibling checkout can also be another repository's own workspace, which
/// that repository's jobs leave owned by the job account. Git then refuses the
/// runner's sibling sync with "detected dubious ownership", so preparing a job
/// that names the path as a sibling must take it back first. Only in-place
/// siblings are checked out at that path; snapshot mode never touches it.
#[tokio::test]
#[ignore = "requires the trusted build-server isolation gate; see motor-runner README"]
async fn isolated_sibling_left_job_owned_by_its_own_workspace_job_still_populates() {
    let f = Fixture::new();
    let mut config = f.config();
    config.isolate_jobs = true;
    config.sibling_snapshot_mode_override = Some(false);
    let runner = Runner::open(config).unwrap();
    let (first, bytes) = f.run("cat ../commons/source");
    f.submit(&runner, &first, &bytes);
    assert_eq!(
        terminal(&runner, first.job_id).await.outcome,
        Outcome::Exit { code: 0 }
    );
    base_done(&runner).await;
    // Leave the sibling as a job running in it as its workspace does: the
    // checkout root and a private file owned by the job account, .git not.
    let sibling = Path::new(&f.canonical);
    fs::write(sibling.join("private-output"), "written by its own job\n").unwrap();
    fs::set_permissions(
        sibling.join("private-output"),
        fs::Permissions::from_mode(0o600),
    )
    .unwrap();
    for path in [sibling.to_path_buf(), sibling.join("private-output")] {
        let chown = Command::new("/usr/bin/sudo")
            .args(["-n", "/usr/bin/chown", "motor-job:motor-job", "--"])
            .arg(&path)
            .output()
            .unwrap();
        assert!(
            chown.status.success(),
            "{}",
            String::from_utf8_lossy(&chown.stderr)
        );
    }
    assert_ne!(
        fs::metadata(sibling).unwrap().uid(),
        unsafe { libc::geteuid() },
        "the sibling root must start out owned by another user"
    );
    // The next job runs in a workspace that doesn't exist yet, as a first job
    // from a new checkout does; the sibling must still be taken back then.
    let (mut second, bytes) = f.run("cat ../commons/source");
    second.workspace_key = text(&f.scratch.join("remote/worktrees/fresh")).into();
    second.cwd = second.workspace_key.clone();
    assert!(!Path::new(&second.workspace_key).exists());
    f.submit(&runner, &second, &bytes);
    let t = terminal(&runner, second.job_id).await;
    assert_eq!(
        t.outcome,
        Outcome::Exit { code: 0 },
        "a job-owned sibling must populate for a new workspace: {:?}",
        t.refusal_detail
    );
    crate::isolation::restore_runner_access(&f.scratch).unwrap();
}

#[test]
#[ignore = "requires the trusted build-server isolation gate; see motor-runner README"]
fn isolated_workspace_prepare_makes_fresh_target_writable_by_job_account() {
    let f = Fixture::new();
    let mut config = f.config();
    config.isolate_jobs = true;
    config.base_commands = vec!["true".into()];
    let runner = Runner::open(config).unwrap();
    let (run, bytes) = f.run("true");
    let mut prepare = prepare_for(&run);
    prepare.bundles = run.bundles.clone();
    let mut upload = Upload::new(runner.spool_dir(prepare.transfer_id), &prepare.bundles).unwrap();
    for (index, data) in bytes.iter().enumerate() {
        upload
            .chunk(Frame::BundleChunk {
                index: index as u32,
                offset: 0,
                data: BytePayload(data.clone()),
            })
            .unwrap();
    }
    runner.fetch_prepare(&prepare, &upload.directory).unwrap();
    runner.prepare(&prepare, true).unwrap();
    drop(upload);

    let target = Path::new(&prepare.workspace_key).join("target");
    let job_uid = Command::new("/usr/bin/id")
        .args(["-u", "motor-job"])
        .output()
        .unwrap();
    assert!(job_uid.status.success());
    let job_uid = String::from_utf8_lossy(&job_uid.stdout)
        .trim()
        .parse::<u32>()
        .unwrap();
    assert_eq!(fs::metadata(&target).unwrap().uid(), job_uid);
    let output = Command::new("/usr/bin/sudo")
        .args(["-n", "-u", "motor-job", "--", "/usr/bin/touch"])
        .arg(target.join("job-can-write"))
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "motor-job could not write freshly prepared target: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(target.join("job-can-write").is_file());
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        let status = runner.status();
        if status.queue_depth == 0 && status.running_jobs.is_empty() {
            break;
        }
        assert!(Instant::now() < deadline, "warm preparation did not finish");
        std::thread::sleep(Duration::from_millis(10));
    }
    crate::isolation::restore_runner_access(&f.scratch).unwrap();
}

/// Runs one privileged helper the way the runner does, through sudo.
fn sudo(program: &str, args: &[&str], path: &Path) {
    let output = Command::new("/usr/bin/sudo")
        .args(["-n", program])
        .args(args)
        .arg("--")
        .arg(path)
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{program} {args:?} {}: {}",
        path.display(),
        String::from_utf8_lossy(&output.stderr)
    );
}

fn acl(path: &Path) -> String {
    let output = Command::new("/usr/bin/getfacl")
        .args(["-p", "--"])
        .arg(path)
        .output()
        .unwrap();
    assert!(output.status.success());
    String::from_utf8(output.stdout).unwrap()
}

/// A job owns its workspace root while it runs. One that chmods the root to
/// 0700 rewrites the root's ACL mask, so the runner's named entry stays listed
/// but grants nothing, and every later admission on the workspace used to be
/// refused with a bare `Permission denied (os error 13)`. The runner now gives
/// itself read access back when the job ends, and an admission that still
/// finds the root locked repairs it and admits the job.
#[tokio::test]
#[ignore = "requires the trusted build-server isolation gate; see motor-runner README"]
async fn isolated_job_that_locks_its_workspace_root_does_not_wedge_later_admissions() {
    let f = Fixture::new();
    let mut config = f.config();
    config.isolate_jobs = true;
    let runner = Runner::open(config).unwrap();
    let workspace = PathBuf::from(&f.key);
    let (first, bytes) = f.run("mkdir -p locked/inner && chmod 000 locked && chmod 0700 .");
    f.submit(&runner, &first, &bytes);
    let ended = terminal(&runner, first.job_id).await;
    assert_eq!(
        ended.outcome,
        Outcome::Exit { code: 0 },
        "{:?}",
        ended.refusal_detail()
    );
    base_done(&runner).await;
    // The job left the root owned by the job account at mode 0700. The
    // runner's reclaim at job end set the mask
    // to r-x, which stat shows as the group bits, so the runner's entry is
    // effective again and the root is readable; the other bits stay as the
    // job left them.
    let metadata = fs::metadata(&workspace).unwrap();
    assert_ne!(metadata.uid(), unsafe { libc::geteuid() });
    assert_eq!(metadata.mode() & 0o777, 0o750, "{:o}", metadata.mode());
    let after_job = acl(&workspace);
    assert!(after_job.contains("mask::r-x"), "{after_job}");
    fs::read_dir(&workspace).expect("the runner reads the root after the job ends");

    // Zero the mask again the way the job did, after that reclaim: chmod as
    // root rewrites the mask exactly as chmod by the owning job does.
    sudo("/usr/bin/chmod", &["0700"], &workspace);
    let locked = acl(&workspace);
    assert!(locked.contains("mask::---"), "{locked}");
    let error = fs::read_dir(&workspace).expect_err("the root must lock the runner out");
    assert_eq!(error.kind(), io::ErrorKind::PermissionDenied);

    let (second, bytes) = f.run("test -d locked");
    f.submit(&runner, &second, &bytes);
    let ended = terminal(&runner, second.job_id).await;
    assert_eq!(
        ended.outcome,
        Outcome::Exit { code: 0 },
        "{:?}",
        ended.refusal_detail()
    );
    crate::isolation::restore_runner_access(&f.scratch).unwrap();
}

/// The access repair walks the whole tree as root, so it also rescues a
/// directory a job made unreadable below the root (mode 0000, mask zeroed),
/// and it leaves the root's mask at r-x without changing any owner.
#[test]
#[ignore = "requires the trusted build-server isolation gate; see motor-runner README"]
fn isolated_access_repair_rescues_a_directory_a_job_made_unreadable() {
    let f = Fixture::new();
    let root = f.scratch.join("remote/worktrees/locked");
    let private = root.join("private");
    fs::create_dir_all(private.join("inner")).unwrap();
    // Setup grants the runner's ACL entry on the whole tree before a job.
    crate::isolation::restore_runner_access(&root).unwrap();
    let chown = Command::new("/usr/bin/sudo")
        .args(["-n", "/usr/bin/chown", "-R", "motor-job:motor-job", "--"])
        .arg(&root)
        .output()
        .unwrap();
    assert!(
        chown.status.success(),
        "{}",
        String::from_utf8_lossy(&chown.stderr)
    );
    sudo("/usr/bin/chmod", &["000"], &private);
    sudo("/usr/bin/chmod", &["0700"], &root);
    assert!(fs::read_dir(&root).is_err());

    crate::isolation::restore_workspace_access(&root).unwrap();
    fs::read_dir(&root).expect("the runner reads the root after the repair");
    fs::read_dir(&private).expect("the runner reads the locked directory after the repair");
    fs::read_dir(private.join("inner")).expect("and what lies below it");
    assert!(acl(&root).contains("mask::r-x"), "{}", acl(&root));
    for path in [&root, &private] {
        let metadata = fs::metadata(path).unwrap();
        assert_ne!(metadata.uid(), unsafe { libc::geteuid() }, "owner kept");
    }
    assert_eq!(
        fs::metadata(&private).unwrap().mode() & 0o700,
        0,
        "job bits kept"
    );
    crate::isolation::restore_runner_access(&f.scratch).unwrap();
}

fn base_store_config(f: &Fixture, command: &str) -> Config {
    let mut config = f.config();
    config.isolate_jobs = true;
    config.disable_warm_builds = false;
    config.manual_tick = true;
    config.account_subject = Some("acct:00000000000000000000000000".into());
    config.fixed_path_repositories = vec![f.root.clone()];
    config.base_commands = vec![command.into()];
    fs::create_dir_all(config.state_dir()).unwrap();
    fs::create_dir_all(&config.workspace_roots[0]).unwrap();
    assert_eq!(
        fs::metadata(config.state_dir()).unwrap().dev(),
        fs::metadata(&config.workspace_roots[0]).unwrap().dev(),
        "state and workspaces must share the fixture's reflink device"
    );
    require_reflink_scratch(&config.state_dir());
    config
}

async fn drive_base_store(runner: &Runner, condition: impl Fn() -> bool) {
    for _ in 0..2000 {
        runner.tick().unwrap();
        if condition() {
            return;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    panic!(
        "base store condition timed out: {}",
        runner_diagnostics(runner)
    );
}

async fn settle_base_store(f: &Fixture, runner: &Runner) -> Job {
    let (run, bytes) = f.run("true");
    f.submit(runner, &run, &bytes);
    drive_base_store(runner, || {
        let inner = runner.core.inner.lock().unwrap();
        inner
            .jobs
            .values()
            .any(|job| job.base && job.terminal.is_some())
            && inner.writer.is_none()
            && inner.base_finishes.is_empty()
            && inner.finalizing.is_empty()
    })
    .await;
    runner
        .core
        .inner
        .lock()
        .unwrap()
        .jobs
        .values()
        .filter(|job| job.base)
        .max_by_key(|job| job.order)
        .unwrap()
        .clone()
}

#[tokio::test]
#[ignore = "requires root, host system manager, and motor-cache/motor-job accounts"]
async fn isolated_base_fill_publishes_nested_uplift_links_and_replaces_current() {
    assert_eq!(unsafe { libc::geteuid() }, 0);
    let f = Fixture::new();
    let command = "mkdir -p \"$CARGO_TARGET_DIR/bd/nested/deeper\"; printf first > \"$CARGO_TARGET_DIR/bd/nested/deeper/artifact\"; ln \"$CARGO_TARGET_DIR/bd/nested/deeper/artifact\" \"$CARGO_TARGET_DIR/uplift\"";
    let config = base_store_config(&f, command);
    let runner = Runner::open(config.clone()).unwrap();
    let fill = settle_base_store(&f, &runner).await;
    assert_eq!(
        fill.terminal.as_ref().unwrap().outcome,
        Outcome::Exit { code: 0 }
    );
    let store = crate::build_base::repository_store(&config.state_dir(), &f.root);
    let current = store.join("current");
    let base: crate::build_base::Record = read_json(&current.join("base.json")).unwrap();
    let launched = fill
        .base_launch
        .as_ref()
        .expect("fill launch identity recorded");
    let request = fill.request.as_ref().unwrap();
    assert_eq!(base.format, 1);
    assert_eq!(base.commit, request.snapshot_commit);
    assert_eq!(base.tree, request.tree_hash);
    assert_eq!(base.inputs, launched.inputs);
    assert_eq!(base.fixed_path, launched.fixed_path);
    assert_eq!(base.fill_start, launched.reference);
    assert!(base.newest_mtime >= base.fill_start);
    let artifact = current.join("target/bd/nested/deeper/artifact");
    assert_eq!(
        fs::metadata(&artifact).unwrap().ino(),
        fs::metadata(current.join("target/uplift")).unwrap().ino()
    );
    let mut pending = vec![current.clone()];
    while let Some(path) = pending.pop() {
        let metadata = fs::symlink_metadata(&path).unwrap();
        assert_eq!(metadata.uid(), unsafe { libc::geteuid() });
        assert_eq!(metadata.mode() & 0o022, 0, "{}", path.display());
        let acl = Command::new("/usr/bin/getfacl")
            .args(["-cp", "--"])
            .arg(&path)
            .output()
            .unwrap();
        assert!(acl.status.success());
        let acl = String::from_utf8(acl.stdout).unwrap();
        assert!(
            acl.lines().all(|line| !line.starts_with("default:")
                && !(line.starts_with("user:") && !line.starts_with("user::"))
                && !(line.starts_with("group:") && !line.starts_with("group::"))),
            "{acl}"
        );
        if metadata.is_dir() {
            pending.extend(
                fs::read_dir(path)
                    .unwrap()
                    .map(|entry| entry.unwrap().path()),
            );
        }
    }
    let denied = Command::new("/usr/bin/sudo")
        .args([
            "-n",
            "-u",
            "motor-job",
            "--",
            "/bin/sh",
            "-c",
            "printf changed >> \"$1\"",
            "sh",
        ])
        .arg(&artifact)
        .output()
        .unwrap();
    assert!(!denied.status.success(), "job account wrote a base");
    assert_eq!(fs::read(&artifact).unwrap(), b"first");
    let old_inode = fs::metadata(&current).unwrap().ino();
    drop(runner);
    let mut replacement = config;
    replacement.base_commands = vec![
        "mkdir -p \"$CARGO_TARGET_DIR/bd\"; printf second > \"$CARGO_TARGET_DIR/bd/artifact\""
            .into(),
    ];
    let runner = Runner::open(replacement).unwrap();
    let second = settle_base_store(&f, &runner).await;
    assert_eq!(
        second.terminal.as_ref().unwrap().outcome,
        Outcome::Exit { code: 0 }
    );
    assert_ne!(fs::metadata(&current).unwrap().ino(), old_inode);
    assert_eq!(
        fs::read(current.join("target/bd/artifact")).unwrap(),
        b"second"
    );
    assert_eq!(fs::read_dir(store).unwrap().count(), 1);
}

#[tokio::test]
#[ignore = "requires root, host system manager, and motor-cache/motor-job accounts"]
async fn isolated_base_fill_external_home_hardlink_preserves_outside_metadata() {
    use std::os::fd::AsRawFd;
    assert_eq!(unsafe { libc::geteuid() }, 0);
    let f = Fixture::new();
    let mut config = base_store_config(&f, "mkdir -p \"$CARGO_TARGET_DIR/bd\"; printf outside > \"$HOME/outside\"; chmod 0640 \"$HOME/outside\"; setfacl -m u:motor-job:rw \"$HOME/outside\"; printf waiting > \"$CARGO_TARGET_DIR/control\"; while [ \"$(cat \"$CARGO_TARGET_DIR/control\")\" != exit ]; do sleep 0.01; done");
    let held_fill = Arc::new(Mutex::new(None::<Uuid>));
    let settled = Arc::new(AtomicBool::new(false));
    let release = Arc::new(AtomicBool::new(false));
    config.before_job_write = Some({
        let (held_fill, settled, release) = (held_fill.clone(), settled.clone(), release.clone());
        let (state, repository) = (config.state_dir(), f.root.clone());
        Arc::new(move |path| {
            let Some(id) = *held_fill.lock().unwrap() else {
                return Ok(());
            };
            if path == state.join("jobs").join(id.to_string()).join("record.json")
                && !crate::build_base::staging(&state, &repository, id).exists()
            {
                // Keep the launch handle (and private home) alive while checking
                // generation removal, independently of later runtime cleanup.
                settled.store(true, Ordering::SeqCst);
                let deadline = Instant::now() + Duration::from_secs(30);
                while !release.load(Ordering::SeqCst) && Instant::now() < deadline {
                    std::thread::sleep(Duration::from_millis(10));
                }
            }
            Ok(())
        })
    });
    struct Release(Arc<AtomicBool>);
    impl Drop for Release {
        fn drop(&mut self) {
            self.0.store(true, Ordering::SeqCst);
        }
    }
    let release_on_unwind = Release(release.clone());
    let runner = Runner::open(config.clone()).unwrap();
    let (run, bytes) = f.run("true");
    f.submit(&runner, &run, &bytes);
    drive_base_store(&runner, || {
        let inner = runner.core.inner.lock().unwrap();
        inner.jobs.values().any(|job| {
            job.base
                && crate::build_base::staging(&config.state_dir(), &f.root, job.id)
                    .join("target/control")
                    .is_file()
                && config
                    .state_dir()
                    .join("sandboxes")
                    .join(job.id.to_string())
                    .join("home/outside")
                    .is_file()
        })
    })
    .await;
    let fill = runner
        .core
        .inner
        .lock()
        .unwrap()
        .jobs
        .values()
        .find(|job| job.base && job.terminal.is_none())
        .unwrap()
        .clone();
    let generation = crate::build_base::staging(&config.state_dir(), &f.root, fill.id);
    let home = config
        .state_dir()
        .join("sandboxes")
        .join(fill.id.to_string())
        .join("home/outside");
    // Home and target are separate sandbox bind mounts, so link(2) between
    // them is rejected as EXDEV. Plant the equivalent host-side link to exercise
    // publication's link defense independently of that mount-layout protection.
    fs::hard_link(&home, generation.join("target/bd/external")).unwrap();
    let outside = fs::File::open(&home).unwrap();
    let descriptor_path = PathBuf::from(format!(
        "/proc/{}/fd/{}",
        std::process::id(),
        outside.as_raw_fd()
    ));
    let acl = || {
        let output = Command::new("/usr/bin/getfacl")
            .args(["-cp", "--"])
            .arg(&descriptor_path)
            .output()
            .unwrap();
        assert!(output.status.success());
        output.stdout
    };
    let before = outside.metadata().unwrap();
    let before_acl = acl();
    *held_fill.lock().unwrap() = Some(fill.id);
    fs::write(generation.join("target/control"), b"exit").unwrap();
    until(|| settled.load(Ordering::SeqCst)).await;
    assert!(
        !crate::build_base::repository_store(&config.state_dir(), &f.root)
            .join("current")
            .exists()
    );
    assert!(!generation.exists());
    let after = outside.metadata().unwrap();
    assert_eq!(after.uid(), before.uid());
    assert_eq!(after.gid(), before.gid());
    assert_eq!(after.mode(), before.mode());
    assert_eq!(acl(), before_acl);
    assert_eq!(fs::read(&descriptor_path).unwrap(), b"outside");
    drop(release_on_unwind);
    drive_base_store(&runner, || {
        let inner = runner.core.inner.lock().unwrap();
        inner.jobs[&fill.id].terminal.is_some()
            && inner.writer.is_none()
            && inner.base_finishes.is_empty()
            && inner.finalizing.is_empty()
    })
    .await;
    assert_eq!(
        runner.core.inner.lock().unwrap().jobs[&fill.id]
            .terminal
            .as_ref()
            .unwrap()
            .outcome,
        Outcome::Exit { code: 0 }
    );
}

#[test]
#[ignore = "requires root and provisioned motor-cache/motor-job accounts"]
fn isolated_base_startup_removes_only_retired_and_proven_stopped_staging() {
    use std::os::unix::process::ExitStatusExt;
    assert_eq!(unsafe { libc::geteuid() }, 0);
    let f = Fixture::new();
    let mut config = base_store_config(&f, "true");
    config.disable_warm_builds = true;
    let runner = Runner::open(config.clone()).unwrap();
    let (mut request, _) = f.run("true");
    request.siblings.clear();
    let mut terminal_job = Job::new(request.job_id, Some(request.clone()), 123, 1, 2);
    terminal_job.base = true;
    terminal_job.terminal(Outcome::Exit { code: 0 }, 125);
    atomic_json(&runner.record_path(terminal_job.id), &terminal_job).unwrap();
    let mut generations = Vec::new();
    for id in [terminal_job.id, Uuid::now_v7(), Uuid::now_v7()] {
        request.job_id = id;
        crate::build_base::prepare_target(&config, &request)
            .unwrap()
            .unwrap();
        generations.push(crate::build_base::staging(&config.state_dir(), &f.root, id));
    }
    let live_id = request.job_id;
    let store = crate::build_base::repository_store(&config.state_dir(), &f.root);
    let retired = store.join(format!("retired-{}", Uuid::now_v7()));
    fs::create_dir(&retired).unwrap();
    fs::write(retired.join("old"), b"retired").unwrap();
    fs::create_dir(store.join("current")).unwrap();
    fs::write(store.join("current/base.json"), b"previous").unwrap();
    drop(runner);
    config.systemd_command = Some(Arc::new(move |args| {
        let live = args.iter().any(|arg| arg.contains(&live_id.to_string()));
        Ok(std::process::Output {
            status: std::process::ExitStatus::from_raw(0),
            stdout: if args.iter().any(|arg| arg.contains("Version")) {
                b"257\n".to_vec()
            } else if args[0] == "show" {
                if live {
                    b"ActiveState=active\n".to_vec()
                } else {
                    b"ActiveState=inactive\n".to_vec()
                }
            } else {
                Vec::new()
            },
            stderr: Vec::new(),
        })
    }));
    let _runner = Runner::open(config).unwrap();
    assert!(!retired.exists());
    assert!(!generations[0].exists());
    assert!(!generations[1].exists());
    assert!(
        generations[2].exists(),
        "unknown but live fill must retain staging"
    );
    assert_eq!(
        fs::read(store.join("current/base.json")).unwrap(),
        b"previous"
    );
}

#[tokio::test]
#[ignore = "requires root and provisioned motor-cache/motor-job accounts"]
async fn isolated_base_recovered_fill_publishes_recorded_inputs() {
    use std::os::unix::process::ExitStatusExt;
    assert_eq!(unsafe { libc::geteuid() }, 0);
    let f = Fixture::new();
    let mut config = base_store_config(&f, "true");
    config.disable_warm_builds = true;
    let active = Arc::new(AtomicBool::new(true));
    config.systemd_command = Some({
        let active = active.clone();
        Arc::new(move |args| {
            Ok(std::process::Output {
                status: std::process::ExitStatus::from_raw(0),
                stdout: if args.iter().any(|arg| arg.contains("Version")) {
                    b"257\n".to_vec()
                } else if args[0] == "show" {
                    if active.load(Ordering::SeqCst) {
                        b"ActiveState=active\nSubState=running\n".to_vec()
                    } else {
                        b"ActiveState=inactive\nSubState=dead\nResult=success\nExecMainCode=1\nExecMainStatus=0\n".to_vec()
                    }
                } else {
                    Vec::new()
                },
                stderr: Vec::new(),
            })
        })
    });
    let runner = Runner::open(config.clone()).unwrap();
    let (run, bytes) = f.run("true");
    f.submit(&runner, &run, &bytes);
    let mut request = run;
    request.job_id = Uuid::now_v7();
    request.workspace_key = format!("base:{}", f.root);
    request.cwd = f.main.display().to_string();
    request.siblings.clear();
    let target = crate::build_base::prepare_target(&config, &request)
        .unwrap()
        .unwrap();
    fs::create_dir(target.join("bd")).unwrap();
    fs::write(target.join("bd/artifact"), b"recovered").unwrap();
    let generation = target.parent().unwrap();
    assert!(Command::new("/usr/bin/chown")
        .args(["-R", "-P", "motor-cache:motor-cache", "--"])
        .arg(generation)
        .status()
        .unwrap()
        .success());
    let inputs = crate::build_base::CompatibleInputs {
        toolchain: crate::build_base::ToolchainIdentity {
            rustc_verbose: "recorded launch compiler, not current compiler".into(),
            rust_toolchain: None,
            rust_toolchain_toml: None,
            wrapper: None,
        },
        environment: "recorded launch fingerprint".into(),
    };
    let mut job = Job::new(request.job_id, Some(request), 123, 1, 2);
    job.base = true;
    job.writer_descriptor = Some("recovered-descriptor".into());
    job.started_ms = Some(124);
    job.state = JobState::Running;
    job.durable_output = true;
    job.process = Some(platform::ProcessIdentity {
        pid: 0,
        start: 0,
        unit: Some(job.id),
    });
    job.base_launch = Some(crate::build_base::FillLaunch {
        inputs: inputs.clone(),
        fixed_path: "/w/recorded/path".into(),
        reference: crate::build_base::Timestamp::now(),
    });
    let output = config
        .state_dir()
        .join("sandboxes")
        .join(job.id.to_string())
        .join("output");
    fs::create_dir_all(&output).unwrap();
    for stream in ["stdout", "stderr"] {
        fs::write(output.join(stream), b"").unwrap();
    }
    atomic_json(&runner.record_path(job.id), &job).unwrap();
    drop(runner);
    let runner = Runner::open(config.clone()).unwrap();
    assert!(generation.exists());
    assert_eq!(
        runner.core.inner.lock().unwrap().jobs[&job.id].base_launch,
        job.base_launch
    );
    fs::write(output.join("exit"), b"0\n").unwrap();
    active.store(false, Ordering::SeqCst);
    drive_base_store(&runner, || {
        let inner = runner.core.inner.lock().unwrap();
        inner.jobs[&job.id].terminal.is_some()
            && inner.writer.is_none()
            && inner.base_finishes.is_empty()
            && inner.finalizing.is_empty()
    })
    .await;
    let current = crate::build_base::repository_store(&config.state_dir(), &f.root).join("current");
    let base: crate::build_base::Record = read_json(&current.join("base.json")).unwrap();
    assert_eq!(base.inputs, inputs);
    assert_eq!(base.fixed_path, PathBuf::from("/w/recorded/path"));
    assert_eq!(base.fill_start, job.base_launch.unwrap().reference);
    assert_eq!(
        fs::read(current.join("target/bd/artifact")).unwrap(),
        b"recovered"
    );
}

#[tokio::test]
#[ignore = "requires root, host system manager, and motor-cache/motor-job accounts"]
async fn isolated_base_fill_masking_runner_source_acl_is_refused() {
    assert_eq!(unsafe { libc::geteuid() }, 0);
    let f = Fixture::new();
    let config = base_store_config(&f, "chmod 0700 .; mkdir -p \"$CARGO_TARGET_DIR/bd\"; printf built > \"$CARGO_TARGET_DIR/bd/artifact\"");
    let runner = Runner::open(config.clone()).unwrap();
    let fill = settle_base_store(&f, &runner).await;
    assert_eq!(
        fill.terminal.as_ref().unwrap().outcome,
        Outcome::Exit { code: 0 }
    );
    let store = crate::build_base::repository_store(&config.state_dir(), &f.root);
    assert!(!store.join("current").exists());
    assert!(!crate::build_base::staging(&config.state_dir(), &f.root, fill.id).exists());
}

#[tokio::test]
#[ignore = "requires root, host system manager, and motor-cache/motor-job accounts"]
async fn isolated_base_retry_uses_a_fresh_single_launch_fill_unit_identity() {
    assert_eq!(unsafe { libc::geteuid() }, 0);
    let f = Fixture::new();
    let runner = Runner::open(base_store_config(&f, "exit 1")).unwrap();
    let first = settle_base_store(&f, &runner).await;
    assert_eq!(
        first.terminal.as_ref().unwrap().outcome,
        Outcome::Exit { code: 1 }
    );
    assert_eq!(first.process.unwrap().unit, Some(first.id));
    let desired = {
        let mut inner = runner.core.inner.lock().unwrap();
        let repository = inner.catalog.repositories.get_mut(&f.root).unwrap();
        repository.failed_descriptor = None;
        repository.desired.clone().unwrap()
    };
    runner.queue_next_base(&f.root, &desired).unwrap();
    drive_base_store(&runner, || {
        let inner = runner.core.inner.lock().unwrap();
        inner
            .jobs
            .values()
            .filter(|job| job.base && job.terminal.is_some())
            .count()
            == 2
            && inner.writer.is_none()
            && inner.base_finishes.is_empty()
            && inner.finalizing.is_empty()
    })
    .await;
    let jobs = runner
        .core
        .inner
        .lock()
        .unwrap()
        .jobs
        .values()
        .filter(|job| job.base)
        .cloned()
        .collect::<Vec<_>>();
    assert_eq!(jobs.len(), 2);
    assert_ne!(jobs[0].id, jobs[1].id);
    for job in jobs {
        assert_eq!(job.process.unwrap().unit, Some(job.id));
    }
}
