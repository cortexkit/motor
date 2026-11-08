use super::*;
use motor_host::billing::LOST_GUEST_CHECK_MS;

const LOST: u64 = MS + 330_000;

fn reply(job: &str) -> Value {
    json!({"ok":true,"job_id":job,"state":"running","terminal":null,
        "unit":"motor-job-test.service","unit_alive":true})
}

fn checking() -> Host {
    let mut h = host();
    started(&mut h, JOB, 1);
    h.billing_tick(LOST).unwrap();
    assert!(!h.snapshot.billing[JOB].host_settled);
    assert!(!h.snapshot.vms["tenant"].stop_pending);
    assert!(
        h.lost_checks_due().is_empty(),
        "hard cancel must precede inspection"
    );
    assert_eq!(
        h.snapshot.pending_cancels.get(JOB).map(String::as_str),
        Some("tenant")
    );
    h.cancel_delivered(JOB).unwrap();
    assert_eq!(h.lost_checks_due(), vec![("tenant".into(), JOB.into())]);
    assert!(
        h.lost_checks_due().is_empty(),
        "one inspection per lost lease"
    );
    h
}

#[test]
fn guest_confirmed_ended_keeps_admitting_with_identical_lost_billing() {
    let mut fallback = checking();
    fallback.billing_tick(LOST + LOST_GUEST_CHECK_MS).unwrap();
    for proof in ["terminal_record", "unit_confirmed_gone"] {
        let mut h = checking();
        let mut status = reply(JOB);
        if proof == "terminal_record" {
            status["terminal"] = terminal(JOB, "exit");
            status["terminal"]["execution_ended"] = json!(true);
        } else {
            status["unit_alive"] = json!(false);
        }
        let logs = Logs::default();
        let captured = logs.0.clone();
        tracing::subscriber::with_default(logs, || {
            h.lost_check_reply(JOB, status);
            h.billing_tick(LOST + 1).unwrap();
        });
        assert!(h.snapshot.billing[JOB].host_settled);
        assert_eq!(h.snapshot.billing[JOB].restart_pending, Some(false));
        assert!(!h.snapshot.vms["tenant"].stop_pending);
        assert!(h.stops_due().is_empty());
        h.billing_tick(LOST + 60_000).unwrap();
        assert_eq!(
            h.snapshot.billing[JOB].usage,
            fallback.snapshot.billing[JOB].usage
        );
        assert_eq!(
            h.snapshot.billing[JOB].terminal,
            fallback.snapshot.billing[JOB].terminal
        );
        assert_eq!(
            h.snapshot.billing[JOB].hosted_terminal,
            fallback.snapshot.billing[JOB].hosted_terminal
        );
        assert_eq!(
            h.snapshot.billing[JOB].hosted_terminal.as_ref().unwrap()["billed_vcpu_ms"],
            30_000
        );
        let usage = h.snapshot.billing[JOB].usage.as_ref().unwrap();
        assert_eq!(
            (usage.outcome, usage.ended_at_ms - usage.started_at_ms),
            (UsageOutcome::Lost, 30_000)
        );
        let mut claim = claims(JOB2);
        claim.iat += 300;
        claim.exp += 300;
        assert!(h.claim(claim, LOST + 1).is_ok());
        assert!(captured.lock().unwrap().iter().any(|line| line
            .contains("lease_settled_lost_guest_confirmed_ended")
            && line.contains(JOB)
            && line.contains(proof)));
    }
}

#[test]
fn guest_still_running_after_hard_cancel_requires_restart() {
    let mut h = checking();
    h.lost_check_reply(JOB, reply(JOB));
    h.billing_tick(LOST + 1).unwrap();
    assert!(h.snapshot.billing[JOB].host_settled);
    assert!(h.snapshot.vms["tenant"].stop_pending);
    assert_eq!(h.snapshot.billing[JOB].restart_pending, Some(true));
}

#[test]
fn guest_silent_old_absent_or_malformed_requires_restart_after_bound() {
    let mut absent = reply(JOB);
    absent["state"] = json!("not_found");
    absent["unit"] = Value::Null;
    absent["unit_alive"] = Value::Null;
    let mut no_unit = absent.clone();
    no_unit["unit_alive"] = json!(false);
    let mut unproven_terminal = absent.clone();
    unproven_terminal["terminal"] = terminal(JOB, "outcome_unknown");
    unproven_terminal["terminal"]["outcome"] = json!({"type":"outcome_unknown"});
    for status in [
        None,
        Some(json!({"ok":false,"error":"unknown_operation"})),
        Some(json!({"ok":true})),
        Some(absent),
        Some(no_unit),
        Some(reply(JOB2)),
        Some(unproven_terminal),
    ] {
        let mut h = checking();
        if let Some(status) = status {
            h.lost_check_reply(JOB, status);
        }
        h.billing_tick(LOST + LOST_GUEST_CHECK_MS - 1).unwrap();
        assert!(!h.snapshot.billing[JOB].host_settled);
        assert!(!h.snapshot.vms["tenant"].stop_pending);
        h.billing_tick(LOST + LOST_GUEST_CHECK_MS).unwrap();
        assert!(h.snapshot.billing[JOB].host_settled);
        assert!(h.snapshot.vms["tenant"].stop_pending);
        let mut late = reply(JOB);
        late["unit_alive"] = json!(false);
        h.lost_check_reply(JOB, late);
        h.billing_tick(LOST + LOST_GUEST_CHECK_MS + 1).unwrap();
        assert!(
            h.snapshot.vms["tenant"].stop_pending,
            "late inspection cannot undo fallback"
        );
    }
}

#[test]
fn guest_ended_claim_cannot_raise_tenant_capacity_ceiling() {
    let mut h = host();
    h.tenants.get_mut("vm").unwrap().vcpu_ceiling = 2;
    h.tenants.get_mut("vm").unwrap().max_weight = Some(2);
    h.guest_ready("tenant", 1).unwrap();
    let normal = h.heartbeat(MS, 0, true).free_vcpus;
    assert_eq!(normal, 2);
    started(&mut h, JOB, 1);
    h.billing_tick(LOST).unwrap();
    assert!(!h.snapshot.billing[JOB].host_settled);
    h.cancel_delivered(JOB).unwrap();
    assert_eq!(h.lost_checks_due().len(), 1);
    let mut status = reply(JOB);
    status["unit_alive"] = json!(false);
    status["free_vcpus"] = json!(1000);
    status["vcpu_ceiling"] = json!(1000);
    h.lost_check_reply(JOB, status.clone());
    h.billing_tick(LOST + 1).unwrap();
    h.lost_check_reply(JOB, status);
    assert!(!h.snapshot.vms["tenant"].stop_pending);
    assert_eq!(h.heartbeat(LOST + 1, 0, true).free_vcpus, normal);
    let mut claim = claims(JOB2);
    claim.vcpus = 2;
    claim.iat += 300;
    claim.exp += 300;
    h.claim(claim, LOST + 1).unwrap();
    let mut excess = claims(JOB3);
    excess.iat += 300;
    excess.exp += 300;
    assert!(matches!(
        h.claim(excess, LOST + 1),
        Err(motor_host::state::Error::Refused(
            motor_host::state::Refusal::HostFull
        ))
    ));
    assert_eq!(h.heartbeat(LOST + 1, 0, true).free_vcpus, 0);
}

#[test]
fn runtime_sends_cancel_before_inspection_and_does_not_block_on_silent_guest() {
    let mut h = host();
    started(&mut h, JOB, 1);
    started(&mut h, JOB2, 3);
    h.snapshot.leases.get_mut(JOB2).unwrap().claims.max_s = 600;
    let (control, _) = DiagnosticsControl::new(None, false);
    let weak = Arc::downgrade(&control);
    let (arrived, received) = std::sync::mpsc::channel();
    let (release, released) = std::sync::mpsc::channel();
    *control.job_status_hook.lock().unwrap() = Some(Box::new(move |request| {
        assert_eq!(request, &json!({"op":"job_status","job_id":JOB}));
        assert!(weak
            .upgrade()
            .unwrap()
            .cancels
            .lock()
            .unwrap()
            .iter()
            .any(|job| job == JOB));
        arrived.send(()).unwrap();
        let _ = released.recv_timeout(std::time::Duration::from_secs(5));
        json!({"ok":false,"error":"unsupported"})
    }));
    let runtime = motor_host::server::Runtime::new("host".into(), h, control);
    runtime.tick(LOST).unwrap();
    received
        .recv_timeout(std::time::Duration::from_secs(5))
        .unwrap();
    assert!(!runtime.host.lock().unwrap().snapshot.vms["tenant"].stop_pending);
    runtime.tick(LOST + LOST_GUEST_CHECK_MS - 1).unwrap();
    assert!(!runtime.host.lock().unwrap().snapshot.vms["tenant"].stop_pending);
    runtime.tick(LOST + LOST_GUEST_CHECK_MS).unwrap();
    assert!(runtime.host.lock().unwrap().snapshot.vms["tenant"].stop_pending);
    release.send(()).unwrap();
    assert_eq!(
        motor_host::helper::control_read_deadline(&json!({"op":"job_status"})),
        std::time::Duration::from_millis(LOST_GUEST_CHECK_MS)
    );
}

#[test]
fn failed_cancel_journal_write_cannot_authorize_guest_inspection() {
    struct Failed;
    impl motor_host::journal::Journal for Failed {
        fn store(&mut self, _: &motor_host::state::Snapshot) -> std::io::Result<()> {
            Err(std::io::Error::other("injected fsync failure"))
        }
    }
    let mut h = host();
    started(&mut h, JOB, 1);
    h.billing_tick(MS + 60_000).unwrap();
    h.cancel_delivered(JOB).unwrap();
    h.replace_journal(Box::new(Failed));
    assert!(h.billing_tick(LOST).is_err());
    assert!(h.lost_checks_due().is_empty());
    h.replace_journal(Box::<MemoryJournal>::default());
    h.billing_tick(LOST + 1).unwrap();
    assert!(h.snapshot.pending_cancels.contains_key(JOB));
    assert!(h.lost_checks_due().is_empty());
}
