use super::*;
use motor_protocol::QueueBlocker;
use std::sync::atomic::{AtomicUsize, Ordering};

fn queued(id: Uuid, owner: Uuid) -> JobStatusReply {
    JobStatusReply {
        job_id: id,
        state: "queued".into(),
        waiting_on: Some("workspace owner".into()),
        blocker: Some(QueueBlocker {
            job_id: owner,
            started_ms: Some((3 * 3600 + 40 * 60 + 49) * 1000),
            timeout_s: Some(7200),
        }),
        queue_wait_ms: Some(0),
        terminal: None,
        unit: None,
        unit_alive: None,
    }
}

#[test]
fn queue_lines_are_once_per_blocker_change_and_started_is_once() {
    let mut display = QueueDisplay::default();
    let mut err = Vec::new();
    let mut reply = queued(Uuid::now_v7(), Uuid::now_v7());
    display.observe(&reply, &mut err);
    reply.blocker.as_mut().unwrap().started_ms = Some(0);
    display.observe(&reply, &mut err);
    reply.blocker.as_mut().unwrap().job_id = Uuid::now_v7();
    display.observe(&reply, &mut err);
    reply.waiting_on = Some("sibling user of this checkout".into());
    display.observe(&reply, &mut err);
    reply.state = "running".into();
    display.observe(&reply, &mut err);
    display.started(&mut err);
    reply.state = "queued".into();
    display.observe(&reply, &mut err);
    let err = String::from_utf8(err).unwrap();
    assert_eq!(err.matches("queued:").count(), 3, "{err}");
    assert_eq!(err.matches("ck motor run: started\n").count(), 1, "{err}");
    assert!(err.contains("started 03:40:49Z, limit 2 h"), "{err}");
    assert!(err.contains("using this checkout as a sibling"), "{err}");
}

#[test]
fn capacity_and_old_runner_statuses_do_not_print_queue_lines() {
    let mut display = QueueDisplay::default();
    let mut err = Vec::new();
    let mut reply = queued(Uuid::now_v7(), Uuid::now_v7());
    reply.blocker = None;
    reply.waiting_on = None;
    display.observe(&reply, &mut err);
    reply.waiting_on = Some("permits".into());
    display.observe(&reply, &mut err);
    assert!(err.is_empty());
    reply.waiting_on = Some("memory".into());
    display.observe(&reply, &mut err);
    display.observe(&reply, &mut err);
    assert_eq!(
        String::from_utf8(err).unwrap(),
        "ck motor run: queued: waiting for memory\n"
    );
}

struct PollingClient {
    polls: AtomicUsize,
    owners: [Uuid; 2],
}

#[allow(clippy::double_must_use)]
#[async_trait]
impl ExecClient for PollingClient {
    async fn run(&self, _: &RunRequest) -> Result<Box<dyn RecordStream>, SubmitError> {
        unreachable!()
    }
    async fn attach(&self, _: Uuid, _: &str) -> Result<Box<dyn RecordStream>, SubmitError> {
        unreachable!()
    }
    async fn status(&self, id: Uuid, secret: &str) -> Result<JobStatusReply, SubmitError> {
        assert_eq!(secret, "private");
        let poll = self.polls.fetch_add(1, Ordering::SeqCst);
        let mut reply = queued(id, self.owners[usize::from(poll >= 2)]);
        if poll >= 4 {
            reply.state = "running".into();
        }
        Ok(reply)
    }
    async fn cancel(&self, _: Uuid, _: Option<&str>) -> Result<(), String> {
        unreachable!()
    }
}

#[tokio::test(start_paused = true)]
async fn run_polls_live_status_until_started_without_heartbeat_spam() {
    let client = PollingClient {
        polls: AtomicUsize::new(0),
        owners: [Uuid::now_v7(), Uuid::now_v7()],
    };
    let id = Uuid::now_v7();
    let (tx, rx) = mpsc::unbounded_channel();
    tx.send(Ok(serde_json::to_vec(&StreamRecord::Accepted(
        motor_protocol::Accepted::new(id, 0),
    ))
    .unwrap()))
        .unwrap();
    tokio::spawn(async move {
        // Polls land at 5, 7, 9, 11 and 13 s; the fifth reports the start.
        tokio::time::sleep(Duration::from_secs(15)).await;
        let terminal = TerminalRecord::new(id, Outcome::Exit { code: 0 }, 0, 0, 0);
        tx.send(Ok(
            serde_json::to_vec(&StreamRecord::Terminal(terminal)).unwrap()
        ))
        .unwrap();
    });
    let (_, mut interrupts) = mpsc::channel(1);
    let (mut out, mut err) = (Vec::new(), Vec::new());
    let code = consume(
        &client,
        Box::new(QueuedStream(rx)),
        Session {
            records: None,
            returns: returned_files::Returns::new(
                None,
                None,
                cortexkit_exec_remote_types::RETURN_CHUNKED_MAX_BYTES_CAP,
            ),
            job_id: None,
            secret: Some("private".into()),
            secret_file: None,
            secret_for_attach: None,
            default_secret_directory: None,
            default_secret_file: None,
        },
        &mut out,
        &mut err,
        &mut interrupts,
    )
    .await;
    assert_eq!(code, 0);
    assert!(out.is_empty());
    let err = String::from_utf8(err).unwrap();
    assert_eq!(err.matches("queued:").count(), 2, "{err}");
    assert_eq!(err.matches("ck motor run: started\n").count(), 1, "{err}");
    assert!(!err.contains("private"));
    assert_eq!(client.polls.load(Ordering::SeqCst), 5);
}

#[tokio::test(start_paused = true)]
async fn a_job_that_finishes_promptly_reads_no_status_and_prints_nothing_new() {
    let client = PollingClient {
        polls: AtomicUsize::new(0),
        owners: [Uuid::now_v7(), Uuid::now_v7()],
    };
    let id = Uuid::now_v7();
    let (tx, rx) = mpsc::unbounded_channel();
    tx.send(Ok(serde_json::to_vec(&StreamRecord::Accepted(
        motor_protocol::Accepted::new(id, 0),
    ))
    .unwrap()))
        .unwrap();
    tokio::spawn(async move {
        tokio::time::sleep(Duration::from_secs(3)).await;
        let terminal = TerminalRecord::new(id, Outcome::Exit { code: 0 }, 0, 0, 0);
        tx.send(Ok(
            serde_json::to_vec(&StreamRecord::Terminal(terminal)).unwrap()
        ))
        .unwrap();
    });
    let (_, mut interrupts) = mpsc::channel(1);
    let (mut out, mut err) = (Vec::new(), Vec::new());
    let code = consume(
        &client,
        Box::new(QueuedStream(rx)),
        Session {
            records: None,
            returns: returned_files::Returns::new(
                None,
                None,
                cortexkit_exec_remote_types::RETURN_CHUNKED_MAX_BYTES_CAP,
            ),
            job_id: None,
            secret: Some("private".into()),
            secret_file: None,
            secret_for_attach: None,
            default_secret_directory: None,
            default_secret_file: None,
        },
        &mut out,
        &mut err,
        &mut interrupts,
    )
    .await;
    assert_eq!(code, 0);
    assert_eq!(client.polls.load(Ordering::SeqCst), 0);
    let err = String::from_utf8(err).unwrap();
    assert!(!err.contains("queued:"), "{err}");
    assert!(!err.contains("started"), "{err}");
}
