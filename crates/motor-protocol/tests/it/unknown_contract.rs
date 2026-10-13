use motor_protocol::*;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use sha2::{Digest, Sha256};
use std::{collections::BTreeSet, fs, path::PathBuf};

const UNKNOWN_OUTCOMES: &[&str] = &[
    "crate-local-unknown-refusal",
    "crate-local-unknown-outcome",
    "crate-local-unknown-stream-record",
    "crate-local-unknown-killed",
    "crate-local-unknown-ran",
    "crate-local-unknown-output-stream",
];
const UNKNOWN_REPLIES: &[&str] = &[
    "crate-local-unknown-prepare-outcome",
    "crate-local-unknown-rebuild-result",
];

fn root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../test-vectors/exec-remote-v1")
}

fn vector(folder: &str, name: &str) -> Value {
    serde_json::from_slice(&fs::read(root().join(folder).join(format!("{name}.jcs"))).unwrap())
        .unwrap()
}

fn record(name: &str) -> StreamRecord {
    serde_json::from_value(vector("outcomes", name)["stream"][0].clone()).unwrap()
}

fn terminal(name: &str) -> TerminalRecord {
    let StreamRecord::Terminal(terminal) = record(name) else {
        panic!("{name} must start with a terminal record")
    };
    terminal
}

#[derive(Serialize, Deserialize)]
struct OutcomeCase {
    request: RunRequest,
    stream: Vec<StreamRecord>,
}

fn round_trip<T: serde::de::DeserializeOwned + Serialize>(bytes: &[u8]) -> Vec<u8> {
    serde_json_canonicalizer::to_vec(&serde_json::from_slice::<T>(bytes).unwrap()).unwrap()
}

#[test]
fn unknown_vector_inventory_hashes_and_typed_round_trips_are_pinned() {
    for (folder, cases) in [("outcomes", UNKNOWN_OUTCOMES), ("replies", UNKNOWN_REPLIES)] {
        let expected: BTreeSet<_> = cases
            .iter()
            .flat_map(|name| [format!("{name}.jcs"), format!("{name}.sha256")])
            .collect();
        let actual: BTreeSet<_> = fs::read_dir(root().join(folder))
            .unwrap()
            .map(|entry| entry.unwrap().file_name().into_string().unwrap())
            .filter(|name| name.starts_with("crate-local-unknown-"))
            .collect();
        assert_eq!(actual, expected, "missing or unchecked {folder} additions");
        for name in cases {
            let stem = root().join(folder).join(name);
            let bytes = fs::read(stem.with_extension("jcs")).unwrap();
            let expected_hash = fs::read_to_string(stem.with_extension("sha256")).unwrap();
            assert_eq!(
                format!("{:x}", Sha256::digest(&bytes)),
                expected_hash,
                "{folder}/{name} SHA-256"
            );
            let canonical = match folder {
                "outcomes" => round_trip::<OutcomeCase>(&bytes),
                "replies" if *name == "crate-local-unknown-prepare-outcome" => {
                    round_trip::<PrepareReply>(&bytes)
                }
                "replies" => round_trip::<StatusReply>(&bytes),
                _ => unreachable!(),
            };
            assert_eq!(canonical, bytes, "{folder}/{name} typed canonical bytes");
        }
    }
    assert_eq!(UNKNOWN_OUTCOMES.len() + UNKNOWN_REPLIES.len(), 8);
}

#[test]
fn sha256sums_covers_every_corpus_file() {
    let manifest = fs::read_to_string(root().join("SHA256SUMS")).unwrap();
    let mut listed = BTreeSet::new();
    for line in manifest.lines() {
        let (hash, path) = line.split_once("  ").unwrap();
        assert!(
            listed.insert(path.to_owned()),
            "duplicate checksum for {path}"
        );
        assert_eq!(
            format!("{:x}", Sha256::digest(fs::read(root().join(path)).unwrap())),
            hash,
            "{path} SHA256SUMS"
        );
    }
    let mut actual = BTreeSet::from(["README.md".to_owned()]);
    for folder in ["frames", "outcomes", "replies"] {
        for entry in fs::read_dir(root().join(folder)).unwrap() {
            actual.insert(format!(
                "{folder}/{}",
                entry.unwrap().file_name().into_string().unwrap()
            ));
        }
    }
    assert_eq!(listed, actual, "missing or unchecked corpus checksums");
    // Explicit bash runon calls retired the command matcher and its policy
    // vectors. Every remaining runner/caller file still needs a checksum.
    assert_eq!(listed.len(), 368);
}

#[test]
fn caller_public_paths_are_the_published_types() {
    // Assignment, not a serialization comparison, proves Rust type identity.
    macro_rules! same_type {
        ($($name:ident),+ $(,)?) => {
            $(let _: Option<cortexkit_exec_remote_types::$name> = None::<motor_protocol::$name>;)+
        };
    }
    same_type!(
        Uuid,
        BytePayload,
        RunRequest,
        Network,
        PrepareRequest,
        RefusalReason,
        Outcome,
        Killed,
        Ran,
        TerminalRecord,
        OutputStream,
        Output,
        Accepted,
        Started,
        PrepareOutcome,
        PrepareReply,
        DropRequest,
        DropReply,
        AttachRequest,
        CancelRequest,
        CancelReply,
        StatusRequest,
        RebuildResult,
        RunningJob,
        RepositoryStatus,
        StatusReply,
        StreamRecord,
    );
    assert_eq!(CAPABILITY, "exec-remote/v1");
    assert_eq!(
        REFUSAL_DETAIL_MAX_BYTES,
        cortexkit_exec_remote_types::REFUSAL_DETAIL_MAX_BYTES
    );
    let terminal: cortexkit_exec_remote_types::TerminalRecord = TerminalRecord::new(
        Uuid::nil(),
        Outcome::RefusedBeforeStart {
            reason: RefusalReason::RunnerDiskFull,
        },
        0,
        0,
        0,
    )
    .with_refusal_detail("free_bytes=10 floor_bytes=20")
    .with_retry_after_ms(1200);
    assert_eq!(
        terminal.refusal_detail(),
        Some("free_bytes=10 floor_bytes=20")
    );
    assert_eq!(terminal.retry_after_ms(), Some(1200));
    let network: cortexkit_exec_remote_types::Network = Network::Outbound;
    let request: cortexkit_exec_remote_types::RunRequest =
        RunRequest::new("key", "root", "cwd", "true").with_network(network.clone());
    assert_eq!(request.network(), Some(&Network::Outbound));
    let accepted: cortexkit_exec_remote_types::Accepted =
        Accepted::new(Uuid::nil(), 0).with_network(network);
    assert_eq!(accepted.network(), Some(&Network::Outbound));
    let reply: cortexkit_exec_remote_types::PrepareReply = PrepareReply::new(
        Uuid::nil(),
        PrepareOutcome::RefusedBeforeStart {
            reason: RefusalReason::RunnerDraining,
        },
    )
    .with_refusal_detail("runner_draining")
    .with_retry_after_ms(1200);
    assert_eq!(reply.refusal_detail(), Some("runner_draining"));
    assert_eq!(reply.retry_after_ms(), Some(1200));
    assert_eq!(
        serde_json::to_value(StatusRequest::new()).unwrap(),
        json!({})
    );
}

#[test]
fn unknown_refusal_retains_the_before_start_arm() {
    let terminal = terminal("crate-local-unknown-refusal");
    assert_eq!(
        terminal.outcome,
        Outcome::RefusedBeforeStart {
            reason: RefusalReason::Unknown("future_refusal".into()),
        }
    );
    assert_eq!(terminal.ran, Some(Ran::None));
}

#[test]
fn unknown_outcome_does_not_claim_a_before_start_proof() {
    let terminal = terminal("crate-local-unknown-outcome");
    assert_eq!(
        terminal.outcome,
        Outcome::Unknown {
            kind: "future_outcome".into()
        }
    );
    assert!(!matches!(
        terminal.outcome,
        Outcome::RefusedBeforeStart { .. }
    ));
}

#[test]
fn unknown_stream_record_retains_sequence_and_is_not_terminal() {
    let value = vector("outcomes", "crate-local-unknown-stream-record");
    let stream: Vec<StreamRecord> = serde_json::from_value(value["stream"].clone()).unwrap();
    assert_eq!(
        stream[0],
        StreamRecord::Unknown {
            kind: "future_record".into(),
            seq: Some(7)
        }
    );
    assert!(!matches!(stream[0], StreamRecord::Terminal(_)));
    assert!(matches!(stream[1], StreamRecord::Terminal(_)));
    let StreamRecord::Unknown { seq: Some(seq), .. } = &stream[0] else {
        panic!("expected a sequenced unknown record")
    };
    assert_eq!(
        AttachRequest::new(terminal("crate-local-unknown-outcome").job_id, seq + 1).from_seq,
        8
    );
}

#[test]
fn unknown_killed_reason_is_retained() {
    assert_eq!(
        terminal("crate-local-unknown-killed").killed,
        Some(Killed::Unknown("future_kill".into()))
    );
}

#[test]
fn unknown_ran_is_not_a_guarantee_of_no_execution() {
    let terminal = terminal("crate-local-unknown-ran");
    assert_eq!(terminal.ran, Some(Ran::Unknown("future_location".into())));
    assert_ne!(terminal.ran, Some(Ran::None));
    assert_eq!(terminal.outcome, Outcome::OutcomeUnknown);
}

#[test]
fn unknown_output_stream_preserves_raw_bytes_and_sequence() {
    let StreamRecord::Output(output) = record("crate-local-unknown-output-stream") else {
        panic!("expected output")
    };
    assert_eq!(output.stream, OutputStream::Unknown("future_stream".into()));
    assert_eq!(output.bytes.0, [0xe2]);
    assert_eq!(output.seq, 7);
}

#[test]
fn unknown_prepare_outcome_is_not_prepared() {
    let reply: PrepareReply =
        serde_json::from_value(vector("replies", "crate-local-unknown-prepare-outcome")).unwrap();
    assert_eq!(
        reply.outcome,
        PrepareOutcome::Unknown {
            kind: "future_prepare".into()
        }
    );
    assert_ne!(reply.outcome, PrepareOutcome::Prepared);
}

#[test]
fn unknown_rebuild_result_is_retained() {
    let reply: StatusReply =
        serde_json::from_value(vector("replies", "crate-local-unknown-rebuild-result")).unwrap();
    assert_eq!(
        reply.repositories[0].last_rebuild_result,
        Some(RebuildResult::Unknown("future_result".into()))
    );
}

#[test]
fn malformed_known_tags_and_invalid_unknown_sequences_still_fail() {
    for value in [
        json!({"type": "exit"}),
        json!({"type": "refused_before_start", "reason": 1}),
    ] {
        assert!(serde_json::from_value::<Outcome>(value).is_err());
    }
    for value in [
        json!({"type": "output", "seq": 7, "stream": "stdout", "bytes": "!"}),
        json!({"type": "terminal"}),
    ] {
        assert!(serde_json::from_value::<StreamRecord>(value).is_err());
    }
    assert!(
        serde_json::from_value::<PrepareOutcome>(json!({"type": "refused_before_start"})).is_err()
    );
    for seq in [json!(null), json!(-1), json!(1.5), json!("7")] {
        assert!(serde_json::from_value::<StreamRecord>(
            json!({"type": "future_record", "seq": seq})
        )
        .is_err());
    }
}
