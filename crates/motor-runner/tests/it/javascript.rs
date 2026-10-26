//! The operator lane exercises tracked nested installs through the public job protocol.
#![cfg(target_os = "linux")]
use super::connect::SocketScratch;
use cortexkit_test_support::ScratchDir;
use motor_protocol::*;
use motor_runner::{Config, Runner};
use sha2::{Digest, Sha256};
use std::{collections::BTreeMap, fs, path::Path, process::Command, time::Duration};
use tokio::net::{UnixListener, UnixStream};

fn javascript_socket_path(scratch: &SocketScratch) -> std::path::PathBuf {
    scratch.path().join("runner.sock")
}

#[tokio::test]
async fn javascript_socket_path_binds_under_owned_short_scratch_alias() {
    let scratch = SocketScratch::new();
    let _listener = UnixListener::bind(javascript_socket_path(&scratch)).unwrap();
}

pub(super) fn git(path: &Path, args: &[&str]) -> String {
    let output = Command::new("git")
        .arg("-C")
        .arg(path)
        .args(args)
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    String::from_utf8(output.stdout).unwrap().trim().into()
}

pub(super) fn snapshot_bundle(source: &Path, path: &Path) -> Vec<u8> {
    // The importer requires a client snapshot ref, not a bundle's HEAD alias.
    let reference = format!("refs/ck-motor/{}/snapshot", Uuid::now_v7());
    git(source, &["update-ref", &reference, "HEAD"]);
    git(
        source,
        &["bundle", "create", path.to_str().unwrap(), &reference],
    );
    git(source, &["update-ref", "-d", &reference]);
    fs::read(path).unwrap()
}

pub(super) fn snapshot_run(
    workspace: &Path,
    source: &Path,
    commit: &str,
    tree: &str,
    command: &str,
    bundle: &[u8],
) -> Run {
    Run {
        return_paths: Vec::new(),
        return_max_bytes: motor_protocol::RETURN_MAX_BYTES,
        return_chunked: false,
        gate_job: false,
        network: None,
        platform: None,
        job_id: Uuid::now_v7(),
        workspace_key: workspace.to_string_lossy().into_owned(),
        repository_root: source.to_string_lossy().into_owned(),
        cwd: workspace.to_string_lossy().into_owned(),
        command: command.into(),
        env: BTreeMap::new(),
        weight_hint: None,
        timeout: Some(60),
        queue_wait_limit_s: 60,
        snapshot_commit: commit.into(),
        tree_hash: tree.into(),
        integration_tip: commit.into(),
        siblings: Vec::new(),
        bundles: vec![BundleManifest {
            index: 0,
            repository: source.to_string_lossy().into_owned(),
            length: bundle.len() as u64,
            sha256: format!("{:x}", Sha256::digest(bundle)),
        }],
        git_dependencies: Vec::new(),
        base_commit: None,
        workspace_kind: None,
    }
}

// Heartbeats can arrive between handshake replies as well as job output.
async fn read_non_heartbeat(stream: &mut UnixStream) -> std::io::Result<Option<Frame>> {
    loop {
        match read_frame_async(stream).await? {
            Some(Frame::Heartbeat { .. }) => {}
            frame => return Ok(frame),
        }
    }
}

#[tokio::test]
async fn javascript_frame_reader_skips_heartbeats_without_swallowing_other_frames() {
    let (mut writer, mut reader) = UnixStream::pair().unwrap();
    let frames = [
        Frame::Hello {
            return_paths: false,
            return_chunked: false,
            sibling_paths: false,
            protocol_version: PROTOCOL_VERSION,
            started: true,
            default_branch_tip: false,
            gate_jobs: false,
            platforms: Vec::new(),
        },
        Frame::Accepted(Accepted::new(Uuid::now_v7(), 0)),
        Frame::Status,
    ];
    for frame in &frames {
        write_frame_async(
            &mut writer,
            &Frame::Heartbeat {
                queue_wait_ms: None,
            },
        )
        .await
        .unwrap();
        write_frame_async(
            &mut writer,
            &Frame::Heartbeat {
                queue_wait_ms: None,
            },
        )
        .await
        .unwrap();
        write_frame_async(&mut writer, frame).await.unwrap();
    }
    write_frame_async(
        &mut writer,
        &Frame::Heartbeat {
            queue_wait_ms: None,
        },
    )
    .await
    .unwrap();
    drop(writer);
    for frame in frames {
        assert_eq!(read_non_heartbeat(&mut reader).await.unwrap(), Some(frame));
    }
    assert_eq!(read_non_heartbeat(&mut reader).await.unwrap(), None);
}

async fn submit(socket: &Path, run: Run, bundle: &[u8]) -> TerminalRecord {
    submit_with_output(socket, run, bundle).await.0
}

pub(super) async fn submit_with_output(
    socket: &Path,
    run: Run,
    bundle: &[u8],
) -> (TerminalRecord, String) {
    submit_with_capability(socket, run, bundle, false).await
}

pub(super) async fn submit_with_capability(
    socket: &Path,
    run: Run,
    bundle: &[u8],
    default_branch_tip: bool,
) -> (TerminalRecord, String) {
    let mut stream = UnixStream::connect(socket).await.unwrap();
    write_frame_async(
        &mut stream,
        &Frame::Hello {
            return_paths: false,
            return_chunked: false,
            sibling_paths: false,
            protocol_version: PROTOCOL_VERSION,
            started: true,
            default_branch_tip,
            gate_jobs: false,
            platforms: Vec::new(),
        },
    )
    .await
    .unwrap();
    assert!(matches!(
        read_non_heartbeat(&mut stream).await.unwrap(),
        Some(Frame::Hello { .. })
    ));
    let job_id = run.job_id;
    write_frame_async(&mut stream, &Frame::Run(run))
        .await
        .unwrap();
    assert!(matches!(
        read_non_heartbeat(&mut stream).await.unwrap(),
        Some(Frame::Accepted(_))
    ));
    write_frame_async(
        &mut stream,
        &Frame::BundleChunk {
            index: 0,
            offset: 0,
            data: BytePayload(bundle.to_vec()),
        },
    )
    .await
    .unwrap();
    tokio::time::timeout(Duration::from_secs(120), async {
        let mut started = false;
        let mut output_bytes = Vec::new();
        loop {
            match read_non_heartbeat(&mut stream)
                .await
                .unwrap()
                .expect("terminal before EOF")
            {
                Frame::Started {
                    job_id: id, seq, ..
                } => {
                    assert_eq!(id, job_id);
                    assert_eq!(seq, 0);
                    assert!(!started, "one Started per job");
                    started = true;
                }
                Frame::Terminal(record) => {
                    assert_eq!(started, record.ran == Some(Ran::Remote));
                    break (record, String::from_utf8_lossy(&output_bytes).into_owned());
                }
                Frame::Output(output) => {
                    assert!(started, "Started before Output");
                    assert!(output.seq > 0);
                    output_bytes.extend(output.bytes.0);
                }
                frame => panic!("unexpected job frame: {frame:?}"),
            }
        }
    })
    .await
    .expect("nested JavaScript job completed")
}

fn fixture_runtimes_reaped(state: &Path, jobs: &[Uuid]) -> bool {
    jobs.iter().all(|id| {
        let path = state.join("sandboxes").join(id.to_string());
        match fs::symlink_metadata(&path) {
            Ok(_) => false,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => true,
            Err(error) => panic!("inspect {} during drain: {error}", path.display()),
        }
    })
}

#[test]
fn javascript_fixture_requires_both_runtime_directories_to_be_reaped() {
    let scratch = ScratchDir::new("js-runtime-reap");
    let jobs = [Uuid::now_v7(), Uuid::now_v7()];
    for id in jobs {
        fs::create_dir_all(scratch.join("sandboxes").join(id.to_string())).unwrap();
    }
    assert!(!fixture_runtimes_reaped(scratch.path(), &jobs));
    fs::remove_dir_all(scratch.join("sandboxes").join(jobs[0].to_string())).unwrap();
    assert!(!fixture_runtimes_reaped(scratch.path(), &jobs));
    fs::remove_dir_all(scratch.join("sandboxes").join(jobs[1].to_string())).unwrap();
    assert!(fixture_runtimes_reaped(scratch.path(), &jobs));
}

#[path = "cleanup.rs"]
pub(super) mod cleanup;

pub(super) async fn drain_fixture_runner(
    runner: Runner,
    server: tokio::task::JoinHandle<std::io::Result<()>>,
    jobs: &[Uuid],
    cleanup: Option<&mut cleanup::OperatorScratch>,
) -> fs::File {
    let state = runner.config().state_dir();
    runner.begin_drain(Duration::from_secs(30), true);
    tokio::time::timeout(Duration::from_secs(30), async {
        loop {
            let status = runner.status();
            let persisted = jobs
                .iter()
                .all(|id| runner.job_status(*id).unwrap().terminal.is_some());
            // A terminal is persisted before the monitor drops its launch handle.
            // RuntimeDir teardown must finish before walking the scratch tree.
            let reaped = fixture_runtimes_reaped(&state, jobs);
            if status.queue_depth == 0
                && status.running_jobs.is_empty()
                && persisted
                && reaped
                && runner.drain_result() == Some(false)
            {
                break;
            }
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
        server
            .await
            .expect("serve task joins after drain")
            .expect("serve exits successfully after drain");
    })
    .await
    .expect("runner drains and reaps fixture jobs before cleanup");
    assert_eq!(
        runner.drain_result(),
        Some(false),
        "fixture drain must not time out"
    );
    if let Some(cleanup) = cleanup {
        cleanup.release_runner();
    }
    drop(runner);
    // The dispatch thread uses a weak Core reference. Acquiring the instance
    // lock proves its last active tick and all remaining Core owners are gone.
    let lock = fs::OpenOptions::new()
        .read(true)
        .write(true)
        .open(state.join("daemon.lock"))
        .unwrap();
    tokio::time::timeout(Duration::from_secs(30), async {
        loop {
            match fs2::FileExt::try_lock_exclusive(&lock) {
                Ok(()) => break,
                Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                    tokio::time::sleep(Duration::from_millis(25)).await;
                }
                Err(error) => panic!("acquire drained fixture instance lock: {error}"),
            }
        }
    })
    .await
    .expect("runner releases its instance lock before cleanup");
    lock
}

pub(super) fn checked_restore_command(command: &mut Command) {
    let output = command
        .output()
        .unwrap_or_else(|error| panic!("{command:?}: {error}"));
    assert!(
        output.status.success(),
        "{command:?} exited with {}\nstdout: {}\nstderr: {}",
        output.status,
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
}

#[tokio::test]
async fn javascript_snapshot_bundle_imports_and_reuses_for_second_job() {
    let scratch = ScratchDir::new("js-bundle-protocol");
    let source = scratch.join("source");
    fs::create_dir_all(&source).unwrap();
    fs::write(source.join("source.txt"), "snapshot fixture").unwrap();
    git(&source, &["init", "-q"]);
    git(&source, &["add", "."]);
    git(
        &source,
        &[
            "-c",
            "user.name=Test",
            "-c",
            "user.email=test@example.invalid",
            "commit",
            "-qm",
            "fixture",
        ],
    );
    let commit = git(&source, &["rev-parse", "HEAD"]);
    let tree = git(&source, &["rev-parse", "HEAD^{tree}"]);
    let path = scratch.join("snapshot.bundle");
    let bundle = snapshot_bundle(&source, &path);
    assert_eq!(
        git(
            &source,
            &["for-each-ref", "--format=%(refname)", "refs/ck-motor/"]
        ),
        "",
        "temporary sender ref must be deleted"
    );
    let workspaces = scratch.join("workspaces");
    let workspace = workspaces.join("job");
    let mut config = Config::new(scratch.join("state"), "bundle");
    config.workspace_roots = vec![workspaces];
    config.owned_workspace_roots = vec![scratch.path().to_path_buf()];
    config.free_space_floor_bytes = 0;
    let socket_scratch = SocketScratch::new();
    let socket = javascript_socket_path(&socket_scratch);
    let runner = Runner::open(config).unwrap();
    let server = tokio::spawn(motor_runner::serve(
        runner.clone(),
        UnixListener::bind(&socket).unwrap(),
    ));
    let mut run = snapshot_run(
        &workspace,
        &source,
        &commit,
        &tree,
        "test -f source.txt",
        &bundle,
    );
    let first = submit(&socket, run.clone(), &bundle).await;
    assert_eq!(first.outcome, Outcome::Exit { code: 0 });
    assert_eq!(first.ran, Some(Ran::Remote));
    run.job_id = Uuid::now_v7();
    let second = submit(&socket, run, &bundle).await;
    assert_eq!(second.outcome, Outcome::Exit { code: 0 });
    assert_eq!(second.ran, Some(Ran::Remote));
    let _instance_lock =
        drain_fixture_runner(runner, server, &[first.job_id, second.job_id], None).await;
}

fn javascript_cache_keys(state: &Path) -> std::collections::BTreeSet<std::ffi::OsString> {
    fs::read_dir(state.join("js-installs"))
        .unwrap()
        .map(|entry| entry.unwrap().file_name())
        .collect()
}

#[tokio::test]
#[ignore = "requires the trusted build-server isolation gate; see motor-runner README"]
async fn isolated_nested_javascript_jobs_resolve_offline_and_reuse_trusted_cache() {
    let mut scratch = cleanup::OperatorScratch::new("nested-js-job");
    let source = scratch.join("source");
    fs::create_dir_all(&source).unwrap();
    for (manager, lockfile) in [
        ("npm", "package-lock.json"),
        ("bun", "bun.lock"),
        ("pnpm", "pnpm-lock.yaml"),
    ] {
        let fixture = Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("test-fixtures/javascript")
            .join(manager);
        for name in [
            "package.json",
            lockfile,
            "vendor/fixture-dep/package.json",
            "vendor/fixture-dep/index.js",
            "vendor/fixture-dep/cli.js",
        ] {
            let destination = source.join(manager).join(name);
            fs::create_dir_all(destination.parent().unwrap()).unwrap();
            fs::copy(fixture.join(name), destination).unwrap();
        }
        if manager == "pnpm" {
            fs::copy(
                fixture.join("pnpm-workspace.yaml"),
                source.join(manager).join("pnpm-workspace.yaml"),
            )
            .unwrap();
        }
    }
    git(&source, &["init", "-q"]);
    git(&source, &["add", "."]);
    git(
        &source,
        &[
            "-c",
            "user.name=Test",
            "-c",
            "user.email=test@example.invalid",
            "commit",
            "-qm",
            "nested fixtures",
        ],
    );
    let commit = git(&source, &["rev-parse", "HEAD"]);
    let tree = git(&source, &["rev-parse", "HEAD^{tree}"]);
    let bundle_path = scratch.join("snapshot.bundle");
    let bundle = snapshot_bundle(&source, &bundle_path);
    let workspaces = scratch.join("workspaces");
    let workspace = workspaces.join("job");
    let mut config = Config::new(scratch.join("state"), "nested-js");
    config.workspace_roots = vec![workspaces];
    config.owned_workspace_roots = vec![scratch.path().to_path_buf()];
    config.free_space_floor_bytes = 0;
    config.isolate_jobs = true;
    let state = config.state_dir();
    // The owned short alias avoids Unix socket path limits without changing
    // the repository, checkout, or runner state paths.
    let socket_scratch = SocketScratch::new();
    let socket = javascript_socket_path(&socket_scratch);
    let runner = Runner::open(config).unwrap();
    let listener = UnixListener::bind(&socket).unwrap();
    let server = tokio::spawn(motor_runner::serve(runner.clone(), listener));
    scratch.attach(&runner, server.abort_handle());
    let run = snapshot_run(
        &workspace, &source, &commit, &tree,
        "set -e; for manager in npm bun pnpm; do (cd \"$manager\"; node -e \"if (require('fixture-dep') !== 'fixture') process.exit(1)\"; test \"$(node_modules/.bin/fixture-tool)\" = fixture; test ! -e lifecycle-ran); done; rm -rf npm/node_modules",
        &bundle,
    );
    let first = submit(&socket, run.clone(), &bundle).await;
    assert_eq!(first.outcome, Outcome::Exit { code: 0 });
    assert_eq!(first.ran, Some(Ran::Remote));
    let cache_keys = javascript_cache_keys(&state);
    assert_eq!(cache_keys.len(), 3);
    let mut second = run;
    second.job_id = Uuid::now_v7();
    let terminal = submit(&socket, second, &bundle).await;
    assert_eq!(terminal.outcome, Outcome::Exit { code: 0 });
    assert_eq!(terminal.ran, Some(Ran::Remote));
    assert_eq!(javascript_cache_keys(&state), cache_keys);
    let _instance_lock = drain_fixture_runner(
        runner,
        server,
        &[first.job_id, terminal.job_id],
        Some(&mut scratch),
    )
    .await;
    // Isolation transfers checkout ownership to the job account; restore physical
    // ownership and owner access without following links outside the fixture.
    checked_restore_command(
        Command::new("sudo")
            .args(["-n", "chown", "-R", "-P", "--"])
            .arg(format!("{}:{}", unsafe { libc::geteuid() }, unsafe {
                libc::getegid()
            }))
            .arg(scratch.path()),
    );
    checked_restore_command(
        Command::new("sudo")
            .args(["-n", "chmod", "-R", "-P", "u+rwX", "--"])
            .arg(scratch.path()),
    );
}
