use super::*;
use crate::config::{HostSettings, PerJobSettings};
use crate::state::jobs::memory_limit_mib;

const MIB: u64 = 1024 * 1024;
const RETRY_AFTER_FAILURE_MS: u64 = 5000;

#[derive(Default)]
pub(crate) struct Controller {
    pending: Option<(String, u64)>,
    targets: BTreeMap<String, u64>,
    retry_after_ms: BTreeMap<String, u64>,
    disabled: bool,
    warned: bool,
    after_stop_sample_ms: Option<u64>,
}

struct Vm {
    job: String,
    overage_bytes: u64,
    started_ms: u64,
    maximum_target_mib: u64,
    guest_ram_mib: u64,
}

impl Runtime {
    pub(super) fn tick_balloon(&self, now_ms: u64) {
        let mut controller = self.job_balloon.lock().expect("job balloon");
        if controller.disabled {
            return;
        }
        if let Err(error) = self.balloon_step(&mut controller, now_ms) {
            if !controller.warned {
                tracing::warn!(detail = %error, "job_balloon_unavailable");
                controller.warned = true;
            }
            if error.is::<crate::helper::BalloonUnavailable>() {
                controller.disabled = true;
            }
        }
    }

    fn balloon_step(&self, controller: &mut Controller, now_ms: u64) -> Result<()> {
        let (settings, sample, executions) = {
            let host = self.host.lock().expect("host lock");
            let HostSettings::PerJob(settings) = &host.settings else {
                return Ok(());
            };
            let Some(sample) = host.fresh_host_resources(now_ms) else {
                // A stale sample cannot authorize either a reclaim or a stop.
                return Ok(());
            };
            let executions: Vec<_> = host
                .snapshot
                .per_job
                .executions
                .values()
                .filter(|e| {
                    e.boot_started
                        && e.runner_hello
                        && !e.ended
                        && e.evidence.as_ref().is_none_or(|v| {
                            v.stop.is_none() && v.exit.is_none() && v.gone_at_ms.is_none()
                        })
                })
                .cloned()
                .collect();
            (settings.clone(), sample, executions)
        };
        if controller
            .after_stop_sample_ms
            .is_some_and(|at| sample.at_ms <= at)
        {
            return Ok(());
        }
        controller.after_stop_sample_ms = None;
        controller
            .targets
            .retain(|job, _| executions.iter().any(|e| &e.claims.jti == job));
        controller
            .retry_after_ms
            .retain(|job, _| executions.iter().any(|e| &e.claims.jti == job));
        // A VM can exit between the lifecycle scan and its balloon request. Pace
        // failed balloon requests per VM without letting them hide an overage at
        // critical pressure.
        let balloon_retry_ready = |job: &str, controller: &Controller| {
            controller
                .retry_after_ms
                .get(job)
                .is_none_or(|after| now_ms >= *after)
        };
        if !sample.memory_pressure(settings.host_reserve_mib) {
            // Ending an episode does not deflate until the hysteresis is crossed.
            controller.pending = None;
            if sample.free_mem_mib(settings.host_reserve_mib) > settings.hysteresis_mib {
                for execution in executions {
                    let job = &execution.claims.jti;
                    // Also reset balloons inherited across a motor-host restart.
                    if balloon_retry_ready(job, controller)
                        && controller.targets.get(job) != Some(&0)
                    {
                        self.set_job_balloon(controller, job, 0, execution.guest_ram_mib, now_ms)?;
                    }
                }
            }
            return Ok(());
        }
        // Compare the same headroom as the pressure check, before subtracting the reserve.
        let critical = sample.free_mem_mib(0) <= settings.critical_free_mib;
        let mut vms = Vec::new();
        for execution in executions {
            let job = &execution.claims.jti;
            if critical || balloon_retry_ready(job, controller) {
                let resident = self.control.job_memory(job)?;
                vms.push(vm(&execution, resident, &settings));
            }
        }
        let largest_overage = || {
            vms.iter().filter(|v| v.overage_bytes > 0).max_by(|a, b| {
                (a.overage_bytes, a.started_ms, &a.job).cmp(&(
                    b.overage_bytes,
                    b.started_ms,
                    &b.job,
                ))
            })
        };
        let stop_victim = if critical {
            largest_overage()
        } else if let Some((job, deadline)) = &controller.pending {
            if now_ms < *deadline || sample.at_ms < *deadline {
                return Ok(());
            }
            vms.iter().find(|v| &v.job == job && v.overage_bytes > 0)
        } else {
            None
        };
        controller.pending = None;
        if let Some(victim) = stop_victim {
            // The stop is recorded before the helper can fail, and releases RAM
            // asynchronously. Wait for a newer sample before choosing again.
            controller.after_stop_sample_ms = Some(sample.at_ms);
            return self.stop_job_pressure(&victim.job, now_ms);
        }
        // No positive overage is never a reason to stop an innocent VM. While
        // either memory floor is breached, admission already returns 0.
        if critical {
            return Ok(());
        }
        if let Some(victim) = largest_overage() {
            let previous = controller.targets.get(&victim.job).copied().unwrap_or(0);
            let target = previous
                .saturating_add(victim.overage_bytes.div_ceil(MIB))
                .min(victim.maximum_target_mib);
            if target != previous {
                self.set_job_balloon(
                    controller,
                    &victim.job,
                    target,
                    victim.guest_ram_mib,
                    now_ms,
                )?;
            }
            // At the guest floor there may be nothing more to inflate. The
            // reclaim deadline still bounds how long a positive overage lives.
            controller.pending = Some((
                victim.job.clone(),
                now_ms.saturating_add(settings.reclaim_timeout_ms),
            ));
        }
        Ok(())
    }

    fn set_job_balloon(
        &self,
        controller: &mut Controller,
        job: &str,
        target: u64,
        guest_ram_mib: u64,
        now_ms: u64,
    ) -> Result<()> {
        log_balloon_request(job, target, guest_ram_mib);
        if let Err(error) = self.control.balloon_job(job, target) {
            controller.retry_after_ms.insert(
                job.to_owned(),
                now_ms.saturating_add(RETRY_AFTER_FAILURE_MS),
            );
            return Err(error);
        }
        controller.retry_after_ms.remove(job);
        controller.targets.insert(job.to_owned(), target);
        Ok(())
    }
}

fn log_balloon_request(job: &str, amount_mib: u64, guest_ram_mib: u64) {
    tracing::info!(
        job_id = job,
        amount_mib,
        guest_ram_mib,
        "job_balloon_request"
    );
}

fn vm(execution: &Execution, resident_bytes: u64, settings: &PerJobSettings) -> Vm {
    let stated = execution.claims.mem_mib;
    let limit = memory_limit_mib(&execution.claims).unwrap_or(stated);
    // Keep the same guest floor that sizing reserves above the job's limit:
    // ceil(limit / 0.75) - limit. It protects the kernel, runner and setup even
    // while the job uses its entire stated allocation. This is not a guest stat.
    let floor = execution.guest_ram_mib.saturating_sub(limit);
    Vm {
        job: execution.claims.jti.clone(),
        overage_bytes: resident_bytes.saturating_sub(
            stated
                .saturating_add(settings.per_vm_overhead_mib)
                .saturating_mul(MIB),
        ),
        started_ms: execution.boot_started_at_ms.unwrap_or(0),
        guest_ram_mib: execution.guest_ram_mib,
        maximum_target_mib: execution
            .guest_ram_mib
            .saturating_sub(stated.saturating_add(floor)),
    }
}

#[cfg(test)]
mod log_tests {
    #[test]
    fn balloon_request_logs_amount_and_guest_ram() {
        let logs = crate::test_logs::capture(|| super::log_balloon_request("job-1", 37, 512));
        assert_eq!(logs.len(), 1, "{logs:?}");
        assert!(logs[0].contains("job-1"), "{logs:?}");
        assert!(logs[0].contains("amount_mib=37"), "{logs:?}");
        assert!(logs[0].contains("guest_ram_mib=512"), "{logs:?}");
    }
}
