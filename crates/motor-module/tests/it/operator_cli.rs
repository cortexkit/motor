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
        Self::start_with_module_id(f, config, "ck-motor").await
    }

    async fn start_with_module_id(f: &Fixture, config: Config, module_id: &str) -> Arc<Self> {
        Self::start_with_tokens(f, config, module_id, None).await
    }
    async fn start_with_tokens(
        f: &Fixture,
        config: Config,
        module_id: &str,
        tokens: Option<Arc<crate::AccountTokens>>,
    ) -> Arc<Self> {
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
        let motor = super::reopen_motor(|| {
            crate::Motor::with_connector(
                f.config.clone(),
                &f.scratch.join("module-state"),
                Arc::new(f.proxy.clone()),
            )
        })
        .await;
        let handler = Handler::new(config)
            .with_module_id(module_id)
            .with_motor(motor);
        let handler = if let Some(tokens) = tokens {
            handler.with_account_tokens(tokens)
        } else {
            handler
        };
        let module_id = module_id.to_owned();
        let boot = tokio::spawn(async move {
            subc_client_rs::serve_with_handle(&path, module::manifest(&module_id), handler).await
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
    /// Send `hosted: true` with each run, as `ck motor run --hosted` does.
    hosted: bool,
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
        let mut params = json!(request);
        if self.hosted {
            params["hosted"] = json!(true);
        }
        let frames = self
            .daemon
            .send(self.channel, json!({"op":"exec.run", "params":params}))
            .await;
        Ok(Box::new(WireStream(frames)))
    }
    async fn attach(
        &self,
        job_id: Uuid,
        secret: &str,
    ) -> Result<Box<dyn RecordStream>, SubmitError> {
        let frames = self.daemon.send(self.channel, json!({"op":"exec.attach", "params":AttachRequest::new(job_id, 0), "direct_secret":secret})).await;
        Ok(Box::new(WireStream(frames)))
    }
    async fn status(
        &self,
        job_id: Uuid,
        secret: &str,
    ) -> Result<motor_protocol::JobStatusReply, SubmitError> {
        let mut frames = self
            .daemon
            .send(
                self.channel,
                json!({"op":"exec.status", "params":{"job_id":job_id}, "direct_secret":secret}),
            )
            .await;
        let reply = next(&mut frames).await;
        if reply.header.ty == FrameType::Response {
            Ok(serde_json::from_slice(&reply.body).unwrap())
        } else {
            Err(SubmitError::NotSent(
                error(&reply)["message"].as_str().unwrap().into(),
            ))
        }
    }
    async fn cancel(&self, job_id: Uuid, secret: Option<&str>) -> Result<(), String> {
        let mut frames = self.daemon.send(self.channel, json!({"op":"exec.cancel", "params":CancelRequest::new(job_id), "direct_secret":secret})).await;
        let reply = next(&mut frames).await;
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
        hosted: false,
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

async fn direct_submission(
    daemon: &Daemon,
    request: RunRequest,
) -> (Uuid, String, mpsc::UnboundedReceiver<Frame>) {
    let mut frames = daemon
        .send(DIRECT_CHANNEL, json!({"op":"exec.run", "params":request}))
        .await;
    let first = next(&mut frames).await;
    assert_eq!(first.header.ty, FrameType::StreamData, "{}", body(&first));
    let value: Value = serde_json::from_slice(&first.body).unwrap();
    let id =
        serde_json::from_value(value["direct_job"]["job_id"].clone()).expect("one-time job id");
    let secret = value["direct_job"]["secret"]
        .as_str()
        .expect("one-time secret")
        .to_owned();
    assert_eq!(secret.len(), 64);
    (id, secret, frames)
}

async fn collect_frames(frames: &mut mpsc::UnboundedReceiver<Frame>) -> Vec<Frame> {
    let mut result = Vec::new();
    loop {
        let frame = next(frames).await;
        assert_eq!(frame.header.ty, FrameType::StreamData, "{}", body(&frame));
        let terminal = matches!(
            serde_json::from_slice::<StreamRecord>(&frame.body).unwrap(),
            StreamRecord::Terminal(_)
        );
        result.push(frame);
        if terminal {
            return result;
        }
    }
}

#[tokio::test]
async fn direct_secret_reattaches_replays_and_never_leaks() {
    let f = Fixture::new(Cut::None, 28).await;
    let logs = Terminal::default();
    let _logging = crate::logging::scoped(Side("log", logs.clone()));
    let daemon = Daemon::start(&f, direct_config(&f)).await;
    daemon.bind(DIRECT_CHANNEL, direct()).await;
    daemon.bind(OTHER_DIRECT_CHANNEL, direct()).await;
    daemon.bind(AFT_CHANNEL, aft()).await;
    let (id, secret, mut original) = direct_submission(
        &daemon,
        timed(&f, "printf first; printf second; exit 17", 60),
    )
    .await;
    let original = collect_frames(&mut original).await;
    assert!(original
        .iter()
        .all(|frame| !body(frame).contains(&secret) && !body(frame).contains("direct_job")));
    let mut own = daemon
        .send(
            DIRECT_CHANNEL,
            json!({"op":"exec.attach", "params":AttachRequest::new(id, 0)}),
        )
        .await;
    assert!(collect_frames(&mut own)
        .await
        .iter()
        .all(|frame| !body(frame).contains(&secret) && !body(frame).contains("direct_job")));
    drop(daemon);
    let daemon = Daemon::start(&f, direct_config(&f)).await;
    daemon.bind(OTHER_DIRECT_CHANNEL, direct()).await;
    daemon.bind(AFT_CHANNEL, aft()).await;
    for op in ["exec.attach", "exec.status", "exec.cancel"] {
        let runner_frames = f.proxy.frames.lock().unwrap().len();
        let mut refused = Vec::new();
        for (job, presented) in [
            (id, Value::Null),
            (id, json!("f".repeat(64))),
            (id, json!({"secret":secret})),
            (Uuid::now_v7(), json!(secret)),
        ] {
            let mut frames = daemon
                .send(
                    OTHER_DIRECT_CHANNEL,
                    json!({"op":op,"params":{"job_id":job,"from_seq":0},"direct_secret":presented}),
                )
                .await;
            let reply = next(&mut frames).await;
            assert_eq!(error(&reply)["code"], "motor_job_not_found");
            refused.push(reply.body);
        }
        assert!(refused.windows(2).all(|pair| pair[0] == pair[1]), "{op}");
        assert_eq!(
            f.proxy.frames.lock().unwrap().len(),
            runner_frames,
            "a refused secret must not contact the runner"
        );
    }
    let status = daemon
        .send(
            OTHER_DIRECT_CHANNEL,
            json!({"op":"exec.status","params":{"job_id":id},"direct_secret":secret}),
        )
        .await;
    let mut status = status;
    let status = next(&mut status).await;
    assert_eq!(status.header.ty, FrameType::Response, "{}", body(&status));
    assert!(!body(&status).contains(&secret));
    let mut attached = daemon
        .send(
            OTHER_DIRECT_CHANNEL,
            json!({"op":"exec.attach","params":{"job_id":id,"from_seq":0},"direct_secret":secret}),
        )
        .await;
    let replay = collect_frames(&mut attached).await;
    let mut output = Vec::new();
    for frame in &replay {
        assert!(!body(frame).contains(&secret));
        assert!(!body(frame).contains("direct_job"));
        match serde_json::from_slice(&frame.body).unwrap() {
            StreamRecord::Output(record) => output.extend(record.bytes.0),
            StreamRecord::Terminal(terminal) => assert_eq!(cli::exit_code(&terminal), 17),
            _ => {}
        }
    }
    assert_eq!(output, b"firstsecond");
    assert!(!logs.text("log").contains(&secret));
    assert!(
        !logs.text("log").is_empty(),
        "log capture must observe the handler"
    );
    let state = crate::storage::durable_value(&f.scratch.join("module-state"))
        .unwrap()
        .to_string();
    assert!(!state.contains(&secret));
    let mut agent = daemon
        .send(
            AFT_CHANNEL,
            json!({"op":"exec.run", "params":timed(&f, "true", 60)}),
        )
        .await;
    let agent = collect_frames(&mut agent).await;
    assert!(agent
        .iter()
        .all(|frame| !body(frame).contains("direct_job") && !body(frame).contains(&secret)));
    fn scan(path: &std::path::Path, secret: &str) {
        for entry in fs::read_dir(path).unwrap() {
            let entry = entry.unwrap();
            let ty = entry.file_type().unwrap();
            if ty.is_dir() {
                scan(&entry.path(), secret);
            } else if ty.is_file() {
                let bytes = fs::read(entry.path()).unwrap();
                assert!(
                    !bytes
                        .windows(secret.len())
                        .any(|window| window == secret.as_bytes()),
                    "secret persisted in {}",
                    entry.path().display()
                );
            }
        }
    }
    scan(&f.scratch, &secret);
    assert!(!format!("{:?}", f.proxy.frames.lock().unwrap()).contains(&secret));
}

#[tokio::test]
async fn direct_secret_attach_streams_a_running_job_and_original_route_needs_no_secret() {
    let f = Fixture::new(Cut::None, 28).await;
    let daemon = Daemon::start(&f, direct_config(&f)).await;
    daemon.bind(DIRECT_CHANNEL, direct()).await;
    daemon.bind(OTHER_DIRECT_CHANNEL, direct()).await;
    let (id, secret, mut original) = direct_submission(
        &daemon,
        timed(&f, "printf first; sleep 1; printf last; exit 23", 60),
    )
    .await;
    loop {
        let frame = next(&mut original).await;
        if matches!(
            serde_json::from_slice::<StreamRecord>(&frame.body).unwrap(),
            StreamRecord::Output(_)
        ) {
            break;
        }
    }
    let own = daemon
        .request(DIRECT_CHANNEL, "exec.status", json!({"job_id":id}))
        .await;
    assert_eq!(own.header.ty, FrameType::Response);
    let client = WireClient {
        daemon: daemon.clone(),
        channel: OTHER_DIRECT_CHANNEL,
        hosted: false,
    };
    let args = cli::AttachArgs {
        job_id: id,
        secret_file: None,
        status: false,
    };
    let terminal = Terminal::default();
    let (_tx, mut interrupts) = mpsc::channel(1);
    let code = cli::drive_attach(
        &client,
        &args,
        &secret,
        &mut Side("out", terminal.clone()),
        &mut Side("err", terminal.clone()),
        &mut interrupts,
    )
    .await;
    assert_eq!(code, 23);
    assert_eq!(terminal.text("out"), "firstlast");
    assert!(!terminal.text("err").contains(&secret));
}

#[tokio::test]
async fn attached_cli_cancels_with_secret_and_status_is_read_only() {
    let f = Fixture::new(Cut::None, 28).await;
    let daemon = Daemon::start(&f, direct_config(&f)).await;
    daemon.bind(DIRECT_CHANNEL, direct()).await;
    daemon.bind(OTHER_DIRECT_CHANNEL, direct()).await;
    let (id, secret, mut original) =
        direct_submission(&daemon, timed(&f, "echo started; exec sleep 30", 60)).await;
    loop {
        let frame = next(&mut original).await;
        if matches!(
            serde_json::from_slice::<StreamRecord>(&frame.body).unwrap(),
            StreamRecord::Output(_)
        ) {
            break;
        }
    }
    let client = WireClient {
        daemon: daemon.clone(),
        channel: OTHER_DIRECT_CHANNEL,
        hosted: false,
    };
    let mut args = cli::AttachArgs {
        job_id: id,
        secret_file: None,
        status: true,
    };
    let terminal = Terminal::default();
    let (tx, mut interrupts) = mpsc::channel(1);
    let code = cli::drive_attach(
        &client,
        &args,
        &secret,
        &mut Side("out", terminal.clone()),
        &mut Side("err", terminal.clone()),
        &mut interrupts,
    )
    .await;
    assert_eq!(code, 0);
    let status: motor_protocol::JobStatusReply =
        serde_json::from_str(&terminal.text("out")).unwrap();
    assert!(status.terminal.is_none());
    assert!(!terminal.text("out").contains(&secret));
    args.status = false;
    let attached = Terminal::default();
    tx.send(()).await.unwrap();
    let code = cli::drive_attach(
        &client,
        &args,
        &secret,
        &mut Side("out", attached.clone()),
        &mut Side("err", attached.clone()),
        &mut interrupts,
    )
    .await;
    assert_eq!(code, cli::EXIT_CANCELLED);
    assert!(!attached.text("err").contains(&secret));
}

#[tokio::test]
async fn direct_secret_grants_only_its_job_and_no_additional_operations() {
    let f = Fixture::new(Cut::None, 28).await;
    let daemon = Daemon::start(&f, direct_config(&f)).await;
    daemon.bind(DIRECT_CHANNEL, direct()).await;
    daemon.bind(OTHER_DIRECT_CHANNEL, direct()).await;
    daemon.bind(AFT_CHANNEL, aft()).await;
    let (first, secret, mut frames) = direct_submission(&daemon, timed(&f, "true", 60)).await;
    collect_frames(&mut frames).await;
    let (second, other_secret, mut frames) =
        direct_submission(&daemon, timed(&f, "true", 60)).await;
    collect_frames(&mut frames).await;
    assert_ne!(secret, other_secret);
    let agent = run_records(&daemon, AFT_CHANNEL, timed(&f, "true", 60)).await;
    let agent = last_terminal(&agent).job_id;
    for id in [second, agent] {
        for op in ["exec.attach", "exec.status", "exec.cancel"] {
            let mut frames = daemon
                .send(
                    OTHER_DIRECT_CHANNEL,
                    json!({"op":op, "params":{"job_id":id,"from_seq":0}, "direct_secret":secret}),
                )
                .await;
            assert_eq!(
                error(&next(&mut frames).await)["code"],
                "motor_job_not_found"
            );
        }
    }
    let mut frames = daemon
        .send(
            OTHER_DIRECT_CHANNEL,
            json!({"op":"runner.health", "params":{}, "direct_secret":secret}),
        )
        .await;
    assert_eq!(
        error(&next(&mut frames).await)["code"],
        "motor_op_not_permitted_for_principal"
    );
    let mut frames = daemon
        .send(
            OTHER_DIRECT_CHANNEL,
            json!({"op":"exec.status", "params":{"job_id":first}, "direct_secret":secret}),
        )
        .await;
    assert_eq!(next(&mut frames).await.header.ty, FrameType::Response);
}

#[tokio::test]
async fn cli_attach_refusals_are_byte_identical_and_secret_file_hides_the_secret() {
    let f = Fixture::new(Cut::None, 28).await;
    let daemon = Daemon::start(&f, direct_config(&f)).await;
    daemon.bind(DIRECT_CHANNEL, direct()).await;
    daemon.bind(OTHER_DIRECT_CHANNEL, direct()).await;
    let client = WireClient {
        daemon: daemon.clone(),
        channel: DIRECT_CHANNEL,
        hosted: false,
    };
    let terminal = Terminal::default();
    let path = f.scratch.join("reattach.key");
    let file = cli::SecretFile::create(&path).unwrap();
    let (_tx, mut interrupts) = mpsc::channel(1);
    let code = cli::drive_run(
        &client,
        &timed(&f, "exit 19", 60),
        Some(file),
        &mut Side("out", terminal.clone()),
        &mut Side("err", terminal.clone()),
        &mut interrupts,
    )
    .await;
    assert_eq!(code, 19);
    let secret = fs::read_to_string(&path).unwrap();
    let secret = secret.trim();
    assert_eq!(secret.len(), 64);
    assert!(!terminal.text("err").contains(secret));
    assert!(terminal.text("err").contains(path.to_str().unwrap()));
    let id: Uuid = terminal
        .text("err")
        .split("job_id=")
        .nth(1)
        .unwrap()
        .split_whitespace()
        .next()
        .unwrap()
        .parse()
        .unwrap();
    let client = WireClient {
        daemon,
        channel: OTHER_DIRECT_CHANNEL,
        hosted: false,
    };
    for status in [false, true] {
        let mut errors = Vec::new();
        for (job_id, presented) in [
            (id, ""),
            (id, "malformed"),
            (id, &"f".repeat(64)),
            (Uuid::now_v7(), secret),
        ] {
            let args = cli::AttachArgs {
                job_id,
                secret_file: None,
                status,
            };
            let refused = Terminal::default();
            let code = cli::drive_attach(
                &client,
                &args,
                presented,
                &mut Side("out", refused.clone()),
                &mut Side("err", refused.clone()),
                &mut interrupts,
            )
            .await;
            assert_eq!(code, cli::EXIT_REFUSED);
            assert_eq!(refused.text("out"), "");
            errors.push(refused.text("err"));
        }
        assert!(errors.windows(2).all(|pair| pair[0] == pair[1]));
    }
}

#[tokio::test]
async fn direct_bind_is_refused_as_before_while_the_switch_is_off() {
    let f = Fixture::new(Cut::None, 28).await;
    assert!(
        !Config::default().admit_direct_exec,
        "the switch is off by default"
    );
    let daemon = Daemon::start_with_module_id(&f, f.config.clone(), "motor").await;
    let refused = error(&daemon.bind(DIRECT_CHANNEL, direct()).await);
    assert_eq!(refused["code"], "motor_direct_refused");
    assert_eq!(refused["message"], "motor refuses direct binds");
    assert_eq!(
        daemon.bind(AFT_CHANNEL, aft()).await.header.ty,
        FrameType::Response
    );
}

#[tokio::test]
async fn hosted_check_is_direct_only_and_mints_through_real_module_with_hosted_off() {
    use crate::hosted::account::tests::{refusal, token, FakeVault};
    let f = Fixture::new(Cut::None, 28).await;
    let config = direct_config(&f);
    assert!(!config.hosted_runners);
    let payload = motor_lease::base64url(
        &serde_json::to_vec(
            &json!({"aud":"cortexkit-runners", "sub":"acct:operator", "iat":1,"exp":u64::MAX}),
        )
        .unwrap(),
    );
    let jwt = format!("operator-secret-header.{payload}.operator-secret-signature");
    let vault = FakeVault::new(vec![
        Ok(token(&jwt)),
        Ok(refusal("auth_required")),
        Ok(refusal("not_found")),
        Ok(refusal("transient")),
        Ok(refusal("permanent")),
        Ok(json!({})),
    ]);
    let daemon = Daemon::start_with_tokens(&f, config, "motor", Some(vault.client())).await;
    assert_eq!(
        daemon.bind(DIRECT_CHANNEL, direct()).await.header.ty,
        FrameType::Response
    );
    assert_eq!(
        daemon.bind(AFT_CHANNEL, aft()).await.header.ty,
        FrameType::Response
    );
    let denied = error(&daemon.request(AFT_CHANNEL, "hosted.check", json!({})).await);
    assert_eq!(denied["code"], "motor_op_not_permitted_for_principal");
    let logs = crate::logging::Captured::default();
    let _guard = crate::logging::scoped(logs.clone());
    tracing::warn!("hosted operator capture active");
    for status in [
        "ok",
        "auth_required",
        "not_found",
        "transient",
        "permanent",
        "invalid",
    ] {
        let reply = daemon
            .request(DIRECT_CHANNEL, "hosted.check", json!({}))
            .await;
        assert_eq!(reply.header.ty, FrameType::Response);
        let output = body(&reply);
        let value: Value = serde_json::from_str(&output).unwrap();
        assert_eq!(value["status"], status);
        if status == "ok" {
            assert_eq!(value["claims"]["sub"], "acct:operator");
        }
        if status == "transient" {
            assert_eq!(value["retry_after_ms"], 4321);
        }
        for secret in [
            &jwt,
            "operator-secret-header",
            "operator-secret-signature",
            "private-vault-code",
        ] {
            assert!(!output.contains(secret));
            assert!(!logs.text().contains(secret));
        }
    }
    assert_eq!(vault.requests.lock().unwrap().len(), 6);
    assert!(logs.text().contains("hosted operator capture active"));
}

#[tokio::test]
async fn switched_on_direct_routes_reach_only_the_four_exec_operations() {
    let f = Fixture::new(Cut::None, 28).await;
    let daemon = Daemon::start_with_module_id(&f, direct_config(&f), "motor").await;
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
            format!("motor refuses {op} for direct"),
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
    assert_eq!(
        aggregate["message"],
        "motor refuses exec.status without a job_id for direct; a direct caller may check only its own jobs"
    );
    // The other three pass the operation gate and reach the ownership check.
    let stranger = Uuid::now_v7();
    for (op, params) in [
        ("exec.attach", json!(AttachRequest::new(stranger, 0))),
        ("exec.cancel", json!(CancelRequest::new(stranger))),
        ("exec.status", json!({"job_id": stranger})),
    ] {
        let reply = error(&daemon.request(DIRECT_CHANNEL, op, params).await);
        assert_eq!(reply["code"], "motor_job_not_found", "{op}");
        assert_eq!(
            reply["message"], "motor has no such job for this caller",
            "{op}"
        );
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
            refusal.contains("disconnected direct caller may not return with its secret"),
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
    let err = terminal.text("err");
    let mut lines = err.lines();
    let credential = lines.next().unwrap();
    assert!(credential.starts_with("ck motor run: job_id="));
    assert!(credential.contains(" secret="));
    assert_eq!(lines.collect::<Vec<_>>(), ["err1", "err2"]);
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
        err.contains(&format!(
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

#[tokio::test]
async fn hosted_runs_are_direct_only_and_need_a_configured_broker() {
    // No broker URL, and a broker URL whose account token source is missing.
    for broker_url in [None, Some("https://runners.example".to_string())] {
        let f = Fixture::new(Cut::None, 28).await;
        let config = Config {
            broker_url,
            ..direct_config(&f)
        };
        let daemon = Daemon::start(&f, config).await;
        for (channel, principal) in [(DIRECT_CHANNEL, direct()), (AFT_CHANNEL, aft())] {
            assert_eq!(
                daemon.bind(channel, principal).await.header.ty,
                FrameType::Response
            );
        }
        let mut params = json!(timed(&f, "true", 60));
        params["hosted"] = json!(true);
        let refused = error(
            &daemon
                .request(DIRECT_CHANNEL, "exec.run", params.clone())
                .await,
        );
        assert_eq!(refused["code"], module::HOSTED_TEST_UNAVAILABLE);
        assert!(
            refused["message"]
                .as_str()
                .unwrap()
                .contains("cannot run a job on a hosted runner"),
            "{refused}"
        );
        // The CLI reports it as a usage refusal, with nothing submitted.
        let client = WireClient {
            daemon: daemon.clone(),
            channel: DIRECT_CHANNEL,
            hosted: true,
        };
        let terminal = Terminal::default();
        let (_tx, mut interrupts) = mpsc::channel(1);
        let code = cli::drive(
            &client,
            &timed(&f, "true", 60),
            &mut Side("out", terminal.clone()),
            &mut Side("err", terminal.clone()),
            &mut interrupts,
        )
        .await;
        assert_eq!(code, cli::EXIT_USAGE, "{:?}", terminal.lines());
        assert!(
            terminal
                .text("err")
                .starts_with("ck motor run: --hosted unavailable: "),
            "{:?}",
            terminal.lines()
        );
        assert!(
            f.proxy.frames.lock().unwrap().is_empty(),
            "a refused hosted run must not reach the build server"
        );
        // An agent route can't ask for it: the key is ignored and the job runs
        // on the build server as usual.
        let mut frames = daemon
            .send(AFT_CHANNEL, json!({"op":"exec.run", "params":params}))
            .await;
        let terminal = loop {
            let frame = next(&mut frames).await;
            assert_eq!(frame.header.ty, FrameType::StreamData, "{}", body(&frame));
            if let StreamRecord::Terminal(terminal) = serde_json::from_slice(&frame.body).unwrap() {
                break terminal;
            }
        };
        assert_eq!(terminal.outcome, Outcome::Exit { code: 0 });
        assert!(f
            .proxy
            .frames
            .lock()
            .unwrap()
            .iter()
            .any(|frame| matches!(frame,
            motor_protocol::Frame::Run(run) if run.job_id == terminal.job_id)));
    }
}
