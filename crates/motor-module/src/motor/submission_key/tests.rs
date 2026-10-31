use super::*;
use crate::motor::queue_tests::{fixture_repo, ResubmitPeer};
use crate::transport::{Connector, Duplex};
use crate::Config;
use cortexkit_test_support::ScratchDir;
use std::path::{Path, PathBuf};
use std::sync::{
    atomic::{AtomicBool, Ordering},
    Arc,
};
use std::{future::Future, pin::Pin, time::Duration};

const PRINCIPAL: &str = "reserved:aft";
const KEY: &str = "caller:submission-0001";

fn request(path: &Path) -> RunRequest {
    let path = path.to_str().unwrap();
    RunRequest::new(path, path, path, "exit 7")
        .with_timeout(30)
        .with_submission_key(KEY)
}

fn motor(path: &Path, peer: ResubmitPeer) -> Motor {
    Motor::with_test_fixture_direct_route(
        Config {
            runner_instance: "test".into(),
            ..Config::default()
        },
        path,
        Arc::new(peer),
    )
    .unwrap()
}

async fn records(mut stream: JobStream) -> Vec<StreamRecord> {
    tokio::time::timeout(Duration::from_secs(30), async {
        let mut records = Vec::new();
        loop {
            let record = stream
                .records
                .recv()
                .await
                .expect("stream closed")
                .expect("stream error");
            let terminal = matches!(record, StreamRecord::Terminal(_));
            records.push(record);
            if terminal {
                return records;
            }
        }
    })
    .await
    .expect("submission did not finish")
}

fn attached(records: &[StreamRecord], id: Uuid) {
    assert!(
        matches!(&records[0], StreamRecord::Accepted(a) if a.job_id == id && a.attached),
        "{records:?}"
    );
    assert!(
        matches!(records.last(), Some(StreamRecord::Terminal(t)) if t.job_id == id),
        "{records:?}"
    );
}

fn never(records: &[StreamRecord], original: Uuid) {
    assert!(
        matches!(records, [StreamRecord::Terminal(t)] if t.job_id != original && t.outcome == Outcome::RefusedBeforeStart { reason: RefusalReason::SubmissionNeverAccepted } && t.ran == Some(Ran::None)),
        "{records:?}"
    );
}

fn seed(motor: &Motor, id: Uuid, request: &RunRequest, sent: bool) {
    motor
        .storage
        .create(id, 30, None, Some("test".into()), false)
        .unwrap();
    motor
        .storage
        .record_submission(
            id,
            KeyRecord {
                principal: PRINCIPAL.into(),
                key: KEY.into(),
                digest: digest(request).unwrap(),
                run_sent: sent,
                accepted: None,
            },
        )
        .unwrap();
    motor
        .storage
        .retain_job(id, Some(request.workspace_key.clone()))
        .unwrap();
}

#[tokio::test]
async fn known_key_replays_exclusively_and_does_not_recapture_checkout() {
    let scratch = ScratchDir::new("key-replay");
    let repo = fixture_repo(scratch.path());
    let peer = ResubmitPeer::new();
    let log = peer.log.clone();
    let motor = motor(&scratch.join("state"), peer);
    let request = request(&repo);
    let stream = motor
        .submit_agent(request.clone(), None, PRINCIPAL)
        .await
        .unwrap();
    let id = stream.job_id;
    let first = records(stream).await;
    assert!(matches!(&first[0], StreamRecord::Accepted(a) if !a.attached));
    assert!(serde_json::to_value(&first[0])
        .unwrap()
        .get("attached")
        .is_none());
    std::fs::write(repo.join("changed-after-submit"), "different snapshot").unwrap();
    let replay = records(
        motor
            .submit_agent(request.clone().with_progress(true), None, PRINCIPAL)
            .await
            .unwrap(),
    )
    .await;
    attached(&replay, id);
    assert!(!replay
        .iter()
        .any(|record| matches!(record, StreamRecord::Progress(_))));
    assert!(replay
        .iter()
        .any(|r| matches!(r, StreamRecord::Started(s) if s.seq == 0)));
    assert!(replay
        .iter()
        .any(|r| matches!(r, StreamRecord::Output(o) if o.seq == 1)));
    for cursor in [0, 1, u64::MAX] {
        let mut resume = request.clone();
        resume.resume_from_seq = Some(cursor);
        let replay = records(motor.submit_agent(resume, None, PRINCIPAL).await.unwrap()).await;
        attached(&replay, id);
        assert!(!replay.iter().any(|r| matches!(r, StreamRecord::Started(_))));
        assert_eq!(
            replay
                .iter()
                .filter(|r| matches!(r, StreamRecord::Output(_)))
                .count(),
            usize::from(cursor == 0)
        );
    }
    assert_eq!(log.lock().unwrap().runs.len(), 1);
}

#[tokio::test]
async fn unknown_key_with_cursor_starts_a_fresh_submission() {
    let scratch = ScratchDir::new("key-unknown");
    let repo = fixture_repo(scratch.path());
    let peer = ResubmitPeer::new();
    let log = peer.log.clone();
    let motor = motor(&scratch.join("state"), peer);
    let mut request = request(&repo);
    request.resume_from_seq = Some(99);
    let result = records(motor.submit_agent(request, None, PRINCIPAL).await.unwrap()).await;
    assert!(matches!(&result[0], StreamRecord::Accepted(a) if !a.attached));
    assert!(result
        .iter()
        .any(|r| matches!(r, StreamRecord::Output(o) if o.seq == 1)));
    assert_eq!(log.lock().unwrap().runs.len(), 1);
}

#[tokio::test]
async fn digest_mismatch_refuses_under_its_own_id_without_changing_job() {
    let scratch = ScratchDir::new("key-mismatch");
    let repo = fixture_repo(scratch.path());
    let peer = ResubmitPeer::new();
    let log = peer.log.clone();
    let motor = motor(&scratch.join("state"), peer);
    let original = request(&repo);
    let stream = motor
        .submit_agent(original.clone(), None, PRINCIPAL)
        .await
        .unwrap();
    let id = stream.job_id;
    records(stream).await;
    let mut changed = original.clone();
    changed.command = "different command".into();
    let refusal = records(motor.submit_agent(changed, None, PRINCIPAL).await.unwrap()).await;
    assert!(
        matches!(&refusal[..], [StreamRecord::Terminal(t)] if t.job_id != id && t.outcome == Outcome::RefusedBeforeStart { reason: RefusalReason::RequestRejected } && t.refusal_detail.as_deref() == Some("submission_key_mismatch")),
        "{refusal:?}"
    );
    attached(
        &records(motor.submit_agent(original, None, PRINCIPAL).await.unwrap()).await,
        id,
    );
    assert_eq!(log.lock().unwrap().runs.len(), 1);
}

#[test]
fn digest_is_canonical_and_covers_all_caller_execution_fields() {
    let original: RunRequest = serde_json::from_str(r#"{"workspace_key":"/repo","repository_root":"/repo","cwd":"/repo","command":"echo hello","env":{"CI":"1","PATH":"ignored"},"siblings":[],"submission_key":"caller:submission-0001"}"#).unwrap();
    let reordered: RunRequest = serde_json::from_str(r#"{"resume_from_seq":7,"submission_key":"caller:submission-0001","siblings":[],"env":{"PATH":"also ignored","CI":"1"},"command":"echo hello","cwd":"/repo","repository_root":"/repo","workspace_key":"/repo"}"#).unwrap();
    assert_eq!(digest(&original).unwrap(), digest(&reordered).unwrap());
    assert_eq!(
        digest(&original).unwrap(),
        digest(&original.clone().with_progress(true)).unwrap()
    );
    for (field, replacement) in [
        ("command", serde_json::json!("echo changed")),
        ("cwd", serde_json::json!("/repo/sub")),
        ("workspace_key", serde_json::json!("/other")),
        ("repository_root", serde_json::json!("/other")),
        ("siblings", serde_json::json!(["/sibling"])),
        ("platform", serde_json::json!("windows")),
        ("weight_hint", serde_json::json!(4)),
        ("network", serde_json::json!("outbound")),
        ("timeout", serde_json::json!(60)),
        ("queue_wait_limit_s", serde_json::json!(40)),
        ("env", serde_json::json!({"CI":"changed"})),
        ("return_paths", serde_json::json!(["result"])),
        ("return_max_bytes", serde_json::json!(123)),
        ("return_chunked", serde_json::json!(true)),
    ] {
        let mut value = serde_json::to_value(&original).unwrap();
        value[field] = replacement;
        let changed: RunRequest = serde_json::from_value(value).unwrap();
        assert_ne!(
            digest(&original).unwrap(),
            digest(&changed).unwrap(),
            "{field} was not digested"
        );
    }
}

#[tokio::test]
async fn two_principals_have_separate_namespaces_and_stream_authority() {
    let scratch = ScratchDir::new("key-principals");
    let repo = fixture_repo(scratch.path());
    let peer = ResubmitPeer::new();
    let log = peer.log.clone();
    let motor = motor(&scratch.join("state"), peer);
    let request = request(&repo);
    let a = motor
        .submit_agent(request.clone(), None, "reserved:a")
        .await
        .unwrap();
    let a_id = a.job_id;
    records(a).await;
    let b = motor
        .submit_agent(request.clone(), None, "reserved:b")
        .await
        .unwrap();
    let b_id = b.job_id;
    records(b).await;
    assert_ne!(a_id, b_id);
    for (principal, own, other) in [("reserved:a", a_id, b_id), ("reserved:b", b_id, a_id)] {
        assert!(motor.submission_owned(own, principal));
        assert!(!motor.submission_owned(other, principal));
        attached(
            &records(
                motor
                    .submit_agent(request.clone(), None, principal)
                    .await
                    .unwrap(),
            )
            .await,
            own,
        );
    }
    assert_eq!(log.lock().unwrap().runs.len(), 2);
}

#[tokio::test]
async fn restart_between_key_record_and_submit_is_final_never_accepted() {
    let scratch = ScratchDir::new("key-pre-run-restart");
    let repo = fixture_repo(scratch.path());
    let root = scratch.join("state");
    let request = request(&repo);
    let id = Uuid::now_v7();
    let before = motor(&root, ResubmitPeer::new());
    seed(&before, id, &request, false);
    before.close().await;
    let peer = ResubmitPeer::new();
    let log = peer.log.clone();
    let after = motor(&root, peer);
    never(
        &records(after.submit_agent(request, None, PRINCIPAL).await.unwrap()).await,
        id,
    );
    assert!(log.lock().unwrap().runs.is_empty());
    assert!(log.lock().unwrap().attaches.is_empty());
}

#[tokio::test]
async fn restart_after_run_with_lost_ack_reattaches_runner_record() {
    let scratch = ScratchDir::new("key-post-run-restart");
    let repo = fixture_repo(scratch.path());
    let root = scratch.join("state");
    let request = request(&repo);
    let id = Uuid::now_v7();
    let before = motor(&root, ResubmitPeer::new());
    seed(&before, id, &request, true);
    before.close().await;
    let peer = ResubmitPeer::new();
    let log = peer.log.clone();
    log.lock().unwrap().completed.insert(id);
    let after = motor(&root, peer);
    let replay = records(after.submit_agent(request, None, PRINCIPAL).await.unwrap()).await;
    attached(&replay, id);
    assert!(replay
        .iter()
        .any(|r| matches!(r, StreamRecord::Output(o) if o.bytes.0 == b"build server output\n")));
    assert!(log.lock().unwrap().runs.is_empty());
    assert_eq!(log.lock().unwrap().attaches, [id]);
}

#[tokio::test]
async fn run_fence_without_runner_admission_uses_attach_tombstone() {
    let scratch = ScratchDir::new("key-run-tombstone");
    let repo = fixture_repo(scratch.path());
    let request = request(&repo);
    let peer = ResubmitPeer::new();
    let log = peer.log.clone();
    let motor = motor(&scratch.join("state"), peer);
    let id = Uuid::now_v7();
    seed(&motor, id, &request, true);
    never(
        &records(
            motor
                .submit_agent(request.clone(), None, PRINCIPAL)
                .await
                .unwrap(),
        )
        .await,
        id,
    );
    never(
        &records(motor.submit_agent(request, None, PRINCIPAL).await.unwrap()).await,
        id,
    );
    assert!(log.lock().unwrap().runs.is_empty());
}

#[tokio::test]
async fn refused_key_survives_restart_and_different_request_still_mismatches() {
    let scratch = ScratchDir::new("key-refusal");
    let repo = fixture_repo(scratch.path());
    let request = request(&repo).with_network(motor_protocol::Network::Outbound);
    let root = scratch.join("state");
    let before = motor(&root, ResubmitPeer::new());
    let stream = before
        .submit_agent(request.clone(), None, PRINCIPAL)
        .await
        .unwrap();
    let id = stream.job_id;
    assert!(
        matches!(records(stream).await.last(), Some(StreamRecord::Terminal(t)) if matches!(t.outcome, Outcome::RefusedBeforeStart { .. }))
    );
    before.close().await;
    let after = motor(&root, ResubmitPeer::new());
    never(
        &records(
            after
                .submit_agent(request.clone(), None, PRINCIPAL)
                .await
                .unwrap(),
        )
        .await,
        id,
    );
    let mut changed = request;
    changed.command = "changed".into();
    let refusal = records(after.submit_agent(changed, None, PRINCIPAL).await.unwrap()).await;
    assert!(
        matches!(&refusal[0], StreamRecord::Terminal(t) if t.job_id != id && t.refusal_detail.as_deref() == Some("submission_key_mismatch"))
    );
}

#[tokio::test]
async fn runner_acceptance_is_remembered_before_network_echo_refusal() {
    let scratch = ScratchDir::new("key-accepted-network-refusal");
    let repo = fixture_repo(scratch.path());
    let peer = ResubmitPeer::new();
    let log = peer.log.clone();
    let motor = Motor::with_test_fixture_direct_route(
        Config {
            runner_instance: "test".into(),
            network_routing: true,
            ..Config::default()
        },
        &scratch.join("state"),
        Arc::new(peer),
    )
    .unwrap();
    let request = request(&repo).with_network(motor_protocol::Network::Outbound);
    let stream = motor
        .submit_agent(request.clone(), None, PRINCIPAL)
        .await
        .unwrap();
    let id = stream.job_id;
    let original = records(stream).await;
    assert!(
        matches!(original.last(), Some(StreamRecord::Terminal(t)) if t.outcome == Outcome::RefusedBeforeStart { reason: RefusalReason::NetworkUnsupported })
    );
    assert!(motor
        .storage
        .job(id)
        .unwrap()
        .submission
        .unwrap()
        .accepted
        .is_some());
    let replay = records(motor.submit_agent(request, None, PRINCIPAL).await.unwrap()).await;
    attached(&replay, id);
    assert_eq!(log.lock().unwrap().runs.len(), 1);
}

struct DurabilityPeer {
    inner: ResubmitPeer,
    root: PathBuf,
    observed: Arc<AtomicBool>,
}

impl Connector for DurabilityPeer {
    fn connect(
        &self,
        id: Uuid,
    ) -> Pin<Box<dyn Future<Output = io::Result<Box<dyn Duplex>>> + Send + '_>> {
        let disk = crate::storage::durable_value(&self.root).unwrap();
        let job = &disk["jobs"][id.to_string()];
        self.observed.store(
            job["submission"]["key"] == KEY
                && job["submission"]["principal"] == PRINCIPAL
                && job["submission"]["digest"]
                    .as_str()
                    .is_some_and(|digest| digest.len() == 64),
            Ordering::SeqCst,
        );
        self.inner.connect(id)
    }
}

#[tokio::test]
async fn durability_before_runner_submission() {
    let scratch = ScratchDir::new("key-durability");
    let repo = fixture_repo(scratch.path());
    let root = scratch.join("state");
    let observed = Arc::new(AtomicBool::new(false));
    let peer = DurabilityPeer {
        inner: ResubmitPeer::new(),
        root: root.clone(),
        observed: observed.clone(),
    };
    let motor = Motor::with_test_fixture_direct_route(
        Config {
            runner_instance: "test".into(),
            ..Config::default()
        },
        &root,
        Arc::new(peer),
    )
    .unwrap();
    let result = records(
        motor
            .submit_agent(request(&repo), None, PRINCIPAL)
            .await
            .unwrap(),
    )
    .await;
    assert!(
        matches!(result.last(), Some(StreamRecord::Terminal(t)) if t.outcome == Outcome::Exit { code: 7 })
    );
    assert!(
        observed.load(Ordering::SeqCst),
        "runner connection opened before the principal/key/digest journal record was durable"
    );
}

#[tokio::test]
async fn concurrent_resubmits_start_one_job() {
    let scratch = ScratchDir::new("key-concurrent");
    let repo = fixture_repo(scratch.path());
    let peer = ResubmitPeer::new();
    let log = peer.log.clone();
    let motor = motor(&scratch.join("state"), peer);
    let request = request(&repo);
    let (a, b) = tokio::join!(
        motor.submit_agent(request.clone(), None, PRINCIPAL),
        motor.submit_agent(request, None, PRINCIPAL)
    );
    let (a, b) = (a.unwrap(), b.unwrap());
    assert_eq!(a.job_id, b.job_id);
    let id = a.job_id;
    let (_, replay) = tokio::join!(records(a), records(b));
    attached(&replay, id);
    assert_eq!(log.lock().unwrap().runs.len(), 1);
}

#[tokio::test]
async fn pre_cutover_keyed_resubmission_reattaches_its_stored_destination() {
    let scratch = ScratchDir::new("key-pre-cutover-resubmission");
    let (_checkout, repo) = crate::hosted::boat_tests::committed_repo();
    let root = scratch.join("state");
    let config = Config {
        runner_instance: "test".into(),
        ..Config::default()
    };
    let peer = ResubmitPeer::new();
    let log = peer.log.clone();
    let motor = Motor::with_connector(config.clone(), &root, Arc::new(peer)).unwrap();
    let request = request(&repo).with_queue_wait_limit_s(30);
    let id = Uuid::now_v7();
    let destination = Uuid::now_v7();
    seed(&motor, id, &request, true);
    motor
        .storage
        .resubmitted(id, destination, "hosted_capacity", None)
        .unwrap();
    log.lock().unwrap().completed.insert(destination);
    motor.close().await;

    for _ in 0..2 {
        let peer = ResubmitPeer {
            log: log.clone(),
            end: crate::motor::queue_tests::ResubmitEnd::Exit,
        };
        let motor = Motor::with_connector(config.clone(), &root, Arc::new(peer)).unwrap();
        let replay = records(
            motor
                .submit_agent(request.clone(), None, PRINCIPAL)
                .await
                .unwrap(),
        )
        .await;
        attached(&replay, id);
        assert_eq!(log.lock().unwrap().attaches.last(), Some(&destination));
        assert!(
            log.lock().unwrap().runs.is_empty(),
            "a stored resubmission must never run again"
        );
        assert_eq!(
            motor.storage.keyed_job(PRINCIPAL, KEY).unwrap().unwrap().0,
            id
        );
        motor.close().await;
    }
}

#[tokio::test]
async fn keyed_hosted_route_reattaches_without_second_run() {
    let boat = crate::hosted::boat_tests::Boat::new(0, false).await;
    let scratch = ScratchDir::new("key-hosted");
    let (_checkout, repo) = crate::hosted::boat_tests::committed_repo();
    let root = scratch.join("state");
    let config = boat.config.clone();
    let motor =
        Motor::with_connector(config.clone(), &root, Arc::new(ResubmitPeer::new())).unwrap();
    let request = request(&repo);
    let stream = motor
        .submit_agent(request.clone(), None, PRINCIPAL)
        .await
        .unwrap();
    let id = stream.job_id;
    records(stream).await;
    assert!(motor.hosted_state(id).unwrap().run_sent);
    motor.close().await;
    let motor = Motor::with_connector(config, &root, Arc::new(ResubmitPeer::new())).unwrap();
    attached(
        &records(motor.submit_agent(request, None, PRINCIPAL).await.unwrap()).await,
        id,
    );
    assert_eq!(boat.runs.load(Ordering::SeqCst), 1);
}
