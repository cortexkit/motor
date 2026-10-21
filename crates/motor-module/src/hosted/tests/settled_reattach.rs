//! A hosted job whose stream drops after its host ended and settled it, but
//! before ck-motor read the terminal, is recovered by reattaching: the broker
//! answers a usage-settled job's reattach with an attach token, the host
//! replays the rest from ck-motor's cursor, and the job ends with its real
//! terminal and the host's billed usage, which ck-motor never reports again.
use super::*;
use crate::hosted::boat_tests::{committed_repo, records};

#[tokio::test]
async fn stream_lost_after_host_settled_reattaches_and_replays_the_real_terminal() {
    let h = Harness::new(Behavior::SettleThenDrop).await;
    let (scratch, repo) = committed_repo();
    let ssh = Arc::new(NoSsh(AtomicUsize::new(0)));
    let config = Config {
        hosted_runners: false,
        hosted_repositories: vec![repo.to_str().unwrap().into()],
        ..h.config()
    };
    let motor =
        crate::Motor::with_connector(config, &scratch.0.join("state"), ssh.clone()).unwrap();
    let args = crate::cli::RunArgs {
        cwd: Some(repo.clone()),
        command: vec!["true".into()],
        timeout: 30,
        ..crate::cli::RunArgs::default()
    };
    let stream = motor
        .run(crate::cli::build_request(&args).unwrap())
        .unwrap();
    let id = stream.job_id;
    let (output, terminal) = records(stream).await;

    assert_eq!(
        terminal.outcome,
        Outcome::Exit {
            code: SETTLED_EXIT_CODE
        },
        "{terminal:?}"
    );
    // Every output record arrives exactly once: the one read before the drop
    // is not replayed, and the one the drop lost is.
    assert_eq!(output, SETTLED_OUTPUT.concat());

    let (settlement, reason) = h
        .broker
        .lock()
        .unwrap()
        .settlement(&id.to_string())
        .map(|(reply, reason)| (reply.clone(), reason))
        .unwrap();
    assert_eq!(reason, "usage");
    let state = motor.hosted_state(id).unwrap();
    assert_eq!(state.billing, BillingStatus::Available);
    assert_eq!(state.billed_vcpu_ms, Some(settlement.settled_vcpu_ms));

    // One lease, one Run, then reattach only: ck-motor neither resubmits nor
    // releases, finishes or reports usage for a job its host already settled.
    let requests = h.requests.lock().unwrap().clone();
    assert_eq!(requests.first().map(String::as_str), Some("/v1/leases"));
    assert!(requests.len() > 1, "{requests:?}");
    assert!(
        requests[1..].iter().all(|path| path.ends_with("/reattach")),
        "{requests:?}"
    );
    assert_eq!(h.runs.load(Ordering::SeqCst), 1);
    assert_eq!(ssh.0.load(Ordering::SeqCst), 0);
}
