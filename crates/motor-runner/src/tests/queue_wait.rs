use super::*;

fn test_runner(f: &Fixture) -> Runner {
    let mut config = f.config();
    config.manual_tick = true;
    config.disable_warm_builds = true;
    config.max_weight = 4;
    Runner::open(config).unwrap()
}

async fn start_holder(f: &Fixture, runner: &Runner) -> (Uuid, ReleaseWriter) {
    let release = f.scratch.join("release");
    let release_writer = ReleaseWriter::new(release.clone());
    let (mut holder, bytes) = job_at(
        f,
        "holder",
        &format!(
            "while test ! -e '{}'; do sleep .02; done",
            release.display()
        ),
    );
    holder.weight_hint = Some(4);
    f.submit(runner, &holder, &bytes);
    runner.tick().unwrap();
    until(|| runner.core.inner.lock().unwrap().jobs[&holder.job_id].state == JobState::Running)
        .await;
    (holder.job_id, release_writer)
}

#[tokio::test]
async fn expired_job_is_refused_before_setup_and_the_next_job_starts() {
    let f = Fixture::new();
    let runner = test_runner(&f);
    let (holder, release) = start_holder(&f, &runner).await;

    let (mut expired, bytes) = job_at(&f, "expired", "true");
    expired.weight_hint = Some(4);
    expired.queue_wait_limit_s = 1;
    f.submit(&runner, &expired, &bytes);
    let (mut next, bytes) = job_at(&f, "next", "true");
    next.weight_hint = Some(4);
    f.submit(&runner, &next, &bytes);
    runner.tick().unwrap();
    assert!(runner.core.inner.lock().unwrap().starting.is_empty());

    f.clock.fetch_add(1_000, Ordering::SeqCst);
    runner.tick().unwrap();
    assert!(runner.core.inner.lock().unwrap().jobs[&expired.job_id]
        .terminal
        .is_none());
    let last_seen = runner.now();
    // Leave a gap between the last observation and the refusal so the detail
    // has to report when the holder was seen, not the time of the refusal.
    f.clock.fetch_add(2, Ordering::SeqCst);
    drop(release);
    assert_eq!(
        terminal(&runner, holder).await.outcome,
        Outcome::Exit { code: 0 }
    );
    runner.tick().unwrap();

    let refused = terminal(&runner, expired.job_id).await;
    assert_eq!(
        refused.outcome,
        Outcome::RefusedBeforeStart {
            reason: RefusalReason::QueueWaitExceeded
        }
    );
    assert_eq!(refused.ran, Some(Ran::None));
    assert_eq!(refused.queue_wait_ms, 1_002);
    assert!(runner.core.inner.lock().unwrap().jobs[&expired.job_id]
        .setup_ms
        .is_none());
    let detail = refused.refusal_detail().unwrap();
    assert!(
        detail.starts_with(&format!("waiting_on=permits; last_seen_ms={last_seen};")),
        "{detail}"
    );
    assert!(
        detail.contains(&format!("holders=[job={holder} ")),
        "{detail}"
    );
    assert!(detail.ends_with("heads_ahead=[]"), "{detail}");
    assert_eq!(
        terminal(&runner, next.job_id).await.outcome,
        Outcome::Exit { code: 0 }
    );
}

#[tokio::test]
async fn withdrawal_with_a_live_holder_keeps_the_live_detail() {
    let f = Fixture::new();
    let runner = test_runner(&f);
    let (holder, release) = start_holder(&f, &runner).await;
    let start_ms = runner.core.inner.lock().unwrap().jobs[&holder]
        .started_ms
        .unwrap();
    let (mut waiting, bytes) = job_at(&f, "waiting", "true");
    waiting.weight_hint = Some(4);
    waiting.queue_wait_limit_s = 1;
    f.submit(&runner, &waiting, &bytes);
    f.clock.fetch_add(1_000, Ordering::SeqCst);

    assert_eq!(
        runner.withdraw(waiting.job_id).unwrap(),
        WithdrawOutcome::Withdrawn
    );
    let term = terminal(&runner, waiting.job_id).await;
    let detail = term.refusal_detail().unwrap();
    assert!(
        detail.starts_with("waiting_on=permits; requested_permits=4; held_permits=4/4;"),
        "{detail}"
    );
    assert!(
        detail.contains(&format!("holders=[job={holder} ")),
        "{detail}"
    );
    assert!(
        detail.contains(&format!("phase=running since_ms={start_ms}]")),
        "{detail}"
    );
    assert!(
        detail.ends_with("reservations=[]; heads_ahead=[]"),
        "{detail}"
    );
    drop(release);
    assert_eq!(
        terminal(&runner, holder).await.outcome,
        Outcome::Exit { code: 0 }
    );
}

#[tokio::test]
async fn detail_uses_the_last_holder_when_the_only_reservation_is_its_own_setup() {
    let f = Fixture::new();
    let runner = test_runner(&f);
    let (holder, release) = start_holder(&f, &runner).await;
    let (mut waiting, bytes) = job_at(&f, "waiting", "true");
    waiting.weight_hint = Some(4);
    f.submit(&runner, &waiting, &bytes);
    let seen = runner.now();
    drop(release);
    assert_eq!(
        terminal(&runner, holder).await.outcome,
        Outcome::Exit { code: 0 }
    );
    f.clock.fetch_add(10, Ordering::SeqCst);

    // A Windows guest boot is still withdrawable. Reproduce the scheduler
    // view for that interval without needing to launch a guest.
    let mut inner = runner.core.inner.lock().unwrap();
    let now = runner.now();
    inner.starting.insert(waiting.job_id);
    inner.reserved.insert(
        waiting.workspace_key.clone(),
        Reservation {
            repository: waiting.repository_root,
            commit: waiting.snapshot_commit,
            siblings: waiting.siblings,
            since_ms: now,
        },
    );
    let detail = runner.queue_wait_detail(&inner, waiting.job_id);
    inner.starting.remove(&waiting.job_id);
    inner.reserved.remove(&waiting.workspace_key);
    drop(inner);
    assert!(
        detail.contains(&format!("holders=[job={holder} ")),
        "{detail}"
    );
    assert!(!detail.contains(&waiting.job_id.to_string()), "{detail}");
    assert!(
        detail.starts_with(&format!("waiting_on=permits; last_seen_ms={seen};")),
        "{detail}"
    );
}
