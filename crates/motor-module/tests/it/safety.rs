use super::recovery::{collect, terminal, Cut, Fixture};
use crate::diagnostics::{count as unreachable_count, UnreachableStep};
use crate::*;
use cortexkit_test_support::ScratchDir;
use motor_protocol::*;
use std::{
    fs,
    future::Future,
    io,
    path::Path,
    pin::Pin,
    sync::{atomic::Ordering, Arc, Mutex},
    time::Duration,
};
fn git(path: &Path, args: &[&str]) -> String {
    crate::snapshot::git(path, args).unwrap()
}
fn state(path: &Path) -> (String, String, std::time::SystemTime, String, Vec<u8>) {
    let index = git(
        path,
        &["rev-parse", "--path-format=absolute", "--git-path", "index"],
    );
    (
        git(path, &["rev-parse", "HEAD"]),
        git(path, &["status", "--porcelain"]),
        fs::metadata(&index).unwrap().modified().unwrap(),
        git(path, &["for-each-ref", "--format=%(refname) %(objectname)"]),
        fs::read(index).unwrap(),
    )
}
#[tokio::test]
async fn snapshots_preserve_head_real_index_status_and_refs_on_success_and_failure() {
    let mut f = Fixture::new(Cut::None, 28).await;
    fs::write(f.key.join("source"), "staged\n").unwrap();
    git(&f.key, &["add", "source"]);
    fs::write(f.key.join("source"), "unstaged\n").unwrap();
    fs::remove_file(f.key.join(".gitignore")).unwrap();
    fs::write(f.key.join("binary"), [0, 255, 37, 0, 128]).unwrap();
    let before = state(&f.key);
    let sibling_before = state(&f.sibling);
    let motor = f.motor();
    let records = collect(
        motor
            .run(f.request("cat source; test ! -f .gitignore; test -f binary"))
            .unwrap(),
    )
    .await;
    assert_eq!(terminal(&records).outcome, Outcome::Exit { code: 0 });
    assert_eq!(before, state(&f.key));
    assert_eq!(sibling_before, state(&f.sibling));
    assert_eq!(
        fs::read_to_string(f.remote_key.join("source")).unwrap(),
        "unstaged\n"
    );
    assert_eq!(
        fs::read(f.remote_key.join("binary")).unwrap(),
        [0, 255, 37, 0, 128]
    );
    assert!(git(&f.key, &["for-each-ref", "refs/ck-motor/"]).is_empty());
    // Injecting commit-tree failure happens after hashing, and must not mutate
    // the real index even though it already holds staged content.
    f.config.inject_snapshot_failure = true;
    let failed = Motor::with_connector(
        f.config.clone(),
        &f.scratch.join("failure-state"),
        Arc::new(f.proxy.clone()),
    )
    .unwrap();
    let records = collect(failed.run(f.request("echo forbidden >> marker")).unwrap()).await;
    assert_eq!(
        terminal(&records).outcome,
        Outcome::RefusedBeforeStart {
            reason: RefusalReason::SnapshotFailed
        }
    );
    assert!(terminal(&records).tree_hash.is_some());
    let detail = terminal(&records).refusal_detail().unwrap();
    assert!(
        detail.starts_with("snapshot capture failed: fatal:"),
        "{detail}"
    );
    assert!(detail.contains("invalid-tree"), "{detail}");
    assert!(!detail.contains('\n'), "{detail}");
    assert!(detail.len() <= REFUSAL_DETAIL_MAX_BYTES);
    let prepare = serde_json::from_value(serde_json::json!({
        "workspace_key": f.key, "repository_root": f.main, "base_commit": "HEAD", "siblings": []
    }))
    .unwrap();
    let reply = failed.prepare(prepare).await.unwrap();
    assert_eq!(reply.refusal_detail(), Some(detail));
    assert_eq!(before, state(&f.key));
    assert_eq!(sibling_before, state(&f.sibling));
    assert!(!f.remote_key.join("marker").exists());
}

#[tokio::test]
async fn failed_git_add_reports_first_stderr_line_with_request_secrets_redacted() {
    let f = Fixture::new(Cut::None, 28).await;
    let attributes = git(
        &f.key,
        &[
            "rev-parse",
            "--path-format=absolute",
            "--git-path",
            "info/attributes",
        ],
    );
    fs::write(attributes, "source filter=snapshot-error\n").unwrap();
    git(
        &f.key,
        &[
            "config",
            "filter.snapshot-error.clean",
            "printf 'clean failed secret-stderr-value\\nsecond stderr line\\n' >&2; exit 1",
        ],
    );
    git(
        &f.key,
        &["config", "filter.snapshot-error.required", "true"],
    );
    fs::write(f.key.join("source"), "changed input").unwrap();
    let mut request = f.request("touch marker");
    request
        .env
        .insert("RUSTFLAGS".into(), "secret-stderr-value".into());
    let records = collect(f.motor().run(request).unwrap()).await;
    let record = terminal(&records);
    assert_eq!(
        record.outcome,
        Outcome::RefusedBeforeStart {
            reason: RefusalReason::SnapshotFailed
        }
    );
    assert_eq!(
        record.refusal_detail(),
        Some("snapshot capture failed: clean failed client environment")
    );
    assert!(!f.remote_key.join("marker").exists());
}
#[tokio::test]
async fn failed_bundle_deletes_temporary_refs_and_ignored_files_never_transfer() {
    let f = Fixture::new(Cut::None, 28).await;
    fs::write(f.key.join("ignored"), "not transferable").unwrap();
    let before = state(&f.key);
    let id = Uuid::now_v7();
    let s = crate::snapshot::Snapshots::build(
        &f.config,
        f.key.to_str().unwrap(),
        f.main.to_str().unwrap(),
        &[],
        &f.scratch.join("spool"),
        id,
        None,
        &crate::storage::Storage::open(&f.scratch.join("capture-store")).unwrap(),
    )
    .ok()
    .unwrap();
    fs::create_dir(s.spool.0.join("bundle-0")).unwrap();
    assert!(s.bundle(id, &[]).is_err());
    assert_eq!(before, state(&f.key));
    assert!(git(&f.key, &["for-each-ref", "refs/ck-motor/"]).is_empty());
    drop(s);
    let records = collect(
        f.motor()
            .run(f.request("test ! -f ignored; echo changed > remote-only"))
            .unwrap(),
    )
    .await;
    assert_eq!(terminal(&records).outcome, Outcome::Exit { code: 0 });
    assert!(terminal(&records)
        .workspace_changes
        .as_ref()
        .unwrap()
        .contains(&"remote-only".into()));
    assert!(!f.key.join("remote-only").exists());
}
/// Git tracks a file once it is in the index, even under an ignored path. The
/// snapshot starts from HEAD, so a file staged with `git add -f` and not yet
/// committed must still reach the job, while unstaged ignored files never do.
#[tokio::test]
async fn staged_file_under_an_ignored_path_reaches_the_job() {
    let f = Fixture::new(Cut::None, 28).await;
    fs::write(f.key.join("ignored"), "staged input\n").unwrap();
    git(&f.key, &["add", "-f", "ignored"]);
    let records = collect(
        f.motor()
            .run(f.request("test \"$(cat ignored)\" = 'staged input' && test \"$(git diff --cached --name-only)\" = ignored"))
            .unwrap(),
    )
    .await;
    assert_eq!(terminal(&records).outcome, Outcome::Exit { code: 0 });
    // The caller's own index still has the file staged and nothing was committed.
    assert_eq!(git(&f.key, &["diff", "--cached", "--name-only"]), "ignored");
}

#[tokio::test]
async fn staged_ignored_literal_names_and_deleted_paths_reach_the_job() {
    let f = Fixture::new(Cut::None, 28).await;
    for path in [&f.key, &f.sibling] {
        fs::write(path.join(".gitignore"), "target\nignored\ninputs/\n").unwrap();
        fs::create_dir_all(path.join("inputs")).unwrap();
        for name in ["a space.tgz", "b*.tgz", "c\nline.tgz", "deleted.tgz"] {
            let relative = format!("inputs/{name}");
            fs::write(path.join(&relative), name).unwrap();
            git(path, &["--literal-pathspecs", "add", "-f", "--", &relative]);
        }
        fs::remove_file(path.join("inputs/deleted.tgz")).unwrap();
        fs::write(path.join("inputs/b-other.tgz"), "not staged").unwrap();
    }
    let before = state(&f.key);
    let sibling_before = state(&f.sibling);
    let records = collect(
        f.motor()
            .run(f.request(
                "for dir in . ../commons; do\n\
                 test \"$(cat \"$dir/inputs/a space.tgz\")\" = 'a space.tgz' &&\n\
                 test \"$(cat \"$dir/inputs/b*.tgz\")\" = 'b*.tgz' &&\n\
                 test -f \"$dir/inputs/c\nline.tgz\" &&\n\
                 test ! -e \"$dir/inputs/deleted.tgz\" &&\n\
                 test ! -e \"$dir/inputs/b-other.tgz\" || exit 1;\n\
                 git -C \"$dir\" diff --cached --name-only -- 'inputs/a space.tgz' |\n\
                 grep -Fx 'inputs/a space.tgz' || exit 1;\n\
                 done",
            ))
            .unwrap(),
    )
    .await;
    assert_eq!(terminal(&records).outcome, Outcome::Exit { code: 0 });
    assert_eq!(before, state(&f.key));
    assert_eq!(sibling_before, state(&f.sibling));
}
/// The runner keeps the top-level target folder as the job's build output and
/// leaves it out of the verified tree. A file staged there must not be sent,
/// or every job from this checkout would be refused with tree_hash_mismatch.
#[tokio::test]
async fn staged_file_under_target_is_not_sent_and_does_not_block_the_job() {
    let f = Fixture::new(Cut::None, 28).await;
    fs::create_dir_all(f.key.join("target/probe")).unwrap();
    fs::write(f.key.join("target/probe/input"), "build output\n").unwrap();
    git(&f.key, &["add", "-f", "target/probe/input"]);
    let records = collect(
        f.motor()
            .run(f.request("test ! -e target/probe/input"))
            .unwrap(),
    )
    .await;
    assert_eq!(terminal(&records).outcome, Outcome::Exit { code: 0 });
}

#[tokio::test]
async fn workspace_and_sibling_exclude_all_top_level_target_but_keep_nested_target() {
    let f = Fixture::new(Cut::None, 28).await;
    for path in [&f.key, &f.sibling] {
        fs::write(path.join(".gitignore"), "").unwrap();
        fs::create_dir_all(path.join("target")).unwrap();
        fs::write(path.join("target/committed"), "committed output").unwrap();
        git(path, &["add", "-f", ".gitignore", "target/committed"]);
        git(
            path,
            &[
                "-c",
                "user.name=Test",
                "-c",
                "user.email=test@example.invalid",
                "commit",
                "-m",
                "track build output",
            ],
        );
        fs::write(path.join("target/staged"), "staged output").unwrap();
        git(path, &["add", "target/staged"]);
        fs::write(path.join("target/untracked"), "untracked output").unwrap();
        fs::create_dir_all(path.join("src/target")).unwrap();
        fs::write(path.join("src/target/input"), "source input").unwrap();
    }
    let before = state(&f.key);
    let sibling_before = state(&f.sibling);
    let records = collect(
        f.motor()
            .run(f.request(
                "for dir in . ../commons; do \
         test ! -e \"$dir/target/committed\" && \
         test ! -e \"$dir/target/staged\" && \
         test ! -e \"$dir/target/untracked\" && \
         test \"$(cat \"$dir/src/target/input\")\" = 'source input' || exit 1; done",
            ))
            .unwrap(),
    )
    .await;
    assert_eq!(terminal(&records).outcome, Outcome::Exit { code: 0 });
    assert_eq!(before, state(&f.key));
    assert_eq!(sibling_before, state(&f.sibling));
}
fn serialized_frame_metadata(frames: &[Frame]) -> String {
    let metadata: Vec<_> = frames
        .iter()
        .map(|frame| match frame {
            // Bundle data is arbitrary Git pack content, not environment
            // metadata. Its base64 can coincidentally contain names such as
            // HOME or LANG. Keep every frame's structured fields in the leak
            // check, but do not interpret the encoded pack as environment text.
            Frame::BundleChunk { index, offset, .. } => Frame::BundleChunk {
                index: *index,
                offset: *offset,
                data: BytePayload(Vec::new()),
            },
            _ => frame.clone(),
        })
        .collect();
    serde_json::to_string(&metadata).unwrap()
}

#[test]
fn transport_spy_bundle_base64_name_collision_is_not_environment_metadata() {
    let frames = [Frame::BundleChunk {
        index: 1,
        offset: 42,
        data: BytePayload(vec![0x1c, 0xe3, 0x04]),
    }];
    let wire = serde_json::to_string(&frames).unwrap();
    assert!(wire.contains(r#""data":"HOME""#));
    let metadata = serialized_frame_metadata(&frames);
    assert!(!metadata.contains("HOME"));
    assert!(metadata.contains(r#""index":1"#));
    assert!(metadata.contains(r#""offset":42"#));
    let structured = [Frame::Hello {
        protocol_version: PROTOCOL_VERSION,
        started: true,
        default_branch_tip: false,
        gate_jobs: false,
        platforms: vec!["HOME".into(), "dropped-value-7-private".into()],
    }];
    let metadata = serialized_frame_metadata(&structured);
    assert!(metadata.contains("HOME"));
    assert!(metadata.contains("dropped-value-7-private"));
}

#[tokio::test]
async fn transport_spy_filters_names_and_values_before_serialization() {
    let f = Fixture::new(Cut::None, 28).await;
    let mut request = f.request("true");
    let allowed = [
        "RUST_BACKTRACE",
        "RUSTFLAGS",
        "RUSTDOCFLAGS",
        "CARGO_INCREMENTAL",
        "CARGO_BUILD_JOBS",
        "NEXTEST_PROFILE",
        "RUST_TEST_THREADS",
        "NEXTEST_TEST_THREADS",
        "CI",
        "NO_COLOR",
        "CLICOLOR",
        "CLICOLOR_FORCE",
    ];
    let denied = [
        "SUBC_LAUNCH_NONCE",
        "SUBC_LAUNCH_NONCE_FD",
        "SUBC_MODULE_ID",
        "SSH_AUTH_SOCK",
        "ANTHROPIC_API_KEY",
        "CARGO_TARGET_DIR",
        "PATH",
        "HOME",
        "TMPDIR",
        "CARGO_HOME",
        "CARGO_INSTALL_ROOT",
        "CARGO_BUILD_RUSTC",
        "RUST_API_KEY",
        "RUST_TOKEN",
        "CARGO_SECRET",
        "NEXTEST_PASSWORD",
        "RUST_CREDENTIAL",
        "RUSTUP_HOME",
        "DYLD_LIBRARY_PATH",
        "SHELL",
        "LANG",
    ];
    request.env = allowed
        .iter()
        .map(|s| ((*s).into(), "1".into()))
        .chain(
            denied
                .iter()
                .enumerate()
                .map(|(i, s)| ((*s).into(), format!("dropped-value-{i}-private"))),
        )
        .collect();
    let records = collect(f.motor().run(request).unwrap()).await;
    assert_eq!(terminal(&records).outcome, Outcome::Exit { code: 0 });
    let frames = f.proxy.frames.lock().unwrap();
    let bytes = serialized_frame_metadata(&frames);
    let run = frames
        .iter()
        .find_map(|f| if let Frame::Run(r) = f { Some(r) } else { None })
        .unwrap();
    assert_eq!(run.env.len(), allowed.len());
    for name in allowed {
        assert_eq!(run.env[name], "1");
    }
    for (i, name) in denied.iter().enumerate() {
        assert!(!bytes.contains(name), "leaked name {name}");
        assert!(
            !bytes.contains(&format!("dropped-value-{i}-private")),
            "leaked value of {name}"
        );
    }
    assert!(run.env.keys().all(|k| motor_runner::allowed_env(k)));
}
#[test]
fn test_control_fields_are_refused_at_main_config_load_even_if_disabled() {
    let scratch = ScratchDir::new("motor-config");
    let path = scratch.join("config.jsonc");
    for field in [
        "ssh_config_override",
        "inject_tree_hash_mismatch",
        "inject_snapshot_failure",
    ] {
        let value = if field == "ssh_config_override" {
            serde_json::Value::Null
        } else {
            serde_json::json!(false)
        };
        fs::write(&path, serde_json::json!({field: value}).to_string()).unwrap();
        assert!(Config::load(&path).is_err(), "main accepted {field}");
        fs::write(
            &path,
            serde_json::json!({"runner_instance":"e2e",field: value}).to_string(),
        )
        .unwrap();
        Config::load(&path).unwrap();
    }
    fs::write(&path, "{\"runner_instance\":\"bad;command\"}").unwrap();
    assert!(Config::load(&path).is_err());
    fs::write(&path, "{\"queue_wait_limit_s\":\"wrong type\"}").unwrap();
    assert!(Config::load(&path).is_err());
    fs::write(&path, "{\"unknown\":true}").unwrap();
    assert!(Config::load(&path).is_err());
}
#[tokio::test]
async fn rejected_workspace_has_no_snapshot_and_unreachable_has_hash_within_bound() {
    let f = Fixture::new(Cut::None, 28).await;
    let motor = f.motor();
    let mut request = f.request("echo forbidden >> marker");
    request.workspace_key = f.sibling.to_str().unwrap().into();
    request.cwd = request.workspace_key.clone();
    let records = collect(motor.run(request).unwrap()).await;
    assert_eq!(
        terminal(&records).outcome,
        Outcome::RefusedBeforeStart {
            reason: RefusalReason::WorkspaceKeyRejected
        }
    );
    assert_eq!(terminal(&records).tree_hash, None);
    assert_eq!(terminal(&records).workspace_changes, None);
    assert!(f.proxy.frames.lock().unwrap().is_empty());
    f.proxy.unavailable.store(true, Ordering::SeqCst);
    let start = std::time::Instant::now();
    let stream = motor.run(f.request("echo forbidden >> marker")).unwrap();
    let id = stream.job_id;
    let records = collect(stream).await;
    assert!(start.elapsed() < Duration::from_secs(30));
    assert!(terminal(&records).tree_hash.is_some());
    assert_eq!(
        terminal(&records).outcome,
        Outcome::RefusedBeforeStart {
            reason: RefusalReason::Unreachable
        }
    );
    let replay = collect(motor.attach(AttachRequest::new(id, 0)).unwrap()).await;
    assert_eq!(terminal(&replay), terminal(&records));
    assert!(git(&f.key, &["for-each-ref", "refs/ck-motor/"]).is_empty());
    assert!(!f.key.join("marker").exists());
}
struct NoHello;
impl Connector for NoHello {
    fn connect(
        &self,
        _: Uuid,
    ) -> Pin<Box<dyn Future<Output = io::Result<Box<dyn Duplex>>> + Send + '_>> {
        Box::pin(async {
            let (a, b) = tokio::io::duplex(1024);
            tokio::spawn(async move {
                let _b = b;
                std::future::pending::<()>().await;
            });
            Ok(Box::new(a) as Box<dyn Duplex>)
        })
    }
}
struct ConnectError;
impl Connector for ConnectError {
    fn connect(
        &self,
        _: Uuid,
    ) -> Pin<Box<dyn Future<Output = io::Result<Box<dyn Duplex>>> + Send + '_>> {
        Box::pin(async { Err(io::Error::other("test connect error")) })
    }
}
struct ClosedHelloSend;
impl Connector for ClosedHelloSend {
    fn connect(
        &self,
        _: Uuid,
    ) -> Pin<Box<dyn Future<Output = io::Result<Box<dyn Duplex>>> + Send + '_>> {
        Box::pin(async {
            let (motor, runner) = tokio::io::duplex(1024);
            drop(runner);
            Ok(Box::new(motor) as Box<dyn Duplex>)
        })
    }
}
struct ClosedHelloRecv;
impl Connector for ClosedHelloRecv {
    fn connect(
        &self,
        _: Uuid,
    ) -> Pin<Box<dyn Future<Output = io::Result<Box<dyn Duplex>>> + Send + '_>> {
        Box::pin(async {
            let (motor, mut runner) = tokio::io::duplex(1024);
            tokio::spawn(async move {
                let _ = read_frame_async(&mut runner).await;
            });
            Ok(Box::new(motor) as Box<dyn Duplex>)
        })
    }
}
struct UnexpectedHelloReply;
impl Connector for UnexpectedHelloReply {
    fn connect(
        &self,
        _: Uuid,
    ) -> Pin<Box<dyn Future<Output = io::Result<Box<dyn Duplex>>> + Send + '_>> {
        Box::pin(async {
            let (motor, mut runner) = tokio::io::duplex(1024);
            tokio::spawn(async move {
                let _ = read_frame_async(&mut runner).await;
                let _ = write_frame_async(
                    &mut runner,
                    &Frame::HaveReply {
                        repositories: Vec::new(),
                        git_dependencies: None,
                    },
                )
                .await;
            });
            Ok(Box::new(motor) as Box<dyn Duplex>)
        })
    }
}
struct DisconnectDuringHave;
impl Connector for DisconnectDuringHave {
    fn connect(
        &self,
        _: Uuid,
    ) -> Pin<Box<dyn Future<Output = io::Result<Box<dyn Duplex>>> + Send + '_>> {
        Box::pin(async {
            let (motor, mut runner) = tokio::io::duplex(4096);
            tokio::spawn(async move {
                assert!(matches!(
                    read_frame_async(&mut runner).await.unwrap(),
                    Some(Frame::Hello { .. })
                ));
                write_frame_async(
                    &mut runner,
                    &Frame::Hello {
                        protocol_version: PROTOCOL_VERSION,
                        started: true,
                        default_branch_tip: false,
                        gate_jobs: false,
                        platforms: Vec::new(),
                    },
                )
                .await
                .unwrap();
                assert!(matches!(
                    read_frame_async(&mut runner).await.unwrap(),
                    Some(Frame::Have { .. })
                ));
            });
            Ok(Box::new(motor) as Box<dyn Duplex>)
        })
    }
}
fn assert_step_incremented(step: UnreachableStep, before: u64) {
    assert!(
        unreachable_count(step) > before,
        "refusal did not record step {}",
        step.label()
    );
}
#[tokio::test]
async fn run_have_failure_records_only_the_run_have_step() {
    let f = Fixture::new(Cut::None, 28).await;
    let step = UnreachableStep::HaveExchange;
    let other_step = UnreachableStep::Prepare;
    let before = unreachable_count(step);
    let other_before = unreachable_count(other_step);
    let motor = Motor::with_connector(
        f.config.clone(),
        &f.scratch.join("run-have-failure-state"),
        Arc::new(DisconnectDuringHave),
    )
    .unwrap();

    let records = collect(motor.run(f.request("true")).unwrap()).await;
    assert_eq!(
        terminal(&records).outcome,
        Outcome::RefusedBeforeStart {
            reason: RefusalReason::Unreachable
        }
    );
    assert_step_incremented(step, before);
    assert_eq!(unreachable_count(other_step), other_before);
}
#[tokio::test]
async fn prepare_have_failure_records_only_the_prepare_step() {
    let f = Fixture::new(Cut::None, 28).await;
    let step = UnreachableStep::Prepare;
    let other_step = UnreachableStep::HaveExchange;
    let before = unreachable_count(step);
    let other_before = unreachable_count(other_step);
    let motor = Motor::with_connector(
        f.config.clone(),
        &f.scratch.join("prepare-have-failure-state"),
        Arc::new(DisconnectDuringHave),
    )
    .unwrap();
    let request: PrepareRequest = serde_json::from_value(serde_json::json!({
        "workspace_key": f.key,
        "repository_root": f.main,
        "base_commit": git(&f.key, &["rev-parse", "HEAD"]),
        "siblings": [f.sibling]
    }))
    .unwrap();

    let reply = motor.prepare(request).await.unwrap();
    assert_eq!(
        reply.outcome,
        PrepareOutcome::RefusedBeforeStart {
            reason: RefusalReason::Unreachable
        }
    );
    assert_step_incremented(step, before);
    assert_eq!(unreachable_count(other_step), other_before);
}
#[tokio::test(start_paused = true)]
async fn persistent_connect_failure_records_only_the_final_timeout_diagnostic() {
    let step = UnreachableStep::Connect;
    let step_before = unreachable_count(step);
    let timeout_before = unreachable_count(UnreachableStep::Timeout15s);
    let start = tokio::time::Instant::now();
    let reason = match crate::transport::Channel::open(&ConnectError, Uuid::now_v7()).await {
        Err(reason) => reason,
        Ok(_) => panic!("connect error unexpectedly opened a channel"),
    };
    assert_eq!(reason, RefusalReason::Unreachable);
    assert_eq!(start.elapsed(), Duration::from_secs(15));
    assert_eq!(unreachable_count(step), step_before);
    assert_eq!(
        unreachable_count(UnreachableStep::Timeout15s),
        timeout_before + 1
    );
}
#[tokio::test(start_paused = true)]
async fn persistent_hello_send_failure_records_only_the_final_timeout_diagnostic() {
    let step_before = unreachable_count(UnreachableStep::HelloSend);
    let timeout_before = unreachable_count(UnreachableStep::Timeout15s);
    let start = tokio::time::Instant::now();
    let result = crate::transport::Channel::open(&ClosedHelloSend, Uuid::now_v7()).await;
    assert!(matches!(result, Err(RefusalReason::Unreachable)));
    assert_eq!(start.elapsed(), Duration::from_secs(15));
    assert_eq!(unreachable_count(UnreachableStep::HelloSend), step_before);
    assert_eq!(
        unreachable_count(UnreachableStep::Timeout15s),
        timeout_before + 1
    );
}
#[tokio::test(start_paused = true)]
async fn persistent_hello_receive_failure_records_only_the_final_timeout_diagnostic() {
    let step_before = unreachable_count(UnreachableStep::HelloRecv);
    let timeout_before = unreachable_count(UnreachableStep::Timeout15s);
    let start = tokio::time::Instant::now();
    let result = crate::transport::Channel::open(&ClosedHelloRecv, Uuid::now_v7()).await;
    assert!(matches!(result, Err(RefusalReason::Unreachable)));
    assert_eq!(start.elapsed(), Duration::from_secs(15));
    assert_eq!(unreachable_count(UnreachableStep::HelloRecv), step_before);
    assert_eq!(
        unreachable_count(UnreachableStep::Timeout15s),
        timeout_before + 1
    );
}
#[tokio::test]
async fn non_hello_first_frame_records_its_step() {
    let step = UnreachableStep::UnexpectedFrame;
    let before = unreachable_count(step);
    let result = crate::transport::Channel::open(&UnexpectedHelloReply, Uuid::now_v7()).await;
    assert!(matches!(result, Err(RefusalReason::Unreachable)));
    assert_step_incremented(step, before);
    assert_eq!(step.label(), "unexpected_frame");
}
#[tokio::test(start_paused = true)]
async fn hello_timeout_names_the_handshake_stage_it_was_waiting_in() {
    struct ConnectHangs;
    impl Connector for ConnectHangs {
        fn connect(
            &self,
            _: Uuid,
        ) -> Pin<Box<dyn Future<Output = io::Result<Box<dyn Duplex>>> + Send + '_>> {
            Box::pin(std::future::pending())
        }
    }
    let captured = crate::logging::Captured::default();
    let _sink = crate::logging::scoped(captured.clone());
    let silent_runner = Uuid::now_v7();
    let result = crate::transport::Channel::open(&NoHello, silent_runner).await;
    assert!(matches!(result, Err(RefusalReason::Unreachable)));
    let stuck_connect = Uuid::now_v7();
    let result = crate::transport::Channel::open(&ConnectHangs, stuck_connect).await;
    assert!(matches!(result, Err(RefusalReason::Unreachable)));
    let text = captured.text();
    let line = |id: Uuid| {
        text.lines()
            .find(|line| {
                line.contains(&format!("job_or_workspace_id={id}"))
                    && line.contains("remote executor refused as unreachable")
            })
            .unwrap_or_else(|| panic!("no refusal logged for {id}: {text:?}"))
            .to_owned()
    };
    let silent = line(silent_runner);
    assert!(silent.contains("step=timeout_15s"), "{silent}");
    assert!(
        silent.contains("no runner hello within 15s; still waiting in hello_recv"),
        "{silent}"
    );
    let stuck = line(stuck_connect);
    assert!(
        stuck.contains("still waiting in connect"),
        "a hung SSH connect must not read as a silent runner: {stuck}"
    );
}
#[tokio::test(start_paused = true)]
async fn hello_times_out_at_fifteen_seconds_and_ssh_connect_timeout_is_ten() {
    let step = UnreachableStep::Timeout15s;
    let before = unreachable_count(step);
    let start = tokio::time::Instant::now();
    let result = crate::transport::Channel::open(&NoHello, Uuid::now_v7()).await;
    assert!(matches!(result, Err(RefusalReason::Unreachable)));
    assert_eq!(start.elapsed(), Duration::from_secs(15));
    assert_step_incremented(step, before);
    assert_eq!(step.label(), "timeout_15s");
    let cmd = SshConnector::new(Config::default()).command(Uuid::now_v7());
    let args: Vec<_> = cmd
        .as_std()
        .get_args()
        .map(|a| a.to_string_lossy().into_owned())
        .collect();
    for arg in [
        "ConnectTimeout=10",
        "BatchMode=yes",
        "ForwardAgent=no",
        "ControlMaster=no",
    ] {
        assert!(args.contains(&arg.into()));
    }
    assert!(args.last().unwrap().contains("--instance main --job "));
}
#[tokio::test]
async fn prepare_status_drop_and_incremental_bundles_use_runner_replies() {
    let f = Fixture::new(Cut::None, 28).await;
    let motor = f.motor();
    let prepare: PrepareRequest = serde_json::from_value(serde_json::json!({"workspace_key":f.key,"repository_root":f.main,"base_commit":git(&f.key,&["rev-parse","HEAD"]),"siblings":[f.sibling]})).unwrap();
    assert_eq!(
        motor.prepare(prepare).await.unwrap().outcome,
        PrepareOutcome::Prepared
    );
    let first = collect(motor.run(f.request("true")).unwrap()).await;
    assert_eq!(terminal(&first).ran, Some(Ran::Remote));
    fs::write(f.key.join("source"), "next\n").unwrap();
    let second = collect(motor.run(f.request("cat source")).unwrap()).await;
    assert_eq!(terminal(&second).outcome, Outcome::Exit { code: 0 });
    assert!(terminal(&second).bundle_bytes < 100000);
    assert!(terminal(&second).bundle_bytes > 0);
    let status = motor.status().await;
    assert!(status.server_reachable);
    assert!(!status.rustc_version.is_empty());
    let drop = motor
        .drop_workspace(DropRequest::new(f.key.to_str().unwrap()))
        .await
        .unwrap();
    assert!(drop.dropped);
    assert!(!f.remote_key.exists());
    assert!(
        !motor
            .drop_workspace(DropRequest::new(f.key.to_str().unwrap()))
            .await
            .unwrap()
            .dropped
    );
}

#[tokio::test]
async fn unchanged_random_file_is_excluded_from_second_bundle() {
    use rand::{rngs::StdRng, RngCore, SeedableRng};
    let f = Fixture::new(Cut::None, 28).await;
    let motor = f.motor();
    let head = git(&f.key, &["rev-parse", "HEAD"]);
    let mut bytes = vec![0; 2 * 1024 * 1024];
    StdRng::seed_from_u64(0x5e2e).fill_bytes(&mut bytes);
    fs::write(f.key.join("unchanged-random.bin"), &bytes).unwrap();
    let first = collect(motor.run(f.request("true")).unwrap()).await;
    assert_eq!(terminal(&first).outcome, Outcome::Exit { code: 0 });
    assert!(
        terminal(&first).bundle_bytes >= bytes.len() as u64,
        "the first transfer must actually carry incompressible content"
    );
    fs::write(f.key.join("source"), "one line changed\n").unwrap();
    let second = collect(motor.run(f.request("cat source")).unwrap()).await;
    assert_eq!(terminal(&second).outcome, Outcome::Exit { code: 0 });
    assert!(terminal(&second).bundle_bytes > 0);
    assert!(
        terminal(&second).bundle_bytes < 1_000_000,
        "unchanged random file resent: {} bytes",
        terminal(&second).bundle_bytes
    );
    assert_eq!(
        fs::read(f.remote_key.join("unchanged-random.bin")).unwrap(),
        bytes
    );
    let snapshot = f
        .proxy
        .frames
        .lock()
        .unwrap()
        .iter()
        .filter_map(|frame| {
            if let Frame::Run(run) = frame {
                Some(run.snapshot_commit.clone())
            } else {
                None
            }
        })
        .next_back()
        .unwrap();
    assert_eq!(
        git(&f.key, &["rev-list", "--parents", "-n", "1", &snapshot]),
        format!("{snapshot} {head}"),
        "transport optimization must not change snapshot ancestry"
    );
    assert!(git(&f.key, &["for-each-ref", "refs/ck-motor/"]).is_empty());
}
struct FutureRecords {
    frames: Arc<Mutex<Vec<Frame>>>,
}

#[tokio::test]
async fn capacity_refusal_is_durable_without_snapshot_or_transport() {
    let f = Fixture::new(Cut::None, 28).await;
    f.proxy.unavailable.store(true, Ordering::SeqCst);
    let motor = f.motor();
    let records = collect(motor.refuse_capacity().unwrap()).await;
    assert_eq!(records.len(), 1);
    let terminal = terminal(&records).clone();
    assert_eq!(
        terminal.outcome,
        Outcome::RefusedBeforeStart {
            reason: RefusalReason::RunnerFull
        }
    );
    assert_eq!(terminal.ran, Some(Ran::None));
    assert!(terminal.tree_hash.is_none());
    assert!(f.proxy.frames.lock().unwrap().is_empty());
    let replay = collect(
        motor
            .attach(AttachRequest::new(terminal.job_id, 0))
            .unwrap(),
    )
    .await;
    assert_eq!(super::recovery::terminal(&replay), &terminal);
}
impl Connector for FutureRecords {
    fn connect(
        &self,
        _: Uuid,
    ) -> Pin<Box<dyn Future<Output = io::Result<Box<dyn Duplex>>> + Send + '_>> {
        Box::pin(async {
            let (a, mut b) = tokio::io::duplex(65536);
            let frames = self.frames.clone();
            tokio::spawn(async move {
                assert!(matches!(
                    read_frame_async(&mut b).await.unwrap(),
                    Some(Frame::Hello { .. })
                ));
                write_frame_async(
                    &mut b,
                    &Frame::Hello {
                        protocol_version: PROTOCOL_VERSION,
                        started: true,
                        default_branch_tip: false,
                        gate_jobs: false,
                        platforms: Vec::new(),
                    },
                )
                .await
                .unwrap();
                let attach = read_frame_async(&mut b).await.unwrap().unwrap();
                if let Frame::JobStatus { job_id } = attach {
                    write_frame_async(
                        &mut b,
                        &Frame::JobStatusReply(JobStatusReply {
                            job_id,
                            state: "running".into(),
                            terminal: None,
                            unit: None,
                            unit_alive: None,
                            queue_wait_ms: None,
                        }),
                    )
                    .await
                    .unwrap();
                    return;
                }
                frames.lock().unwrap().push(attach.clone());
                let Frame::Attach(attach) = attach else {
                    panic!("attach only")
                };
                if attach.from_seq == 0 {
                    write_frame_async(
                        &mut b,
                        &Frame::Output(Output::new(
                            0,
                            OutputStream::Stdout,
                            BytePayload(b"known".to_vec()),
                        )),
                    )
                    .await
                    .unwrap();
                    let json = br#"{"type":"future_record","seq":1}"#;
                    use tokio::io::AsyncWriteExt;
                    b.write_u32(json.len() as u32).await.unwrap();
                    b.write_all(json).await.unwrap();
                    let output: Output = serde_json::from_value(
                        serde_json::json!({"seq":2,"stream":"future_stream","bytes":"YQ=="}),
                    )
                    .unwrap();
                    write_frame_async(&mut b, &Frame::Output(output))
                        .await
                        .unwrap();
                } else {
                    assert_eq!(attach.from_seq, 3);
                    let terminal: TerminalRecord = serde_json::from_value(serde_json::json!({"job_id":attach.job_id,"outcome":{"type":"future_outcome"},"wall_ms":0,"queue_wait_ms":0,"bundle_bytes":0,"ran":"future_ran","tree_hash":null,"workspace_changes":null})).unwrap();
                    write_frame_async(&mut b, &Frame::Terminal(terminal))
                        .await
                        .unwrap();
                }
            });
            Ok(Box::new(a) as Box<dyn Duplex>)
        })
    }
}
#[tokio::test]
async fn unknown_records_advance_cursor_without_fallback_or_premature_terminal() {
    let scratch = ScratchDir::new("motor-future");
    let frames = Arc::new(Mutex::new(Vec::new()));
    let motor = Motor::with_connector(
        Config::default(),
        &scratch,
        Arc::new(FutureRecords {
            frames: frames.clone(),
        }),
    )
    .unwrap();
    let records = collect(motor.attach(AttachRequest::new(Uuid::now_v7(), 0)).unwrap()).await;
    assert_eq!(records.len(), 2);
    assert!(matches!(records[0], StreamRecord::Output(_)));
    assert!(!matches!(
        terminal(&records).outcome,
        Outcome::RefusedBeforeStart { .. }
    ));
    assert_ne!(terminal(&records).ran, Some(Ran::None));
    assert_eq!(frames.lock().unwrap().len(), 2);
}

struct UnknownCursorRecovery {
    unknown: serde_json::Value,
    resumed_output_seq: u64,
    attaches: Arc<Mutex<Vec<AttachRequest>>>,
}
impl Connector for UnknownCursorRecovery {
    fn connect(
        &self,
        _: Uuid,
    ) -> Pin<Box<dyn Future<Output = io::Result<Box<dyn Duplex>>> + Send + '_>> {
        Box::pin(async {
            let (a, mut b) = tokio::io::duplex(65536);
            let unknown = self.unknown.clone();
            let resumed_output_seq = self.resumed_output_seq;
            let attaches = self.attaches.clone();
            tokio::spawn(async move {
                assert!(matches!(
                    read_frame_async(&mut b).await.unwrap(),
                    Some(Frame::Hello { .. })
                ));
                write_frame_async(
                    &mut b,
                    &Frame::Hello {
                        protocol_version: PROTOCOL_VERSION,
                        started: true,
                        default_branch_tip: false,
                        gate_jobs: false,
                        platforms: Vec::new(),
                    },
                )
                .await
                .unwrap();
                let frame = read_frame_async(&mut b).await.unwrap();
                if let Some(Frame::JobStatus { job_id }) = frame {
                    write_frame_async(
                        &mut b,
                        &Frame::JobStatusReply(JobStatusReply {
                            job_id,
                            state: "running".into(),
                            terminal: None,
                            unit: None,
                            unit_alive: None,
                            queue_wait_ms: None,
                        }),
                    )
                    .await
                    .unwrap();
                    return;
                }
                let Some(Frame::Attach(attach)) = frame else {
                    panic!("attach only")
                };
                let first_connection = {
                    let mut requests = attaches.lock().unwrap();
                    let first = requests.is_empty();
                    requests.push(attach.clone());
                    first
                };
                let (seq, bytes) = if first_connection {
                    (6, b"before unknown".to_vec())
                } else {
                    (resumed_output_seq, b"after unknown".to_vec())
                };
                write_frame_async(
                    &mut b,
                    &Frame::Output(Output::new(seq, OutputStream::Stdout, BytePayload(bytes))),
                )
                .await
                .unwrap();
                if first_connection {
                    // Disconnect immediately after the future record: no later
                    // output may advance the cursor and conceal a skipped seq.
                    let json = serde_json::to_vec(&unknown).unwrap();
                    use tokio::io::AsyncWriteExt;
                    b.write_u32(json.len() as u32).await.unwrap();
                    b.write_all(&json).await.unwrap();
                } else {
                    // Always finish, even for a wrong attach cursor, so the test
                    // fails on the recorded request rather than a retry timeout.
                    let terminal =
                        TerminalRecord::new(attach.job_id, Outcome::Exit { code: 0 }, 0, 0, 0)
                            .with_ran(Ran::Remote);
                    write_frame_async(&mut b, &Frame::Terminal(terminal))
                        .await
                        .unwrap();
                }
            });
            Ok(Box::new(a) as Box<dyn Duplex>)
        })
    }
}

async fn assert_unknown_record_recovery(unknown: serde_json::Value, expected_cursor: u64) {
    let scratch = ScratchDir::new("motor-unknown-cursor");
    let attaches = Arc::new(Mutex::new(Vec::new()));
    let motor = Motor::with_connector(
        Config::default(),
        &scratch,
        Arc::new(UnknownCursorRecovery {
            unknown,
            resumed_output_seq: expected_cursor,
            attaches: attaches.clone(),
        }),
    )
    .unwrap();
    let id = Uuid::now_v7();
    let records = collect(motor.attach(AttachRequest::new(id, 6)).unwrap()).await;
    let requests = attaches.lock().unwrap();
    assert_eq!(requests.len(), 2);
    assert_eq!(requests[0].job_id, id);
    assert_eq!(requests[0].from_seq, 6);
    assert_eq!(requests[1].job_id, id);
    assert_eq!(requests[1].from_seq, expected_cursor);
    // Only the two known outputs and the actual terminal may reach the caller.
    assert_eq!(records.len(), 3);
    let StreamRecord::Output(before) = &records[0] else {
        panic!("known output before the future record")
    };
    assert_eq!(before.seq, 6);
    assert_eq!(before.bytes.0, b"before unknown");
    assert_eq!(before.truncated_before_seq, None);
    let StreamRecord::Output(after) = &records[1] else {
        panic!("known output after the future record")
    };
    assert_eq!(after.seq, expected_cursor);
    assert_eq!(after.bytes.0, b"after unknown");
    assert_eq!(after.truncated_before_seq, None);
    assert_eq!(terminal(&records).outcome, Outcome::Exit { code: 0 });
    assert_eq!(terminal(&records).ran, Some(Ran::Remote));
}

#[tokio::test]
async fn unknown_record_seq_advances_reattach_cursor_before_next_known_output() {
    assert_unknown_record_recovery(serde_json::json!({"type":"future_record", "seq":7}), 8).await;
}

#[tokio::test]
async fn unknown_record_without_seq_leaves_reattach_cursor_unchanged() {
    assert_unknown_record_recovery(serde_json::json!({"type":"future_record"}), 7).await;
}

#[tokio::test]
async fn malformed_resume_reply_does_not_advance_reattach_cursor() {
    assert_unknown_record_recovery(
        serde_json::json!({"type":"resume_reply", "resumed":"wrong-type", "seq":7}),
        7,
    )
    .await;
}

#[test]
fn client_and_runner_share_one_environment_predicate() {
    assert!(std::ptr::fn_addr_eq(
        crate::allowed_env as fn(&str) -> bool,
        motor_runner::allowed_env as fn(&str) -> bool,
    ));
    assert!(std::ptr::fn_addr_eq(
        crate::allowed_env as fn(&str) -> bool,
        motor_protocol::allowed_env as fn(&str) -> bool,
    ));
}

struct WireRecords(Vec<u8>);
impl Connector for WireRecords {
    fn connect(
        &self,
        _: Uuid,
    ) -> Pin<Box<dyn Future<Output = io::Result<Box<dyn Duplex>>> + Send + '_>> {
        Box::pin(async {
            let (a, mut b) = tokio::io::duplex(4096);
            let wire = self.0.clone();
            tokio::spawn(async move {
                assert!(matches!(
                    read_frame_async(&mut b).await.unwrap(),
                    Some(Frame::Hello { .. })
                ));
                write_frame_async(
                    &mut b,
                    &Frame::Hello {
                        protocol_version: PROTOCOL_VERSION,
                        started: true,
                        default_branch_tip: false,
                        gate_jobs: false,
                        platforms: Vec::new(),
                    },
                )
                .await
                .unwrap();
                use tokio::io::{AsyncReadExt, AsyncWriteExt};
                let _ = b.write_all(&wire).await;
                // Keep the stream open so an oversized prefix must be rejected
                // by the size bound, not by EOF or the inactivity deadline.
                let mut byte = [0];
                while matches!(b.read(&mut byte).await, Ok(1)) {}
            });
            Ok(Box::new(a) as Box<dyn Duplex>)
        })
    }
}

#[tokio::test]
async fn channel_rejects_oversized_prefix_without_waiting_for_body() {
    let wire = (motor_protocol::MAX_FRAME_BYTES as u32 + 1)
        .to_be_bytes()
        .to_vec();
    let mut channel = crate::transport::Channel::open(&WireRecords(wire), Uuid::now_v7())
        .await
        .unwrap();
    let result = tokio::time::timeout(Duration::from_secs(1), channel.next_job()).await;
    assert!(
        matches!(result, Ok(Err(_))),
        "oversized frame was not rejected immediately: {result:?}"
    );
}

#[tokio::test]
async fn channel_decodes_resume_and_future_frames_with_the_shared_decoder() {
    use crate::transport::Incoming;
    let mut wire = Vec::new();
    motor_protocol::write_frame(&mut wire, &Frame::Resume).unwrap();
    motor_protocol::write_frame(&mut wire, &Frame::ResumeReply { resumed: true }).unwrap();
    let future = br#"{"type":"future_control","seq":7,"payload":{"anything":[1,2]}}"#;
    wire.extend((future.len() as u32).to_be_bytes());
    wire.extend(future);
    let mut channel = crate::transport::Channel::open(&WireRecords(wire), Uuid::now_v7())
        .await
        .unwrap();
    assert!(matches!(
        channel.next_job().await.unwrap(),
        Incoming::Frame(Frame::Resume)
    ));
    assert!(matches!(
        channel.next_job().await.unwrap(),
        Incoming::Frame(Frame::ResumeReply { resumed: true })
    ));
    assert!(matches!(
        channel.next_job().await.unwrap(),
        Incoming::Unknown(Some(7))
    ));
}

#[test]
fn manifest_advertises_exact_capability_and_all_operations() {
    let value = serde_json::to_value(crate::module::manifest("ck-motor")).unwrap();
    assert_eq!(
        value["capabilities"]["provides"],
        serde_json::json!(["exec-remote/v1", "flow-scopes/v1"])
    );
    let bytes = serde_json::to_string(&value).unwrap();
    for name in crate::module::OPERATIONS {
        assert!(bytes.contains(name));
    }
    let health = value["provides"]
        .as_array()
        .unwrap()
        .iter()
        .flat_map(|role| role["operations"].as_array().into_iter().flatten())
        .find(|op| op["name"] == "runner.health")
        .expect("runner.health is advertised");
    assert_eq!(health["kind"], "query");
}

#[test]
fn non_streaming_operations_use_a_single_response_not_stream_data() {
    let reply = StatusReply::new(0, Vec::new(), false, Vec::new(), String::new());
    let outcome = crate::module::response(reply.clone()).unwrap();
    let subc_client_rs::HandlerOutcome::Response(bytes) = outcome else {
        panic!("status requires one response");
    };
    assert_eq!(
        serde_json::from_slice::<StatusReply>(&bytes).unwrap(),
        reply
    );
}

/// Run the scenario behind every alfonso task after the first: a job from one
/// worktree, its workspace dropped once the task ends, main moving on by one
/// changed file, and a job from a fresh worktree at the new main. Returns the
/// bundle bytes each of the two jobs sent.
async fn second_worktree_bytes(f: &Fixture, history: Option<&[u8]>) -> (u64, u64) {
    let motor = f.motor();
    let commit = |path: &Path, message: &str| {
        git(path, &["add", "-A"]);
        git(
            path,
            &[
                "-c",
                "user.name=Test",
                "-c",
                "user.email=test@example.invalid",
                "commit",
                "-q",
                "--allow-empty",
                "-m",
                message,
            ],
        );
        git(path, &["rev-parse", "HEAD"])
    };
    if let Some(bytes) = history {
        fs::write(f.main.join("history.bin"), bytes).unwrap();
        commit(&f.main, "history");
    }
    let tip = git(&f.main, &["rev-parse", "HEAD"]);
    git(&f.key, &["reset", "-q", "--hard", &tip]);
    let first_branch = commit(&f.key, "unmerged first worktree");
    let first = collect(motor.run(f.request("true")).unwrap()).await;
    assert_eq!(terminal(&first).outcome, Outcome::Exit { code: 0 });
    assert!(
        motor
            .drop_workspace(DropRequest::new(f.key.to_str().unwrap()))
            .await
            .unwrap()
            .dropped
    );
    let changed = ["source", "README.md", "Cargo.toml"]
        .iter()
        .map(|name| f.main.join(name))
        .find(|path| path.exists())
        .unwrap();
    let mut text = fs::read(&changed).unwrap();
    text.extend_from_slice(b"\none changed line\n");
    fs::write(&changed, text).unwrap();
    let next_tip = commit(&f.main, "one changed file");
    let next = f.key.parent().unwrap().join("b");
    git(
        &f.main,
        &[
            "worktree",
            "add",
            "-q",
            "-b",
            "task-b",
            next.to_str().unwrap(),
        ],
    );
    let second_branch = commit(&next, "unmerged second worktree");
    let request: RunRequest = serde_json::from_value(serde_json::json!({
        "workspace_key": next, "repository_root": f.main, "cwd": next,
        "command": "true", "env": {}, "siblings": [f.sibling], "queue_wait_limit_s": 900
    }))
    .unwrap();
    let second = collect(motor.run(request).unwrap()).await;
    assert_eq!(terminal(&second).outcome, Outcome::Exit { code: 0 });
    assert!(
        f.jobs()
            .iter()
            .filter(|job| job["base"] == true)
            .all(|job| {
                let commit = job["request"]["snapshot_commit"].as_str().unwrap();
                (commit == tip || commit == next_tip)
                    && commit != first_branch
                    && commit != second_branch
            }),
        "current clients must fill only from local default tips, never unmerged worktree commits"
    );
    (
        terminal(&first).bundle_bytes,
        terminal(&second).bundle_bytes,
    )
}

/// The second job's candidates (its worktree HEAD, snapshot and the new main
/// tip) are all commits the runner has never seen, and the first workspace's
/// snapshot was dropped with it. The runner pins integration tips independently
/// of cache fills and must still report the retained tip, so the second transfer
/// carries the one change instead of the whole history again.
#[tokio::test]
async fn second_worktree_after_main_moves_sends_only_the_change() {
    use rand::{rngs::StdRng, RngCore, SeedableRng};
    let f = Fixture::new(Cut::None, 28).await;
    let mut history = vec![0; 2 * 1024 * 1024];
    StdRng::seed_from_u64(0x7b1).fill_bytes(&mut history);
    let (first, second) = second_worktree_bytes(&f, Some(&history)).await;
    assert!(
        first >= history.len() as u64,
        "the first transfer must carry the history: {first} bytes"
    );
    assert!(second > 0);
    assert!(
        second < 100_000,
        "the second transfer resent history: {second} bytes after {first}"
    );
}

/// Measurement, not a gate: the same scenario over a real repository's
/// history. Run with
/// `MOTOR_MEASURE_REPO=/path/to/repo cargo nextest run -p motor-module --lib --run-ignored only measure_second_worktree_bytes --no-capture`.
#[tokio::test]
#[ignore = "measurement over an external repository named by MOTOR_MEASURE_REPO"]
async fn measure_second_worktree_bytes() {
    let source = std::path::PathBuf::from(
        std::env::var_os("MOTOR_MEASURE_REPO").expect("MOTOR_MEASURE_REPO names a repository"),
    );
    let f = Fixture::with_main(Cut::None, 28, Some(&source)).await;
    let (first, second) = second_worktree_bytes(&f, None).await;
    println!("motor transfer bytes: first job {first}, second job with one changed file {second}");
}

#[tokio::test]
async fn first_tip_prepare_publishes_status_without_generation_seeding() {
    let f = Fixture::new(Cut::None, 28).await;
    let tip = git(&f.main, &["rev-parse", "HEAD"]);
    if git(&f.main, &["branch", "--show-current"]) != "main" {
        git(&f.main, &["branch", "-f", "main", &tip]);
    }
    let tip = git(&f.main, &["rev-parse", "refs/heads/main"]);
    let motor = Motor::with_connector(
        f.config.clone(),
        &f.scratch.join("module-state"),
        Arc::new(DefaultBranchFixtureTransport {
            inner: f.proxy.clone(),
            tip: tip.clone(),
        }),
    )
    .unwrap();
    let prepare: PrepareRequest = serde_json::from_value(serde_json::json!({
        "workspace_key": f.key, "repository_root": f.main,
        "base_commit": tip, "siblings": [f.sibling]
    }))
    .unwrap();
    assert_eq!(
        motor.prepare(prepare).await.unwrap().outcome,
        PrepareOutcome::Prepared
    );
    let deadline = tokio::time::Instant::now() + Duration::from_secs(10);
    loop {
        let status = motor.status().await;
        if let Some(repository) = status
            .repositories
            .iter()
            .find(|repository| repository.published_commit.as_deref() == Some(tip.as_str()))
        {
            assert_eq!(repository.last_rebuild_result, Some(RebuildResult::Ok));
            assert!(repository.warm_target_age_s.is_some());
            break;
        }
        assert!(
            tokio::time::Instant::now() < deadline,
            "first integration tip never published: {status:?}"
        );
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    assert_eq!(
        fs::read_dir(f.remote_key.join("target")).unwrap().count(),
        0
    );
}

// This relay stands in for the client-tip slice's production behavior: advertise
// the default-branch capability and send the fixture's explicitly pinned local
// main tip. All other client bytes and every server byte pass through unchanged.
struct DefaultBranchFixtureTransport {
    inner: Arc<super::recovery::Proxy>,
    tip: String,
}
impl Connector for DefaultBranchFixtureTransport {
    fn connect(
        &self,
        id: Uuid,
    ) -> Pin<Box<dyn Future<Output = io::Result<Box<dyn Duplex>>> + Send + '_>> {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        Box::pin(async move {
            let upstream = self.inner.connect(id).await?;
            let (client, relay) = tokio::io::duplex(65536);
            let tip = self.tip.clone();
            tokio::spawn(async move {
                let (mut client_read, mut client_write) = tokio::io::split(relay);
                let (mut server_read, mut server_write) = tokio::io::split(upstream);
                let send = async {
                    loop {
                        let mut length = [0; 4];
                        if client_read.read(&mut length[..1]).await? == 0 {
                            server_write.shutdown().await?;
                            return Ok::<_, io::Error>(());
                        }
                        client_read.read_exact(&mut length[1..]).await?;
                        let mut bytes = vec![0; u32::from_be_bytes(length) as usize];
                        client_read.read_exact(&mut bytes).await?;
                        let frame: Frame = serde_json::from_slice(&bytes)?;
                        let replacement = match frame {
                            Frame::Hello {
                                default_branch_tip: false,
                                gate_jobs: false,
                                ..
                            } => {
                                let mut text = String::from_utf8(bytes).unwrap();
                                assert!(!text.contains("\"default_branch_tip\""));
                                assert!(text.ends_with('}'));
                                text.insert_str(text.len() - 1, ",\"default_branch_tip\":true");
                                bytes = text.into_bytes();
                                None
                            }
                            Frame::Prepare(prepare) => Some((
                                format!(
                                    "\"integration_tip\":{}",
                                    serde_json::to_string(&prepare.integration_tip)?
                                ),
                                format!("\"integration_tip\":{}", serde_json::to_string(&tip)?),
                            )),
                            Frame::Run(run) => Some((
                                format!(
                                    "\"integration_tip\":{}",
                                    serde_json::to_string(&run.integration_tip)?
                                ),
                                format!("\"integration_tip\":{}", serde_json::to_string(&tip)?),
                            )),
                            _ => None,
                        };
                        if let Some((old, new)) = replacement {
                            let text = String::from_utf8(bytes).unwrap();
                            assert_eq!(text.matches(&old).count(), 1);
                            bytes = text.replacen(&old, &new, 1).into_bytes();
                        }
                        server_write
                            .write_all(&(bytes.len() as u32).to_be_bytes())
                            .await?;
                        server_write.write_all(&bytes).await?;
                    }
                };
                let receive = tokio::io::copy(&mut server_read, &mut client_write);
                let _ = tokio::try_join!(send, receive);
            });
            Ok(Box::new(client) as Box<dyn Duplex>)
        })
    }
}
