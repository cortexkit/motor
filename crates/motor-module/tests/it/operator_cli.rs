//! Direct callers (`ck motor run`) against the real handler and an in-process
//! runner. A small stand-in for the subc daemon speaks the module wire to the
//! handler: it binds routes with the principal the daemon would attest and
//! relays requests, so admission, ownership and the CLI's drive loop are the
//! production code paths. Only the SDK consumer glue (`cli::SubcClient`) is
//! not exercised here; it needs a live daemon.
use super::recovery::{Cut, Fixture};
use crate::{
    cli::{self, ExecClient, RecordStream, StreamFailure, SubmitError},
    module::{self, Handler},
    Config,
};
use motor_protocol::{AttachRequest, CancelRequest, Outcome, RunRequest, StreamRecord, Uuid};
use serde_json::{json, Value};
use std::{
    collections::HashMap,
    fs,
    io::Write,
    os::unix::fs::OpenOptionsExt,
    sync::{
        atomic::{AtomicU64, Ordering},
        Arc, Mutex,
    },
    time::Duration,
};
use subc_protocol::{Flags, Frame, FrameType, ModuleHelloAckBody, Priority, PROTOCOL_VERSION};
use subc_transport::{authenticate_server, read_frame, write_frame};
use tokio::{io::AsyncWriteExt, net::tcp::OwnedWriteHalf, sync::mpsc, task::JoinHandle};

const DIRECT_CHANNEL: u16 = 7;
const OTHER_DIRECT_CHANNEL: u16 = 8;
const AFT_CHANNEL: u16 = 9;

type Waiters = Arc<Mutex<HashMap<(u16, u64), mpsc::UnboundedSender<Frame>>>>;

/// The daemon's side of one module connection. Replies are matched to their
/// request by channel and correlation ID, so several requests can be in
/// flight at once, as they are when a run streams while a cancel is sent.
struct Daemon {
    writer: tokio::sync::Mutex<OwnedWriteHalf>,
    waiters: Waiters,
    corr: AtomicU64,
    reader: JoinHandle<()>,
    serving: JoinHandle<()>,
}

impl Drop for Daemon {
    fn drop(&mut self) {
        self.reader.abort();
        self.serving.abort();
    }
}

impl Daemon {
    async fn start(f: &Fixture, config: Config) -> Arc<Self> {
        let path = f
            .scratch
            .join(format!("connection-{}.json", Uuid::now_v7().simple()));
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let connection = subc_transport::ConnectionInfo {
            schema: subc_transport::SCHEMA_VERSION,
            wire_version: None,
            endpoints: vec![subc_transport::Endpoint {
                host: "127.0.0.1".into(),
                port: listener.local_addr().unwrap().port(),
            }],
            key: subc_transport::generate_key().unwrap(),
            daemon_id: subc_transport::generate_daemon_id().unwrap(),
            pid: std::process::id(),
            daemon_ver: "motor-test".into(),
        };
        fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(&path)
            .and_then(|mut file| file.write_all(&serde_json::to_vec(&connection).unwrap()))
            .unwrap();
        // The motor is opened here against the in-process runner, so the
        // handler never opens storage or SSH from the hello acknowledgement.
        let handler = Handler::new(config).with_motor(f.motor());
        let boot = tokio::spawn(async move {
            subc_client_rs::serve_with_handle(&path, module::manifest("ck-motor"), handler).await
        });
        let (mut stream, _) = listener.accept().await.unwrap();
        authenticate_server(
            &mut stream,
            &connection.key,
            &connection.daemon_id,
            &connection.daemon_ver,
            Duration::from_secs(2),
        )
        .await
        .unwrap();
        let hello = read_frame(&mut stream).await.unwrap().unwrap();
        assert_eq!(hello.header.ty, FrameType::Hello);
        let ack = ModuleHelloAckBody {
            negotiated_ver: PROTOCOL_VERSION,
            subc_ops: Vec::new(),
            subc_capabilities: Vec::new(),
            storage: None,
            machine_id: None,
        };
        write_frame(
            &mut stream,
            &frame(FrameType::HelloAck, 0, 1, serde_json::to_vec(&ack).unwrap()),
        )
        .await
        .unwrap();
        let (_, serving) = boot.await.unwrap().unwrap();
        let serving = tokio::spawn(async move {
            let _ = serving.await;
        });
        let (mut read, write) = stream.into_split();
        let waiters: Waiters = Arc::default();
        let routed = waiters.clone();
        let reader = tokio::spawn(async move {
            while let Ok(Some(frame)) = read_frame(&mut read).await {
                let key = (frame.header.channel, frame.header.corr);
                if let Some(waiter) = routed.lock().unwrap().get(&key) {
                    let _ = waiter.send(frame);
                }
            }
        });
        Arc::new(Self {
            writer: tokio::sync::Mutex::new(write),
            waiters,
            corr: AtomicU64::new(10),
            reader,
            serving,
        })
    }

    async fn send(&self, channel: u16, body: Value) -> mpsc::UnboundedReceiver<Frame> {
        let corr = self.corr.fetch_add(1, Ordering::SeqCst);
        let (tx, rx) = mpsc::unbounded_channel();
        self.waiters.lock().unwrap().insert((channel, corr), tx);
        let mut writer = self.writer.lock().await;
        write_frame(
            &mut *writer,
            &frame(
                FrameType::Request,
                channel,
                corr,
                serde_json::to_vec(&body).unwrap(),
            ),
        )
        .await
        .unwrap();
        writer.flush().await.unwrap();
        rx
    }

    /// Bind `channel` the way the daemon relays a route open, stamped with
    /// `principal`. The bind's `identity` field is the caller's own unverified
    /// claim (here it even names AFT's harness); admission must depend only on
    /// the daemon-attested principal.
    async fn bind(&self, channel: u16, principal: Value) -> Frame {
        let mut reply = self
            .send(
                0,
                json!({
                    "op":"route.bind", "route_channel":channel, "epoch":1,
                    "target":{"kind":"management_surface", "module_id":"ck-motor"},
                    "identity":{"project_root":"/repo", "harness":"aft", "session":"s"},
                    "principal":principal, "scope":null
                }),
            )
            .await;
        next(&mut reply).await
    }

    async fn request(&self, channel: u16, op: &str, params: Value) -> Frame {
        let mut reply = self.send(channel, json!({"op":op, "params":params})).await;
        next(&mut reply).await
    }
}

fn frame(ty: FrameType, channel: u16, corr: u64, body: Vec<u8>) -> Frame {
    Frame::build(
        ty,
        Flags::new(false, Priority::Passive, false),
        channel,
        if channel == 0 { 0 } else { 1 },
        corr,
        body,
    )
    .unwrap()
}

async fn next(frames: &mut mpsc::UnboundedReceiver<Frame>) -> Frame {
    tokio::time::timeout(Duration::from_secs(40), frames.recv())
        .await
        .expect("no reply from the module")
        .expect("reply channel closed")
}

fn direct() -> Value {
    json!({"kind":"direct"})
}
fn aft() -> Value {
    json!({"kind":"reserved", "module_id":"aft"})
}

fn error(frame: &Frame) -> Value {
    assert_eq!(frame.header.ty, FrameType::Error, "{:?}", body(frame));
    serde_json::from_slice(&frame.body).unwrap()
}
fn body(frame: &Frame) -> String {
    String::from_utf8_lossy(&frame.body).into_owned()
}

/// A Direct caller's `ck motor run` over this stand-in daemon.
struct WireClient {
    daemon: Arc<Daemon>,
    channel: u16,
}

struct WireStream(mpsc::UnboundedReceiver<Frame>);

#[subc_client_rs::async_trait]
impl RecordStream for WireStream {
    async fn next(&mut self) -> Result<Option<Vec<u8>>, StreamFailure> {
        let frame = next(&mut self.0).await;
        match frame.header.ty {
            FrameType::StreamData => Ok(Some(frame.body)),
            FrameType::StreamEnd => Ok(None),
            _ => {
                let body: Value = serde_json::from_slice(&frame.body).unwrap_or(Value::Null);
                Err(StreamFailure {
                    code: body["code"].as_str().map(str::to_owned),
                    message: body["message"].as_str().unwrap_or_default().to_owned(),
                })
            }
        }
    }
}

#[subc_client_rs::async_trait]
impl ExecClient for WireClient {
    async fn run(&self, request: &RunRequest) -> Result<Box<dyn RecordStream>, SubmitError> {
        let frames = self
            .daemon
            .send(self.channel, json!({"op":"exec.run", "params":request}))
            .await;
        Ok(Box::new(WireStream(frames)))
    }
    async fn cancel(&self, job_id: Uuid) -> Result<(), String> {
        let reply = self
            .daemon
            .request(
                self.channel,
                "exec.cancel",
                serde_json::to_value(CancelRequest::new(job_id)).unwrap(),
            )
            .await;
        match reply.header.ty {
            FrameType::Response => Ok(()),
            _ => Err(body(&reply)),
        }
    }
}

/// Collects what `drive` writes, in arrival order, and wakes a waiter when a
/// marker line appears on stdout.
/// Each write, tagged "out" or "err".
type Chunks = Arc<Mutex<Vec<(&'static str, Vec<u8>)>>>;

#[derive(Clone, Default)]
struct Terminal {
    chunks: Chunks,
    marker: Arc<tokio::sync::Notify>,
}
struct Side(&'static str, Terminal);
impl Write for Side {
    fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
        self.1.chunks.lock().unwrap().push((self.0, bytes.to_vec()));
        if self.0 == "out" && String::from_utf8_lossy(bytes).contains("started") {
            self.1.marker.notify_one();
        }
        Ok(bytes.len())
    }
    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}
impl Terminal {
    fn text(&self, side: &str) -> String {
        self.chunks
            .lock()
            .unwrap()
            .iter()
            .filter(|(s, _)| *s == side)
            .map(|(_, bytes)| String::from_utf8_lossy(bytes).into_owned())
            .collect()
    }
    /// Lines in the order they arrived, tagged with their stream.
    fn lines(&self) -> Vec<String> {
        self.chunks
            .lock()
            .unwrap()
            .iter()
            .flat_map(|(side, bytes)| {
                String::from_utf8_lossy(bytes)
                    .lines()
                    .map(|line| format!("{side}:{line}"))
                    .collect::<Vec<_>>()
            })
            .collect()
    }
}

fn direct_config(f: &Fixture) -> Config {
    Config {
        admit_direct_exec: true,
        ..f.config.clone()
    }
}

/// A request with a timeout. Long-running test commands `exec` their last
/// step: the fixture runner signals the shell it started, and a shell waiting
/// on a child would otherwise defer that signal until the child exits.
fn timed(f: &Fixture, command: &str, timeout: u64) -> RunRequest {
    f.request(command).with_timeout(timeout)
}

/// Run `request` through the CLI's drive loop as a Direct caller.
async fn drive(
    daemon: &Arc<Daemon>,
    request: RunRequest,
    terminal: &Terminal,
    interrupts: &mut mpsc::Receiver<()>,
) -> u8 {
    let client = WireClient {
        daemon: daemon.clone(),
        channel: DIRECT_CHANNEL,
    };
    let mut out = Side("out", terminal.clone());
    let mut err = Side("err", terminal.clone());
    tokio::time::timeout(
        Duration::from_secs(60),
        cli::drive(&client, &request, &mut out, &mut err, interrupts),
    )
    .await
    .expect("drive did not finish")
}

/// Every record of one `exec.run` on `channel`, through its terminal.
async fn run_records(daemon: &Daemon, channel: u16, request: RunRequest) -> Vec<StreamRecord> {
    let mut frames = daemon
        .send(channel, json!({"op":"exec.run", "params":request}))
        .await;
    let mut records = Vec::new();
    loop {
        let frame = next(&mut frames).await;
        assert_eq!(frame.header.ty, FrameType::StreamData, "{}", body(&frame));
        let record: StreamRecord = serde_json::from_slice(&frame.body).unwrap();
        let terminal = matches!(record, StreamRecord::Terminal(_));
        records.push(record);
        if terminal {
            return records;
        }
    }
}

fn last_terminal(records: &[StreamRecord]) -> &motor_protocol::TerminalRecord {
    match records.last() {
        Some(StreamRecord::Terminal(terminal)) => terminal,
        other => panic!("expected a terminal record last, got {other:?}"),
    }
}

#[tokio::test]
async fn direct_bind_is_refused_as_before_while_the_switch_is_off() {
    let f = Fixture::new(Cut::None, 28).await;
    assert!(
        !Config::default().admit_direct_exec,
        "the switch is off by default"
    );
    let daemon = Daemon::start(&f, f.config.clone()).await;
    let refused = error(&daemon.bind(DIRECT_CHANNEL, direct()).await);
    assert_eq!(refused["code"], "motor_direct_refused");
    assert_eq!(refused["message"], "ck-motor refuses direct binds");
    assert_eq!(
        daemon.bind(AFT_CHANNEL, aft()).await.header.ty,
        FrameType::Response
    );
}

#[tokio::test]
async fn switched_on_direct_routes_reach_only_the_four_exec_operations() {
    let f = Fixture::new(Cut::None, 28).await;
    let daemon = Daemon::start(&f, direct_config(&f)).await;
    assert_eq!(
        daemon.bind(DIRECT_CHANNEL, direct()).await.header.ty,
        FrameType::Response
    );
    let prepare = json!({
        "workspace_key": f.key, "repository_root": f.main,
        "base_commit": "HEAD", "siblings": []
    });
    for (op, params) in [
        ("workspace.prepare", prepare),
        ("workspace.drop", json!({"workspace_key": f.key})),
        ("runner.health", json!({})),
        ("unknown", json!({})),
    ] {
        let refused = error(&daemon.request(DIRECT_CHANNEL, op, params).await);
        assert_eq!(
            refused["code"], "motor_op_not_permitted_for_principal",
            "{op}"
        );
        assert_eq!(
            refused["message"],
            format!("ck-motor refuses {op} for direct"),
            "{op}"
        );
    }
    // The aggregate report covers every caller's jobs, so it needs a job ID.
    let aggregate = error(
        &daemon
            .request(DIRECT_CHANNEL, "exec.status", json!({}))
            .await,
    );
    assert_eq!(aggregate["code"], "motor_op_not_permitted_for_principal");
    // The other three pass the operation gate and reach the ownership check.
    let stranger = Uuid::now_v7();
    for (op, params) in [
        ("exec.attach", json!(AttachRequest::new(stranger, 0))),
        ("exec.cancel", json!(CancelRequest::new(stranger))),
        ("exec.status", json!({"job_id": stranger})),
    ] {
        let reply = error(&daemon.request(DIRECT_CHANNEL, op, params).await);
        assert_eq!(reply["code"], "motor_job_not_found", "{op}");
    }
    assert!(
        f.proxy.frames.lock().unwrap().is_empty(),
        "a refused operation must not reach the runner"
    );
}

#[tokio::test]
async fn a_direct_route_cannot_reach_another_routes_job_and_aft_still_can() {
    let f = Fixture::new(Cut::None, 28).await;
    let daemon = Daemon::start(&f, direct_config(&f)).await;
    for (channel, principal) in [
        (DIRECT_CHANNEL, direct()),
        (OTHER_DIRECT_CHANNEL, direct()),
        (AFT_CHANNEL, aft()),
    ] {
        assert_eq!(
            daemon.bind(channel, principal).await.header.ty,
            FrameType::Response
        );
    }
    let mut run = daemon
        .send(
            DIRECT_CHANNEL,
            json!({"op":"exec.run", "params": timed(&f, "echo started; exec sleep 30", 600)}),
        )
        .await;
    let job_id = loop {
        let frame = next(&mut run).await;
        assert_eq!(frame.header.ty, FrameType::StreamData, "{}", body(&frame));
        match serde_json::from_slice(&frame.body).unwrap() {
            StreamRecord::Accepted(accepted) => break accepted.job_id,
            StreamRecord::Terminal(terminal) => panic!("job ended early: {terminal:?}"),
            _ => {}
        }
    };
    let stranger = Uuid::now_v7();
    let attach = |id| json!(AttachRequest::new(id, 0));
    let cancel = |id| json!(CancelRequest::new(id));
    let status = |id| json!({ "job_id": id });
    for (op, params) in [
        ("exec.attach", &attach as &dyn Fn(Uuid) -> Value),
        ("exec.cancel", &cancel),
        ("exec.status", &status),
    ] {
        let theirs = daemon
            .request(OTHER_DIRECT_CHANNEL, op, params(job_id))
            .await;
        let missing = daemon
            .request(OTHER_DIRECT_CHANNEL, op, params(stranger))
            .await;
        assert_eq!(error(&theirs)["code"], "motor_job_not_found", "{op}");
        // Byte-identical to a job that does not exist, so a direct caller
        // can't learn that another caller's job ID is live.
        assert_eq!(theirs.header.ty, missing.header.ty, "{op}");
        assert_eq!(theirs.body, missing.body, "{op}");
    }
    // AFT's access to every job is unchanged.
    let checked = daemon
        .request(AFT_CHANNEL, "exec.status", status(job_id))
        .await;
    assert_eq!(checked.header.ty, FrameType::Response, "{}", body(&checked));
    let checked: motor_protocol::JobStatusReply = serde_json::from_slice(&checked.body).unwrap();
    assert_eq!(checked.job_id, job_id);
    assert!(
        checked.terminal.is_none(),
        "the other route's cancel must not have reached the job: {checked:?}"
    );
    // The route that started the job can check and cancel it.
    let own = daemon
        .request(DIRECT_CHANNEL, "exec.status", status(job_id))
        .await;
    assert_eq!(own.header.ty, FrameType::Response, "{}", body(&own));
    let cancelled = daemon
        .request(DIRECT_CHANNEL, "exec.cancel", cancel(job_id))
        .await;
    assert_eq!(
        cancelled.header.ty,
        FrameType::Response,
        "{}",
        body(&cancelled)
    );
    let terminal = loop {
        let frame = next(&mut run).await;
        assert_eq!(frame.header.ty, FrameType::StreamData, "{}", body(&frame));
        if let StreamRecord::Terminal(terminal) = serde_json::from_slice(&frame.body).unwrap() {
            break terminal;
        }
    };
    assert_eq!(
        cli::exit_code(&terminal),
        cli::EXIT_CANCELLED,
        "{terminal:?}"
    );
}

#[tokio::test]
async fn direct_runs_need_a_timeout_within_the_cap_and_aft_runs_do_not() {
    let f = Fixture::new(Cut::None, 28).await;
    let config = Config {
        direct_exec_max_timeout_secs: 100,
        ..direct_config(&f)
    };
    let daemon = Daemon::start(&f, config).await;
    for (channel, principal) in [(DIRECT_CHANNEL, direct()), (AFT_CHANNEL, aft())] {
        assert_eq!(
            daemon.bind(channel, principal).await.header.ty,
            FrameType::Response
        );
    }
    for (request, detail) in [
        (
            f.request("true"),
            "a direct exec.run needs a timeout of at most 100 seconds: ",
        ),
        (
            timed(&f, "true", 101),
            "timeout 101 seconds exceeds the direct-job cap of 100 seconds: ",
        ),
    ] {
        let records = run_records(&daemon, DIRECT_CHANNEL, request).await;
        assert_eq!(records.len(), 1, "{records:?}");
        let terminal = last_terminal(&records);
        assert_eq!(
            terminal.outcome,
            Outcome::RefusedBeforeStart {
                reason: motor_protocol::RefusalReason::RequestRejected
            }
        );
        assert_eq!(terminal.ran, Some(motor_protocol::Ran::None));
        let refusal = terminal.refusal_detail.as_deref().unwrap();
        assert!(
            refusal.starts_with(&format!("direct_timeout_refused: {detail}")),
            "{refusal}"
        );
        assert_eq!(terminal.retry_after_ms(), None);
        assert!(
            refusal.contains("can't re-attach to or cancel its job"),
            "{refusal}"
        );
    }
    assert!(
        f.proxy.frames.lock().unwrap().is_empty(),
        "a refused direct run must not reach the runner"
    );
    // At the cap a direct run is admitted; AFT is not held to the cap at all.
    for (channel, request) in [
        (DIRECT_CHANNEL, timed(&f, "true", 100)),
        (AFT_CHANNEL, f.request("true")),
        (AFT_CHANNEL, timed(&f, "true", 101)),
    ] {
        let records = run_records(&daemon, channel, request).await;
        assert_eq!(
            last_terminal(&records).outcome,
            Outcome::Exit { code: 0 },
            "channel {channel}: {records:?}"
        );
    }
}

#[tokio::test]
async fn only_direct_routes_mark_gates_and_extend_the_queue_wait() {
    let f = Fixture::new(Cut::None, 28).await;
    let daemon = Daemon::start(&f, direct_config(&f)).await;
    for (channel, principal) in [(DIRECT_CHANNEL, direct()), (AFT_CHANNEL, aft())] {
        assert_eq!(
            daemon.bind(channel, principal).await.header.ty,
            FrameType::Response
        );
    }
    for (channel, override_wait, expected_wait, expected_gate) in [
        (DIRECT_CHANNEL, None, 1800, true),
        (DIRECT_CHANNEL, Some(1), 1800, true),
        (AFT_CHANNEL, None, 300, false),
        (AFT_CHANNEL, Some(123), 123, false),
    ] {
        let mut request = timed(&f, "true", 60);
        request.queue_wait_limit_s = override_wait;
        let records = run_records(&daemon, channel, request).await;
        let terminal = last_terminal(&records);
        assert_eq!(terminal.outcome, Outcome::Exit { code: 0 });
        let runs = f.proxy.frames.lock().unwrap();
        let run = runs
            .iter()
            .find_map(|frame| match frame {
                motor_protocol::Frame::Run(run) if run.job_id == terminal.job_id => Some(run),
                _ => None,
            })
            .unwrap();
        assert_eq!(run.gate_job, expected_gate);
        assert_eq!(run.queue_wait_limit_s, expected_wait);
    }
    // The agent protocol RunRequest has no gate_job field; motor's private Run does.
    let mut params = json!(timed(&f, "true", 60));
    params["gate_job"] = json!(true);
    let request: motor_protocol::RunRequest = serde_json::from_value(params).unwrap();
    let records = run_records(&daemon, AFT_CHANNEL, request).await;
    assert_eq!(last_terminal(&records).outcome, Outcome::Exit { code: 0 });
    assert!(f.proxy.frames.lock().unwrap().iter().any(|frame| matches!(frame,
        motor_protocol::Frame::Run(run) if run.job_id == last_terminal(&records).job_id && !run.gate_job)));
}

#[tokio::test]
async fn cli_streams_both_outputs_in_order_and_exits_with_the_commands_code() {
    let captured = crate::logging::Captured::default();
    let _log = crate::logging::scoped(captured.clone());
    let f = Fixture::new(Cut::None, 28).await;
    let daemon = Daemon::start(&f, direct_config(&f)).await;
    assert_eq!(
        daemon.bind(DIRECT_CHANNEL, direct()).await.header.ty,
        FrameType::Response
    );
    let terminal = Terminal::default();
    let (_tx, mut interrupts) = mpsc::channel(1);
    // Each stream must arrive complete and in order. The order across the two
    // streams is not checked: they come through separate pipes, and the runner
    // reads whichever has data, so under load a later stdout line can arrive
    // before an earlier stderr line.
    let command = "echo out1; sleep 0.3; echo err1 >&2; sleep 0.3; echo out2; sleep 0.3; echo err2 >&2; exit 3";
    let code = drive(&daemon, timed(&f, command, 600), &terminal, &mut interrupts).await;
    assert_eq!(code, 3, "{:?}", terminal.lines());
    assert_eq!(terminal.text("out"), "out1\nout2\n");
    assert_eq!(terminal.text("err"), "err1\nerr2\n");
    let log = captured.text();
    let submitted = log
        .lines()
        .find(|line| line.contains("direct job submitted"))
        .unwrap_or_else(|| panic!("no submission line in {log}"));
    assert!(submitted.contains("caller=direct"), "{submitted}");
    let job = submitted
        .split_whitespace()
        .find(|word| word.starts_with("job_id="))
        .unwrap();
    for event in ["direct job accepted", "direct job terminal"] {
        assert!(
            log.lines().any(|line| line.contains(event)
                && line.contains("caller=direct")
                && line.contains(job)),
            "no {event:?} line for {job} in {log}"
        );
    }
}

#[tokio::test]
async fn cli_returns_the_refused_code_with_the_reason_and_detail() {
    let f = Fixture::new(Cut::None, 28).await;
    let daemon = Daemon::start(&f, direct_config(&f)).await;
    assert_eq!(
        daemon.bind(DIRECT_CHANNEL, direct()).await.header.ty,
        FrameType::Response
    );
    let terminal = Terminal::default();
    let (_tx, mut interrupts) = mpsc::channel(1);
    let over = crate::config::DEFAULT_DIRECT_EXEC_MAX_TIMEOUT_SECS + 1;
    let code = drive(&daemon, timed(&f, "true", over), &terminal, &mut interrupts).await;
    assert_eq!(code, cli::EXIT_REFUSED);
    let err = terminal.text("err");
    assert!(
        err.starts_with(&format!(
            "ck motor run: refused before start: request_rejected: direct_timeout_refused: timeout {over} seconds exceeds"
        )),
        "{err}"
    );
    assert_eq!(terminal.text("out"), "");
}

#[tokio::test]
async fn cli_interrupt_cancels_the_job_and_returns_the_cancelled_code() {
    let f = Fixture::new(Cut::None, 28).await;
    let daemon = Daemon::start(&f, direct_config(&f)).await;
    assert_eq!(
        daemon.bind(DIRECT_CHANNEL, direct()).await.header.ty,
        FrameType::Response
    );
    let terminal = Terminal::default();
    let (tx, mut interrupts) = mpsc::channel(1);
    let request = timed(&f, "echo started; exec sleep 30", 600);
    let (code, ()) = tokio::join!(drive(&daemon, request, &terminal, &mut interrupts), async {
        terminal.marker.notified().await;
        tx.send(()).await.unwrap();
    });
    assert_eq!(code, cli::EXIT_CANCELLED, "{:?}", terminal.lines());
    assert!(
        terminal.text("err").contains("was cancelled"),
        "{:?}",
        terminal.lines()
    );
}

#[tokio::test]
async fn cli_returns_the_timeout_code_when_the_runner_stops_the_command() {
    let f = Fixture::new(Cut::None, 28).await;
    let daemon = Daemon::start(&f, direct_config(&f)).await;
    assert_eq!(
        daemon.bind(DIRECT_CHANNEL, direct()).await.header.ty,
        FrameType::Response
    );
    let terminal = Terminal::default();
    let (_tx, mut interrupts) = mpsc::channel(1);
    let request = timed(&f, "echo started; exec sleep 30", 5);
    let (code, ()) = tokio::join!(drive(&daemon, request, &terminal, &mut interrupts), async {
        terminal.marker.notified().await;
        // The fixture runner's clock only moves when a test moves it.
        f.clock.fetch_add(10_000, Ordering::SeqCst);
    });
    assert_eq!(code, cli::EXIT_TIMEOUT, "{:?}", terminal.lines());
    assert!(
        terminal.text("err").contains("reached its timeout"),
        "{:?}",
        terminal.lines()
    );
}
