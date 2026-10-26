use super::*;
use crate::windows::{prep, vm::UnitChild, WindowsBackend};
use motor_winagent::{Frame as AgentFrame, Peer, Session, Stream};
use std::{
    io::{Read, Write},
    net::Shutdown,
    os::unix::{net::UnixStream, process::ExitStatusExt},
    sync::atomic::AtomicBool,
};

// These tests skip startup account and ACL checks, which windows_instance.rs
// covers separately. Preparation, VM ownership and socket I/O still use the
// production backend with fake systemd commands and a fake guest agent.
struct FakeHost;
impl Backend for FakeHost {
    fn materialize(&self, c: &Config, t: &Trees, s: &Setup) -> io::Result<Prepared> {
        WindowsBackend.materialize(c, t, s)
    }
    fn launch(
        &self,
        c: &Config,
        t: &Trees,
        s: &Setup,
        p: &Prepared,
        w: u32,
    ) -> io::Result<backend::Launch> {
        WindowsBackend.launch(c, t, s, p, w)
    }
    fn cancel(&self, c: &Config, j: &Job, hard: bool) -> io::Result<()> {
        WindowsBackend.cancel(c, j, hard)
    }
    fn unit_alive(
        &self,
        c: &Config,
        p: Option<platform::ProcessIdentity>,
    ) -> io::Result<Option<bool>> {
        WindowsBackend.unit_alive(c, p)
    }
    fn collect_changes(&self, t: &Trees, j: &Job) -> io::Result<Option<Vec<String>>> {
        WindowsBackend.collect_changes(t, j)
    }
    fn reconcile(&self, c: &Config, j: &[Job]) -> io::Result<Vec<Uuid>> {
        WindowsBackend.reconcile(c, j)
    }
}
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Fault {
    None,
    Hello,
    Transfer,
    SlowTransfer,
    Start,
    RejectStart,
    Collect,
    Dismount,
    DismountOnce,
    CacheGrantMissingOnce,
    CacheGrantMissingAlways,
    // The guest's RAW-disk initialization fails part-way on the first boot
    // only, leaving a disk that is no longer RAW but holds no NTFS volume.
    CacheInitFailedOnce,
    CacheInitFailed,
    UnitExit,
    Silent,
    FloodOutput,
    Oversized,
    Malformed,
    Nonce,
    Version,
    Order,
    CollectOversized,
    DirtyDismount,
    BadUnitExit,
    DismountAtUnitExit,
    ClosedDismountAtUnitExit,
    ClosedAtUnitExit,
    TruncatedDismountAtUnitExit,
}
struct ExitReply {
    port: UnixStream,
    bytes: Vec<u8>,
    close: bool,
}
#[derive(Default)]
struct Agent {
    starts: AtomicU64,
    stops: AtomicU64,
    phase: AtomicU64,
    host_phase: AtomicU64,
    unit_exit_checked: AtomicBool,
    clock: AtomicU64,
    tree: Mutex<Vec<u8>>,
    units: Mutex<Vec<Vec<String>>>,
    exit_reply: Mutex<Option<ExitReply>>,
}
struct FakeChild {
    exit_code: i32,
    host_port: Option<UnixStream>,
    done: Arc<AtomicBool>,
    stop: Arc<AtomicBool>,
    socket: Arc<Mutex<Option<UnixStream>>>,
    thread: Option<std::thread::JoinHandle<()>>,
    agent: Arc<Agent>,
}
impl UnitChild for FakeChild {
    fn agent_stream(&mut self) -> Option<UnixStream> {
        self.host_port.take()
    }
    fn try_wait(&mut self) -> io::Result<Option<std::process::ExitStatus>> {
        if let Some(mut reply) = self.agent.exit_reply.lock().unwrap().take() {
            // This check follows a WouldBlock read. Publish the guest's last
            // bytes and its exit together to force the read/exit race without
            // relying on thread scheduling or elapsed time.
            reply.port.write_all(&reply.bytes)?;
            if reply.close {
                reply.port.shutdown(Shutdown::Write)?;
            }
            self.done.store(true, Ordering::SeqCst);
        }
        if self.agent.phase.load(Ordering::SeqCst) == 8 {
            self.agent.unit_exit_checked.store(true, Ordering::SeqCst);
        }
        Ok(self
            .done
            .load(Ordering::SeqCst)
            .then(|| std::process::ExitStatus::from_raw(self.exit_code << 8)))
    }
    fn stop(&mut self) -> io::Result<()> {
        self.agent.stops.fetch_add(1, Ordering::SeqCst);
        self.stop.store(true, Ordering::SeqCst);
        if let Some(socket) = &*self.socket.lock().unwrap() {
            let _ = socket.shutdown(Shutdown::Both);
        }
        if let Some(thread) = self.thread.take() {
            thread.join().unwrap();
        }
        Ok(())
    }
}
fn stall(agent: &Agent, stop: &AtomicBool, phase: u64) {
    agent.phase.store(phase, Ordering::SeqCst);
    while !stop.load(Ordering::SeqCst) {
        std::thread::park_timeout(Duration::from_millis(2));
    }
}
fn archive(name: &str, bytes: &[u8]) -> Vec<u8> {
    let mut builder = tar::Builder::new(Vec::new());
    let mut header = tar::Header::new_gnu();
    header.set_mode(0o644);
    header.set_size(bytes.len() as u64);
    header.set_cksum();
    builder.append_data(&mut header, name, bytes).unwrap();
    builder.into_inner().unwrap()
}
fn output(code: i32, stdout: Vec<u8>, stderr: Vec<u8>) -> std::process::Output {
    std::process::Output {
        status: std::process::ExitStatus::from_raw(code << 8),
        stdout,
        stderr,
    }
}
fn configure(f: &Fixture, fault: Fault, code: u32) -> (Config, Arc<Agent>) {
    let mut c = super::windows_instance::config(f);
    c.windows_socket_pair = true;
    let agent = Arc::new(Agent::default());
    let clock = agent.clone();
    c.windows_clock = Some(Arc::new(move || clock.clock.load(Ordering::SeqCst)));
    let phases = agent.clone();
    c.windows_phase = Some(Arc::new(move |phase| {
        phases.host_phase.store(
            match phase {
                motor_winagent::Phase::Running => 5,
                motor_winagent::Phase::UnitExit => 8,
                _ => 0,
            },
            Ordering::SeqCst,
        );
    }));
    c.systemd_command = Some(Arc::new(|args| {
        Ok(output(
            0,
            if args.first() == Some(&"show") {
                b"LoadState=not-found\nActiveState=inactive\nSubState=dead\n".to_vec()
            } else {
                Vec::new()
            },
            Vec::new(),
        ))
    }));
    c.windows_command = Some(Arc::new(|program, args| {
        if program.ends_with("rustup") {
            return Ok(output(0, b"/toolchain/bin/cargo\n".to_vec(), Vec::new()));
        }
        if program.ends_with("qemu-img") {
            let disk = if args.len() > 5 {
                args.last().unwrap()
            } else {
                &args[3]
            };
            fs::write(disk, b"fake qcow2")?;
        }
        if args.iter().any(|s| s == "windows-prepare") {
            assert!(args.iter().any(|s| s == "--property=User=motor-prep"));
            for property in [
                "--property=Slice=motor-jobs.slice",
                "--property=OOMScoreAdjust=900",
                "--property=OOMPolicy=kill",
            ] {
                assert!(args.iter().any(|s| s == property), "missing {property}");
            }
            assert!(args.iter().any(|s| s == "--pipe"));
            assert!(!args.iter().any(|s| s.contains("PrivateNetwork")));
            let plan = Path::new(args.last().unwrap());
            let prepared = prep::execute_with_fetch(plan, |cmd| {
                let args = cmd
                    .get_args()
                    .map(|s| s.to_str().unwrap())
                    .collect::<Vec<_>>();
                for target in ["x86_64-pc-windows-msvc", "aarch64-pc-windows-msvc"] {
                    assert!(
                        args.windows(2).any(|a| a == ["--target", target]),
                        "prep must fetch crates for {target}: {args:?}"
                    );
                }
                assert!(args.contains(&"--locked"));
                let home = cmd
                    .get_envs()
                    .find(|(k, _)| *k == "CARGO_HOME")
                    .unwrap()
                    .1
                    .unwrap();
                let home = Path::new(home);
                for (name, data) in [
                    ("registry/index/cache/index", b"index".as_slice()),
                    ("registry/cache/crates.io/pkg.crate", b"crate".as_slice()),
                    ("git/db/mirror/HEAD", b"git".as_slice()),
                    ("git/checkouts/mirror/src/lib.rs", b"checkout".as_slice()),
                ] {
                    fs::create_dir_all(home.join(name).parent().unwrap())?;
                    fs::write(home.join(name), data)?;
                }
                Ok(())
            });
            if let Err(error) = prepared {
                return Ok(output(
                    1,
                    Vec::new(),
                    format!("{}\n", crate::windows::prep_error::error_line(&error)).into_bytes(),
                ));
            }
        }
        Ok(output(0, Vec::new(), Vec::new()))
    }));
    let captured = agent.clone();
    c.windows_vm = Some(Arc::new(move |args, nonce, path| {
        captured.units.lock().unwrap().push(args.to_vec());
        let fault = if matches!(
            fault,
            Fault::DismountOnce | Fault::CacheGrantMissingOnce | Fault::CacheInitFailedOnce
        ) {
            if captured.units.lock().unwrap().len() == 1 {
                match fault {
                    Fault::DismountOnce => Fault::Dismount,
                    Fault::CacheInitFailedOnce => Fault::CacheInitFailed,
                    _ => Fault::CacheGrantMissingAlways,
                }
            } else {
                Fault::None
            }
        } else {
            fault
        };
        let done = Arc::new(AtomicBool::new(false));
        let stop = Arc::new(AtomicBool::new(false));
        let socket = Arc::new(Mutex::new(None));
        let (d, s, p, a) = (done.clone(), stop.clone(), socket.clone(), captured.clone());
        let nonce = nonce.to_owned();
        let _ = path;
        let (host_port, mut port) = UnixStream::pair()?;
        let thread = std::thread::spawn(move || {
            if fault == Fault::Hello {
                stall(&a, &s, 1);
                return;
            }
            *p.lock().unwrap() = Some(port.try_clone().unwrap());
            let mut session = Session::new(Peer::Agent, &nonce).unwrap();
            let result = (|| -> io::Result<()> {
                if fault == Fault::Oversized {
                    port.write_all(&((motor_winagent::MAX_FRAME_BYTES + 1) as u32).to_be_bytes())?;
                    stall(&a, &s, 1);
                    return Ok(());
                }
                if fault == Fault::Malformed {
                    port.write_all(&[0, 0, 0, 1, b'!'])?;
                    stall(&a, &s, 1);
                    return Ok(());
                }
                if fault == Fault::Nonce || fault == Fault::Version {
                    motor_winagent::write_frame(
                        &mut port,
                        &AgentFrame::Hello {
                            version: if fault == Fault::Version {
                                motor_winagent::PROTOCOL_VERSION + 1
                            } else {
                                motor_winagent::PROTOCOL_VERSION
                            },
                            nonce: if fault == Fault::Nonce {
                                "wrong".into()
                            } else {
                                nonce.clone()
                            },
                        },
                    )?;
                    stall(&a, &s, 1);
                    return Ok(());
                }
                if fault == Fault::Order {
                    motor_winagent::write_frame(
                        &mut port,
                        &AgentFrame::Output {
                            stream: Stream::Stdout,
                            bytes: vec![1],
                        },
                    )?;
                    stall(&a, &s, 1);
                    return Ok(());
                }
                session.send(
                    &mut port,
                    &AgentFrame::Hello {
                        version: motor_winagent::PROTOCOL_VERSION,
                        nonce,
                    },
                )?;
                if fault == Fault::CacheGrantMissingAlways {
                    session.send(
                        &mut port,
                        &AgentFrame::CacheFailed {
                            failure: motor_winagent::cache_steps::CacheFailure {
                                step: motor_winagent::cache_steps::CacheStep::VolumeAcl,
                                elapsed_ms: 1,
                                message: "VolumeAcl: cache_grant_missing".into(),
                                completed: Vec::new(),
                            },
                        },
                    )?;
                    stall(&a, &s, 2);
                    return Ok(());
                }
                if fault == Fault::CacheInitFailed {
                    session.send(
                        &mut port,
                        &AgentFrame::CacheFailed {
                            failure: motor_winagent::cache_steps::CacheFailure {
                                step: motor_winagent::cache_steps::CacheStep::InitializeRaw,
                                elapsed_ms: 36_494,
                                message: "native storage step timed out".into(),
                                completed: Vec::new(),
                            },
                        },
                    )?;
                    stall(&a, &s, 2);
                    return Ok(());
                }
                session.send(
                    &mut port,
                    &AgentFrame::CacheMounted {
                        timings: motor_winagent::CacheTimings {
                            disk_selection_ms: 23,
                            ..Default::default()
                        },
                    },
                )?;
                let command = loop {
                    match session.receive(&mut port)?.unwrap() {
                        AgentFrame::ReceiveTree { bytes } => {
                            a.tree.lock().unwrap().extend(bytes);
                            if fault == Fault::SlowTransfer {
                                a.clock.fetch_add(5_000, Ordering::SeqCst);
                            }
                        }
                        AgentFrame::TreeEnd { command } => break command,
                        _ => unreachable!(),
                    }
                };
                if fault == Fault::Transfer {
                    stall(&a, &s, 2);
                    return Ok(());
                }
                session.send(&mut port, &AgentFrame::TreeReceived {})?;
                a.phase.store(3, Ordering::SeqCst);
                assert!(matches!(
                    session.receive(&mut port)?,
                    Some(AgentFrame::Start {})
                ));
                if fault == Fault::Start {
                    stall(&a, &s, 4);
                    return Ok(());
                }
                if fault == Fault::RejectStart {
                    motor_winagent::write_frame(&mut port, &AgentFrame::Exit { code: 9 })?;
                    stall(&a, &s, 4);
                    return Ok(());
                }
                session.send(&mut port, &AgentFrame::Started {})?;
                a.starts.fetch_add(1, Ordering::SeqCst);
                if fault == Fault::Silent {
                    stall(&a, &s, 5);
                    return Ok(());
                }
                if fault == Fault::FloodOutput {
                    for sequence in 0..96 {
                        for stream in [Stream::Stdout, Stream::Stderr] {
                            session.send(
                                &mut port,
                                &AgentFrame::Output {
                                    stream,
                                    bytes: vec![sequence; 256],
                                },
                            )?;
                        }
                    }
                } else {
                    session.send(
                        &mut port,
                        &AgentFrame::Output {
                            stream: Stream::Stdout,
                            bytes: b"Windows stdout\n".to_vec(),
                        },
                    )?;
                    session.send(
                        &mut port,
                        &AgentFrame::Output {
                            stream: Stream::Stderr,
                            bytes: b"Windows stderr\n".to_vec(),
                        },
                    )?;
                }
                session.send(&mut port, &AgentFrame::Exit { code })?;
                assert!(matches!(
                    session.receive(&mut port)?,
                    Some(AgentFrame::Collect {})
                ));
                if fault == Fault::Collect {
                    stall(&a, &s, 6);
                    return Ok(());
                }
                if fault == Fault::CollectOversized {
                    for _ in 0..65 {
                        motor_winagent::write_frame(
                            &mut port,
                            &AgentFrame::Collected {
                                bytes: vec![0; 256 * 1024],
                            },
                        )?;
                    }
                    stall(&a, &s, 6);
                    return Ok(());
                }
                let name = format!("m{}/changed.rs", command.workspace);
                let changed = archive(&name, b"guest change");
                session.send(&mut port, &AgentFrame::Collected { bytes: changed })?;
                session.send(
                    &mut port,
                    &AgentFrame::CollectEnd {
                        deleted: vec![format!("m{}/deleted.rs", command.workspace)],
                    },
                )?;
                assert!(matches!(
                    session.receive(&mut port)?,
                    Some(AgentFrame::Dismount {})
                ));
                if fault == Fault::Dismount {
                    stall(&a, &s, 7);
                    return Ok(());
                }
                if fault == Fault::DirtyDismount {
                    motor_winagent::write_frame(
                        &mut port,
                        &AgentFrame::Dismounted { clean: false },
                    )?;
                    stall(&a, &s, 7);
                    return Ok(());
                }
                if matches!(
                    fault,
                    Fault::DismountAtUnitExit
                        | Fault::ClosedDismountAtUnitExit
                        | Fault::ClosedAtUnitExit
                        | Fault::TruncatedDismountAtUnitExit
                ) {
                    let mut bytes = Vec::new();
                    session.send(&mut bytes, &AgentFrame::Dismounted { clean: true })?;
                    if fault == Fault::ClosedAtUnitExit {
                        bytes.clear();
                    } else if fault == Fault::TruncatedDismountAtUnitExit {
                        bytes.pop();
                    }
                    *a.exit_reply.lock().unwrap() = Some(ExitReply {
                        port: port.try_clone()?,
                        bytes,
                        close: fault != Fault::DismountAtUnitExit,
                    });
                    return Ok(());
                }
                session.send(&mut port, &AgentFrame::Dismounted { clean: true })?;
                if fault == Fault::UnitExit {
                    stall(&a, &s, 8);
                    return Ok(());
                }
                d.store(true, Ordering::SeqCst);
                Ok(())
            })();
            if let Err(error) = result {
                assert!(
                    s.load(Ordering::SeqCst)
                        || matches!(
                            error.kind(),
                            io::ErrorKind::BrokenPipe
                                | io::ErrorKind::UnexpectedEof
                                | io::ErrorKind::ConnectionReset
                        ),
                    "fake agent: {error}"
                );
            }
        });
        Ok(Box::new(FakeChild {
            exit_code: if fault == Fault::BadUnitExit { 1 } else { 0 },
            host_port: Some(host_port),
            done,
            stop,
            socket,
            thread: Some(thread),
            agent: captured.clone(),
        }))
    }));
    (c, agent)
}
fn source(f: &Fixture) {
    for repository in [&f.main, &f.sibling] {
        git(repository, &["config", "user.name", "Motor fixture"]);
        git(
            repository,
            &["config", "user.email", "motor-fixture@example.invalid"],
        );
    }
    fs::write(
        f.main.join("Cargo.toml"),
        "[package]\nname='test'\nversion='0.1.0'\nedition='2021'\n",
    )
    .unwrap();
    fs::write(
        f.main.join("Cargo.lock"),
        "version = 3\n[[package]]\nname='test'\nversion='0.1.0'\n",
    )
    .unwrap();
    git(&f.main, &["add", "."]);
    git(&f.main, &["commit", "-m", "crate"]);
}
fn request(f: &Fixture) -> (Run, Vec<Vec<u8>>) {
    let (mut r, bytes) = f.run("cargo test --offline");
    r.platform = Some(Platform::Windows);
    (r, bytes)
}
async fn phase(agent: &Agent, wanted: u64) {
    tokio::time::timeout(Duration::from_secs(5), async {
        while agent.phase.load(Ordering::SeqCst) != wanted {
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();
}
fn job_artifacts(c: &Config, id: Uuid) -> [PathBuf; 3] {
    let vm = c.windows.as_ref().unwrap();
    [
        vm.layout_root
            .join("vm/overlays")
            .join(format!("{id}.qcow2")),
        vm.layout_root.join("vm/sockets").join(id.to_string()),
        vm.layout_root.join("staging").join(id.to_string()),
    ]
}
fn cleaned(c: &Config, id: Uuid, agent: &Agent) {
    for path in job_artifacts(c, id) {
        assert!(!path.exists(), "{} remains", path.display());
    }
    assert_eq!(agent.stops.load(Ordering::SeqCst), 1);
}

/// A setup attempt for a job whose earlier attempt is not cleaned up yet
/// (its staging directory still exists) fails, and must not delete that
/// earlier attempt's overlay, socket directory or staging: its VM may still
/// be using them.
#[test]
fn windows_backend_failed_attempt_keeps_an_earlier_attempt_s_files() {
    let f = Fixture::new();
    let (c, _) = configure(&f, Fault::None, 0);
    let runner = Runner::open_with_backend(c, Box::new(FakeHost)).unwrap();
    let id = Uuid::now_v7();
    let [overlay, sockets, staging] = job_artifacts(runner.config(), id);
    fs::write(&overlay, b"earlier overlay").unwrap();
    fs::create_dir_all(&sockets).unwrap();
    fs::create_dir_all(&staging).unwrap();
    fs::write(staging.join("tree"), b"earlier staging").unwrap();
    let error =
        crate::windows::prepare_and_drop_resources(runner.config(), id, "workspace").unwrap_err();
    assert!(error.to_string().contains("staging"), "{error}");
    for path in [&overlay, &sockets, &staging] {
        assert!(
            path.exists(),
            "{} of the earlier attempt was deleted",
            path.display()
        );
    }
}

#[test]
fn windows_backend_recovery_notice_describes_only_automatic_retry_policy() {
    let f = Fixture::new();
    let (mut c, _) = configure(&f, Fault::None, 0);
    // No background scheduler tick. A tick holds a strong reference to the
    // runner while it runs; if one is running when the fixture is dropped,
    // the fixture's cleanup can still reach the runner and cancels the
    // hand-inserted Running job, whose missing request then panics in
    // finalization. With no tick thread the runner is gone by then.
    c.manual_tick = true;
    let runner = Runner::open_with_backend(c, Box::new(FakeHost)).unwrap();
    let id = Uuid::now_v7();
    let mut job = Job::new(id, None, 0, 0, 4);
    job.state = JobState::Running;
    fs::create_dir_all(runner.record_path(id).parent().unwrap()).unwrap();
    runner.core.inner.lock().unwrap().jobs.insert(id, job);
    runner.recovery_diagnostic(id, None, false).unwrap();
    let text = runner
        .replay(id, 0)
        .unwrap()
        .iter()
        .filter_map(|frame| match frame {
            Frame::Output(output) => Some(String::from_utf8_lossy(&output.bytes.0).into_owned()),
            _ => None,
        })
        .collect::<String>();
    assert!(text.contains("runner recovery:"), "{text}");
    assert!(
        text.contains("outcome_unknown unit=unrecorded unit_alive=false"),
        "{text}"
    );
    assert!(
        text.contains("runner will not retry the job automatically"),
        "{text}"
    );
    assert!(!text.contains("never rerun"), "{text}");
}

#[tokio::test]
async fn windows_backend_without_lockfile_starts_without_preparing_local_registry() {
    registry_job(false).await;
}

#[tokio::test]
async fn windows_backend_with_lockfile_prepares_local_registry_before_start() {
    registry_job(true).await;
}

async fn registry_job(has_lockfile: bool) {
    let f = Fixture::new();
    source(&f);
    if !has_lockfile {
        git(&f.main, &["rm", "Cargo.lock", "Cargo.toml"]);
        git(&f.main, &["commit", "-m", "non-Cargo job"]);
    } else {
        fs::write(
            f.main.join("Cargo.lock"),
            format!("version = 3\n[[package]]\nname='test'\nversion='0.1.0'\n[[package]]\nname='registry-dependency'\nversion='1.0.0'\nsource='registry+https://github.com/rust-lang/crates.io-index'\nchecksum='{}'\n", "a".repeat(64)),
        ).unwrap();
        git(&f.main, &["add", "Cargo.lock"]);
        git(&f.main, &["commit", "-m", "locked registry dependency"]);
    }
    let (mut c, a) = configure(&f, Fault::None, 0);
    let original = c.windows_command.take().unwrap();
    let registry_calls = Arc::new(AtomicU64::new(0));
    let captured = registry_calls.clone();
    // Intercept windows-prepare and run execute_with_registry directly. Keep
    // the other mocked host commands and the guest's Started exchange shared.
    c.windows_command = Some(Arc::new(move |program, args| {
        if args.iter().any(|arg| arg == "windows-prepare") {
            let prepared = prep::execute_with_registry(
                Path::new(args.last().unwrap()),
                |staging, workspace, lockfile, routes| {
                    captured.fetch_add(1, Ordering::SeqCst);
                    assert!(workspace.join("Cargo.lock").is_file());
                    assert!(routes.is_empty());
                    let parsed: toml::Value = toml::from_str(lockfile).unwrap();
                    assert_eq!(parsed["package"][0]["name"].as_str(), Some("test"));
                    assert_eq!(
                        parsed["package"][1]["name"].as_str(),
                        Some("registry-dependency")
                    );
                    assert_eq!(
                        parsed["package"][1]["checksum"].as_str(),
                        Some("a".repeat(64).as_str())
                    );
                    let registry = staging.join("cargo-home/local-registry");
                    fs::create_dir_all(&registry)?;
                    fs::write(registry.join("prepared"), b"registry fixture")
                },
            );
            return Ok(match prepared {
                Ok(()) => output(0, Vec::new(), Vec::new()),
                Err(error) => output(
                    1,
                    Vec::new(),
                    format!("{}\n", crate::windows::prep_error::error_line(&error)).into_bytes(),
                ),
            });
        }
        original(program, args)
    }));
    let runner = Runner::open_with_backend(c, Box::new(FakeHost)).unwrap();
    let (r, bytes) = request(&f);
    f.submit(&runner, &r, &bytes);
    let t = terminal(&runner, r.job_id).await;
    assert_eq!(t.outcome, Outcome::WindowsExit { code: 0 });
    assert!(matches!(
        runner.replay(r.job_id, 0).unwrap()[0],
        Frame::Started { .. }
    ));
    assert_eq!(a.starts.load(Ordering::SeqCst), 1);
    assert_eq!(
        registry_calls.load(Ordering::SeqCst),
        u64::from(has_lockfile)
    );
    let tree = a.tree.lock().unwrap();
    let members: Vec<_> = tar::Archive::new(io::Cursor::new(&*tree))
        .entries()
        .unwrap()
        .map(|entry| entry.unwrap().path().unwrap().into_owned())
        .collect();
    assert_eq!(
        members.contains(&PathBuf::from("cargo-home/local-registry/prepared")),
        has_lockfile
    );
    cleaned(runner.config(), r.job_id, &a);
}

#[tokio::test]
async fn windows_backend_streams_collects_and_preserves_full_width_status_at_wire_seam() {
    for code in [0, 256, 0x7FFF_FFFF, 0xFFFF_FFFF, 0xC000_0005] {
        let f = Fixture::new();
        source(&f);
        let (c, a) = configure(&f, Fault::None, code);
        let runner = Runner::open_with_backend(c, Box::new(FakeHost)).unwrap();
        let (r, bytes) = request(&f);
        f.submit(&runner, &r, &bytes);
        let terminal = terminal(&runner, r.job_id).await;
        assert_eq!(terminal.outcome, Outcome::WindowsExit { code });
        assert_eq!(
            terminal.outcome.ntstatus_failure(),
            code & 0xC000_0000 == 0xC000_0000
        );
        let stored: Job = read_json(&runner.record_path(r.job_id)).unwrap();
        assert_eq!(
            stored.terminal.as_ref().unwrap().outcome,
            Outcome::WindowsExit { code }
        );
        assert_eq!(
            stored.started.as_ref().unwrap().path_mapping,
            Some(PathMapping::new("/", r"C:\m"))
        );
        assert_eq!(
            terminal.workspace_changes.unwrap(),
            ["changed.rs", "deleted.rs"]
        );
        let replay = runner.replay(r.job_id, 0).unwrap();
        assert!(
            matches!(&replay[0], Frame::Started { path_mapping: Some(mapping), .. } if mapping == &PathMapping::new("/", r"C:\m"))
        );
        let mut wire = Vec::new();
        for frame in &replay {
            motor_protocol::write_frame(&mut wire, frame).unwrap();
        }
        let mut wire = &wire[..];
        for frame in &replay {
            assert_eq!(
                motor_protocol::read_frame(&mut wire).unwrap().as_ref(),
                Some(frame)
            );
        }
        assert!(
            matches!(replay.last(), Some(Frame::Terminal(t)) if t.outcome == Outcome::WindowsExit { code })
        );
        assert!(replay
            .iter()
            .any(|f| matches!(f, Frame::Output(output) if output.bytes.0 == b"Windows stdout\n")));
        let stderr = replay
            .iter()
            .filter_map(|frame| match frame {
                Frame::Output(output) if output.stream == OutputStream::Stderr => {
                    Some(output.bytes.0.as_slice())
                }
                _ => None,
            })
            .flatten()
            .copied()
            .collect::<Vec<_>>();
        assert_eq!(stderr, b"Windows stderr\n");
        assert_eq!(
            runner
                .config()
                .windows_runtime
                .cache_timings
                .lock()
                .unwrap()[&r.job_id]
                .disk_selection_ms,
            23
        );
        cleaned(runner.config(), r.job_id, &a);
        assert_eq!(a.starts.load(Ordering::SeqCst), 1);
        assert!(runner
            .config()
            .windows
            .as_ref()
            .unwrap()
            .caches()
            .disk(&r.workspace_key)
            .exists());
        assert!(!Path::new(&r.workspace_key).exists());
    }
}

#[test]
fn windows_backend_reports_workspace_changes_relative_to_checkout_root() {
    let f = Fixture::new();
    let (c, _) = configure(&f, Fault::None, 0);
    let runner = Runner::open_with_backend(c, Box::new(FakeHost)).unwrap();
    // Each key is paired with the collection prefix the guest uses for it. The
    // trailing-slash key must not change the result, and the deeper key must
    // not match its shallower neighbour.
    for (key, member) in [
        ("/Users/u/repo", "m/Users/u/repo"),
        ("/Users/u/repo/", "m/Users/u/repo"),
        ("/srv/work/a/b/c/repo", "m/srv/work/a/b/c/repo"),
    ] {
        let id = Uuid::now_v7();
        let mut builder = tar::Builder::new(Vec::new());
        for name in [
            format!("{member}/a.txt"),
            format!("{member}/sub/b.txt"),
            // A sibling checkout beside the workspace, and one whose name
            // only shares the workspace's text prefix.
            format!("{member}-sibling/c.txt"),
            "m/Users/u/sibling/d.txt".into(),
            "m/srv/work/a/b/c/other/e.txt".into(),
        ] {
            let mut header = tar::Header::new_gnu();
            header.set_mode(0o644);
            header.set_size(1);
            header.set_cksum();
            builder.append_data(&mut header, name, &b"x"[..]).unwrap();
        }
        let bytes = builder.into_inner().unwrap();
        prep::store_collection(
            runner.config(),
            id,
            &bytes,
            &[
                format!("{member}/gone.txt"),
                "m/Users/u/sibling/gone.txt".into(),
            ],
        )
        .unwrap();
        let (mut r, _) = request(&f);
        r.workspace_key = key.into();
        let job = Job::new(id, Some(r), 0, 0, 4);
        assert_eq!(
            WindowsBackend
                .collect_changes(&runner.core.trees, &job)
                .unwrap()
                .unwrap(),
            ["a.txt", "gone.txt", "sub/b.txt"],
            "{key}"
        );
        // The stored collection keeps its guest form for its other readers.
        let stored: Vec<String> = read_json(
            &runner
                .config()
                .state_dir()
                .join("jobs")
                .join(id.to_string())
                .join("windows-changes.json"),
        )
        .unwrap();
        assert!(
            stored.contains(&format!("{member}/sub/b.txt")),
            "{stored:?}"
        );
    }
}

#[tokio::test]
async fn windows_backend_slow_output_storage_preserves_exit_and_ordered_streams() {
    let f = Fixture::new();
    source(&f);
    let (mut c, a) = configure(&f, Fault::FloodOutput, 0xC000_0005);
    let (pause, entered) = super::monitoring::IoPause::new();
    let release = super::monitoring::ReleaseIo(pause.clone());
    c.before_job_write = Some(Arc::new(move |path| {
        if path
            .file_name()
            .unwrap()
            .to_string_lossy()
            .starts_with("chunk-")
        {
            pause.block();
        }
        Ok(())
    }));
    let runner = Runner::open_with_backend(c, Box::new(FakeHost)).unwrap();
    let (r, bytes) = request(&f);
    f.submit(&runner, &r, &bytes);
    entered.recv_timeout(Duration::from_secs(5)).unwrap();
    tokio::time::timeout(Duration::from_secs(5), async {
        while !runner.config().windows_runtime.output_blocked(r.job_id) {
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();
    // A delay while the host writes storage must not count as the guest failing
    // to reply, even when it exceeds the reply timeout.
    a.clock
        .store(10 * motor_winagent::REPLY_TIMEOUT_MS, Ordering::SeqCst);
    drop(release);
    let result = terminal(&runner, r.job_id).await;
    assert_eq!(result.outcome, Outcome::WindowsExit { code: 0xC000_0005 });
    let mut streams = [Vec::new(), Vec::new()];
    let mut cursor = 0;
    loop {
        let page = runner.replay(r.job_id, cursor).unwrap();
        for frame in &page {
            if let Frame::Started { seq, .. } = frame {
                assert_eq!(*seq, cursor);
                cursor += 1;
            }
            if let Frame::Output(output) = frame {
                assert_eq!(output.seq, cursor);
                cursor += 1;
                let index = usize::from(output.stream == OutputStream::Stderr);
                streams[index].extend_from_slice(&output.bytes.0);
            }
        }
        if matches!(page.last(), Some(Frame::Terminal(_))) {
            break;
        }
        assert!(!page.is_empty());
    }
    let expected: Vec<u8> = (0..96).flat_map(|sequence| [sequence; 256]).collect();
    assert_eq!(streams[0], expected);
    assert_eq!(streams[1], expected);
    cleaned(runner.config(), r.job_id, &a);
}

#[tokio::test]
async fn windows_backend_launch_transfers_ownership_without_io_and_wait_keeps_all_status_bits() {
    for code in [0, 0xFFFF_FFFF, 0xC000_0005] {
        let f = Fixture::new();
        source(&f);
        let (c, a) = configure(&f, Fault::None, code);
        let runner = Runner::open_with_backend(c, Box::new(FakeHost)).unwrap();
        let (r, _) = request(&f);
        for (repo, local) in [
            (&r.repository_root, &f.main),
            (&r.siblings[0].canonical_path, &f.sibling),
        ] {
            let mirror = runner.core.trees.ensure_mirror(repo).unwrap();
            git(&mirror, &["fetch", text(local), "HEAD"]);
        }
        let setup = Setup {
            id: r.job_id,
            cpus: vec![12, 13, 14, 15],
            request: r,
            base: false,
            recorded: BTreeMap::new(),
            snapshots: None,
        };
        let prepared = WindowsBackend
            .materialize(runner.config(), &runner.core.trees, &setup)
            .unwrap();
        phase(&a, 3).await;
        assert_eq!(a.starts.load(Ordering::SeqCst), 0);
        let mut launch = WindowsBackend
            .launch(runner.config(), &runner.core.trees, &setup, &prepared, 4)
            .unwrap();
        launch.execution =
            super::windows_instance::direct_execution(runner.config(), setup.id, launch.execution);
        assert!(!launch.durable_output);
        drop(prepared);
        assert_eq!(a.stops.load(Ordering::SeqCst), 0);
        assert_eq!(a.starts.load(Ordering::SeqCst), 0);
        launch.execution.release().unwrap();
        let mut output = launch.execution.output().unwrap();
        let mut stdout = String::new();
        let mut stderr = String::new();
        output.stdout.read_to_string(&mut stdout).unwrap();
        output.stderr.read_to_string(&mut stderr).unwrap();
        let status = launch.execution.wait().unwrap();
        assert!(
            matches!(status, backend::ExecutionOutcome::WindowsExit { code: actual } if actual == code)
        );
        assert_eq!(status.wire(), Outcome::WindowsExit { code });
        assert_eq!(stdout, "Windows stdout\n");
        assert_eq!(stderr, "Windows stderr\n");
        launch.execution.reap();
        launch.execution.reap();
        drop(launch);
        cleaned(runner.config(), setup.id, &a);
        let config = runner.config().clone();
        drop(runner);
        let reopened =
            reopen_with(|| Runner::open_with_backend(config.clone(), Box::new(FakeHost)));
        assert!(
            reopened.core.quarantined.is_empty(),
            "direct backend collections must not become unreadable jobs on restart"
        );
    }
}

#[tokio::test]
async fn windows_backend_drains_final_reply_written_between_read_and_unit_exit() {
    for fault in [Fault::DismountAtUnitExit, Fault::ClosedDismountAtUnitExit] {
        let f = Fixture::new();
        source(&f);
        let code = 0xC000_0005;
        let (c, a) = configure(&f, fault, code);
        let runner = Runner::open_with_backend(c, Box::new(FakeHost)).unwrap();
        let (r, bytes) = request(&f);
        f.submit(&runner, &r, &bytes);
        assert_eq!(
            terminal(&runner, r.job_id).await.outcome,
            Outcome::WindowsExit { code },
            "{fault:?}"
        );
        assert!(a.exit_reply.lock().unwrap().is_none());
        cleaned(runner.config(), r.job_id, &a);
    }
}

#[tokio::test]
async fn windows_backend_missing_dismount_reply_preserves_command_exit_and_discards_cache() {
    for fault in [Fault::ClosedAtUnitExit, Fault::TruncatedDismountAtUnitExit] {
        let f = Fixture::new();
        source(&f);
        let (c, a) = configure(&f, fault, 0);
        let runner = Runner::open_with_backend(c, Box::new(FakeHost)).unwrap();
        let (r, bytes) = request(&f);
        f.submit(&runner, &r, &bytes);
        assert_eq!(
            terminal(&runner, r.job_id).await.outcome,
            Outcome::WindowsExit { code: 0 },
            "{fault:?}"
        );
        assert!(!runner
            .config()
            .windows
            .as_ref()
            .unwrap()
            .caches()
            .disk(&r.workspace_key)
            .exists());
        assert!(a.exit_reply.lock().unwrap().is_none());
        cleaned(runner.config(), r.job_id, &a);
    }
}

#[tokio::test]
async fn windows_backend_started_write_failure_destroys_unreleased_vm_and_dispatches_next() {
    let f = Fixture::new();
    source(&f);
    let (mut c, a) = configure(&f, Fault::None, 0);
    let failed = Arc::new(AtomicBool::new(false));
    let fault = failed.clone();
    c.before_job_write = Some(Arc::new(move |path| {
        if let Ok(job) = read_json::<Job>(path) {
            if job.setup_ms.is_some()
                && job.started_ms.is_none()
                && job.terminal.is_none()
                && !fault.swap(true, Ordering::SeqCst)
            {
                return Err(io::Error::other("injected Started write failure"));
            }
        }
        Ok(())
    }));
    let runner = Runner::open_with_backend(c, Box::new(FakeHost)).unwrap();
    let (first, bytes) = request(&f);
    f.submit(&runner, &first, &bytes);
    let first_terminal = terminal(&runner, first.job_id).await;
    assert!(matches!(
        first_terminal.outcome,
        Outcome::RefusedBeforeStart {
            reason: RefusalReason::WorkspaceSetupFailed
        }
    ));
    assert_eq!(
        first_terminal.refusal_detail(),
        Some("execution gate was not opened: start_not_persisted")
    );
    assert_eq!(first_terminal.retry_after_ms(), None);
    assert_eq!(first_terminal.ran, Some(Ran::None));
    assert!(failed.load(Ordering::SeqCst));
    tokio::time::timeout(Duration::from_secs(5), async {
        while a.stops.load(Ordering::SeqCst) != 1 {
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();
    assert_eq!(a.starts.load(Ordering::SeqCst), 0);
    cleaned(runner.config(), first.job_id, &a);
    assert!(runner
        .replay(first.job_id, 0)
        .unwrap()
        .iter()
        .all(|f| !matches!(f, Frame::Started { .. })));
    let (second, bytes) = request(&f);
    f.submit(&runner, &second, &bytes);
    assert_eq!(
        terminal(&runner, second.job_id).await.outcome,
        Outcome::WindowsExit { code: 0 }
    );
    assert_eq!(a.starts.load(Ordering::SeqCst), 1);
    assert_eq!(a.stops.load(Ordering::SeqCst), 2);
}

#[tokio::test]
async fn windows_backend_missing_exit_is_unknown_but_post_exit_failures_preserve_status() {
    for (fault, stalled) in [
        (Fault::RejectStart, 0),
        (Fault::Start, 4),
        (Fault::Collect, 6),
        (Fault::Dismount, 7),
        (Fault::UnitExit, 8),
        (Fault::CollectOversized, 0),
        (Fault::DirtyDismount, 0),
        (Fault::BadUnitExit, 0),
    ] {
        let f = Fixture::new();
        source(&f);
        let (c, a) = configure(&f, fault, 0);
        let runner = Runner::open_with_backend(c, Box::new(FakeHost)).unwrap();
        let (r, bytes) = request(&f);
        f.submit(&runner, &r, &bytes);
        if stalled != 0 {
            phase(&a, stalled).await;
            if fault == Fault::UnitExit {
                tokio::time::timeout(Duration::from_secs(5), async {
                    while a.host_phase.load(Ordering::SeqCst) != 8 {
                        tokio::task::yield_now().await;
                    }
                })
                .await
                .unwrap();
            }
            a.clock.store(59_999, Ordering::SeqCst);
            assert!(runner.job_status(r.job_id).unwrap().terminal.is_none());
            a.clock.store(60_000, Ordering::SeqCst);
        }
        if fault == Fault::CollectOversized {
            tokio::time::timeout(Duration::from_secs(90), async {
                while runner.job_status(r.job_id).unwrap().terminal.is_none() {
                    tokio::task::yield_now().await;
                }
            })
            .await
            .unwrap();
        }
        let terminal = terminal(&runner, r.job_id).await;
        if matches!(fault, Fault::RejectStart | Fault::Start) {
            assert_execution_ended_wire(&terminal, false);
        } else {
            assert_execution_ended_wire(&terminal, true);
        }
        assert_eq!(
            terminal.outcome,
            if matches!(fault, Fault::RejectStart | Fault::Start) {
                Outcome::OutcomeUnknown
            } else {
                Outcome::WindowsExit { code: 0 }
            },
            "{fault:?}"
        );
        assert_eq!(
            runner.core.inner.lock().unwrap().jobs[&r.job_id]
                .monitoring_error
                .is_some(),
            matches!(fault, Fault::RejectStart | Fault::Start),
            "{fault:?}"
        );
        if matches!(fault, Fault::RejectStart | Fault::Start) {
            let text = runner
                .replay(r.job_id, 0)
                .unwrap()
                .iter()
                .filter_map(|frame| {
                    if let Frame::Output(output) = frame {
                        Some(String::from_utf8_lossy(&output.bytes.0).into_owned())
                    } else {
                        None
                    }
                })
                .collect::<String>();
            assert!(
                text.contains("runner will not retry the job automatically"),
                "{text}"
            );
            assert!(!text.contains("never rerun"), "{text}");
        }
        assert!(!runner
            .config()
            .windows
            .as_ref()
            .unwrap()
            .caches()
            .disk(&r.workspace_key)
            .exists());
        cleaned(runner.config(), r.job_id, &a);
        assert_eq!(Runner::held_weight(&runner.core.inner.lock().unwrap()), 0);
    }
}

#[tokio::test]
async fn windows_backend_active_transfer_can_exceed_one_reply_timeout() {
    let f = Fixture::new();
    source(&f);
    // Advance simulated time faster instead of serializing a multi-megabyte
    // snapshot just to cross the reply deadline under the terminal poll's bound.
    fs::write(f.main.join("large-snapshot"), vec![42; 1024 * 1024]).unwrap();
    git(&f.main, &["add", "."]);
    git(&f.main, &["commit", "-m", "large snapshot"]);
    let (c, a) = configure(&f, Fault::SlowTransfer, 0);
    let runner = Runner::open_with_backend(c, Box::new(FakeHost)).unwrap();
    let (r, bytes) = request(&f);
    f.submit(&runner, &r, &bytes);
    assert_eq!(
        terminal(&runner, r.job_id).await.outcome,
        Outcome::WindowsExit { code: 0 }
    );
    assert!(a.clock.load(Ordering::SeqCst) > motor_winagent::REPLY_TIMEOUT_MS);
    cleaned(runner.config(), r.job_id, &a);
}

#[tokio::test]
async fn windows_backend_failed_dismount_then_immediate_same_workspace_job() {
    let f = Fixture::new();
    source(&f);
    let (c, a) = configure(&f, Fault::DismountOnce, 256);
    let runner = Runner::open_with_backend(c, Box::new(FakeHost)).unwrap();
    let (first, bytes) = request(&f);
    f.submit(&runner, &first, &bytes);
    phase(&a, 7).await;
    a.clock.store(60_000, Ordering::SeqCst);
    assert_eq!(
        terminal(&runner, first.job_id).await.outcome,
        Outcome::WindowsExit { code: 256 }
    );
    cleaned(runner.config(), first.job_id, &a);
    let disk = runner
        .config()
        .windows
        .as_ref()
        .unwrap()
        .caches()
        .disk(&first.workspace_key);
    assert!(!disk.exists());
    let (second, bytes) = request(&f);
    assert_eq!(first.workspace_key, second.workspace_key);
    f.submit(&runner, &second, &bytes);
    assert_eq!(
        terminal(&runner, second.job_id).await.outcome,
        Outcome::WindowsExit { code: 256 }
    );
    assert!(disk.exists());
    assert_eq!(a.starts.load(Ordering::SeqCst), 2);
    for path in job_artifacts(runner.config(), second.job_id) {
        assert!(!path.exists(), "{} remains", path.display());
    }
    assert_eq!(a.stops.load(Ordering::SeqCst), 2);
}

#[tokio::test]
async fn windows_backend_missing_cache_grant_discards_and_retries_once_before_start() {
    let f = Fixture::new();
    source(&f);
    let (c, a) = configure(&f, Fault::CacheGrantMissingOnce, 0);
    let runner = Runner::open_with_backend(c, Box::new(FakeHost)).unwrap();
    let (request, bytes) = request(&f);
    let disk = runner
        .config()
        .windows
        .as_ref()
        .unwrap()
        .caches()
        .disk(&request.workspace_key);
    fs::write(&disk, b"untrusted reused cache").unwrap();
    f.submit(&runner, &request, &bytes);
    assert_eq!(
        terminal(&runner, request.job_id).await.outcome,
        Outcome::WindowsExit { code: 0 }
    );
    assert_eq!(fs::read(&disk).unwrap(), b"fake qcow2");
    assert_eq!(a.units.lock().unwrap().len(), 2);
    assert_eq!(a.starts.load(Ordering::SeqCst), 1);
    assert_eq!(a.stops.load(Ordering::SeqCst), 2);
    for path in job_artifacts(runner.config(), request.job_id) {
        assert!(!path.exists(), "{} remains", path.display());
    }
}

#[tokio::test]
async fn windows_backend_missing_cache_grant_on_retry_refuses_without_third_boot() {
    let f = Fixture::new();
    source(&f);
    let (c, a) = configure(&f, Fault::CacheGrantMissingAlways, 0);
    let runner = Runner::open_with_backend(c, Box::new(FakeHost)).unwrap();
    let (request, bytes) = request(&f);
    f.submit(&runner, &request, &bytes);
    assert!(matches!(
        terminal(&runner, request.job_id).await.outcome,
        Outcome::RefusedBeforeStart { .. }
    ));
    assert_eq!(a.units.lock().unwrap().len(), 2);
    assert_eq!(a.starts.load(Ordering::SeqCst), 0);
    assert_eq!(a.stops.load(Ordering::SeqCst), 2);
    assert!(!runner
        .config()
        .windows
        .as_ref()
        .unwrap()
        .caches()
        .disk(&request.workspace_key)
        .exists());
}

#[tokio::test]
async fn windows_backend_interrupted_cache_initialization_discards_disk_and_next_job_recreates_it()
{
    // A guest can lose its 30 s initialization budget after the disk got a GPT
    // but before the NTFS volume exists. The guest only initializes RAW disks,
    // so such a disk can never be completed later; it must not stay at the
    // workspace's cache path.
    let f = Fixture::new();
    source(&f);
    let (c, a) = configure(&f, Fault::CacheInitFailedOnce, 0);
    let runner = Runner::open_with_backend(c, Box::new(FakeHost)).unwrap();
    let (first, bytes) = request(&f);
    let disk = runner
        .config()
        .windows
        .as_ref()
        .unwrap()
        .caches()
        .disk(&first.workspace_key);
    fs::write(&disk, b"GPT without an NTFS volume").unwrap();
    f.submit(&runner, &first, &bytes);
    assert!(matches!(
        terminal(&runner, first.job_id).await.outcome,
        Outcome::RefusedBeforeStart { .. }
    ));
    assert!(!disk.exists(), "interrupted cache disk was kept");
    assert_eq!(
        a.units.lock().unwrap().len(),
        1,
        "no retry for this failure"
    );
    assert_eq!(a.starts.load(Ordering::SeqCst), 0);
    for path in job_artifacts(runner.config(), first.job_id) {
        assert!(!path.exists(), "{} remains", path.display());
    }

    let (second, bytes) = request(&f);
    assert_eq!(first.workspace_key, second.workspace_key);
    f.submit(&runner, &second, &bytes);
    assert_eq!(
        terminal(&runner, second.job_id).await.outcome,
        Outcome::WindowsExit { code: 0 }
    );
    // The fake qemu-img writes this content only when it creates a new disk.
    assert_eq!(fs::read(&disk).unwrap(), b"fake qcow2");
    assert_eq!(a.units.lock().unwrap().len(), 2);
    assert_eq!(a.starts.load(Ordering::SeqCst), 1);
}

#[tokio::test]
async fn windows_backend_pre_start_bounds_and_untrusted_frames_refuse_and_clean_up() {
    for (fault, stalled, deadline) in [
        (Fault::Hello, 1, 120_000),
        (Fault::Transfer, 2, 60_000),
        (Fault::Oversized, 0, 0),
        (Fault::Malformed, 0, 0),
        (Fault::Nonce, 0, 0),
        (Fault::Version, 0, 0),
        (Fault::Order, 0, 0),
    ] {
        let f = Fixture::new();
        source(&f);
        let (c, a) = configure(&f, fault, 0);
        let runner = Runner::open_with_backend(c, Box::new(FakeHost)).unwrap();
        let (r, bytes) = request(&f);
        f.submit(&runner, &r, &bytes);
        if stalled != 0 {
            phase(&a, stalled).await;
            a.clock.store(deadline, Ordering::SeqCst);
        }
        let t = terminal(&runner, r.job_id).await;
        assert!(
            matches!(t.outcome, Outcome::RefusedBeforeStart { .. }),
            "{fault:?}: {:?}",
            t.outcome
        );
        assert_eq!(t.ran, Some(Ran::None));
        assert_eq!(a.starts.load(Ordering::SeqCst), 0);
        cleaned(runner.config(), r.job_id, &a);
    }
}

#[tokio::test]
async fn windows_backend_silent_commands_have_no_agent_deadline_and_cancel_never_waits_for_agent() {
    for fault in [Fault::Silent, Fault::Start] {
        let f = Fixture::new();
        source(&f);
        let (mut c, a) = configure(&f, fault, 0);
        c.manual_tick = true;
        let (running_tx, running_rx) = std::sync::mpsc::channel();
        let phase_hook = c.windows_phase.take().unwrap();
        c.windows_phase = Some(Arc::new(move |phase| {
            phase_hook(phase);
            if phase == motor_winagent::Phase::Running {
                running_tx.send(()).unwrap();
            }
        }));
        let (pause, entered) = super::monitoring::IoPause::new();
        let clock_pause = pause.clone();
        let clock = a.clone();
        let advanced_checks = AtomicU64::new(0);
        c.windows_clock = Some(Arc::new(move || {
            let now = clock.clock.load(Ordering::SeqCst);
            let blocked = match fault {
                Fault::Silent => {
                    // Park on the second read of 600_000: the previous
                    // Transport::check already accepted that timestamp.
                    now == 600_000 && advanced_checks.fetch_add(1, Ordering::SeqCst) == 1
                }
                Fault::Start => clock.phase.load(Ordering::SeqCst) == 4,
                _ => unreachable!(),
            };
            if blocked {
                clock_pause.block();
            }
            now
        }));
        let runner = Runner::open_with_backend(c, Box::new(FakeHost)).unwrap();
        let release = super::monitoring::ReleaseIo(pause);
        let (r, bytes) = request(&f);
        f.submit(&runner, &r, &bytes);
        runner.tick().unwrap();
        if fault == Fault::Silent {
            running_rx.recv_timeout(Duration::from_secs(15)).unwrap();
            a.clock.store(600_000, Ordering::SeqCst);
        }
        entered
            .recv_timeout(Duration::from_secs(15))
            .expect("agent I/O did not reach the clock rendezvous");
        assert!(runner.job_status(r.job_id).unwrap().terminal.is_none());
        // The clock hook stays blocked until ReleaseIo is dropped below.
        // Receiving cancel's result first proves cancel does not wait for
        // the parked I/O thread to make progress.
        let (cancel_tx, cancel_rx) = std::sync::mpsc::channel();
        let cancelling = runner.clone();
        let id = r.job_id;
        let cancel = std::thread::spawn(move || {
            let _ = cancel_tx.send(cancelling.cancel(id));
        });
        cancel_rx
            .recv_timeout(Duration::from_secs(15))
            .expect("cancel waited for the parked agent I/O thread")
            .unwrap();
        cancel.join().unwrap();
        assert_eq!(a.stops.load(Ordering::SeqCst), 0);
        assert!(runner.job_status(r.job_id).unwrap().terminal.is_none());
        f.clock.fetch_add(2_000, Ordering::SeqCst);
        runner.tick().unwrap();
        drop(release);
        let t = terminal(&runner, r.job_id).await;
        assert_eq!(t.outcome, Outcome::OutcomeUnknown);
        assert_eq!(t.killed, Some(Killed::Cancel));
        cleaned(runner.config(), r.job_id, &a);
    }
}

#[tokio::test]
async fn windows_backend_cancel_during_boot_never_releases_command() {
    let f = Fixture::new();
    source(&f);
    let (c, a) = configure(&f, Fault::Hello, 0);
    let runner = Runner::open_with_backend(c, Box::new(FakeHost)).unwrap();
    let (r, bytes) = request(&f);
    f.submit(&runner, &r, &bytes);
    phase(&a, 1).await;
    runner.cancel(r.job_id).unwrap();
    assert_eq!(
        terminal(&runner, r.job_id).await.outcome,
        Outcome::Cancelled
    );
    tokio::time::timeout(Duration::from_secs(5), async {
        while a.stops.load(Ordering::SeqCst) != 1 {
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();
    // FakeChild represents this job's VM unit and records a stop before joining
    // the fake agent thread. Resources stops that child before deleting the VM
    // files, so the stop counter alone does not prove that file cleanup finished.
    // Wait for the runner to finish setup cleanup before checking the paths.
    tokio::time::timeout(Duration::from_secs(5), async {
        while runner
            .core
            .inner
            .lock()
            .unwrap()
            .starting
            .contains(&r.job_id)
        {
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();
    cleaned(runner.config(), r.job_id, &a);
    assert_eq!(a.starts.load(Ordering::SeqCst), 0);
}

struct ExitedChild(i32);
impl UnitChild for ExitedChild {
    fn try_wait(&mut self) -> io::Result<Option<std::process::ExitStatus>> {
        Ok(Some(std::process::ExitStatus::from_raw(self.0 << 8)))
    }
    fn stop(&mut self) -> io::Result<()> {
        Ok(())
    }
}
#[tokio::test]
async fn windows_backend_slot_busy_comes_from_child_code_75_before_hello_deadline() {
    for code in [75, 74, 1, 0] {
        let f = Fixture::new();
        source(&f);
        let (mut c, a) = configure(&f, Fault::None, 0);
        c.windows_vm = Some(Arc::new(move |_, _, _| Ok(Box::new(ExitedChild(code)))));
        let runner = Runner::open_with_backend(c, Box::new(FakeHost)).unwrap();
        let (r, bytes) = request(&f);
        f.submit(&runner, &r, &bytes);
        let t = terminal(&runner, r.job_id).await;
        // The agent link reads the fixture's fake clock, which nothing advances
        // here, so a terminal outcome while it still reads below the reply
        // deadline proves the exit was noticed without waiting for that
        // deadline. Wall time isn't asserted: under a loaded parallel suite,
        // setup alone can take seconds.
        assert!(a.clock.load(Ordering::SeqCst) < motor_winagent::REPLY_TIMEOUT_MS);
        assert_eq!(
            t.outcome,
            Outcome::RefusedBeforeStart {
                reason: if code == 75 {
                    RefusalReason::RunnerFull
                } else {
                    RefusalReason::WorkspaceSetupFailed
                }
            }
        );
        let detail = t.refusal_detail().unwrap();
        if code == 75 {
            assert_eq!(detail, "windows_slot_busy");
        } else {
            assert!(detail.starts_with("prepare job workspace: "), "{detail}");
            assert!(
                detail.contains("Windows VM unit exited before agent reply"),
                "{detail}"
            );
        }
        assert_eq!(t.ran, Some(Ran::None));
        assert_eq!(t.retry_after_ms(), None);
        assert_eq!(a.clock.load(Ordering::SeqCst), 0);
        assert_eq!(a.starts.load(Ordering::SeqCst), 0);
        let vm = runner.config().windows.as_ref().unwrap();
        assert!(!vm
            .layout_root
            .join("vm/overlays")
            .join(format!("{}.qcow2", r.job_id))
            .exists());
    }
}

#[tokio::test]
async fn windows_backend_qemu_cache_serial_is_a_device_property_not_a_drive_option() {
    let f = Fixture::new();
    source(&f);
    let (c, a) = configure(&f, Fault::None, 0);
    let runner = Runner::open_with_backend(c, Box::new(FakeHost)).unwrap();
    let (r, bytes) = request(&f);
    f.submit(&runner, &r, &bytes);
    assert_eq!(
        terminal(&runner, r.job_id).await.outcome,
        Outcome::WindowsExit { code: 0 }
    );
    let units = a.units.lock().unwrap();
    let args = &units[0];
    let drives = args
        .windows(2)
        .filter(|pair| pair[0] == "-drive")
        .map(|pair| &pair[1])
        .collect::<Vec<_>>();
    assert_eq!(drives.len(), 2, "{args:?}");
    assert!(
        drives.iter().all(|drive| !drive.contains("serial=")),
        "serial is not a block backend option: {args:?}"
    );
    assert!(
        drives
            .iter()
            .any(|drive| drive.ends_with("format=qcow2,if=none,id=motor-os")),
        "OS backend must not create an implicit device: {args:?}"
    );
    assert!(
        drives
            .iter()
            .any(|drive| drive.ends_with("format=qcow2,if=none,id=motor-cache")),
        "{args:?}"
    );
    assert!(
        args.windows(2).any(|pair| pair
            == [
                "-device",
                "virtio-blk-pci,drive=motor-cache,serial=motor-cache"
            ]),
        "{args:?}"
    );
    let devices = args
        .windows(2)
        .filter(|pair| pair[0] == "-device")
        .map(|pair| pair[1].as_str())
        .collect::<Vec<_>>();
    assert_eq!(devices, [
        "virtio-blk-pci,drive=motor-os,bootindex=0",
        "virtio-blk-pci,drive=motor-cache,serial=motor-cache",
        "virtio-serial-pci",
        "virtserialport,chardev=agent,name=org.cortexkit.motor.winagent.0",
    ], "only the OS disk may have a bootindex; disk/serial PCI device order must stay stable: {args:?}");
    cleaned(runner.config(), r.job_id, &a);
}

#[tokio::test]
async fn windows_backend_qemu_unit_has_confined_account_fixed_paths_limits_and_offline_devices() {
    let f = Fixture::new();
    source(&f);
    let (c, a) = configure(&f, Fault::None, 0);
    let runner = Runner::open_with_backend(c, Box::new(FakeHost)).unwrap();
    let (r, bytes) = request(&f);
    f.submit(&runner, &r, &bytes);
    terminal(&runner, r.job_id).await;
    let units = a.units.lock().unwrap();
    let args = &units[0];
    for property in [
        "--wait",
        "--collect",
        "--property=User=motor-qemu",
        "--property=Slice=motor-jobs.slice",
        "--property=OOMScoreAdjust=900",
        "--property=OOMPolicy=kill",
        "--property=SupplementaryGroups=kvm",
        "--property=ProtectSystem=strict",
        "--property=ProtectHome=yes",
        "--property=NoNewPrivileges=yes",
        "--property=PrivateNetwork=yes",
        "--property=AllowedCPUs=12-15",
        "--property=CPUQuota=400%",
        "--property=TasksMax=256",
    ] {
        assert!(args.iter().any(|a| a == property), "missing {property}");
    }
    assert!(args.windows(2).any(|a| a == ["-nic", "none"]));
    assert!(args.windows(2).any(|a| a == ["-sandbox", "on"]));
    assert!(args.iter().any(|a| a == "-nodefaults"));
    assert!(args.windows(2).any(|a| a == ["-E", "75"]));
    let vm = runner.config().windows.as_ref().unwrap();
    let rw = args
        .iter()
        .find(|a| a.starts_with("--property=ReadWritePaths="))
        .unwrap();
    assert_eq!(
        rw,
        &format!(
            "--property=ReadWritePaths={} {} {}",
            vm.layout_root
                .join("vm/overlays")
                .join(format!("{}.qcow2", r.job_id))
                .display(),
            vm.caches().disk(&r.workspace_key).display(),
            vm.layout_root
                .join("vm/sockets")
                .join(r.job_id.to_string())
                .display()
        )
    );
    assert!(!rw.contains("baselines"));
    assert!(!rw.contains("state"));
    assert!(args.iter().any(|a| a.contains("motor.boot_nonce=")));
    assert!(!args.iter().any(|a| a.contains("motor-job-")));
}

#[tokio::test]
async fn windows_backend_tar_contains_only_m_and_selected_cargo_home_members() {
    let f = Fixture::new();
    source(&f);
    fs::write(f.main.join("Cargo.toml"),"[package]\nname='test'\nversion='0.1.0'\n[dependencies]\nsibling={path='../../source/commons'}\n").unwrap();
    fs::write(
        f.sibling.join("Cargo.toml"),
        "[package]\nname='sibling'\nversion='0.1.0'\n",
    )
    .unwrap();
    git(&f.main, &["add", "."]);
    git(&f.main, &["commit", "-m", "sibling dependency"]);
    git(&f.sibling, &["add", "."]);
    git(&f.sibling, &["commit", "-m", "sibling crate"]);
    let (c, a) = configure(&f, Fault::None, 0);
    let runner = Runner::open_with_backend(c, Box::new(FakeHost)).unwrap();
    let (r, bytes) = request(&f);
    f.submit(&runner, &r, &bytes);
    assert_eq!(
        terminal(&runner, r.job_id).await.outcome,
        Outcome::WindowsExit { code: 0 }
    );
    let tree = a.tree.lock().unwrap();
    let names = tar::Archive::new(io::Cursor::new(&*tree))
        .entries()
        .unwrap()
        .map(|e| {
            e.unwrap()
                .path()
                .unwrap()
                .to_string_lossy()
                .trim_end_matches('/')
                .to_string()
        })
        .collect::<BTreeSet<_>>();
    assert!(names.contains(&format!("m{}/Cargo.toml", r.workspace_key)));
    assert!(names.contains(&format!("m{}/Cargo.toml", r.siblings[0].canonical_path)));
    for name in [
        "cargo-home/registry/index/cache/index",
        "cargo-home/registry/cache/crates.io/pkg.crate",
        "cargo-home/git/db/mirror/HEAD",
        "cargo-home/git/checkouts/mirror/src/lib.rs",
    ] {
        assert!(names.contains(name));
    }
    assert!(names.iter().all(|name| name == "m"
        || name.starts_with("m/")
        || name.starts_with("cargo-home/registry/index")
        || name.starts_with("cargo-home/registry/cache")
        || name.starts_with("cargo-home/git/db")
        || name.starts_with("cargo-home/git/checkouts")
        || ["cargo-home/registry", "cargo-home/git", "cargo-home"].contains(&name.as_str())));
    assert!(names
        .iter()
        .all(|name| !name.contains("/.git/") && !name.ends_with(".json")));
}

#[tokio::test]
async fn windows_backend_fetches_tracked_nested_workspaces_and_tolerates_a_failed_nested_fetch() {
    let f = Fixture::new();
    source(&f);
    for name in ["broken", "playground"] {
        fs::create_dir_all(f.main.join(name).join("src")).unwrap();
        fs::write(
            f.main.join(name).join("Cargo.toml"),
            format!("[workspace]\n[package]\nname='{name}'\nversion='0.1.0'\nedition='2021'\n"),
        )
        .unwrap();
        fs::write(f.main.join(name).join("Cargo.lock"), "version = 3\n").unwrap();
        fs::write(f.main.join(name).join("src/lib.rs"), "").unwrap();
    }
    fs::create_dir_all(f.main.join("malformed")).unwrap();
    fs::write(f.main.join("malformed/Cargo.lock"), "version = 3\n").unwrap();
    git(&f.main, &["add", "."]);
    git(&f.main, &["commit", "-m", "nested workspaces"]);
    fs::create_dir_all(f.main.join("untracked")).unwrap();
    fs::write(f.main.join("untracked/Cargo.lock"), "version = 3\n").unwrap();
    let (mut c, a) = configure(&f, Fault::None, 0);
    let fetched = Arc::new(Mutex::new(Vec::new()));
    let observed = fetched.clone();
    let original = c.windows_command.take().unwrap();
    c.windows_command = Some(Arc::new(move |program, args| {
        if args.iter().any(|arg| arg == "windows-prepare") {
            prep::execute_with_fetch(Path::new(args.last().unwrap()), |cmd| {
                let args: Vec<_> = cmd.get_args().collect();
                let manifest = Path::new(
                    args[args
                        .iter()
                        .position(|arg| *arg == "--manifest-path")
                        .unwrap()
                        + 1],
                );
                let name = manifest
                    .parent()
                    .unwrap()
                    .file_name()
                    .unwrap()
                    .to_string_lossy()
                    .into_owned();
                observed.lock().unwrap().push(name.clone());
                if name == "broken" {
                    return Err(io::Error::other("stale optional lockfile"));
                }
                let home = cmd
                    .get_envs()
                    .find(|(key, _)| *key == "CARGO_HOME")
                    .unwrap()
                    .1
                    .unwrap();
                let path = Path::new(home).join(format!("registry/cache/fixture/{name}.crate"));
                fs::create_dir_all(path.parent().unwrap())?;
                fs::write(path, b"fetched dependency")?;
                Ok(())
            })?;
            return Ok(output(0, Vec::new(), Vec::new()));
        }
        original(program, args)
    }));
    let runner = Runner::open_with_backend(c, Box::new(FakeHost)).unwrap();
    let (r, bytes) = request(&f);
    f.submit(&runner, &r, &bytes);
    let t = terminal(&runner, r.job_id).await;
    assert_eq!(
        t.outcome,
        Outcome::WindowsExit { code: 0 },
        "{:?}",
        t.refusal_detail
    );
    assert_eq!(*fetched.lock().unwrap(), ["a", "broken", "playground"]);
    let tree = a.tree.lock().unwrap();
    let names: Vec<_> = tar::Archive::new(io::Cursor::new(&*tree))
        .entries()
        .unwrap()
        .map(|entry| {
            entry
                .unwrap()
                .path()
                .unwrap()
                .to_string_lossy()
                .into_owned()
        })
        .collect();
    assert!(
        names.contains(&"cargo-home/registry/cache/fixture/playground.crate".into()),
        "{names:?}"
    );
    assert!(
        names.contains(&"cargo-home/registry/cache/fixture/a.crate".into()),
        "{names:?}"
    );
}

#[tokio::test]
async fn windows_backend_snapshot_io_refusal_names_operation_entry_and_destination() {
    let f = Fixture::new();
    source(&f);
    fs::create_dir_all(f.main.join("blocked")).unwrap();
    fs::write(f.main.join("blocked/source.txt"), b"snapshot bytes").unwrap();
    git(&f.main, &["add", "."]);
    git(&f.main, &["commit", "-m", "snapshot directory"]);
    let (mut c, a) = configure(&f, Fault::None, 0);
    let original = c.windows_command.take().unwrap();
    c.windows_command = Some(Arc::new(move |program, args| {
        if args.iter().any(|arg| arg == "windows-prepare") {
            let plan = Path::new(args.last().unwrap());
            let value: serde_json::Value = read_json(plan)?;
            let workspace = value["workspace"].as_str().unwrap();
            let blocked = Path::new(value["staging"].as_str().unwrap())
                .join("m")
                .join(workspace.trim_start_matches('/'))
                .join("blocked");
            fs::create_dir_all(blocked.parent().unwrap())?;
            fs::write(&blocked, b"not a directory")?;
            let error = prep::execute_with_registry(plan, |_, _, _, _| {
                panic!("extraction must fail before registry preparation")
            })
            .unwrap_err();
            let detail = error.to_string();
            assert!(detail.contains("create directory"), "{detail}");
            assert!(detail.contains("entry=blocked"), "{detail}");
            assert!(
                detail.contains(value["snapshots"][0]["commit"].as_str().unwrap()),
                "{detail}"
            );
            assert!(detail.contains("/blocked"), "{detail}");
            return Ok(output(
                1,
                Vec::new(),
                format!("{}\n", crate::windows::prep_error::error_line(&error)).into_bytes(),
            ));
        }
        original(program, args)
    }));
    let runner = Runner::open_with_backend(c, Box::new(FakeHost)).unwrap();
    let (r, bytes) = request(&f);
    f.submit(&runner, &r, &bytes);
    let t = terminal(&runner, r.job_id).await;
    assert!(matches!(t.outcome, Outcome::RefusedBeforeStart { .. }));
    let detail = t.refusal_detail.unwrap();
    assert!(
        detail.contains("extract snapshots: create directory"),
        "{detail}"
    );
    assert!(detail.contains("entry=blocked"), "{detail}");
    assert!(detail.contains("/blocked"), "{detail}");
    assert_eq!(a.starts.load(Ordering::SeqCst), 0);
}

#[tokio::test]
async fn windows_backend_prep_step_failure_reaches_refusal_without_host_paths() {
    let f = Fixture::new();
    source(&f);
    let (mut c, a) = configure(&f, Fault::None, 0);
    let original = c.windows_command.take().unwrap();
    c.windows_command = Some(Arc::new(move |program, args| {
        if args.iter().any(|arg| arg == "windows-prepare") {
            assert!(args.iter().any(|arg| arg == "--pipe"));
            let error = prep::execute_with_fetch(Path::new(args.last().unwrap()), |cmd| {
                let home = cmd
                    .get_envs()
                    .find(|(key, _)| *key == "CARGO_HOME")
                    .unwrap()
                    .1
                    .unwrap();
                let checkout = Path::new(home).join("git/checkouts/dependency/rev");
                fs::create_dir_all(&checkout)?;
                let path =
                    std::ffi::CString::new(checkout.join("fifo").as_os_str().as_encoded_bytes())
                        .unwrap();
                assert_eq!(unsafe { libc::mkfifo(path.as_ptr(), 0o600) }, 0);
                Ok(())
            })
            .unwrap_err();
            return Ok(output(
                1,
                Vec::new(),
                format!(
                    "Running as unit: motor-prep.service\n{}\nFinished with result: exit-code\n",
                    crate::windows::prep_error::error_line(&error)
                )
                .into_bytes(),
            ));
        }
        original(program, args)
    }));
    let runner = Runner::open_with_backend(c, Box::new(FakeHost)).unwrap();
    let (r, bytes) = request(&f);
    f.submit(&runner, &r, &bytes);
    let t = terminal(&runner, r.job_id).await;
    assert!(matches!(t.outcome, Outcome::RefusedBeforeStart { .. }));
    let detail = t.refusal_detail.unwrap();
    assert!(
        detail.contains("windows prep: stage dependencies: special file: cargo-home/git/checkouts/dependency/rev/fifo"),
        "{detail}"
    );
    assert!(detail.contains("exit status: 1"), "{detail}");
    assert!(
        !detail.contains(f.scratch.path().to_str().unwrap()),
        "{detail}"
    );
    assert_eq!(a.starts.load(Ordering::SeqCst), 0);
    assert!(a.units.lock().unwrap().is_empty());
}

#[tokio::test]
async fn windows_backend_every_invalid_path_refuses_before_start_and_a_symlink_never_follows() {
    for name in [
        "CON",
        "nul.txt",
        "COM1.rs",
        "lpt9.x",
        "trailing.",
        "trailing ",
        "ads:stream",
        "back\\slash",
        "control\u{1}",
        "bad<",
        "bad>",
        "bad\"",
        "bad|",
        "bad?",
        "bad*",
    ] {
        let f = Fixture::new();
        source(&f);
        fs::write(f.main.join(name), b"invalid").unwrap();
        git(&f.main, &["add", "."]);
        git(&f.main, &["commit", "-m", "invalid path"]);
        let (c, a) = configure(&f, Fault::None, 0);
        let runner = Runner::open_with_backend(c, Box::new(FakeHost)).unwrap();
        let (r, bytes) = request(&f);
        f.submit(&runner, &r, &bytes);
        let t = terminal(&runner, r.job_id).await;
        assert!(
            matches!(t.outcome, Outcome::RefusedBeforeStart { .. }),
            "{name}"
        );
        let detail = t.refusal_detail.unwrap();
        let quoted = format!("{name:?}");
        assert!(
            detail.contains(&quoted[1..quoted.len() - 1]),
            "{name}: {detail}"
        );
        assert_eq!(a.starts.load(Ordering::SeqCst), 0);
        assert!(a.units.lock().unwrap().is_empty());
    }
    // A symlink pointing outside the repository is staged as a file holding
    // the target's path text; the target itself is never read.
    let f = Fixture::new();
    source(&f);
    std::os::unix::fs::symlink("/etc/passwd", f.main.join("link")).unwrap();
    git(&f.main, &["add", "."]);
    git(&f.main, &["commit", "-m", "link"]);
    let (c, a) = configure(&f, Fault::None, 0);
    let runner = Runner::open_with_backend(c, Box::new(FakeHost)).unwrap();
    let (r, bytes) = request(&f);
    f.submit(&runner, &r, &bytes);
    let t = terminal(&runner, r.job_id).await;
    assert_eq!(
        t.outcome,
        Outcome::WindowsExit { code: 0 },
        "{:?}",
        t.refusal_detail
    );
    let tree = a.tree.lock().unwrap();
    let mut staged = None;
    for entry in tar::Archive::new(io::Cursor::new(&*tree))
        .entries()
        .unwrap()
    {
        let mut entry = entry.unwrap();
        if entry.path().unwrap().to_string_lossy().ends_with("/link") {
            assert!(entry.header().entry_type().is_file());
            let mut text = String::new();
            entry.read_to_string(&mut text).unwrap();
            staged = Some(text);
        }
    }
    assert_eq!(staged.as_deref(), Some("/etc/passwd"));
}

#[tokio::test]
async fn windows_backend_rejects_unsupported_manifests_but_accepts_cargo_config() {
    for (file,body,offender) in [("Cargo.toml","[dependencies]\noutside={path='../../../../outside'}\n","outside"),("Cargo.toml","[dependencies]\nremote={git='ssh://private/repo'}\n","ssh://private/repo"),("Cargo.lock","version=3\n[[package]]\nname='bad'\nversion='0.1.0'\nsource='registry+https://other.example/index'\n","https://other.example/index")] {
        let f = Fixture::new(); source(&f);
        fs::write(f.main.join(file),body).unwrap();
        git(&f.main,&["add","."]); git(&f.main,&["commit","-m","refusal"]);
        let (c,a) = configure(&f,Fault::None,0); let runner = Runner::open_with_backend(c,Box::new(FakeHost)).unwrap();
        let (r,bytes) = request(&f); f.submit(&runner,&r,&bytes); let t = terminal(&runner,r.job_id).await;
        assert!(matches!(t.outcome,Outcome::RefusedBeforeStart { .. })); assert!(t.refusal_detail.unwrap().contains(offender));
        assert_eq!(a.starts.load(Ordering::SeqCst),0); assert!(a.units.lock().unwrap().is_empty());
    }

    let f = Fixture::new();
    source(&f);
    fs::create_dir_all(f.main.join(".cargo")).unwrap();
    fs::write(
        f.main.join(".cargo/config.toml"),
        "[build]\ntarget-dir='sentinel-target'\n[source.crates-io]\nreplace-with='evil'\n[source.evil]\nregistry='sparse+https://example.invalid/'\n",
    )
    .unwrap();
    git(&f.main, &["add", "."]);
    git(&f.main, &["commit", "-m", "repository cargo config"]);
    let (c, a) = configure(&f, Fault::None, 0);
    let runner = Runner::open_with_backend(c, Box::new(FakeHost)).unwrap();
    let (r, bytes) = request(&f);
    f.submit(&runner, &r, &bytes);
    let t = terminal(&runner, r.job_id).await;
    assert_eq!(
        t.outcome,
        Outcome::WindowsExit { code: 0 },
        "{:?}",
        t.refusal_detail
    );
    cleaned(runner.config(), r.job_id, &a);
    assert_eq!(a.starts.load(Ordering::SeqCst), 1);
    assert_eq!(a.units.lock().unwrap().len(), 1);
}

#[tokio::test]
async fn windows_backend_case_collisions_are_detected_before_case_insensitive_extraction() {
    let f = Fixture::new();
    source(&f);
    let oid = git(&f.main, &["hash-object", "-w", "Cargo.toml"]);
    let mut child = Command::new("git")
        .current_dir(&f.main)
        .args(["update-index", "--index-info"])
        .stdin(std::process::Stdio::piped())
        .spawn()
        .unwrap();
    child
        .stdin
        .take()
        .unwrap()
        .write_all(format!("100644 {oid}\tA.rs\n100644 {oid}\ta.rs\n").as_bytes())
        .unwrap();
    assert!(child.wait().unwrap().success());
    let tree = git(&f.main, &["write-tree"]);
    let parent = git(&f.main, &["rev-parse", "HEAD"]);
    let commit = git(
        &f.main,
        &["commit-tree", &tree, "-p", &parent, "-m", "case collision"],
    );
    git(&f.main, &["update-ref", "HEAD", &commit]);
    let (c, a) = configure(&f, Fault::None, 0);
    let runner = Runner::open_with_backend(c, Box::new(FakeHost)).unwrap();
    let (r, bytes) = request(&f);
    f.submit(&runner, &r, &bytes);
    let t = terminal(&runner, r.job_id).await;
    assert!(matches!(t.outcome, Outcome::RefusedBeforeStart { .. }));
    let detail = t.refusal_detail.unwrap();
    assert!(
        detail.contains("case collision") && detail.contains("A.rs") && detail.contains("a.rs"),
        "unexpected refusal detail: {detail}"
    );
    assert_eq!(a.starts.load(Ordering::SeqCst), 0);
}

#[tokio::test]
async fn windows_backend_stages_a_tracked_symlink_as_a_file_holding_its_target() {
    let f = Fixture::new();
    source(&f);
    // Commit `link.txt` as a symlink (mode 120000) pointing at `Cargo.toml`.
    let mut child = Command::new("git")
        .current_dir(&f.main)
        .args(["hash-object", "-w", "--stdin"])
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .spawn()
        .unwrap();
    child
        .stdin
        .take()
        .unwrap()
        .write_all(b"Cargo.toml")
        .unwrap();
    let out = child.wait_with_output().unwrap();
    assert!(out.status.success());
    let target = String::from_utf8(out.stdout).unwrap().trim().to_owned();
    let mut child = Command::new("git")
        .current_dir(&f.main)
        .args(["update-index", "--index-info"])
        .stdin(std::process::Stdio::piped())
        .spawn()
        .unwrap();
    child
        .stdin
        .take()
        .unwrap()
        .write_all(format!("120000 {target}\tlink.txt\n").as_bytes())
        .unwrap();
    assert!(child.wait().unwrap().success());
    let tree = git(&f.main, &["write-tree"]);
    let parent = git(&f.main, &["rev-parse", "HEAD"]);
    let commit = git(
        &f.main,
        &["commit-tree", &tree, "-p", &parent, "-m", "tracked symlink"],
    );
    git(&f.main, &["update-ref", "HEAD", &commit]);
    let (c, a) = configure(&f, Fault::None, 0);
    let runner = Runner::open_with_backend(c, Box::new(FakeHost)).unwrap();
    let (r, bytes) = request(&f);
    f.submit(&runner, &r, &bytes);
    let t = terminal(&runner, r.job_id).await;
    assert_eq!(
        t.outcome,
        Outcome::WindowsExit { code: 0 },
        "{:?}",
        t.refusal_detail
    );
    let tree = a.tree.lock().unwrap();
    let mut found = false;
    for entry in tar::Archive::new(io::Cursor::new(&*tree))
        .entries()
        .unwrap()
    {
        let mut entry = entry.unwrap();
        if entry
            .path()
            .unwrap()
            .to_string_lossy()
            .ends_with("/link.txt")
        {
            assert!(entry.header().entry_type().is_file());
            let mut text = String::new();
            entry.read_to_string(&mut text).unwrap();
            assert_eq!(text, "Cargo.toml");
            found = true;
        }
    }
    assert!(found, "link.txt missing from the transferred tree");
}

#[tokio::test]
async fn windows_backend_unreleased_prepared_and_execution_drop_each_clean_once() {
    for transfer in [false, true] {
        let f = Fixture::new();
        source(&f);
        let (c, a) = configure(&f, Fault::None, 0);
        let runner = Runner::open_with_backend(c, Box::new(FakeHost)).unwrap();
        let (r, _) = request(&f);
        for (repo, local) in [
            (&r.repository_root, &f.main),
            (&r.siblings[0].canonical_path, &f.sibling),
        ] {
            let mirror = runner.core.trees.ensure_mirror(repo).unwrap();
            git(&mirror, &["fetch", text(local), "HEAD"]);
        }
        let setup = Setup {
            id: r.job_id,
            request: r,
            cpus: vec![12, 13, 14, 15],
            base: false,
            recorded: BTreeMap::new(),
            snapshots: None,
        };
        let prepared = WindowsBackend
            .materialize(runner.config(), &runner.core.trees, &setup)
            .unwrap();
        if transfer {
            let launch = WindowsBackend
                .launch(runner.config(), &runner.core.trees, &setup, &prepared, 4)
                .unwrap();
            drop(prepared);
            assert_eq!(a.stops.load(Ordering::SeqCst), 0);
            drop(launch);
        } else {
            drop(prepared);
        }
        let vm = runner.config().windows.as_ref().unwrap();
        tokio::time::timeout(Duration::from_secs(5), async {
            while vm
                .layout_root
                .join("staging")
                .join(setup.id.to_string())
                .exists()
            {
                tokio::task::yield_now().await;
            }
        })
        .await
        .unwrap();
        cleaned(runner.config(), setup.id, &a);
        assert_eq!(a.starts.load(Ordering::SeqCst), 0);
    }
}

#[tokio::test]
async fn windows_backend_failed_refusal_persistence_retries_at_maintenance_without_holding_slot() {
    let f = Fixture::new();
    source(&f);
    let (mut c, a) = configure(&f, Fault::None, 0);
    let writes_restored = Arc::new(AtomicBool::new(false));
    let restored = writes_restored.clone();
    c.before_job_write = Some(Arc::new(move |path| {
        if !restored.load(Ordering::SeqCst)
            && read_json::<Job>(path)
                .is_ok_and(|job| job.setup_ms.is_some() && job.started_ms.is_none())
        {
            return Err(io::Error::other("injected storage outage"));
        }
        Ok(())
    }));
    let runner = Runner::open_with_backend(c, Box::new(FakeHost)).unwrap();
    let (r, bytes) = request(&f);
    f.submit(&runner, &r, &bytes);
    let t = terminal(&runner, r.job_id).await;
    assert!(matches!(
        t.outcome,
        Outcome::RefusedBeforeStart {
            reason: RefusalReason::WorkspaceSetupFailed
        }
    ));
    assert_eq!(
        t.refusal_detail(),
        Some("execution gate was not opened: start_not_persisted")
    );
    assert_eq!(t.ran, Some(Ran::None));
    assert_eq!(t.retry_after_ms(), None);
    cleaned(runner.config(), r.job_id, &a);
    assert!(!runner
        .core
        .inner
        .lock()
        .unwrap()
        .pending_start_refusals
        .is_empty());
    assert_eq!(Runner::held_weight(&runner.core.inner.lock().unwrap()), 0);
    writes_restored.store(true, Ordering::SeqCst);
    runner.maintenance().unwrap();
    assert!(runner
        .core
        .inner
        .lock()
        .unwrap()
        .pending_start_refusals
        .is_empty());
    let durable: Job = read_json(&runner.record_path(r.job_id)).unwrap();
    assert!(matches!(
        durable.terminal.unwrap().outcome,
        Outcome::RefusedBeforeStart {
            reason: RefusalReason::WorkspaceSetupFailed
        }
    ));
    assert_eq!(a.starts.load(Ordering::SeqCst), 0);
}

#[tokio::test]
async fn windows_backend_private_git_routes_verified_mirror_with_or_without_bundle() {
    private_git_routes(false).await;
}

#[tokio::test]
async fn windows_backend_reuses_git_dependency_shared_by_root_and_nested_lockfiles() {
    private_git_routes(true).await;
}

async fn private_git_routes(nested: bool) {
    let f = Fixture::new();
    source(&f);
    let url = "https://unroutable.invalid/private.git";
    let commit = git(&f.sibling, &["rev-parse", "HEAD"]);
    let reference = format!("refs/ck-motor/dep/{commit}");
    git(&f.sibling, &["update-ref", &reference, &commit]);
    let bundle = f.scratch.join("dep.bundle");
    git(&f.sibling, &["bundle", "create", text(&bundle), &reference]);
    let dependency_bytes = fs::read(bundle).unwrap();
    fs::write(f.main.join("Cargo.toml"),format!("[package]\nname='test'\nversion='0.1.0'\n[dependencies]\nprivate={{git='{url}',rev='{commit}'}}\n")).unwrap();
    fs::write(f.main.join("Cargo.lock"),format!("version=3\n[[package]]\nname='private'\nversion='0.1.0'\nsource='git+{url}?rev={commit}#{commit}'\n")).unwrap();
    if nested {
        for name in ["nested", "second-nested"] {
            let directory = f.main.join("tools").join(name);
            fs::create_dir_all(&directory).unwrap();
            fs::write(directory.join("Cargo.toml"), format!("[package]\nname='{name}'\nversion='0.1.0'\n[dependencies]\nprivate={{git='{url}',rev='{commit}'}}\n")).unwrap();
            fs::copy(f.main.join("Cargo.lock"), directory.join("Cargo.lock")).unwrap();
        }
    }
    git(&f.main, &["add", "."]);
    git(&f.main, &["commit", "-m", "private dependency"]);
    let (mut c, a) = configure(&f, Fault::None, 0);
    let fetches = Arc::new(AtomicU64::new(0));
    let captured = fetches.clone();
    let original = c.windows_command.take().unwrap();
    c.windows_command = Some(Arc::new(move |program, args| {
        if args.iter().any(|a| a == "windows-prepare") {
            prep::execute_with_fetch(Path::new(args.last().unwrap()), |cmd| {
                captured.fetch_add(1, Ordering::SeqCst);
                let env = cmd
                    .get_envs()
                    .filter_map(|(k, v)| {
                        v.map(|v| {
                            (
                                k.to_string_lossy().to_string(),
                                v.to_string_lossy().to_string(),
                            )
                        })
                    })
                    .collect::<BTreeMap<_, _>>();
                assert_eq!(env["GIT_CONFIG_COUNT"], "2");
                assert!(env["GIT_CONFIG_KEY_0"].starts_with("url.file://"));
                assert!(env["GIT_CONFIG_KEY_0"].ends_with(".insteadOf"));
                assert_eq!(env["GIT_CONFIG_VALUE_0"], url);
                assert_eq!(env["GIT_CONFIG_KEY_1"], "safe.directory");
                assert!(env["GIT_CONFIG_VALUE_1"].contains("/git-deps/"));
                fs::create_dir_all(Path::new(&env["CARGO_HOME"]).join("git/checkouts/private"))?;
                fs::write(
                    Path::new(&env["CARGO_HOME"]).join("git/checkouts/private/source"),
                    b"verified mirror source",
                )?;
                Ok(())
            })?;
            return Ok(output(0, Vec::new(), Vec::new()));
        }
        original(program, args)
    }));
    let runner = Runner::open_with_backend(c, Box::new(FakeHost)).unwrap();
    for with_bundle in [true, false] {
        let (mut r, bytes) = request(&f);
        r.git_dependencies = vec![GitDependency {
            url: url.into(),
            commits: vec![commit.clone()],
            bundle: with_bundle.then(|| BundleManifest {
                index: 2,
                repository: url.into(),
                length: dependency_bytes.len() as u64,
                sha256: format!("{:x}", Sha256::digest(&dependency_bytes)),
            }),
        }];
        if nested {
            r.git_dependencies.push(GitDependency {
                url: url.into(),
                commits: vec![commit.clone()],
                bundle: None,
            });
        }
        assert!(matches!(
            runner.accept(r.clone()).unwrap(),
            Admission::Accepted(_)
        ));
        let mut manifests = r.bundles.clone();
        if let Some(bundle) = r.git_dependencies[0].bundle.clone() {
            manifests.push(bundle);
        }
        let mut upload = Upload::new(runner.spool_dir(r.job_id), &manifests).unwrap();
        for (index, data) in bytes.iter().enumerate() {
            upload
                .chunk(Frame::BundleChunk {
                    index: index as u32,
                    offset: 0,
                    data: BytePayload(data.clone()),
                })
                .unwrap();
        }
        if with_bundle {
            upload
                .chunk(Frame::BundleChunk {
                    index: 2,
                    offset: 0,
                    data: BytePayload(dependency_bytes.clone()),
                })
                .unwrap();
        }
        runner.fetch_run(&r, &upload.directory, true).unwrap();
        assert_eq!(
            terminal(&runner, r.job_id).await.outcome,
            Outcome::WindowsExit { code: 0 }
        );
    }
    assert_eq!(a.starts.load(Ordering::SeqCst), 2);
    assert_eq!(fetches.load(Ordering::SeqCst), if nested { 6 } else { 2 });
    assert!(tar::Archive::new(io::Cursor::new(&*a.tree.lock().unwrap()))
        .entries()
        .unwrap()
        .any(|e| e
            .unwrap()
            .path()
            .unwrap()
            .to_string_lossy()
            .contains("cargo-home/git/checkouts/private/source")));
}

#[tokio::test]
async fn windows_backend_withdrawal_during_boot_destroys_prepared_vm() {
    let f = Fixture::new();
    source(&f);
    let (c, a) = configure(&f, Fault::Hello, 0);
    let runner = Runner::open_with_backend(c, Box::new(FakeHost)).unwrap();
    let (r, bytes) = request(&f);
    f.submit(&runner, &r, &bytes);
    phase(&a, 1).await;
    f.clock
        .fetch_add(r.queue_wait_limit_s * 1000 + 1, Ordering::SeqCst);
    assert!(matches!(
        runner.withdraw(r.job_id).unwrap(),
        WithdrawOutcome::Withdrawn
    ));
    let t = terminal(&runner, r.job_id).await;
    assert!(matches!(t.outcome, Outcome::RefusedBeforeStart { .. }));
    assert_eq!(t.ran, Some(Ran::None));
    tokio::time::timeout(Duration::from_secs(5), async {
        while runner
            .core
            .inner
            .lock()
            .unwrap()
            .starting
            .contains(&r.job_id)
        {
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();
    cleaned(runner.config(), r.job_id, &a);
    assert_eq!(a.starts.load(Ordering::SeqCst), 0);
}

#[tokio::test]
async fn windows_backend_cleanup_failure_discards_cache_after_known_exit() {
    struct FailedCleanup(Box<dyn UnitChild>);
    impl UnitChild for FailedCleanup {
        fn agent_stream(&mut self) -> Option<UnixStream> {
            self.0.agent_stream()
        }
        fn try_wait(&mut self) -> io::Result<Option<std::process::ExitStatus>> {
            self.0.try_wait()
        }
        fn stop(&mut self) -> io::Result<()> {
            self.0.stop()?;
            Err(io::Error::other("injected cleanup failure"))
        }
    }
    let f = Fixture::new();
    source(&f);
    let (mut config, agent) = configure(&f, Fault::None, 0);
    let spawn = config.windows_vm.take().unwrap();
    config.windows_vm = Some(Arc::new(move |args, nonce, path| {
        Ok(Box::new(FailedCleanup(spawn(args, nonce, path)?)))
    }));
    let runner = Runner::open_with_backend(config, Box::new(FakeHost)).unwrap();
    let (request, bytes) = request(&f);
    f.submit(&runner, &request, &bytes);
    assert_eq!(
        terminal(&runner, request.job_id).await.outcome,
        Outcome::WindowsExit { code: 0 }
    );
    assert_eq!(agent.stops.load(Ordering::SeqCst), 1);
    let disk = runner
        .config()
        .windows
        .as_ref()
        .unwrap()
        .caches()
        .disk(&request.workspace_key);
    assert!(!disk.exists());
    runner.maintenance().unwrap();
    cleaned(runner.config(), request.job_id, &agent);
    assert_eq!(unit_alive(&runner, request.job_id), Some(false));
}

fn unit_alive(runner: &Runner, id: Uuid) -> Option<bool> {
    runner
        .unit_alive(Some(platform::ProcessIdentity {
            pid: 0,
            start: 0,
            unit: Some(id),
        }))
        .unwrap()
}

struct StopFailures {
    remaining: AtomicU64,
    attempts: AtomicU64,
    alive: AtomicBool,
}
struct FailingStop {
    child: Box<dyn UnitChild>,
    failures: Arc<StopFailures>,
}
impl UnitChild for FailingStop {
    fn agent_stream(&mut self) -> Option<UnixStream> {
        self.child.agent_stream()
    }
    fn try_wait(&mut self) -> io::Result<Option<std::process::ExitStatus>> {
        // The guest protocol can finish while the QEMU unit is still alive.
        if self.failures.alive.load(Ordering::SeqCst) {
            Ok(None)
        } else {
            self.child.try_wait()
        }
    }
    fn stop(&mut self) -> io::Result<()> {
        self.failures.attempts.fetch_add(1, Ordering::SeqCst);
        if self.failures.remaining.load(Ordering::SeqCst) > 0 {
            self.failures.remaining.fetch_sub(1, Ordering::SeqCst);
            return Err(io::Error::other("injected live unit stop failure"));
        }
        self.child.stop()?;
        self.failures.alive.store(false, Ordering::SeqCst);
        Ok(())
    }
}
fn fail_stops(config: &mut Config, remaining: u64) -> Arc<StopFailures> {
    let failures = Arc::new(StopFailures {
        remaining: AtomicU64::new(remaining),
        attempts: AtomicU64::new(0),
        alive: AtomicBool::new(false),
    });
    let spawn = config.windows_vm.take().unwrap();
    let first = AtomicBool::new(true);
    let captured = failures.clone();
    config.windows_vm = Some(Arc::new(move |args, nonce, path| {
        let child = spawn(args, nonce, path)?;
        if first.swap(false, Ordering::SeqCst) {
            captured.alive.store(true, Ordering::SeqCst);
            Ok(Box::new(FailingStop {
                child,
                failures: captured.clone(),
            }))
        } else {
            Ok(child)
        }
    }));
    let captured = failures.clone();
    config.systemd_command = Some(Arc::new(move |args| {
        Ok(output(
            0,
            if args.first() == Some(&"show") {
                if captured.alive.load(Ordering::SeqCst) {
                    b"LoadState=loaded\nActiveState=active\nSubState=running\n".to_vec()
                } else {
                    b"LoadState=not-found\nActiveState=inactive\nSubState=dead\n".to_vec()
                }
            } else {
                Vec::new()
            },
            Vec::new(),
        ))
    }));
    failures
}

#[tokio::test]
async fn windows_backend_stop_failure_once_is_retried_at_maintenance() {
    let f = Fixture::new();
    source(&f);
    let (mut config, agent) = configure(&f, Fault::RejectStart, 0);
    let failures = fail_stops(&mut config, 1);
    let runner = Runner::open_with_backend(config, Box::new(FakeHost)).unwrap();
    let (request, bytes) = request(&f);
    f.submit(&runner, &request, &bytes);
    assert_eq!(
        terminal(&runner, request.job_id).await.outcome,
        Outcome::OutcomeUnknown
    );
    assert_eq!(failures.attempts.load(Ordering::SeqCst), 1);
    assert_eq!(unit_alive(&runner, request.job_id), Some(true));
    for path in job_artifacts(runner.config(), request.job_id) {
        assert!(path.exists(), "live unit lost {}", path.display());
    }
    runner.maintenance().unwrap();
    assert_eq!(failures.attempts.load(Ordering::SeqCst), 2);
    assert!(!failures.alive.load(Ordering::SeqCst));
    assert_eq!(unit_alive(&runner, request.job_id), Some(false));
    cleaned(runner.config(), request.job_id, &agent);
    let caches = runner.config().windows.as_ref().unwrap().caches();
    let disk = caches.disk(&request.workspace_key);
    assert!(!disk.exists());
    assert!(!caches.marker(&disk).exists());
    runner.maintenance().unwrap();
    assert_eq!(failures.attempts.load(Ordering::SeqCst), 2);
}

#[tokio::test]
async fn windows_backend_repeated_stop_failure_preserves_live_unit_files() {
    let f = Fixture::new();
    source(&f);
    let (mut config, agent) = configure(&f, Fault::RejectStart, 0);
    let failures = fail_stops(&mut config, u64::MAX);
    let runner = Runner::open_with_backend(config, Box::new(FakeHost)).unwrap();
    let (first, bytes) = request(&f);
    f.submit(&runner, &first, &bytes);
    assert_eq!(
        terminal(&runner, first.job_id).await.outcome,
        Outcome::OutcomeUnknown
    );
    for expected in 2..=3 {
        runner.maintenance().unwrap();
        assert_eq!(failures.attempts.load(Ordering::SeqCst), expected);
        assert_eq!(unit_alive(&runner, first.job_id), Some(true));
        assert!(failures.alive.load(Ordering::SeqCst));
        assert_eq!(agent.stops.load(Ordering::SeqCst), 0);
        for path in job_artifacts(runner.config(), first.job_id) {
            assert!(path.exists(), "live unit lost {}", path.display());
        }
        let caches = runner.config().windows.as_ref().unwrap().caches();
        let disk = caches.disk(&first.workspace_key);
        assert!(disk.exists());
        assert!(caches.marker(&disk).exists());
    }
    let (mut second, bytes) = request(&f);
    second.workspace_key = Path::new(&first.workspace_key)
        .with_file_name("cleanup-next")
        .to_string_lossy()
        .into_owned();
    second.cwd = second.workspace_key.clone();
    f.submit(&runner, &second, &bytes);
    assert_eq!(
        terminal(&runner, second.job_id).await.outcome,
        Outcome::OutcomeUnknown
    );
    assert_eq!(agent.units.lock().unwrap().len(), 2);
    assert_eq!(agent.stops.load(Ordering::SeqCst), 1);
    assert!(failures.alive.load(Ordering::SeqCst));
    failures.remaining.store(0, Ordering::SeqCst);
    runner.maintenance().unwrap();
    for path in job_artifacts(runner.config(), first.job_id) {
        assert!(!path.exists(), "{} remains", path.display());
    }
    assert_eq!(agent.stops.load(Ordering::SeqCst), 2);
    assert_eq!(unit_alive(&runner, first.job_id), Some(false));
}

#[tokio::test]
async fn windows_backend_reclaim_preserves_terminal_staging_held_by_live_unit() {
    let f = Fixture::new();
    source(&f);
    let (mut config, agent) = configure(&f, Fault::RejectStart, 0);
    let failures = fail_stops(&mut config, u64::MAX);
    let pressure = Arc::new(AtomicBool::new(false));
    let probe = pressure.clone();
    config.free_space_floor_bytes = 1;
    config.free_space_probe =
        Arc::new(move |_| Ok(if probe.load(Ordering::SeqCst) { 0 } else { 100 }));
    let runner = Runner::open_with_backend(config, Box::new(FakeHost)).unwrap();
    let (request, bytes) = request(&f);
    f.submit(&runner, &request, &bytes);
    assert_eq!(
        terminal(&runner, request.job_id).await.outcome,
        Outcome::OutcomeUnknown
    );
    pressure.store(true, Ordering::SeqCst);
    assert!(runner.ensure_disk_space().unwrap().is_some());
    assert!(failures.alive.load(Ordering::SeqCst));
    assert_eq!(unit_alive(&runner, request.job_id), Some(true));
    for path in job_artifacts(runner.config(), request.job_id) {
        assert!(
            path.exists(),
            "reclaim removed live unit's {}",
            path.display()
        );
    }
    failures.remaining.store(0, Ordering::SeqCst);
    runner.maintenance().unwrap();
    cleaned(runner.config(), request.job_id, &agent);
    assert_eq!(unit_alive(&runner, request.job_id), Some(false));
}

#[tokio::test]
async fn windows_backend_cleanup_accepts_collected_unit_only_after_confirmed_exit() {
    for (kill_code, stop_code) in [(1, 0), (0, 1), (1, 1)] {
        let f = Fixture::new();
        source(&f);
        let (mut config, agent) = configure(&f, Fault::RejectStart, 0);
        let probes = Arc::new(AtomicU64::new(0));
        let captured = probes.clone();
        config.systemd_command = Some(Arc::new(move |args| {
            let (code, stdout) = match args[0] {
                "kill" => (kill_code, Vec::new()),
                "stop" => (stop_code, Vec::new()),
                "show" => {
                    captured.fetch_add(1, Ordering::SeqCst);
                    (
                        0,
                        b"LoadState=not-found\nActiveState=inactive\nSubState=dead\n".to_vec(),
                    )
                }
                _ => (0, Vec::new()),
            };
            Ok(output(code, stdout, b"unit already collected".to_vec()))
        }));
        let runner = Runner::open_with_backend(config, Box::new(FakeHost)).unwrap();
        let (request, bytes) = request(&f);
        f.submit(&runner, &request, &bytes);
        assert_eq!(
            terminal(&runner, request.job_id).await.outcome,
            Outcome::OutcomeUnknown
        );
        assert_eq!(probes.load(Ordering::SeqCst), 1);
        assert!(!runner
            .config()
            .windows_runtime
            .cleanup_pending(request.job_id));
        cleaned(runner.config(), request.job_id, &agent);
        assert!(!runner
            .config()
            .windows
            .as_ref()
            .unwrap()
            .caches()
            .disk(&request.workspace_key)
            .exists());
    }
}

#[tokio::test]
async fn windows_backend_failed_systemctl_stop_retains_live_files_until_maintenance() {
    let f = Fixture::new();
    source(&f);
    let (mut config, agent) = configure(&f, Fault::RejectStart, 0);
    let live = Arc::new(AtomicBool::new(true));
    let captured = live.clone();
    config.systemd_command = Some(Arc::new(move |args| {
        Ok(output(
            if args[0] == "stop" { 1 } else { 0 },
            if args[0] == "show" {
                if captured.load(Ordering::SeqCst) {
                    b"LoadState=loaded\nActiveState=active\nSubState=running\n".to_vec()
                } else {
                    b"LoadState=not-found\nActiveState=inactive\nSubState=dead\n".to_vec()
                }
            } else {
                Vec::new()
            },
            b"injected systemctl stop failure".to_vec(),
        ))
    }));
    let runner = Runner::open_with_backend(config, Box::new(FakeHost)).unwrap();
    let (request, bytes) = request(&f);
    f.submit(&runner, &request, &bytes);
    assert_eq!(
        terminal(&runner, request.job_id).await.outcome,
        Outcome::OutcomeUnknown
    );
    assert_eq!(agent.stops.load(Ordering::SeqCst), 0);
    assert_eq!(unit_alive(&runner, request.job_id), Some(true));
    for path in job_artifacts(runner.config(), request.job_id) {
        assert!(path.exists(), "live unit lost {}", path.display());
    }
    let caches = runner.config().windows.as_ref().unwrap().caches();
    let disk = caches.disk(&request.workspace_key);
    assert!(disk.exists());
    assert!(caches.marker(&disk).exists());
    assert!(caches
        .attach(&request.workspace_key, (runner.config().clock)())
        .is_err());
    live.store(false, Ordering::SeqCst);
    runner.maintenance().unwrap();
    cleaned(runner.config(), request.job_id, &agent);
    assert!(!disk.exists());
    assert!(!caches.marker(&disk).exists());
    assert_eq!(unit_alive(&runner, request.job_id), Some(false));
}

#[tokio::test]
async fn windows_backend_hard_cancel_discards_collected_unit_cache_before_reuse() {
    let f = Fixture::new();
    source(&f);
    let (mut config, agent) = configure(&f, Fault::Silent, 0);
    config.systemd_command = Some(Arc::new(|args| {
        Ok(output(
            if args[0] == "stop" { 1 } else { 0 },
            if args[0] == "show" {
                b"LoadState=not-found\nActiveState=inactive\nSubState=dead\n".to_vec()
            } else {
                Vec::new()
            },
            b"unit already collected".to_vec(),
        ))
    }));
    let runner = Runner::open_with_backend(config, Box::new(FakeHost)).unwrap();
    let caches = runner.config().windows.as_ref().unwrap().caches();
    for attempt in 0..2 {
        let (request, bytes) = request(&f);
        f.submit(&runner, &request, &bytes);
        tokio::time::timeout(Duration::from_secs(5), async {
            while agent.starts.load(Ordering::SeqCst) != attempt + 1 {
                tokio::task::yield_now().await;
            }
        })
        .await
        .unwrap();
        let disk = caches.disk(&request.workspace_key);
        assert_eq!(
            fs::read(&disk).unwrap(),
            b"fake qcow2",
            "a later VM saw the cancelled cache"
        );
        fs::write(&disk, b"cancelled cache canary").unwrap();
        runner.cancel(request.job_id).unwrap();
        f.clock.fetch_add(2_000, Ordering::SeqCst);
        runner.tick().unwrap();
        let terminal = terminal(&runner, request.job_id).await;
        assert_eq!(terminal.outcome, Outcome::OutcomeUnknown);
        assert_eq!(terminal.killed, Some(Killed::Cancel));
        for path in job_artifacts(runner.config(), request.job_id) {
            assert!(!path.exists(), "{} remains", path.display());
        }
        assert_eq!(agent.stops.load(Ordering::SeqCst), attempt + 1);
        assert!(
            !disk.exists(),
            "hard cancel retained its disk before maintenance"
        );
        assert!(!caches.marker(&disk).exists());
        assert!(!runner
            .config()
            .windows_runtime
            .cleanup_pending(request.job_id));
    }
}

#[tokio::test]
async fn windows_backend_start_failure_with_never_loaded_unit_discards_all_files() {
    let f = Fixture::new();
    source(&f);
    let (mut config, agent) = configure(&f, Fault::None, 0);
    config.windows_vm = Some(Arc::new(|_, _, _| {
        Err(io::Error::other(
            "injected VM start failure before unit creation",
        ))
    }));
    let probes = Arc::new(Mutex::new(Vec::new()));
    let captured = probes.clone();
    config.systemd_command = Some(Arc::new(move |args| {
        let (code, stdout) = match args[0] {
            "kill" | "stop" => {
                captured.lock().unwrap().push(args[0].to_string());
                (1, Vec::new())
            }
            "show" => {
                captured.lock().unwrap().push(args[0].to_string());
                (
                    1,
                    b"LoadState=not-found\nActiveState=inactive\nSubState=dead\n".to_vec(),
                )
            }
            _ => (0, Vec::new()),
        };
        Ok(output(code, stdout, b"Unit was never loaded".to_vec()))
    }));
    let runner = Runner::open_with_backend(config, Box::new(FakeHost)).unwrap();
    let (request, bytes) = request(&f);
    f.submit(&runner, &request, &bytes);
    let terminal = terminal(&runner, request.job_id).await;
    assert!(matches!(
        terminal.outcome,
        Outcome::RefusedBeforeStart { .. }
    ));
    assert_eq!(terminal.ran, Some(Ran::None));
    assert_eq!(*probes.lock().unwrap(), ["kill", "stop", "show"]);
    assert_eq!(agent.starts.load(Ordering::SeqCst), 0);
    assert!(!runner
        .config()
        .windows_runtime
        .cleanup_pending(request.job_id));
    for path in job_artifacts(runner.config(), request.job_id) {
        assert!(!path.exists(), "failed start retained {}", path.display());
    }
    let caches = runner.config().windows.as_ref().unwrap().caches();
    let disk = caches.disk(&request.workspace_key);
    assert!(!disk.exists());
    assert!(!caches.marker(&disk).exists());
    runner.maintenance().unwrap();
    assert_eq!(*probes.lock().unwrap(), ["kill", "stop", "show"]);
    assert!(!runner
        .config()
        .windows_runtime
        .cleanup_pending(request.job_id));
}
