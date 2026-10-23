use super::*;
use cortexkit_exec_remote_types as shared;
use sha2::{Digest, Sha256};
use std::sync::atomic::{AtomicUsize, Ordering};

struct FakeRunner {
    root: PathBuf,
    records: Vec<serde_json::Value>,
    submissions: AtomicUsize,
}

#[async_trait]
impl ExecClient for FakeRunner {
    fn local_workspace(&self) -> Option<&Path> {
        Some(&self.root)
    }
    async fn run(&self, _: &RunRequest) -> Result<Box<dyn RecordStream>, SubmitError> {
        self.submissions.fetch_add(1, Ordering::SeqCst);
        self.attach(Uuid::nil(), "").await
    }
    async fn attach(&self, _: Uuid, _: &str) -> Result<Box<dyn RecordStream>, SubmitError> {
        let (tx, rx) = mpsc::unbounded_channel();
        for record in &self.records {
            tx.send(Ok(serde_json::to_vec(record).unwrap())).unwrap();
        }
        Ok(Box::new(QueuedStream(rx)))
    }
    async fn status(&self, _: Uuid, _: &str) -> Result<JobStatusReply, SubmitError> {
        unreachable!()
    }
    async fn cancel(&self, _: Uuid, _: Option<&str>) -> Result<(), String> {
        unreachable!()
    }
}

fn terminal(id: Uuid) -> serde_json::Value {
    json!(StreamRecord::Terminal(TerminalRecord::new(
        id,
        Outcome::Exit { code: 23 },
        0,
        0,
        0
    )))
}

fn stream(id: Uuid, paths: &[String], chunked: bool, bytes: &[u8]) -> Vec<serde_json::Value> {
    let digest = format!("{:x}", Sha256::digest(bytes));
    let mut records = if chunked {
        let mut records = Vec::new();
        for (index, data) in bytes.chunks(shared::RETURN_CHUNK_MAX_BYTES).enumerate() {
            records.push(json!(StreamRecord::ReturnChunk(shared::ReturnChunk::new(
                id,
                index as u64,
                0,
                (index * shared::RETURN_CHUNK_MAX_BYTES) as u64,
                shared::BytePayload(data.to_vec())
            ))));
        }
        records.push(json!(StreamRecord::ReturnManifest(
            shared::ReturnManifest::new(
                id,
                records.len() as u64,
                vec![shared::ReturnManifestEntry::returned(
                    &paths[0],
                    bytes.len() as u64,
                    digest
                )],
                bytes.len() as u64,
                shared::RETURN_CHUNKED_MAX_BYTES_CAP
            )
        )));
        records
    } else {
        vec![json!(StreamRecord::ReturnResult(
            shared::ReturnResult::with_files(
                id,
                0,
                vec![shared::ReturnedFile::new(
                    &paths[0],
                    digest,
                    shared::BytePayload(bytes.to_vec())
                )]
            )
        ))]
    };
    records[0]["return_paths"] = json!(paths);
    records.push(terminal(id));
    records
}

async fn run(client: &FakeRunner, paths: Vec<String>, attach: bool) -> (u8, String) {
    let (_tx, mut interrupts) = mpsc::channel(1);
    let mut out = Vec::new();
    let mut err = Vec::new();
    let code = if attach {
        drive_attach(
            client,
            &AttachArgs {
                job_id: Uuid::nil(),
                secret_file: None,
                status: false,
            },
            "secret",
            &mut out,
            &mut err,
            &mut interrupts,
        )
        .await
    } else {
        let request = RunRequest::new(
            client.root.to_str().unwrap(),
            client.root.to_str().unwrap(),
            client.root.to_str().unwrap(),
            "true",
        )
        .with_return_paths(paths)
        .with_return_chunked(true)
        .with_return_max_bytes(shared::RETURN_CHUNKED_MAX_BYTES_CAP);
        drive_run(client, &request, None, &mut out, &mut err, &mut interrupts).await
    };
    assert!(out.is_empty());
    (code, String::from_utf8(err).unwrap())
}

#[tokio::test]
async fn cli_return_v1_and_v2_write_files_and_reattach() {
    for chunked in [false, true] {
        let scratch = cortexkit_test_support::ScratchDir::new("cli-return-stream");
        let root = fs::canonicalize(scratch.path()).unwrap();
        let path = root.join(".cortexkit/gate/return-probe.txt");
        let paths = vec![path.to_str().unwrap().to_owned()];
        let bytes = vec![b'x'; if chunked { 700_000 } else { 8 }];
        let client = FakeRunner {
            root,
            records: stream(Uuid::now_v7(), &paths, chunked, &bytes),
            submissions: AtomicUsize::new(0),
        };
        for attach in [false, true] {
            let (code, err) = run(&client, paths.clone(), attach).await;
            assert_eq!(code, 23, "{err}");
            assert_eq!(fs::read(&path).unwrap(), bytes);
            assert_eq!(
                err.matches(&format!(
                    "ck motor run: returned {} ({} bytes)",
                    path.display(),
                    bytes.len()
                ))
                .count(),
                1,
                "{err}"
            );
            fs::remove_file(&path).unwrap();
        }
        assert_eq!(client.submissions.load(Ordering::SeqCst), 1);
    }
}

#[tokio::test]
async fn cli_return_each_failure_cause_is_reported_without_changing_exit() {
    for chunked in [false, true] {
        for cause in [
            "missing",
            "not_regular",
            "escape",
            "over_cap",
            "read_error",
            "expired",
        ] {
            let scratch = cortexkit_test_support::ScratchDir::new("cli-return-failure");
            let root = fs::canonicalize(scratch.path()).unwrap();
            let paths = vec![root.join("out").to_str().unwrap().to_owned()];
            let id = Uuid::now_v7();
            let failure = if chunked {
                json!(StreamRecord::ReturnManifest(shared::ReturnManifest::new(
                    id,
                    0,
                    vec![shared::ReturnManifestEntry::failed(
                        &paths[0],
                        shared::ReturnPathsFailureCause::from(cause.to_owned())
                    )],
                    0,
                    shared::RETURN_CHUNKED_MAX_BYTES_CAP
                )))
            } else {
                json!(StreamRecord::ReturnResult(shared::ReturnResult::failed(
                    id,
                    0,
                    shared::ReturnPathsFailed::new(
                        0,
                        262144,
                        shared::ReturnPathsFailureCause::from(cause.to_owned())
                    )
                )))
            };
            let mut records = vec![failure, terminal(id)];
            records[0]["return_paths"] = json!(paths);
            let client = FakeRunner {
                root,
                records,
                submissions: AtomicUsize::new(0),
            };
            for attach in [false, true] {
                let (code, err) = run(&client, paths.clone(), attach).await;
                assert_eq!(code, 23);
                assert!(
                    err.contains(&format!("ck motor run: not returned {}: {cause}", paths[0])),
                    "{err}"
                );
                assert_eq!(err.matches("not returned").count(), 1);
                assert_eq!(
                    err.contains("the job didn't create it, or it is outside the workspace"),
                    cause == "missing"
                );
                assert!(!Path::new(&paths[0]).exists());
            }
        }
    }
}

#[tokio::test]
async fn cli_return_local_escape_is_refused() {
    let scratch = cortexkit_test_support::ScratchDir::new("cli-return-escape");
    let base = fs::canonicalize(scratch.path()).unwrap();
    let root = base.join("workspace");
    fs::create_dir(&root).unwrap();
    let outside = base.join("outside");
    fs::write(&outside, b"keep").unwrap();
    for path in [outside.clone(), root.join("../outside")] {
        let paths = vec![path.to_str().unwrap().to_owned()];
        let client = FakeRunner {
            root: root.clone(),
            records: stream(Uuid::now_v7(), &paths, true, b"bad"),
            submissions: AtomicUsize::new(0),
        };
        let (code, err) = run(&client, paths, false).await;
        assert_eq!(code, 23);
        assert!(err.contains(": escape"), "{err}");
        assert_eq!(fs::read(&outside).unwrap(), b"keep");
    }
}

#[tokio::test]
async fn cli_return_local_symlink_chain_and_destination_are_refused() {
    let scratch = cortexkit_test_support::ScratchDir::new("cli-return-symlink");
    let root = fs::canonicalize(scratch.path()).unwrap();
    let original = root.join("original");
    fs::create_dir(&original).unwrap();
    fs::write(original.join("out"), b"keep").unwrap();
    std::os::unix::fs::symlink(&original, root.join("parent-link")).unwrap();
    std::os::unix::fs::symlink(original.join("out"), root.join("destination-link")).unwrap();
    for path in [root.join("parent-link/out"), root.join("destination-link")] {
        let paths = vec![path.to_str().unwrap().to_owned()];
        let client = FakeRunner {
            root: root.clone(),
            records: stream(Uuid::now_v7(), &paths, false, b"bad"),
            submissions: AtomicUsize::new(0),
        };
        let (code, err) = run(&client, paths, false).await;
        assert_eq!(code, 23);
        assert!(err.contains(": read_error (local write refused:"), "{err}");
        assert_eq!(fs::read(original.join("out")).unwrap(), b"keep");
    }
    assert!(fs::symlink_metadata(root.join("destination-link"))
        .unwrap()
        .file_type()
        .is_symlink());
    assert_eq!(
        fs::read_dir(&root).unwrap().count(),
        3,
        "no temporary file remains"
    );
}

#[tokio::test]
async fn cli_return_invalid_digest_never_installs_unverified_bytes() {
    let scratch = cortexkit_test_support::ScratchDir::new("cli-return-integrity");
    let root = fs::canonicalize(scratch.path()).unwrap();
    let paths = vec![root.join("out").to_str().unwrap().to_owned()];
    let mut records = stream(Uuid::now_v7(), &paths, true, b"bad");
    records[1]["files"][0]["sha256"] = json!("0".repeat(64));
    let client = FakeRunner {
        root,
        records,
        submissions: AtomicUsize::new(0),
    };
    let (code, err) = run(&client, paths.clone(), false).await;
    assert_eq!(code, 23);
    assert!(
        err.contains("not returned") && err.contains("read_error"),
        "{err}"
    );
    assert!(!Path::new(&paths[0]).exists());
}

#[test]
fn cli_return_parse_refuses_relative_literal_variable_and_outside_paths() {
    let scratch = cortexkit_test_support::ScratchDir::new("cli-return-parse");
    let root = fs::canonicalize(scratch.path()).unwrap();
    crate::snapshot::git(&root, &["init", "-q"]).unwrap();
    let parse = |path: &str| {
        parse_args(&[
            "--return-path".into(),
            path.into(),
            "--cwd".into(),
            root.to_str().unwrap().into(),
            "--".into(),
            "true".into(),
        ])
    };
    for path in [
        "relative",
        "$PWD/.cortexkit/gate/return-probe.txt",
        "/outside/file",
    ] {
        let error = parse(path).unwrap_err();
        assert!(error.contains(path), "{error}");
        if path.starts_with('$') {
            assert!(error.contains("a shell variable inside single quotes isn't expanded"));
        }
    }
    let escape = root.join("../outside");
    assert!(parse(escape.to_str().unwrap()).is_err());
    let good = root.join(".cortexkit/gate/return-probe.txt");
    let Invocation::Run(args) = parse(good.to_str().unwrap()).unwrap() else {
        panic!("run")
    };
    let request = build_request(&args).unwrap();
    assert!(request.return_chunked);
    assert_eq!(
        request.return_max_bytes,
        Some(shared::RETURN_CHUNKED_MAX_BYTES_CAP)
    );
    assert_eq!(request.return_paths, [good.to_str().unwrap()]);
}

#[test]
fn cli_return_install_keeps_an_existing_mode_and_gives_new_files_the_umask_mode() {
    use std::os::unix::fs::PermissionsExt;
    let scratch = cortexkit_test_support::ScratchDir::new("cli-return-mode");
    let root = fs::canonicalize(scratch.path()).unwrap();
    let script = root.join("run.sh");
    fs::write(&script, b"old").unwrap();
    fs::set_permissions(&script, fs::Permissions::from_mode(0o750)).unwrap();
    super::returned_files::install(&root, &script, b"new").unwrap();
    assert_eq!(fs::read(&script).unwrap(), b"new");
    assert_eq!(
        fs::metadata(&script).unwrap().permissions().mode() & 0o7777,
        0o750
    );

    let umask = unsafe { libc::umask(0o022) };
    unsafe { libc::umask(umask) };
    let fresh = root.join("fresh.txt");
    super::returned_files::install(&root, &fresh, b"x").unwrap();
    assert_eq!(
        fs::metadata(&fresh).unwrap().permissions().mode() & 0o777,
        0o666 & !(umask as u32)
    );
}
