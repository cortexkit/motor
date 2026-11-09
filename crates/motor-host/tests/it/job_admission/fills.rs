use super::*;
use motor_host::{
    helper::Control,
    jobs::{
        fills::{Candidate, FillGuest, Publication, Scheduler},
        Assignment, Resources,
    },
};
use serde_json::{json, Value};
use std::sync::{
    atomic::{AtomicUsize, Ordering},
    Arc, Mutex,
};

fn pins() -> Vec<motor_protocol::MirrorRef> {
    vec![motor_protocol::MirrorRef {
        repository: Some("repo".into()),
        dependency: None,
        reference: "refs/motor/snapshot/one".into(),
        commit: "a".repeat(40),
    }]
}

#[derive(Default)]
struct Helper {
    starts: Mutex<Vec<motor_vm_helper::job::Start>>,
    finishes: Mutex<Vec<bool>>,
    seeds: Mutex<Vec<motor_vm_helper::job::Seed>>,
}
impl Control for Helper {
    fn prepare_fill(&self, fill: &motor_vm_helper::job::Fill) -> anyhow::Result<()> {
        fill.validate()?;
        Ok(())
    }
    fn start_job(&self, start: &motor_vm_helper::job::Start) -> anyhow::Result<Value> {
        self.starts.lock().unwrap().push(start.clone());
        Ok(json!({"tap_index":7}))
    }
    fn finish_fill(&self, _: &motor_vm_helper::job::Fill, published: bool) -> anyhow::Result<bool> {
        self.finishes.lock().unwrap().push(published);
        Ok(published)
    }
    fn seed_workspace(&self, seed: &motor_vm_helper::job::Seed) -> anyhow::Result<()> {
        self.seeds.lock().unwrap().push(seed.clone());
        Ok(())
    }
    fn job_exits(&self) -> anyhow::Result<Vec<motor_vm_helper::job::ExitReport>> {
        Ok(self
            .starts
            .lock()
            .unwrap()
            .iter()
            .map(|s| motor_vm_helper::job::ExitReport {
                job_id: s.job_id.clone(),
                oom_kill: 0,
                egress_bytes: Some(999_999),
            })
            .collect())
    }
    fn ack_job_exit(&self, _: &motor_vm_helper::job::ExitReport) -> anyhow::Result<()> {
        Ok(())
    }
    fn adopt(&self, _: &str) -> anyhow::Result<bool> {
        unreachable!()
    }
    fn wake(&self, _: &str, _: Option<u64>) -> anyhow::Result<u64> {
        unreachable!()
    }
    fn quota(&self, _: &str, _: u32) -> anyhow::Result<()> {
        unreachable!()
    }
    fn relay(&self, _: &str, _: u32) -> anyhow::Result<std::os::unix::net::UnixStream> {
        unreachable!()
    }
    fn admin(&self, _: &str, _: &Value) -> anyhow::Result<Value> {
        unreachable!()
    }
    fn suspend(&self, _: &str) -> anyhow::Result<()> {
        unreachable!()
    }
    fn available_bytes(&self) -> anyhow::Result<u64> {
        Ok(u64::MAX)
    }
}
struct Images;
impl Resources for Images {
    fn acquire_fill(
        &self,
        _: &motor_host::state::jobs::fills::Reservation,
    ) -> anyhow::Result<Assignment> {
        Ok(Assignment {
            image_generation_id: "image".into(),
            hello: json!({}),
            guest_init: json!({}),
            spool: "unused".into(),
        })
    }
    fn acquire(&self, _: &motor_host::state::jobs::Execution) -> anyhow::Result<Assignment> {
        unreachable!()
    }
    fn prepare(&self, _: &motor_host::state::jobs::Execution) -> anyhow::Result<()> {
        unreachable!()
    }
    fn release(&self, _: &str) -> anyhow::Result<()> {
        Ok(())
    }
}
struct Guest {
    success: bool,
    calls: AtomicUsize,
}
impl FillGuest for Guest {
    fn candidates(&self) -> anyhow::Result<Vec<Candidate>> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        Ok(vec![
            Candidate {
                account: job(JOB).sub,
                repository_key: "older".into(),
                mem_mib: 100,
                last_active_ms: 1,
            },
            Candidate {
                account: job(JOB).sub,
                repository_key: "recent".into(),
                mem_mib: 100,
                last_active_ms: 2,
            },
        ])
    }
    fn initialize(
        &self,
        _: &motor_host::state::jobs::fills::Reservation,
        _: &Assignment,
        _: &dyn Control,
        _: &Value,
    ) -> anyhow::Result<()> {
        Ok(())
    }
    fn run(
        &self,
        _: &motor_host::state::jobs::fills::Reservation,
        _: &dyn Control,
    ) -> anyhow::Result<Publication> {
        anyhow::ensure!(self.success, "fill command failed");
        Ok(Publication { have: pins() })
    }
}

#[test]
fn fills_debit_fixed_samples_advertise_capacity_and_are_not_preempted_or_billed() {
    let mut h = memory_host(400);
    let fill = h
        .reserve_fill(&job(JOB).sub, "repo", 100, AT)
        .unwrap()
        .unwrap();
    assert_eq!(fill.vcpus, 2);
    assert_eq!(fill.memory_debit_mib, 120);
    assert_eq!(h.job_free_mem_mib(AT), 230);
    let memory = h.free_mem_mib(AT);
    let heartbeat = serde_json::to_value(h.heartbeat(AT, memory, true)).unwrap();
    assert_eq!(heartbeat["free_vcpus"], 2);
    h.claim(job(JOB), AT).unwrap();
    full(h.claim(job(JOB2), AT));
    assert_eq!(h.snapshot.per_job.fills.len(), 1);
    h.fill_runner_hello(&fill.fill.fill_id).unwrap();
    assert_eq!(h.job_free_mem_mib(AT), 230);
    h.fill_publication(&fill.fill.fill_id, pins()).unwrap();
    h.fill_finished(&fill.fill.fill_id, true, AT).unwrap();
    assert!(!h.snapshot.leases.contains_key(&fill.fill.fill_id));
    assert!(!h
        .snapshot
        .per_job
        .executions
        .contains_key(&fill.fill.fill_id));
    assert!(h.snapshot.outbox.is_empty());
    assert!(h.snapshot.billing.is_empty());
    assert!(h.snapshot.zero_usage.is_empty());
}

#[test]
fn fills_wait_for_queued_claims_and_capacity_refusals_then_require_fresh_memory() {
    let mut h = memory_host(170);
    let mut oversized = job(JOB);
    oversized.mem_mib = 101;
    full(h.claim(oversized, AT));
    assert!(h
        .reserve_fill(&job(JOB).sub, "repo", 100, AT)
        .unwrap()
        .is_none());
    let later = AT
        + motor_host::state::MEMORY_FRESH_MS.max(motor_host::state::jobs::fills::CAPACITY_QUIET_MS)
        + 1;
    assert!(h
        .reserve_fill(&job(JOB).sub, "repo", 100, later)
        .unwrap()
        .is_none());
    h.record_host_resources(HostResources {
        at_ms: later,
        available_mem_mib: 170,
        vcpu_ceiling: 2,
        permit_budget: 4,
    });
    let fill = h
        .reserve_fill(&job(JOB).sub, "repo", 100, later)
        .unwrap()
        .unwrap();
    h.fill_finished(&fill.fill.fill_id, false, later).unwrap();
    let mut queued = job(JOB2);
    queued.exp += 100;
    h.claim(queued, later).unwrap();
    assert!(h
        .reserve_fill(&job(JOB).sub, "repo", 1, later)
        .unwrap()
        .is_none());
}

#[test]
fn scheduler_uses_recent_repository_and_success_have_but_discards_failures_and_never_bills_traffic()
{
    for success in [true, false] {
        let host = Arc::new(Mutex::new(memory_host(10_000)));
        let helper = Arc::new(Helper::default());
        let mut scheduler = Scheduler::new(Arc::new(Guest {
            success,
            calls: AtomicUsize::new(0),
        }));
        scheduler
            .tick(host.clone(), helper.clone(), Arc::new(Images), AT)
            .unwrap();
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
        while !host.lock().unwrap().snapshot.per_job.fills.is_empty() {
            assert!(
                std::time::Instant::now() < deadline,
                "fill worker did not finish"
            );
            scheduler
                .tick(host.clone(), helper.clone(), Arc::new(Images), AT)
                .unwrap();
            std::thread::yield_now();
        }
        let starts = helper.starts.lock().unwrap();
        assert_eq!(starts.len(), 1);
        assert_eq!(starts[0].kind, motor_vm_helper::job::VmKind::Fill);
        assert_eq!(starts[0].account, job(JOB).sub);
        assert_eq!(*helper.finishes.lock().unwrap(), [success]);
        let h = host.lock().unwrap();
        assert!(h.snapshot.outbox.is_empty());
        assert!(h.snapshot.billing.is_empty());
        assert!(h.snapshot.zero_usage.is_empty());
        assert!(h.snapshot.leases.is_empty());
        assert!(h.snapshot.per_job.executions.is_empty());
        if success {
            let base = h.snapshot.per_job.bases.values().next().unwrap();
            assert_eq!(base.repository_key.as_deref(), Some("recent"));
            assert_eq!(base.have, pins());
        } else {
            assert!(h.snapshot.per_job.bases.is_empty());
        }
    }
}

#[test]
fn new_disks_are_empty_and_seeded_disks_start_with_only_the_same_accounts_base_have() {
    let mut h = memory_host(10_000);
    let fill = h
        .reserve_fill(&job(JOB).sub, "repo", 100, AT)
        .unwrap()
        .unwrap();
    h.fill_publication(&fill.fill.fill_id, pins()).unwrap();
    h.fill_finished(&fill.fill.fill_id, true, AT).unwrap();
    let helper = Arc::new(Helper::default());
    h.set_job_storage(helper.clone());
    h.claim(job(JOB), AT).unwrap();
    let disk = h.snapshot.per_job.executions[JOB].disk_id.clone();
    assert!(h.snapshot.per_job.disks[&disk].have.is_empty());
    assert!(!h.seed_job_from_base(JOB, "other-repo", AT).unwrap());
    assert!(h.seed_job_from_base(JOB, "repo", AT).unwrap());
    assert_eq!(h.snapshot.per_job.disks[&disk].have, pins());
    assert_eq!(helper.seeds.lock().unwrap()[0].base_id, fill.fill.disk_id());
    let mut other = job(JOB2);
    other.sub = "other-account".into();
    h.claim(other, AT).unwrap();
    assert!(!h.seed_job_from_base(JOB2, "repo", AT).unwrap());
}

#[test]
fn unavailable_guest_is_disabled_once_and_reserves_nothing() {
    struct Unavailable(AtomicUsize);
    impl FillGuest for Unavailable {
        fn candidates(&self) -> anyhow::Result<Vec<Candidate>> {
            self.0.fetch_add(1, Ordering::SeqCst);
            Err(motor_host::jobs::fills::GuestUnavailable.into())
        }
        fn initialize(
            &self,
            _: &motor_host::state::jobs::fills::Reservation,
            _: &Assignment,
            _: &dyn Control,
            _: &Value,
        ) -> anyhow::Result<()> {
            unreachable!()
        }
        fn run(
            &self,
            _: &motor_host::state::jobs::fills::Reservation,
            _: &dyn Control,
        ) -> anyhow::Result<Publication> {
            unreachable!()
        }
    }
    let host = Arc::new(Mutex::new(memory_host(10_000)));
    let helper = Arc::new(Helper::default());
    let guest = Arc::new(Unavailable(AtomicUsize::new(0)));
    let mut scheduler = Scheduler::new(guest.clone());
    for _ in 0..3 {
        scheduler
            .tick(host.clone(), helper.clone(), Arc::new(Images), AT)
            .unwrap();
    }
    assert!(host.lock().unwrap().snapshot.per_job.is_empty());
    assert!(helper.starts.lock().unwrap().is_empty());
    assert_eq!(guest.0.load(Ordering::SeqCst), 1);
    assert!(motor_host::jobs::fills::FillGuest::candidates(
        &motor_host::jobs::fills::UnavailableFillGuest
    )
    .unwrap_err()
    .to_string()
    .contains("fill guest operation unavailable"));
}

#[test]
fn fill_reservations_and_publication_have_survive_restart_and_legacy_ledgers_load() {
    let legacy: motor_host::state::jobs::Ledger =
        serde_json::from_value(json!({"disks":{},"executions":{}})).unwrap();
    assert!(legacy.fills.is_empty());
    assert!(serde_json::to_value(legacy).unwrap().get("fills").is_none());
    let dir = scratch();
    let id;
    {
        let mut h = durable_host(dir.path(), 10_000);
        let fill = h
            .reserve_fill(&job(JOB).sub, "repo", 100, AT)
            .unwrap()
            .unwrap();
        id = fill.fill.fill_id;
        h.fill_publication(&id, pins()).unwrap();
    }
    let h = durable_host(dir.path(), 10_000);
    assert_eq!(h.snapshot.per_job.fills[&id].publication, Some(pins()));
    assert_eq!(h.job_free_mem_mib(AT), 9830);
}
