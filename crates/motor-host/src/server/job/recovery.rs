use super::*;
use crate::state::jobs::recovery::{Phase, Recovery};
use std::sync::mpsc::{self, Receiver};

pub(in crate::server) struct Worker {
    starting: Arc<AtomicBool>,
    result: Receiver<Result<String>>,
}

impl Runtime {
    pub(super) fn tick_recoveries(&self, now_ms: u64) -> Result<()> {
        let disks: Vec<_> = self
            .host
            .lock()
            .expect("host lock")
            .snapshot
            .per_job
            .disks
            .iter()
            .filter(|(_, d)| d.quarantined)
            .map(|(id, _)| id.clone())
            .collect();
        let mut failure = None;
        for disk_id in disks {
            if let Err(error) = self.tick_recovery(&disk_id, now_ms) {
                failure.get_or_insert(error);
            }
        }
        failure.map_or(Ok(()), Err)
    }

    fn tick_recovery(&self, disk_id: &str, now_ms: u64) -> Result<()> {
        let (disk, timeout) = {
            let mut host = self.host.lock().expect("host lock");
            host.reserve_recovery(disk_id, now_ms)?;
            let crate::config::HostSettings::PerJob(settings) = &host.settings else {
                return Ok(());
            };
            (
                host.snapshot.per_job.disks[disk_id].clone(),
                settings.recovery_timeout_ms,
            )
        };
        let Some(mut recovery) = disk.recovery.clone() else {
            return Ok(());
        };
        let mut workers = self.job_recoveries.lock().expect("recovery workers");
        if recovery
            .started_at_ms
            .is_some_and(|at| now_ms >= at.saturating_add(timeout))
        {
            self.host.lock().expect("host lock").recovery_phase(
                disk_id,
                &recovery.id,
                Phase::Deleting,
                now_ms,
                Some("timeout"),
            )?;
            recovery.phase = Phase::Deleting;
        }
        if let Some(worker) = workers.get(&recovery.id) {
            if let Ok(result) = worker.result.try_recv() {
                let (phase, result) = match result {
                    Ok(result) => (Phase::Stopping, result),
                    Err(error) => {
                        tracing::warn!(disk_id, recovery = recovery.id, %error, "workspace_recovery_failed");
                        (Phase::Deleting, "failed".into())
                    }
                };
                self.host.lock().expect("host lock").recovery_phase(
                    disk_id,
                    &recovery.id,
                    phase,
                    now_ms,
                    Some(&result),
                )?;
                if recovery.phase != Phase::Deleting {
                    recovery.phase = phase;
                }
            }
        }
        if matches!(recovery.phase, Phase::Stopping | Phase::Deleting) {
            // A start request may still be inside the helper. Its reply must
            // precede stop/deletion, including when the timeout already fired.
            if workers
                .get(&recovery.id)
                .is_some_and(|w| w.starting.load(Ordering::Acquire))
            {
                return Ok(());
            }
            if let Err(error) = self.control.stop_job(&recovery.id) {
                self.host.lock().expect("host lock").recovery_phase(
                    disk_id,
                    &recovery.id,
                    Phase::Deleting,
                    now_ms,
                    Some("stop_failed"),
                )?;
                recovery.phase = Phase::Deleting;
                // An unsuccessful start may have no helper record. Deletion
                // still independently refuses every disk held by any VM.
                tracing::warn!(disk_id, %error, "recovery_stop_before_delete_failed");
            }
            for report in self.control.job_exits()? {
                if report.job_id == recovery.id {
                    self.control.ack_job_exit(&report)?;
                }
            }
            let deleted = recovery.phase == Phase::Deleting;
            if deleted {
                self.control.delete_workspace(&disk.sub, disk_id)?;
            }
            self.job_resources.release(&recovery.id)?;
            self.host
                .lock()
                .expect("host lock")
                .recovery_finished(disk_id, deleted)?;
            workers.remove(&recovery.id);
            return Ok(());
        }
        if workers.contains_key(&recovery.id) {
            return Ok(());
        }
        let fresh = recovery.phase == Phase::Reserved;
        let assignment = match self
            .job_resources
            .acquire_recovery(disk_id, &disk, &recovery)
        {
            Ok(assignment) => assignment,
            Err(error) => {
                // A reservation manager can be unavailable temporarily. There
                // has been no VM start, so its recovery clock has not begun.
                tracing::warn!(disk_id, %error, "workspace_recovery_image_wait");
                return Ok(());
            }
        };
        if fresh {
            self.host.lock().expect("host lock").recovery_phase(
                disk_id,
                &recovery.id,
                Phase::Running,
                now_ms,
                None,
            )?;
        }
        let control = self.control.clone();
        let start = recovery_start(disk_id, &disk.sub, &recovery, &assignment);
        let starting = Arc::new(AtomicBool::new(fresh));
        let flag = starting.clone();
        let (tx, rx) = mpsc::channel();
        let remaining =
            timeout.saturating_sub(now_ms.saturating_sub(recovery.started_at_ms.unwrap_or(now_ms)));
        std::thread::spawn(move || {
            let result = (|| {
                if fresh {
                    control.start_job(&start)?;
                } else {
                    ensure!(control.adopt_job(&start.job_id)?, "recovery VM gone");
                }
                flag.store(false, Ordering::Release);
                let reply = control.job_admin(
                    &start.job_id,
                    &json!({"op":"recover_workspace", "budget_ms":remaining}),
                )?;
                ensure!(reply["ok"] == true, "guest recovery refused");
                let result = reply["recovery"]
                    .as_str()
                    .filter(|r| matches!(*r, "clean" | "repaired" | "reformatted"))
                    .ok_or_else(|| anyhow::anyhow!("missing guest recovery result"))?;
                Ok(result.into())
            })();
            flag.store(false, Ordering::Release);
            let _ = tx.send(result);
        });
        workers.insert(
            recovery.id,
            Worker {
                starting,
                result: rx,
            },
        );
        Ok(())
    }
}

fn recovery_start(
    disk_id: &str,
    account: &str,
    recovery: &Recovery,
    assignment: &Assignment,
) -> motor_vm_helper::job::Start {
    motor_vm_helper::job::Start {
        kind: motor_vm_helper::job::VmKind::Recovery,
        job_id: recovery.id.clone(),
        account: account.into(),
        workspace_disk_id: disk_id.into(),
        image_generation_id: assignment.image_generation_id.clone(),
        vcpus: 1,
        memory_mib: recovery.guest_ram_mib,
        burst_mib: None,
    }
}
