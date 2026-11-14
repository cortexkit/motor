//! Cost of host work against a day of retained per-job history.
//!
//! A per-job host keeps every finished execution, its lease and its billing
//! record until a day after the broker accepts its usage, so a busy host holds
//! thousands of finished records. Host work (the periodic tick, and a durable
//! save such as the relay watermark written for every output frame) has to
//! cost in proportion to what changes, not to how much finished history the
//! host still holds.
//!
//! `retained_history_cost` prints the measurement used to size that work; run
//! it with `--run-ignored only --no-capture`. The non-ignored tests check the
//! property itself by counting allocations, which does not depend on machine
//! speed or load.
use super::*;
use std::alloc::{GlobalAlloc, Layout, System};
use std::cell::Cell;
use std::time::Instant;

/// Counts heap allocations made by the current thread, so a test can measure
/// the allocations one operation makes without counting other threads.
struct Counting;

thread_local! {
    static ALLOCATIONS: Cell<u64> = const { Cell::new(0) };
    static ALLOCATED_BYTES: Cell<u64> = const { Cell::new(0) };
}

fn count(bytes: usize) {
    // try_with: the counters may already be gone while a thread exits.
    let _ = ALLOCATIONS.try_with(|n| n.set(n.get() + 1));
    let _ = ALLOCATED_BYTES.try_with(|n| n.set(n.get() + bytes as u64));
}

unsafe impl GlobalAlloc for Counting {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        count(layout.size());
        System.alloc(layout)
    }
    unsafe fn alloc_zeroed(&self, layout: Layout) -> *mut u8 {
        count(layout.size());
        System.alloc_zeroed(layout)
    }
    unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
        System.dealloc(ptr, layout)
    }
    unsafe fn realloc(&self, ptr: *mut u8, layout: Layout, new_size: usize) -> *mut u8 {
        count(new_size);
        System.realloc(ptr, layout, new_size)
    }
}

#[global_allocator]
static GLOBAL: Counting = Counting;

/// What one measured operation cost on the calling thread.
#[derive(Clone, Copy, Debug)]
struct Cost {
    elapsed: Duration,
    allocations: u64,
    bytes: u64,
}

fn measure<T>(f: impl FnOnce() -> T) -> (T, Cost) {
    let allocations = ALLOCATIONS.with(Cell::get);
    let bytes = ALLOCATED_BYTES.with(Cell::get);
    let started = Instant::now();
    let value = f();
    let elapsed = started.elapsed();
    let cost = Cost {
        elapsed,
        allocations: ALLOCATIONS.with(Cell::get) - allocations,
        bytes: ALLOCATED_BYTES.with(Cell::get) - bytes,
    };
    (value, cost)
}

const LIVE: &str = "00000000-0000-7000-8000-00000000ffff";

fn job_id(index: usize) -> String {
    format!("00000000-0000-7000-8000-{:012x}", index + 0x1000)
}

/// A finished job as a per-job host retains it: an ended lease, a billing
/// record holding its terminal and accepted usage, and an ended execution
/// whose replay is terminal, shaped like the records on a production host.
fn insert_finished(snapshot: &mut Snapshot, job: &str, ended_at_ms: u64) {
    let claims = claims(job);
    let claims_json = serde_json::to_value(&claims).unwrap();
    let started_at_ms = ended_at_ms.saturating_sub(20_000);
    snapshot.leases.insert(
        job.into(),
        serde_json::from_value(json!({
            "claims": claims_json, "tenant": job, "state": "Ended", "delivered": true,
            "replacing": false, "started_at_ms": started_at_ms, "ended_at_ms": ended_at_ms,
            "highest_seq": 48, "refusal": null
        }))
        .unwrap(),
    );
    let usage = json!({
        "job_id": job, "vcpus": 1, "started_at_ms": started_at_ms, "ended_at_ms": ended_at_ms,
        "mem_peak_mib": 412, "egress_bytes": 18_204, "outcome": "exit",
        "mem_working_peak_mib": 380, "mem_over_mib_ms": 0
    });
    snapshot.billing.insert(
        job.into(),
        serde_json::from_value(json!({
            "started_live": true, "terminal_live": true, "ended_observed_ms": ended_at_ms,
            "terminal_received_ms": ended_at_ms,
            "terminal": {
                "type": "terminal", "job_id": job, "seq": 48, "last_seq": 47,
                "outcome": {"type": "exited", "code": 0}, "wall_ms": 19_871,
                "queue_wait_ms": 312, "bundle_bytes": 1_843_201, "ran": "remote",
                "tree_hash": "9f2c4a51e0b7d3c86a1f5e2b7c9d0a3e4f6b8c1d",
                "workspace_changes": {"added": 3, "modified": 12, "deleted": 1},
                "returned": [{"path": "target/report.json", "bytes": 20_481}]
            },
            "hosted_terminal": {
                "type": "hosted_terminal", "job_id": job, "placement": "first_party/host",
                "billed_vcpu_ms": 19_871, "max_runtime_s": 30
            },
            "usage": usage, "enqueued": true, "delivered": true, "mem_peak_mib": 412,
            "metrics_observed": true, "egress_start": 0, "egress_end": 18_204
        }))
        .unwrap(),
    );
    snapshot.per_job.executions.insert(
        job.into(),
        serde_json::from_value(json!({
            "image_generation": "gen-1",
            "replay_reservation_bytes": crate::state::jobs::storage::REPLAY_RECORD_BYTES,
            "final_replay_reservation_bytes": crate::state::jobs::storage::REPLAY_RECORD_BYTES,
            "replay_completion": "terminal", "relayed_return_paths": true,
            "usage_accepted_at_ms": ended_at_ms + 2_000,
            "claims": claims_json, "disk_id": format!("ws-{}", &job[24..]),
            "memory_debit_mib": 160, "guest_ram_mib": 200, "boot_started": true,
            "boot_started_at_ms": started_at_ms - 3_000, "runner_hello": true, "ended": true,
            "evidence": {
                "winner": "guest", "exit": {"job_id": job, "oom_kill": 0, "egress_bytes": 18_204},
                "acknowledged": true,
                "memory": {"jti": job, "mem_working_peak_mib": 380, "mem_over_mib_ms": 0},
                "usage_inputs": {
                    "started_at_ms": started_at_ms, "ended_at_ms": ended_at_ms,
                    "outcome": "exit", "mem_peak_mib": 412, "oom_kill": null
                }
            }
        }))
        .unwrap(),
    );
}

/// A per-job host whose durable store holds `finished` finished jobs (plus a
/// tenth as many no-start and nonce records) and one running job, `LIVE`.
fn retained_host(directory: &std::path::Path, finished: usize) -> Host {
    let (mut journal, mut snapshot) = crate::store::SqliteJournal::open(directory).unwrap();
    let now = unix_ms();
    for index in 0..finished {
        // Spread the ends across the last day, newest first, all still
        // inside the replay retention window.
        let ended_at_ms = now - 60_000 - (index as u64 * 30_000) % 80_000_000;
        insert_finished(&mut snapshot, &job_id(index), ended_at_ms);
    }
    for index in 0..finished / 10 {
        let job = job_id(finished + index);
        snapshot.zero_usage.insert(
            job.clone(),
            serde_json::from_value(json!({
                "job_id": job, "vcpus": 1, "started_at_ms": now - 90_000, "ended_at_ms": now - 90_000,
                "mem_peak_mib": null, "egress_bytes": null, "outcome": "cancelled"
            }))
            .unwrap(),
        );
        snapshot
            .nonces
            .insert(format!("nonce-{index:08}"), now / 1000 + 300);
    }
    snapshot.per_job.images.insert(
        "gen-1".into(),
        serde_json::from_value(json!({
            "manifest": {
                "kernel": {"bytes": 4096, "sha256": "a".repeat(64)},
                "rootfs": {"bytes": 4096, "sha256": "a".repeat(64)},
                "toolchain": {"bytes": 4096, "sha256": "a".repeat(64)},
                "hello": hello(), "guest_init": {}
            },
            "current": true, "artifacts_ready": true,
            "accounts": {"acct:01ARZ3NDEKTSV4RRFFQ69G5FAV": true}
        }))
        .unwrap(),
    );
    let mut live = claims(LIVE);
    live.max_s = 3_600;
    let live_json = serde_json::to_value(&live).unwrap();
    snapshot.leases.insert(
        LIVE.into(),
        serde_json::from_value(json!({
            "claims": live_json, "tenant": LIVE, "state": "Started", "delivered": true,
            "replacing": false, "started_at_ms": now - 1_000, "ended_at_ms": null,
            "highest_seq": 1, "refusal": null
        }))
        .unwrap(),
    );
    snapshot.billing.insert(LIVE.into(), Default::default());
    crate::journal::Journal::store(&mut journal, &snapshot).unwrap();
    drop(journal);
    // Reopen, as a restarted host would, so the host starts from the store.
    let (journal, snapshot) = crate::store::SqliteJournal::open(directory).unwrap();
    let mut host = Host::new(BTreeMap::new(), snapshot, Box::new(journal));
    host.settings = crate::config::HostSettings::PerJob(crate::config::PerJobSettings {
        vcpu_ceiling: 2,
        permit_budget: 4,
        host_reserve_mib: 50,
        boot_reserve_mib: 20,
        per_vm_overhead_mib: 10,
        reclaim_timeout_ms: 100,
        hysteresis_mib: 10,
        workspace_disk_cap_bytes: 1024,
        workspace_growth_reservation_bytes: 1024,
        boot_deadline_ms: 1000,
        recovery_timeout_ms: 10000,
        exit_evidence_bound_ms: 1000,
        teardown_timeout_ms: 1000,
    });
    host.record_host_resources(HostResources {
        at_ms: now,
        job_vm_headroom_mib: u64::MAX,
        available_mem_mib: 1000,
        vcpu_ceiling: 2,
        permit_budget: 4,
    });
    host
}

fn scratch() -> tempfile::TempDir {
    let root = cortexkit_test_support::scratch_root();
    std::fs::create_dir_all(&root).unwrap();
    tempfile::tempdir_in(root).unwrap()
}

/// The relay's per-frame save: one lease's highest sequence number.
fn watermark_cost(host: &mut Host, seq: u64) -> Cost {
    let (result, cost) = measure(|| host.watermark(LIVE, seq));
    result.unwrap();
    cost
}

/// A runtime over a retained host, using the production image manager so a
/// tick does the same per-job resource work a production host does.
fn retained_runtime(directory: &std::path::Path, finished: usize) -> Arc<Runtime> {
    let host = retained_host(directory, finished);
    let fake = Arc::new(Fake::default());
    *fake.slice_headroom_mib.lock().unwrap() = Some(10_000);
    Runtime::with_broker_and_image_generations(
        "host".into(),
        host,
        fake,
        motor_lease::ISSUER.into(),
        motor_lease::JWKS_URL.into(),
    )
}

fn tick_cost(runtime: &Arc<Runtime>) -> Cost {
    let (result, cost) = measure(|| runtime.tick_jobs(unix_ms()));
    result.unwrap();
    cost
}

/// The steps of one tick, timed one by one, to show where its time goes.
fn tick_steps(runtime: &Arc<Runtime>) {
    let now = unix_ms();
    let report = |step: &str, cost: Cost| {
        println!(
            "  {step}: {:?}, {} allocations",
            cost.elapsed, cost.allocations
        )
    };
    report(
        "exits",
        measure(|| runtime.collect_job_exits_at(now, false).unwrap()).1,
    );
    report(
        "fill_sources",
        measure(|| runtime.host.lock().unwrap().clean_fill_sources().unwrap()).1,
    );
    let ended: Vec<String> = runtime
        .host
        .lock()
        .unwrap()
        .snapshot
        .per_job
        .executions
        .iter()
        .filter(|(_, e)| e.ended)
        .map(|(job, _)| job.clone())
        .collect();
    report(
        "release",
        measure(|| {
            for job in &ended {
                runtime.job_resources.release(job).unwrap();
            }
        })
        .1,
    );
    report(
        "billing",
        measure(|| runtime.host.lock().unwrap().billing_tick(now).unwrap()).1,
    );
    report(
        "retention",
        measure(|| runtime.host.lock().unwrap().retention_tick(now).unwrap()).1,
    );
    report(
        "recoveries",
        measure(|| runtime.tick_recoveries(now).unwrap()).1,
    );
    report(
        "fills",
        measure(|| {
            runtime
                .job_fills
                .lock()
                .unwrap()
                .tick(
                    runtime.host.clone(),
                    runtime.control.clone(),
                    runtime.job_resources.clone(),
                    now,
                )
                .unwrap()
        })
        .1,
    );
    report("balloon", measure(|| runtime.tick_balloon(now)).1);
}

#[test]
#[ignore = "measurement; run with --run-ignored only --no-capture"]
fn retained_history_cost() {
    let finished: usize = std::env::var("MOTOR_SCALE_FINISHED")
        .ok()
        .and_then(|value| value.parse().ok())
        .unwrap_or(2_500);
    let directory = scratch();
    let (runtime, open) = measure(|| retained_runtime(directory.path(), finished));
    {
        let host = runtime.host.lock().unwrap();
        println!(
            "retained: {} executions, {} leases, {} billing, {} no-start, {} nonces (setup {:?})",
            host.snapshot.per_job.executions.len(),
            host.snapshot.leases.len(),
            host.snapshot.billing.len(),
            host.snapshot.zero_usage.len(),
            host.snapshot.nonces.len(),
            open.elapsed,
        );
    }
    if std::env::var_os("MOTOR_SCALE_STEPS").is_some() {
        println!("tick steps:");
        tick_steps(&runtime);
    }
    for round in 1..=3 {
        let cost = tick_cost(&runtime);
        println!(
            "tick {round}: {:?}, {} allocations, {} bytes allocated",
            cost.elapsed, cost.allocations, cost.bytes
        );
    }
    let mut host = runtime.host.lock().unwrap();
    let mut total = Duration::ZERO;
    let mut allocations = 0;
    let mut bytes = 0;
    const SAVES: u64 = 20;
    for seq in 0..SAVES {
        let cost = watermark_cost(&mut host, 100 + seq);
        total += cost.elapsed;
        allocations += cost.allocations;
        bytes += cost.bytes;
    }
    println!(
        "watermark save: {:?} each, {} allocations, {} bytes allocated",
        total / SAVES as u32,
        allocations / SAVES,
        bytes / SAVES
    );
}

/// A save writes the records it changed and nothing else: the relay's
/// per-frame watermark makes the same allocations with a hundred times more
/// finished history behind it.
#[test]
fn a_save_costs_the_same_however_many_finished_jobs_are_kept() {
    let small_dir = scratch();
    let large_dir = scratch();
    let mut small = retained_host(small_dir.path(), 20);
    let mut large = retained_host(large_dir.path(), 2_000);
    // The first save after opening settles one-time work; measure the next.
    watermark_cost(&mut small, 10);
    watermark_cost(&mut large, 10);
    let small_cost = watermark_cost(&mut small, 11);
    let large_cost = watermark_cost(&mut large, 11);
    assert!(
        large_cost.allocations <= small_cost.allocations + 8,
        "a save with 2000 finished jobs made {} allocations, with 20 it made {}",
        large_cost.allocations,
        small_cost.allocations
    );
    // And it was durable: a reopened store holds the new watermark.
    drop(large);
    let (_, reopened) = crate::store::SqliteJournal::open(large_dir.path()).unwrap();
    assert_eq!(reopened.leases[LIVE].highest_seq, 11);
    assert_eq!(reopened.per_job.executions.len(), 2_000);
}
