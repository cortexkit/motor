use super::*;
use cortexkit_test_support::ScratchDir;
use std::collections::BTreeMap;

#[test]
fn state_clone_cost_for_sixteen_mib_hosted_history() {
    use crate::hosted::{BillingStatus, HostedJobState};
    use crate::storage::Job;
    use motor_protocol::{Outcome, Ran, TerminalRecord, Uuid};

    let mut state = State::default();
    let id = Uuid::now_v7();
    let terminal = TerminalRecord::new(id, Outcome::Exit { code: 0 }, 30_000, 500, 1_000_000)
        .with_ran(Ran::Remote)
        .with_last_seq(20);
    let job = Job {
        hosted: Some(HostedJobState {
            endpoint: Some("https://runner.example.invalid:443/tenant/job".into()),
            endpoint_spki_sha256: Some("a".repeat(64)),
            broker_url: "https://broker.example.invalid".into(),
            billing: BillingStatus::Available,
            finish_done: true,
            run_sent: true,
            lease_attempted: true,
            max_runtime_s: Some(900),
            billed_vcpu_ms: Some(120_000),
            terminal: Some(terminal.clone()),
            replay_end_seq: Some(21),
            ..Default::default()
        }),
        remote_terminal: Some(terminal),
        queue_wait_limit_s: 120,
        terminal_observed_at_ms: Some(1_700_000_000_000),
        terminal_delivered_at_ms: Some(1_700_000_001_000),
        retain_until_ms: Some(1_700_086_400_000),
        workspace_key: Some("/work/projects/example".into()),
        ..Default::default()
    };
    let per_job = serde_json::to_vec(&job).unwrap().len() + 40;
    for _ in 0..(16 * 1024 * 1024 / per_job + 1) {
        state.jobs.insert(Uuid::now_v7(), job.clone());
    }
    let serialized_bytes = serde_json::to_vec(&state).unwrap().len();
    let (copy, heap_bytes) = crate::allocation_tests::peak(|| state.clone());
    assert_eq!(copy.jobs.len(), state.jobs.len());
    eprintln!(
        "hosted history: jobs={} serialized_bytes={serialized_bytes} cloned_heap_bytes={heap_bytes}",
        state.jobs.len()
    );
}

#[test]
fn checkpoint_has_only_one_serialized_copy_and_round_trips() {
    let root = ScratchDir::new("checkpoint-memory");
    let mut state = State::default();
    state.snapshots.insert(
        "repository".into(),
        (0..30_000)
            .map(|n| (format!("snapshot-{n:08}"), "a".repeat(96)))
            .collect::<BTreeMap<_, _>>(),
    );
    let expected = serde_json::to_value(&state).unwrap();
    let serialized_bytes = serde_json::to_vec(&state).unwrap().len();
    for journaled in [false, true] {
        let metrics = Arc::new(Metrics::default());
        let backend = if journaled {
            let (_, mut backend) = journal::open(&root, &metrics).unwrap();
            backend
                .append(&root, &State::default(), &state, &metrics)
                .unwrap();
            backend
        } else {
            Backend::Legacy
        };
        let persistence = Persistence::new(&root, backend, metrics.clone());
        let state = Mutex::new(state.clone());
        let (result, peak) = crate::allocation_tests::peak(|| persistence.checkpoint(&state));
        result.unwrap();
        eprintln!("checkpoint: journaled={journaled} serialized_bytes={serialized_bytes} peak_heap_bytes={peak}");
        assert!(
            peak < serialized_bytes * 5 / 2,
            "checkpoint allocated {peak} bytes for {serialized_bytes} serialized bytes"
        );
        // Both the journal reader and a reader that ignores journal metadata
        // must recover the same legacy-readable state.
        let legacy: State =
            serde_json::from_reader(std::fs::File::open(root.join("state.json")).unwrap()).unwrap();
        assert_eq!(serde_json::to_value(legacy).unwrap(), expected);
        let (loaded, _) = journal::open(&root, &metrics).unwrap();
        assert_eq!(serde_json::to_value(loaded).unwrap(), expected);
    }
}
