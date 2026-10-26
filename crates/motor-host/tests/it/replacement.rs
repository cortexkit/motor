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
