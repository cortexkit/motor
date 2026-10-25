use super::recovery::{collect, Cut, Fixture};
use crate::{transport::Duplex, Config, Connector, Motor, SshConnector};
use motor_protocol::*;
use serde_json::{json, Value};
use std::{
    fs,
    future::Future,
    io,
    path::PathBuf,
    pin::Pin,
    sync::{
        atomic::{AtomicBool, AtomicUsize, Ordering},
        Arc, Mutex,
    },
    time::Duration,
};
use subc_client_rs::HandlerOutcome;

const WINDOWS_ROOT: &str = "/srv/build/motor-windows/windows/state";

fn configured(mut config: Config) -> Config {
    config.windows_runner_instance = Some("windows".into());
    config.windows_runner_state_root = Some(WINDOWS_ROOT.into());
    config
}

#[derive(Clone, Debug)]
struct Visit {
    id: Uuid,
    instance: String,
    root: Option<String>,
    at: tokio::time::Instant,
}

struct RoutingPeer {
    visits: Mutex<Vec<Visit>>,
    frames: Arc<Mutex<Vec<Frame>>>,
    platforms: Vec<String>,
    echo: Option<Platform>,
    module_state: PathBuf,
    linux_status: RunnerStatusReply,
    windows_status: RunnerStatusReply,
    cut_attaches: Arc<AtomicUsize>,
    fail_cancel_once: Arc<AtomicBool>,
    fail_windows: AtomicBool,
    ally_behavior: AtomicUsize,
    endpoints: Mutex<Vec<crate::WindowsRunner>>,
}

impl RoutingPeer {
    fn new(f: &Fixture) -> Arc<Self> {
        let mut linux_status = f.runner.runner_status();
        linux_status.status.rustc_version = "linux-toolchain".into();
        let mut windows_status = linux_status.clone();
        windows_status.status.rustc_version = "1.99.0-windows-baseline".into();
        windows_status.health.as_mut().unwrap().failed_job_units = Some(7);
        Arc::new(Self {
            visits: Mutex::new(Vec::new()),
            frames: Arc::new(Mutex::new(Vec::new())),
            platforms: vec!["windows".into()],
            echo: Some(Platform::Windows),
            module_state: f.scratch.join(format!("routing-state-{}", Uuid::now_v7())),
            linux_status,
            windows_status,
            cut_attaches: Arc::new(AtomicUsize::new(0)),
            fail_cancel_once: Arc::new(AtomicBool::new(false)),
            fail_windows: AtomicBool::new(false),
            ally_behavior: AtomicUsize::new(0),
            endpoints: Mutex::new(Vec::new()),
        })
    }
    fn motor(self: &Arc<Self>, config: Config) -> Motor {
        Motor::with_connector(config, &self.module_state, self.clone()).unwrap()
    }
    fn assert_windows_only(&self) {
        let visits = self.visits.lock().unwrap();
        assert!(!visits.is_empty());
        for visit in visits.iter() {
            assert_eq!(visit.instance, "windows", "{visit:?}");
            assert_eq!(visit.root.as_deref(), Some(WINDOWS_ROOT), "{visit:?}");
        }
    }
}

fn terminal(id: Uuid, seq: u64) -> Frame {
    Frame::Terminal(
        TerminalRecord::new(
            id,
            Outcome::RefusedBeforeStart {
                reason: RefusalReason::Unknown("scripted_peer".into()),
            },
            seq,
            0,
            0,
        )
        .with_ran(Ran::None),
    )
}

impl Connector for RoutingPeer {
    fn connect_windows<'a>(
        &'a self,
        id: Uuid,
        runner: &'a crate::WindowsRunner,
    ) -> Pin<Box<dyn Future<Output = io::Result<Box<dyn Duplex>>> + Send + 'a>> {
        self.endpoints.lock().unwrap().push(runner.clone());
        self.connect_to(id, &runner.instance, Some(&runner.state_root))
    }
    fn connect(
        &self,
        _id: Uuid,
    ) -> Pin<Box<dyn Future<Output = io::Result<Box<dyn Duplex>>> + Send + '_>> {
        panic!("job and report channels must resolve an instance explicitly")
    }
    fn connect_to<'a>(
        &'a self,
        id: Uuid,
        instance: &'a str,
        root: Option<&'a str>,
    ) -> Pin<Box<dyn Future<Output = io::Result<Box<dyn Duplex>>> + Send + 'a>> {
        self.visits.lock().unwrap().push(Visit {
            id,
            instance: instance.into(),
            root: root.map(str::to_owned),
            at: tokio::time::Instant::now(),
        });
        let ally_behavior = if instance == "ally-windows" {
            self.ally_behavior.load(Ordering::SeqCst)
        } else {
            0
        };
        let windows = instance == "windows" || instance == "ally-windows";
        let offline = windows && self.fail_windows.load(Ordering::SeqCst);
        let platforms = if windows {
            self.platforms.clone()
        } else {
            Vec::new()
        };
        let echo = if windows {
            self.echo.clone()
        } else {
            Some(Platform::Linux)
        };
        let status = if windows {
            self.windows_status.clone()
        } else {
            self.linux_status.clone()
        };
        let frames = self.frames.clone();
        let module_state = self.module_state.clone();
        let instance = instance.to_owned();
        let cut_attaches = self.cut_attaches.clone();
        let fail_cancel = self.fail_cancel_once.clone();
        Box::pin(async move {
            if ally_behavior == 1 {
                return std::future::pending().await;
            }
            if offline {
                return Err(io::Error::other("windows peer offline"));
            }
            let (client, mut peer) = tokio::io::duplex(65536);
            tokio::spawn(async move {
                assert!(matches!(
                    read_frame_async(&mut peer).await.unwrap(),
                    Some(Frame::Hello { .. })
                ));
                write_frame_async(
                    &mut peer,
                    &Frame::Hello {
                        return_paths: false,
                        return_chunked: false,
                        sibling_paths: false,
                        protocol_version: PROTOCOL_VERSION,
                        started: true,
                        default_branch_tip: false,
                        gate_jobs: false,
                        platforms,
                    },
                )
                .await
                .unwrap();
                while let Ok(Some(frame)) = read_frame_async(&mut peer).await {
                    frames.lock().unwrap().push(frame.clone());
                    match frame {
                        Frame::Have { .. } => {
                            if write_frame_async(
                                &mut peer,
                                &Frame::HaveReply {
                                    repositories: Vec::new(),
                                    git_dependencies: Some(Vec::new()),
                                },
                            )
                            .await
                            .is_err()
                            {
                                return;
                            }
                        }
                        Frame::Run(run) => {
                            let stored = crate::storage::durable_value(&module_state).unwrap();
                            assert_eq!(
                                stored["jobs"][id.to_string()]["runner_instance"],
                                instance,
                                "binding must be durable before Run"
                            );
                            assert_eq!(run.platform, windows.then_some(Platform::Windows));
                            if ally_behavior == 2 {
                                let _ = write_frame_async(
                                    &mut peer,
                                    &Frame::Refusal(Refusal {
                                        job_id: Some(id),
                                        transfer_id: None,
                                        reason: RefusalReason::RunnerFull,
                                        disk_full: None,
                                        retry_after_ms: None,
                                        detail: Some("windows_slot_busy".into()),
                                    }),
                                )
                                .await;
                                return;
                            }
                            if ally_behavior == 3 {
                                return;
                            }
                            let mut accepted = Accepted::new(id, 3);
                            accepted.platform = echo;
                            let _ = write_frame_async(&mut peer, &Frame::Accepted(accepted)).await;
                            if ally_behavior == 4 {
                                let _ = write_frame_async(
                                    &mut peer,
                                    &Frame::Started {
                                        job_id: id,
                                        seq: 0,
                                        queue_wait_ms: 0,
                                        started_at_ms: 1,
                                        path_mapping: None,
                                    },
                                )
                                .await;
                            }
                            return;
                        }
                        Frame::Attach(request) => {
                            if ally_behavior == 6 {
                                std::future::pending::<()>().await;
                            }
                            if cut_attaches
                                .try_update(Ordering::SeqCst, Ordering::SeqCst, |n| {
                                    n.checked_sub(1)
                                })
                                .is_ok()
                            {
                                assert_eq!(request.from_seq, 0);
                                let _ = write_frame_async(
                                    &mut peer,
                                    &Frame::Output(Output::new(
                                        0,
                                        OutputStream::Stdout,
                                        BytePayload(b"first".to_vec()),
                                    )),
                                )
                                .await;
                            } else {
                                let _ =
                                    write_frame_async(&mut peer, &terminal(id, request.from_seq))
                                        .await;
                            }
                            return;
                        }
                        Frame::Cancel(_) => {
                            if fail_cancel.swap(false, Ordering::SeqCst) {
                                return;
                            }
                            let _ = write_frame_async(
                                &mut peer,
                                &Frame::CancelReply(CancelReply::new(id)),
                            )
                            .await;
                            return;
                        }
                        Frame::JobStatus { .. } => {
                            let _ = write_frame_async(
                                &mut peer,
                                &Frame::JobStatusReply(JobStatusReply {
                                    job_id: id,
                                    state: instance.clone(),
                                    waiting_on: None,
                                    terminal: None,
                                    unit: None,
                                    unit_alive: None,
                                    queue_wait_ms: None,
                                }),
                            )
                            .await;
                            return;
                        }
                        Frame::Status => {
                            let mut status = status;
                            if ally_behavior == 5 {
                                status.status.queue_depth = 1;
                                status.health.as_mut().unwrap().queue_depth = 1;
                            }
                            let _ = write_frame_async(&mut peer, &Frame::StatusReply(status)).await;
                            return;
                        }
                        _ => {}
                    }
                }
            });
            Ok(Box::new(client) as Box<dyn Duplex>)
        })
    }
}

fn ordered(config: Config) -> Config {
    let key = ssh_key::PrivateKey::random(&mut rand::rngs::OsRng, ssh_key::Algorithm::Ed25519)
        .unwrap()
        .public_key()
        .to_openssh()
        .unwrap();
    Config {
        windows_runners: vec![
            crate::WindowsRunner {
                name: "ally".into(),
                host: "192.0.2.41".into(),
                user: "runner".into(),
                host_key: key.clone(),
                instance: "ally-windows".into(),
                state_root: WINDOWS_ROOT.into(),
                ..Default::default()
            },
            crate::WindowsRunner {
                name: "build-server".into(),
                host: "192.0.2.42".into(),
                user: "runner".into(),
                host_key: key,
                instance: "windows".into(),
                state_root: WINDOWS_ROOT.into(),
                ..Default::default()
            },
        ],
        ..config
    }
}

#[tokio::test]
async fn ordered_windows_runners_prefer_first_and_persist_full_binding_across_restart() {
    let f = Fixture::new(Cut::None, 28).await;
    let peer = RoutingPeer::new(&f);
    let config = ordered(f.config.clone());
    let motor = peer.motor(config.clone());
    let mut request = f.request("true");
    request.platform = Some(Platform::Windows);
    let stream = motor.run(request).unwrap();
    let id = stream.job_id;
    collect(stream).await;
    assert!(peer
        .visits
        .lock()
        .unwrap()
        .iter()
        .all(|v| v.instance == "ally-windows"));
    // The store keeps recent mutations in its journal, so read the durable
    // state (checkpoint plus journal), not the checkpoint file alone.
    let stored = crate::storage::durable_value(&peer.module_state).unwrap();
    assert_eq!(
        stored["jobs"][id.to_string()]["windows_runner"]["host"],
        "192.0.2.41"
    );
    drop(motor);
    let mut changed = config;
    changed.windows_runners.reverse();
    changed.windows_runners[1].host = "192.0.2.99".into();
    let motor = peer.motor(changed);
    peer.visits.lock().unwrap().clear();
    peer.endpoints.lock().unwrap().clear();
    assert_eq!(motor.job_status(id).await.unwrap().state, "ally-windows");
    motor.cancel(CancelRequest::new(id)).await.unwrap();
    collect(motor.attach(AttachRequest::new(id, 0)).unwrap()).await;
    assert!(peer
        .visits
        .lock()
        .unwrap()
        .iter()
        .all(|v| v.instance == "ally-windows"));
    assert!(peer
        .endpoints
        .lock()
        .unwrap()
        .iter()
        .all(|r| r.host == "192.0.2.41"));
    let stored = crate::storage::durable_value(&peer.module_state).unwrap();
    assert_eq!(
        stored["jobs"][id.to_string()]["windows_runner"]["host"],
        "192.0.2.41"
    );
}

#[tokio::test]
async fn ordered_windows_unreachable_handshake_falls_through_within_three_seconds() {
    let f = Fixture::new(Cut::None, 28).await;
    let peer = RoutingPeer::new(&f);
    peer.ally_behavior.store(1, Ordering::SeqCst);
    let motor = peer.motor(ordered(f.config.clone()));
    let mut request = f.request("true");
    request.platform = Some(Platform::Windows);
    collect(motor.run(request).unwrap()).await;
    let visits = peer.visits.lock().unwrap();
    assert_eq!(visits[0].instance, "ally-windows");
    assert!(visits.iter().skip(1).all(|v| v.instance == "windows"));
    // Time only the gap between the Ally attempt and the fallback connect.
    // Snapshot capture before the first connect slows down on a loaded host
    // and is not what this test is about.
    let fallback = visits[1].at.duration_since(visits[0].at);
    assert!(fallback >= Duration::from_secs(3), "{fallback:?}");
    assert!(fallback < Duration::from_secs(6), "{fallback:?}");
}

#[tokio::test]
async fn ordered_windows_local_slot_reservation_falls_through_without_queueing() {
    let f = Fixture::new(Cut::None, 28).await;
    let peer = RoutingPeer::new(&f);
    peer.ally_behavior.store(6, Ordering::SeqCst);
    let motor = peer.motor(ordered(f.config.clone()));
    let mut first = motor
        .run(f.request("true").with_platform(Platform::Windows))
        .unwrap();
    assert!(matches!(
        first.records.recv().await.unwrap().unwrap(),
        StreamRecord::Accepted(_)
    ));
    let second = motor
        .run(f.request("true").with_platform(Platform::Windows))
        .unwrap();
    let second_id = second.job_id;
    collect(second).await;
    assert!(peer
        .visits
        .lock()
        .unwrap()
        .iter()
        .filter(|v| v.id == second_id)
        .all(|v| v.instance == "windows"));
    drop(first);
}

#[tokio::test]
async fn ordered_windows_busy_refusal_resubmits_but_lost_ack_and_started_never_move() {
    for behavior in [2, 3, 4, 5] {
        let f = Fixture::new(Cut::None, 28).await;
        let peer = RoutingPeer::new(&f);
        peer.ally_behavior.store(behavior, Ordering::SeqCst);
        let motor = peer.motor(ordered(f.config.clone()));
        let mut request = f.request("true");
        request.platform = Some(Platform::Windows);
        let records = collect(motor.run(request).unwrap()).await;
        let visits = peer.visits.lock().unwrap();
        assert_eq!(visits[0].instance, "ally-windows");
        assert_eq!(
            visits.iter().any(|v| v.instance == "windows"),
            matches!(behavior, 2 | 5),
            "behavior {behavior}"
        );
        assert_eq!(
            records
                .iter()
                .filter(|r| matches!(r, StreamRecord::Accepted(_)))
                .count(),
            usize::from(behavior != 3)
        );
    }
}

#[tokio::test]
async fn ordered_windows_health_reports_each_runner_reachability_and_busy_state() {
    let f = Fixture::new(Cut::None, 28).await;
    let peer = RoutingPeer::new(&f);
    peer.ally_behavior.store(5, Ordering::SeqCst);
    let motor = peer.motor(ordered(f.config.clone()));
    let samples = motor.windows_health().await;
    assert_eq!(samples[0]["name"], "ally");
    assert_eq!(samples[0]["reachable"], true);
    assert_eq!(samples[0]["busy"], true);
    assert_eq!(samples[1]["name"], "build-server");
    assert_eq!(samples[1]["busy"], false);
    peer.ally_behavior.store(1, Ordering::SeqCst);
    let samples = motor.windows_health().await;
    assert_eq!(samples[0]["reachable"], false);
    assert_eq!(samples[0]["busy"], Value::Null);
    assert_eq!(samples[1]["reachable"], true);
}

#[test]
fn ordered_windows_config_defaults_are_omitted_and_legacy_pair_still_routes() {
    let config = Config::default();
    assert!(serde_json::to_value(&config)
        .unwrap()
        .get("windows_runners")
        .is_none());
    assert_eq!(
        serde_json::to_value(crate::WindowsRunner::default()).unwrap(),
        json!({})
    );
    let old = configured(config);
    let decoded: Config = serde_json::from_value(serde_json::to_value(&old).unwrap()).unwrap();
    assert_eq!(
        decoded.target(Some(&Platform::Windows)).unwrap().instance,
        "windows"
    );
    let new = ordered(old);
    new.validate().unwrap();
    assert_eq!(
        new.target(Some(&Platform::Windows)).unwrap().instance,
        "ally-windows"
    );
    let mut bad = new.clone();
    bad.windows_runners[0].host_key.push_str("\nmalicious");
    assert!(bad.validate().is_err());
    let mut bad = new.clone();
    bad.windows_runners[0].max_concurrent = 0;
    assert!(bad.validate().is_err());
    let mut bad = new;
    bad.windows_runners[1].name = "ally".into();
    assert!(bad.validate().is_err());
}

fn ally_entry() -> serde_json::Value {
    serde_json::from_str(r#"{"name":"ally","host":"192.168.1.44","user":"motor-ssh","host_key":"ssh-ed25519 AAAAC3NzaC1lZDI1NTE5AAAAIOQbca4geGC+qIj32MrwVGGuBxvSCm8EsEMRT41+YEW8","instance":"ally","state_root":"C:/ProgramData/MotorRunner/state"}"#).unwrap()
}

fn posix_windows_entry() -> serde_json::Value {
    let mut entry = ally_entry();
    entry["name"] = "build-server".into();
    entry["host"] = "build-server".into();
    entry["instance"] = "windows".into();
    entry["state_root"] = "/var/lib/motor-win/state".into();
    entry
}

#[test]
fn ordered_windows_config_accepts_ally_drive_root_and_mixed_posix_entries() {
    for entries in [
        vec![ally_entry()],
        vec![posix_windows_entry(), ally_entry()],
    ] {
        let config: Config = serde_json::from_value(serde_json::json!({
            "windows_runners": entries
        }))
        .unwrap();
        config.validate().unwrap();
        let ally = config.windows_runners.last().unwrap();
        assert_eq!(ally.name, "ally");
        assert_eq!(ally.state_root, "C:/ProgramData/MotorRunner/state");
        assert_eq!(ally.port, 22);
        assert_eq!(ally.max_concurrent, 1);
        assert!(!ally.state_root.contains(char::is_whitespace));
    }
}

#[test]
fn ordered_windows_config_refuses_unsafe_state_roots_with_entry_and_field() {
    for root in [
        r"C:\x",
        "C:/a/../b",
        "C:/a b",
        "1:/x",
        "C:x",
        "C:/a:b",
        "/a/../b",
        "C:/a\tb",
        "C:/a\nb",
        "C:/a;b",
        "C:/a&b",
        "C:/a|b",
        "C:/a$b",
        "C:/a`b",
        "C:/a'b",
        "C:/a\"b",
        "C:/a>b",
        "C:/a<b",
        "C:/a(b",
        "C:/a*b",
        "C:/a?b",
    ] {
        let mut entry = ally_entry();
        entry["state_root"] = root.into();
        let config: Config = serde_json::from_value(serde_json::json!({
            "windows_runners": [posix_windows_entry(), entry]
        }))
        .unwrap();
        let error = config.validate().unwrap_err().to_string();
        assert!(
            error.starts_with("windows_runners[1] (ally): state_root "),
            "{root:?}: {error}"
        );
    }
}

#[test]
fn ordered_windows_config_names_every_invalid_entry_field() {
    for (field, value) in [
        ("name", serde_json::json!("bad name")),
        ("host", serde_json::json!("-option")),
        ("host", serde_json::json!("bad host")),
        ("user", serde_json::json!("-option")),
        ("user", serde_json::json!("bad user")),
        ("port", serde_json::json!(0)),
        ("instance", serde_json::json!("bad.instance")),
        ("instance", serde_json::json!("")),
        ("host_key", serde_json::json!("not a key")),
        (
            "host_key",
            serde_json::json!(format!("{}\n", ally_entry()["host_key"].as_str().unwrap())),
        ),
        ("max_concurrent", serde_json::json!(0)),
        ("max_concurrent", serde_json::json!(usize::MAX)),
    ] {
        let mut entry = ally_entry();
        entry[field] = value;
        let name = entry["name"].as_str().unwrap().to_owned();
        let config: Config = serde_json::from_value(serde_json::json!({
            "windows_runners": [posix_windows_entry(), entry]
        }))
        .unwrap();
        let error = config.validate().unwrap_err().to_string();
        assert!(
            error.starts_with(&format!("windows_runners[1] ({name}): {field} ")),
            "{error}"
        );
    }
    let config: Config = serde_json::from_value(serde_json::json!({
        "windows_runners": [ally_entry(), ally_entry()]
    }))
    .unwrap();
    assert_eq!(
        config.validate().unwrap_err().to_string(),
        "windows_runners[1] (ally): name must be unique"
    );
    let mut macos = posix_windows_entry();
    macos.as_object_mut().unwrap().remove("name");
    macos["instance"] = "ally".into();
    macos["user"] = "motor".into();
    let config: Config = serde_json::from_value(serde_json::json!({
        "windows_runners": [posix_windows_entry(), ally_entry()],
        "macos_runner": macos
    }))
    .unwrap();
    assert_eq!(
        config.validate().unwrap_err().to_string(),
        "windows_runners[1] (ally): instance must differ from macos_runner.instance"
    );
}

fn write_job(peer: &RoutingPeer, id: Uuid, instance: Option<&str>, pending: bool) {
    fs::create_dir_all(&peer.module_state).unwrap();
    let mut job = json!({"queue_wait_limit_s":900,"pending_cancel":pending,"local_terminal":null});
    if let Some(instance) = instance {
        job["runner_instance"] = json!(instance);
    }
    fs::write(
        peer.module_state.join("state.json"),
        serde_json::to_vec(&json!({"jobs":{id.to_string():job},"snapshots":{}})).unwrap(),
    )
    .unwrap();
}

async fn response_bytes(result: io::Result<HandlerOutcome>) -> Vec<u8> {
    let HandlerOutcome::Response(bytes) = result.unwrap() else {
        panic!("expected response")
    };
    bytes
}

#[test]
fn windows_config_load_requires_safe_distinct_paired_keys() {
    let scratch = cortexkit_test_support::ScratchDir::new("motor-routing-config");
    let path = scratch.join("config.json");
    let base = Config {
        runner_instance: "linux-test".into(),
        ..Config::default()
    };
    for (instance, root) in [
        (Some("windows"), None),
        (None, Some(WINDOWS_ROOT)),
        (Some(""), Some(WINDOWS_ROOT)),
        (Some("win.dows"), Some(WINDOWS_ROOT)),
        (Some("win/dows"), Some(WINDOWS_ROOT)),
        (Some("win;dows"), Some(WINDOWS_ROOT)),
        (Some("wíndows"), Some(WINDOWS_ROOT)),
        (Some("linux-test"), Some(WINDOWS_ROOT)),
        (Some("windows"), Some("relative/state")),
        (Some("windows"), Some("/srv/state root")),
        (Some("windows"), Some("/srv/state;echo")),
        (Some("windows"), Some("/srv/$HOME")),
        (Some("windows"), Some("/srv/é")),
        (Some("windows"), Some("/srv/a\n")),
    ] {
        let mut config = base.clone();
        config.windows_runner_instance = instance.map(str::to_owned);
        config.windows_runner_state_root = root.map(str::to_owned);
        assert!(config.validate().is_err(), "{instance:?} {root:?}");
        fs::write(&path, serde_json::to_vec(&config).unwrap()).unwrap();
        assert!(Config::load(&path).is_err(), "{instance:?} {root:?}");
    }
    let mut config = configured(base);
    config.windows_runner_instance = Some("Windows_2-test".into());
    config.windows_runner_state_root = Some("/srv/build/motor_windows-2/state.v1".into());
    fs::write(&path, serde_json::to_vec(&config).unwrap()).unwrap();
    Config::load(&path).unwrap();
    Config::default().validate().unwrap();
    let legacy_config = serde_json::to_value(Config::default()).unwrap();
    assert!(legacy_config.get("windows_runner_instance").is_none());
    assert!(legacy_config.get("windows_runner_state_root").is_none());
}

#[test]
fn bridge_commands_preserve_linux_bytes_and_include_windows_state_root() {
    let id = Uuid::parse_str("00000000-0000-0000-0000-000000000001").unwrap();
    let connector = SshConnector::new(configured(Config::default()));
    let linux = connector.command(id);
    let linux_args: Vec<_> = linux
        .as_std()
        .get_args()
        .map(|s| s.to_str().unwrap())
        .collect();
    assert_eq!(linux_args.last().unwrap(), &"~/.cargo/bin/motor-runner connect --instance main --job 00000000-0000-0000-0000-000000000001");
    let windows = connector.command_for(id, "windows", Some(WINDOWS_ROOT));
    let windows_args: Vec<_> = windows
        .as_std()
        .get_args()
        .map(|s| s.to_str().unwrap())
        .collect();
    assert_eq!(windows_args.last().unwrap(), &"~/.cargo/bin/motor-runner connect --instance windows --state-root /srv/build/motor-windows/windows/state --job 00000000-0000-0000-0000-000000000001");
    assert_eq!(
        &linux_args[..linux_args.len() - 1],
        &windows_args[..windows_args.len() - 1]
    );
}

#[tokio::test]
async fn unconfigured_windows_and_unknown_platforms_refuse_before_any_channel() {
    let f = Fixture::new(Cut::None, 28).await;
    let peer = RoutingPeer::new(&f);
    let hello_manifest =
        serde_json::to_value(crate::module::manifest_with_config("ck-motor", &f.config)).unwrap();
    assert!(!hello_manifest
        .to_string()
        .contains(crate::module::WINDOWS_CAPABILITY));
    let motor = peer.motor(f.config.clone());
    for platform in [Platform::Windows, Platform::Unknown("plan9".into())] {
        let name = serde_json::to_value(&platform)
            .unwrap()
            .as_str()
            .unwrap()
            .to_owned();
        let mut request = f.request("touch must-not-run").with_platform(platform);
        request.workspace_key = "/nonexistent/workspace".into();
        let records = collect(motor.run(request).unwrap()).await;
        let StreamRecord::Terminal(t) = &records[0] else {
            panic!("{records:?}")
        };
        assert_eq!(records.len(), 1);
        assert_eq!(t.ran, Some(Ran::None));
        assert!(matches!(
            t.outcome,
            Outcome::RefusedBeforeStart {
                reason: RefusalReason::PlatformUnsupported
            }
        ));
        assert!(t.refusal_detail.as_ref().unwrap().contains(&name));
    }
    assert!(peer.visits.lock().unwrap().is_empty());
    assert!(!peer.module_state.join("spools").exists());
}

#[tokio::test]
async fn windows_hello_gate_refuses_old_peer_without_sending_run() {
    let f = Fixture::new(Cut::None, 28).await;
    let mut peer = RoutingPeer::new(&f);
    Arc::get_mut(&mut peer).unwrap().platforms.clear();
    let motor = peer.motor(configured(f.config.clone()));
    let records = collect(
        motor
            .run(
                f.request("touch must-not-run")
                    .with_platform(Platform::Windows),
            )
            .unwrap(),
    )
    .await;
    let StreamRecord::Terminal(t) = &records[0] else {
        panic!("{records:?}")
    };
    assert!(matches!(
        t.outcome,
        Outcome::RefusedBeforeStart {
            reason: RefusalReason::PlatformUnsupported
        }
    ));
    assert_eq!(t.ran, Some(Ran::None));
    assert!(t.refusal_detail.as_ref().unwrap().contains("windows"));
    assert!(
        peer.frames.lock().unwrap().is_empty(),
        "old peer must get no Have, Run or bundle"
    );
    peer.assert_windows_only();
}

#[tokio::test]
async fn windows_run_binding_echo_and_reconnect_survive_module_restart() {
    let f = Fixture::new(Cut::None, 28).await;
    let peer = RoutingPeer::new(&f);
    peer.cut_attaches.store(1, Ordering::SeqCst);
    let config = configured(f.config.clone());
    let motor = peer.motor(config.clone());
    let stream = motor
        .run(f.request("true").with_platform(Platform::Windows))
        .unwrap();
    let id = stream.job_id;
    let records = collect(stream).await;
    let StreamRecord::Accepted(accepted) = &records[0] else {
        panic!("{records:?}")
    };
    assert_eq!(accepted.platform, Some(Platform::Windows));
    assert_eq!(accepted.queue_position, 3);
    assert!(records.iter().any(|r| matches!(r, StreamRecord::Output(_))));
    {
        let frames = peer.frames.lock().unwrap();
        let cursors: Vec<_> = frames
            .iter()
            .filter_map(|f| {
                if let Frame::Attach(a) = f {
                    Some(a.from_seq)
                } else {
                    None
                }
            })
            .collect();
        assert_eq!(cursors, vec![0, 1]);
    }
    let state = crate::storage::durable_value(&peer.module_state).unwrap();
    assert_eq!(state["jobs"][id.to_string()]["runner_instance"], "windows");
    drop(motor);
    let restarted = super::reopen_motor(|| {
        Motor::with_connector(config.clone(), &peer.module_state, peer.clone())
    })
    .await;
    collect(restarted.attach(AttachRequest::new(id, 1)).unwrap()).await;
    assert_eq!(restarted.job_status(id).await.unwrap().state, "windows");
    let reply: Value = serde_json::from_slice(
        &response_bytes(crate::module::status_reply(&restarted, &json!({"job_id":id})).await).await,
    )
    .unwrap();
    assert_eq!(reply["state"], "windows");
    restarted.cancel(CancelRequest::new(id)).await.unwrap();
    assert!(!restarted.pending(id));
    peer.assert_windows_only();
    assert!(peer.visits.lock().unwrap().iter().all(|v| v.id == id));
    assert!(
        f.jobs().is_empty(),
        "Linux runner must have no record for the Windows job"
    );
}

#[tokio::test]
async fn windows_wrong_or_absent_accepted_echo_is_forwarded_and_cancelled() {
    let f = Fixture::new(Cut::None, 28).await;
    for echo in [Some(Platform::Linux), None] {
        let mut peer = RoutingPeer::new(&f);
        Arc::get_mut(&mut peer).unwrap().echo = echo.clone();
        let motor = peer.motor(configured(f.config.clone()));
        let stream = motor
            .run(
                f.request("must-not-upload")
                    .with_platform(Platform::Windows),
            )
            .unwrap();
        let id = stream.job_id;
        let records = collect(stream).await;
        let StreamRecord::Accepted(accepted) = &records[0] else {
            panic!("{records:?}")
        };
        assert_eq!(accepted.platform, echo);
        let frames = peer.frames.lock().unwrap();
        assert!(frames
            .iter()
            .any(|f| matches!(f, Frame::Cancel(c) if c.job_id == id)));
        assert!(!frames
            .iter()
            .any(|f| matches!(f, Frame::BundleChunk { .. })));
        drop(frames);
        peer.assert_windows_only();
        drop(motor);
    }
}

#[tokio::test]
async fn linux_run_omits_platform_and_forwards_linux_echo() {
    let f = Fixture::new(Cut::None, 28).await;
    let peer = RoutingPeer::new(&f);
    let motor = peer.motor(configured(f.config.clone()));
    for platform in [None, Some(Platform::Linux)] {
        let mut request = f.request("true");
        request.platform = platform;
        let records = collect(motor.run(request).unwrap()).await;
        let StreamRecord::Accepted(accepted) = &records[0] else {
            panic!("{records:?}")
        };
        assert_eq!(accepted.platform, Some(Platform::Linux));
    }
    assert!(peer
        .frames
        .lock()
        .unwrap()
        .iter()
        .filter_map(|f| if let Frame::Run(r) = f { Some(r) } else { None })
        .all(|r| r.platform.is_none()));
    assert!(peer
        .visits
        .lock()
        .unwrap()
        .iter()
        .all(|v| v.instance == "e2e" && v.root.is_none()));
}

#[tokio::test]
async fn legacy_record_with_pending_cancel_and_missing_record_stay_on_linux() {
    let f = Fixture::new(Cut::None, 28).await;
    let peer = RoutingPeer::new(&f);
    let id = Uuid::now_v7();
    write_job(&peer, id, None, true);
    let motor = peer.motor(configured(f.config.clone()));
    collect(motor.attach(AttachRequest::new(id, 0)).unwrap()).await;
    assert!(!motor.pending(id));
    assert_eq!(motor.job_status(id).await.unwrap().state, "e2e");
    motor.cancel(CancelRequest::new(id)).await.unwrap();
    let absent = Uuid::now_v7();
    collect(motor.attach(AttachRequest::new(absent, 0)).unwrap()).await;
    assert_eq!(motor.job_status(absent).await.unwrap().state, "e2e");
    motor.cancel(CancelRequest::new(absent)).await.unwrap();
    assert!(peer
        .visits
        .lock()
        .unwrap()
        .iter()
        .all(|v| v.instance == "e2e" && v.root.is_none()));
    assert!(peer
        .frames
        .lock()
        .unwrap()
        .iter()
        .any(|f| matches!(f, Frame::Cancel(c) if c.job_id == id)));
}

#[tokio::test]
async fn removed_or_unknown_stored_instance_fails_closed_without_channels() {
    let f = Fixture::new(Cut::None, 28).await;
    let peer = RoutingPeer::new(&f);
    let id = Uuid::now_v7();
    for (index, instance) in ["windows", "retired-linux"].into_iter().enumerate() {
        write_job(&peer, id, Some(instance), true);
        let motor = if index == 0 {
            peer.motor(f.config.clone())
        } else {
            super::reopen_motor(|| {
                Motor::with_connector(f.config.clone(), &peer.module_state, peer.clone())
            })
            .await
        };
        assert!(motor.attach(AttachRequest::new(id, 0)).is_err());
        assert!(motor.job_status(id).await.is_err());
        assert!(
            crate::module::status_reply(&motor, &json!({"job_id":id,"platform":"linux"}))
                .await
                .is_err()
        );
        assert!(motor.cancel(CancelRequest::new(id)).await.is_err());
        assert!(peer.visits.lock().unwrap().is_empty());
        assert!(
            motor.pending(id),
            "refusing an unconfigured binding must not clear cancel intent"
        );
        drop(motor);
    }
}

#[tokio::test]
async fn detached_windows_cancel_retry_keeps_the_persisted_instance() {
    let f = Fixture::new(Cut::None, 28).await;
    let peer = RoutingPeer::new(&f);
    let id = Uuid::now_v7();
    write_job(&peer, id, Some("windows"), false);
    let motor = peer.motor(configured(f.config.clone()));
    peer.fail_cancel_once.store(true, Ordering::SeqCst);
    motor.cancel(CancelRequest::new(id)).await.unwrap();
    tokio::time::timeout(Duration::from_secs(5), async {
        while motor.pending(id) {
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();
    assert_eq!(
        peer.frames
            .lock()
            .unwrap()
            .iter()
            .filter(|f| matches!(f, Frame::Cancel(c) if c.job_id == id))
            .count(),
        2
    );
    peer.assert_windows_only();
    assert!(f.jobs().is_empty());
}

#[tokio::test]
async fn platform_reports_select_windows_and_default_linux_reply_bytes_are_unchanged() {
    let f = Fixture::new(Cut::None, 28).await;
    let peer = RoutingPeer::new(&f);
    let motor = peer.motor(configured(f.config.clone()));
    for params in [json!({}), json!({"platform":"linux"})] {
        let bytes = response_bytes(crate::module::status_reply(&motor, &params).await).await;
        assert_eq!(
            bytes,
            serde_json::to_vec(&peer.linux_status.status).unwrap()
        );
        let bytes =
            response_bytes(crate::module::runner_health_reply_for(&motor, &params).await).await;
        assert_eq!(
            bytes,
            serde_json::to_vec(&peer.linux_status.health).unwrap()
        );
    }
    for (params, expected) in [
        (
            json!({"disk":true}),
            serde_json::to_vec(&peer.linux_status).unwrap(),
        ),
        (
            json!({"platform":"windows"}),
            serde_json::to_vec(&peer.windows_status.status).unwrap(),
        ),
        (
            json!({"platform":"windows","disk":true}),
            serde_json::to_vec(&peer.windows_status).unwrap(),
        ),
    ] {
        assert_eq!(
            response_bytes(crate::module::status_reply(&motor, &params).await).await,
            expected
        );
    }
    assert_eq!(
        response_bytes(
            crate::module::runner_health_reply_for(&motor, &json!({"platform":"windows"})).await
        )
        .await,
        serde_json::to_vec(&peer.windows_status.health).unwrap()
    );
    let visits = peer.visits.lock().unwrap();
    assert!(visits
        .iter()
        .any(|v| v.instance == "e2e" && v.root.is_none()));
    assert!(visits
        .iter()
        .any(|v| v.instance == "windows" && v.root.as_deref() == Some(WINDOWS_ROOT)));
}

#[tokio::test(start_paused = true)]
async fn windows_reports_error_when_unconfigured_or_unreachable_without_linux_fallback() {
    let f = Fixture::new(Cut::None, 28).await;
    let peer = RoutingPeer::new(&f);
    let mut config = f.config.clone();
    for configured_windows in [false, true] {
        if configured_windows {
            config = configured(config);
            peer.fail_windows.store(true, Ordering::SeqCst);
        }
        let motor = if configured_windows {
            super::reopen_motor(|| {
                Motor::with_connector(config.clone(), &peer.module_state, peer.clone())
            })
            .await
        } else {
            peer.motor(config.clone())
        };
        assert!(
            crate::module::status_reply(&motor, &json!({"platform":"windows"}))
                .await
                .is_err()
        );
        let HandlerOutcome::Error { code, message } =
            crate::module::runner_health_reply_for(&motor, &json!({"platform":"windows"}))
                .await
                .unwrap()
        else {
            panic!("must fail")
        };
        assert_eq!(code, "runner_health_unavailable");
        assert!(message.contains("windows"));
        if !configured_windows {
            assert!(peer.visits.lock().unwrap().is_empty());
        }
        drop(motor);
    }
    peer.assert_windows_only();
}

#[test]
fn manifest_advertises_windows_only_with_both_routing_keys() {
    let instance_only = Config {
        windows_runner_instance: Some("windows".into()),
        ..Config::default()
    };
    let root_only = Config {
        windows_runner_state_root: Some(WINDOWS_ROOT.into()),
        ..Config::default()
    };
    for (config, expected) in [
        (Config::default(), false),
        (instance_only, false),
        (root_only, false),
        (configured(Config::default()), true),
    ] {
        for network_routing in [false, true] {
            let mut config = config.clone();
            config.network_routing = network_routing;
            let manifest =
                serde_json::to_value(crate::module::manifest_with_config("ck-motor", &config))
                    .unwrap();
            let provides = manifest["capabilities"]["provides"].as_array().unwrap();
            assert_eq!(
                provides.contains(&json!(crate::module::WINDOWS_CAPABILITY)),
                expected
            );
            assert_eq!(
                provides.contains(&json!(crate::module::NETWORK_CAPABILITY)),
                network_routing
            );
            assert!(provides.contains(&json!(crate::module::CAPABILITY)));
            assert!(provides.contains(&json!(crate::module::RETURN_CAPABILITY)));
            assert!(provides.contains(&json!(crate::module::RETURN_CHUNKED_CAPABILITY)));
            // The daemon refuses the module's HELLO if any capability breaks its
            // grammar, which would take down every runon, Linux included.
            for capability in provides {
                let capability = capability.as_str().unwrap();
                assert!(
                    subc_protocol::manifest::is_valid_capability_identifier(capability),
                    "daemon would refuse capability {capability}"
                );
            }
        }
    }
}

#[test]
fn network_routing_defaults_off_and_omits_the_legacy_config_key() {
    let scratch = cortexkit_test_support::ScratchDir::new("motor-network-config");
    let path = scratch.join("config.json");
    let mut value = serde_json::to_value(Config {
        runner_instance: "network-config-test".into(),
        ..Config::default()
    })
    .unwrap();
    assert!(value.get("network_routing").is_none());
    fs::write(&path, serde_json::to_vec(&value).unwrap()).unwrap();
    assert!(!Config::load(&path).unwrap().network_routing);
    for enabled in [false, true] {
        value["network_routing"] = json!(enabled);
        fs::write(&path, serde_json::to_vec(&value).unwrap()).unwrap();
        let config = Config::load(&path).unwrap();
        assert_eq!(config.network_routing, enabled);
        let serialized = serde_json::to_value(config).unwrap();
        assert_eq!(
            serialized.get("network_routing"),
            enabled.then_some(&json!(true))
        );
    }
}

#[tokio::test]
async fn hosted_switch_keeps_windows_on_its_direct_stored_route() {
    let f = Fixture::new(Cut::None, 28).await;
    let peer = RoutingPeer::new(&f);
    let mut config = configured(f.config.clone());
    config.hosted_runners = true;
    config.broker_url = Some("https://unused.invalid".into());
    let motor = peer.motor(config.clone());
    let stream = motor
        .run(f.request("true").with_platform(Platform::Windows))
        .unwrap();
    let id = stream.job_id;
    let records = collect(stream).await;
    assert!(records
        .iter()
        .any(|r| matches!(r, StreamRecord::Accepted(_))));
    assert!(motor.hosted_state(id).is_none());
    drop(motor);
    config.hosted_runners = false;
    let restarted = super::reopen_motor(|| {
        Motor::with_connector(config.clone(), &peer.module_state, peer.clone())
    })
    .await;
    collect(restarted.attach(AttachRequest::new(id, 0)).unwrap()).await;
    restarted.cancel(CancelRequest::new(id)).await.unwrap();
    assert_eq!(restarted.job_status(id).await.unwrap().state, "windows");
    peer.assert_windows_only();
}
