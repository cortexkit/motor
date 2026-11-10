use super::*;
use crate::state::settlement::{StopCause, Winner};
use motor_lease::{OomKill, UsageOutcome};
use motor_protocol::{TenantEvent, TenantEventKind};
use motor_vm_helper::job::ExitReport;

fn event(job: &str, seq: u64, kind: TenantEventKind) -> TenantEvent {
    serde_json::from_value(json!({"type":"event","event_seq":seq,"event":kind,"jti":job})).unwrap()
}
fn running(runtime: &Arc<Runtime>, job: &str, delivered: bool, started: bool) {
    let id = claim(runtime, &claims(job));
    let mut host = runtime.host.lock().unwrap();
    host.begin_job_boot_at(job, 100).unwrap().unwrap();
    host.job_runner_hello(job).unwrap();
    if delivered {
        host.delivered(id).unwrap();
        host.event_observed(job, &event(job, 1, TenantEventKind::Admitted), 100, true)
            .unwrap();
    }
    if started {
        host.event_observed(job, &event(job, 2, TenantEventKind::Started), 200, true)
            .unwrap();
    }
}
fn terminal(job: &str) -> Value {
    json!({"type":"terminal","job_id":job,"outcome":{"type":"exit","code":0},"wall_ms":100,"queue_wait_ms":0,"bundle_bytes":0,"ran":"remote","tree_hash":null,"workspace_changes":null,"mem_working_peak_mib":111,"mem_over_mib_ms":222})
}
fn exit(job: &str, count: u64) -> ExitReport {
    ExitReport {
        job_id: job.into(),
        oom_kill: count,
        egress_bytes: Some(321),
    }
}
fn body(runtime: &Arc<Runtime>, job: &str) -> Vec<u8> {
    serde_json::to_vec(
        runtime.host.lock().unwrap().snapshot.billing[job]
            .usage
            .as_ref()
            .unwrap(),
    )
    .unwrap()
}

#[test]
fn failed_resource_cleanup_does_not_starve_billing_in_the_same_tick() {
    let (runtime, _, storage) = fixture();
    running(&runtime, JOB, true, true);
    {
        let mut host = runtime.host.lock().unwrap();
        host.receive_terminal(JOB, terminal(JOB), 400).unwrap();
        assert!(host.job_exit_received(&exit(JOB, 0), 400).unwrap());
        host.job_exit_acknowledged(&exit(JOB, 0)).unwrap();
        host.job_stopped_at(JOB, false, 500).unwrap();
    }
    // A directory in place of the spool makes its unlink fail repeatedly.
    // Settlement must still reach the broker outbox on every such tick.
    std::fs::create_dir(storage.spool(JOB)).unwrap();
    assert!(runtime.tick_jobs(100_000).is_err());
    assert!(runtime
        .host
        .lock()
        .unwrap()
        .snapshot
        .outbox
        .iter()
        .any(|r| r.job.as_deref() == Some(JOB)));
}

#[test]
fn every_terminal_pressure_exit_order_keeps_one_immutable_body() {
    for order in [
        [0, 1, 2],
        [0, 2, 1],
        [1, 0, 2],
        [1, 2, 0],
        [2, 0, 1],
        [2, 1, 0],
    ] {
        let (runtime, _, _) = fixture();
        running(&runtime, JOB, true, true);
        let mut original = None;
        for action in order {
            let mut host = runtime.host.lock().unwrap();
            match action {
                0 => host.receive_terminal(JOB, terminal(JOB), 400).unwrap(),
                1 => host
                    .job_host_stop(JOB, StopCause::HostPressure, 400)
                    .unwrap(),
                _ => {
                    assert!(host.job_exit_received(&exit(JOB, 0), 400).unwrap());
                }
            }
            if host.snapshot.per_job.executions[JOB]
                .evidence
                .as_ref()
                .unwrap()
                .exit
                .is_none()
            {
                assert!(host.snapshot.billing[JOB].usage.is_none());
            } else {
                drop(host);
                let actual = body(&runtime, JOB);
                assert_eq!(*original.get_or_insert(actual.clone()), actual);
            }
        }
        runtime.host.lock().unwrap().billing_tick(100_000).unwrap();
        let host = runtime.host.lock().unwrap();
        let requests: Vec<_> = host
            .snapshot
            .outbox
            .iter()
            .filter(|r| r.job.as_deref() == Some(JOB))
            .collect();
        assert_eq!(requests.len(), 1);
        assert_eq!(requests[0].body, original.unwrap());
        assert_eq!(
            host.snapshot.billing[JOB]
                .usage
                .as_ref()
                .unwrap()
                .egress_bytes,
            Some(321)
        );
    }
}

#[test]
fn concurrent_terminal_pressure_and_exit_commit_one_winner() {
    let (runtime, _, _) = fixture();
    running(&runtime, JOB, true, true);
    let barrier = Arc::new(std::sync::Barrier::new(3));
    let threads: Vec<_> = (0..3)
        .map(|action| {
            let runtime = runtime.clone();
            let barrier = barrier.clone();
            std::thread::spawn(move || {
                barrier.wait();
                let mut host = runtime.host.lock().unwrap();
                match action {
                    0 => host.receive_terminal(JOB, terminal(JOB), 400).unwrap(),
                    1 => host
                        .job_host_stop(JOB, StopCause::HostPressure, 400)
                        .unwrap(),
                    _ => {
                        host.job_exit_received(&exit(JOB, 0), 400).unwrap();
                    }
                }
            })
        })
        .collect();
    for thread in threads {
        thread.join().unwrap();
    }
    let original = body(&runtime, JOB);
    runtime
        .host
        .lock()
        .unwrap()
        .receive_terminal(JOB, terminal(JOB2), 600)
        .unwrap();
    assert_eq!(body(&runtime, JOB), original);
}

/// The normal end of a job: the host acknowledges the stored terminal, the
/// guest ends its own VM, and the helper reports the exit. The disk must come
/// back clean (no recovery VM) and the helper must release it exactly once.
#[test]
fn guest_ended_vm_after_terminal_stored_releases_its_disk_clean() {
    let (runtime, fake, _) = fixture();
    running(&runtime, JOB, true, true);
    runtime
        .host
        .lock()
        .unwrap()
        .job_stop_intent(JOB, false)
        .unwrap();
    // Live order: a tick sees the VMM gone before the helper's exit report is
    // collected, and the report arrives on a later poll.
    fake.gone.lock().unwrap().insert(JOB.into());
    runtime.tick_jobs(500).unwrap();
    fake.exits.lock().unwrap().push(exit(JOB, 0));
    runtime.tick_jobs(1600).unwrap();
    let calls = fake.calls.lock().unwrap().clone();
    assert_eq!(
        calls
            .iter()
            .filter(|c| *c == &format!("stop:{JOB}"))
            .count(),
        1,
        "{calls:?}"
    );
    let host = runtime.host.lock().unwrap();
    let execution = &host.snapshot.per_job.executions[JOB];
    assert!(execution.ended);
    let disk = &host.snapshot.per_job.disks[&execution.disk_id];
    assert!(
        !disk.quarantined,
        "a clean guest exit must not need recovery"
    );
    assert!(disk.recovery.is_none());
}

#[test]
fn a_forced_stop_after_a_clean_intent_still_quarantines() {
    let (runtime, _, _) = fixture();
    running(&runtime, JOB, true, true);
    let mut host = runtime.host.lock().unwrap();
    host.job_stop_intent(JOB, false).unwrap();
    host.job_stop_intent(JOB, true).unwrap();
    let execution = &host.snapshot.per_job.executions[JOB];
    assert_eq!(
        execution.evidence.as_ref().unwrap().unclean_stop,
        Some(true)
    );
    assert!(host.snapshot.per_job.disks[&execution.disk_id].quarantined);
}

#[test]
fn receipt_is_the_only_oom_evidence_and_never_corrects_an_earlier_usage() {
    for count in [0, 4] {
        let (runtime, fake, storage) = fixture();
        running(&runtime, JOB, true, true);
        fake.gone.lock().unwrap().insert(JOB.into());
        runtime.tick_jobs(300).unwrap();
        assert!(runtime.host.lock().unwrap().snapshot.billing[JOB]
            .usage
            .is_none());
        fake.exits.lock().unwrap().push(exit(JOB, count));
        runtime.tick_jobs(500).unwrap();
        let first = body(&runtime, JOB);
        let host = runtime.host.lock().unwrap();
        let usage = host.snapshot.billing[JOB].usage.as_ref().unwrap();
        assert_eq!(
            usage.outcome,
            if count > 0 {
                UsageOutcome::Signal
            } else {
                UsageOutcome::Lost
            }
        );
        assert_eq!(usage.oom_kill, (count > 0).then_some(OomKill::HostOom));
        // Lost or killed, a job is billed up to when the host settled it, never
        // for its whole requested runtime.
        assert_eq!(usage.ended_at_ms, 500);
        assert!(usage.ended_at_ms - usage.started_at_ms < 30_000);
        assert_eq!(
            host.snapshot.per_job.executions[JOB]
                .evidence
                .as_ref()
                .unwrap()
                .exit
                .as_ref()
                .unwrap()
                .oom_kill,
            count
        );
        assert!(host.snapshot.per_job.executions[JOB].ended);
        drop(host);
        assert!(!storage.held.lock().unwrap().contains(JOB));
        fake.exits.lock().unwrap().push(exit(JOB, count));
        runtime.tick_jobs(100_000).unwrap();
        assert_eq!(body(&runtime, JOB), first);
        assert_eq!(fake.acknowledgements.lock().unwrap().len(), 1);
    }
}

#[test]
fn delivery_is_the_no_start_boundary_for_exit_and_pressure() {
    for (delivered, started, pressure) in [
        (false, false, false),
        (true, false, false),
        (true, true, false),
        (true, false, true),
    ] {
        let (runtime, fake, _) = fixture();
        running(&runtime, JOB, delivered, started);
        if pressure {
            runtime.stop_job_pressure(JOB, 400).unwrap();
        } else {
            fake.exits.lock().unwrap().push(exit(JOB, 0));
            runtime.tick_jobs(400).unwrap();
        }
        let host = runtime.host.lock().unwrap();
        let usage = host.snapshot.billing[JOB].usage.as_ref().unwrap();
        if delivered {
            assert_eq!(usage.outcome, UsageOutcome::Lost);
            // Billed for observed running time only, up to settlement at 400.
            if started {
                assert_eq!(usage.ended_at_ms, 400);
                assert!(usage.started_at_ms < 400);
            } else {
                assert_eq!(usage.ended_at_ms, usage.started_at_ms);
            }
            let frames = host.host_settled_frames(JOB, 0).unwrap();
            assert_eq!(frames[1]["outcome"]["type"], "outcome_unknown");
            assert_eq!(frames[1]["ran"], "remote");
            assert!(host.snapshot.leases[JOB].delivered);
        } else {
            assert_eq!(
                host.snapshot.leases[JOB].refusal,
                Some(Refusal::InternalFailure)
            );
            assert_eq!(usage.started_at_ms, usage.ended_at_ms);
            assert_eq!(usage.outcome, UsageOutcome::Cancelled);
        }
        assert!(host.snapshot.per_job.executions[JOB].ended);
        drop(host);
        assert!(runtime
            .host
            .lock()
            .unwrap()
            .begin_job_boot(JOB)
            .unwrap()
            .is_none());
    }
}

#[test]
fn pressure_terminal_uses_latest_sample_and_unobserved_results_are_null() {
    for sampled in [false, true] {
        let (runtime, _, _) = fixture();
        running(&runtime, JOB, true, true);
        if sampled {
            let mut host = runtime.host.lock().unwrap();
            for peak in [110, 120] {
                host.job_memory_sample(motor_protocol::MemorySample {
                    jti: JOB.parse().unwrap(),
                    mem_working_peak_mib: Some(peak),
                    mem_over_mib_ms: Some(Some(1000)),
                })
                .unwrap();
            }
        }
        runtime.stop_job_pressure(JOB, 400).unwrap();
        let host = runtime.host.lock().unwrap();
        let b = &host.snapshot.billing[JOB];
        let usage = b.usage.as_ref().unwrap();
        assert_eq!(usage.outcome, UsageOutcome::Signal);
        assert_eq!(usage.oom_kill, Some(OomKill::HostPressure));
        assert_eq!(usage.ended_at_ms, 400);
        assert_eq!(usage.mem_working_peak_mib, Some(sampled.then_some(120)));
        assert_eq!(usage.mem_over_mib_ms, Some(sampled.then_some(1000)));
        let terminal = b.terminal.as_ref().unwrap();
        serde_json::from_value::<motor_protocol::Frame>(terminal.clone()).unwrap();
        assert_eq!(terminal["outcome"]["type"], "signal");
        assert!(terminal["workspace_changes"].is_null());
        assert!(terminal["returns"].is_null());
        assert!(!b.host_settled);
        assert!(host.host_settled_frames(JOB, 0).is_some());
    }
}

#[test]
fn guest_cannot_forge_a_host_stop() {
    for cause in ["host_oom", "host_pressure"] {
        let (runtime, _, _) = fixture();
        running(&runtime, JOB, true, true);
        let mut frame = terminal(JOB);
        frame["oom_kill"] = json!({"cause":cause});
        runtime
            .host
            .lock()
            .unwrap()
            .receive_terminal(JOB, frame, 400)
            .unwrap();
        let mut host = runtime.host.lock().unwrap();
        host.job_exit_received(&exit(JOB, 0), 500).unwrap();
        assert_eq!(
            host.snapshot.billing[JOB].usage.as_ref().unwrap().oom_kill,
            None
        );
        assert!(host.snapshot.billing[JOB]
            .terminal
            .as_ref()
            .unwrap()
            .get("oom_kill")
            .is_none());
    }
}

fn reopen(runtime: &Arc<Runtime>, directory: &std::path::Path) {
    let (journal, snapshot) = crate::store::SqliteJournal::open(directory).unwrap();
    let mut host = runtime.host.lock().unwrap();
    let settings = host.settings.clone();
    *host = Host::new(BTreeMap::new(), snapshot, Box::new(journal));
    host.settings = settings;
}

#[test]
fn reboot_gap_bound_starts_at_first_gone_observation_and_survives_restart() {
    let storage_root = Storage::new();
    let directory = storage_root.root.path().join("journal");
    let (runtime, fake, _) = fixture();
    reopen(&runtime, &directory);
    // The host rebooted long after the VM's start. No helper evidence survived.
    runtime
        .host
        .lock()
        .unwrap()
        .record_host_resources(HostResources {
            at_ms: unix_ms(),
            available_mem_mib: 1000,
            vcpu_ceiling: 2,
            permit_budget: 4,
        });
    running(&runtime, JOB, true, true);
    fake.gone.lock().unwrap().insert(JOB.into());
    runtime.tick_jobs(1_000_000).unwrap();
    assert!(runtime.host.lock().unwrap().snapshot.billing[JOB]
        .usage
        .is_none());
    drop(runtime);
    let (runtime, fake, _) = fixture();
    reopen(&runtime, &directory);
    fake.gone.lock().unwrap().insert(JOB.into());
    runtime.tick_jobs(1_000_999).unwrap();
    assert!(runtime.host.lock().unwrap().snapshot.billing[JOB]
        .usage
        .is_none());
    runtime.tick_jobs(1_001_000).unwrap();
    let host = runtime.host.lock().unwrap();
    let evidence = host.snapshot.per_job.executions[JOB]
        .evidence
        .as_ref()
        .unwrap();
    assert_eq!(evidence.gone_at_ms, Some(1_000_000));
    assert_eq!(evidence.winner, Some(Winner::EvidenceExpired));
    assert!(evidence.exit.is_none());
    assert_eq!(
        host.snapshot.billing[JOB].usage.as_ref().unwrap().oom_kill,
        None
    );
    assert_eq!(
        host.snapshot.billing[JOB].usage.as_ref().unwrap().outcome,
        UsageOutcome::Lost
    );
}

#[test]
fn helper_restart_re_adopts_running_jobs_and_keeps_their_outcomes_separate() {
    let (runtime, _, storage) = fixture();
    running(&runtime, JOB, true, true);
    // A distinct workspace lets both VMs run concurrently.
    let mut second = claims(JOB2);
    second.workspace_key_hash = Some(motor_lease::base64url(&[6; 32]));
    let id = claim(&runtime, &second);
    {
        let mut host = runtime.host.lock().unwrap();
        host.begin_job_boot_at(JOB2, 100).unwrap().unwrap();
        host.job_runner_hello(JOB2).unwrap();
        host.delivered(id).unwrap();
        host.event_observed(JOB2, &event(JOB2, 1, TenantEventKind::Admitted), 100, true)
            .unwrap();
        host.event_observed(JOB2, &event(JOB2, 2, TenantEventKind::Started), 200, true)
            .unwrap();
    }
    // A fresh helper RPC fixture represents a restarted helper. Adoption must
    // reconnect to the existing VMs without authorizing another launch.
    let fake = Arc::new(Fake::default());
    let snapshot = runtime.host.lock().unwrap().snapshot.clone();
    let (journal, _) = crate::journal::FileJournal::open(storage.root.path()).unwrap();
    let mut host = Host::new(BTreeMap::new(), snapshot, Box::new(journal));
    host.settings = runtime.host.lock().unwrap().settings.clone();
    let restarted = Runtime::with_job_resources("host".into(), host, fake.clone(), storage);
    restarted.recover_jobs().unwrap();
    assert_eq!(
        fake.calls
            .lock()
            .unwrap()
            .iter()
            .filter(|c| c.starts_with("adopt:"))
            .count(),
        2
    );
    assert!(!fake
        .calls
        .lock()
        .unwrap()
        .iter()
        .any(|c| c.starts_with("start:")));
    restarted
        .host
        .lock()
        .unwrap()
        .receive_terminal(JOB, terminal(JOB), 400)
        .unwrap();
    fake.exits
        .lock()
        .unwrap()
        .extend([exit(JOB, 0), exit(JOB2, 3)]);
    restarted.tick_jobs(500).unwrap();
    let host = restarted.host.lock().unwrap();
    assert_eq!(
        host.snapshot.billing[JOB].usage.as_ref().unwrap().outcome,
        UsageOutcome::Exit
    );
    assert_eq!(
        host.snapshot.billing[JOB2].usage.as_ref().unwrap().oom_kill,
        Some(OomKill::HostOom)
    );
}

#[test]
fn failed_ack_retains_reservations_and_retries_the_exact_receipt() {
    let (runtime, fake, storage) = fixture();
    running(&runtime, JOB, true, true);
    fake.exits.lock().unwrap().push(exit(JOB, 7));
    fake.fail_ack.store(true, Ordering::Release);
    assert!(runtime.tick_jobs(500).is_err());
    let original = body(&runtime, JOB);
    assert!(storage.held.lock().unwrap().contains(JOB));
    assert!(!runtime.host.lock().unwrap().snapshot.per_job.executions[JOB].ended);
    fake.fail_ack.store(false, Ordering::Release);
    runtime.tick_jobs(600).unwrap();
    assert_eq!(body(&runtime, JOB), original);
    assert!(!storage.held.lock().unwrap().contains(JOB));
}

#[test]
#[ignore = "subprocess crash fixture, invoked only by crash_boundaries_recover_one_captured_settlement"]
fn crash_child() {
    let directory =
        std::path::PathBuf::from(std::env::var_os("MOTOR_SETTLEMENT_CRASH_DIR").unwrap());
    let stage = std::env::var("MOTOR_SETTLEMENT_CRASH_STAGE").unwrap();
    let (runtime, _, _) = fixture();
    reopen(&runtime, &directory);
    runtime
        .host
        .lock()
        .unwrap()
        .record_host_resources(HostResources {
            at_ms: unix_ms(),
            available_mem_mib: 1000,
            vcpu_ceiling: 2,
            permit_budget: 4,
        });
    running(&runtime, JOB, true, true);
    if stage == "delivery" {
        std::process::exit(77)
    }
    runtime
        .host
        .lock()
        .unwrap()
        .job_exit_received(&exit(JOB, 5), 400)
        .unwrap();
    if stage == "receipt" {
        std::process::exit(77)
    }
    let ack = std::fs::File::create(directory.join("helper-ack")).unwrap();
    ack.sync_all().unwrap();
    std::fs::File::open(&directory).unwrap().sync_all().unwrap();
    if stage == "helper-ack" {
        std::process::exit(77)
    }
    runtime
        .host
        .lock()
        .unwrap()
        .job_exit_acknowledged(&exit(JOB, 5))
        .unwrap();
    if stage == "host-ack" {
        std::process::exit(77)
    }
    runtime.host.lock().unwrap().job_stopped(JOB, true).unwrap();
    std::process::exit(77)
}

#[test]
fn crash_boundaries_recover_one_captured_settlement() {
    for stage in ["delivery", "receipt", "helper-ack", "host-ack", "release"] {
        let storage = Storage::new();
        let directory = storage.root.path().join("journal");
        let status = std::process::Command::new(std::env::current_exe().unwrap())
            .args([
                "--exact",
                "server::job::tests::settlement_tests::crash_child",
                "--ignored",
                "--nocapture",
            ])
            .env("MOTOR_SETTLEMENT_CRASH_DIR", &directory)
            .env("MOTOR_SETTLEMENT_CRASH_STAGE", stage)
            .status()
            .unwrap();
        assert_eq!(status.code(), Some(77), "crash stage {stage}");
        let (runtime, fake, storage) = fixture();
        reopen(&runtime, &directory);
        storage.held.lock().unwrap().insert(JOB.into());
        if !directory.join("helper-ack").exists() {
            fake.exits.lock().unwrap().push(exit(JOB, 5));
        }
        runtime.recover_jobs().unwrap();
        runtime.tick_jobs(unix_ms() + 100_000).unwrap();
        let original = body(&runtime, JOB);
        let host = runtime.host.lock().unwrap();
        let e = &host.snapshot.per_job.executions[JOB];
        assert!(e.ended, "stage {stage}");
        let evidence = e.evidence.as_ref().unwrap();
        assert!(evidence.acknowledged);
        assert_eq!(evidence.exit.as_ref().unwrap().oom_kill, 5);
        assert_eq!(
            host.snapshot.billing[JOB].usage.as_ref().unwrap().oom_kill,
            Some(OomKill::HostOom)
        );
        assert_eq!(
            host.snapshot
                .outbox
                .iter()
                .filter(|r| r.job.as_deref() == Some(JOB))
                .count(),
            1
        );
        assert!(!storage.held.lock().unwrap().contains(JOB));
        drop(host);
        runtime.tick_jobs(unix_ms() + 100_001).unwrap();
        assert_eq!(body(&runtime, JOB), original);
    }
}

#[test]
fn host_originated_usage_resend_is_accepted_without_409() {
    use ed25519_dalek::SigningKey;
    use motor_lease::fake::{BrokerRequest, FakeBroker, FakeConfig};
    let now = unix_ms();
    let key = SigningKey::from_bytes(&[4; 32]);
    let mut broker = FakeBroker::new(
        "settlement-test",
        FakeConfig {
            worker_bearer: "worker".into(),
            subject: claims(JOB).sub,
            placement: motor_lease::Placement {
                host: "host".into(),
                vm: None,
            },
            endpoint: "127.0.0.1:4443".into(),
            endpoint_spki_sha256: motor_lease::base64url(&[8; 32]),
            kid: "current".into(),
            signing_key: SigningKey::from_bytes(&[1; 32]),
            announced_keys: vec![],
        },
    )
    .unwrap();
    broker.register_host("host".into(), key.verifying_key());
    let request = json!({"job_id":JOB,"vcpus":1,"platform":"linux-x64","net":false,"workspace_key_hash":motor_lease::base64url(&[5;32]),"repo_hash":motor_lease::base64url(&[6;32]),"placement_hint":null,"avoid_host":null,"timeout_s":30,"allow_spill":true,"requester_jwk":motor_lease::RequesterJwk::from_key(&SigningKey::from_bytes(&[3;32]).verifying_key())});
    let body = serde_json::to_vec(&request).unwrap();
    let reply = broker.handle(
        BrokerRequest {
            content_type: Some("application/json"),
            method: "POST",
            path: "/v1/leases",
            bearer: Some("worker"),
            host_headers: None,
            body: &body,
        },
        now,
    );
    assert_eq!(reply.status, 201, "{}", reply.body);
    let issued: motor_lease::LeaseResponse = serde_json::from_value(reply.body).unwrap();
    let mut keys = motor_lease::KeySet::default();
    keys.refresh(Ok(broker.jwks())).unwrap();
    let claims = motor_lease::verify_token(
        &issued.job_token,
        &keys,
        "host",
        Some(JOB),
        Purpose::Run,
        now / 1000,
    )
    .unwrap();
    let (runtime, _, _) = fixture();
    let request = {
        let mut host = runtime.host.lock().unwrap();
        host.record_host_resources(HostResources {
            at_ms: now,
            available_mem_mib: 100_000,
            vcpu_ceiling: 2,
            permit_budget: 4,
        });
        let id = host.claim(claims, now).unwrap().0;
        host.begin_job_boot_at(JOB, now).unwrap().unwrap();
        host.job_runner_hello(JOB).unwrap();
        host.delivered(id).unwrap();
        host.event_observed(JOB, &event(JOB, 1, TenantEventKind::Admitted), now, true)
            .unwrap();
        host.event_observed(JOB, &event(JOB, 2, TenantEventKind::Started), now, true)
            .unwrap();
        host.job_host_stop(JOB, StopCause::HostPressure, now + 1000)
            .unwrap();
        assert!(host.snapshot.billing[JOB].usage.is_none());
        host.job_exit_received(&exit(JOB, 0), now + 2000).unwrap();
        host.billing_tick(now + 100_000).unwrap();
        host.snapshot
            .outbox
            .iter()
            .find(|r| r.job.as_deref() == Some(JOB))
            .unwrap()
            .clone()
    };
    for counter in [1, 2] {
        let headers = motor_lease::HostHeaders::sign(
            "host",
            "POST",
            &request.path,
            &request.body,
            now + 100_000,
            counter,
            &key,
        )
        .unwrap();
        let reply = broker.handle(
            BrokerRequest {
                content_type: Some("application/json"),
                method: "POST",
                path: &request.path,
                bearer: None,
                host_headers: Some(&headers),
                body: &request.body,
            },
            now + 100_000,
        );
        assert_eq!(reply.status, 200, "resend {counter}: {}", reply.body);
    }
}

#[test]
fn relay_started_precedes_lifecycle_and_terminal_uses_the_recorded_body() {
    let (runtime, fake, _) = fixture();
    running(&runtime, JOB, true, false);
    runtime
        .observe_job_frame(JOB, &json!({"type":"started","job_id":JOB}))
        .unwrap();
    let mut forged = terminal(JOB);
    forged["oom_kill"] = json!({"cause":"host_pressure"});
    let actual = runtime.observe_job_frame(JOB, &forged).unwrap().unwrap();
    assert!(actual.get("oom_kill").is_none());
    assert!(runtime.host.lock().unwrap().snapshot.billing[JOB]
        .usage
        .is_none());
    fake.exits.lock().unwrap().push(exit(JOB, 0));
    runtime.tick_jobs(unix_ms()).unwrap();
    assert_eq!(
        runtime.host.lock().unwrap().snapshot.billing[JOB]
            .usage
            .as_ref()
            .unwrap()
            .outcome,
        UsageOutcome::Exit
    );
}

#[derive(Clone, Default)]
struct Logs(Arc<Mutex<Vec<String>>>);
impl tracing::Subscriber for Logs {
    fn enabled(&self, _: &tracing::Metadata<'_>) -> bool {
        true
    }
    fn new_span(&self, _: &tracing::span::Attributes<'_>) -> tracing::span::Id {
        tracing::span::Id::from_u64(1)
    }
    fn record(&self, _: &tracing::span::Id, _: &tracing::span::Record<'_>) {}
    fn record_follows_from(&self, _: &tracing::span::Id, _: &tracing::span::Id) {}
    fn enter(&self, _: &tracing::span::Id) {}
    fn exit(&self, _: &tracing::span::Id) {}
    fn event(&self, event: &tracing::Event<'_>) {
        struct Visitor(String);
        impl tracing::field::Visit for Visitor {
            fn record_debug(&mut self, field: &tracing::field::Field, value: &dyn std::fmt::Debug) {
                self.0.push_str(&format!("{}={value:?} ", field.name()));
            }
        }
        let mut visitor = Visitor(String::new());
        event.record(&mut visitor);
        self.0.lock().unwrap().push(visitor.0);
    }
}

#[test]
fn every_host_stop_and_missing_evidence_gap_is_logged_with_jti() {
    let (runtime, _, _) = fixture();
    running(&runtime, JOB, true, true);
    let logs = Logs::default();
    let dispatch = tracing::Dispatch::new(logs.clone());
    let _spare = tracing::Dispatch::new(Logs::default());
    tracing::dispatcher::with_default(&dispatch, || {
        tracing::callsite::rebuild_interest_cache();
        let mut host = runtime.host.lock().unwrap();
        host.receive_terminal(JOB, terminal(JOB), 300).unwrap();
        // Even a stop that loses to a guest terminal must be visible.
        host.job_host_stop(JOB, StopCause::HostPressure, 400)
            .unwrap();
        host.job_exit_received(&exit(JOB, 2), 500).unwrap();
        host.job_vmm_gone(JOB, 600).unwrap();
    });
    let lines = logs.0.lock().unwrap();
    for cause in ["host_pressure", "host_oom"] {
        assert!(
            lines
                .iter()
                .any(|line| line.contains("message=job_host_stop")
                    && line.contains(cause)
                    && line.contains(JOB)),
            "{lines:?}"
        );
    }
    drop(lines);
    let (runtime, _, _) = fixture();
    running(&runtime, JOB, true, true);
    tracing::dispatcher::with_default(&dispatch, || {
        let mut host = runtime.host.lock().unwrap();
        host.job_vmm_gone(JOB, 1000).unwrap();
        host.job_vmm_gone(JOB, 2000).unwrap();
    });
    assert!(logs
        .0
        .lock()
        .unwrap()
        .iter()
        .any(|line| line.contains("message=job_exit_evidence_gap") && line.contains(JOB)));
}

#[test]
fn evidence_deadline_runs_through_poll_failure_but_never_releases_an_unacked_receipt() {
    let (runtime, fake, storage) = fixture();
    running(&runtime, JOB, true, true);
    fake.gone.lock().unwrap().insert(JOB.into());
    runtime.tick_jobs(300).unwrap();
    fake.fail_poll.store(true, Ordering::Release);
    assert!(runtime.tick_jobs(1300).is_err());
    assert_eq!(
        runtime.host.lock().unwrap().snapshot.billing[JOB]
            .usage
            .as_ref()
            .unwrap()
            .outcome,
        UsageOutcome::Lost
    );
    assert!(!storage.held.lock().unwrap().contains(JOB));

    let (runtime, fake, storage) = fixture();
    running(&runtime, JOB, true, true);
    fake.gone.lock().unwrap().insert(JOB.into());
    runtime.tick_jobs(300).unwrap();
    fake.exits.lock().unwrap().push(exit(JOB, 4));
    fake.fail_ack.store(true, Ordering::Release);
    assert!(runtime.tick_jobs(1300).is_err());
    assert!(storage.held.lock().unwrap().contains(JOB));
    assert!(!runtime.host.lock().unwrap().snapshot.per_job.executions[JOB].ended);
}

#[test]
fn late_receipt_after_bound_cannot_add_egress_or_change_the_guest_winner() {
    let (runtime, fake, _) = fixture();
    running(&runtime, JOB, true, true);
    let result = runtime
        .observe_job_frame(JOB, &terminal(JOB))
        .unwrap()
        .unwrap();
    assert_eq!(result["outcome"]["type"], "exit");
    assert!(runtime.host.lock().unwrap().snapshot.billing[JOB]
        .usage
        .is_none());
    fake.gone.lock().unwrap().insert(JOB.into());
    runtime.tick_jobs(500).unwrap();
    runtime.tick_jobs(1499).unwrap();
    assert!(runtime.host.lock().unwrap().snapshot.billing[JOB]
        .usage
        .is_none());
    runtime.tick_jobs(1500).unwrap();
    let original = body(&runtime, JOB);
    {
        let host = runtime.host.lock().unwrap();
        let usage = host.snapshot.billing[JOB].usage.as_ref().unwrap();
        assert_eq!(usage.outcome, UsageOutcome::Exit);
        assert_eq!(usage.egress_bytes, None);
        assert_eq!(usage.oom_kill, None);
        assert_eq!(
            host.snapshot.per_job.executions[JOB]
                .evidence
                .as_ref()
                .unwrap()
                .winner,
            Some(Winner::Guest)
        );
    }
    fake.exits.lock().unwrap().push(exit(JOB, 3));
    runtime.tick_jobs(unix_ms() + 100_000).unwrap();
    assert_eq!(body(&runtime, JOB), original);
    let host = runtime.host.lock().unwrap();
    let requests: Vec<_> = host
        .snapshot
        .outbox
        .iter()
        .filter(|r| r.job.as_deref() == Some(JOB))
        .collect();
    assert_eq!(requests.len(), 1);
    assert_eq!(requests[0].body, original);
    assert_eq!(
        fake.acknowledgements.lock().unwrap().as_slice(),
        &[exit(JOB, 3)]
    );
}

#[tokio::test]
async fn host_stop_client_result_does_not_wait_for_usage_or_exit_delivery() {
    let (runtime, fake, storage) = fixture();
    running(&runtime, JOB, true, true);
    runtime
        .host
        .lock()
        .unwrap()
        .job_host_stop(JOB, StopCause::HostPressure, 400)
        .unwrap();
    fake.fail_poll.store(true, Ordering::Release);
    let (mut client, mut server) = tokio::io::duplex(8192);
    failed(&runtime, &claims(JOB), &mut server).await.unwrap();
    let frame = read_frame(&mut client).await.unwrap();
    assert_eq!(frame["type"], "terminal");
    assert_eq!(frame["outcome"]["type"], "signal");
    assert!(runtime.host.lock().unwrap().snapshot.billing[JOB]
        .usage
        .is_none());
    assert!(storage.held.lock().unwrap().contains(JOB));
    fake.fail_poll.store(false, Ordering::Release);
    runtime.tick_jobs(500).unwrap();
    assert_eq!(
        runtime.host.lock().unwrap().snapshot.billing[JOB]
            .usage
            .as_ref()
            .unwrap()
            .egress_bytes,
        None
    );
    assert!(!storage.held.lock().unwrap().contains(JOB));
}
