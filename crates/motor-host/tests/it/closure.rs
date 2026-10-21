use super::billing::{end, started, terminal};
use super::*;
use anyhow::{bail, Result};
use motor_host::closure::{closed_subjects, REMOVAL_OVERDUE_MS, RUNNER_RETENTION_MS};
use motor_host::state::{Error, Refusal};
use motor_host::{helper::Control, server::Runtime};
use serde_json::{json, Value};

const MS: u64 = NOW * 1000;
/// A well-formed subject that no tenant on this host is bound to.
const STRANGER: &str = "acct:01BX5ZZKBKACTAV9WEVGEMMVRZ";

/// Records the helper calls a removal makes; `fail_removals` refusals first.
#[derive(Default)]
struct Helper {
    calls: Mutex<Vec<String>>,
    fail_removals: std::sync::atomic::AtomicUsize,
}
impl Control for Helper {
    fn adopt(&self, _: &str) -> Result<bool> {
        Ok(false)
    }
    fn wake(&self, _: &str, _: Option<u64>) -> Result<u64> {
        bail!("unexpected wake")
    }
    fn quota(&self, _: &str, _: u32) -> Result<()> {
        Ok(())
    }
    fn relay(&self, _: &str, _: u32) -> Result<std::os::unix::net::UnixStream> {
        bail!("unexpected relay")
    }
    fn admin(&self, _: &str, _: &Value) -> Result<Value> {
        Ok(json!({"ok":true}))
    }
    fn suspend(&self, _: &str) -> Result<()> {
        bail!("unexpected suspend")
    }
    fn available_bytes(&self) -> Result<u64> {
        Ok(u64::MAX)
    }
    fn stop(&self, tenant: &str) -> Result<()> {
        self.calls.lock().unwrap().push(format!("stop {tenant}"));
        Ok(())
    }
    fn remove(&self, tenant: &str) -> Result<String> {
        self.calls.lock().unwrap().push(format!("remove {tenant}"));
        let failing = &self.fail_removals;
        if failing.load(std::sync::atomic::Ordering::SeqCst) > 0 {
            failing.fetch_sub(1, std::sync::atomic::Ordering::SeqCst);
            bail!("helper refused remove_tenant: provisioning is in progress")
        }
        Ok(format!("/var/lib/motor-vm/removed/{tenant}-1"))
    }
}

/// Waits for the removal thread a tick started to record its outcome.
fn settle(runtime: &Runtime, done: impl Fn(&Host) -> bool) {
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
    while !done(&runtime.host.lock().unwrap()) {
        assert!(
            std::time::Instant::now() < deadline,
            "removal did not finish"
        );
        std::thread::sleep(std::time::Duration::from_millis(5));
    }
}

fn sub() -> String {
    claims(JOB).sub
}

#[test]
fn closed_tenant_is_removed_only_after_its_last_lease_ends_and_the_runner_retention_passes() {
    let mut h = host();
    started(&mut h, JOB, 1);
    assert_eq!(h.close_subjects(&[sub()], MS).unwrap(), vec!["tenant"]);
    // Reported again by the next heartbeat: nothing changes.
    assert!(h.close_subjects(&[sub()], MS + 15_000).unwrap().is_empty());
    assert_eq!(h.snapshot.closed["tenant"].since_ms, MS);
    // No new job is admitted for the closed account.
    assert!(matches!(
        h.claim(claims(JOB2), MS + 1_000),
        Err(Error::Refused(Refusal::Auth))
    ));
    // The lease is still running: nothing is removed, however long it runs.
    assert!(h
        .removals_due(MS + RUNNER_RETENTION_MS * 2)
        .unwrap()
        .is_empty());
    end(&mut h, JOB, 3, MS + 3_000, terminal(JOB, "exit"));
    // While the job's client connection is still open, nothing is removed.
    assert!(h
        .removals_due(MS + 3_000 + RUNNER_RETENTION_MS)
        .unwrap()
        .is_empty());
    h.close(1).unwrap();
    // Ended, but the runner still keeps the result for a day.
    assert!(h
        .removals_due(MS + 3_000 + RUNNER_RETENTION_MS - 1)
        .unwrap()
        .is_empty());
    assert_eq!(
        h.removals_due(MS + 3_000 + RUNNER_RETENTION_MS).unwrap(),
        vec!["tenant"]
    );

    let helper = Arc::new(Helper::default());
    let runtime = Runtime::new("host".into(), h, helper.clone());
    runtime.tick(MS + 3_000 + RUNNER_RETENTION_MS).unwrap();
    settle(&runtime, |h| h.is_removed("tenant"));
    assert_eq!(
        *helper.calls.lock().unwrap(),
        ["stop tenant", "remove tenant"]
    );
    let mut h = runtime.host.lock().unwrap();
    let closed = &h.snapshot.closed["tenant"];
    assert_eq!(closed.removed_ms, Some(MS + 3_000 + RUNNER_RETENTION_MS));
    assert_eq!(
        closed.quarantine.as_deref(),
        Some("/var/lib/motor-vm/removed/tenant-1")
    );
    // The tenant is forgotten: no token binds to it any more.
    assert!(h.tenants.is_empty());
    assert!(h
        .removals_due(MS + 4 * RUNNER_RETENTION_MS)
        .unwrap()
        .is_empty());
}

#[test]
fn closed_tenant_with_an_open_lease_is_never_removed_by_the_runtime() {
    let mut h = host();
    started(&mut h, JOB, 1);
    h.close_subjects(&[sub()], MS).unwrap();
    // Keep the job inside its own runtime cap, so the watchdog leaves it.
    h.snapshot.leases.get_mut(JOB).unwrap().claims.max_s = u64::MAX / 4_000;
    let helper = Arc::new(Helper::default());
    let runtime = Runtime::new("host".into(), h, helper.clone());
    for now in [
        MS + 1_000,
        MS + RUNNER_RETENTION_MS,
        MS + 2 * RUNNER_RETENTION_MS,
    ] {
        runtime.tick(now).unwrap();
        // A removal thread would have been started by now; give it time.
        std::thread::sleep(std::time::Duration::from_millis(50));
        runtime.remove_closed("tenant", now);
    }
    assert!(helper.calls.lock().unwrap().is_empty());
    assert!(!runtime.host.lock().unwrap().is_removed("tenant"));
}

#[test]
fn unknown_subjects_are_ignored_and_malformed_lists_yield_nothing() {
    let mut h = host();
    assert!(h.close_subjects(&[STRANGER.into()], MS).unwrap().is_empty());
    assert!(h.snapshot.closed.is_empty());
    for reply in [
        Value::Null,
        json!("closed_subjects"),
        json!([sub()]),
        json!({}),
        json!({"closed_subjects": null}),
        json!({"closed_subjects": sub()}),
        json!({"closed_subjects": {"tenant": sub()}}),
        json!({"closed_subjects": [1, null, {"sub": sub()}, ["x"]]}),
        json!({"closed_subjects": ["tenant", "vm", "acct:", "acct:01arz3ndektsv4rrffq69g5fav",
            "acct:01ARZ3NDEKTSV4RRFFQ69G5FAVX", "acct:01ARZ3NDEKTSV4RRFFQ69G5FAI", "user:01ARZ3NDEKTSV4RRFFQ69G5FAV"]}),
    ] {
        assert!(closed_subjects(&reply).is_empty(), "{reply}");
    }
    assert_eq!(
        closed_subjects(
            &json!({"closed_subjects": [7, sub(), STRANGER, "org:01ARZ3NDEKTSV4RRFFQ69G5FAV"]})
        ),
        vec![
            sub(),
            STRANGER.to_owned(),
            "org:01ARZ3NDEKTSV4RRFFQ69G5FAV".to_owned()
        ]
    );
    // A tenant label from the broker is never trusted: only bound subjects map.
    assert!(h
        .close_subjects(
            &closed_subjects(&json!({"closed_subjects": ["tenant"]})),
            MS
        )
        .unwrap()
        .is_empty());
}

#[test]
fn failed_removal_backs_off_retries_and_is_logged_overdue_once_after_seven_days() {
    let mut h = host();
    h.close_subjects(&[sub()], MS).unwrap();
    let helper = Arc::new(Helper::default());
    helper
        .fail_removals
        .store(2, std::sync::atomic::Ordering::SeqCst);
    let runtime = Runtime::new("host".into(), h, helper.clone());
    runtime.remove_closed("tenant", MS);
    {
        let mut h = runtime.host.lock().unwrap();
        let closed = &h.snapshot.closed["tenant"];
        assert_eq!((closed.attempts, closed.retry_at_ms), (1, MS + 60_000));
        assert!(h.removals_due(MS + 59_999).unwrap().is_empty());
        assert_eq!(h.removals_due(MS + 60_000).unwrap(), vec!["tenant"]);
    }
    runtime.remove_closed("tenant", MS + 60_000);
    let late = MS + REMOVAL_OVERDUE_MS;
    {
        let mut h = runtime.host.lock().unwrap();
        // The second failure waits twice as long.
        assert_eq!(h.snapshot.closed["tenant"].retry_at_ms, MS + 180_000);
        assert!(!h.snapshot.closed["tenant"].overdue_logged);
        assert!(h.removals_due(late - 1).is_ok());
        assert!(!h.snapshot.closed["tenant"].overdue_logged);
        assert_eq!(h.removals_due(late).unwrap(), vec!["tenant"]);
        assert!(h.snapshot.closed["tenant"].overdue_logged);
    }
    runtime.remove_closed("tenant", late);
    assert!(runtime.host.lock().unwrap().is_removed("tenant"));
    assert_eq!(helper.calls.lock().unwrap().len(), 6);
}
