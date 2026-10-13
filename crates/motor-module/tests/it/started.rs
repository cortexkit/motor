use super::recovery::{collect, terminal, Cut, Fixture};
use motor_protocol::*;
use std::{fs, sync::atomic::Ordering, time::Duration};

#[tokio::test]
async fn queued_caller_receives_accepted_started_output_terminal_and_replays_start() {
    let f = Fixture::new(Cut::None, 28).await;
    let motor = f.motor();
    let mut holder = motor
        .run(f.request("while test ! -f release-holder; do sleep 0.02; done"))
        .unwrap();
    loop {
        let record = tokio::time::timeout(Duration::from_secs(30), holder.records.recv())
            .await
            .unwrap()
            .unwrap()
            .unwrap();
        if matches!(record, StreamRecord::Started(_)) {
            break;
        }
        assert!(matches!(record, StreamRecord::Accepted(_)));
    }
    let mut queued = motor.run(f.request("printf queued-output")).unwrap();
    let id = queued.job_id;
    let accepted = tokio::time::timeout(Duration::from_secs(30), queued.records.recv())
        .await
        .unwrap()
        .unwrap()
        .unwrap();
    assert!(matches!(accepted, StreamRecord::Accepted(_)));
    tokio::time::timeout(Duration::from_secs(30), async {
        while f.runner.job_status(id).unwrap().queue_wait_ms.is_none() {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .unwrap();
    f.clock.fetch_add(125, Ordering::SeqCst);
    fs::write(f.remote_key.join("release-holder"), "go").unwrap();
    assert_eq!(
        terminal(&collect(holder).await).outcome,
        Outcome::Exit { code: 0 }
    );
    let remaining = collect(queued).await;
    assert_eq!(remaining.len(), 3);
    let StreamRecord::Started(started) = &remaining[0] else {
        panic!("Started before Output")
    };
    assert_eq!(started.seq, 0);
    assert_eq!(started.queue_wait_ms, 125);
    assert_eq!(started.started_at_ms, f.clock.load(Ordering::SeqCst));
    assert!(
        matches!(&remaining[1], StreamRecord::Output(o) if o.seq == 1 && o.bytes.0 == b"queued-output")
    );
    assert_eq!(terminal(&remaining).outcome, Outcome::Exit { code: 0 });
    assert_eq!(terminal(&remaining).queue_wait_ms, started.queue_wait_ms);
    let before = collect(motor.attach(AttachRequest::new(id, 0)).unwrap()).await;
    assert_eq!(before, remaining);
    let after = collect(motor.attach(AttachRequest::new(id, 1)).unwrap()).await;
    assert_eq!(after, remaining[1..]);
}

#[tokio::test]
async fn refused_caller_has_no_started_record() {
    let f = Fixture::new(Cut::DiskFull, 28).await;
    let records = collect(f.motor().run(f.request("echo forbidden > marker")).unwrap()).await;
    assert_eq!(records.len(), 1);
    assert!(matches!(&records[0], StreamRecord::Terminal(t)
        if t.outcome == Outcome::RefusedBeforeStart { reason: RefusalReason::RunnerDiskFull } && t.ran == Some(Ran::None)));
    assert!(!f.remote_key.join("marker").exists());
}
