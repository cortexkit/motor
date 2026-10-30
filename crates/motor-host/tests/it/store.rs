use super::{claims, JOB, JOB2, JOB3};
use motor_host::billing::{JobBilling, Request};
use motor_host::closure::ClosedTenant;
use motor_host::journal::{FileJournal, Journal};
use motor_host::state::{Lease, LeaseState, Refusal, Snapshot, Vm};
use motor_host::store::SqliteJournal;
use motor_lease::{Usage, UsageOutcome};
use std::path::Path;

fn scratch() -> tempfile::TempDir {
    tempfile::tempdir_in(cortexkit_test_support::scratch_root()).unwrap()
}

fn lease(job: &str, state: LeaseState) -> Lease {
    Lease {
        claims: claims(job),
        tenant: "tenant".into(),
        state,
        delivered: true,
        replacing: false,
        started_at_ms: Some(1),
        ended_at_ms: (state == LeaseState::Ended).then_some(2),
        highest_seq: 7,
        refusal: None,
    }
}

fn request(job: Option<&str>, created_ms: u64) -> Request {
    Request {
        path: "/v1/hosts/host/usage".into(),
        body: b"{\"usage\":1}".to_vec(),
        created_ms,
        job: job.map(Into::into),
    }
}

/// A snapshot with every field set away from its default, so a field the
/// store forgets to write or read changes the round trip.
fn full() -> Snapshot {
    let mut s = Snapshot::default();
    s.leases.insert(JOB.into(), lease(JOB, LeaseState::Started));
    s.leases.insert(JOB2.into(), lease(JOB2, LeaseState::Ended));
    s.nonces.insert("nonce".into(), 99);
    s.vms.insert(
        "tenant".into(),
        Vm {
            initialized_generation: Some(3),
            stop_pending: true,
            event_seq: 11,
        },
    );
    s.zero_usage.insert(
        JOB3.into(),
        Usage {
            job_id: JOB3.into(),
            vcpus: 2,
            started_at_ms: 0,
            ended_at_ms: 5,
            mem_peak_mib: None,
            egress_bytes: Some(1),
            outcome: UsageOutcome::Lost,
        },
    );
    s.pending_cancels.insert(JOB.into(), "tenant".into());
    s.billing.insert(
        JOB2.into(),
        JobBilling {
            terminal_live: true,
            host_settled: true,
            mem_peak_mib: Some(512),
            terminal: Some(serde_json::json!({"type": "terminal"})),
            ..Default::default()
        },
    );
    s.closed.insert("tenant".into(), ClosedTenant::default());
    s.outbox.push(request(Some(JOB2), 10));
    s.expired_outbox.push(request(Some(JOB3), 4));
    s.rejected_outbox.push(request(None, 3));
    s.counter = 42;
    s.last_alive = 1_800_000_000_000;
    s.draining = true;
    s
}

fn reopen(dir: &Path) -> Snapshot {
    SqliteJournal::open(dir).unwrap().1
}

#[test]
fn sqlite_journal_round_trips_every_field_through_each_change() {
    let dir = scratch();
    let (mut journal, start) = SqliteJournal::open(dir.path()).unwrap();
    assert_eq!(start, Snapshot::default());
    let mut s = full();
    journal.store(&s).unwrap();
    // An update, a removal of a non-job entry, a new job and new values.
    s.leases.get_mut(JOB).unwrap().highest_seq = 8;
    s.pending_cancels.clear();
    s.leases
        .insert(JOB3.into(), lease(JOB3, LeaseState::Reserved));
    s.outbox.clear();
    s.counter = 43;
    s.draining = false;
    journal.store(&s).unwrap();
    // A finished job leaving the host, as the retention pass removes it.
    s.leases.remove(JOB2);
    s.billing.remove(JOB2);
    s.leases.get_mut(JOB).unwrap().refusal = Some(Refusal::Auth);
    journal.store(&s).unwrap();
    drop(journal);
    assert_eq!(reopen(dir.path()), s);
}

#[test]
fn first_open_imports_the_whole_file_journal_once_and_sets_it_aside() {
    let dir = scratch();
    let (mut file, _) = FileJournal::open(dir.path()).unwrap();
    file.store(&full()).unwrap();
    let (_, imported) = SqliteJournal::open(dir.path()).unwrap();
    assert_eq!(imported, full());
    assert!(!dir.path().join("journal.json").exists());
    assert!(dir.path().join("journal.json.imported").exists());
    // A stray file journal appearing later is not imported over the store.
    let (mut file, _) = FileJournal::open(dir.path()).unwrap();
    file.store(&Snapshot::default()).unwrap();
    assert_eq!(reopen(dir.path()), full());
}

#[test]
fn a_removed_job_stays_removed_when_an_older_copy_of_the_database_returns() {
    let dir = scratch();
    let db = dir.path().join("store.db");
    let (mut journal, _) = SqliteJournal::open(dir.path()).unwrap();
    let mut s = full();
    journal.store(&s).unwrap();
    // Closing the only connection checkpoints the write-ahead log into the
    // database file, so the copy holds the finished job.
    drop(journal);
    let older = dir.path().join("older.db");
    std::fs::copy(&db, &older).unwrap();
    let (mut journal, _) = SqliteJournal::open(dir.path()).unwrap();
    s.leases.remove(JOB2);
    s.billing.remove(JOB2);
    journal.store(&s).unwrap();
    drop(journal);
    for suffix in ["-wal", "-shm"] {
        let _ = std::fs::remove_file(dir.path().join(format!("store.db{suffix}")));
    }
    std::fs::rename(&older, &db).unwrap();
    let restored = reopen(dir.path());
    assert!(!restored.leases.contains_key(JOB2));
    assert!(!restored.billing.contains_key(JOB2));
    assert!(restored.leases.contains_key(JOB));
}

#[test]
fn export_writes_the_file_journal_and_the_next_open_imports_it_again() {
    let dir = scratch();
    let (mut journal, _) = SqliteJournal::open(dir.path()).unwrap();
    journal.store(&full()).unwrap();
    drop(journal);
    let written = SqliteJournal::export(dir.path()).unwrap();
    let (_, exported) = FileJournal::open(dir.path()).unwrap();
    assert_eq!(written, dir.path().join("journal.json"));
    assert_eq!(exported, full());
    // What an older binary changes in that file is what the next open serves.
    let mut older = full();
    older.counter = 77;
    older.leases.remove(JOB);
    let (mut file, _) = FileJournal::open(dir.path()).unwrap();
    file.store(&older).unwrap();
    assert_eq!(reopen(dir.path()), older);
}

#[test]
fn a_second_writer_is_refused_while_the_first_holds_the_store() {
    let dir = scratch();
    let (_journal, _) = SqliteJournal::open(dir.path()).unwrap();
    assert!(SqliteJournal::open(dir.path()).is_err());
}

/// Measures one output-sequence update against a copy of a live host's
/// journal, through the whole-file journal and through SQLite. Run by hand:
/// `MOTOR_HOST_STORE_BENCH=<dir holding journal.json> cargo test --release
/// -p motor-host store_cost_on_a_live_journal -- --ignored --nocapture`.
#[test]
#[ignore = "needs a copy of a live journal"]
fn store_cost_on_a_live_journal() {
    let source = std::env::var("MOTOR_HOST_STORE_BENCH").expect("MOTOR_HOST_STORE_BENCH");
    let bytes = std::fs::read(Path::new(&source).join("journal.json")).unwrap();
    let live: Snapshot = serde_json::from_slice(&bytes).unwrap();
    let job = live
        .leases
        .iter()
        .find(|(_, l)| l.state == LeaseState::Started)
        .or_else(|| live.leases.iter().next())
        .map(|(job, _)| job.clone())
        .unwrap();
    const ROUNDS: u64 = 100;
    let bump = |s: &mut Snapshot| s.leases.get_mut(&job).unwrap().highest_seq += 1;
    for (name, mut journal) in [
        (
            "file",
            Box::new(FileJournal::open(scratch().keep().as_path()).unwrap().0) as Box<dyn Journal>,
        ),
        (
            "sqlite",
            Box::new(SqliteJournal::open(scratch().keep().as_path()).unwrap().0),
        ),
    ] {
        let mut s = live.clone();
        journal.store(&s).unwrap();
        let started = std::time::Instant::now();
        for _ in 0..ROUNDS {
            bump(&mut s);
            journal.store(&s).unwrap();
        }
        println!(
            "{name}: {} leases, {:?} per output-sequence update",
            s.leases.len(),
            started.elapsed() / ROUNDS as u32
        );
    }
}
