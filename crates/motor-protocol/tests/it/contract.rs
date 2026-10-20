use motor_protocol::*;
use rand::{rngs::StdRng, Rng, SeedableRng};
use serde_json::{json, Value};
use sha2::{Digest, Sha256};
use std::{collections::BTreeSet, fs, io, io::Read, path::PathBuf};

const FRAME_CASES: &[&str] = &[
    "hello",
    "hello-started",
    "hello-default-branch-tip",
    "hello-gate-jobs",
    "run-gate-job",
    "have",
    "have_reply",
    "have-git-dependencies",
    "have_reply-git-dependencies",
    "run-git-dependencies",
    "run-git-dependencies-two-commits",
    "prepare-git-dependencies",
    "run-new-client-old-runner",
    "run-old-client-new-runner",
    "prepare-new-client-old-runner",
    "prepare-old-client-new-runner",
    "run-git-dependencies-transcript",
    "prepare-git-dependencies-transcript",
    "run",
    "run-network-outbound",
    "run-workspace-kind-head",
    "run-workspace-kind-task",
    "run-queue-wait-override",
    "accepted",
    "accepted-network-outbound",
    "started",
    "run-started-transcript",
    "attach-before-started",
    "attach-after-started",
    "refusal",
    "refusal-draining-retry",
    "refusal-disk-full",
    "job-status-queued",
    "job-status-setup",
    "withdraw-stalled",
    "refusal-prepare-bundle",
    "refusal-prepare-setup",
    "refusal-prepare-setup-detail",
    "refusal-prepare-request-rejected-detail",
    "bundle_chunk",
    "bundle_chunk-sibling",
    "output",
    "output-stderr",
    "output-truncated",
    "heartbeat",
    "heartbeat-queue-entered",
    "heartbeat-queued",
    "run-queue-state-transcript",
    "withdraw",
    "withdraw_reply",
    "withdraw_reply-started",
    "attach",
    "terminal",
    "prepare",
    "prepare-workspace-kind-head",
    "prepare-workspace-kind-task",
    "prepare_accepted",
    "prepared",
    "drop",
    "drop_reply",
    "status",
    "resume",
    "resume_reply",
    "status_reply",
    "status-health-absent",
    "status-health-repository-usage",
    "cancel",
    "cancel-unknown",
    "cancel_reply",
    "run-two-bundle-transcript",
    "prepare-two-bundle-prepared",
    "prepare-two-bundle-bundle-rejected",
    "attach-transfer-interrupted",
    "attach-truncated-replay",
    "withdraw-spill",
    "status-health-spill-withdrawals",
];
const OUTCOME_CASES: &[&str] = &[
    "request-network-outbound",
    "network_unsupported",
    "accepted-no-network",
    "started-then-output",
    "exit",
    "exit-nonzero",
    "signal",
    "cancelled",
    "outcome_unknown",
    "outcome_unknown-queued",
    "history_expired",
    "killed-deadline",
    "killed-cancel",
    "pipestatus",
    "unreachable",
    "runner_version_mismatch",
    "snapshot_failed",
    "workspace_key_rejected",
    "runner_full",
    "queue_wait_exceeded",
    "transfer_interrupted",
    "tree_hash_mismatch",
    "bundle_rejected",
    "workspace_setup_failed",
    "runner_draining",
    "runner_disk_full",
    "runner_disk_full-detail",
    "runner_draining-detail",
    "workspace_key_rejected-detail",
    "snapshot_failed-detail",
    "transfer_stalled",
    "queue_wait_exceeded-detail",
    "workspace_setup_failed-detail",
    "transfer_interrupted-stalled-detail",
    "runner_full-slot-busy-detail",
    "workspace_setup_failed-start-not-persisted-detail",
    "workspace_setup_failed-output-unavailable-detail",
    "lost_to_restart-detail",
    "request_rejected-direct-timeout-detail",
    "runner_authentication_failed-detail",
    "queue_wait_exceeded-spill-detail",
];
const REPLY_CASES: &[&str] = &[
    "prepare-prepared",
    "prepare-runner-draining-detail",
    "prepare-workspace-key-rejected-detail",
    "prepare-unreachable",
    "prepare-runner_version_mismatch",
    "prepare-snapshot_failed",
    "prepare-workspace_key_rejected",
    "prepare-transfer_interrupted",
    "prepare-bundle_rejected",
    "prepare-workspace_setup_failed",
    "prepare-workspace_setup_failed-detail",
    "prepare-request_rejected-detail",
    "prepare-runner_authentication_failed-detail",
    "drop-existing",
    "drop-missing",
    "status",
    "status-unreachable",
    "status-cold",
    "cancel",
];

fn root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../test-vectors/exec-remote-v1")
}

fn vector(folder: &str, name: &str) -> Value {
    serde_json::from_slice(&fs::read(root().join(folder).join(format!("{name}.json"))).unwrap())
        .unwrap()
}

fn frames(value: &Value) -> Vec<Frame> {
    let values = value
        .as_array()
        .cloned()
        .unwrap_or_else(|| vec![value.clone()]);
    values
        .into_iter()
        .map(|v| serde_json::from_value(v).unwrap())
        .collect()
}

fn round_trip<T>(value: &Value)
where
    T: serde::de::DeserializeOwned + serde::Serialize,
{
    let decoded: T = serde_json::from_value(value.clone()).unwrap();
    assert_eq!(serde_json::to_value(decoded).unwrap(), *value);
}

#[test]
fn workspace_kind_is_optional_and_old_frames_keep_task_lifetime() {
    for (name, expected) in [
        ("run", None),
        ("run-workspace-kind-head", Some(WorkspaceKind::Head)),
        ("run-workspace-kind-task", Some(WorkspaceKind::Task)),
    ] {
        let Frame::Run(run) = frames(&vector("frames", name)).remove(0) else {
            panic!("run vector")
        };
        assert_eq!(run.workspace_kind, expected);
        if expected.is_none() {
            assert_eq!(run.workspace_kind.unwrap_or_default(), WorkspaceKind::Task);
        }
    }
    for (name, expected) in [
        ("prepare", None),
        ("prepare-workspace-kind-head", Some(WorkspaceKind::Head)),
        ("prepare-workspace-kind-task", Some(WorkspaceKind::Task)),
    ] {
        let Frame::Prepare(prepare) = frames(&vector("frames", name)).remove(0) else {
            panic!("prepare vector")
        };
        assert_eq!(prepare.workspace_kind, expected);
    }
}

#[test]
fn golden_vectors_are_complete_and_canonical() {
    let mut count = 0;
    for (folder, cases) in [
        ("frames", FRAME_CASES),
        ("outcomes", OUTCOME_CASES),
        ("replies", REPLY_CASES),
    ] {
        let expected: BTreeSet<_> = cases.iter().map(|s| s.to_string()).collect();
        let actual: BTreeSet<_> = fs::read_dir(root().join(folder))
            .unwrap()
            .map(|p| p.unwrap().path())
            .filter(|p| p.extension().is_some_and(|ext| ext == "json"))
            .map(|p| p.file_stem().unwrap().to_str().unwrap().to_owned())
            .collect();
        assert_eq!(actual, expected, "missing or unchecked {folder} cases");
        for name in cases {
            let value = vector(folder, name);
            let stem = root().join(folder).join(name);
            let canonical = serde_json_canonicalizer::to_vec(&value).unwrap();
            assert_eq!(
                fs::read(stem.with_extension("jcs")).unwrap(),
                canonical,
                "{folder}/{name} JCS"
            );
            let hash = format!("{:x}", Sha256::digest(&canonical));
            assert_eq!(
                fs::read_to_string(stem.with_extension("sha256")).unwrap(),
                hash,
                "{folder}/{name} SHA-256"
            );
            match folder {
                "frames" => {
                    for frame in value
                        .as_array()
                        .cloned()
                        .unwrap_or_else(|| vec![value.clone()])
                    {
                        round_trip::<Frame>(&frame);
                    }
                }
                "outcomes" => {
                    round_trip::<RunRequest>(&value["request"]);
                    for record in value["stream"].as_array().unwrap() {
                        round_trip::<StreamRecord>(record);
                    }
                }
                "replies" if name.starts_with("prepare-") => round_trip::<PrepareReply>(&value),
                "replies" if name.starts_with("drop-") => round_trip::<DropReply>(&value),
                "replies" if name.starts_with("status") => round_trip::<StatusReply>(&value),
                "replies" => round_trip::<CancelReply>(&value),
                _ => unreachable!(),
            }
            count += 1;
        }
    }
    assert_eq!(count, 136);
}

#[test]
fn network_vectors_pin_requests_refusals_and_offline_acceptance() {
    let outbound = vector("outcomes", "request-network-outbound");
    let request: RunRequest = serde_json::from_value(outbound["request"].clone()).unwrap();
    assert_eq!(request.network, Some(Network::Outbound));
    round_trip::<RunRequest>(&outbound["request"]);

    let refused = vector("outcomes", "network_unsupported");
    let StreamRecord::Terminal(terminal) =
        serde_json::from_value::<StreamRecord>(refused["stream"][0].clone()).unwrap()
    else {
        panic!("network refusal must be terminal");
    };
    assert_eq!(refused["stream"].as_array().unwrap().len(), 1);
    assert_eq!(
        terminal.outcome,
        Outcome::RefusedBeforeStart {
            reason: RefusalReason::NetworkUnsupported,
        }
    );
    assert_eq!(terminal.ran, Some(Ran::None));
    assert_eq!(terminal.bundle_bytes, 0);
    assert_eq!(terminal.tree_hash, None);
    assert_eq!(terminal.retry_after_ms(), None);
    assert_eq!(
        terminal.refusal_detail(),
        Some("outbound network for remote jobs is not available yet; run without net")
    );

    let offline = vector("outcomes", "accepted-no-network");
    let request: RunRequest = serde_json::from_value(offline["request"].clone()).unwrap();
    assert_eq!(request.network, None);
    let StreamRecord::Accepted(accepted) =
        serde_json::from_value::<StreamRecord>(offline["stream"][0].clone()).unwrap()
    else {
        panic!("offline acceptance vector");
    };
    assert_eq!(accepted.network, None);
    round_trip::<RunRequest>(&offline["request"]);
    round_trip::<StreamRecord>(&offline["stream"][0]);

    let mut future = outbound["request"].clone();
    future["network"] = json!("x");
    let request: RunRequest = serde_json::from_value(future.clone()).unwrap();
    assert_eq!(request.network, Some(Network::Unknown("x".into())));
    round_trip::<RunRequest>(&future);
}

fn add_unknown_fields(value: &mut Value) {
    match value {
        Value::Object(object) => {
            // These fields are open maps, not typed objects; don't add a fake key.
            for (key, child) in object.iter_mut() {
                match key.as_str() {
                    "env" | "refusals" | "jobs_by_vcpu_size" => {}
                    "repository_usage" => {
                        if let Value::Object(repositories) = child {
                            for usage in repositories.values_mut() {
                                add_unknown_fields(usage);
                            }
                        }
                    }
                    _ => add_unknown_fields(child),
                }
            }
            object.insert("future_extension".into(), json!({"version": 2}));
        }
        Value::Array(array) => array.iter_mut().for_each(add_unknown_fields),
        _ => {}
    }
}

fn ignores_unknown<T>(original: Value)
where
    T: serde::de::DeserializeOwned + serde::Serialize,
{
    let mut extended = original.clone();
    add_unknown_fields(&mut extended);
    let decoded: T = serde_json::from_value(extended).unwrap();
    assert_eq!(serde_json::to_value(decoded).unwrap(), original);
}

#[test]
fn all_frame_and_reply_types_ignore_unknown_fields() {
    for name in FRAME_CASES {
        let value = vector("frames", name);
        for frame in value.as_array().cloned().unwrap_or_else(|| vec![value]) {
            ignores_unknown::<Frame>(frame);
        }
    }
    for name in OUTCOME_CASES {
        let value = vector("outcomes", name);
        ignores_unknown::<RunRequest>(value["request"].clone());
        for record in value["stream"].as_array().unwrap() {
            ignores_unknown::<StreamRecord>(record.clone());
        }
    }
    for name in REPLY_CASES {
        let value = vector("replies", name);
        if name.starts_with("prepare-") {
            ignores_unknown::<PrepareReply>(value);
        } else if name.starts_with("drop-") {
            ignores_unknown::<DropReply>(value);
        } else if name.starts_with("status") {
            ignores_unknown::<StatusReply>(value);
        } else {
            ignores_unknown::<CancelReply>(value);
        }
    }
}

#[test]
fn outcome_vectors_pin_before_start_proofs_and_nullability() {
    for name in OUTCOME_CASES {
        let value = vector("outcomes", name);
        let stream: Vec<StreamRecord> = serde_json::from_value(value["stream"].clone()).unwrap();
        assert_eq!(
            stream
                .iter()
                .filter(|f| matches!(f, StreamRecord::Terminal(_)))
                .count(),
            1
        );
        let StreamRecord::Terminal(record) = stream.last().unwrap() else {
            panic!("terminal must be last")
        };
        match &record.outcome {
            Outcome::RefusedBeforeStart { reason } => {
                let tag = if *name == "request-network-outbound" {
                    "network_unsupported"
                } else {
                    match *name {
                        "request_rejected-direct-timeout-detail" => "request_rejected",
                        "transfer_interrupted-stalled-detail" => "transfer_interrupted",
                        "runner_full-slot-busy-detail" => "runner_full",
                        "queue_wait_exceeded-spill-detail" => "queue_wait_exceeded",
                        "workspace_setup_failed-start-not-persisted-detail"
                        | "workspace_setup_failed-output-unavailable-detail" => {
                            "workspace_setup_failed"
                        }
                        _ => name.strip_suffix("-detail").unwrap_or(name),
                    }
                };
                assert_eq!(serde_json::to_value(reason).unwrap(), tag);
                assert_eq!(record.ran, Some(Ran::None));
                assert_eq!(record.workspace_changes, None);
                assert_eq!(
                    record.tree_hash.is_none(),
                    matches!(tag, "workspace_key_rejected" | "network_unsupported")
                        || matches!(
                            *name,
                            "request_rejected-direct-timeout-detail"
                                | "runner_authentication_failed-detail"
                        )
                );
                let accepted = matches!(stream.first(), Some(StreamRecord::Accepted(_)));
                assert_eq!(
                    accepted,
                    matches!(
                        *name,
                        "queue_wait_exceeded"
                            | "transfer_interrupted"
                            | "tree_hash_mismatch"
                            | "bundle_rejected"
                            | "workspace_setup_failed"
                            | "transfer_stalled"
                            | "lost_to_restart-detail"
                            | "queue_wait_exceeded-detail"
                            | "queue_wait_exceeded-spill-detail"
                            | "workspace_setup_failed-detail"
                            | "transfer_interrupted-stalled-detail"
                            | "runner_full-slot-busy-detail"
                            | "workspace_setup_failed-start-not-persisted-detail"
                            | "workspace_setup_failed-output-unavailable-detail"
                    )
                );
            }
            Outcome::HistoryExpired => {
                assert_eq!(record.ran, None);
                assert_eq!(record.tree_hash, None);
                assert_eq!(record.workspace_changes, None);
            }
            _ => {
                assert_eq!(
                    record.workspace_changes.is_some(),
                    record.ran == Some(Ran::Remote)
                );
            }
        }
        assert_eq!(
            record.killed,
            match *name {
                "killed-deadline" => Some(Killed::Deadline),
                "killed-cancel" => Some(Killed::Cancel),
                _ => None,
            }
        );
        assert_eq!(
            record.pipestatus,
            if *name == "pipestatus" {
                Some(vec![0, 1])
            } else {
                None
            }
        );
    }
    assert_eq!(
        vector("outcomes", "unreachable")["request"]["env"]["RUST_BACKTRACE"],
        "1"
    );
    assert_eq!(vector("frames", "attach")["from_seq"], 7);
    assert_eq!(
        vector("frames", "output-truncated")["truncated_before_seq"],
        7
    );
    assert_eq!(
        vector("frames", "run-queue-wait-override")["queue_wait_limit_s"],
        0
    );
}

struct FragmentedReader<'a> {
    remaining: &'a [u8],
    rng: StdRng,
}

impl Read for FragmentedReader<'_> {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        if buf.is_empty() || self.remaining.is_empty() {
            return Ok(0);
        }
        let len = self
            .rng
            .gen_range(1..=buf.len().min(self.remaining.len()).min(23));
        buf[..len].copy_from_slice(&self.remaining[..len]);
        self.remaining = &self.remaining[len..];
        Ok(len)
    }
}

fn bundle_transcript(prepare: bool, seed: u64) -> (Vec<Frame>, Vec<Vec<u8>>) {
    let mut rng = StdRng::seed_from_u64(seed);
    let payloads: Vec<Vec<u8>> = [4097, 8193]
        .into_iter()
        .map(|len| (0..len).map(|_| rng.gen()).collect())
        .collect();
    let manifests: Vec<_> = payloads
        .iter()
        .enumerate()
        .map(|(index, data)| BundleManifest {
            index: index as u32,
            repository: if index == 0 {
                "/Users/example/src/prefrontal"
            } else {
                "/Users/example/src/commons"
            }
            .into(),
            length: data.len() as u64,
            sha256: format!("{:x}", Sha256::digest(data)),
        })
        .collect();
    let mut opening = frames(&vector("frames", if prepare { "prepare" } else { "run" })).remove(0);
    match &mut opening {
        Frame::Run(run) => run.bundles = manifests,
        Frame::Prepare(prepare) => prepare.bundles = manifests,
        _ => unreachable!(),
    }
    let mut transcript = vec![
        opening,
        frames(&vector(
            "frames",
            if prepare {
                "prepare_accepted"
            } else {
                "accepted"
            },
        ))
        .remove(0),
    ];
    for (index, payload) in payloads.iter().enumerate() {
        let mut offset = 0;
        while offset < payload.len() {
            let end = (offset + rng.gen_range(1..=511)).min(payload.len());
            transcript.push(Frame::BundleChunk {
                index: index as u32,
                offset: offset as u64,
                data: BytePayload(payload[offset..end].to_vec()),
            });
            transcript.push(Frame::Heartbeat {
                queue_wait_ms: None,
            });
            offset = end;
        }
    }
    transcript.push(
        frames(&vector(
            "frames",
            if prepare { "prepared" } else { "terminal" },
        ))
        .remove(0),
    );
    (transcript, payloads)
}

#[test]
fn bundles_round_trip_run_and_prepare_with_random_chunks_and_heartbeats() {
    for prepare in [false, true] {
        for seed in 0..32 {
            let (transcript, payloads) = bundle_transcript(prepare, seed);
            let mut wire = Vec::new();
            for frame in &transcript {
                write_frame(&mut wire, frame).unwrap();
            }
            let mut reader = FragmentedReader {
                remaining: &wire,
                rng: StdRng::seed_from_u64(seed + 100),
            };
            let mut received = Vec::new();
            while let Some(frame) = read_frame(&mut reader).unwrap() {
                received.push(frame);
            }
            assert_eq!(received, transcript);
            let mut bundles = [Vec::new(), Vec::new()];
            let mut last_index = 0;
            for frame in &received {
                if let Frame::BundleChunk {
                    index,
                    offset,
                    data,
                } = frame
                {
                    assert!(*index >= last_index);
                    last_index = *index;
                    assert_eq!(*offset, bundles[*index as usize].len() as u64);
                    bundles[*index as usize].extend_from_slice(&data.0);
                }
            }
            assert_eq!(bundles.as_slice(), payloads);
            let manifest = match &received[0] {
                Frame::Run(run) => &run.bundles,
                Frame::Prepare(prepare) => &prepare.bundles,
                _ => unreachable!(),
            };
            for (entry, bytes) in manifest.iter().zip(&bundles) {
                assert_eq!(entry.length, bytes.len() as u64);
                assert_eq!(entry.sha256, format!("{:x}", Sha256::digest(bytes)));
            }
        }
    }
}

#[test]
fn codec_pins_big_endian_prefix_and_detects_truncation() {
    let mut wire = Vec::new();
    write_frame(
        &mut wire,
        &Frame::Heartbeat {
            queue_wait_ms: None,
        },
    )
    .unwrap();
    assert_eq!(wire, b"\x00\x00\x00\x14{\"type\":\"heartbeat\"}");
    assert_eq!(
        read_frame(&mut &wire[..]).unwrap(),
        Some(Frame::Heartbeat {
            queue_wait_ms: None
        })
    );
    assert_eq!(read_frame(&mut &[][..]).unwrap(), None);
    for end in 1..wire.len() {
        assert_eq!(
            read_frame(&mut &wire[..end]).unwrap_err().kind(),
            io::ErrorKind::UnexpectedEof
        );
    }
    let hostile = [255u8; 4];
    assert_eq!(
        read_frame(&mut &hostile[..]).unwrap_err().kind(),
        io::ErrorKind::InvalidData
    );
    let empty = [0; 4];
    assert_eq!(
        read_frame(&mut &empty[..]).unwrap_err().kind(),
        io::ErrorKind::InvalidData
    );
}

#[test]
fn byte_payloads_preserve_split_utf8_and_reject_invalid_base64() {
    let output = frames(&vector("frames", "output")).remove(0);
    let Frame::Output(output) = output else {
        panic!("output")
    };
    assert_eq!(output.bytes.0, [0xe2]);
    let output = frames(&vector("frames", "output-stderr")).remove(0);
    let Frame::Output(output) = output else {
        panic!("output")
    };
    assert_eq!(output.bytes.0, [0x82, 0xac]);
    for encoded in ["not base64!", "4g", "_w=="] {
        assert!(serde_json::from_value::<BytePayload>(json!(encoded)).is_err());
    }
    assert!(serde_json::from_value::<Frame>(json!({"type": "future_frame"})).is_err());
}

#[test]
fn oversized_header_is_refused_before_reading_body() {
    struct HeaderOnly(io::Cursor<[u8; 4]>);
    impl Read for HeaderOnly {
        fn read(&mut self, bytes: &mut [u8]) -> io::Result<usize> {
            assert!(self.0.position() < 4, "oversized frame body was read");
            self.0.read(bytes)
        }
    }
    let mut reader = HeaderOnly(io::Cursor::new(u32::MAX.to_be_bytes()));
    assert_eq!(
        read_frame(&mut reader).unwrap_err().kind(),
        io::ErrorKind::InvalidData
    );
    assert_eq!(reader.0.position(), 4);
}

#[tokio::test]
async fn protocol_reader_skips_future_types_and_reads_resume_frames() {
    let future = br#"{"type":"future_control","seq":7,"payload":{"anything":[1,2]}}"#;
    let mut wire = (future.len() as u32).to_be_bytes().to_vec();
    wire.extend(future);
    write_frame(&mut wire, &Frame::Resume).unwrap();
    write_frame(&mut wire, &Frame::ResumeReply { resumed: true }).unwrap();
    let mut reader = &wire[..];
    assert_eq!(read_frame(&mut reader).unwrap(), Some(Frame::Resume));
    assert_eq!(
        read_frame(&mut reader).unwrap(),
        Some(Frame::ResumeReply { resumed: true })
    );
    let mut reader = &wire[..];
    assert_eq!(
        read_frame_async(&mut reader).await.unwrap(),
        Some(Frame::Resume)
    );
    assert_eq!(
        read_frame_async(&mut reader).await.unwrap(),
        Some(Frame::ResumeReply { resumed: true })
    );
}

#[test]
fn tolerant_decoder_uses_supported_tags_and_preserves_future_cursors() {
    let mut cases = Vec::new();
    for name in FRAME_CASES {
        cases.extend(frames(&vector("frames", name)));
    }
    cases.extend([
        Frame::Drain {
            deadline_s: 30,
            exit_after: false,
        },
        Frame::DrainReply {
            timed_out: false,
            restart_recoverable_jobs: None,
        },
        Frame::JobStatus {
            job_id: uuid_for_test(),
        },
    ]);
    let mut tags = BTreeSet::new();
    for frame in cases {
        let bytes = serde_json::to_vec(&frame).unwrap();
        assert_eq!(decode_frame(&bytes).unwrap(), DecodedFrame::Frame(frame));
        let value: Value = serde_json::from_slice(&bytes).unwrap();
        let tag = value["type"].as_str().unwrap();
        tags.insert(tag.to_owned());
        let minimal = serde_json::to_vec(&json!({"type":tag,"seq":7})).unwrap();
        match decode_frame(&minimal) {
            Ok(DecodedFrame::Frame(_)) => {}
            Err(error) => assert_eq!(error.kind(), io::ErrorKind::InvalidData),
            Ok(DecodedFrame::Unknown(_)) => panic!("supported tag {tag} treated as future"),
        }
    }
    assert_eq!(tags.len(), 29);
    for tag in ["resume", "resume_reply"] {
        assert!(tags.contains(tag));
    }
    assert!(decode_frame(br#"{"type":"resume_reply","resumed":"wrong-type","seq":7}"#).is_err());
    for (json, seq) in [
        (
            json!({"type":"future_control","seq":7,"payload":[1,2,3]}),
            Some(7),
        ),
        (
            json!({"type":"future_control","seq":u64::MAX}),
            Some(u64::MAX),
        ),
        (json!({"type":"future_control"}), None),
        (json!({"type":"future_control","seq":"not-a-cursor"}), None),
        (json!({"type":"future_control","seq":-1}), None),
    ] {
        let bytes = serde_json::to_vec(&json).unwrap();
        assert_eq!(decode_frame(&bytes).unwrap(), DecodedFrame::Unknown(seq));
        let mut wire = (bytes.len() as u32).to_be_bytes().to_vec();
        wire.extend(bytes);
        assert_eq!(
            read_record(&mut &wire[..]).unwrap(),
            Some(DecodedFrame::Unknown(seq))
        );
    }
    for bytes in [
        b"{}".as_slice(),
        br#"{"type":7}"#,
        br#"{"type":"future","broken":}"#,
    ] {
        assert_eq!(
            decode_frame(bytes).unwrap_err().kind(),
            io::ErrorKind::InvalidData
        );
    }
}

fn uuid_for_test() -> Uuid {
    serde_json::from_value(json!("018f8f2c-1234-7abc-8000-000000000001")).unwrap()
}

#[tokio::test]
async fn frame_limit_accepts_boundary_and_bundle_chunks_and_rejects_oversized_writes() {
    let frame = Frame::BundleChunk {
        index: u32::MAX,
        offset: u64::MAX,
        data: BytePayload(vec![0xff; 64 * 1024]),
    };
    let bytes = serde_json::to_vec(&frame).unwrap();
    assert_eq!(bytes.len(), 87_466);
    assert!(bytes.len() < MAX_FRAME_BYTES);
    assert_eq!(decode_frame(&bytes).unwrap(), DecodedFrame::Frame(frame));
    let mut body = br#"{"type":"future_control","seq":7}"#.to_vec();
    body.resize(MAX_FRAME_BYTES, b' ');
    let mut wire = (body.len() as u32).to_be_bytes().to_vec();
    wire.extend(&body);
    assert_eq!(
        read_record(&mut &wire[..]).unwrap(),
        Some(DecodedFrame::Unknown(Some(7)))
    );
    assert_eq!(
        read_record_async(&mut &wire[..]).await.unwrap(),
        Some(DecodedFrame::Unknown(Some(7)))
    );
    body.push(b' ');
    assert_eq!(
        decode_frame(&body).unwrap_err().kind(),
        io::ErrorKind::InvalidData
    );
    let oversized = Frame::BundleChunk {
        index: 0,
        offset: 0,
        data: BytePayload(vec![0; MAX_FRAME_BYTES]),
    };
    let mut wire = vec![42];
    assert_eq!(
        write_frame(&mut wire, &oversized).unwrap_err().kind(),
        io::ErrorKind::InvalidData
    );
    assert_eq!(wire, [42]);
    assert_eq!(
        write_frame_async(&mut wire, &oversized)
            .await
            .unwrap_err()
            .kind(),
        io::ErrorKind::InvalidData
    );
    assert_eq!(wire, [42]);
}

#[tokio::test]
async fn asynchronous_oversized_header_is_refused_before_reading_body() {
    struct HeaderOnly(io::Cursor<[u8; 4]>);
    impl tokio::io::AsyncRead for HeaderOnly {
        fn poll_read(
            mut self: std::pin::Pin<&mut Self>,
            _: &mut std::task::Context<'_>,
            buf: &mut tokio::io::ReadBuf<'_>,
        ) -> std::task::Poll<io::Result<()>> {
            assert!(self.0.position() < 4, "oversized frame body was polled");
            let n = self.0.read(buf.initialize_unfilled()).unwrap();
            buf.advance(n);
            std::task::Poll::Ready(Ok(()))
        }
    }
    for length in [MAX_FRAME_BYTES as u32 + 1, u32::MAX] {
        let mut reader = HeaderOnly(io::Cursor::new(length.to_be_bytes()));
        assert_eq!(
            read_record_async(&mut reader).await.unwrap_err().kind(),
            io::ErrorKind::InvalidData
        );
        assert_eq!(reader.0.position(), 4);
    }
}

#[test]
fn request_environment_policy_allows_build_controls_but_not_paths_or_credentials() {
    for name in [
        "RUST_BACKTRACE",
        "RUSTFLAGS",
        "RUSTDOCFLAGS",
        "RUST_TEST_THREADS",
        "CARGO_INCREMENTAL",
        "CARGO_BUILD_JOBS",
        "NEXTEST_PROFILE",
        "CI",
        "NO_COLOR",
        "CLICOLOR",
        "CLICOLOR_FORCE",
    ] {
        assert!(allowed_env(name), "denied {name}");
    }
    for name in [
        "",
        "PATH",
        "HOME",
        "TMPDIR",
        "SSH_AUTH_SOCK",
        "SUBC_LAUNCH_NONCE",
        "CARGO_HOME",
        "CARGO_INSTALL_ROOT",
        "CARGO_TARGET_DIR",
        "CARGO_BUILD_RUSTC",
        "RUSTUP_HOME",
        "cargo_incremental",
    ] {
        assert!(!allowed_env(name), "allowed {name}");
    }
    for prefix in ["RUST_", "CARGO_", "NEXTEST_"] {
        for secret in ["TOKEN", "SECRET", "PASSWORD", "KEY", "CREDENTIAL"] {
            assert!(!allowed_env(&format!("{prefix}{secret}")));
        }
    }
}

#[tokio::test]
async fn async_codec_interoperates_with_sync_and_fragmented_io() {
    use tokio::io::AsyncWriteExt;
    let (transcript, _) = bundle_transcript(true, 31);
    let mut wire = Vec::new();
    for frame in &transcript {
        write_frame(&mut wire, frame).unwrap();
    }
    let (mut writer, mut reader) = tokio::io::duplex(7);
    let task = tokio::spawn(async move {
        for chunk in wire.chunks(3) {
            writer.write_all(chunk).await.unwrap();
        }
        writer.shutdown().await.unwrap();
    });
    let mut decoded = Vec::new();
    while let Some(frame) = read_frame_async(&mut reader).await.unwrap() {
        decoded.push(frame);
    }
    task.await.unwrap();
    assert_eq!(decoded, transcript);
    let mut async_wire = Vec::new();
    for frame in &transcript {
        write_frame_async(&mut async_wire, frame).await.unwrap();
    }
    let mut reader = &async_wire[..];
    for expected in &transcript {
        assert_eq!(read_frame(&mut reader).unwrap().as_ref(), Some(expected));
    }
    assert_eq!(read_frame(&mut reader).unwrap(), None);
    for end in 1..24 {
        let heartbeat = b"\x00\x00\x00\x14{\"type\":\"heartbeat\"}";
        assert_eq!(
            read_frame_async(&mut &heartbeat[..end])
                .await
                .unwrap_err()
                .kind(),
            io::ErrorKind::UnexpectedEof
        );
    }
}

#[test]
fn retry_refusals_remain_never_started_in_old_and_new_callers() {
    for name in [
        "runner_draining",
        "runner_disk_full",
        "queue_wait_exceeded",
        "transfer_stalled",
    ] {
        let value = vector("outcomes", name);
        let terminal = value["stream"].as_array().unwrap().last().unwrap();
        let new: StreamRecord = serde_json::from_value(terminal.clone()).unwrap();
        let old: cortexkit_exec_remote_types::types::StreamRecord =
            serde_json::from_value(terminal.clone()).unwrap();
        assert!(
            matches!(new, StreamRecord::Terminal(ref t) if matches!(t.outcome, Outcome::RefusedBeforeStart { .. }) && t.ran == Some(Ran::None))
        );
        assert!(
            matches!(old, cortexkit_exec_remote_types::types::StreamRecord::Terminal(ref t) if matches!(t.outcome, Outcome::RefusedBeforeStart { .. }) && t.ran == Some(Ran::None))
        );
    }
}

#[test]
fn optional_queue_and_retry_fields_work_in_both_upgrade_orders() {
    #[derive(serde::Deserialize)]
    struct OldRefusal {
        reason: RefusalReason,
    }
    #[derive(serde::Deserialize)]
    struct OldStatus {
        state: String,
    }
    let mut refusal_with_detail = vector("frames", "refusal-draining-retry");
    refusal_with_detail["detail"] = serde_json::json!("runner is draining");
    let old: OldRefusal = serde_json::from_value(refusal_with_detail).unwrap();
    assert_eq!(old.reason, RefusalReason::RunnerDraining);
    let old: OldStatus = serde_json::from_value(vector("frames", "job-status-queued")).unwrap();
    assert_eq!(old.state, "queued");
    let new: Frame = serde_json::from_value(vector("frames", "job-status-setup")).unwrap();
    assert!(matches!(
        new,
        Frame::JobStatusReply(JobStatusReply {
            queue_wait_ms: None,
            ..
        })
    ));
    let new: Frame = serde_json::from_value(vector("frames", "refusal")).unwrap();
    assert!(matches!(
        new,
        Frame::Refusal(Refusal {
            retry_after_ms: None,
            detail: None,
            ..
        })
    ));
}

#[test]
fn queue_heartbeats_are_additive_in_both_upgrade_orders() {
    #[derive(serde::Deserialize, Debug, PartialEq)]
    #[serde(tag = "type", rename_all = "snake_case")]
    enum OldFrame {
        Heartbeat,
    }
    for name in ["heartbeat-queue-entered", "heartbeat-queued"] {
        let value = vector("frames", name);
        assert_eq!(
            serde_json::from_value::<OldFrame>(value.clone()).unwrap(),
            OldFrame::Heartbeat
        );
        assert!(matches!(
            serde_json::from_value::<Frame>(value).unwrap(),
            Frame::Heartbeat {
                queue_wait_ms: Some(_)
            }
        ));
    }
    let value = vector("frames", "heartbeat");
    assert!(matches!(
        serde_json::from_value::<Frame>(value.clone()).unwrap(),
        Frame::Heartbeat {
            queue_wait_ms: None
        }
    ));
    round_trip::<Frame>(&value);
}

#[test]
fn started_vectors_share_output_sequence_and_replay_boundaries() {
    let expected = Frame::Started {
        job_id: Uuid::parse_str("0192a64a-1234-7000-8000-000000000001").unwrap(),
        seq: 0,
        queue_wait_ms: 25,
        started_at_ms: 1_729_468_800_000,
        path_mapping: None,
    };
    assert_eq!(frames(&vector("frames", "started")), vec![expected.clone()]);
    let before = frames(&vector("frames", "attach-before-started"));
    assert_eq!(before[1], expected);
    assert!(matches!(before[2], Frame::Output(ref o) if o.seq == 1));
    let after = frames(&vector("frames", "attach-after-started"));
    assert!(matches!(after[1], Frame::Output(ref o) if o.seq == 1));
    assert!(!after.iter().any(|f| matches!(f, Frame::Started { .. })));
    let caller: Vec<StreamRecord> =
        serde_json::from_value(vector("outcomes", "started-then-output")["stream"].clone())
            .unwrap();
    assert!(matches!(caller[0], StreamRecord::Accepted(_)));
    assert_eq!(
        caller[1],
        StreamRecord::Started(Started::new(0, 25, 1_729_468_800_000))
    );
    assert!(matches!(caller[2], StreamRecord::Output(ref o) if o.seq == 1));
    assert!(matches!(caller[3], StreamRecord::Terminal(_)));
}

#[test]
fn hello_without_started_capability_keeps_legacy_wire_bytes() {
    let legacy = br#"{"type":"hello","protocol_version":1}"#;
    let frame: Frame = serde_json::from_slice(legacy).unwrap();
    assert_eq!(
        frame,
        Frame::Hello {
            protocol_version: 1,
            return_paths: false,
            started: false,
            default_branch_tip: false,
            gate_jobs: false,
            platforms: Vec::new(),
        }
    );
    assert_eq!(serde_json::to_vec(&frame).unwrap(), legacy);
    let capable: Frame = serde_json::from_value(vector("frames", "hello-started")).unwrap();
    assert_eq!(
        capable,
        Frame::Hello {
            protocol_version: 1,
            return_paths: false,
            started: true,
            default_branch_tip: false,
            gate_jobs: false,
            platforms: Vec::new(),
        }
    );
}

#[test]
fn hello_default_branch_tip_capability_is_additive() {
    let capable: Frame =
        serde_json::from_value(vector("frames", "hello-default-branch-tip")).unwrap();
    assert_eq!(
        capable,
        Frame::Hello {
            protocol_version: 1,
            return_paths: false,
            started: true,
            default_branch_tip: true,
            gate_jobs: false,
            platforms: Vec::new(),
        }
    );
    assert_eq!(
        serde_json::to_value(&capable).unwrap(),
        vector("frames", "hello-default-branch-tip")
    );
    #[derive(serde::Deserialize)]
    #[serde(tag = "type", rename_all = "snake_case")]
    enum LegacyFrame {
        Hello {
            protocol_version: u32,
            #[serde(default)]
            started: bool,
        },
    }
    let LegacyFrame::Hello {
        protocol_version,
        started,
    } = serde_json::from_slice(&serde_json::to_vec(&capable).unwrap()).unwrap();
    assert_eq!(protocol_version, 1);
    assert!(started);
}

#[test]
fn gate_capability_and_mark_are_additive_motor_only_fields() {
    let hello: Frame = serde_json::from_value(vector("frames", "hello-gate-jobs")).unwrap();
    assert!(matches!(
        hello,
        Frame::Hello {
            gate_jobs: true,
            ..
        }
    ));
    assert_eq!(
        serde_json::to_value(hello).unwrap(),
        vector("frames", "hello-gate-jobs")
    );
    let Frame::Run(gate) = serde_json::from_value(vector("frames", "run-gate-job")).unwrap() else {
        panic!("Run required")
    };
    assert!(gate.gate_job);
    assert_eq!(gate.queue_wait_limit_s, 1800);
    let Frame::Run(ordinary) = serde_json::from_value(vector("frames", "run")).unwrap() else {
        panic!("Run required")
    };
    assert!(!ordinary.gate_job);
    assert!(serde_json::to_value(ordinary)
        .unwrap()
        .get("gate_job")
        .is_none());
}

#[test]
fn typed_refusal_detail_vectors_preserve_codes_and_before_start_proof() {
    for (name, reason, detail) in [
        (
            "queue_wait_exceeded-detail",
            RefusalReason::QueueWaitExceeded,
            "waiting_on=permits; requested_permits=1; held_permits=0/1",
        ),
        (
            "queue_wait_exceeded-spill-detail",
            RefusalReason::QueueWaitExceeded,
            "spill_withdrawal: waiting_on=permits; requested_permits=1; held_permits=0/1",
        ),
        (
            "workspace_setup_failed-detail",
            RefusalReason::WorkspaceSetupFailed,
            "prepare job workspace: populate workspace: destination path is not a directory",
        ),
        (
            "transfer_interrupted-stalled-detail",
            RefusalReason::TransferInterrupted,
            "transfer_stalled: git fetch made no progress for 60 seconds",
        ),
        (
            "runner_full-slot-busy-detail",
            RefusalReason::RunnerFull,
            "windows_slot_busy",
        ),
        (
            "workspace_setup_failed-start-not-persisted-detail",
            RefusalReason::WorkspaceSetupFailed,
            "execution gate was not opened: start_not_persisted",
        ),
        (
            "workspace_setup_failed-output-unavailable-detail",
            RefusalReason::WorkspaceSetupFailed,
            "execution gate was not opened: output_unavailable",
        ),
    ] {
        let value = vector("outcomes", name);
        let records: Vec<StreamRecord> = serde_json::from_value(value["stream"].clone()).unwrap();
        let Some(StreamRecord::Terminal(terminal)) = records.last() else {
            panic!("missing terminal in {name}")
        };
        assert_eq!(
            terminal.outcome,
            Outcome::RefusedBeforeStart { reason },
            "{name}"
        );
        assert_eq!(terminal.refusal_detail(), Some(detail), "{name}");
        assert_eq!(terminal.ran, Some(Ran::None), "{name}");
        assert_eq!(terminal.retry_after_ms(), None, "{name}");
        assert!(
            !records
                .iter()
                .any(|record| matches!(record, StreamRecord::Started(_) | StreamRecord::Output(_))),
            "{name}"
        );
    }
    let detail = "prepare workspace: populate workspace: destination path is not a directory";
    let Frame::Refusal(refusal) =
        serde_json::from_value(vector("frames", "refusal-prepare-setup-detail")).unwrap()
    else {
        panic!("missing Prepare refusal")
    };
    assert_eq!(refusal.reason, RefusalReason::WorkspaceSetupFailed);
    assert_eq!(refusal.detail.as_deref(), Some(detail));
    assert_eq!(refusal.retry_after_ms, None);
    let reply: PrepareReply =
        serde_json::from_value(vector("replies", "prepare-workspace_setup_failed-detail")).unwrap();
    assert_eq!(
        reply.outcome,
        PrepareOutcome::RefusedBeforeStart {
            reason: RefusalReason::WorkspaceSetupFailed
        }
    );
    assert_eq!(reply.refusal_detail(), Some(detail));
    assert_eq!(reply.retry_after_ms(), None);
}

#[test]
fn grouped_refusal_detail_vectors_keep_specific_human_codes() {
    for (name, reason, detail) in [
        (
            "lost_to_restart-detail",
            RefusalReason::LostToRestart,
            "runner restarted before the job could start",
        ),
        (
            "request_rejected-direct-timeout-detail",
            RefusalReason::RequestRejected,
            "direct_timeout_refused: timeout 101 seconds exceeds the direct-job cap of 100 seconds",
        ),
        (
            "runner_authentication_failed-detail",
            RefusalReason::RunnerAuthenticationFailed,
            "runner_credential_missing",
        ),
    ] {
        let value = vector("outcomes", name);
        let records: Vec<StreamRecord> = serde_json::from_value(value["stream"].clone()).unwrap();
        let Some(StreamRecord::Terminal(t)) = records.last() else {
            panic!("missing terminal in {name}")
        };
        assert_eq!(t.outcome, Outcome::RefusedBeforeStart { reason }, "{name}");
        assert_eq!(t.refusal_detail(), Some(detail), "{name}");
        assert_eq!(t.ran, Some(Ran::None), "{name}");
        assert_eq!(t.retry_after_ms(), None, "{name}");
    }
    for (name, reason, detail) in [
        (
            "prepare-request_rejected-detail",
            RefusalReason::RequestRejected,
            "prepare_unsupported: Prepare is unsupported by the Windows instance",
        ),
        (
            "prepare-runner_authentication_failed-detail",
            RefusalReason::RunnerAuthenticationFailed,
            "runner_credential_missing",
        ),
    ] {
        let reply: PrepareReply = serde_json::from_value(vector("replies", name)).unwrap();
        assert_eq!(reply.outcome, PrepareOutcome::RefusedBeforeStart { reason });
        assert_eq!(reply.refusal_detail(), Some(detail));
        assert_eq!(reply.retry_after_ms(), None);
    }
    let Frame::Refusal(refusal) =
        serde_json::from_value(vector("frames", "refusal-prepare-request-rejected-detail"))
            .unwrap()
    else {
        panic!("missing Prepare refusal")
    };
    assert_eq!(refusal.reason, RefusalReason::RequestRejected);
    assert_eq!(
        refusal.detail.as_deref(),
        Some("prepare_unsupported: Prepare is unsupported by the Windows instance")
    );
    assert_eq!(refusal.retry_after_ms, None);
}

#[test]
fn spill_withdrawal_fields_are_additive_and_omitted_at_default() {
    for (name, spill) in [("withdraw", false), ("withdraw-spill", true)] {
        let value = vector("frames", name);
        let frame: Frame = serde_json::from_value(value.clone()).unwrap();
        assert!(matches!(frame, Frame::Withdraw { spill: actual, .. } if actual == spill));
        assert_eq!(serde_json::to_value(frame).unwrap(), value);
    }
    let Frame::StatusReply(reply) =
        serde_json::from_value(vector("frames", "status-health-spill-withdrawals")).unwrap()
    else {
        panic!("expected health reply");
    };
    let wait = reply.health.unwrap().queue_wait_1h.unwrap();
    assert_eq!(wait.refusals, 4);
    assert_eq!(wait.spill_withdrawals, 65);
    let legacy = serde_json::json!({"p50_ms": 0, "p90_ms": 0, "refusals": 0});
    let wait: HealthQueueWait = serde_json::from_value(legacy.clone()).unwrap();
    assert_eq!(wait.spill_withdrawals, 0);
    assert_eq!(serde_json::to_value(wait).unwrap(), legacy);
}
