use super::super::Host;
use crate::config::HostSettings;

const SAMPLE_INTERVAL_MS: u64 = 5_000;
const SAMPLE_FRESH_MS: u64 = 30_000;

#[derive(Default)]
pub(crate) struct ResidentMemory {
    sample: Option<(u64, u64)>,
    attempted_at_ms: Option<u64>,
    warned: bool,
}

#[derive(Default)]
pub(super) struct MemoryHold {
    pub held_back_mib: u64,
    pub resident_samples_stale: u64,
}

impl Host {
    fn resident_jobs(&self) -> impl Iterator<Item = (&str, u64)> {
        self.snapshot
            .per_job
            .executions
            .values()
            .filter(|job| job.boot_started && job.runner_hello && !job.ended)
            .map(|job| (job.claims.jti.as_str(), job.claims.mem_mib))
            .chain(
                self.snapshot
                    .per_job
                    .fills
                    .values()
                    .filter(|fill| fill.runner_hello)
                    .map(|fill| {
                        (
                            fill.fill.fill_id.as_str(),
                            fill_stated_mib(fill.guest_ram_mib),
                        )
                    }),
            )
    }

    pub(crate) fn resident_samples_due(&mut self, now_ms: u64) -> Vec<String> {
        if !matches!(self.settings, HostSettings::PerJob(_)) {
            return Vec::new();
        }
        let running: std::collections::BTreeSet<_> = self
            .resident_jobs()
            .map(|(job, _)| job.to_owned())
            .collect();
        self.job_residents.retain(|job, _| running.contains(job));
        running
            .into_iter()
            .filter(|job| {
                let resident = self.job_residents.entry(job.clone()).or_default();
                if resident
                    .attempted_at_ms
                    .is_some_and(|at_ms| now_ms.saturating_sub(at_ms) < SAMPLE_INTERVAL_MS)
                {
                    return false;
                }
                // Rate-limit attempts too, so unavailable helpers are not retried every tick.
                resident.attempted_at_ms = Some(now_ms);
                true
            })
            .collect()
    }

    pub(crate) fn record_job_resident(
        &mut self,
        job: &str,
        now_ms: u64,
        result: anyhow::Result<u64>,
    ) {
        if !self.resident_jobs().any(|(running, _)| running == job) {
            self.job_residents.remove(job);
            return;
        }
        let resident = self.job_residents.entry(job.to_owned()).or_default();
        match result {
            Ok(bytes) => resident.sample = Some((bytes / (1024 * 1024), now_ms)),
            Err(error) if !resident.warned => {
                tracing::warn!(job, detail = %error, "job_resident_memory_unavailable");
                resident.warned = true;
            }
            Err(_) => {}
        }
    }

    pub(super) fn job_memory_hold(&self, now_ms: u64) -> MemoryHold {
        let HostSettings::PerJob(settings) = &self.settings else {
            return MemoryHold::default();
        };
        let mut hold = MemoryHold::default();
        for (job, stated_mib) in self.resident_jobs() {
            let sample = self
                .job_residents
                .get(job)
                .and_then(|resident| resident.sample)
                .filter(|(_, at_ms)| now_ms.saturating_sub(*at_ms) <= SAMPLE_FRESH_MS);
            if sample.is_none() {
                hold.resident_samples_stale += 1;
            }
            hold.held_back_mib = hold.held_back_mib.saturating_add(unused_hold_mib(
                settings.unused_memory_hold_percent,
                stated_mib,
                settings.per_vm_overhead_mib,
                sample.map_or(0, |(mib, _)| mib),
            ));
        }
        hold
    }
}

fn unused_hold_mib(percent: u8, stated_mib: u64, overhead_mib: u64, resident_mib: u64) -> u64 {
    let unused = (u128::from(stated_mib) + u128::from(overhead_mib))
        .saturating_sub(u128::from(resident_mib));
    let hold = (u128::from(percent) * unused).div_ceil(100);
    hold.min(u128::from(u64::MAX)) as u64
}

fn fill_stated_mib(guest_ram_mib: u64) -> u64 {
    // floor(3 * guest / 4) exactly inverts the fill's ceil(4 * stated / 3) sizing.
    (u128::from(guest_ram_mib) * 3 / 4) as u64
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hold_arithmetic_widens_before_adding_multiplying_and_rounding() {
        assert_eq!(unused_hold_mib(25, 0, 0, 0), 0);
        assert_eq!(unused_hold_mib(0, u64::MAX, u64::MAX, 0), 0);
        assert_eq!(unused_hold_mib(100, u64::MAX, 0, u64::MAX), 0);
        assert_eq!(unused_hold_mib(100, u64::MAX, u64::MAX, 0), u64::MAX);
        assert_eq!(
            unused_hold_mib(25, u64::MAX, u64::MAX, 0),
            9_223_372_036_854_775_808
        );
        assert_eq!(
            unused_hold_mib(25, u64::MAX, u64::MAX, u64::MAX),
            4_611_686_018_427_387_904
        );
        assert_eq!(unused_hold_mib(25, 0, 1, 0), 1);
    }

    #[test]
    fn fill_sizing_recovers_stated_memory_without_a_stored_field() {
        for stated in [0u64, 1, 2, 3, 4, 24_576, 24_577, 32_768] {
            let guest = (stated * 4).div_ceil(3);
            assert_eq!(fill_stated_mib(guest), stated);
        }
        assert_eq!(fill_stated_mib(u64::MAX), 13_835_058_055_282_163_711);
    }
}
