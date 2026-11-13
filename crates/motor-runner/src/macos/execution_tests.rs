use super::super::tests::{build_disk_marker, write_build_disk, write_build_disk_marker};
use super::*;
use crate::test_helpers::CanonicalScratchDir as ScratchDir;
const NONCE: &str = "0123456789abcdef0123456789abcdef";
fn transport(root: &Path, running: bool) -> (Transport, UnixStream) {
    let runtime = Runtime::default();
    let entry = runtime
        .begin(root, Uuid::now_v7(), "/Users/test/repo")
        .unwrap();
    let (socket, peer) = UnixStream::pair().unwrap();
    socket.set_nonblocking(true).unwrap();
    let mut protocol = Protocol::with_profile(Profile::Macos, NONCE).unwrap();
    if running {
        protocol
            .accept(
                Peer::Agent,
                &Frame::Hello {
                    version: motor_winagent::MACOS_PROTOCOL_VERSION,
                    nonce: NONCE.into(),
                },
            )
            .unwrap();
        protocol
            .accept(
                Peer::Host,
                &Frame::TreeEnd {
                    command: motor_winagent::Command {
                        command: "true".into(),
                        cwd: "/Users/test/repo".into(),
                        workspace: "/Users/test/repo".into(),
                        source_commit: "a".repeat(40),
                        env: Default::default(),
                    },
                },
            )
            .unwrap();
        protocol.accept(Peer::Agent, &Frame::Started {}).unwrap();
    }
    (
        Transport {
            socket,
            input: Vec::new(),
            protocol,
            entry,
            deadline: None,
            acceptor: None,
        },
        peer,
    )
}
#[test]
fn signal_is_preserved_and_output_precedes_terminal_status() {
    for terminal in [Frame::Exit { code: 7 }, Frame::Signal { signal: 11 }] {
        let scratch = ScratchDir::new("macos-output");
        let (mut transport, mut peer) = transport(&scratch.join("state"), true);
        for (stream, bytes) in [
            (Stream::Stdout, b"one".to_vec()),
            (Stream::Stderr, b"error".to_vec()),
            (Stream::Stdout, b"two".to_vec()),
        ] {
            motor_winagent::write_frame(&mut peer, &Frame::Output { stream, bytes }).unwrap();
        }
        motor_winagent::write_frame(&mut peer, &terminal).unwrap();
        let (stdout_tx, stdout_rx) = mpsc::sync_channel(4);
        let (stderr_tx, stderr_rx) = mpsc::sync_channel(4);
        let outcome = run_output(&mut transport, &stdout_tx, &stderr_tx)
            .unwrap()
            .wire();
        drop(stdout_tx);
        drop(stderr_tx);
        assert_eq!(
            stdout_rx.into_iter().flatten().collect::<Vec<_>>(),
            b"onetwo"
        );
        assert_eq!(
            stderr_rx.into_iter().flatten().collect::<Vec<_>>(),
            b"error"
        );
        assert_eq!(
            outcome,
            if matches!(terminal, Frame::Exit { .. }) {
                motor_protocol::Outcome::Exit { code: 7 }
            } else {
                motor_protocol::Outcome::Signal { signal: 11 }
            }
        );
    }
}
#[test]
fn missing_exit_or_signal_names_output_incomplete() {
    let scratch = ScratchDir::new("macos-incomplete");
    let (mut transport, peer) = transport(&scratch.join("state"), true);
    drop(peer);
    let (tx, _) = mpsc::sync_channel(1);
    let error = run_output(&mut transport, &tx, &tx).unwrap_err();
    assert!(error.to_string().contains("output_incomplete"));
}
#[test]
fn wrong_agent_version_refuses_with_image_name_before_command() {
    let scratch = ScratchDir::new("macos-agent-version");
    let (mut transport, mut peer) = transport(&scratch.join("state"), false);
    let mut acceptor =
        motor_macvm::SessionAcceptor::new(NONCE.into(), "sealed-image-27".into()).unwrap();
    acceptor.claim().unwrap();
    transport.acceptor = Some(acceptor);
    motor_winagent::write_frame(
        &mut peer,
        &Frame::Hello {
            version: 99,
            nonce: NONCE.into(),
        },
    )
    .unwrap();
    let error = transport.receive().unwrap_err().to_string();
    assert!(
        error.contains("guest_agent_version") && error.contains("sealed-image-27"),
        "{error}"
    );
    peer.set_nonblocking(true).unwrap();
    assert_eq!(
        peer.read(&mut [0; 1]).unwrap_err().kind(),
        io::ErrorKind::WouldBlock
    );
}
#[test]
fn blocked_output_and_unresponsive_agent_both_observe_cancel_within_two_seconds() {
    for blocked in [false, true] {
        let scratch = ScratchDir::new("macos-cancel-io");
        let (mut transport, mut peer) = transport(&scratch.join("state"), true);
        let entry = transport.entry.clone();
        let (tx, rx) = mpsc::sync_channel(1);
        if blocked {
            for _ in 0..3 {
                motor_winagent::write_frame(
                    &mut peer,
                    &Frame::Output {
                        stream: Stream::Stdout,
                        bytes: vec![1; 1024],
                    },
                )
                .unwrap();
            }
        }
        let worker = std::thread::spawn(move || run_output(&mut transport, &tx, &tx));
        if blocked {
            let deadline = Instant::now() + Duration::from_secs(2);
            while !entry.output_blocked.load(Ordering::Acquire) {
                assert!(Instant::now() < deadline);
                std::thread::yield_now();
            }
        }
        let now = Instant::now();
        entry.cancel.cancel();
        assert!(worker.join().unwrap().is_err());
        assert!(now.elapsed() < Duration::from_secs(2));
        drop(rx);
        drop(peer);
    }
}
fn staged(root: &Path, id: Uuid) -> Staged {
    let tree = root.join("jobs").join(id.to_string());
    let workspace = tree.join("Users/test/repo");
    std::fs::create_dir_all(&workspace).unwrap();
    Staged {
        tree: tree.clone(),
        workspace,
        cargo_home: tree.join("cargo-home"),
        siblings: vec![],
        prepared_javascript_directories: vec![],
        source_files: vec![],
    }
}

#[test]
fn linked_worktree_sibling_alias_shares_the_read_only_snapshot() {
    let scratch = ScratchDir::new("macos-sibling-alias");
    let mut staged = staged(&scratch, Uuid::now_v7());
    let host = staged.tree.join("Users/test/subconscious");
    staged
        .siblings
        .push(("/Users/test/subconscious".into(), host.clone()));
    let workspace = "/Users/test/worktrees/branch";
    let siblings = sibling_shares(workspace, &staged).unwrap();
    assert_eq!(
        siblings,
        vec![
            ShareLocation {
                host: host.clone(),
                tag: "sibling-0".into(),
                guest: "/Users/test/subconscious".into(),
            },
            ShareLocation {
                host,
                tag: "sibling-1".into(),
                guest: "/Users/test/worktrees/subconscious".into(),
            },
        ]
    );
    let job = JobVmConfig::new(JobVmRequest {
        clone: scratch.join("clone"),
        nonce_disk: scratch.join("nonce"),
        workspace: ShareLocation {
            host: staged.workspace,
            tag: "workspace".into(),
            guest: workspace.into(),
        },
        siblings: siblings.clone(),
        cargo_home: ShareLocation {
            host: staged.cargo_home,
            tag: "cargo-home".into(),
            guest: motor_macvm::CARGO_HOME.into(),
        },
    })
    .unwrap();
    assert_eq!(job.shares().len(), 4);
    for (share, location) in job.shares()[1..3].iter().zip(siblings) {
        assert_eq!(share.location, location);
        assert!(share.read_only);
    }
}

#[test]
#[cfg(unix)]
fn sibling_alias_map_uses_existing_guest_roles_in_canonical_then_alias_order() {
    let scratch = ScratchDir::new("macos-sibling-agent-compatibility");
    let mut staged = staged(&scratch, Uuid::now_v7());
    staged.siblings = [
        "/Users/test/subconscious",
        "/Users/test/worktrees/adjacent",
        "/Users/other/third",
    ]
    .into_iter()
    .map(|canonical| {
        (
            canonical.to_owned(),
            staged.tree.join(canonical.trim_start_matches('/')),
        )
    })
    .collect();
    let workspace = "/Users/test/worktrees/branch";
    let siblings = sibling_shares(workspace, &staged).unwrap();
    assert_eq!(
        siblings
            .iter()
            .map(|share| (share.tag.as_str(), share.guest.to_str().unwrap()))
            .collect::<Vec<_>>(),
        vec![
            ("sibling-0", "/Users/test/subconscious"),
            ("sibling-1", "/Users/test/worktrees/adjacent"),
            ("sibling-2", "/Users/other/third"),
            ("sibling-3", "/Users/test/worktrees/subconscious"),
            ("sibling-4", "/Users/test/worktrees/third"),
        ]
    );
    assert_eq!(siblings[0].host, siblings[3].host);
    assert_eq!(siblings[2].host, siblings[4].host);
    let job = JobVmConfig::new(JobVmRequest {
        clone: scratch.join("clone"),
        nonce_disk: scratch.join("nonce"),
        workspace: ShareLocation {
            host: staged.workspace,
            tag: "workspace".into(),
            guest: workspace.into(),
        },
        siblings,
        cargo_home: ShareLocation {
            host: staged.cargo_home,
            tag: "cargo-home".into(),
            guest: motor_macvm::CARGO_HOME.into(),
        },
    })
    .unwrap();
    let mounts = job.mounts();
    assert!(mounts[1..6].iter().all(|mount| mount.read_only));
    motor_macagent::mounts::validate(&mounts).unwrap();
}

#[test]
fn adjacent_workspace_sibling_has_no_alias_share() {
    let scratch = ScratchDir::new("macos-sibling-adjacent");
    let mut staged = staged(&scratch, Uuid::now_v7());
    let host = staged.tree.join("Users/test/subconscious");
    staged
        .siblings
        .push(("/Users/test/subconscious".into(), host.clone()));
    assert_eq!(
        sibling_shares("/Users/test/repo", &staged).unwrap(),
        vec![ShareLocation {
            host,
            tag: "sibling-0".into(),
            guest: "/Users/test/subconscious".into(),
        }]
    );
}

/// A job's runtime entry with its workspace lock and an empty job clone
/// directory holding the blank build disk that `clone_image` creates.
fn job(root: &Path, runtime: &Runtime) -> Arc<Entry> {
    let entry = runtime
        .begin(root, Uuid::now_v7(), "/Users/test/repo")
        .unwrap();
    runtime.workspace(root, &entry).unwrap();
    let clone = entry.owned.lock().unwrap().record.paths.clone.clone();
    std::fs::create_dir_all(&clone).unwrap();
    motor_macvm::create_blank_build_disk(&clone.join(motor_macvm::BUILD_DISK)).unwrap();
    entry
}
fn kept(root: &Path) -> std::path::PathBuf {
    root.join("targets")
        .join(lifecycle::image("/Users/test/repo"))
}
fn job_disk(entry: &Entry) -> std::path::PathBuf {
    entry
        .owned
        .lock()
        .unwrap()
        .record
        .paths
        .clone
        .join(motor_macvm::BUILD_DISK)
}
fn finish(runtime: &Runtime, root: &Path, entry: &Entry) {
    entry.owned.lock().unwrap().record.stop_proven = true;
    runtime.finish(root, entry);
}

#[test]
fn kept_build_disk_is_cloned_per_job_and_replaced_only_after_stop_proof() {
    let scratch = ScratchDir::new("macos-build-disk-swap");
    let root = scratch.join("state");
    let config = Config::new(&root, "macos");
    let runtime = Runtime::default();
    let entry = job(&root, &runtime);
    write_build_disk(&kept(&root), b"old");
    build_disk_in(&config, &entry).unwrap();
    assert_eq!(build_disk_marker(&job_disk(&entry), 3), b"old");
    // The job writes its own clone; the kept disk is a different file.
    write_build_disk_marker(&job_disk(&entry), b"new");
    assert_eq!(build_disk_marker(&kept(&root), 3), b"old");
    assert!(
        target_out(&config, &entry).is_err(),
        "publication requires the stop proof"
    );
    assert_eq!(build_disk_marker(&kept(&root), 3), b"old");
    runtime.stop(&root, &entry).unwrap();
    target_out(&config, &entry).unwrap();
    assert_eq!(build_disk_marker(&kept(&root), 3), b"new");
    assert_eq!(
        std::fs::metadata(kept(&root)).unwrap().len(),
        motor_macvm::BUILD_DISK_SIZE
    );
    assert!(!job_disk(&entry).exists());
    finish(&runtime, &root, &entry);
    assert_eq!(build_disk_marker(&kept(&root), 3), b"new");
}
#[test]
fn first_job_formats_a_blank_disk_and_its_publication_creates_the_kept_disk() {
    let scratch = ScratchDir::new("macos-build-disk-first");
    let root = scratch.join("state");
    let config = Config::new(&root, "macos");
    let runtime = Runtime::default();
    let entry = job(&root, &runtime);
    build_disk_in(&config, &entry).unwrap();
    // The blank disk stays: all zero, so the guest formats it.
    assert_eq!(build_disk_marker(&job_disk(&entry), 4096), vec![0; 4096]);
    assert!(!kept(&root).exists());
    runtime.stop(&root, &entry).unwrap();
    target_out(&config, &entry).unwrap();
    assert_eq!(
        std::fs::metadata(kept(&root)).unwrap().len(),
        motor_macvm::BUILD_DISK_SIZE
    );
    finish(&runtime, &root, &entry);
}
#[test]
fn old_build_folder_is_deleted_only_after_the_first_build_disk_is_published() {
    let scratch = ScratchDir::new("macos-build-disk-migration");
    let root = scratch.join("state");
    let config = Config::new(&root, "macos");
    let runtime = Runtime::default();
    let legacy = root
        .join("targets")
        .join(lifecycle::key("/Users/test/repo"));
    std::fs::create_dir_all(legacy.join("debug")).unwrap();
    std::fs::write(legacy.join("debug/app"), "old build").unwrap();
    // A job that never publishes leaves the old folder alone.
    let failed = job(&root, &runtime);
    build_disk_in(&config, &failed).unwrap();
    failed.cancel.cancel();
    runtime.stop(&root, &failed).unwrap();
    assert!(target_out(&config, &failed).is_err());
    finish(&runtime, &root, &failed);
    assert!(legacy.join("debug/app").exists());
    assert!(!kept(&root).exists());
    let entry = job(&root, &runtime);
    build_disk_in(&config, &entry).unwrap();
    // The old folder is never cloned into the disk: the first job builds cold.
    assert_eq!(build_disk_marker(&job_disk(&entry), 4096), vec![0; 4096]);
    runtime.stop(&root, &entry).unwrap();
    target_out(&config, &entry).unwrap();
    assert!(kept(&root).exists());
    assert!(!legacy.exists());
    finish(&runtime, &root, &entry);
}
#[test]
fn nothing_is_published_without_terminal_status_stop_proof_or_with_cancel() {
    assert!(publishable(false, true, true));
    for (cancelled, stop_proven, terminal) in [
        (true, true, true),
        (false, false, true),
        (false, true, false),
        (true, false, false),
    ] {
        assert!(!publishable(cancelled, stop_proven, terminal));
    }
    let scratch = ScratchDir::new("macos-build-disk-cancel");
    let root = scratch.join("state");
    let config = Config::new(&root, "macos");
    let runtime = Runtime::default();
    write_build_disk(&kept(&root), b"old");
    let entry = job(&root, &runtime);
    build_disk_in(&config, &entry).unwrap();
    write_build_disk_marker(&job_disk(&entry), b"new");
    runtime.stop(&root, &entry).unwrap();
    entry.cancel.cancel();
    assert!(target_out(&config, &entry).is_err());
    assert_eq!(build_disk_marker(&kept(&root), 3), b"old");
    finish(&runtime, &root, &entry);
    assert_eq!(build_disk_marker(&kept(&root), 3), b"old");
}
#[test]
fn kept_disk_of_another_size_is_not_cloned_and_a_resized_job_disk_is_not_published() {
    let scratch = ScratchDir::new("macos-build-disk-size");
    let root = scratch.join("state");
    let config = Config::new(&root, "macos");
    let runtime = Runtime::default();
    std::fs::create_dir_all(root.join("targets")).unwrap();
    std::fs::write(kept(&root), "short").unwrap();
    let entry = job(&root, &runtime);
    build_disk_in(&config, &entry).unwrap();
    assert_eq!(build_disk_marker(&job_disk(&entry), 5), vec![0; 5]);
    std::fs::OpenOptions::new()
        .write(true)
        .open(job_disk(&entry))
        .unwrap()
        .set_len(4096)
        .unwrap();
    runtime.stop(&root, &entry).unwrap();
    assert!(target_out(&config, &entry).is_err());
    assert_eq!(std::fs::read(kept(&root)).unwrap(), b"short");
    finish(&runtime, &root, &entry);
}
#[test]
fn staged_target_becomes_an_empty_directory_without_following_a_link() {
    use std::os::unix::fs::symlink;
    let scratch = ScratchDir::new("macos-target-mount-point");
    let root = scratch.join("state");
    let staged = staged(&root, Uuid::now_v7());
    let sentinel = scratch.join("sentinel");
    std::fs::create_dir(&sentinel).unwrap();
    std::fs::write(sentinel.join("whole"), "unchanged").unwrap();
    symlink(&sentinel, staged.workspace.join("target")).unwrap();
    target_in(&staged).unwrap();
    let target = staged.workspace.join("target");
    assert!(std::fs::symlink_metadata(&target).unwrap().is_dir());
    assert_eq!(std::fs::read_dir(&target).unwrap().count(), 0);
    assert_eq!(
        std::fs::read_to_string(sentinel.join("whole")).unwrap(),
        "unchanged"
    );
    std::fs::write(target.join("stale"), "from the checkout").unwrap();
    target_in(&staged).unwrap();
    assert_eq!(std::fs::read_dir(&target).unwrap().count(), 0);
}
/// A chunked-return request in `/Users/test/repo` for these paths and cap.
fn request(paths: &[&str], cap: u64) -> motor_protocol::Run {
    motor_protocol::Run {
        return_paths: paths.iter().map(|path| path.to_string()).collect(),
        return_max_bytes: cap,
        return_chunked: true,
        gate_job: false,
        network: None,
        platform: Some(motor_protocol::Platform::Macos),
        workspace_kind: None,
        job_id: Uuid::now_v7(),
        workspace_key: "/Users/test/repo".into(),
        repository_root: "/Users/test/repo".into(),
        cwd: "/Users/test/repo".into(),
        command: "true".into(),
        env: Default::default(),
        weight_hint: None,
        timeout: None,
        queue_wait_limit_s: 300,
        snapshot_commit: "a".repeat(40),
        tree_hash: String::new(),
        integration_tip: "a".repeat(40),
        siblings: vec![],
        bundles: vec![],
        git_dependencies: vec![],
        base_commit: None,
    }
}
#[test]
fn only_return_paths_inside_target_reach_the_guest_relative_to_target() {
    let mut request = request(
        &[
            "/Users/test/repo/target/debug/app",
            "/Users/test/repo/Target/doc/index.html",
            "/Users/test/repo/src/main.rs",
            "/Users/test/repo/target",
            "/Users/test/repo/target/../src/lib.rs",
            "/Users/test/repo/target/./release/app",
            "/Users/test/repo/targets/x",
        ],
        1234,
    );
    assert_eq!(
        build_config(&request).unwrap(),
        motor_macvm::BuildConfig {
            returns: vec![
                "debug/app".into(),
                "doc/index.html".into(),
                "release/app".into()
            ],
            return_max_bytes: 1234,
        }
    );
    request.return_paths.clear();
    assert!(build_config(&request).unwrap().returns.is_empty());
}

/// The host admits returned files in request order across every path, while
/// the guest budgets only those under `target/`. With a cap of 100, a 50-byte
/// file outside `target/`, then target files of 60 and 45 bytes: the guest
/// copies the 60 and leaves a placeholder for the 45, while the host refuses
/// the 60 and has 50 left. The placeholder must still be refused, never
/// returned as 45 zero bytes.
#[test]
fn guest_placeholder_is_never_returned_as_content() {
    use motor_macagent::build_volume::{place_returns, stage_returns};
    use motor_protocol::{ReturnManifestOutcome, ReturnPathsFailureCause};
    let scratch = ScratchDir::new("macos-placeholder");
    let staged = staged(&scratch.join("state"), Uuid::now_v7());
    let volume = scratch.join("volume");
    std::fs::create_dir_all(volume.join("debug")).unwrap();
    std::fs::write(volume.join("debug/large"), vec![6; 60]).unwrap();
    std::fs::write(volume.join("debug/small"), vec![4; 45]).unwrap();
    std::fs::create_dir_all(staged.workspace.join("src")).unwrap();
    std::fs::write(staged.workspace.join("src/outside"), vec![5; 50]).unwrap();
    target_in(&staged).unwrap();
    let request = request(
        &[
            "/Users/test/repo/src/outside",
            "/Users/test/repo/target/debug/large",
            "/Users/test/repo/target/debug/small",
        ],
        100,
    );
    let copies = scratch.join("copies");
    std::fs::create_dir(&copies).unwrap();
    let placed = stage_returns(
        &std::fs::File::open(&volume).unwrap(),
        &build_config(&request).unwrap(),
        &copies,
    )
    .unwrap();
    place_returns(
        &std::fs::File::open(staged.workspace.join("target")).unwrap(),
        &placed,
        &copies,
    )
    .unwrap();
    let job = scratch.join("job");
    std::fs::create_dir(&job).unwrap();
    let crate::return_paths::Collection::Chunked(collection) =
        crate::return_paths::collect_staged(&request, &staged.workspace, &job)
    else {
        panic!("chunked request collected without chunks");
    };
    assert!(matches!(
        collection.files[0].outcome(),
        ReturnManifestOutcome::Returned { size: 50, .. }
    ));
    for file in &collection.files[1..] {
        assert!(
            matches!(
                file.outcome(),
                ReturnManifestOutcome::Failed {
                    cause: ReturnPathsFailureCause::OverCap,
                    ..
                }
            ),
            "{:?}",
            file.outcome()
        );
    }
    assert_eq!(collection.total_bytes, 50);
}

/// A cancel that arrives while the build disk is being renamed into place
/// waits for the rename, and the job is then recorded as published; one that
/// arrives before it publishes nothing (see the cancel test above).
#[test]
fn a_cancel_during_publication_waits_for_it_and_finds_the_job_published() {
    let scratch = ScratchDir::new("macos-publish-cancel");
    let root = scratch.join("state");
    let runtime = Arc::new(Runtime::default());
    let entry = job(&root, &runtime);
    let id = entry.owned.lock().unwrap().record.id;
    runtime.stop(&root, &entry).unwrap();
    let cancelled = Arc::new(AtomicBool::new(false));
    let cancelled_during_commit = Arc::new(AtomicBool::new(false));
    let canceller = Arc::new(Mutex::new(None));
    let mut config = Config::new(&root, "macos");
    {
        let (runtime, cancelled, during, canceller) = (
            runtime.clone(),
            cancelled.clone(),
            cancelled_during_commit.clone(),
            canceller.clone(),
        );
        config.long_work = Some(Arc::new(move |phase| {
            if phase != "macos-target-publishing" {
                return;
            }
            let (runtime, flag) = (runtime.clone(), cancelled.clone());
            *canceller.lock().unwrap() = Some(std::thread::spawn(move || {
                runtime.cancel(id);
                flag.store(true, Ordering::Release);
            }));
            std::thread::sleep(Duration::from_millis(300));
            during.store(cancelled.load(Ordering::Acquire), Ordering::Release);
        }));
    }
    target_out(&config, &entry).unwrap();
    canceller.lock().unwrap().take().unwrap().join().unwrap();
    assert!(
        !cancelled_during_commit.load(Ordering::Acquire),
        "a cancel must not complete inside the publication commit"
    );
    assert!(cancelled.load(Ordering::Acquire));
    assert!(entry.published());
    assert!(runtime.cancelled(id));
    assert!(kept(&root).exists());
    finish(&runtime, &root, &entry);
}
