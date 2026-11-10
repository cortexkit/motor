use super::*;
use crate::config::{HostSettings, PerJobSettings};
use crate::state::jobs::memory_limit_mib;

const MIB: u64 = 1024 * 1024;

#[derive(Default)]
pub(crate) struct Controller {
    pending: Option<(String, u64)>,
    targets: BTreeMap<String, u64>,
    disabled: bool,
    warned: bool,
    after_stop_sample_ms: Option<u64>,
}

struct Vm {
    job: String,
    overage_bytes: u64,
    started_ms: u64,
    maximum_target_mib: u64,
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
        if !sample.memory_pressure(settings.host_reserve_mib) {
            // Ending an episode does not deflate until the hysteresis is crossed.
            controller.pending = None;
            if sample.free_mem_mib(settings.host_reserve_mib) > settings.hysteresis_mib {
                for execution in executions {
                    let job = &execution.claims.jti;
                    // Also reset balloons inherited across a motor-host restart.
                    if controller.targets.get(job) != Some(&0) {
                        self.control.balloon_job(job, 0)?;
                        controller.targets.insert(job.clone(), 0);
                    }
                }
            }
            return Ok(());
        }
        let mut vms = Vec::new();
        for execution in executions {
            let resident = self.control.job_memory(&execution.claims.jti)?;
            vms.push(vm(&execution, resident, &settings));
        }
        if let Some((job, deadline)) = &controller.pending {
            if now_ms < *deadline || sample.at_ms < *deadline {
                return Ok(());
            }
            let victim = vms.iter().find(|v| &v.job == job && v.overage_bytes > 0);
            if let Some(victim) = victim {
                self.stop_job_pressure(&victim.job, now_ms)?;
                controller.pending = None;
                controller.after_stop_sample_ms = Some(sample.at_ms);
                // Stop releases RAM asynchronously. Re-measure host memory on the
                // next tick before selecting another VM, never from this sample.
                return Ok(());
            }
            controller.pending = None;
        }
        let victim = vms
            .into_iter()
            .filter(|v| v.overage_bytes > 0)
            .max_by(|a, b| {
                (a.overage_bytes, a.started_ms, &a.job).cmp(&(
                    b.overage_bytes,
                    b.started_ms,
                    &b.job,
                ))
            });
        if let Some(victim) = victim {
            let previous = controller.targets.get(&victim.job).copied().unwrap_or(0);
            let target = previous
                .saturating_add(victim.overage_bytes.div_ceil(MIB))
                .min(victim.maximum_target_mib);
            if target != previous {
                self.control.balloon_job(&victim.job, target)?;
                controller.targets.insert(victim.job.clone(), target);
            }
            // At the guest floor there may be nothing more to inflate. The
            // reclaim deadline still bounds how long a positive overage lives.
            controller.pending = Some((
                victim.job,
                now_ms.saturating_add(settings.reclaim_timeout_ms),
            ));
        }
        // No positive overage is never a reason to stop an innocent VM. While
        // either memory floor is breached, admission already returns 0.
        Ok(())
    }
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
        maximum_target_mib: execution
            .guest_ram_mib
            .saturating_sub(stated.saturating_add(floor)),
    }
}
