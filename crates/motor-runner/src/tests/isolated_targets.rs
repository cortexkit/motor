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
    // The job left the root as the production incident did: owned by the
    // job account at mode 0700. The runner's reclaim at job end set the mask
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
