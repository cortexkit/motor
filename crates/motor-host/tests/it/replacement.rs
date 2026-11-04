use super::*;
use anyhow::Result;
use motor_host::{
    helper::Control,
    server::Runtime,
    state::{Error, Refusal},
};
use serde_json::{json, Value};

#[derive(Default)]
struct Cancels(Mutex<Vec<String>>);
impl Control for Cancels {
    fn adopt(&self, _: &str) -> Result<bool> {
        Ok(true)
    }
    fn wake(&self, _: &str, _: Option<u64>) -> Result<u64> {
        anyhow::bail!("unexpected wake")
    }
    fn quota(&self, _: &str, _: u32) -> Result<()> {
        Ok(())
    }
    fn relay(&self, _: &str, _: u32) -> Result<std::os::unix::net::UnixStream> {
        anyhow::bail!("unexpected relay")
    }
    fn admin(&self, _: &str, request: &Value) -> Result<Value> {
        if request["op"] == "cancel" {
            self.0
                .lock()
                .unwrap()
                .push(request["job_id"].as_str().unwrap().into());
        }
        Ok(json!({"ok":true}))
    }
    fn suspend(&self, _: &str) -> Result<()> {
        anyhow::bail!("unexpected suspend")
    }
    fn available_bytes(&self) -> Result<u64> {
        Ok(u64::MAX)
    }
}

fn ignored(h: &mut Host, job: &str) {
    let ms = NOW * 1000;
    h.queue_heartbeat("host", &h.heartbeat(ms, 100_000, true), ms)
        .unwrap();
    let request = h.snapshot.outbox.last().unwrap().clone();
    assert!(h
        .finish_request(
            &request,
            200,
            &json!({"ignored":[job], "closed_subjects":[]})
        )
        .unwrap());
}

fn revoked(h: &mut Host, job: &str, until_ms: u64) {
    let ms = NOW * 1000;
    h.queue_heartbeat("host", &h.heartbeat(ms, 100_000, true), ms)
        .unwrap();
    let request = h.snapshot.outbox.last().unwrap().clone();
    assert!(h
        .finish_request(
            &request,
            200,
            &json!({
                "ignored":[], "closed_subjects":[],
                "revoked":[{"job_id":job,"until_ms":until_ms.to_string()}]
            })
        )
        .unwrap());
}

#[test]
fn unseen_revocation_survives_sqlite_reopen_and_refuses_admission() {
    let scratch = scratch();
    let path = scratch.path().join("journal");
    let until = (NOW + 300) * 1000;
    {
        let mut h = disk_host(&path);
        revoked(&mut h, JOB, until);
        assert!(h.snapshot.leases.is_empty());
    }
    let mut h = disk_host(&path);
    assert_eq!(h.snapshot.revocations[JOB].until_ms, until);
    assert_eq!(h.heartbeat(NOW * 1000, 100_000, true).revoked_ack, [JOB]);
    assert!(matches!(
        h.claim(claims(JOB), NOW * 1000),
        Err(Error::Refused(Refusal::Auth))
    ));
}

#[test]
fn revoked_live_job_cancels_but_acks_only_after_terminal_even_after_restart() {
    let scratch = scratch();
    let path = scratch.path().join("journal");
    let mut h = disk_host(&path);
    super::billing::started(&mut h, JOB, 1);
    revoked(&mut h, JOB, (NOW + 300) * 1000);
    drop(h);
    let control = Arc::new(Cancels::default());
    let runtime = Runtime::new("host".into(), disk_host(&path), control.clone());
    runtime.tick(NOW * 1000 + 100).unwrap();
    assert_eq!(*control.0.lock().unwrap(), [JOB]);
    let mut h = runtime.host.lock().unwrap();
    assert!(h
        .heartbeat(NOW * 1000 + 100, 100_000, true)
        .revoked_ack
        .is_empty());
    h.event(
        "tenant",
        &event(3, TenantEventKind::Terminal, Some(JOB), None),
        NOW * 1000 + 200,
    )
    .unwrap();
    assert_eq!(
        h.heartbeat(NOW * 1000 + 200, 100_000, true).revoked_ack,
        [JOB]
    );
    // A lost ack has no effect on the next heartbeat's acknowledgement.
    revoked(&mut h, JOB, (NOW + 300) * 1000);
    assert_eq!(
        h.heartbeat(NOW * 1000 + 300, 100_000, true).revoked_ack,
        [JOB]
    );
}

#[test]
fn tombstones_outlive_finished_job_retention_and_prune_only_after_expiry() {
    let scratch = scratch();
    let path = scratch.path().join("journal");
    let mut h = disk_host(&path);
    let until = NOW * 1000 + motor_host::billing::RECORD_RETENTION_MS * 2;
    super::billing::started(&mut h, JOB, 1);
    revoked(&mut h, JOB, until);
    super::billing::end(
        &mut h,
        JOB,
        3,
        NOW * 1000 + 100,
        super::billing::terminal(JOB, "cancelled"),
    );
    h.terminal_delivered(JOB, NOW * 1000 + 100).unwrap();
    let report = h
        .snapshot
        .outbox
        .iter()
        .find(|r| r.job.as_deref() == Some(JOB))
        .unwrap()
        .clone();
    assert!(h
        .finish_request(
            &report,
            200,
            &json!({"job_id":JOB,"settled_vcpu_ms":100,"released_vcpu_ms":0})
        )
        .unwrap());
    assert_eq!(h.prune_retention(until - 1).unwrap(), 1);
    assert!(!h.snapshot.leases.contains_key(JOB));
    drop(h);
    let mut h = disk_host(&path);
    assert!(h.snapshot.revocations.contains_key(JOB));
    h.retention_tick(until).unwrap();
    assert!(h.snapshot.revocations.contains_key(JOB));
    h.retention_tick(until + 1).unwrap();
    assert!(h.snapshot.revocations.is_empty());
}

#[test]
fn full_tombstone_store_never_evicts_or_acks_a_new_job_and_ack_pages_advance() {
    use motor_host::{billing::REVOCATION_CAP, state::Revocation};
    let mut h = host();
    let until = (NOW + 300) * 1000;
    for n in 0..REVOCATION_CAP {
        h.snapshot.revocations.insert(
            format!("01900000-0000-7000-8000-{n:012x}"),
            Revocation {
                until_ms: until,
                listed: true,
            },
        );
    }
    assert_eq!(
        h.heartbeat(NOW * 1000, 100_000, true).revoked_ack.len(),
        256
    );
    revoked(&mut h, JOB, until);
    assert_eq!(h.snapshot.revocations.len(), REVOCATION_CAP);
    assert!(!h.snapshot.revocations.contains_key(JOB));
    assert!(h
        .heartbeat(NOW * 1000, 100_000, true)
        .revoked_ack
        .is_empty());
    let later_page = "01900000-0000-7000-8000-000000000fff";
    revoked(&mut h, later_page, until + 1);
    assert_eq!(h.snapshot.revocations[later_page].until_ms, until + 1);
    assert_eq!(
        h.heartbeat(NOW * 1000, 100_000, true).revoked_ack,
        [later_page]
    );
    h.retention_tick(until + 2).unwrap();
    revoked(&mut h, JOB, until + 100);
    assert_eq!(h.snapshot.revocations.len(), 1);
}

#[test]
fn malformed_revocation_reply_confirms_without_new_fences_and_recovers_on_next_reply() {
    let mut h = host();
    h.queue_heartbeat("host", &h.heartbeat(NOW * 1000, 100_000, true), NOW * 1000)
        .unwrap();
    let request = h.snapshot.outbox.last().unwrap().clone();
    assert!(h
        .finish_request(
            &request,
            200,
            &json!({"ignored":[],"closed_subjects":[],"revoked":null})
        )
        .unwrap());
    assert!(h.snapshot.revocations.is_empty());
    assert!(h
        .heartbeat(NOW * 1000, 100_000, true)
        .revoked_ack
        .is_empty());
    assert!(h.snapshot.outbox.is_empty());
    revoked(&mut h, JOB, (NOW + 300) * 1000);
    assert_eq!(h.heartbeat(NOW * 1000, 100_000, true).revoked_ack, [JOB]);
}

#[test]
fn heartbeat_ignored_running_job_is_cancelled_even_after_restart() {
    let scratch = scratch();
    let path = scratch.path().join("journal");
    {
        let mut h = disk_host(&path);
        super::billing::started(&mut h, JOB, 1);
        ignored(&mut h, JOB);
    }
    let h = disk_host(&path);
    assert_eq!(h.snapshot.pending_cancels[JOB], "tenant");
    let control = Arc::new(Cancels::default());
    let runtime = Runtime::new("host".into(), h, control.clone());
    runtime.tick(NOW * 1000 + 100).unwrap();
    assert_eq!(*control.0.lock().unwrap(), [JOB]);
    assert!(runtime
        .host
        .lock()
        .unwrap()
        .snapshot
        .pending_cancels
        .is_empty());
    assert!(matches!(
        runtime
            .host
            .lock()
            .unwrap()
            .claim(claims(JOB), NOW * 1000 + 200),
        Err(Error::Refused(Refusal::Auth))
    ));
}

#[test]
fn heartbeat_ignored_reserved_job_refuses_old_token_and_inflight_delivery() {
    let scratch = scratch();
    let path = scratch.path().join("journal");
    {
        let mut h = disk_host(&path);
        let (connection, _) = h.claim(claims(JOB), NOW * 1000).unwrap();
        ignored(&mut h, JOB);
        assert!(matches!(h.pending(JOB), Err(Error::Refused(Refusal::Auth))));
        assert!(matches!(
            h.delivered(connection),
            Err(Error::Refused(Refusal::Auth))
        ));
    }
    let mut h = disk_host(&path);
    assert!(matches!(
        h.claim(claims(JOB), NOW * 1000 + 100),
        Err(Error::Refused(Refusal::Auth))
    ));
    let mut newer = claims(JOB);
    newer.iat += 1;
    newer.exp += 1;
    assert!(matches!(
        h.claim(newer, NOW * 1000 + 1000),
        Err(Error::Refused(Refusal::Auth))
    ));
    assert!(h.claim(claims(JOB2), NOW * 1000 + 1000).is_ok());
}
