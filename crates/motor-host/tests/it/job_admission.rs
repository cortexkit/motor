use super::*;
use motor_host::state::jobs::{guest_ram_mib, memory_limit_mib, HostResources};
use motor_host::state::{Admission, Error, Refusal};
use motor_host::store::SqliteJournal;

const AT: u64 = NOW * 1000;

fn job(job: &str) -> Claims {
    let mut c = claims(job);
    c.mem_mib = 100;
    c.placement.vm = None;
    c
}

fn configure(mut h: Host, available_mem_mib: u64) -> Host {
    let mut settings = host_settings()[1].clone();
    if let motor_host::config::HostSettings::PerJob(settings) = &mut settings {
        settings.host_reserve_mib = 50;
        settings.boot_reserve_mib = 20;
    }
    h.settings = settings;
    sample(&mut h, available_mem_mib);
    h
}

fn sample(h: &mut Host, available_mem_mib: u64) {
    h.record_host_resources(HostResources {
        at_ms: AT,
        available_mem_mib,
        vcpu_ceiling: 2,
        permit_budget: 4,
    });
}

fn memory_host(available: u64) -> Host {
    configure(
        Host::new(
            BTreeMap::new(),
            Snapshot::default(),
            Box::<MemoryJournal>::default(),
        ),
        available,
    )
}

fn durable_host(path: &std::path::Path, available: u64) -> Host {
    let (journal, snapshot) = SqliteJournal::open(path).unwrap();
    configure(
        Host::new(BTreeMap::new(), snapshot, Box::new(journal)),
        available,
    )
}

fn full(result: Result<(u64, Admission), Error>) {
    assert!(
        matches!(result, Err(Error::Refused(Refusal::HostFull))),
        "{result:?}"
    );
}

fn auth(result: Result<(u64, Admission), Error>) {
    assert!(
        matches!(result, Err(Error::Refused(Refusal::Auth))),
        "{result:?}"
    );
}

#[test]
fn disk_identity_uses_account_and_hash_ignoring_cold_and_foreign_hints() {
    let mut h = memory_host(10_000);
    h.claim(job(JOB), AT).unwrap();
    let first = h.snapshot.per_job.executions[JOB].disk_id.clone();
    assert!((1..=64).contains(&first.len()));
    assert!(first
        .bytes()
        .all(|b| b.is_ascii_alphanumeric() || b"._-".contains(&b)));
    h.begin_job_boot(JOB).unwrap().unwrap();
    // A cold placement for an existing pair still queues on the same disk.
    assert_eq!(h.claim(job(JOB2), AT).unwrap().1, Admission::Ready);
    assert_eq!(h.snapshot.per_job.executions[JOB2].disk_id, first);
    assert!(h.begin_job_boot(JOB2).unwrap().is_none());

    let mut other_account = job(JOB3);
    other_account.sub.push_str("-other");
    other_account.placement.vm = Some(first.clone());
    h.claim(other_account.clone(), AT).unwrap();
    let second = h.snapshot.per_job.executions[JOB3].disk_id.clone();
    assert_ne!(first, second);
    h.begin_job_boot(JOB3).unwrap().unwrap();

    // A different workspace in the original account cannot attach either disk.
    let fourth = "00000000-0000-7000-8000-000000000004";
    let mut other_hash = job(fourth);
    other_hash.workspace_key_hash = Some(motor_lease::base64url(&[6; 32]));
    other_hash.placement.vm = Some(second.clone());
    h.claim(other_hash, AT).unwrap();
    assert_ne!(h.snapshot.per_job.executions[fourth].disk_id, first);
    assert_ne!(h.snapshot.per_job.executions[fourth].disk_id, second);
    assert_eq!(h.snapshot.per_job.disks.len(), 3);
    assert_eq!(
        h.snapshot.per_job.disks[&first].attached_job.as_deref(),
        Some(JOB)
    );
    h.job_stopped(JOB, false).unwrap();
    h.begin_job_boot(JOB2).unwrap().unwrap();
    assert_eq!(
        h.snapshot.per_job.disks[&first].attached_job.as_deref(),
        Some(JOB2)
    );
    let fifth = "00000000-0000-7000-8000-000000000005";
    let mut foreign_hint = job(fifth);
    foreign_hint.placement.vm = Some(second);
    h.claim(foreign_hint, AT).unwrap();
    assert_eq!(h.snapshot.per_job.executions[fifth].disk_id, first);
    assert!(h.begin_job_boot(fifth).unwrap().is_none());
}

#[test]
fn reconnect_uses_recorded_disk_and_rejects_another_account_or_hash() {
    let mut h = memory_host(10_000);
    let (id, _) = h.claim(job(JOB), AT).unwrap();
    let first = h.snapshot.per_job.executions[JOB].disk_id.clone();
    h.begin_job_boot(JOB).unwrap().unwrap();
    h.close(id).unwrap();
    let mut reconnect = job(JOB);
    reconnect.placement.vm = Some("unrelated-disk".into());
    let (id, _) = h.claim(reconnect.clone(), AT).unwrap();
    assert_eq!(h.snapshot.per_job.executions[JOB].disk_id, first);
    assert!(h.begin_job_boot(JOB).unwrap().is_none());
    h.close(id).unwrap();

    for purpose in [Purpose::Run, Purpose::Attach] {
        let mut wrong = reconnect.clone();
        wrong.purpose = purpose;
        wrong.sub.push_str("-other");
        auth(h.claim(wrong, AT));
        let mut wrong = reconnect.clone();
        wrong.purpose = purpose;
        wrong.workspace_key_hash = Some(motor_lease::base64url(&[7; 32]));
        auth(h.claim(wrong, AT));
    }
    assert_eq!(h.snapshot.per_job.disks.len(), 1);
    assert_eq!(h.snapshot.per_job.executions.len(), 1);
}

#[test]
fn attach_never_creates_disk_or_execution_live_ended_or_after_restart() {
    let dir = scratch();
    let mut h = durable_host(dir.path(), 10_000);
    let mut unknown = job(JOB2);
    unknown.purpose = Purpose::Attach;
    auth(h.claim(unknown.clone(), AT));
    assert!(h.snapshot.per_job.is_empty());
    let (id, _) = h.claim(job(JOB), AT).unwrap();
    h.begin_job_boot(JOB).unwrap().unwrap();
    h.close(id).unwrap();
    let mut attach = job(JOB);
    attach.purpose = Purpose::Attach;
    attach.placement.vm = Some("deleted-vm".into());
    let before = h.snapshot.clone();
    let (id, _) = h.claim(attach.clone(), AT).unwrap();
    assert_eq!(h.snapshot, before);
    h.close(id).unwrap();
    // job_stopped is called after VM deletion; replay keeps the execution only.
    h.job_stopped(JOB, false).unwrap();
    assert!(h
        .snapshot
        .per_job
        .disks
        .values()
        .all(|disk| disk.attached_job.is_none()));
    let before = h.snapshot.clone();
    h.claim(attach.clone(), AT).unwrap();
    assert_eq!(h.snapshot, before);
    drop(h);
    let mut h = durable_host(dir.path(), 10_000);
    h.claim(attach, AT).unwrap();
    auth(h.claim(unknown, AT));
    assert_eq!(h.snapshot, before);
    assert!(h.begin_job_boot(JOB).unwrap().is_none());
}

#[test]
fn claims_debit_fixed_sample_durably_before_next_claim() {
    let dir = scratch();
    let mut h = durable_host(dir.path(), 289);
    let mut burst = job(JOB);
    burst.mem_burst_mib = Some(50);
    h.claim(burst, AT).unwrap();
    // 289 available - 50 host - (100 stated + 20 boot) = 119, not 169 or 69.
    assert_eq!(h.free_mem_mib(AT), 119);
    assert_eq!(h.snapshot.per_job.executions[JOB].memory_debit_mib, 120);
    full(h.claim(job(JOB2), AT));
    assert_eq!(h.snapshot.per_job.executions.len(), 1);
    let saved = h.snapshot.clone();
    drop(h);
    let mut h = durable_host(dir.path(), 289);
    assert_eq!(h.snapshot, saved);
    assert_eq!(h.free_mem_mib(AT), 119);
    full(h.claim(job(JOB2), AT));
    sample(&mut h, 290);
    h.claim(job(JOB2), AT).unwrap();
    assert_eq!(h.free_mem_mib(AT), 0);
    assert_eq!(h.snapshot.per_job.disks.len(), 1);
    assert_eq!(
        h.snapshot.per_job.disks.values().next().unwrap().queue,
        [JOB, JOB2]
    );
}

#[test]
fn queued_and_booting_debits_end_once_on_hello_not_on_boot() {
    let mut h = memory_host(1_000);
    h.claim(job(JOB), AT).unwrap();
    h.claim(job(JOB2), AT).unwrap();
    assert_eq!(h.free_mem_mib(AT), 710);
    assert!(h.job_runner_hello(JOB2).is_err());
    h.begin_job_boot(JOB).unwrap().unwrap();
    assert_eq!(h.free_mem_mib(AT), 710);
    h.job_runner_hello(JOB).unwrap();
    assert_eq!(h.free_mem_mib(AT), 830);
    h.job_runner_hello(JOB).unwrap();
    assert_eq!(h.free_mem_mib(AT), 830);
    assert!(h.begin_job_boot(JOB2).unwrap().is_none());
    h.job_stopped(JOB, false).unwrap();
    h.begin_job_boot(JOB2).unwrap().unwrap();
    h.job_runner_hello(JOB2).unwrap();
    assert_eq!(h.free_mem_mib(AT), 950);
}

#[test]
fn restart_before_and_after_launch_never_authorizes_a_second_execution() {
    let dir = scratch();
    let mut h = durable_host(dir.path(), 10_000);
    h.claim(job(JOB), AT).unwrap();
    let disk = h.snapshot.per_job.executions[JOB].disk_id.clone();
    drop(h);
    // Crash after admission, before launch: resume the one recorded execution.
    let mut h = durable_host(dir.path(), 10_000);
    h.claim(job(JOB), AT).unwrap();
    let launch = h.begin_job_boot(JOB).unwrap().unwrap();
    assert_eq!(launch.disk_id, disk);
    assert!(launch.boot_started);
    drop(h);
    // Crash after the authorization committed, even before the helper replied:
    // adoption/settlement is required instead of another launch.
    let mut h = durable_host(dir.path(), 10_000);
    h.claim(job(JOB), AT).unwrap();
    assert!(h.begin_job_boot(JOB).unwrap().is_none());
    assert_eq!(h.snapshot.per_job.executions.len(), 1);
    assert_eq!(h.snapshot.per_job.disks.len(), 1);
    h.job_runner_hello(JOB).unwrap();
    drop(h);
    let mut h = durable_host(dir.path(), 10_000);
    assert_eq!(h.free_mem_mib(AT), 9_950);
    assert!(h.begin_job_boot(JOB).unwrap().is_none());
}

#[test]
fn permits_include_overcommit_and_queued_jobs_and_are_released_only_on_stop() {
    let mut h = memory_host(10_000);
    let mut c = job(JOB);
    c.vcpus = 2;
    h.claim(c, AT).unwrap();
    let mut c = job(JOB2);
    c.vcpus = 2;
    h.claim(c, AT).unwrap();
    full(h.claim(job(JOB3), AT));
    h.begin_job_boot(JOB).unwrap().unwrap();
    h.job_runner_hello(JOB).unwrap();
    full(h.claim(job(JOB3), AT));
    h.job_stopped(JOB, false).unwrap();
    h.claim(job(JOB3), AT).unwrap();
    let mut c = job("00000000-0000-7000-8000-000000000004");
    c.vcpus = 3;
    full(h.claim(c, AT));
}

#[test]
fn missing_stale_or_future_memory_samples_refuse_without_reservations() {
    let mut h = memory_host(1_000);
    full(h.claim(job(JOB), AT + motor_host::state::MEMORY_FRESH_MS + 1));
    full(h.claim(job(JOB), AT - 1));
    assert!(h.snapshot.per_job.is_empty());
    let mut h = configure(
        Host::new(
            BTreeMap::new(),
            Snapshot::default(),
            Box::<MemoryJournal>::default(),
        ),
        1000,
    );
    // A restarted host must take a trusted host sample before accepting work.
    let snapshot = h.snapshot.clone();
    h = Host::new(BTreeMap::new(), snapshot, Box::<MemoryJournal>::default());
    h.settings = host_settings()[1].clone();
    full(h.claim(job(JOB), AT));
    assert!(h.snapshot.per_job.is_empty());
}

#[test]
fn quarantine_refuses_new_run_but_not_its_recorded_execution_reconnect() {
    let dir = scratch();
    let mut h = durable_host(dir.path(), 10_000);
    let (id, _) = h.claim(job(JOB), AT).unwrap();
    h.begin_job_boot(JOB).unwrap().unwrap();
    h.close(id).unwrap();
    h.job_stopped(JOB, true).unwrap();
    drop(h);
    let mut h = durable_host(dir.path(), 10_000);
    full(h.claim(job(JOB2), AT));
    assert_eq!(h.claim(job(JOB), AT).unwrap().1, Admission::Ready);
    let mut attach = job(JOB);
    attach.purpose = Purpose::Attach;
    h.claim(attach, AT).unwrap();
    assert!(h.begin_job_boot(JOB).unwrap().is_none());
}

#[test]
fn failed_transaction_publishes_no_identity_queue_debit_or_launch() {
    struct FailedJournal;
    impl Journal for FailedJournal {
        fn store(&mut self, _: &Snapshot) -> std::io::Result<()> {
            Err(std::io::Error::other("injected fsync failure"))
        }
    }
    let mut h = memory_host(1_000);
    h.replace_journal(Box::new(FailedJournal));
    assert!(matches!(h.claim(job(JOB), AT), Err(Error::Journal(_))));
    assert!(h.snapshot.per_job.is_empty());
    assert_eq!(h.free_mem_mib(AT), 950);
    assert!(h.begin_job_boot(JOB).is_err());

    h.replace_journal(Box::<MemoryJournal>::default());
    h.claim(job(JOB), AT).unwrap();
    let before = h.snapshot.clone();
    h.replace_journal(Box::new(FailedJournal));
    assert!(matches!(h.begin_job_boot(JOB), Err(Error::Journal(_))));
    assert_eq!(h.snapshot, before);
    h.replace_journal(Box::<MemoryJournal>::default());
    h.begin_job_boot(JOB).unwrap().unwrap();
    h.replace_journal(Box::new(FailedJournal));
    assert!(h.job_runner_hello(JOB).is_err());
    assert_eq!(h.free_mem_mib(AT), 830);
}

#[test]
fn guest_ram_rounds_limit_up_after_contract_burst_not_admission_debit() {
    for (stated, burst, limit, ram) in [
        (1, None, 1, 2),
        (2, None, 2, 3),
        (3, None, 3, 4),
        (5, Some(2), 7, 10),
        (16_385, Some(8192), 24_577, 32_770),
        (32_768, Some(8192), 40_960, 54_614),
    ] {
        let mut c = job(JOB);
        c.mem_mib = stated;
        c.mem_burst_mib = burst;
        assert_eq!(memory_limit_mib(&c), Some(limit));
        assert_eq!(guest_ram_mib(&c), Some(ram));
        let mut h = memory_host(100_000);
        h.claim(c, AT).unwrap();
        let execution = &h.snapshot.per_job.executions[JOB];
        assert_eq!(execution.guest_ram_mib, ram);
        assert_eq!(execution.memory_debit_mib, stated + 20);
    }
    let mut c = job(JOB);
    c.mem_burst_mib = Some(u64::MAX);
    assert_eq!(memory_limit_mib(&c), Some(150));
    c.mem_mib = u64::MAX;
    assert_eq!(memory_limit_mib(&c), None);
    c.mem_burst_mib = None;
    assert_eq!(guest_ram_mib(&c), None);
}

#[test]
fn hashless_run_and_attach_cannot_enter_job_admission_ledger() {
    let mut h = memory_host(1_000);
    for purpose in [Purpose::Run, Purpose::Attach] {
        let mut c = job(JOB);
        c.purpose = purpose;
        c.workspace_key_hash = None;
        auth(h.claim(c, AT));
    }
    assert!(h.snapshot.leases.is_empty());
    assert!(h.snapshot.per_job.is_empty());
}
