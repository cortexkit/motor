//! Storage refusal latch, per-tenant free memory advertisement and the
//! capacity file motor-host reads them from.
use super::*;
use anyhow::{bail, Result};
use motor_host::broker::Heartbeats;
use motor_host::capacity::{self, Capacity};
use motor_host::helper::Control;
use motor_host::server::Runtime;
use motor_host::state::{Error, Refusal};
use motor_host::wire::heartbeat_violation;
use serde_json::{json, Value};
use std::os::unix::fs::PermissionsExt;

const MIB: u64 = 1 << 20;
const GIB: u64 = 1 << 30;

/// The owner's values for ovh-1.
fn capacity_values() -> Capacity {
    Capacity {
        windows_vm_bytes: 16 * GIB,
        host_reserve_bytes: 6 * GIB,
        jobs_min_bytes: 24 * GIB,
        storage_margin_bytes: 8 * GIB,
        growth_allowance_bytes: 64 * GIB,
        tenant_memory_reserve_bytes: 2 * GIB,
    }
}

/// Tenant 1 after its resize: 78 GiB, idle suspend off and a 128 GiB floor,
/// so the threshold is 136 GiB and the latch releases at 144 GiB.
fn tenant_one() -> Tenant {
    Tenant {
        tenant: "tenant".into(),
        sub: claims(JOB).sub,
        vcpu_ceiling: 24,
        max_weight: Some(64),
        guest_ram_mib: 79_872,
        idle_suspend_s: 0,
        host_floor_bytes: 128 * GIB,
    }
}

fn host_with(tenants: Vec<(&str, Tenant)>) -> Host {
    Host::new(
        tenants
            .into_iter()
            .map(|(vm, tenant)| (vm.to_owned(), tenant))
            .collect(),
        Snapshot::default(),
        Box::<MemoryJournal>::default(),
    )
}

fn observe(h: &mut Host, free: u64) {
    h.observe_storage(Ok(capacity_values()), Ok(free));
}

fn refused<T: std::fmt::Debug>(result: Result<T, Error>, reason: Refusal) {
    assert!(
        matches!(result, Err(Error::Refused(r)) if r == reason),
        "{result:?}"
    );
}

#[derive(Clone, Default)]
struct Buffer(Arc<Mutex<Vec<u8>>>);
impl std::io::Write for Buffer {
    fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
        self.0.lock().unwrap().extend_from_slice(bytes);
        Ok(bytes.len())
    }
    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}
/// Captures this thread's log lines. One subscriber is registered per
/// `Logs` and reused by every `capture`: registering subscribers while other
/// tests log can make tracing drop events, so tests keep registrations few.
struct Logs {
    buffer: Buffer,
    dispatch: tracing::Dispatch,
}
impl Default for Logs {
    fn default() -> Self {
        let buffer = Buffer::default();
        let dispatch = tracing::Dispatch::new(motor_protocol::logging::LineSubscriber::new(
            "[host]",
            buffer.clone(),
        ));
        Self { buffer, dispatch }
    }
}
impl Logs {
    fn capture<T>(&self, f: impl FnOnce() -> T) -> T {
        tracing::dispatcher::with_default(&self.dispatch, f)
    }
    fn text(&self) -> String {
        String::from_utf8(self.buffer.0.lock().unwrap().clone()).unwrap()
    }
    fn count(&self, name: &str) -> usize {
        self.text().lines().filter(|l| l.contains(name)).count()
    }
}

#[test]
fn storage_threshold_is_the_floor_plus_suspend_reserve_plus_margin() {
    let h = host_with(vec![("vm", tenant_one())]);
    assert_eq!(h.storage_threshold(&capacity_values()), Some(136 * GIB));
    let mut idle = tenant_one();
    idle.idle_suspend_s = 600;
    let h = host_with(vec![("vm", idle)]);
    assert_eq!(
        h.storage_threshold(&capacity_values()),
        Some(128 * GIB + 79_872 * MIB + 8 * GIB)
    );
    let mut overflow = tenant_one();
    overflow.host_floor_bytes = u64::MAX;
    let mut h = host_with(vec![("vm", overflow)]);
    assert_eq!(h.storage_threshold(&capacity_values()), None);
    h.enforce_storage();
    observe(&mut h, u64::MAX);
    assert!(h.storage_refusing());
}

#[test]
fn storage_latch_refuses_one_byte_below_threshold_and_releases_only_after_the_margin() {
    let logs = Logs::default();
    let mut h = host_with(vec![("vm", tenant_one())]);
    h.enforce_storage();
    let now = NOW * 1000;
    let threshold = 136 * GIB;
    let release = 144 * GIB;
    // A host starts latched: nothing is admitted before a probe.
    assert!(h.storage_refusing());
    refused(h.claim(claims(JOB), now), Refusal::HostFull);
    let heartbeat = h.heartbeat(now, 0, true);
    assert_eq!(heartbeat.free_vcpus, 0);
    assert_eq!(heartbeat_violation(&heartbeat), None);
    logs.capture(|| observe(&mut h, release - 1));
    assert!(h.storage_refusing());
    refused(h.claim(claims(JOB), now), Refusal::HostFull);
    logs.capture(|| observe(&mut h, release));
    assert!(!h.storage_refusing());
    assert_eq!(logs.count("host_storage_recovered"), 1);
    h.claim(claims(JOB), now).unwrap();
    assert_eq!(h.heartbeat(now, 0, true).free_vcpus, 63);
    // Exactly the threshold is not below it.
    logs.capture(|| observe(&mut h, threshold));
    assert!(!h.storage_refusing());
    h.claim(claims(JOB2), now).unwrap();

    logs.capture(|| observe(&mut h, threshold - 1));
    assert!(h.storage_refusing());
    refused(h.claim(claims(JOB3), now), Refusal::HostFull);
    let heartbeat = h.heartbeat(now, 0, true);
    assert_eq!(heartbeat.free_vcpus, 0);
    assert_eq!(heartbeat.running.len(), 2);
    assert_eq!(heartbeat_violation(&heartbeat), None);
    // One log per transition, however often the low reading repeats.
    logs.capture(|| {
        observe(&mut h, threshold - 1);
        observe(&mut h, threshold + 1);
        observe(&mut h, threshold - 1);
        observe(&mut h, release - 1);
    });
    assert_eq!(logs.count("host_storage_low"), 1, "{}", logs.text());
    assert!(h.storage_refusing());
    refused(h.claim(claims(JOB3), now), Refusal::HostFull);
    logs.capture(|| observe(&mut h, release));
    assert!(!h.storage_refusing());
    h.claim(claims(JOB3), now).unwrap();
    assert_eq!(heartbeat_violation(&h.heartbeat(now, 0, true)), None);
}

#[test]
fn restarted_host_between_threshold_and_release_and_failed_probes_still_refuse() {
    let logs = Logs::default();
    let now = NOW * 1000;
    let mut h = host_with(vec![("vm", tenant_one())]);
    h.enforce_storage();
    logs.capture(|| observe(&mut h, 136 * GIB + 1));
    assert!(h.storage_refusing());
    refused(h.claim(claims(JOB), now), Refusal::HostFull);
    // Above the threshold is not low, so nothing is logged yet.
    assert_eq!(logs.count("host_storage_low"), 0);

    for (low_logs, (capacity, free, detail)) in (1..).zip([
        (
            Ok(capacity_values()),
            Err("statvfs failed".to_owned()),
            "statvfs failed",
        ),
        (
            Err("capacity.json missing".to_owned()),
            Ok(u64::MAX),
            "capacity.json missing",
        ),
    ]) {
        let mut h = host_with(vec![("vm", tenant_one())]);
        h.enforce_storage();
        logs.capture(|| h.observe_storage(capacity, free));
        assert!(h.storage_refusing());
        refused(h.claim(claims(JOB), now), Refusal::HostFull);
        assert!(logs.text().contains(detail), "{}", logs.text());
        assert_eq!(logs.count("host_storage_low"), low_logs);
        // After a failed probe the latch clears only at the release point.
        observe(&mut h, 144 * GIB - 1);
        assert!(h.storage_refusing());
        observe(&mut h, 144 * GIB);
        assert!(!h.storage_refusing());
    }
}

#[test]
fn delivered_leases_may_still_be_replaced_while_storage_is_low() {
    let mut h = host_with(vec![("vm", tenant_one())]);
    h.enforce_storage();
    let now = NOW * 1000;
    observe(&mut h, 144 * GIB);
    let (id, _) = h.claim(claims(JOB), now).unwrap();
    h.delivered(id).unwrap();
    let (other, _) = h.claim(claims(JOB2), now).unwrap();
    h.close(other).unwrap();
    observe(&mut h, 0);
    let mut replacement = claims(JOB);
    replacement.iat += 1;
    replacement.exp += 1;
    h.claim(replacement, now).unwrap();
    // A lease that never reached the runner is new work again.
    let mut reconnect = claims(JOB2);
    reconnect.iat += 1;
    reconnect.exp += 1;
    refused(h.claim(reconnect, now), Refusal::HostFull);
}

#[test]
fn idle_suspend_tenant_at_threshold_plus_one_byte_can_still_suspend() {
    let mut tenant = tenant_one();
    tenant.idle_suspend_s = 600;
    tenant.guest_ram_mib = 28_672;
    tenant.host_floor_bytes = 100 * GIB;
    let mut h = host_with(vec![("vm", tenant)]);
    h.enforce_storage();
    let threshold = h.storage_threshold(&capacity_values()).unwrap();
    assert_eq!(threshold, 100 * GIB + 28 * GIB + 8 * GIB);
    // A restart at threshold + 1 byte stays latched for new leases ...
    observe(&mut h, threshold + 1);
    assert!(h.storage_refusing());
    // ... but the suspend reserve in the threshold keeps suspend possible.
    let now = NOW * 1000;
    h.guest_ready("tenant", 1).unwrap();
    h.event("tenant", &event(1, TenantEventKind::Idle, None, None), now)
        .unwrap();
    assert!(!h.suspend_due("tenant", now, threshold + 1));
    assert!(h.suspend_due("tenant", now + 600_000, threshold + 1));
}

#[test]
fn two_tenants_use_the_larger_storage_term() {
    let mut small = tenant_one();
    small.tenant = "tenant-2".into();
    small.idle_suspend_s = 600;
    small.guest_ram_mib = 28_672;
    small.host_floor_bytes = 64 * GIB;
    let mut h = host_with(vec![("vm", tenant_one()), ("vm-2", small.clone())]);
    h.enforce_storage();
    // 128 GiB (tenant 1, idle off) beats 64 + 28 GiB (idle on).
    assert_eq!(h.storage_threshold(&capacity_values()), Some(136 * GIB));
    observe(&mut h, 144 * GIB);
    assert!(!h.storage_refusing());
    observe(&mut h, 136 * GIB - 1);
    assert!(h.storage_refusing());
    // With a larger floor, the idle tenant's term wins.
    small.host_floor_bytes = 120 * GIB;
    let h = host_with(vec![("vm", tenant_one()), ("vm-2", small)]);
    assert_eq!(
        h.storage_threshold(&capacity_values()),
        Some(120 * GIB + 28 * GIB + 8 * GIB)
    );
}

#[test]
fn free_memory_follows_the_guest_sample_with_the_reserve_and_freshness_bound() {
    let logs = Logs::default();
    let mut h = host_with(vec![("vm", tenant_one())]);
    let now = NOW * 1000;
    // No valid capacity file: no reserve is known, so nothing is advertised.
    assert_eq!(h.free_mem_mib(now), 0);
    observe(&mut h, u64::MAX);
    // Stopped: what the 78 GiB guest will have once running.
    assert_eq!(h.free_mem_mib(now), 77_824);
    // The stopped tenant can be placed; the lease is what wakes it.
    h.claim(claims(JOB), now).unwrap();
    h.guest_ready("tenant", 1).unwrap();
    // Running, not yet sampled: 0 until the first probe, without a log.
    logs.capture(|| assert_eq!(h.free_mem_mib(now), 0));
    assert_eq!(logs.count("tenant_memory_unavailable"), 0);
    // The broker needs max(16384, 3584 x vcpus) MiB; 16,384 MiB plus the
    // 2 GiB reserve is exactly its 4-vCPU threshold.
    for (bytes, advertised) in [
        ((16_384 + 2_048) * MIB, 16_384),
        ((16_384 + 2_048 + 1) * MIB, 16_385),
        ((16_384 + 2_048 - 1) * MIB, 16_383),
        ((16_384 + 2_048 + 1) * MIB - 1, 16_384),
        (2_048 * MIB, 0),
        (GIB, 0),
    ] {
        h.record_memory("tenant", now, Ok(Some(bytes)));
        assert_eq!(h.free_mem_mib(now), advertised, "{bytes}");
    }
    h.record_memory("tenant", now, Ok(Some(50 * GIB)));
    assert_eq!(h.free_mem_mib(now + 30_000), 51_200 - 2_048);
    logs.capture(|| assert_eq!(h.free_mem_mib(now + 30_001), 0));
    assert!(logs.text().contains("stale_sample"), "{}", logs.text());
    for (reading, case) in [
        (Ok(None), "missing_mem_available_bytes"),
        (Err("relay port54 refused".to_owned()), "probe_failed"),
    ] {
        h.record_memory("tenant", now, reading);
        logs.capture(|| {
            assert_eq!(h.free_mem_mib(now), 0);
            assert_eq!(h.free_mem_mib(now + 1), 0);
        });
        assert_eq!(
            logs.text().lines().filter(|l| l.contains(case)).count(),
            1,
            "{}",
            logs.text()
        );
    }
    assert!(logs.text().contains("relay port54 refused"));
    // Suspended (or stopped, or waking again): the guest RAM figure, and a
    // sample from the previous run is never reused.
    h.record_memory("tenant", now, Ok(Some(GIB)));
    h.guest_unready("tenant");
    assert_eq!(h.free_mem_mib(now), 77_824);
    h.guest_ready("tenant", 2).unwrap();
    assert_eq!(h.free_mem_mib(now), 0);
}

#[test]
fn several_tenants_advertise_the_largest_free_memory_and_vcpus() {
    let mut second = tenant_one();
    second.tenant = "tenant-2".into();
    second.sub = "acct:01ARZ3NDEKTSV4RRFFQ69G5FAW".into();
    second.guest_ram_mib = 28_672;
    second.max_weight = None;
    second.vcpu_ceiling = 8;
    let mut h = host_with(vec![("vm", tenant_one()), ("vm-2", second)]);
    let now = NOW * 1000;
    observe(&mut h, u64::MAX);
    assert_eq!(h.free_mem_mib(now), 77_824);
    h.guest_ready("tenant", 1).unwrap();
    h.guest_ready("tenant-2", 1).unwrap();
    h.record_memory("tenant", now, Ok(Some(10 * GIB)));
    h.record_memory("tenant-2", now, Ok(Some(20 * GIB)));
    assert_eq!(h.free_mem_mib(now), 20_480 - 2_048);
    let mut big = claims(JOB);
    big.vcpus = 24;
    h.claim(big, now).unwrap();
    // The two fields are separate maxima: tenant 1 still has 40 permits.
    assert_eq!(h.heartbeat(now, 0, true).free_vcpus, 40);
    // Admission binds the job to its own tenant: tenant 1 is full even
    // though the host advertises tenant 2's capacity.
    for job in [JOB2, JOB3] {
        let mut c = claims(job);
        c.vcpus = 20;
        h.claim(c, now).unwrap();
    }
    assert_eq!(h.heartbeat(now, 0, true).free_vcpus, 8);
    refused(
        h.claim(claims("00000000-0000-7000-8000-000000000004"), now),
        Refusal::HostFull,
    );
}

/// A helper and runner double for the heartbeat publisher.
#[derive(Default)]
struct Probe {
    capacity: Mutex<Option<Capacity>>,
    free: Mutex<Option<u64>>,
    status: Mutex<Option<Value>>,
    statuses: Mutex<u32>,
}
impl Control for Probe {
    fn metrics(&self, _: &str) -> Result<motor_host::billing::Metrics> {
        Ok(motor_host::billing::Metrics {
            memory_bytes: 0,
            egress_bytes: 0,
        })
    }
    fn adopt(&self, _: &str) -> Result<bool> {
        Ok(true)
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
    fn admin(&self, _: &str, request: &Value) -> Result<Value> {
        assert_eq!(request, &json!({"op":"status"}));
        *self.statuses.lock().unwrap() += 1;
        match self.status.lock().unwrap().clone() {
            Some(reply) => Ok(reply),
            None => bail!("tenant control port54 open relay failed"),
        }
    }
    fn suspend(&self, _: &str) -> Result<()> {
        bail!("unexpected suspend")
    }
    fn available_bytes(&self) -> Result<u64> {
        self.free
            .lock()
            .unwrap()
            .ok_or_else(|| anyhow::anyhow!("statvfs failed"))
    }
    fn capacity(&self) -> Result<Capacity> {
        self.capacity
            .lock()
            .unwrap()
            .clone()
            .ok_or_else(|| anyhow::anyhow!("capacity.json missing"))
    }
}

fn last_heartbeat(runtime: &Runtime) -> motor_lease::Heartbeat {
    let host = runtime.host.lock().unwrap();
    let request = host
        .snapshot
        .outbox
        .iter()
        .rev()
        .find(|r| r.path.ends_with("/heartbeat"))
        .expect("queued heartbeat");
    serde_json::from_slice(&request.body).unwrap()
}

#[test]
fn heartbeats_continue_while_latched_and_carry_the_probed_guest_memory() {
    let probe = Arc::new(Probe::default());
    let mut host = host_with(vec![("vm", tenant_one())]);
    host.enforce_storage();
    let runtime = Runtime::new("host".into(), host, probe.clone());
    let mut publisher = Heartbeats::new(runtime.clone());
    let t0 = NOW * 1000;
    // No capacity file: latched, but the heartbeat is still queued.
    publisher.step(t0).unwrap();
    let heartbeat = last_heartbeat(&runtime);
    assert_eq!((heartbeat.free_vcpus, heartbeat.free_mem_mib), (0, 0));
    assert_eq!(heartbeat_violation(&heartbeat), None);

    *probe.capacity.lock().unwrap() = Some(capacity_values());
    *probe.free.lock().unwrap() = Some(144 * GIB);
    publisher.step(t0 + 1).unwrap();
    let heartbeat = last_heartbeat(&runtime);
    assert_eq!((heartbeat.free_vcpus, heartbeat.free_mem_mib), (64, 77_824));
    assert_eq!(
        *probe.statuses.lock().unwrap(),
        0,
        "a stopped tenant is not probed"
    );

    runtime
        .host
        .lock()
        .unwrap()
        .guest_ready("tenant", 1)
        .unwrap();
    *probe.status.lock().unwrap() =
        Some(json!({"ok":true,"health":{"mem_available_bytes": 70 * GIB}}));
    publisher.step(t0 + 2).unwrap();
    assert_eq!(*probe.statuses.lock().unwrap(), 1);
    // The next probe waits for the renewal interval.
    publisher.step(t0 + 2 + 14_999).unwrap();
    assert_eq!(*probe.statuses.lock().unwrap(), 1);
    publisher.step(t0 + 2 + 15_000).unwrap();
    assert_eq!(*probe.statuses.lock().unwrap(), 2);
    let heartbeat = last_heartbeat(&runtime);
    assert_eq!(heartbeat.free_mem_mib, 71_680 - 2_048);

    // A probe that fails, or a reply without the field, advertises 0.
    *probe.status.lock().unwrap() = None;
    publisher.step(t0 + 2 + 30_000).unwrap();
    assert_eq!(last_heartbeat(&runtime).free_mem_mib, 0);
    *probe.status.lock().unwrap() = Some(json!({"ok":true,"health":{}}));
    publisher.step(t0 + 2 + 45_000).unwrap();
    assert_eq!(last_heartbeat(&runtime).free_mem_mib, 0);

    // Free space below the threshold latches again; heartbeats go on.
    *probe.free.lock().unwrap() = Some(136 * GIB - 1);
    publisher.step(t0 + 2 + 45_001).unwrap();
    let heartbeat = last_heartbeat(&runtime);
    assert_eq!(heartbeat.free_vcpus, 0);
    assert_eq!(heartbeat_violation(&heartbeat), None);
    assert!(runtime.host.lock().unwrap().storage_refusing());
    runtime.host.lock().unwrap().guest_unready("tenant");
}

#[test]
fn capacity_file_must_be_complete_owned_and_protected() {
    let directory = scratch();
    let path = directory.path().join("capacity.json");
    let me = std::fs::metadata(directory.path()).unwrap();
    let uid = std::os::unix::fs::MetadataExt::uid(&me);
    let good = r#"{"windows_vm_bytes":17179869184,"host_reserve_bytes":6442450944,"jobs_min_bytes":25769803776,"storage_margin_bytes":8589934592,"growth_allowance_bytes":68719476736,"tenant_memory_reserve_bytes":2147483648}"#;
    let write = |body: &str, mode: u32| {
        let _ = std::fs::remove_file(&path);
        std::fs::write(&path, body).unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(mode)).unwrap();
    };
    write(good, 0o644);
    assert_eq!(capacity::read(&path, uid).unwrap(), capacity_values());
    assert!(capacity::read(&path, uid.wrapping_add(1)).is_err());
    for mode in [0o664, 0o646, 0o666] {
        write(good, mode);
        assert!(capacity::read(&path, uid).is_err(), "{mode:o}");
    }
    write(good, 0o600);
    let link = directory.path().join("link.json");
    std::os::unix::fs::symlink(&path, &link).unwrap();
    assert!(capacity::read(&link, uid).is_err());
    assert!(capacity::read(&directory.path().join("absent.json"), uid).is_err());
    for body in [
        String::new(),
        "{".into(),
        "[]".into(),
        good.replace(",\"tenant_memory_reserve_bytes\":2147483648", ""),
        good.replace('}', ",\"extra_bytes\":1}"),
        good.replace("8589934592", "8589934592.0"),
        good.replace("8589934592", "-1"),
        good.replace("8589934592", "\"8589934592\""),
        good.replace("8589934592", "18446744073709551616"),
        good.replace("25769803776", "0"),
        good.replace("17179869184", "18446744073709551615"),
        good.replace('}', ",\"storage_margin_bytes\":1}"),
    ] {
        write(&body, 0o644);
        assert!(capacity::read(&path, uid).is_err(), "{body}");
    }
}

/// Operator fixture: a sparse tenant disk on a small loop-mounted XFS volume
/// is filled by a writer under an admitted lease. motor-host's real capacity
/// file reader and statvfs probe must refuse new leases while free space is
/// still above the direct runner's own refusal floor.
#[cfg(target_os = "linux")]
#[test]
#[ignore = "requires trusted operator host sudo and loop mount privileges; never run on shared runners"]
fn isolated_sparse_disk_on_loop_filesystem_trips_the_latch_before_the_direct_runner_floor() {
    use std::io::{Seek, SeekFrom, Write};
    use std::process::Command;
    const DIRECT_RUNNER_FLOOR: u64 = 64 * MIB;
    fn sudo(args: &[&str]) {
        let status = Command::new("sudo").arg("-n").args(args).status().unwrap();
        assert!(status.success(), "sudo {args:?}");
    }
    struct Mount(std::path::PathBuf);
    impl Drop for Mount {
        fn drop(&mut self) {
            sudo(&["umount", self.0.to_str().unwrap()]);
        }
    }
    let directory = scratch();
    let image = directory.path().join("volume.img");
    let mount = directory.path().join("volume");
    std::fs::create_dir(&mount).unwrap();
    std::fs::File::create(&image)
        .unwrap()
        .set_len(512 * MIB)
        .unwrap();
    assert!(Command::new("mkfs.xfs")
        .args(["-q", "-f"])
        .arg(&image)
        .status()
        .unwrap()
        .success());
    sudo(&[
        "mount",
        "-o",
        "loop",
        image.to_str().unwrap(),
        mount.to_str().unwrap(),
    ]);
    let _mount = Mount(mount.clone());
    // SAFETY: geteuid has no preconditions.
    let uid = unsafe { libc::geteuid() }.to_string();
    sudo(&["chown", &uid, mount.to_str().unwrap()]);
    let values = Capacity {
        storage_margin_bytes: 32 * MIB,
        ..capacity_values()
    };
    let staged = directory.path().join("capacity.staged");
    std::fs::write(
        &staged,
        serde_json::to_vec(&json!({
            "windows_vm_bytes": values.windows_vm_bytes,
            "host_reserve_bytes": values.host_reserve_bytes,
            "jobs_min_bytes": values.jobs_min_bytes,
            "storage_margin_bytes": values.storage_margin_bytes,
            "growth_allowance_bytes": values.growth_allowance_bytes,
            "tenant_memory_reserve_bytes": values.tenant_memory_reserve_bytes,
        }))
        .unwrap(),
    )
    .unwrap();
    let capacity_path = mount.join("capacity.json");
    sudo(&[
        "install",
        "-o",
        "root",
        "-g",
        "root",
        "-m",
        "0644",
        staged.to_str().unwrap(),
        capacity_path.to_str().unwrap(),
    ]);
    // The tenant's floor equals the direct runner's floor (provisioning
    // refuses a smaller one), idle suspend is off, so the threshold is
    // floor + margin, 96 MiB.
    let mut tenant = tenant_one();
    tenant.host_floor_bytes = DIRECT_RUNNER_FLOOR;
    tenant.max_weight = Some(1_000);
    let mut host = host_with(vec![("vm", tenant)]);
    host.enforce_storage();
    let threshold = host.storage_threshold(&values).unwrap();
    assert_eq!(threshold, 96 * MIB);
    let helper = Arc::new(motor_host::helper::Helper {
        socket: directory.path().join("no-helper.sock"),
        storage: mount.clone(),
        capacity: capacity_path,
        guest_init: BTreeMap::new(),
    });
    let runtime = Runtime::new("host".into(), host, helper.clone());
    let now = NOW * 1000;
    runtime.probe_storage();
    assert!(!runtime.host.lock().unwrap().storage_refusing());
    runtime
        .host
        .lock()
        .unwrap()
        .claim(claims(JOB), now)
        .unwrap();
    // The tenant disk is sparse and longer than the whole volume.
    let mut disk = std::fs::File::create(mount.join("disk.img")).unwrap();
    disk.set_len(2 * GIB).unwrap();
    let chunk = vec![0x5a; 4 * MIB as usize];
    for n in 1u64.. {
        disk.seek(SeekFrom::Start((n - 1) * chunk.len() as u64))
            .unwrap();
        disk.write_all(&chunk).unwrap();
        disk.sync_all().unwrap();
        runtime.probe_storage();
        let free = helper.available_bytes().unwrap();
        let job = format!("00000000-0000-7000-8000-{:012x}", n + 16);
        let admitted = runtime.host.lock().unwrap().claim(claims(&job), now);
        if let Err(error) = admitted {
            assert!(
                matches!(error, Error::Refused(Refusal::HostFull)),
                "{error}"
            );
            assert!(free < threshold, "refused at {free} free");
            assert!(free >= DIRECT_RUNNER_FLOOR, "refused only at {free} free");
            return;
        }
        assert!(free >= threshold, "admitted at {free} free");
    }
}
