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
                tag: "sibling-alias-0".into(),
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

#[test]
fn source_unseal_barrier_prevents_target_cache_publication() {
    let scratch = ScratchDir::new("macos-target-seal-barrier");
    let root = scratch.join("state");
    let config = Config::new(&root, "macos");
    let runtime = Runtime::default();
    let entry = runtime
        .begin(&root, Uuid::now_v7(), "/Users/test/repo")
        .unwrap();
    let id = entry.owned.lock().unwrap().record.id;
    let staged = staged(&root, id);
    let store = root
        .join("targets")
        .join(lifecycle::key("/Users/test/repo"));
    std::fs::create_dir(&store).unwrap();
    std::fs::write(store.join("whole"), "old").unwrap();
    std::fs::create_dir(staged.workspace.join("target")).unwrap();
    std::fs::write(staged.workspace.join("target/new"), "must not publish").unwrap();
    assert!(
        target_out(&config, &entry, &staged.workspace).is_err(),
        "stop proof is required even without an intent"
    );
    runtime.stop(&root, &entry).unwrap();
    let journal = super::super::source_seal::path(&root, id);
    crate::storage::atomic_json_private(&journal, &serde_json::json!({
        "version": 1, "entries": [{"identity": {"device": 0, "inode": 0, "birth_seconds": 0, "birth_nanos": 0}, "original_flags": 0}],
        "complete": true, "cleared": false,
    })).unwrap();
    assert!(target_out(&config, &entry, &staged.workspace).is_err());
    assert_eq!(std::fs::read_to_string(store.join("whole")).unwrap(), "old");
    assert!(!store.join("new").exists());
    assert!(!entry.owned.lock().unwrap().record.paths.target_tmp.exists());
    let saved: serde_json::Value = crate::storage::read_json(&journal).unwrap();
    assert_eq!(saved["cleared"], false);
    // The unresolved identity remains an obligation for lifecycle maintenance.
    runtime.finish(&root, &entry);
    assert!(runtime.entry(id).is_some());
    assert!(journal.exists());
}
#[test]
fn target_store_is_cloned_and_atomically_replaced_with_recorded_old_store() {
    let scratch = ScratchDir::new("macos-target-swap");
    let root = scratch.join("state");
    let config = Config::new(&root, "macos");
    let runtime = Runtime::default();
    let entry = runtime
        .begin(&root, Uuid::now_v7(), "/Users/test/repo")
        .unwrap();
    runtime.workspace(&root, &entry).unwrap();
    let staged = staged(&root, entry.owned.lock().unwrap().record.id);
    let store = root
        .join("targets")
        .join(lifecycle::key("/Users/test/repo"));
    std::fs::create_dir(&store).unwrap();
    std::fs::write(store.join("whole"), "old").unwrap();
    target_in(&config, &entry, &staged).unwrap();
    assert_eq!(
        std::fs::read_to_string(staged.workspace.join("target/whole")).unwrap(),
        "old"
    );
    std::fs::write(staged.workspace.join("target/whole"), "new").unwrap();
    runtime.stop(&root, &entry).unwrap();
    target_out(&config, &entry, &staged.workspace).unwrap();
    assert_eq!(std::fs::read_to_string(store.join("whole")).unwrap(), "new");
    let temporary = entry.owned.lock().unwrap().record.paths.target_tmp.clone();
    assert_eq!(
        std::fs::read_to_string(temporary.join("whole")).unwrap(),
        "old"
    );
    // A restart may delete the recorded old store, but never the published one.
    entry.owned.lock().unwrap().record.stop_proven = true;
    runtime.finish(&root, &entry);
    assert!(!temporary.exists());
    assert_eq!(std::fs::read_to_string(store.join("whole")).unwrap(), "new");
}
#[test]
fn top_level_target_symlink_never_reaches_outside_sentinel() {
    use std::os::unix::fs::symlink;
    let scratch = ScratchDir::new("macos-target-poison");
    let root = scratch.join("state");
    let config = Config::new(&root, "macos");
    let runtime = Runtime::default();
    let entry = runtime
        .begin(&root, Uuid::now_v7(), "/Users/test/repo")
        .unwrap();
    let staged = staged(&root, entry.owned.lock().unwrap().record.id);
    let sentinel = scratch.join("sentinel");
    std::fs::create_dir(&sentinel).unwrap();
    std::fs::write(sentinel.join("whole"), "unchanged").unwrap();
    symlink(&sentinel, staged.workspace.join("target")).unwrap();
    runtime.stop(&root, &entry).unwrap();
    assert!(target_out(&config, &entry, &staged.workspace).is_err());
    target_in(&config, &entry, &staged).unwrap();
    assert!(!staged.workspace.join("target").exists());
    entry.owned.lock().unwrap().record.stop_proven = true;
    runtime.finish(&root, &entry);
    assert_eq!(
        std::fs::read_to_string(sentinel.join("whole")).unwrap(),
        "unchanged"
    );
}
