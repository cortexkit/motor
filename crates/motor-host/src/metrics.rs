//! Best-effort observations, collected away from heartbeat and admission work.
use crate::server::Runtime;
#[cfg(target_os = "linux")]
use crate::state::LeaseState;
#[cfg(target_os = "linux")]
use anyhow::Context;
use anyhow::Result;
use motor_lease::metrics::*;
use std::{path::Path, sync::Arc};

// Mirrors the VM helper's delegated service cgroup. If the helper's location
// diverges, pressure is omitted rather than affecting host or tenant operation.
const HELPER_TENANT_CGROUPS: &str = "/sys/fs/cgroup/system.slice/motor-vm-helper.service/tenants";

pub fn tenant_pressure_cgroup(tap_index: u8) -> std::path::PathBuf {
    Path::new(HELPER_TENANT_CGROUPS).join(format!("tenant-{tap_index}"))
}

#[derive(Default)]
pub struct Sampler {
    previous_cpu: Option<(u64, u64)>,
    #[cfg(target_os = "linux")]
    pressure_warned: std::collections::HashSet<String>,
}

impl Sampler {
    #[cfg(target_os = "linux")]
    pub fn sample(
        &mut self,
        runtime: &Arc<Runtime>,
        storage: &Path,
        now_ms: u64,
    ) -> Result<HostMetrics> {
        let stat = std::fs::read_to_string("/proc/stat")?;
        let (total, idle) = cpu_ticks(&stat)?;
        let (old_total, old_idle) = self
            .previous_cpu
            .replace((total, idle))
            .unwrap_or((total, idle));
        let elapsed = total.saturating_sub(old_total);
        let busy = elapsed.saturating_sub(idle.saturating_sub(old_idle));
        let load: f64 = std::fs::read_to_string("/proc/loadavg")?
            .split_whitespace()
            .next()
            .context("load1")?
            .parse()?;
        let mut sample = HostMetrics {
            at_ms: now_ms,
            cpu: Some(Cpu {
                threads: stat
                    .lines()
                    .filter(|l| {
                        l.strip_prefix("cpu")
                            .is_some_and(|s| s.starts_with(|c: char| c.is_ascii_digit()))
                    })
                    .count() as u64,
                busy_pct: busy
                    .saturating_mul(100)
                    .checked_div(elapsed)
                    .unwrap_or(0)
                    .min(100),
                load1: Some((load * 100.0).round() / 100.0),
            }),
            mem: Some(memory(&std::fs::read_to_string("/proc/meminfo")?)?),
            disks: vec![disk(Path::new("/"))?],
            tenants: vec![],
            runners: vec![],
            pressure: pressure(Path::new("/proc/pressure"), ["cpu", "memory", "io"]),
        };
        // Mount names are the only paths allowed by the wire contract. Never
        // publish the configured storage path, which can contain tenant data.
        let mounts = std::fs::read_to_string("/proc/self/mounts")?;
        if let Some(mount) = storage_mount(&mounts, storage) {
            if mount != Path::new("/") {
                sample.disks.push(disk(&mount)?);
            }
        }
        self.tenants(runtime, &mut sample);
        Ok(sample)
    }

    #[cfg(not(target_os = "linux"))]
    pub fn sample(
        &mut self,
        _runtime: &Arc<Runtime>,
        _storage: &Path,
        _now_ms: u64,
    ) -> Result<HostMetrics> {
        let _ = &self.previous_cpu;
        anyhow::bail!("host metrics sampling requires Linux")
    }

    #[cfg(target_os = "linux")]
    fn tenants(&mut self, runtime: &Arc<Runtime>, sample: &mut HostMetrics) {
        let bindings: Vec<_> = {
            let host = runtime.host.lock().expect("host lock");
            host.tenants
                .iter()
                .take(8)
                .map(|(vm, t)| (vm.clone(), t.clone()))
                .collect()
        };
        for (index, (vm, binding)) in bindings.into_iter().enumerate() {
            let (mut tenant, ready) = {
                let host = runtime.host.lock().expect("host lock");
                let ready = host.is_ready(&binding.tenant);
                let recovery = host.recovery_failure(&binding.tenant).map(|f| Recovery {
                    step: recovery_step(&f.timeout.step).into(),
                    since_ms: f.since_ms,
                    attempts: u64::from(f.attempts),
                });
                let detail = if host.recovery_stuck(&binding.tenant) {
                    Some("tenant_recovery_stuck")
                } else if recovery.is_some() {
                    Some("tenant_recovery_timeout")
                } else if host.start_refusal_case(&binding.tenant).is_some() {
                    Some("tenant_start_refused")
                } else if host.storage_refusing() {
                    Some("host_storage_low")
                } else {
                    host.last_metrics_error(&binding.tenant)
                };
                (
                    Tenant {
                        vm,
                        state: if recovery.is_some() {
                            TenantState::Recovering
                        } else if ready {
                            TenantState::Ready
                        } else if host.is_waking(&binding.tenant) {
                            TenantState::Starting
                        } else {
                            TenantState::Stopped
                        },
                        vcpus: u64::from(binding.vcpu_ceiling),
                        mem_mib: binding.guest_ram_mib,
                        slots_total: u64::from(binding.budget()),
                        slots_used: u64::from(host.share(&binding.tenant, sample.at_ms)),
                        jobs_running: host
                            .snapshot
                            .leases
                            .values()
                            .filter(|l| {
                                l.tenant == binding.tenant && l.state == LeaseState::Started
                            })
                            .count() as u64,
                        recovery,
                        detail: detail.map(str::to_owned),
                        disk_free_gib: None,
                        disk_total_gib: None,
                        pressure: None,
                    },
                    ready,
                )
            };
            if let Some(cgroup) = runtime.control.pressure_cgroup(&binding.tenant) {
                tenant.pressure = self.tenant_pressure(&binding.tenant, cgroup);
            }
            let idle = ready
                && runtime
                    .tenant_operations(&binding.tenant)
                    .is_ok_and(|operation| operation.try_lock().is_ok());
            if idle {
                // The status RPC is read-only. Release the operation lock before
                // probing so slow diagnostics cannot stall a wake or suspension.
                if let Ok(reply) = runtime
                    .control
                    .admin(&binding.tenant, &serde_json::json!({"op":"status"}))
                {
                    if let Ok(quota) = runtime.control.disk_quota_bytes(&binding.tenant) {
                        if let Some((free, total)) = guest_disk(&reply, quota) {
                            tenant.disk_free_gib = Some(Some(free));
                            tenant.disk_total_gib = Some(Some(total));
                        }
                    }
                    if let Some(runner) = runner(&reply, format!("vm-{}", index + 1)) {
                        tenant.slots_total = runner.slots_total;
                        tenant.slots_used = runner.slots_used;
                        tenant.jobs_running = reply["health"]["running"]
                            .as_array()
                            .map_or(tenant.jobs_running, |jobs| jobs.len() as u64);
                        sample.runners.push(runner);
                    }
                }
            }
            sample.tenants.push(tenant);
        }
    }

    #[cfg(target_os = "linux")]
    fn tenant_pressure(&mut self, tenant: &str, directory: &Path) -> Option<Pressure> {
        let pressure = pressure(
            directory,
            ["cpu.pressure", "memory.pressure", "io.pressure"],
        );
        if pressure.is_none() && self.pressure_warned.insert(tenant.to_owned()) {
            tracing::warn!(tenant, cgroup = %directory.display(), "tenant pressure unavailable; omitting PSI metrics");
        }
        pressure
    }
}

#[cfg(target_os = "linux")]
fn pressure(directory: &Path, files: [&str; 3]) -> Option<Pressure> {
    let [cpu, memory, io] = files.map(|file| std::fs::read_to_string(directory.join(file)).ok());
    let (cpu, memory, io) = (cpu?, memory?, io?);
    let pressure = Pressure {
        cpu_some: psi_avg10(&cpu, "some")?,
        mem_some: psi_avg10(&memory, "some")?,
        mem_full: psi_avg10(&memory, "full")?,
        io_some: psi_avg10(&io, "some")?,
        io_full: psi_avg10(&io, "full")?,
    };
    pressure.is_valid().then_some(pressure)
}

#[cfg(target_os = "linux")]
fn psi_avg10(text: &str, kind: &str) -> Option<f64> {
    text.lines().find_map(|line| {
        let mut fields = line.split_whitespace();
        if fields.next()? != kind {
            return None;
        }
        fields
            .find_map(|field| field.strip_prefix("avg10="))?
            .parse()
            .ok()
    })
}

#[cfg(target_os = "linux")]
fn guest_disk(reply: &serde_json::Value, quota: u64) -> Option<(u64, u64)> {
    if reply["ok"] != true || quota == 0 {
        return None;
    }
    let disk = &reply["health"]["guest_disk"];
    if disk["age_ms"].as_u64()? > 60_000 {
        return None;
    }
    // Guest input is display-only. The helper's registry, not the guest,
    // bounds the figures; none of these values enter Host admission state.
    let total = disk["total_bytes"].as_u64()?.min(quota);
    let free = disk["free_bytes"].as_u64()?.min(total);
    Some((free / (1 << 30), total / (1 << 30)))
}

#[cfg(target_os = "linux")]
fn runner(reply: &serde_json::Value, name: String) -> Option<Runner> {
    if reply["ok"] != true {
        return None;
    }
    let health = &reply["health"];
    Some(Runner {
        name,
        state: RunnerState::Ready,
        slots_total: health["permits_cap"].as_u64()?,
        slots_used: health["permits_held"].as_u64()?,
        queued: health["queue_depth"].as_u64()?,
        oldest_wait_s: health["oldest_wait_s"].as_u64()?,
        detail: None,
    })
}

/// Guest strings are not trusted log messages, even when called a step name.
#[cfg(target_os = "linux")]
pub(crate) fn recovery_step(step: &str) -> &'static str {
    match step {
        "initial_unmount" => "initial_unmount",
        "replay" => "replay",
        "remount" => "remount",
        "unmount" => "unmount",
        "xfs_check" => "xfs_check",
        "git_fsck" => "git_fsck",
        "attach_reply" => "attach_reply",
        _ => "unknown",
    }
}

#[cfg(target_os = "linux")]
fn cpu_ticks(stat: &str) -> Result<(u64, u64)> {
    // guest and guest_nice are already included in user and nice.
    let ticks: Vec<u64> = stat
        .lines()
        .next()
        .context("cpu counters")?
        .split_whitespace()
        .skip(1)
        .take(8)
        .map(str::parse)
        .collect::<std::result::Result<_, _>>()?;
    anyhow::ensure!(ticks.len() >= 5, "cpu counters");
    Ok((ticks.iter().sum(), ticks[3] + ticks[4]))
}

#[cfg(target_os = "linux")]
fn memory(info: &str) -> Result<Memory> {
    let kib = |name: &str| -> Result<u64> {
        Ok(info
            .lines()
            .find_map(|l| l.strip_prefix(name))
            .context("memory counter")?
            .split_whitespace()
            .next()
            .context("memory value")?
            .parse()?)
    };
    Ok(Memory {
        total_mib: kib("MemTotal:")? / 1024,
        avail_mib: kib("MemAvailable:")? / 1024,
    })
}

#[cfg(target_os = "linux")]
fn storage_mount(mounts: &str, storage: &Path) -> Option<std::path::PathBuf> {
    mounts
        .lines()
        .filter_map(|line| line.split_whitespace().nth(1))
        .filter(|m| !m.contains('\\'))
        .map(std::path::PathBuf::from)
        .filter(|m| storage.starts_with(m))
        .max_by_key(|m| m.as_os_str().len())
}

#[cfg(target_os = "linux")]
fn disk(mount: &Path) -> Result<Disk> {
    use std::os::unix::ffi::OsStrExt;
    let path = std::ffi::CString::new(mount.as_os_str().as_bytes())?;
    let mut stat = std::mem::MaybeUninit::<libc::statvfs>::uninit();
    // statvfs initializes the whole result on success; the path is NUL terminated.
    anyhow::ensure!(
        unsafe { libc::statvfs(path.as_ptr(), stat.as_mut_ptr()) } == 0,
        "disk counters"
    );
    let stat = unsafe { stat.assume_init() };
    Ok(Disk {
        mount: mount.to_str().context("mount encoding")?.into(),
        total_gib: stat.f_blocks.saturating_mul(stat.f_frsize) >> 30,
        free_gib: stat.f_bavail.saturating_mul(stat.f_frsize) >> 30,
    })
}

#[cfg(all(test, target_os = "linux"))]
mod tests {
    use super::*;

    const CPU_PSI: &str = "some avg10=0.00 avg60=0.01 avg300=0.05 total=2435237\n";
    const MEMORY_PSI: &str = "some avg10=1.23 avg60=2.34 avg300=3.45 total=1234567\nfull avg10=0.12 avg60=0.23 avg300=0.34 total=12345\n";
    const IO_PSI: &str = "some avg10=45.67 avg60=34.56 avg300=23.45 total=9876543\nfull avg10=8.90 avg60=7.89 avg300=6.78 total=123456\n";
    const HOST_PSI_FILES: [&str; 3] = ["cpu", "memory", "io"];
    const TENANT_PSI_FILES: [&str; 3] = ["cpu.pressure", "memory.pressure", "io.pressure"];

    fn write_psi(directory: &Path, files: [&str; 3]) {
        std::fs::create_dir_all(directory).unwrap();
        for (file, text) in files.into_iter().zip([CPU_PSI, MEMORY_PSI, IO_PSI]) {
            std::fs::write(directory.join(file), text).unwrap();
        }
    }

    #[test]
    fn pressure_reads_kernel_avg10_with_or_without_cpu_full() {
        let scratch = tempfile::tempdir_in(cortexkit_test_support::scratch_root()).unwrap();
        write_psi(scratch.path(), HOST_PSI_FILES);
        let expected = Pressure {
            cpu_some: 0.0,
            mem_some: 1.23,
            mem_full: 0.12,
            io_some: 45.67,
            io_full: 8.9,
        };
        assert_eq!(
            pressure(scratch.path(), HOST_PSI_FILES),
            Some(expected.clone())
        );
        std::fs::write(
            scratch.path().join("cpu"),
            format!("{CPU_PSI}full avg10=0.09 avg60=0.08 avg300=0.07 total=123\n"),
        )
        .unwrap();
        assert_eq!(pressure(scratch.path(), HOST_PSI_FILES), Some(expected));
    }

    #[test]
    fn tenant_pressure_uses_vmm_cgroup_files_and_warns_once_per_tenant() {
        assert_eq!(
            tenant_pressure_cgroup(7),
            Path::new("/sys/fs/cgroup/system.slice/motor-vm-helper.service/tenants/tenant-7")
        );
        let scratch = tempfile::tempdir_in(cortexkit_test_support::scratch_root()).unwrap();
        let mut sampler = Sampler::default();
        assert!(sampler
            .tenant_pressure("tenant-a", scratch.path())
            .is_none());
        assert!(sampler
            .tenant_pressure("tenant-a", scratch.path())
            .is_none());
        assert_eq!(sampler.pressure_warned.len(), 1);
        assert!(sampler
            .tenant_pressure("tenant-b", scratch.path())
            .is_none());
        assert_eq!(sampler.pressure_warned.len(), 2);
        write_psi(scratch.path(), TENANT_PSI_FILES);
        let sampled = sampler.tenant_pressure("tenant-a", scratch.path()).unwrap();
        assert_eq!(sampled.mem_some, 1.23);
        assert_eq!(sampled.io_full, 8.9);
        assert_eq!(sampler.pressure_warned.len(), 2);
    }

    #[test]
    fn pressure_files_omit_only_the_missing_or_malformed_object() {
        let scratch = tempfile::tempdir_in(cortexkit_test_support::scratch_root()).unwrap();
        let host = scratch.path().join("host");
        let tenant = scratch.path().join("tenant");
        for (broken, files, healthy, healthy_files) in [
            (&host, HOST_PSI_FILES, &tenant, TENANT_PSI_FILES),
            (&tenant, TENANT_PSI_FILES, &host, HOST_PSI_FILES),
        ] {
            write_psi(healthy, healthy_files);
            for file in files {
                write_psi(broken, files);
                std::fs::remove_file(broken.join(file)).unwrap();
                assert!(pressure(broken, files).is_none());
                assert!(pressure(healthy, healthy_files).is_some());
                for malformed in [
                    "some avg60=0.00 total=1\nfull avg10=0.00\n",
                    "some avg10=bad\nfull avg10=0.00\n",
                    "some avg10=NaN\nfull avg10=0.00\n",
                    "some avg10=100.01\nfull avg10=0.00\n",
                    "some avg10=1.234\nfull avg10=0.00\n",
                    "full avg10=0.00\n",
                ] {
                    std::fs::write(broken.join(file), malformed).unwrap();
                    assert!(pressure(broken, files).is_none(), "{file}: {malformed}");
                    assert!(pressure(healthy, healthy_files).is_some());
                }
                if file != files[0] {
                    std::fs::write(broken.join(file), CPU_PSI).unwrap();
                    assert!(pressure(broken, files).is_none(), "full line is required");
                }
            }
        }
    }

    #[test]
    fn guest_disk_ignores_old_guests_stale_reports_and_malformed_values() {
        use serde_json::json;
        let mut reply = json!({"ok":true,"health":{}});
        assert_eq!(guest_disk(&reply, 100), None);
        reply["health"]["guest_disk"] = json!({"free_bytes":51,"total_bytes":99,"age_ms":60_000});
        assert_eq!(guest_disk(&reply, 100), Some((0, 0)));
        assert_eq!(guest_disk(&reply, 0), None);
        for disk in [
            json!({"free_bytes":51,"total_bytes":99,"age_ms":60_001}),
            json!({"free_bytes":-1,"total_bytes":99,"age_ms":0}),
            json!({"free_bytes":1.5,"total_bytes":99,"age_ms":0}),
            json!({"free_bytes":51,"total_bytes":null,"age_ms":0}),
            json!({"free_bytes":51,"total_bytes":99}),
        ] {
            reply["health"]["guest_disk"] = disk;
            assert_eq!(guest_disk(&reply, 100), None);
        }
        reply["ok"] = false.into();
        assert_eq!(guest_disk(&reply, 100), None);
    }

    #[test]
    fn proc_counters_use_available_memory_and_do_not_double_count_guest_cpu() {
        assert_eq!(
            cpu_ticks("cpu 10 20 30 40 50 60 70 80 900 1000\ncpu0 1").unwrap(),
            (360, 90)
        );
        let mem = memory("MemTotal: 8192 kB\nMemFree: 1 kB\nMemAvailable: 4096 kB\n").unwrap();
        assert_eq!((mem.total_mib, mem.avail_mib), (8, 4));
        let mounts = "root / xfs rw 0 0\ndisk /srv xfs rw 0 0\ndisk2 /srv/build xfs rw 0 0\n";
        assert_eq!(
            storage_mount(mounts, Path::new("/srv/build/tenant-data")).unwrap(),
            Path::new("/srv/build")
        );
        assert_eq!(
            storage_mount(mounts, Path::new("/srv/build-other")).unwrap(),
            Path::new("/srv")
        );
    }

    #[test]
    fn old_runner_without_queue_age_is_omitted_instead_of_inventing_a_wait() {
        let mut reply = serde_json::json!({"ok":true,"health":{
            "permits_cap":16,"permits_held":3,"queue_depth":2}});
        assert!(runner(&reply, "vm-1".into()).is_none());
        reply["health"]["oldest_wait_s"] = 19.into();
        let runner = runner(&reply, "vm-1".into()).unwrap();
        assert_eq!(
            (
                runner.slots_total,
                runner.slots_used,
                runner.queued,
                runner.oldest_wait_s
            ),
            (16, 3, 2, 19)
        );
        assert!(runner.detail.is_none());
    }
}
