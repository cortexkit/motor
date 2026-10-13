use crate::*;
use cortexkit_test_support::ScratchDir;
use motor_protocol::*;
use motor_runner::{Config as RunnerConfig, Runner};
use std::{
    fs,
    future::Future,
    io,
    path::{Path, PathBuf},
    pin::Pin,
    sync::{
        atomic::{AtomicBool, AtomicU64, Ordering},
        Arc, Mutex,
    },
    time::{Duration, SystemTime, UNIX_EPOCH},
};
use tokio::{
    io::AsyncWriteExt,
    net::{UnixListener, UnixStream},
    sync::Notify,
    task::JoinHandle,
};

fn git(path: &Path, args: &[&str]) -> String {
    crate::snapshot::git(path, args).unwrap()
}
fn text(path: &Path) -> String {
    path.to_str().unwrap().into()
}
#[derive(Clone, Copy, PartialEq)]
pub(super) enum Cut {
    None,
    BeforeSeen,
    AfterAccepted,
    MidSecond,
    BeforeSecond,
    AfterLast,
    Seq7,
    HoldSecond,
    DiskFull,
    RetryTerminal,
}
pub(super) struct Proxy {
    socket: PathBuf,
    local: String,
    remote: String,
    cut: Cut,
    used: Arc<AtomicBool>,
    pub unavailable: AtomicBool,
    pub pause_on_cut: AtomicBool,
    pub legacy_have: AtomicBool,
    pub strip_run_network: AtomicBool,
    // None leaves the runner's grant unchanged; Some sets or removes it.
    pub accepted_network: Mutex<Option<Option<Network>>>,
    pub frames: Arc<Mutex<Vec<Frame>>>,
    pub cut_seen: Arc<Notify>,
}
impl Proxy {
    fn rewrite(&self, value: &mut serde_json::Value, reverse: bool) {
        match value {
            serde_json::Value::String(s) => {
                let (from, to) = if reverse {
                    (&self.remote, &self.local)
                } else {
                    (&self.local, &self.remote)
                };
                if let Some(rest) = s.strip_prefix(from) {
                    *s = format!("{to}{rest}");
                }
            }
            serde_json::Value::Array(a) => {
                for v in a {
                    self.rewrite(v, reverse);
                }
            }
            serde_json::Value::Object(o) => {
                for v in o.values_mut() {
                    self.rewrite(v, reverse);
                }
            }
            _ => {}
        }
    }
}
impl Connector for Arc<Proxy> {
    fn connect(
        &self,
        _id: Uuid,
    ) -> Pin<Box<dyn Future<Output = io::Result<Box<dyn Duplex>>> + Send + '_>> {
        Box::pin(async move {
            if self.unavailable.load(Ordering::SeqCst) {
                return Err(io::Error::other("offline"));
            }
            let server = UnixStream::connect(&self.socket).await?;
            let (client, bridge) = tokio::io::duplex(262144);
            let proxy = self.clone();
            tokio::spawn(async move {
                let (mut cr, mut cw) = tokio::io::split(bridge);
                let (mut sr, mut sw) = server.into_split();
                let is_run = Arc::new(AtomicBool::new(false));
                let running = is_run.clone();
                let up = async {
                    let mut lengths = Vec::new();
                    loop {
                        let Some(mut frame) = read_frame_async(&mut cr).await? else {
                            return Ok::<_, io::Error>(());
                        };
                        proxy.frames.lock().unwrap().push(frame.clone());
                        if proxy.strip_run_network.load(Ordering::SeqCst) {
                            if let Frame::Run(run) = &mut frame {
                                run.network = None;
                            }
                        }
                        if proxy.cut == Cut::HoldSecond
                            && matches!(frame, Frame::BundleChunk { index: 1, .. })
                            && !proxy.used.swap(true, Ordering::SeqCst)
                        {
                            proxy.cut_seen.notify_one();
                            std::future::pending::<()>().await;
                        }
                        if let Frame::Run(r) = &frame {
                            running.store(true, Ordering::SeqCst);
                            lengths = r.bundles.iter().map(|b| b.length).collect();
                        }
                        let should_cut = match &mut frame {
                            Frame::Run(_) if proxy.cut == Cut::BeforeSeen => true,
                            Frame::BundleChunk { index: 1, data, .. }
                                if proxy.cut == Cut::MidSecond =>
                            {
                                data.0.truncate(data.0.len() / 2);
                                false
                            }
                            Frame::BundleChunk { index: 1, .. }
                                if proxy.cut == Cut::BeforeSecond =>
                            {
                                true
                            }
                            _ => false,
                        };
                        if should_cut && !proxy.used.swap(true, Ordering::SeqCst) {
                            if proxy.pause_on_cut.load(Ordering::SeqCst) {
                                proxy.unavailable.store(true, Ordering::SeqCst);
                            }
                            proxy.cut_seen.notify_one();
                            return Ok(());
                        }
                        let after = match &frame {
                            Frame::BundleChunk {
                                index,
                                offset,
                                data,
                            } => {
                                (proxy.cut == Cut::MidSecond && *index == 1)
                                    || (proxy.cut == Cut::AfterLast
                                        && *index as usize + 1 == lengths.len()
                                        && *offset + data.0.len() as u64
                                            == lengths[*index as usize])
                            }
                            _ => false,
                        };
                        let mut value = serde_json::to_value(frame).unwrap();
                        // In one process the Mac and server cannot own the same
                        // absolute directory. Only the fixture remaps roots;
                        // production transports send the original paths intact.
                        proxy.rewrite(&mut value, false);
                        write_frame_async(&mut sw, &serde_json::from_value(value).unwrap()).await?;
                        sw.flush().await?;
                        if after && !proxy.used.swap(true, Ordering::SeqCst) {
                            proxy.cut_seen.notify_one();
                            return Ok(());
                        }
                    }
                };
                let down = async {
                    loop {
                        let Some(mut frame) = read_frame_async(&mut sr).await? else {
                            return Ok::<_, io::Error>(());
                        };
                        if let Frame::Accepted(accepted) = &mut frame {
                            if let Some(network) = proxy.accepted_network.lock().unwrap().as_ref() {
                                accepted.network = network.clone();
                            }
                        }
                        let refused = proxy.cut == Cut::DiskFull
                            && matches!(frame, Frame::Accepted(_) | Frame::PrepareAccepted { .. });
                        if refused {
                            let job_id = match &frame {
                                Frame::Accepted(a) => Some(a.job_id),
                                _ => None,
                            };
                            let transfer_id = match &frame {
                                Frame::PrepareAccepted { transfer_id } => Some(*transfer_id),
                                _ => None,
                            };
                            frame = Frame::Refusal(Refusal {
                                job_id,
                                transfer_id,
                                reason: RefusalReason::RunnerDiskFull,
                                disk_full: Some(RunnerDiskFull {
                                    free_bytes: 10,
                                    floor_bytes: 20,
                                }),
                                retry_after_ms: Some(1200),
                                detail: None,
                            });
                        }
                        if proxy.cut == Cut::RetryTerminal {
                            if let Frame::Terminal(t) = &mut frame {
                                t.retry_after_ms = Some(1200);
                            }
                        }
                        let after = (proxy.cut == Cut::AfterAccepted
                            && is_run.load(Ordering::SeqCst)
                            && matches!(frame, Frame::Accepted(_)))
                            || (proxy.cut == Cut::Seq7
                                && is_run.load(Ordering::SeqCst)
                                && matches!(&frame, Frame::Output(o) if o.seq == 7));
                        let mut value = serde_json::to_value(frame).unwrap();
                        if proxy.legacy_have.load(Ordering::SeqCst) && value["type"] == "have_reply"
                        {
                            value.as_object_mut().unwrap().remove("git_dependencies");
                        }
                        proxy.rewrite(&mut value, true);
                        write_frame_async(&mut cw, &serde_json::from_value(value).unwrap()).await?;
                        cw.flush().await?;
                        if refused || (after && !proxy.used.swap(true, Ordering::SeqCst)) {
                            proxy.cut_seen.notify_one();
                            return Ok(());
                        }
                    }
                };
                tokio::select! { _ = up => {}, _ = down => {} }
            });
            Ok(Box::new(client) as Box<dyn Duplex>)
        })
    }
}

pub(super) struct Fixture {
    pub scratch: ScratchDir,
    pub key: PathBuf,
    pub main: PathBuf,
    pub sibling: PathBuf,
    pub proxy: Arc<Proxy>,
    pub runner: Runner,
    server: JoinHandle<io::Result<()>>,
    pub config: Config,
    pub clock: Arc<AtomicU64>,
    pub remote_key: PathBuf,
}
impl Drop for Fixture {
    fn drop(&mut self) {
        self.server.abort();
        let _ = fs::remove_file(&self.proxy.socket);
    }
}
impl Fixture {
    pub fn use_external_runner_socket(&mut self, socket: PathBuf) {
        self.server.abort();
        fs::remove_file(&self.proxy.socket).unwrap();
        std::sync::Arc::get_mut(&mut self.proxy).unwrap().socket = socket;
    }
    async fn restart_runner(&mut self) {
        let config = self.runner.config().clone();
        self.server.abort();
        let _ = (&mut self.server).await;
        fs::remove_file(&self.proxy.socket).unwrap();
        let placeholder =
            Runner::open(RunnerConfig::new(self.scratch.join("placeholder"), "e2e")).unwrap();
        drop(std::mem::replace(&mut self.runner, placeholder));
        self.runner = tokio::time::timeout(Duration::from_secs(5), async {
            loop {
                match Runner::open(config.clone()) {
                    Ok(runner) => break runner,
                    Err(error) if error.kind() == io::ErrorKind::WouldBlock => {
                        tokio::time::sleep(Duration::from_millis(25)).await
                    }
                    Err(error) => panic!("restart: {error}"),
                }
            }
        })
        .await
        .unwrap();
        let listener = UnixListener::bind(&self.proxy.socket).unwrap();
        self.server = tokio::spawn(motor_runner::serve(self.runner.clone(), listener));
    }
    pub async fn new(cut: Cut, max_weight: u32) -> Self {
        Self::with_main(cut, max_weight, None).await
    }
    /// `source`, when given, is cloned as the main repository instead of a
    /// one-commit fixture, so a test can transfer a real repository's history.
    pub async fn with_main(cut: Cut, max_weight: u32, source: Option<&Path>) -> Self {
        Self::with_runner_config(cut, max_weight, source, |_| {}).await
    }
    pub async fn with_runner_config(
        cut: Cut,
        max_weight: u32,
        source: Option<&Path>,
        configure: impl FnOnce(&mut RunnerConfig),
    ) -> Self {
        let scratch = ScratchDir::new("motor-module");
        fs::create_dir_all(scratch.join("local")).unwrap();
        fs::create_dir_all(scratch.join("remote")).unwrap();
        let local = fs::canonicalize(scratch.join("local")).unwrap();
        let remote = fs::canonicalize(scratch.join("remote")).unwrap();
        let main = local.join("source/main");
        let sibling = local.join("source/commons");
        if let Some(source) = source {
            fs::create_dir_all(main.parent().unwrap()).unwrap();
            git(
                main.parent().unwrap(),
                &["clone", "--quiet", &text(source), &text(&main)],
            );
        }
        for path in [&main, &sibling] {
            if path.join(".git").exists() {
                continue;
            }
            fs::create_dir_all(path).unwrap();
            git(path, &["init"]);
            fs::write(path.join("source"), "initial\n").unwrap();
            fs::write(path.join(".gitignore"), "target\nignored\n").unwrap();
            git(path, &["add", "."]);
            git(
                path,
                &[
                    "-c",
                    "user.name=Test",
                    "-c",
                    "user.email=test@example.invalid",
                    "commit",
                    "-m",
                    "initial",
                ],
            );
        }
        // Large, incompressible sibling content ensures multiple bundle chunks.
        use rand::{RngCore, SeedableRng};
        let mut bytes = vec![0; 180000];
        rand::rngs::StdRng::seed_from_u64(37).fill_bytes(&mut bytes);
        fs::write(sibling.join("binary"), bytes).unwrap();
        let key = local.join("worktrees/a");
        fs::create_dir_all(key.parent().unwrap()).unwrap();
        git(&main, &["worktree", "add", "-b", "task", &text(&key)]);
        let mut runner_config = RunnerConfig::new(scratch.join("runner-state"), "e2e");
        runner_config.owned_workspace_roots = vec![scratch.path().to_path_buf()];
        runner_config.free_space_floor_bytes = 0;
        runner_config.max_weight = max_weight.max(1);
        // No generated artifacts: empty warm targets do not require a reflink
        // fixture filesystem on Linux. Reflink behavior belongs to runner tests.
        runner_config.base_commands = vec!["true".into()];
        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_millis() as u64;
        let clock = Arc::new(AtomicU64::new(now));
        let c = clock.clone();
        runner_config.clock = Arc::new(move || c.load(Ordering::SeqCst));
        configure(&mut runner_config);
        let runner = Runner::open(runner_config).unwrap();
        static NEXT: AtomicU64 = AtomicU64::new(0);
        // macOS sun_path cannot fit the process scratch directory's full name.
        let socket = cortexkit_test_support::scratch_root()
            .parent()
            .unwrap()
            .join(format!(
                "mm{}-{}",
                std::process::id(),
                NEXT.fetch_add(1, Ordering::Relaxed)
            ));
        let listener = UnixListener::bind(&socket).unwrap();
        let server = tokio::spawn(motor_runner::serve(runner.clone(), listener));
        let proxy = Arc::new(Proxy {
            socket,
            local: text(&local),
            remote: text(&remote),
            cut,
            used: Arc::new(AtomicBool::new(false)),
            unavailable: AtomicBool::new(false),
            pause_on_cut: AtomicBool::new(false),
            legacy_have: AtomicBool::new(false),
            strip_run_network: AtomicBool::new(false),
            accepted_network: Mutex::new(None),
            frames: Arc::new(Mutex::new(Vec::new())),
            cut_seen: Arc::new(Notify::new()),
        });
        let config = Config {
            runner_instance: "e2e".into(),
            worktree_roots: vec![local.join("worktrees")],
            ..Config::default()
        };
        Self {
            scratch,
            key,
            main,
            sibling,
            proxy,
            runner,
            server,
            config,
            clock,
            remote_key: remote.join("worktrees/a"),
        }
    }
    pub fn motor(&self) -> Motor {
        Motor::with_connector(
            self.config.clone(),
            &self.scratch.join("module-state"),
            Arc::new(self.proxy.clone()),
        )
        .unwrap()
    }
    pub fn request(&self, command: &str) -> RunRequest {
        serde_json::from_value(serde_json::json!({"workspace_key":text(&self.key),"repository_root":text(&self.main),"cwd":text(&self.key),"command":command,"env":{},"siblings":[text(&self.sibling)],"queue_wait_limit_s":900})).unwrap()
    }
    pub fn recorded_run(&self) -> Run {
        self.proxy
            .frames
            .lock()
            .unwrap()
            .iter()
            .find_map(|f| {
                if let Frame::Run(r) = f {
                    Some(r.clone())
                } else {
                    None
                }
            })
            .unwrap()
    }
    /// Job records the live runner has written so far. The runner creates a
    /// job's directory (for its bundle chunks) before it writes record.json,
    /// and replaces the record atomically, so a directory without a record yet
    /// is skipped, never read half-written.
    pub fn jobs(&self) -> Vec<serde_json::Value> {
        fs::read_dir(self.runner.config().state_dir().join("jobs"))
            .unwrap()
            .filter_map(|e| match fs::read(e.unwrap().path().join("record.json")) {
                Ok(bytes) => Some(serde_json::from_slice(&bytes).unwrap()),
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => None,
                Err(error) => panic!("read job record: {error}"),
            })
            .collect()
    }
    pub fn absent_from_mirrors(&self, request: &Run) {
        let mirrors = self.runner.config().state_dir().join("mirrors");
        if !mirrors.exists() {
            return;
        }
        for mirror in fs::read_dir(mirrors).unwrap() {
            let path = mirror.unwrap().path();
            for commit in std::iter::once(&request.snapshot_commit)
                .chain(request.siblings.iter().map(|s| &s.commit))
            {
                assert!(
                    crate::snapshot::git(&path, &["cat-file", "-e", commit]).is_err(),
                    "interrupted upload fetched {commit}"
                );
            }
        }
    }
}

#[tokio::test]
async fn runner_disk_status_is_opt_in_and_aggregate_bytes_stay_unchanged() {
    let f = Fixture::new(Cut::None, 28).await;
    let motor = f.motor();
    let aggregate = motor.status().await;
    let disk = motor.disk_status().await;
    assert!(disk.disk.as_ref().unwrap().free_bytes.is_some());
    assert_eq!(disk.disk.as_ref().unwrap().floor_bytes, 0);
    let expected = format!("{{\"queue_depth\":0,\"running_jobs\":[],\"server_reachable\":true,\"repositories\":[],\"rustc_version\":{}}}", serde_json::to_string(&aggregate.rustc_version).unwrap()).into_bytes();
    assert_eq!(serde_json::to_vec(&aggregate).unwrap(), expected);
    assert_eq!(serde_json::to_vec(&disk.status).unwrap(), expected);
    assert!(serde_json::to_value(&aggregate)
        .unwrap()
        .get("disk")
        .is_none());
    assert!(serde_json::to_value(&disk).unwrap().get("disk").is_some());
    let subc_client_rs::HandlerOutcome::Response(legacy_bytes) =
        crate::module::status_reply(&motor, &serde_json::json!({}))
            .await
            .unwrap()
    else {
        panic!("expected status response")
    };
    assert_eq!(legacy_bytes, expected);
    let subc_client_rs::HandlerOutcome::Response(disk_bytes) =
        crate::module::status_reply(&motor, &serde_json::json!({"disk":true}))
            .await
            .unwrap()
    else {
        panic!("expected disk response")
    };
    assert!(
        serde_json::from_slice::<serde_json::Value>(&disk_bytes).unwrap()["disk"]["free_bytes"]
            .is_number()
    );
}
pub(super) async fn collect(mut stream: JobStream) -> Vec<StreamRecord> {
    tokio::time::timeout(Duration::from_secs(40), async move {
        let mut records = Vec::new();
        while let Some(record) = stream.records.recv().await {
            records.push(record.unwrap());
        }
        assert_eq!(
            records
                .iter()
                .filter(|r| matches!(r, StreamRecord::Terminal(_)))
                .count(),
            1,
            "{records:?}"
        );
        records
    })
    .await
    .expect("stream did not terminate")
}
pub(super) fn terminal(records: &[StreamRecord]) -> &TerminalRecord {
    let StreamRecord::Terminal(t) = records.last().unwrap() else {
        panic!("terminal last")
    };
    t
}

#[tokio::test]
async fn network_routing_relays_requested_grant_and_reaches_terminal() {
    let mut f = Fixture::new(Cut::None, 28).await;
    f.config.network_routing = true;
    let manifest =
        serde_json::to_value(crate::module::manifest_with_config("ck-motor", &f.config)).unwrap();
    assert!(manifest["capabilities"]["provides"]
        .as_array()
        .unwrap()
        .contains(&serde_json::json!("exec-remote-net/v1")));
    // The portable runner stays offline; the proxy simulates its network grant.
    f.proxy.strip_run_network.store(true, Ordering::SeqCst);
    *f.proxy.accepted_network.lock().unwrap() = Some(Some(Network::Outbound));
    let mut request = f.request("true");
    request.network = Some(Network::Outbound);
    let records = collect(f.motor().run(request).unwrap()).await;
    let StreamRecord::Accepted(accepted) = &records[0] else {
        panic!("accepted must be the first stream record")
    };
    assert_eq!(accepted.network, Some(Network::Outbound));
    assert_eq!(f.recorded_run().network, Some(Network::Outbound));
    assert_eq!(terminal(&records).outcome, Outcome::Exit { code: 0 });
    assert!(f
        .proxy
        .frames
        .lock()
        .unwrap()
        .iter()
        .any(|frame| { matches!(frame, Frame::BundleChunk { .. }) }));
}

#[tokio::test]
async fn network_routing_off_refuses_before_contacting_runner() {
    let f = Fixture::new(Cut::None, 28).await;
    assert!(!f.config.network_routing);
    let mut request = f.request("true");
    request.network = Some(Network::Outbound);
    let motor = f.motor();
    let records = collect(motor.run(request).unwrap()).await;
    assert_eq!(records.len(), 1);
    assert_eq!(
        terminal(&records).outcome,
        Outcome::RefusedBeforeStart {
            reason: RefusalReason::NetworkUnsupported,
        }
    );
    assert_eq!(terminal(&records).ran, Some(Ran::None));
    assert!(f.proxy.frames.lock().unwrap().is_empty());
    assert!(f.jobs().is_empty());
}

#[tokio::test]
async fn network_routing_missing_grant_cancels_before_upload_and_refuses_once() {
    let mut f = Fixture::new(Cut::None, 28).await;
    f.config.network_routing = true;
    f.proxy.strip_run_network.store(true, Ordering::SeqCst);
    *f.proxy.accepted_network.lock().unwrap() = Some(None);
    let mut request = f.request("touch NET_SHOULD_NOT_RUN");
    request.network = Some(Network::Outbound);
    let motor = f.motor();
    let records = collect(motor.run(request).unwrap()).await;
    assert_eq!(records.len(), 1, "must not relay Accepted: {records:?}");
    let t = terminal(&records);
    assert_eq!(
        t.outcome,
        Outcome::RefusedBeforeStart {
            reason: RefusalReason::NetworkUnsupported,
        }
    );
    assert_eq!(t.ran, Some(Ran::None));
    let run = f.recorded_run();
    assert_eq!(run.network, Some(Network::Outbound));
    assert_eq!(t.tree_hash.as_deref(), Some(run.tree_hash.as_str()));
    assert!(t.refusal_detail.as_ref().unwrap().contains("runner"));
    let frames = f.proxy.frames.lock().unwrap().clone();
    assert_eq!(
        frames
            .iter()
            .filter(|frame| matches!(frame, Frame::Cancel(c) if c.job_id == t.job_id))
            .count(),
        1
    );
    assert!(!frames
        .iter()
        .any(|frame| matches!(frame, Frame::BundleChunk { .. })));
    let jobs = f.jobs();
    assert_eq!(jobs.len(), 1);
    assert_eq!(jobs[0]["state"], "Tombstone");
    assert!(jobs[0].get("started_ms").unwrap().is_null());
    assert!(!f.remote_key.join("NET_SHOULD_NOT_RUN").exists());
    let replay = collect(motor.attach(AttachRequest::new(t.job_id, 0)).unwrap()).await;
    assert_eq!(replay, records);
}

#[tokio::test]
async fn network_routing_drops_unsolicited_grant() {
    for network_routing in [false, true] {
        let mut f = Fixture::new(Cut::None, 28).await;
        f.config.network_routing = network_routing;
        *f.proxy.accepted_network.lock().unwrap() = Some(Some(Network::Outbound));
        let request = f.request("true");
        assert_eq!(request.network, None);
        let records = collect(f.motor().run(request).unwrap()).await;
        let StreamRecord::Accepted(accepted) = &records[0] else {
            panic!("accepted must be the first stream record")
        };
        assert_eq!(accepted.network, None);
        assert!(serde_json::to_value(accepted)
            .unwrap()
            .get("network")
            .is_none());
        assert_eq!(f.recorded_run().network, None);
        assert_eq!(terminal(&records).outcome, Outcome::Exit { code: 0 });
    }
}

#[tokio::test]
async fn accepted_reports_dropped_env_names_without_env_values() {
    fn contains_string(value: &serde_json::Value, expected: &str) -> bool {
        match value {
            serde_json::Value::String(value) => value == expected,
            serde_json::Value::Array(values) => {
                values.iter().any(|value| contains_string(value, expected))
            }
            serde_json::Value::Object(values) => values
                .values()
                .any(|value| contains_string(value, expected)),
            _ => false,
        }
    }

    let f = Fixture::new(Cut::None, 28).await;
    let mut request = f.request("true");
    request.env = [
        ("RUST_BACKTRACE".into(), "1".into()),
        ("ANTHROPIC_API_KEY".into(), "x".into()),
        ("MY_FLAG".into(), "y".into()),
    ]
    .into();
    let env_values: Vec<_> = request.env.values().cloned().collect();
    let records = collect(f.motor().run(request).unwrap()).await;
    let StreamRecord::Accepted(accepted) = records.first().unwrap() else {
        panic!("accepted must be the first stream record")
    };

    assert_eq!(
        accepted.env_not_forwarded.as_ref().unwrap(),
        &vec![String::from("ANTHROPIC_API_KEY"), String::from("MY_FLAG"),]
    );
    let serialized = serde_json::to_value(records.first().unwrap()).unwrap();
    for value in env_values {
        assert!(
            !contains_string(&serialized, &value),
            "accepted record leaked an environment value"
        );
    }
}

#[tokio::test]
async fn accepted_reports_an_empty_list_when_no_env_names_were_dropped() {
    let f = Fixture::new(Cut::None, 28).await;
    let request = f.request("true");
    assert_eq!(request.network, None);
    let records = collect(f.motor().run(request).unwrap()).await;
    let StreamRecord::Accepted(accepted) = records.first().unwrap() else {
        panic!("accepted must be the first stream record")
    };
    assert_eq!(accepted.env_not_forwarded, Some(Vec::<String>::new()));
    assert_eq!(accepted.network, None);
    assert!(serde_json::to_value(accepted)
        .unwrap()
        .get("network")
        .is_none());
    assert_eq!(terminal(&records).outcome, Outcome::Exit { code: 0 });
    let frames = f.proxy.frames.lock().unwrap();
    assert!(frames.iter().any(|frame| matches!(frame, Frame::Run(_))));
    assert!(frames
        .iter()
        .any(|frame| matches!(frame, Frame::BundleChunk { .. })));
}

async fn interrupted(cut: Cut) {
    let f = Fixture::new(cut, 28).await;
    let motor = f.motor();
    let stream = motor.run(f.request("echo ran >> marker")).unwrap();
    let id = stream.job_id;
    let records = collect(stream).await;
    assert_eq!(
        terminal(&records).outcome,
        Outcome::RefusedBeforeStart {
            reason: RefusalReason::TransferInterrupted
        }
    );
    assert_eq!(terminal(&records).ran, Some(Ran::None));
    let run = f.recorded_run();
    assert!(!f.remote_key.join("marker").exists());
    assert!(!f.key.join("marker").exists());
    assert!(f.jobs().iter().all(|j| j["state"] != "AwaitingBundles"));
    f.absent_from_mirrors(&run);
    // Replay run itself, not just attach: a duplicate cannot replace the proof.
    let channel = crate::transport::Channel::open(&f.proxy.clone(), id)
        .await
        .unwrap();
    channel.send(Frame::Run(run)).await.unwrap();
    drop(channel);
    let replay = collect(motor.attach(AttachRequest::new(id, 0)).unwrap()).await;
    assert_eq!(terminal(&replay), terminal(&records));
}
#[tokio::test]
async fn lost_upload_a_before_runner_reads_run() {
    interrupted(Cut::BeforeSeen).await;
}

#[tokio::test]
async fn queued_job_restart_returns_lost_to_restart_promptly_without_resubmission() {
    let mut f = Fixture::new(Cut::None, 1).await;
    let motor = f.motor();
    let mut request = f.request("echo forbidden >> marker");
    // Valid CPU claims above the one-permit budget queue instead of being refused at admission.
    request.weight_hint = Some(2);
    let mut stream = motor.run(request).unwrap();
    let id = stream.job_id;
    assert!(matches!(
        stream.records.recv().await.unwrap().unwrap(),
        StreamRecord::Accepted(_)
    ));
    tokio::time::timeout(Duration::from_secs(10), async {
        while !f
            .jobs()
            .iter()
            .any(|j| j["id"] == id.to_string() && j["state"] == "Queued")
        {
            tokio::time::sleep(Duration::from_millis(25)).await;
        }
    })
    .await
    .unwrap();
    f.restart_runner().await;
    let records = tokio::time::timeout(Duration::from_secs(5), collect(stream))
        .await
        .expect("restart terminal must not wait for the 1830-second transport deadline");
    let terminal = terminal(&records);
    assert_eq!(terminal.job_id, id);
    assert_eq!(terminal.ran, Some(Ran::None));
    assert_eq!(
        terminal.outcome,
        Outcome::RefusedBeforeStart {
            reason: RefusalReason::Unknown("lost_to_restart".into())
        }
    );
    let status = motor.job_status(id).await.unwrap();
    assert_eq!(status.terminal.as_ref(), Some(terminal));
    assert!(!f.remote_key.join("marker").exists());
    let frames = f.proxy.frames.lock().unwrap();
    assert!(frames
        .iter()
        .any(|f| matches!(f, Frame::JobStatus { job_id } if *job_id == id)));
    assert_eq!(
        frames
            .iter()
            .filter(|f| matches!(f, Frame::Run(r) if r.job_id == id))
            .count(),
        1
    );
}

#[tokio::test]
async fn drain_barrier_returns_typed_safe_refusal_to_client() {
    let f = Fixture::new(Cut::None, 28).await;
    let mut channel = crate::transport::Channel::open(&f.proxy.clone(), Uuid::now_v7())
        .await
        .unwrap();
    channel
        .send(Frame::Drain {
            deadline_s: 30,
            exit_after: false,
        })
        .await
        .unwrap();
    assert!(matches!(
        channel.next().await.unwrap(),
        crate::transport::Incoming::Frame(Frame::DrainReply {
            timed_out: false,
            restart_recoverable_jobs: Some(0)
        })
    ));
    let records = collect(
        f.motor()
            .run(f.request("echo forbidden >> marker"))
            .unwrap(),
    )
    .await;
    assert_eq!(
        terminal(&records).outcome,
        Outcome::RefusedBeforeStart {
            reason: RefusalReason::RunnerDraining
        }
    );
    assert_eq!(terminal(&records).ran, Some(Ran::None));
    assert!(!records
        .iter()
        .any(|r| matches!(r, StreamRecord::Accepted(_))));
    assert!(!f.remote_key.join("marker").exists());
}
#[tokio::test]
async fn lost_upload_b_after_accepted() {
    interrupted(Cut::AfterAccepted).await;
}
#[tokio::test]
async fn lost_upload_c_midway_second_bundle() {
    interrupted(Cut::MidSecond).await;
}
#[tokio::test]
async fn lost_upload_d_first_verified_second_held() {
    interrupted(Cut::BeforeSecond).await;
}
#[tokio::test]
async fn lost_upload_e_after_last_byte_runs_once() {
    let f = Fixture::new(Cut::AfterLast, 28).await;
    let motor = f.motor();
    let stream = motor.run(f.request("echo ran >> marker")).unwrap();
    let id = stream.job_id;
    let records = collect(stream).await;
    assert_eq!(terminal(&records).outcome, Outcome::Exit { code: 0 });
    assert_eq!(terminal(&records).ran, Some(Ran::Remote));
    assert_eq!(
        fs::read_to_string(f.remote_key.join("marker")).unwrap(),
        "ran\n"
    );
    assert!(!f.key.join("marker").exists());
    let replay = collect(motor.attach(AttachRequest::new(id, 0)).unwrap()).await;
    assert_eq!(terminal(&replay).outcome, Outcome::Exit { code: 0 });
}
#[tokio::test]
async fn cancel_before_seen_survives_module_restart_and_attach_cancel() {
    let f = Fixture::new(Cut::BeforeSeen, 0).await;
    f.proxy.pause_on_cut.store(true, Ordering::SeqCst);
    let motor = f.motor();
    let stream = motor.run(f.request("echo ran >> marker")).unwrap();
    let id = stream.job_id;
    f.proxy.cut_seen.notified().await;
    f.proxy.unavailable.store(true, Ordering::SeqCst);
    // The attach caller's Cancel is the same durable path used by subc pump.
    let attached = motor.attach(AttachRequest::new(id, 0)).unwrap();
    attached.cancel().unwrap();
    let persisted: serde_json::Value =
        serde_json::from_slice(&fs::read(f.scratch.join("module-state/state.json")).unwrap())
            .unwrap();
    assert_eq!(persisted["jobs"][id.to_string()]["pending_cancel"], true);
    drop(attached);
    drop(stream);
    drop(motor);
    // Reopen the actual module storage once detached consumers release the
    // single-writer lease. Nothing in memory is handed to the new instance.
    let restarted = super::reopen_motor(|| {
        Motor::with_connector(
            f.config.clone(),
            &f.scratch.join("module-state"),
            Arc::new(f.proxy.clone()),
        )
    })
    .await;
    f.proxy.unavailable.store(false, Ordering::SeqCst);
    let records = collect(restarted.attach(AttachRequest::new(id, 0)).unwrap()).await;
    assert_eq!(terminal(&records).outcome, Outcome::Cancelled);
    assert!(!f.remote_key.join("marker").exists());
    let channel = crate::transport::Channel::open(&f.proxy.clone(), id)
        .await
        .unwrap();
    channel.send(Frame::Run(f.recorded_run())).await.unwrap();
    drop(channel);
    let records = collect(restarted.attach(AttachRequest::new(id, 0)).unwrap()).await;
    assert_eq!(terminal(&records).outcome, Outcome::Cancelled);
}

#[tokio::test]
async fn cancel_while_second_bundle_is_held_preserves_cancelled_tombstone() {
    let f = Fixture::new(Cut::HoldSecond, 28).await;
    let motor = f.motor();
    let stream = motor.run(f.request("echo forbidden >> marker")).unwrap();
    f.proxy.cut_seen.notified().await;
    stream.cancel().unwrap();
    let records = collect(stream).await;
    assert!(matches!(records.first(), Some(StreamRecord::Accepted(_))));
    assert_eq!(terminal(&records).outcome, Outcome::Cancelled);
    assert_eq!(terminal(&records).ran, Some(Ran::None));
    assert!(!f.remote_key.join("marker").exists());
}
#[tokio::test]
async fn replay_cursor_resumes_at_eight_and_independent_attach_is_inclusive() {
    let f = Fixture::new(Cut::Seq7, 28).await;
    let motor = f.motor();
    // Release the next write only after the caller received the previous one,
    // so thread scheduling cannot coalesce several lines into one raw chunk.
    let mut stream = motor
        .run(f.request(
            "for i in $(seq 1 18); do echo $i; while [ ! -f permit-$i ]; do sleep 0.01; done; done",
        ))
        .unwrap();
    let id = stream.job_id;
    let mut seqs = Vec::new();
    loop {
        let r = tokio::time::timeout(Duration::from_secs(30), stream.records.recv())
            .await
            .unwrap()
            .unwrap()
            .unwrap();
        match r {
            StreamRecord::Output(o) => {
                assert_eq!(o.bytes.0, format!("{}\n", o.seq).as_bytes());
                seqs.push(o.seq);
                fs::write(f.remote_key.join(format!("permit-{}", o.seq)), "").unwrap();
            }
            StreamRecord::Terminal(t) => {
                assert_eq!(t.outcome, Outcome::Exit { code: 0 });
                break;
            }
            _ => {}
        }
    }
    assert_eq!(seqs, (1..=18).collect::<Vec<_>>());
    {
        let frames = f.proxy.frames.lock().unwrap();
        assert!(frames
            .iter()
            .any(|f| matches!(f, Frame::Attach(a) if a.job_id == id && a.from_seq == 8)));
    }
    let replay = collect(motor.attach(AttachRequest::new(id, 7)).unwrap()).await;
    assert!(matches!(replay.first(), Some(StreamRecord::Output(o)) if o.seq == 7));
}
#[tokio::test]
async fn expired_history_never_becomes_before_start_refusal() {
    let f = Fixture::new(Cut::None, 28).await;
    let motor = f.motor();
    let marker = fs::canonicalize(f.scratch.join("remote"))
        .unwrap()
        .join("history-marker");
    let stream = motor
        .run(f.request(&format!("echo ran >> '{}'", marker.display())))
        .unwrap();
    let id = stream.job_id;
    collect(stream).await;
    // Retention counts from the later of the terminal time and the job ID's
    // mint time, a wall-clock UUIDv7 stamp. The fake clock was frozen when the
    // fixture was built, before the ID was minted, so age the job past its mint
    // time; aging only the frozen clock fails whenever setup takes over a second.
    let (secs, nanos) = id.get_timestamp().expect("job IDs are UUIDv7").to_unix();
    f.clock
        .fetch_max(secs * 1000 + u64::from(nanos) / 1_000_000, Ordering::SeqCst);
    f.clock
        .fetch_add(motor_runner::RETENTION_MS + 1000, Ordering::SeqCst);
    f.runner.maintenance().unwrap();
    let replay = collect(motor.attach(AttachRequest::new(id, 0)).unwrap()).await;
    assert_eq!(terminal(&replay).outcome, Outcome::HistoryExpired);
    assert_eq!(terminal(&replay).ran, None);
    assert_eq!(fs::read_to_string(marker).unwrap(), "ran\n");
}
#[tokio::test]
async fn setup_failure_has_one_terminal_and_no_queue_entry() {
    let f = Fixture::new(Cut::None, 28).await;
    fs::create_dir_all(f.remote_key.parent().unwrap()).unwrap();
    fs::write(&f.remote_key, "not a directory").unwrap();
    let records = collect(f.motor().run(f.request("echo ran >> marker")).unwrap()).await;
    assert!(matches!(records.first(), Some(StreamRecord::Accepted(_))));
    let detail = match &terminal(&records).outcome {
        Outcome::RefusedBeforeStart {
            reason: RefusalReason::Unknown(detail),
        } => detail.clone(),
        outcome => panic!("expected a detailed setup refusal, got {outcome:?}"),
    };
    assert!(
        detail.starts_with("workspace_setup_failed: prepare job workspace: populate workspace:"),
        "{detail}"
    );
    assert!(detail.contains("destination path"), "{detail}");
    assert!(
        detail.contains(&f.remote_key.display().to_string()),
        "{detail}"
    );
    // The failed job leaves no queue entry. The background warm build it
    // queued is excluded: it is set up on its own thread and may still be
    // queued while this terminal is delivered.
    assert!(f
        .jobs()
        .iter()
        .filter(|j| j["base"] != true)
        .all(|j| j["state"] != "AwaitingBundles" && j["state"] != "Queued"));
    assert!(!f.key.join("marker").exists());
    let deadline = tokio::time::Instant::now() + Duration::from_secs(10);
    loop {
        let status = f.runner.status();
        if status.queue_depth == 0 && status.running_jobs.is_empty() {
            break;
        }
        assert!(
            tokio::time::Instant::now() < deadline,
            "warm build queued by the setup failure did not finish"
        );
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
}
#[tokio::test]
async fn queue_depth_counts_sixty_four_awaiting_bundles() {
    let f = Fixture::new(Cut::None, 0).await;
    let motor = f.motor();
    // Obtain real snapshot/manifest data without letting its first job dispatch.
    let first = motor.run(f.request("true")).unwrap();
    let first_id = first.job_id;
    let mut first = first;
    assert!(matches!(
        first.records.recv().await.unwrap().unwrap(),
        StreamRecord::Accepted(_)
    ));
    motor.cancel(CancelRequest::new(first_id)).await.unwrap();
    collect(first).await;
    let template = f.recorded_run();
    let mut channels = Vec::new();
    for _ in 0..64 {
        let mut r = template.clone();
        r.job_id = Uuid::now_v7();
        let mut c = crate::transport::Channel::open(&f.proxy.clone(), r.job_id)
            .await
            .unwrap();
        c.send(Frame::Run(r)).await.unwrap();
        assert!(matches!(
            c.next().await.unwrap(),
            crate::transport::Incoming::Frame(Frame::Accepted(_))
        ));
        channels.push(c);
    }
    assert_eq!(f.runner.status().queue_depth, 64);
    let mut r = template;
    r.job_id = Uuid::now_v7();
    let mut c = crate::transport::Channel::open(&f.proxy.clone(), r.job_id)
        .await
        .unwrap();
    c.send(Frame::Run(r)).await.unwrap();
    assert!(
        matches!(c.next().await.unwrap(), crate::transport::Incoming::Frame(Frame::Refusal(r)) if r.reason == RefusalReason::RunnerFull)
    );
    drop(channels);
}
#[tokio::test]
async fn zero_queue_wait_withdraws_and_cancel_is_idempotent() {
    let f = Fixture::new(Cut::None, 0).await;
    let motor = f.motor();
    let mut request = f.request("echo ran >> marker");
    request.queue_wait_limit_s = Some(0);
    let stream = motor.run(request).unwrap();
    let id = stream.job_id;
    let records = collect(stream).await;
    assert!(matches!(&terminal(&records).outcome,
        Outcome::RefusedBeforeStart { reason: RefusalReason::Unknown(detail) }
        if detail.starts_with("queue_wait_exceeded:") && detail.contains("held_permits=0/1")));
    motor.cancel(CancelRequest::new(id)).await.unwrap();
    motor.cancel(CancelRequest::new(id)).await.unwrap();
    assert_eq!(
        terminal(&collect(motor.attach(AttachRequest::new(id, 0)).unwrap()).await),
        terminal(&records)
    );
    assert!(!f.remote_key.join("marker").exists());
}

#[tokio::test]
async fn draining_retry_hint_is_decoded_and_logged_by_ck_motor() {
    let f = Fixture::new(Cut::None, 28).await;
    f.runner.begin_drain(Duration::from_secs(60), false);
    let captured = crate::logging::Captured::default();
    let _logging = crate::logging::scoped(captured.clone());
    let motor = f.motor();
    let records = collect(motor.run(f.request("true")).unwrap()).await;
    let terminal = terminal(&records);
    let text = captured.text();
    let line = text
        .lines()
        .find(|line| line.contains("runner admission refused"))
        .unwrap();
    assert!(line.contains("INFO") && line.contains(&format!("job_id={}", terminal.job_id)));
    let hint: u64 = line
        .split("retry_after_ms=")
        .nth(1)
        .unwrap()
        .split_whitespace()
        .next()
        .unwrap()
        .parse()
        .unwrap();
    assert!(hint > 0 && hint <= 60_000);
    assert_eq!(terminal.retry_after_ms(), Some(hint));
    assert_eq!(terminal.refusal_detail(), Some("runner_draining"));
    assert_eq!(terminal.ran, Some(Ran::None));
    assert!(matches!(
        &terminal.outcome,
        Outcome::RefusedBeforeStart {
            reason: RefusalReason::RunnerDraining
        }
    ));
    let prepare = serde_json::from_value(serde_json::json!({
        "workspace_key": f.key, "repository_root": f.main, "base_commit": "HEAD", "siblings": []
    }))
    .unwrap();
    let reply = motor.prepare(prepare).await.unwrap();
    assert_eq!(
        reply.outcome,
        PrepareOutcome::RefusedBeforeStart {
            reason: RefusalReason::RunnerDraining
        }
    );
    assert_eq!(reply.refusal_detail(), Some("runner_draining"));
    assert!(reply
        .retry_after_ms()
        .is_some_and(|ms| ms > 0 && ms <= 60_000));
}

#[tokio::test]
async fn disk_full_reaches_run_and_prepare_callers_with_known_reason_and_detail() {
    let f = Fixture::new(Cut::DiskFull, 28).await;
    let motor = f.motor();
    let records = collect(motor.run(f.request("true")).unwrap()).await;
    let t = terminal(&records);
    assert_eq!(
        t.outcome,
        Outcome::RefusedBeforeStart {
            reason: RefusalReason::RunnerDiskFull
        }
    );
    assert_eq!(
        t.refusal_detail(),
        Some("runner_disk_full: free_bytes=10 floor_bytes=20")
    );
    assert_eq!(t.retry_after_ms(), Some(1200));
    assert_eq!(t.ran, Some(Ran::None));
    let prepare = serde_json::from_value(serde_json::json!({
        "workspace_key": f.key, "repository_root": f.main, "base_commit": "HEAD", "siblings": []
    }))
    .unwrap();
    let reply = motor.prepare(prepare).await.unwrap();
    assert_eq!(
        reply.outcome,
        PrepareOutcome::RefusedBeforeStart {
            reason: RefusalReason::RunnerDiskFull
        }
    );
    assert_eq!(reply.refusal_detail(), t.refusal_detail());
    assert_eq!(reply.retry_after_ms(), Some(1200));
    assert!(!f
        .proxy
        .frames
        .lock()
        .unwrap()
        .iter()
        .any(|f| matches!(f, Frame::BundleChunk { .. })));
}

#[tokio::test]
async fn retry_hint_is_removed_from_started_job_terminal() {
    let f = Fixture::new(Cut::RetryTerminal, 28).await;
    let records = collect(f.motor().run(f.request("true")).unwrap()).await;
    let t = terminal(&records);
    assert_eq!(t.outcome, Outcome::Exit { code: 0 });
    assert_eq!(t.ran, Some(Ran::Remote));
    assert_eq!(t.retry_after_ms(), None);
}

#[tokio::test]
async fn refused_snapshot_retry_after_maintenance_sends_no_bundle() {
    let f = Fixture::new(Cut::None, 1).await;
    fs::write(f.key.join("source"), "dirty retained snapshot\n").unwrap();
    let motor = f.motor();
    let mut request = f.request("true");
    // Import the snapshot before withdrawing the request from the queue; a CPU claim above the allocatable limit is refused without importing it.
    request.weight_hint = Some(2);
    request.queue_wait_limit_s = Some(0);
    let records = collect(motor.run(request.clone()).unwrap()).await;
    assert_eq!(terminal(&records).ran, Some(Ran::None));
    assert!(matches!(&terminal(&records).outcome,
        Outcome::RefusedBeforeStart { reason: RefusalReason::Unknown(detail) }
        if detail.starts_with("queue_wait_exceeded:")));
    assert!(f.recorded_run().bundles.iter().any(|b| b.length > 0));
    f.runner.maintenance().unwrap();
    f.proxy.frames.lock().unwrap().clear();
    let records = collect(motor.run(request).unwrap()).await;
    assert_eq!(terminal(&records).ran, Some(Ran::None));
    assert!(matches!(&terminal(&records).outcome,
        Outcome::RefusedBeforeStart { reason: RefusalReason::Unknown(detail) }
        if detail.starts_with("queue_wait_exceeded:")));
    assert!(f.recorded_run().bundles.is_empty());
}

#[tokio::test]
async fn capped_unavailable_tips_do_not_resend_a_prepared_worktrees_history() {
    use rand::{rngs::StdRng, RngCore, SeedableRng};
    use sha2::{Digest, Sha256};
    let f = Fixture::new(Cut::None, 28).await;
    let mut random = vec![0; 1024 * 1024];
    StdRng::seed_from_u64(781).fill_bytes(&mut random);
    fs::write(f.main.join("history.bin"), random).unwrap();
    git(&f.main, &["add", "."]);
    git(
        &f.main,
        &[
            "-c",
            "user.name=Test",
            "-c",
            "user.email=test@example.invalid",
            "commit",
            "-m",
            "held history",
        ],
    );
    let base = git(&f.main, &["rev-parse", "HEAD"]);
    git(&f.key, &["reset", "--hard", &base]);
    let motor = f.motor();
    let prepare = PrepareRequest::new(text(&f.key), text(&f.main), &base)
        .with_siblings(vec![text(&f.sibling)]);
    assert_eq!(
        motor.prepare(prepare.clone()).await.unwrap().outcome,
        PrepareOutcome::Prepared
    );
    f.proxy.frames.lock().unwrap().clear();
    assert_eq!(
        motor.prepare(prepare).await.unwrap().outcome,
        PrepareOutcome::Prepared
    );
    assert!(f
        .proxy
        .frames
        .lock()
        .unwrap()
        .iter()
        .any(|f| matches!(f, Frame::Prepare(p) if p.bundles.is_empty())));

    let remote_root = f
        .remote_key
        .parent()
        .unwrap()
        .parent()
        .unwrap()
        .join("source/main");
    let mirror = f.runner.config().state_dir().join("mirrors").join(format!(
        "{:x}",
        Sha256::digest(text(&remote_root).as_bytes())
    ));
    let tree = git(&mirror, &["rev-parse", &format!("{base}^{{tree}}")]);
    for i in 0..33 {
        let output = std::process::Command::new("git")
            .arg("-C")
            .arg(&mirror)
            .env("GIT_AUTHOR_DATE", "2030-01-01T00:00:00Z")
            .env("GIT_COMMITTER_DATE", "2030-01-01T00:00:00Z")
            .args([
                "-c",
                "user.name=Runner",
                "-c",
                "user.email=runner@example.invalid",
                "commit-tree",
                &tree,
                "-p",
                &base,
                "-m",
                &format!("remote-only snapshot {i}"),
            ])
            .output()
            .unwrap();
        assert!(output.status.success());
        let child = String::from_utf8(output.stdout).unwrap().trim().to_owned();
        git(
            &mirror,
            &["update-ref", &format!("refs/tips/{child}"), &child],
        );
    }
    // Remove the old tip ref just as maintenance can when another branch is
    // current. The prepared workspace still holds its snapshot and all history.
    git(&mirror, &["update-ref", "-d", &format!("refs/tips/{base}")]);
    fs::write(f.main.join("source"), "new integration commit\n").unwrap();
    git(&f.main, &["add", "."]);
    git(
        &f.main,
        &[
            "-c",
            "user.name=Test",
            "-c",
            "user.email=test@example.invalid",
            "commit",
            "-m",
            "new work",
        ],
    );
    let new_head = git(&f.main, &["rev-parse", "HEAD"]);
    git(&f.key, &["reset", "--hard", &new_head]);
    f.proxy.frames.lock().unwrap().clear();
    let records = collect(motor.run(f.request("true")).unwrap()).await;
    assert_eq!(terminal(&records).outcome, Outcome::Exit { code: 0 });
    assert!(
        terminal(&records).bundle_bytes < 20_000,
        "old history resent: {}",
        terminal(&records).bundle_bytes
    );
    assert!(terminal(&records).bundle_bytes > 0);
}

#[tokio::test]
async fn oversized_vcpu_refusal_detail_reaches_the_run_terminal() {
    let f = Fixture::new(Cut::None, 28).await;
    let motor = f.motor();
    let mut request = f.request("true");
    request.weight_hint = Some(40);

    let records = collect(motor.run(request).unwrap()).await;
    let terminal = terminal(&records);
    assert_eq!(
        terminal.outcome,
        Outcome::RefusedBeforeStart {
            reason: RefusalReason::RunnerFull
        }
    );
    let detail = terminal.refusal_detail().expect("terminal refusal detail");
    // The upper bound depends on the host's CPUs (30 on the build server, 2 on
    // a 4-vCPU CI machine), so only the request and the range wording are fixed.
    assert!(detail.contains("vcpu request 40"), "{detail}");
    assert!(
        detail.contains("outside the supported range 1..="),
        "{detail}"
    );
    assert_eq!(terminal.ran, Some(Ran::None));
}
