use super::*;
use crate::{tenant, Kind, Lease, Machine, Peer};
use std::{
    io::Write,
    os::fd::OwnedFd,
    path::PathBuf,
    process::Command,
    sync::mpsc::{self, Receiver, Sender},
};

struct FencedMachine {
    config: crate::config::Config,
    tenants: tenant::Controller<tenant::tests::Fake>,
}

impl FencedMachine {
    fn new(missing: bool) -> Self {
        let mut value: serde_json::Value =
            serde_json::from_str(include_str!("../../../fixtures/helper.json")).unwrap();
        let problem = if missing {
            value.as_object_mut().unwrap().remove("uplink");
            "uplink <missing>: invalid uplink"
        } else {
            value["uplink"] = serde_json::json!("bad/name");
            "uplink bad/name: invalid uplink"
        };
        let config = serde_json::from_value(value).unwrap();
        Self {
            config,
            tenants: tenant::tests::degraded_service_fixture(problem),
        }
    }
}

impl Machine for FencedMachine {
    fn reconcile(&mut self) -> Result<()> {
        self.config.validate_resources()?;
        super::super::tenant_network::reconciliation_tests::fenced_startup(&self.config)?
            .serve_cleanup();
        Ok(())
    }
    fn start(&mut self, _: usize, _: &Lease, _: Kind) -> Result<Vec<OwnedFd>> {
        panic!("fixture must not start a slot")
    }
    fn stop(&mut self, _: usize, _: &Lease, _: bool) -> Result<()> {
        unreachable!()
    }
    fn cleanup(&mut self, _: usize) -> Result<()> {
        unreachable!()
    }
    fn alive(&mut self, _: usize) -> bool {
        false
    }
    fn tenant(&mut self, name: &str, op: tenant::Operation) -> Result<Response> {
        self.tenants.handle(name, op)
    }
}

struct Scratch(PathBuf);
impl Scratch {
    fn new() -> Self {
        let root = cortexkit_test_support::scratch_root().join(format!(
            "hs-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos(),
        ));
        std::fs::create_dir_all(&root).unwrap();
        Self(root)
    }
}
impl Drop for Scratch {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

pub(super) fn paths(root: &Path) -> crate::removal::Paths {
    crate::removal::Paths {
        root: root.into(),
        registry: root.join("registry"),
        provision: root.join("provision"),
        bindings: root.join("bindings.json"),
    }
}

fn request(stream: &mut BufReader<UnixStream>, op: &str) -> serde_json::Value {
    writeln!(stream.get_mut(), "{{\"op\":\"{op}\",\"tenant\":\"a\"}}").unwrap();
    let mut line = String::new();
    stream.read_line(&mut line).unwrap();
    serde_json::from_str(&line).unwrap()
}

#[test]
fn invalid_or_missing_uplink_binds_fences_and_serves_cleanup_but_refuses_opening() {
    for missing in [false, true] {
        let scratch = Scratch::new();
        let socket = scratch.0.join("helper.sock");
        let (helper, listener) =
            initialize_helper(FencedMachine::new(missing), 1, vec![1000], &socket).unwrap();
        let server = std::thread::spawn(move || {
            let (stream, _) = listener.accept().unwrap();
            let service = Service::new(&helper.lock().unwrap());
            let id = service
                .connections
                .connect(Peer {
                    uid: 1000,
                    gid: 1000,
                })
                .unwrap();
            connection_loop(&helper, &service, &|_, _| unreachable!(), id, stream).unwrap();
        });
        let stream = UnixStream::connect(&socket).unwrap();
        stream
            .set_read_timeout(Some(Duration::from_secs(5)))
            .unwrap();
        let mut client = BufReader::new(stream);
        assert_eq!(request(&mut client, "adopt")["result"], "tenant");
        assert_eq!(request(&mut client, "stop_tenant")["result"], "tenant");
        for op in ["ensure", "restore", "prepare_start", "set_egress"] {
            let refusal = request(&mut client, op);
            assert_eq!(refusal["result"], "start_refused", "{op}: {refusal}");
            assert_eq!(refusal["case"], "uplink");
            assert!(refusal["message"].as_str().unwrap().contains(if missing {
                "<missing>"
            } else {
                "bad/name"
            }));
        }
        assert_eq!(request(&mut client, "remove_tenant")["result"], "removed");
        // The fake backend now has no registry, so this second request must
        // exercise Controller's interrupted-removal route over the same socket.
        assert_eq!(request(&mut client, "remove_tenant")["result"], "removed");
        drop(client);
        server.join().unwrap();
    }
}

#[test]
fn qualification_helper_subprocess() {
    let Some(root) = std::env::var_os("MOTOR_TEST_QUALIFICATION_ROOT") else {
        return;
    };
    let root = PathBuf::from(root);
    // Requiring provision.lock at startup conflicts with qualification, which
    // holds that lock until its helper has started and exited. The legacy
    // branch reproduces that conflict; normal startup must bind without it.
    let _legacy_lock = if std::env::var_os("MOTOR_TEST_LEGACY_STARTUP").is_some() {
        Some(crate::removal::lock(&paths(&root)).unwrap())
    } else {
        None
    };
    let (_helper, _listener) = initialize_helper(
        FencedMachine::new(true),
        1,
        vec![1000],
        &root.join("helper.sock"),
    )
    .unwrap();
    assert!(UnixStream::connect(root.join("helper.sock")).is_ok());
    println!("QUALIFICATION_HELPER_BOUND");
}

#[test]
fn qualification_lock_allows_helper_startup_and_reproduces_legacy_lock_failure() {
    let scratch = Scratch::new();
    let _qualification_lock = crate::removal::lock(&paths(&scratch.0)).unwrap();
    let child = |legacy| {
        let mut command = Command::new(std::env::current_exe().unwrap());
        command
            .args([
                "--exact",
                "linux::server::tests::qualification_helper_subprocess",
                "--nocapture",
            ])
            .env("MOTOR_TEST_QUALIFICATION_ROOT", &scratch.0)
            .env_remove("MOTOR_TEST_LEGACY_STARTUP");
        if legacy {
            command.env("MOTOR_TEST_LEGACY_STARTUP", "1");
        }
        command.output().unwrap()
    };
    let legacy = child(true);
    let output = format!(
        "{}{}",
        String::from_utf8_lossy(&legacy.stdout),
        String::from_utf8_lossy(&legacy.stderr)
    );
    assert!(
        !legacy.status.success(),
        "the old wrapper must conflict with qualification"
    );
    assert!(output.contains("provisioning is in progress"), "{output}");
    assert!(!output.contains("QUALIFICATION_HELPER_BOUND"));
    let current = child(false);
    let output = format!(
        "{}{}",
        String::from_utf8_lossy(&current.stdout),
        String::from_utf8_lossy(&current.stderr)
    );
    assert!(current.status.success(), "{output}");
    assert!(output.contains("QUALIFICATION_HELPER_BOUND"), "{output}");
}

struct BlockingMachine {
    entered: Sender<()>,
    release: Receiver<()>,
    stopped: Sender<()>,
}

impl Machine for BlockingMachine {
    fn reconcile(&mut self) -> Result<()> {
        Ok(())
    }
    fn start(&mut self, _: usize, _: &Lease, _: Kind) -> Result<Vec<OwnedFd>> {
        unreachable!()
    }
    fn stop(&mut self, _: usize, _: &Lease, _: bool) -> Result<()> {
        unreachable!()
    }
    fn cleanup(&mut self, _: usize) -> Result<()> {
        unreachable!()
    }
    fn alive(&mut self, _: usize) -> bool {
        false
    }
    fn tenant(&mut self, _: &str, _: tenant::Operation) -> Result<Response> {
        self.entered.send(()).unwrap();
        self.release.recv_timeout(Duration::from_secs(5)).unwrap();
        Ok(Response::new(Reply::Ok))
    }
    fn shutdown(&mut self) -> Result<()> {
        self.stopped.send(()).unwrap();
        Ok(())
    }
}

fn test_peer() -> Peer {
    Peer {
        uid: 1000,
        gid: 1000,
    }
}

fn jobs() -> super::super::job::LinuxJobs {
    let config = serde_json::from_str(include_str!("../../../fixtures/helper.json")).unwrap();
    super::super::job::LinuxJobs::new(&config)
}

fn receive(client: &mut BufReader<UnixStream>) -> std::io::Result<serde_json::Value> {
    let mut line = String::new();
    client.read_line(&mut line)?;
    Ok(serde_json::from_str(&line).unwrap())
}

#[test]
fn blocked_tenant_does_not_delay_registration_or_job_exits_and_gates_still_refuse() {
    let (entered, entering) = mpsc::channel();
    let (release, releasing) = mpsc::channel();
    let (stopped, _stopping) = mpsc::channel();
    let helper = Arc::new(Mutex::new(
        Helper::new(
            BlockingMachine {
                entered,
                release: releasing,
                stopped,
            },
            1,
            vec![1000],
        )
        .unwrap(),
    ));
    let service = Arc::new(Service::new(&helper.lock().unwrap()));
    let tenant_id = service.connections.connect(test_peer()).unwrap();
    let tenant = {
        let helper = helper.clone();
        std::thread::spawn(move || {
            helper
                .lock()
                .unwrap()
                .handle_authorized(
                    tenant_id,
                    test_peer(),
                    Request::Ensure { tenant: "a".into() },
                )
                .unwrap()
        })
    };
    entering.recv_timeout(Duration::from_secs(1)).unwrap();

    let (client, stream) = UnixStream::pair().unwrap();
    client
        .set_read_timeout(Some(Duration::from_secs(1)))
        .unwrap();
    let mut client = BufReader::new(client);
    let worker = {
        let helper = helper.clone();
        let service = service.clone();
        std::thread::spawn(move || {
            // Registration must work while the Helper lock is held.
            let id = service.connections.connect(test_peer()).unwrap();
            let jobs = Mutex::new(jobs());
            connection_loop(
                &helper,
                &service,
                &|op, peer| jobs.lock().unwrap().handle(op, peer),
                id,
                stream,
            )
            .unwrap();
        })
    };
    writeln!(client.get_mut(), "{{\"op\":\"job_exits\"}}").unwrap();
    let reply = receive(&mut client);
    // Release the tenant even if the bounded read failed, so the test's failure
    // cannot strand a worker holding the Helper lock.
    release.send(()).unwrap();
    tenant.join().unwrap();
    assert_eq!(
        reply.expect("job_exits must reply within 1 s despite the tenant")["result"],
        "job_exits"
    );

    assert!(service
        .connections
        .connect(Peer { uid: 999, gid: 999 })
        .is_err());
    let (unknown, stream) = UnixStream::pair().unwrap();
    unknown
        .set_read_timeout(Some(Duration::from_secs(1)))
        .unwrap();
    let mut unknown = BufReader::new(unknown);
    let refused = {
        let helper = helper.clone();
        let service = service.clone();
        std::thread::spawn(move || {
            connection_loop(
                &helper,
                &service,
                &|_, _| panic!("unauthorized work"),
                u64::MAX,
                stream,
            )
            .unwrap()
        })
    };
    writeln!(unknown.get_mut(), "{{\"op\":\"job_exits\"}}").unwrap();
    assert_eq!(
        receive(&mut unknown).unwrap()["message"],
        "connection is not authorised"
    );
    drop(unknown);
    refused.join().unwrap();

    service.connections.stop();
    writeln!(client.get_mut(), "{{\"op\":\"job_exits\"}}").unwrap();
    assert_eq!(
        receive(&mut client).unwrap()["message"],
        "helper is stopping"
    );
    drop(client);
    worker.join().unwrap();
}

#[test]
fn shutdown_waits_for_paused_start_and_refuses_previously_authorized_lifecycle() {
    let (entered, _entering) = mpsc::channel();
    let (_release, releasing) = mpsc::channel();
    let (stopped, stopping) = mpsc::channel();
    let helper = Arc::new(Mutex::new(
        Helper::new(
            BlockingMachine {
                entered,
                release: releasing,
                stopped,
            },
            1,
            vec![1000],
        )
        .unwrap(),
    ));
    let service = Arc::new(Service::new(&helper.lock().unwrap()));
    let id = service.connections.connect(test_peer()).unwrap();
    let peer = service.connections.authorize(id).unwrap();
    let (paused, pausing) = mpsc::channel();
    let (resume, resuming) = mpsc::channel();
    let jobs = Arc::new(Mutex::new(jobs()));
    jobs.lock()
        .unwrap()
        .pause_exit_at(super::super::job::ExitStep::Captured, move || {
            paused.send(()).unwrap();
            resuming.recv_timeout(Duration::from_secs(5)).unwrap();
        });
    let start = crate::job::Start {
        kind: crate::job::VmKind::Job,
        job_id: crate::job::test_id(9),
        account: "a".into(),
        workspace_disk_id: "disk".into(),
        image_generation_id: "gen".into(),
        vcpus: 1,
        memory_mib: 128,
        burst_mib: None,
        cache_write_blocked: false,
    };
    let start_worker = {
        let service = service.clone();
        let jobs = jobs.clone();
        std::thread::spawn(move || {
            service
                .job(crate::job::Operation::Start(start), peer, &|_, _| {
                    // The test pauses at the exit hook instead of booting a VM;
                    // the service gate must cover the entire operation.
                    jobs.lock()
                        .unwrap()
                        .exit_checkpoint(super::super::job::ExitStep::Captured)?;
                    Ok(Response::new(Reply::Ok))
                })
                .unwrap()
        })
    };
    pausing.recv_timeout(Duration::from_secs(1)).unwrap();
    let shutdown = {
        let helper = helper.clone();
        let service = service.clone();
        std::thread::spawn(move || {
            let (_guard, result) = service.shutdown(&helper);
            result.unwrap();
        })
    };
    // Wait for the shutdown's stopping publication, not a scheduling delay.
    let deadline = std::time::Instant::now() + Duration::from_secs(1);
    while service.connections.check_running().is_ok() {
        assert!(
            std::time::Instant::now() < deadline,
            "shutdown did not set stopping"
        );
        std::thread::yield_now();
    }
    assert!(
        stopping.recv_timeout(Duration::from_millis(100)).is_err(),
        "shutdown passed an in-flight start"
    );
    let queued = {
        let service = service.clone();
        std::thread::spawn(move || {
            service.job(
                crate::job::Operation::Stop(crate::job::test_id(9)),
                peer,
                &|_, _| panic!("stopped lifecycle work reached the plane"),
            )
        })
    };
    resume.send(()).unwrap();
    start_worker.join().unwrap();
    stopping.recv_timeout(Duration::from_secs(1)).unwrap();
    shutdown.join().unwrap();
    assert_eq!(
        queued.join().unwrap().err().unwrap().0,
        "helper is stopping"
    );
}
