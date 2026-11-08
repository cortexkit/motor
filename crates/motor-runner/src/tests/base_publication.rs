use super::*;
use crate::build_base::{self, CompatibleInputs, FillLaunch, Record, Timestamp, ToolchainIdentity};

#[path = "base_legacy_job.rs"]
mod legacy;

fn launch() -> FillLaunch {
    FillLaunch {
        inputs: CompatibleInputs {
            toolchain: ToolchainIdentity {
                rustc_verbose: "runner rustc verbose identity".into(),
                rust_toolchain: None,
                rust_toolchain_toml: Some("toolchain-blob".into()),
                wrapper: None,
            },
            environment: "launch-environment".into(),
        },
        fixed_path: "/w/acct/repo/main".into(),
        reference: Timestamp::now(),
    }
}

fn record(commit: &str) -> Record {
    let launch = launch();
    Record {
        format: 1,
        commit: commit.into(),
        tree: format!("{commit}-tree"),
        inputs: launch.inputs,
        fixed_path: launch.fixed_path,
        fill_start: launch.reference,
        newest_mtime: Timestamp::now(),
    }
}

#[test]
fn base_launch_roundtrips_and_pre_change_job_schema_loads_both_record_shapes() {
    let mut job = Job::new(Uuid::now_v7(), None, 123, 1, 2);
    let original = serde_json::to_vec(&job).unwrap();
    let old: legacy::LegacyJob = serde_json::from_slice(&original).unwrap();
    assert_eq!(serde_json::to_vec(&old).unwrap(), original);
    assert!(!String::from_utf8_lossy(&original).contains("base_launch"));
    assert!(serde_json::from_slice::<Job>(&original)
        .unwrap()
        .base_launch
        .is_none());
    job.base = true;
    job.base_launch = Some(launch());
    let fill_bytes = serde_json::to_vec(&job).unwrap();
    let old: legacy::LegacyJob = serde_json::from_slice(&fill_bytes).unwrap();
    assert_eq!(old.id, job.id);
    assert!(old.base);
    let loaded: Job = serde_json::from_slice(&fill_bytes).unwrap();
    assert_eq!(loaded.base_launch, job.base_launch);
    assert_eq!(serde_json::to_vec(&loaded).unwrap(), fill_bytes);
}

#[test]
fn generation_store_ignores_triggering_worktree_parent() {
    let f = Fixture::new();
    let config = f.config();
    let (mut first, _) = f.run("true");
    let mut second = first.clone();
    first.workspace_key = f.scratch.join("one/worktree").display().to_string();
    second.workspace_key = f
        .scratch
        .join("elsewhere/two/worktree")
        .display()
        .to_string();
    assert_ne!(first.workspace_key, second.workspace_key);
    let a = build_base::staging(&config.state_dir(), &first.repository_root, first.job_id);
    let b = build_base::staging(&config.state_dir(), &second.repository_root, second.job_id);
    assert_eq!(a, b);
    assert!(a.ends_with(format!(
        "build-bases/{}/staging-{}",
        workspace::name(&f.root),
        first.job_id
    )));
    assert_ne!(
        build_base::repository_store(&config.state_dir(), &f.root),
        build_base::repository_store(&config.state_dir(), &f.canonical)
    );
}

#[test]
fn failed_cancelled_timed_out_and_unrun_fills_keep_current_and_log_each_reason() {
    let f = Fixture::new();
    let mut config = f.config();
    config.disable_warm_builds = true;
    config.manual_tick = true;
    let runner = Runner::open(config).unwrap();
    let (request, _) = f.run("true");
    let mut job = Job::new(request.job_id, Some(request), 123, 1, 2);
    job.base = true;
    job.started_ms = Some(124);
    job.base_launch = Some(launch());
    let generation = build_base::staging(&runner.core.trees.state, &f.root, job.id);
    fs::create_dir_all(&generation).unwrap();
    let current = generation.parent().unwrap().join("current");
    fs::create_dir_all(&current).unwrap();
    fs::write(current.join("base.json"), b"previous base").unwrap();
    for (started, killed, success, identity, reason) in [
        (None, None, true, true, "fill never ran"),
        (
            Some(124),
            None,
            false,
            true,
            "fill did not exit successfully",
        ),
        (
            Some(124),
            Some(Killed::Cancel),
            true,
            true,
            "fill cancelled or timed out",
        ),
        (
            Some(124),
            Some(Killed::Deadline),
            true,
            true,
            "fill cancelled or timed out",
        ),
        (
            Some(124),
            None,
            true,
            false,
            "fill launch identity unavailable",
        ),
    ] {
        job.started_ms = started;
        job.kill = killed.map(|kind| (kind, 125));
        job.base_launch = identity.then(launch);
        runner
            .core
            .inner
            .lock()
            .unwrap()
            .jobs
            .insert(job.id, job.clone());
        let output = capture_runner_logs(|| runner.publish_build_base(job.id, success));
        assert!(
            output.contains("build base publication refused"),
            "{output}"
        );
        assert!(output.contains(reason), "{output}");
        assert_eq!(
            fs::read(current.join("base.json")).unwrap(),
            b"previous base"
        );
        assert!(generation.exists());
    }
}

#[test]
fn tracked_change_and_restore_and_untracked_root_lock_are_refused_before_metadata_repair() {
    let f = Fixture::new();
    let mut config = f.config();
    config.disable_warm_builds = true;
    config.manual_tick = true;
    let runner = Runner::open(config).unwrap();
    let (request, bytes) = f.run("true");
    f.submit(&runner, &request, &bytes);
    let trees = &runner.core.trees;
    let reference = Timestamp::now();
    build_base::verify_unchanged(trees, &request, &f.main, reference).unwrap();
    let original = fs::read(f.main.join("source")).unwrap();
    fs::write(f.main.join("source"), b"changed").unwrap();
    let error = build_base::verify_unchanged(trees, &request, &f.main, reference).unwrap_err();
    assert!(
        error.to_string().contains("tracked inode changed"),
        "{error}"
    );
    fs::write(f.main.join("source"), original).unwrap();
    assert_eq!(trees.tree_hash(&f.main).unwrap(), request.tree_hash);
    let error = build_base::verify_unchanged(trees, &request, &f.main, reference).unwrap_err();
    assert!(
        error.to_string().contains("tracked inode changed"),
        "{error}"
    );
    let reference = Timestamp::now();
    fs::write(f.main.join("Cargo.lock"), b"untracked lock").unwrap();
    let error = build_base::verify_unchanged(trees, &request, &f.main, reference).unwrap_err();
    assert!(
        error
            .to_string()
            .contains("tracked directory inode changed"),
        "{error}"
    );
}

#[test]
fn publication_is_exclusive_no_follow_and_atomically_replaces_the_generation() {
    use std::os::unix::fs::{symlink, MetadataExt};
    let scratch = ScratchDir::new("base-record-publication");
    let store = build_base::repository_store(&scratch, "/repo");
    let first = build_base::staging(&scratch, "/repo", Uuid::now_v7());
    fs::create_dir_all(first.join("target/bd/nested")).unwrap();
    fs::write(first.join("target/bd/nested/artifact"), b"first").unwrap();
    let first_record = record("first");
    build_base::publish_record(&first, &first_record).unwrap();
    let old_inode = fs::metadata(store.join("current")).unwrap().ino();
    let old = fs::File::open(store.join("current")).unwrap();
    let second = build_base::staging(&scratch, "/repo", Uuid::now_v7());
    fs::create_dir_all(second.join("target/bd")).unwrap();
    fs::write(second.join("target/bd/artifact"), b"second").unwrap();
    let second_record = record("second");
    build_base::publish_record(&second, &second_record).unwrap();
    let current = store.join("current");
    assert_ne!(fs::metadata(&current).unwrap().ino(), old_inode);
    assert_eq!(old.metadata().unwrap().ino(), old_inode);
    assert_eq!(
        read_json::<Record>(&current.join("base.json")).unwrap(),
        second_record
    );
    assert_eq!(
        fs::read(current.join("target/bd/artifact")).unwrap(),
        b"second"
    );
    assert_eq!(fs::read_dir(&store).unwrap().count(), 1);
    for symlink_record in [false, true] {
        let next = build_base::staging(&scratch, "/repo", Uuid::now_v7());
        fs::create_dir_all(&next).unwrap();
        let outside = scratch.join("outside");
        fs::write(&outside, b"untouched").unwrap();
        if symlink_record {
            symlink(&outside, next.join("base.json")).unwrap();
        } else {
            fs::write(next.join("base.json"), b"already exists").unwrap();
        }
        let error = build_base::publish_record(&next, &record("refused")).unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::AlreadyExists);
        assert_eq!(fs::read(&outside).unwrap(), b"untouched");
        assert_eq!(
            read_json::<Record>(&current.join("base.json")).unwrap(),
            second_record
        );
    }
}

#[test]
fn startup_keeps_live_fill_generation_and_recorded_launch_inputs_and_deletes_retired() {
    use std::os::unix::process::ExitStatusExt;
    let f = Fixture::new();
    let mut config = f.config();
    config.manual_tick = true;
    config.disable_warm_builds = true;
    let active = Arc::new(AtomicBool::new(true));
    let commands = Arc::new(Mutex::new(Vec::new()));
    config.systemd_command = Some({
        let (active, commands) = (active.clone(), commands.clone());
        Arc::new(move |args| {
            commands.lock().unwrap().push(args.join(" "));
            Ok(std::process::Output {
                status: std::process::ExitStatus::from_raw(0),
                stdout: if args[0] == "show" {
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
    let (mut request, _) = f.run("true");
    request.siblings.clear();
    let mut job = Job::new(request.job_id, Some(request), 123, 1, 2);
    job.base = true;
    job.state = JobState::Running;
    job.writer_descriptor = Some("recorded-descriptor".into());
    job.started_ms = Some(124);
    job.durable_output = true;
    job.process = Some(platform::ProcessIdentity {
        pid: 0,
        start: 0,
        unit: Some(job.id),
    });
    job.base_launch = Some(launch());
    let generation = build_base::staging(&config.state_dir(), &f.root, job.id);
    fs::create_dir_all(generation.join("target/bd")).unwrap();
    fs::write(generation.join("target/bd/artifact"), b"live").unwrap();
    let retired = generation
        .parent()
        .unwrap()
        .join(format!("retired-{}", Uuid::now_v7()));
    fs::create_dir_all(&retired).unwrap();
    let output = config
        .state_dir()
        .join("sandboxes")
        .join(job.id.to_string())
        .join("output");
    fs::create_dir_all(&output).unwrap();
    for stream in ["stdout", "stderr"] {
        fs::write(output.join(stream), b"").unwrap();
    }
    let path = config
        .state_dir()
        .join("jobs")
        .join(job.id.to_string())
        .join("record.json");
    atomic_json(&path, &job).unwrap();
    let runner = Runner::open(config).unwrap();
    assert_eq!(
        runner.core.inner.lock().unwrap().jobs[&job.id].base_launch,
        job.base_launch
    );
    assert_eq!(runner.core.inner.lock().unwrap().writer, Some(job.id));
    assert_eq!(
        fs::read(generation.join("target/bd/artifact")).unwrap(),
        b"live"
    );
    assert!(!retired.exists());
    assert!(!commands
        .lock()
        .unwrap()
        .iter()
        .any(|line| line.starts_with("stop ")));
    // End the synthetic monitor without exercising publication or privileged
    // removal: those operations have their own real-account isolation fixtures.
    fs::remove_dir_all(generation).unwrap();
    fs::write(output.join("exit"), b"0\n").unwrap();
    active.store(false, Ordering::SeqCst);
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        let done = runner.core.inner.lock().unwrap().jobs[&job.id]
            .terminal
            .is_some();
        if done {
            break;
        }
        assert!(
            Instant::now() < deadline,
            "synthetic recovery monitor did not stop"
        );
        std::thread::sleep(Duration::from_millis(10));
    }
}

#[test]
fn startup_keeps_unknown_staging_on_live_transitions_or_unit_query_errors() {
    use std::os::unix::process::ExitStatusExt;
    for state in [
        "ActiveState=active\n",
        "ActiveState=activating\n",
        "ActiveState=deactivating\n",
        "ActiveState=failed\n",
        "missing state\n",
        "query error",
    ] {
        let f = Fixture::new();
        let mut config = f.config();
        config.manual_tick = true;
        config.disable_warm_builds = true;
        config.systemd_command = Some(Arc::new(move |_| {
            if state == "query error" {
                return Err(io::Error::other("injected query error"));
            }
            Ok(std::process::Output {
                status: std::process::ExitStatus::from_raw(0),
                stdout: state.as_bytes().to_vec(),
                stderr: Vec::new(),
            })
        }));
        let generation = build_base::staging(&config.state_dir(), &f.root, Uuid::now_v7());
        fs::create_dir_all(generation.join("target")).unwrap();
        let logs = capture_runner_logs(|| {
            let _runner = Runner::open(config).unwrap();
        });
        assert!(generation.exists(), "staging removed for {state}");
        if state.contains("error") || state.contains("missing") || state.contains("failed") {
            assert!(logs.contains("staging retained"), "{logs}");
        }
    }
}
