use super::*;

#[tokio::test]
async fn return_paths_result_is_sequenced_before_real_terminal_and_durable_on_attach() {
    let f = Fixture::new();
    let config = f.config();
    let runner = Runner::open(config.clone()).unwrap();
    let (mut request, bytes) = f.run("printf x > returned; printf output; exit 23");
    request.return_paths = vec![format!("{}/returned", f.key)];
    request.return_max_bytes = 1;
    f.submit(&runner, &request, &bytes);
    let t = terminal(&runner, request.job_id).await;
    assert_eq!(t.outcome, Outcome::Exit { code: 23 });
    assert_eq!(t.ran, Some(Ran::Remote));
    base_done(&runner).await;
    let frames = runner.replay(request.job_id, 0).unwrap();
    let Frame::ReturnResult(result) = &frames[frames.len() - 2] else {
        panic!("missing result: {frames:?}")
    };
    assert_eq!(result.seq, 2);
    assert_eq!(result.return_paths_failed, None);
    let file = &result.returned_files.as_ref().unwrap()[0];
    assert_eq!(
        file.sha256,
        "2d711642b726b04401627ca9fbac32f5c8530fb1903cc4db02258717921a4881"
    );
    assert_eq!(file.data, "eA==");
    assert_eq!(
        runner.replay(request.job_id, result.seq).unwrap(),
        frames[frames.len() - 2..]
    );
    assert!(matches!(
        &runner.replay(request.job_id, result.seq + 1).unwrap()[..],
        [Frame::Terminal(_)]
    ));
    let legacy = runner
        .replay_legacy(request.job_id, result.seq - 1)
        .unwrap();
    assert!(
        matches!(&legacy[..], [Frame::ReturnResult(r), Frame::Terminal(_)] if r.seq == result.seq-1 && r.returned_files == result.returned_files)
    );
    drop(runner);
    let runner = reopen(config);
    assert_eq!(runner.replay(request.job_id, 0).unwrap(), frames);
}

#[tokio::test]
async fn return_paths_cap_failure_preserves_remote_exit() {
    let f = Fixture::new();
    let runner = Runner::open(f.config()).unwrap();
    let (mut request, bytes) = f.run("printf xy > returned; exit 17");
    request.return_paths = vec![format!("{}/returned", f.key)];
    request.return_max_bytes = 1;
    f.submit(&runner, &request, &bytes);
    let t = terminal(&runner, request.job_id).await;
    assert_eq!(t.outcome, Outcome::Exit { code: 17 });
    assert_eq!(t.ran, Some(Ran::Remote));
    let frames = runner.replay(request.job_id, 0).unwrap();
    let Frame::ReturnResult(result) = &frames[frames.len() - 2] else {
        panic!("missing result")
    };
    assert_eq!(result.returned_files, None);
    assert_eq!(
        result.return_paths_failed,
        Some(ReturnPathsFailed {
            measured_bytes: 2,
            cap_bytes: 1,
            cause: ReturnFailureCause::OverCap
        })
    );
}

#[test]
fn return_paths_invalid_bounds_are_refused_before_admission() {
    let f = Fixture::new();
    let runner = Runner::open(f.config()).unwrap();
    let (mut request, _) = f.run("touch should-not-run");
    request.return_paths = vec![format!("{}/../outside", f.key)];
    assert!(matches!(
        runner.accept(request.clone()).unwrap(),
        Admission::Refused(RefusalReason::RequestRejected)
    ));
    let frames = runner.replay(request.job_id, 0).unwrap();
    assert!(
        matches!(&frames[..], [Frame::Terminal(t)] if matches!(t.outcome, Outcome::RefusedBeforeStart { reason: RefusalReason::RequestRejected }) && t.refusal_detail.as_deref().unwrap().contains("return_paths_exceeded"))
    );
}

#[cfg(target_os = "linux")]
#[tokio::test]
async fn return_paths_missing_escape_and_frame_limit_keep_real_remote_exit() {
    let f = Fixture::new();
    let runner = Runner::open(f.config()).unwrap();
    let secret = f.scratch.join("outside-file");
    fs::write(&secret, b"private").unwrap();
    let relative = format!("{}/returned", vec!["\u{1}".repeat(100); 34].join("/"));
    for (command, paths, cause, measured) in [
        (
            "exit 17".into(),
            vec![format!("{}/missing", f.key)],
            ReturnFailureCause::Missing,
            0,
        ),
        (
            format!("ln -s '{}' returned-link; exit 17", text(&secret)),
            vec![format!("{}/returned-link", f.key)],
            ReturnFailureCause::Escape,
            0,
        ),
        (
            format!(
                "mkdir -p '{}'; printf x > '{}'; exit 17",
                Path::new(&relative).parent().unwrap().display(),
                relative
            ),
            vec![format!("{}/{}", f.key, relative); 64],
            ReturnFailureCause::FrameLimit,
            64,
        ),
    ] {
        let (mut request, bytes) = f.run(&command);
        request.return_paths = paths;
        f.submit(&runner, &request, &bytes);
        let t = terminal(&runner, request.job_id).await;
        assert_eq!(t.outcome, Outcome::Exit { code: 17 });
        assert_eq!(t.ran, Some(Ran::Remote));
        let result = runner
            .replay(request.job_id, 0)
            .unwrap()
            .into_iter()
            .find_map(|frame| match frame {
                Frame::ReturnResult(result) => Some(result),
                _ => None,
            })
            .unwrap();
        assert_eq!(result.returned_files, None);
        assert_eq!(
            result.return_paths_failed,
            Some(ReturnPathsFailed {
                measured_bytes: measured,
                cap_bytes: RETURN_MAX_BYTES,
                cause
            })
        );
    }
}

/// The returned-file records of one replay, checked with the shared
/// caller-side checker: each path's verified bytes (`None` when it failed),
/// the manifest, and the seqs of the chunks.
pub(super) fn assemble(
    frames: &[Frame],
    run: &Run,
) -> (Vec<Option<Vec<u8>>>, ReturnManifest, Vec<u64>) {
    let mut assembler = ReturnAssembler::new(&run.return_paths, run.return_max_bytes);
    let mut bytes = vec![Vec::new(); run.return_paths.len()];
    let mut seqs = Vec::new();
    let mut manifest = None;
    let mut last_output_seq = 0;
    for frame in frames {
        match frame {
            Frame::ReturnChunk(chunk) => {
                assert!(manifest.is_none(), "chunks come before the manifest");
                assert!(serde_json::to_vec(frame).unwrap().len() <= MAX_FRAME_BYTES);
                seqs.push(chunk.seq);
                let data = assembler.accept_chunk(chunk).unwrap();
                bytes[chunk.path_index as usize].extend_from_slice(data);
            }
            Frame::ReturnManifest(record) => manifest = Some(record.clone()),
            Frame::ReturnResult(_) => panic!("a chunked request never gets return_result"),
            Frame::Output(output) => last_output_seq = output.seq,
            _ => {}
        }
    }
    let manifest = manifest.expect("chunked request replays its manifest");
    let verified = if manifest
        .files
        .iter()
        .any(|entry| entry.cause() == Some(&ReturnPathsFailureCause::Expired))
    {
        assembler
            .finish_expired_replay(&manifest, last_output_seq)
            .unwrap()
    } else {
        assembler.finish(&manifest).unwrap()
    };
    let files = verified
        .iter()
        .zip(bytes)
        .map(|(file, bytes)| file.is_returned().then_some(bytes))
        .collect();
    (files, manifest, seqs)
}

fn chunked_request(f: &Fixture, command: &str, files: &[&str]) -> (Run, Vec<Vec<u8>>) {
    let (mut request, bytes) = f.run(command);
    request.return_paths = files
        .iter()
        .map(|file| format!("{}/{file}", f.key))
        .collect();
    request.return_max_bytes = RETURN_CHUNKED_MAX_BYTES_CAP;
    request.return_chunked = true;
    (request, bytes)
}

#[tokio::test]
async fn return_chunked_430_kb_file_round_trips_through_replay_and_restart() {
    let f = Fixture::new();
    let config = f.config();
    let runner = Runner::open(config.clone()).unwrap();
    let (request, bytes) = chunked_request(
        &f,
        "head -c 440320 /dev/urandom > generated.bin; printf output; exit 3",
        &["generated.bin"],
    );
    f.submit(&runner, &request, &bytes);
    let t = terminal(&runner, request.job_id).await;
    assert_eq!(t.outcome, Outcome::Exit { code: 3 });
    base_done(&runner).await;
    let on_disk = fs::read(Path::new(&f.key).join("generated.bin")).unwrap();
    assert_eq!(on_disk.len(), 440_320);
    let frames = runner.replay(request.job_id, 0).unwrap();
    let (files, manifest, seqs) = assemble(&frames, &request);
    assert_eq!(files, vec![Some(on_disk.clone())]);
    assert_eq!(
        manifest.files[0].sha256(),
        Some(format!("{:x}", Sha256::digest(&on_disk)).as_str())
    );
    assert_eq!(manifest.files[0].path(), request.return_paths[0]);
    // Chunks, then the manifest, then the terminal record, in consecutive seqs.
    let n = frames.len();
    assert!(matches!(&frames[n - 2], Frame::ReturnManifest(_)));
    assert!(matches!(&frames[n - 1], Frame::Terminal(_)));
    assert!(matches!(&frames[n - 3], Frame::ReturnChunk(_)));
    assert_eq!(manifest.seq, seqs.last().unwrap() + 1);
    assert!(seqs.windows(2).all(|pair| pair[1] == pair[0] + 1));
    assert_eq!(
        runner.replay(request.job_id, manifest.seq).unwrap(),
        frames[n - 2..]
    );
    // The bytes are kept beside the record, not inside it.
    let record = fs::read(runner.record_path(request.job_id)).unwrap();
    assert!(
        record.len() < 64 * 1024,
        "record holds {} bytes",
        record.len()
    );
    drop(runner);
    let runner = reopen(config);
    assert_eq!(runner.replay(request.job_id, 0).unwrap(), frames);
}

#[tokio::test]
async fn return_chunked_replay_after_one_hour_skips_chunks_and_leaves_a_seq_gap() {
    let f = Fixture::new();
    let config = f.config();
    let runner = Runner::open(config.clone()).unwrap();
    let (request, bytes) = chunked_request(
        &f,
        "head -c 614400 /dev/urandom > kept.bin; printf output",
        &["kept.bin", "missing.bin"],
    );
    f.submit(&runner, &request, &bytes);
    terminal(&runner, request.job_id).await;
    base_done(&runner).await;
    let finished = runner.core.inner.lock().unwrap().jobs[&request.job_id]
        .finished_ms
        .unwrap();
    let directory = {
        let inner = runner.core.inner.lock().unwrap();
        let returned = inner.jobs[&request.job_id].returned.clone().unwrap();
        runner
            .record_path(request.job_id)
            .parent()
            .unwrap()
            .join(returned.directory)
    };

    f.clock
        .store(finished + RETURN_RETENTION_MS - 1, Ordering::SeqCst);
    runner.maintenance().unwrap();
    assert!(directory.exists());
    let retained = runner.replay(request.job_id, 0).unwrap();
    let (files, kept_manifest, seqs) = assemble(&retained, &request);
    assert_eq!(seqs.len(), 2, "600 KiB is two chunks");
    assert_eq!(files[0].as_ref().map(Vec::len), Some(614_400));
    assert_eq!(files[1], None);

    f.clock
        .store(finished + RETURN_RETENTION_MS, Ordering::SeqCst);
    let expired = runner.replay(request.job_id, 0).unwrap();
    let check_expired = |frames: &[Frame]| {
        assert!(!frames.iter().any(|f| matches!(f, Frame::ReturnChunk(_))));
        let (files, manifest, _) = assemble(frames, &request);
        assert_eq!(files, vec![None, None]);
        // Same seq as before; the chunks' seqs are a gap just before it.
        assert_eq!(manifest.seq, kept_manifest.seq);
        let position = frames
            .iter()
            .position(|f| matches!(f, Frame::ReturnManifest(_)))
            .unwrap();
        let Frame::Output(before) = &frames[position - 1] else {
            panic!("output precedes the manifest: {frames:?}")
        };
        assert_eq!(before.seq + 1, seqs[0], "the gap is exactly the chunks");
        assert_eq!(manifest.files[0].chunks(), Some(2));
        assert_eq!(manifest.files[1].chunks(), Some(0));
        let count: u64 = manifest
            .files
            .iter()
            .map(|entry| u64::from(entry.chunks().unwrap()))
            .sum();
        assert_eq!(manifest.seq - before.seq - 1, count);
        assert_eq!(
            manifest.files[0].cause(),
            Some(&ReturnPathsFailureCause::Expired)
        );
        assert_eq!(manifest.files[0].size(), Some(614_400));
        assert_eq!(
            manifest.files[1].cause(),
            Some(&ReturnPathsFailureCause::Missing)
        );
        assert_eq!(manifest.total_bytes, 0);
        assert_eq!(manifest.cap_bytes, kept_manifest.cap_bytes);
        assert!(
            matches!(frames.last(), Some(Frame::Terminal(t)) if t.outcome == Outcome::Exit { code: 0 })
        );
    };
    check_expired(&expired);
    // Maintenance deletes the bytes and records the expiry; the job record
    // itself stays for the 24-hour history.
    runner.maintenance().unwrap();
    assert!(!directory.exists());
    assert!(runner.record_path(request.job_id).exists());
    assert_eq!(runner.replay(request.job_id, 0).unwrap(), expired);
    drop(runner);
    let runner = reopen(config);
    check_expired(&runner.replay(request.job_id, 0).unwrap());
}

#[tokio::test]
async fn return_paths_without_chunked_keep_the_single_return_result() {
    let f = Fixture::new();
    let runner = Runner::open(f.config()).unwrap();
    let (mut request, bytes) = f.run("printf x > returned");
    request.return_paths = vec![format!("{}/returned", f.key)];
    let wire = serde_json::to_value(Frame::Run(request.clone())).unwrap();
    assert!(wire.get("return_chunked").is_none());
    f.submit(&runner, &request, &bytes);
    terminal(&runner, request.job_id).await;
    let frames = runner.replay(request.job_id, 0).unwrap();
    assert!(!frames
        .iter()
        .any(|f| matches!(f, Frame::ReturnChunk(_) | Frame::ReturnManifest(_))));
    assert!(
        matches!(&frames[frames.len() - 2], Frame::ReturnResult(r) if r.returned_files.as_ref().unwrap()[0].data == "eA==")
    );
    let record: serde_json::Value =
        serde_json::from_slice(&fs::read(runner.record_path(request.job_id)).unwrap()).unwrap();
    assert!(record.get("returned").is_none());
    assert!(record["request"].get("return_chunked").is_none());
    assert!(
        !fs::read_dir(runner.record_path(request.job_id).parent().unwrap())
            .unwrap()
            .any(|entry| entry
                .unwrap()
                .file_name()
                .to_string_lossy()
                .starts_with("returned-"))
    );
}

#[test]
fn return_chunked_cap_above_ten_mib_and_unchunked_above_256_kib_are_refused() {
    let f = Fixture::new();
    let runner = Runner::open(f.config()).unwrap();
    for (chunked, cap) in [
        (true, RETURN_CHUNKED_MAX_BYTES_CAP + 1),
        (false, RETURN_MAX_BYTES + 1),
    ] {
        let (mut request, _) = f.run("touch should-not-run");
        request.return_paths = vec![format!("{}/out", f.key)];
        request.return_chunked = chunked;
        request.return_max_bytes = cap;
        assert!(matches!(
            runner.accept(request.clone()).unwrap(),
            Admission::Refused(RefusalReason::RequestRejected)
        ));
        let inner = runner.core.inner.lock().unwrap();
        let terminal = inner.jobs[&request.job_id].terminal.as_ref().unwrap();
        assert_eq!(terminal.ran, Some(Ran::None));
        let limit = if chunked {
            RETURN_CHUNKED_MAX_BYTES_CAP
        } else {
            RETURN_MAX_BYTES
        };
        assert!(terminal
            .refusal_detail
            .as_ref()
            .unwrap()
            .contains(&format!("max_bytes={limit}")));
    }
}

fn large_config(f: &Fixture) -> Config {
    let mut config = f.config();
    config.manual_tick = true;
    config.base_commands.clear();
    config.return_chunked_max_bytes = Some(RETURN_LARGE_MAX_BYTES_CAP);
    config.return_budget_bytes = Some(Config::return_charge(RETURN_LARGE_MAX_BYTES_CAP));
    config.free_space_probe = Arc::new(|_| Ok(u64::MAX));
    config
}

#[test]
fn return_large_configuration_and_runner_ceiling_are_explicit() {
    let f = Fixture::new();
    let mut config = f.config();
    assert_eq!(config.return_chunked_max_bytes, None);
    assert_eq!(config.return_maximum(), 10_485_760);
    config.return_chunked_max_bytes = Some(134_217_728);
    assert!(
        config.check_options().is_err(),
        "large returns need a budget"
    );
    config.return_budget_bytes = Some(Config::return_charge(134_217_728));
    assert!(config.check_options().is_ok());
    let runner = Runner::open(config).unwrap();
    let (mut request, _) = f.run("touch should-not-run");
    request.return_chunked = true;
    request.return_max_bytes = 134_217_729;
    assert!(matches!(
        runner.accept(request.clone()).unwrap(),
        Admission::Refused(RefusalReason::RequestRejected)
    ));
    let inner = runner.core.inner.lock().unwrap();
    let terminal = inner.jobs[&request.job_id].terminal.as_ref().unwrap();
    assert_eq!(terminal.ran, Some(Ran::None));
    assert!(terminal
        .refusal_detail
        .as_deref()
        .unwrap()
        .contains("max_bytes=134217728"));
}

#[tokio::test]
async fn return_budget_free_space_floor_waits_and_queue_wait_expires() {
    let f = Fixture::new();
    let mut config = large_config(&f);
    config.free_space_floor_bytes = 4096;
    let free = Arc::new(AtomicU64::new(Config::return_charge(134_217_728) + 4095));
    let probe = free.clone();
    config.free_space_probe = Arc::new(move |_| Ok(probe.load(Ordering::SeqCst)));
    let runner = Runner::open(config).unwrap();
    let (mut request, bytes) = chunked_request(&f, "printf x > one", &["one"]);
    request.return_max_bytes = 134_217_728;
    request.queue_wait_limit_s = 1;
    f.submit(&runner, &request, &bytes);
    runner.tick().unwrap();
    assert!(runner.core.inner.lock().unwrap().jobs[&request.job_id]
        .setup_ms
        .is_none());
    f.clock.fetch_add(1001, Ordering::SeqCst);
    runner.tick().unwrap();
    assert_eq!(
        runner.withdraw(request.job_id).unwrap(),
        WithdrawOutcome::Withdrawn
    );
    let refused = terminal(&runner, request.job_id).await;
    assert!(matches!(
        refused.outcome,
        Outcome::RefusedBeforeStart {
            reason: RefusalReason::QueueWaitExceeded
        }
    ));
}

fn verify_large_pages(runner: &Runner, request: &Run, expected: u64) -> usize {
    let mut assembler = Some(ReturnAssembler::new(
        &request.return_paths,
        request.return_max_bytes,
    ));
    let mut cursor = 0;
    let mut pages = 0;
    let mut total = 0;
    loop {
        let page = runner.replay(request.job_id, cursor).unwrap();
        assert!(!page.is_empty());
        pages += 1;
        let mut terminal = false;
        for frame in page {
            match frame {
                Frame::Started { seq, .. } => cursor = seq + 1,
                Frame::Output(output) => cursor = output.seq + 1,
                Frame::ReturnChunk(chunk) => {
                    let bytes = assembler.as_mut().unwrap().accept_chunk(&chunk).unwrap();
                    assert!(bytes.iter().all(|byte| *byte == 0));
                    total += bytes.len() as u64;
                    cursor = chunk.seq + 1;
                }
                Frame::ReturnManifest(manifest) => {
                    assert_eq!(manifest.total_bytes, expected);
                    assert!(assembler
                        .take()
                        .unwrap()
                        .finish(&manifest)
                        .unwrap()
                        .iter()
                        .all(|file| file.is_returned()));
                    cursor = manifest.seq + 1;
                }
                Frame::Terminal(t) => {
                    assert_eq!(t.outcome, Outcome::Exit { code: 0 });
                    terminal = true;
                }
                _ => {}
            }
        }
        if terminal {
            break;
        }
    }
    assert_eq!(total, expected);
    pages
}

#[tokio::test]
async fn large_returns_ninety_mb_and_exact_ceiling_replay_across_pages_and_restart() {
    for sizes in [[45_000_000u64, 45_000_000], [67_108_864, 67_108_864]] {
        let f = Fixture::new();
        let config = large_config(&f);
        let runner = Runner::open(config.clone()).unwrap();
        let command = format!(
            "head -c {} /dev/zero > one; head -c {} /dev/zero > two",
            sizes[0], sizes[1]
        );
        let (mut request, bytes) = chunked_request(&f, &command, &["one", "two"]);
        request.return_max_bytes = RETURN_LARGE_MAX_BYTES_CAP;
        f.submit(&runner, &request, &bytes);
        runner.tick().unwrap();
        terminal(&runner, request.job_id).await;
        assert!(verify_large_pages(&runner, &request, sizes.iter().sum()) > 64);
        drop(runner);
        let runner = reopen(config);
        assert!(verify_large_pages(&runner, &request, sizes.iter().sum()) > 64);
    }
}

#[tokio::test]
async fn return_reservation_waits_for_deletion_and_survives_restart_and_replay_lease() {
    let f = Fixture::new();
    let config = large_config(&f);
    let runner = Runner::open(config.clone()).unwrap();
    let (mut first, bytes) = chunked_request(&f, "printf x > one", &["one"]);
    first.return_max_bytes = RETURN_LARGE_MAX_BYTES_CAP;
    f.submit(&runner, &first, &bytes);
    runner.tick().unwrap();
    terminal(&runner, first.job_id).await;
    assert_eq!(
        runner.core.inner.lock().unwrap().jobs[&first.job_id].return_reserved_bytes,
        Config::return_charge(RETURN_LARGE_MAX_BYTES_CAP)
    );
    drop(runner);
    let runner = reopen(config);
    let (mut second, bytes) = chunked_request(&f, "printf y > two", &["two"]);
    second.return_max_bytes = RETURN_LARGE_MAX_BYTES_CAP;
    second.queue_wait_limit_s = 7200;
    f.submit(&runner, &second, &bytes);
    runner.tick().unwrap();
    assert!(runner.core.inner.lock().unwrap().jobs[&second.job_id]
        .setup_ms
        .is_none());
    let log = runner.job_log(first.job_id);
    let lease = log.lease(Vec::new());
    let finished = runner.core.inner.lock().unwrap().jobs[&first.job_id]
        .finished_ms
        .unwrap();
    f.clock
        .store(finished + RETURN_RETENTION_MS, Ordering::SeqCst);
    runner.maintenance().unwrap();
    assert_ne!(
        runner.core.inner.lock().unwrap().jobs[&first.job_id].return_reserved_bytes,
        0
    );
    drop(lease);
    runner.maintenance().unwrap();
    assert_eq!(
        runner.core.inner.lock().unwrap().jobs[&first.job_id].return_reserved_bytes,
        0
    );
    runner.tick().unwrap();
    terminal(&runner, second.job_id).await;
}

#[tokio::test]
async fn return_empty_terminal_collection_releases_its_reservation_without_retention_wait() {
    let f = Fixture::new();
    let runner = Runner::open(large_config(&f)).unwrap();
    let (mut request, bytes) = chunked_request(&f, "true", &["missing"]);
    request.return_max_bytes = RETURN_LARGE_MAX_BYTES_CAP;
    f.submit(&runner, &request, &bytes);
    runner.tick().unwrap();
    terminal(&runner, request.job_id).await;
    tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            if runner.core.inner.lock().unwrap().jobs[&request.job_id].return_reserved_bytes == 0 {
                break;
            }
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("empty terminal return reservation was not released at completion");
    let stored: Job = read_json(&runner.record_path(request.job_id)).unwrap();
    assert_eq!(stored.return_reserved_bytes, 0);
    let returned = stored.returned.as_ref().unwrap();
    assert!(
        !returned.expired,
        "metadata-only failures must remain replayable"
    );
    assert!(returned.chunks.is_empty());
    assert_eq!(
        returned.manifest.files[0].cause(),
        Some(&ReturnPathsFailureCause::Missing)
    );
    let directory = runner
        .record_path(request.job_id)
        .parent()
        .unwrap()
        .to_owned();
    assert!(!directory.join(&returned.directory).exists());
    assert!(runner.replay(request.job_id, 0).unwrap().iter().any(|frame| matches!(frame, Frame::ReturnManifest(manifest) if manifest.files[0].cause() == Some(&ReturnPathsFailureCause::Missing))));
}

#[test]
fn return_empty_reservation_waits_for_orphan_deletion_but_not_metadata_replay() {
    let f = Fixture::new();
    let runner = Runner::open(large_config(&f)).unwrap();
    let (mut request, _) = chunked_request(&f, "true", &["missing"]);
    request.return_max_bytes = RETURN_LARGE_MAX_BYTES_CAP;
    let id = request.job_id;
    let directory = runner.record_path(id).parent().unwrap().to_owned();
    fs::create_dir_all(&directory).unwrap();
    let crate::return_paths::Collection::Chunked(collection) =
        crate::return_paths::unread(&request, &directory)
    else {
        panic!("expected chunked collection");
    };
    let empty_directory = directory.join(&collection.directory);
    fs::create_dir(&empty_directory).unwrap();
    let mut job = Job::new(id, Some(request), f.clock.load(Ordering::SeqCst), 0, 4);
    job.returned = Some(collection.commit(id, &mut job.next_seq));
    job.return_reserved_bytes = Config::return_charge(RETURN_LARGE_MAX_BYTES_CAP);
    job.terminal(Outcome::Exit { code: 0 }, f.clock.load(Ordering::SeqCst));
    runner.save(&job).unwrap();
    runner.core.inner.lock().unwrap().jobs.insert(id, job);
    let blocker = directory.join("returned-blocker");
    fs::write(&blocker, "unindexed payload").unwrap();
    let log = runner.job_log(id);
    let _metadata_reader = log.lease(Vec::new());
    runner.release_return_reservations();
    assert!(
        !empty_directory.exists(),
        "empty referenced directories must be removed immediately"
    );
    assert!(
        blocker.exists(),
        "a file cannot be removed by directory cleanup"
    );
    assert_ne!(
        runner.core.inner.lock().unwrap().jobs[&id].return_reserved_bytes,
        0
    );
    fs::remove_file(blocker).unwrap();
    runner.release_return_reservations();
    assert_eq!(
        runner.core.inner.lock().unwrap().jobs[&id].return_reserved_bytes,
        0
    );
    let stored: Job = read_json(&runner.record_path(id)).unwrap();
    assert_eq!(stored.return_reserved_bytes, 0);
    assert!(!stored.returned.unwrap().expired);
}
