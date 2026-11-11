use super::*;
use std::io::ErrorKind;
use std::pin::Pin;
use std::task::{Context, Poll};
use tokio::io::ReadBuf;

#[tokio::test]
async fn full_spool_with_buffered_heartbeat_replays_when_vm_becomes_ready() {
    let (runtime, fake, storage) = fixture();
    hold_boot(&runtime, &fake, "runner");
    let c = claims(JOB);
    let id = claim(&runtime, &c);
    let mut frames = vec![hello(), run(JOB)];
    let mut spooled: u64 = frames
        .iter()
        .map(|f| serde_json::to_vec(f).unwrap().len() as u64 + 4)
        .sum();
    let threshold = SPOOL_LIMIT - FRAME_LIMIT as u64 - 4;
    while spooled <= threshold {
        let chunk =
            json!({"type":"bundle_chunk","index":0,"offset":0,"data":"YWFh".repeat(175_000)});
        spooled += serde_json::to_vec(&chunk).unwrap().len() as u64 + 4;
        frames.push(chunk);
        if spooled > threshold {
            // CutClient reads 8192 bytes at a time. Ensure the next heartbeat
            // is already buffered when the last spool append closes admission.
            assert!(spooled % 8192 < 8100);
            frames.push(json!({"type":"heartbeat"}));
            break;
        }
        let heartbeat = json!({"type":"heartbeat"});
        spooled += serde_json::to_vec(&heartbeat).unwrap().len() as u64 + 4;
        frames.push(heartbeat);
    }
    let mut client = CutClient::new(&frames, Cut::Write("output"));
    let r = runtime.clone();
    let mut task = tokio::spawn(async move { serve(r, &c, id, &mut client).await });
    // Spooling 64 MiB through a debug build takes several seconds on a small
    // CI machine; these bounds only catch a stall, which never completes.
    tokio::time::timeout(Duration::from_secs(60), async {
        loop {
            if std::fs::metadata(storage.spool(JOB)).is_ok_and(|m| m.len() > threshold) {
                break;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .unwrap();
    *fake.gate.0.lock().unwrap() = true;
    fake.gate.1.notify_all();
    let completed = tokio::time::timeout(Duration::from_secs(30), &mut task).await;
    if completed.is_err() {
        task.abort();
    }
    let delivered = runtime.host.lock().unwrap().snapshot.leases[JOB].delivered;
    runtime.stop_job(JOB, true, true).unwrap();
    assert!(
        completed.is_ok(),
        "a buffered heartbeat must not pin a full spool after boot"
    );
    completed.unwrap().unwrap().unwrap();
    assert!(delivered, "the queued Run must reach the guest");
}

#[derive(Clone, Copy)]
enum Cut {
    Read(ErrorKind),
    Write(&'static str),
}

// Keep reads pending for a write cut so only the selected reply can fail.
// A read cut instead allows every reply to succeed, including heartbeats.
struct CutClient {
    input: std::io::Cursor<Vec<u8>>,
    output: Vec<u8>,
    sent: Vec<Value>,
    cut: Cut,
    accepted: bool,
    hit: bool,
}
impl CutClient {
    fn new(frames: &[Value], cut: Cut) -> Self {
        let mut input = Vec::new();
        for frame in frames {
            let bytes = serde_json::to_vec(frame).unwrap();
            input.extend_from_slice(&(bytes.len() as u32).to_be_bytes());
            input.extend_from_slice(&bytes);
        }
        Self {
            input: std::io::Cursor::new(input),
            output: Vec::new(),
            sent: Vec::new(),
            cut,
            accepted: false,
            hit: false,
        }
    }
}
impl AsyncRead for CutClient {
    fn poll_read(
        mut self: Pin<&mut Self>,
        _: &mut Context<'_>,
        buf: &mut ReadBuf<'_>,
    ) -> Poll<std::io::Result<()>> {
        let position = self.input.position() as usize;
        if position < self.input.get_ref().len() {
            let n = buf.remaining().min(self.input.get_ref().len() - position);
            buf.put_slice(&self.input.get_ref()[position..position + n]);
            self.input.set_position((position + n) as u64);
            return Poll::Ready(Ok(()));
        }
        match self.cut {
            Cut::Read(kind) => {
                self.hit = true;
                if kind == ErrorKind::UnexpectedEof {
                    Poll::Ready(Ok(()))
                } else {
                    Poll::Ready(Err(std::io::Error::new(kind, "forced caller read cut")))
                }
            }
            Cut::Write(_) => Poll::Pending,
        }
    }
}
impl AsyncWrite for CutClient {
    fn poll_write(
        mut self: Pin<&mut Self>,
        _: &mut Context<'_>,
        bytes: &[u8],
    ) -> Poll<std::io::Result<usize>> {
        if self.hit && matches!(self.cut, Cut::Write(_)) {
            return Poll::Ready(Err(ErrorKind::BrokenPipe.into()));
        }
        self.output.extend_from_slice(bytes);
        if self.output.len() >= 4
            && self.output.len()
                == 4 + u32::from_be_bytes(self.output[..4].try_into().unwrap()) as usize
        {
            let frame = crate::replay::decode(&self.output).unwrap();
            self.output.clear();
            if frame["type"] == "accepted" {
                self.accepted = true;
            }
            if let Cut::Write(kind) = self.cut {
                if frame["type"] == kind && (kind != "heartbeat" || self.accepted) {
                    self.hit = true;
                    return Poll::Ready(Err(ErrorKind::BrokenPipe.into()));
                }
            }
            self.sent.push(frame);
        }
        Poll::Ready(Ok(bytes.len()))
    }
    fn poll_flush(self: Pin<&mut Self>, _: &mut Context<'_>) -> Poll<std::io::Result<()>> {
        Poll::Ready(Ok(()))
    }
    fn poll_shutdown(self: Pin<&mut Self>, _: &mut Context<'_>) -> Poll<std::io::Result<()>> {
        Poll::Ready(Ok(()))
    }
}

async fn interrupted(cut: Cut, frames: &[Value]) {
    let (runtime, fake, storage) = fixture();
    let first = claims(JOB);
    claim(&runtime, &first);
    runtime
        .host
        .lock()
        .unwrap()
        .begin_job_boot(JOB)
        .unwrap()
        .unwrap();
    // A busy workspace holds the upload in the host spool without a runner or
    // a timing-dependent boot future competing with the chosen transport cut.
    let c = claims(JOB2);
    let id = claim(&runtime, &c);
    let mut client = CutClient::new(frames, cut);
    let _ = serve(runtime.clone(), &c, id, &mut client).await;
    assert!(client.hit, "the selected transport cut must be reached");
    let snapshot = runtime.host.lock().unwrap().snapshot.clone();
    let terminal = snapshot.billing[JOB2]
        .terminal
        .as_ref()
        .expect("durable no-start terminal");
    assert_eq!(
        terminal["outcome"],
        json!({"type":"refused_before_start","reason":"transfer_interrupted"})
    );
    assert_eq!(terminal["ran"], "none");
    assert_eq!(terminal["wall_ms"], 0);
    assert_eq!(terminal["queue_wait_ms"], 0);
    assert_eq!(terminal["bundle_bytes"], 0);
    assert!(!snapshot.leases[JOB2].delivered);
    assert_eq!(
        snapshot.zero_usage[JOB2].started_at_ms,
        snapshot.zero_usage[JOB2].ended_at_ms
    );
    assert!(fake.calls.lock().unwrap().is_empty());
    assert!(!storage.spool(JOB2).exists());
    let mut reattached = CutClient::new(
        &[hello(), json!({"type":"attach","job_id":JOB2,"from_seq":0})],
        Cut::Read(ErrorKind::UnexpectedEof),
    );
    live(&runtime, &c, &mut reattached).await.unwrap();
    assert_eq!(
        reattached
            .sent
            .iter()
            .find(|frame| frame["type"] == "terminal"),
        Some(terminal)
    );
    assert!(reattached
        .sent
        .iter()
        .any(|frame| frame["type"] == "hosted_terminal"));
}

#[tokio::test(start_paused = true)]
async fn interrupted_upload_read_before_hello_records_no_start() {
    interrupted(Cut::Read(ErrorKind::UnexpectedEof), &[]).await;
}

#[tokio::test(start_paused = true)]
async fn interrupted_upload_read_before_run_records_no_start() {
    interrupted(Cut::Read(ErrorKind::UnexpectedEof), &[hello()]).await;
}

#[tokio::test(start_paused = true)]
async fn interrupted_upload_reset_after_acceptance_records_no_start() {
    interrupted(Cut::Read(ErrorKind::ConnectionReset), &[hello(), run(JOB2)]).await;
}

#[tokio::test(start_paused = true)]
async fn interrupted_upload_eof_after_acceptance_records_no_start() {
    interrupted(Cut::Read(ErrorKind::UnexpectedEof), &[hello(), run(JOB2)]).await;
}

#[tokio::test(start_paused = true)]
async fn interrupted_upload_lease_reply_records_no_start() {
    interrupted(Cut::Write("lease_ok"), &[]).await;
}

#[tokio::test(start_paused = true)]
async fn interrupted_upload_hello_reply_records_no_start() {
    interrupted(Cut::Write("hello"), &[hello()]).await;
}

#[tokio::test(start_paused = true)]
async fn interrupted_upload_have_reply_records_no_start() {
    interrupted(
        Cut::Write("have_reply"),
        &[
            hello(),
            json!({"type":"have","repositories":[],"git_dependencies":[]}),
        ],
    )
    .await;
}

#[tokio::test(start_paused = true)]
async fn interrupted_upload_accepted_reply_records_no_start() {
    interrupted(Cut::Write("accepted"), &[hello(), run(JOB2)]).await;
}

#[tokio::test(start_paused = true)]
async fn interrupted_upload_withdraw_reply_records_no_start() {
    let mut request = run(JOB2);
    request["queue_wait_limit_s"] = json!(30);
    interrupted(
        Cut::Write("withdraw_reply"),
        &[hello(), request, json!({"type":"withdraw","job_id":JOB2})],
    )
    .await;
}

#[tokio::test(start_paused = true)]
async fn interrupted_upload_heartbeat_after_acceptance_records_no_start() {
    interrupted(
        Cut::Write("heartbeat"),
        &[
            hello(),
            run(JOB2),
            json!({"type":"bundle_chunk","index":0,"offset":0,"data":"YQ=="}),
        ],
    )
    .await;
}

#[tokio::test]
async fn replay_disconnect_before_delivery_is_returned_to_session_settlement() {
    let (runtime, _, _) = fixture();
    let c = claims(JOB);
    claim(&runtime, &c);
    // Before the replay worker authorizes Run, an owner disconnect must still
    // reach serve's no-start settlement, rather than look like a live detach.
    let mut client = CutClient::new(&[], Cut::Read(ErrorKind::UnexpectedEof));
    let error = stream::serve(&runtime, &c, &mut client, false, Reader::default(), true)
        .await
        .expect_err("an upload owner disconnect is returned for settlement");
    assert!(error.downcast_ref::<std::io::Error>().is_some());
    assert!(!runtime.host.lock().unwrap().snapshot.leases[JOB].delivered);
}
