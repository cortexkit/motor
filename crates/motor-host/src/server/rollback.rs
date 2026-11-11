//! Returning to kept tenant disks never restores credentials from an old boot.
use super::*;

impl Runtime {
    pub(super) fn rollback_tenants(self: &Arc<Self>) -> Result<()> {
        let (tenants, disks, executions) = {
            let mut host = self.host.lock().expect("host lock");
            host.set_draining(true)?;
            ensure!(
                active_leases(&host) == 0,
                "tenant rollback requires drained leases"
            );
            ensure!(
                host.snapshot.per_job.fills.is_empty(),
                "tenant rollback requires completed fills"
            );
            ensure!(
                host.snapshot.per_job.executions.values().all(|e| e.ended),
                "tenant rollback requires stopped executions"
            );
            (
                host.tenants.values().cloned().collect::<Vec<_>>(),
                host.snapshot.per_job.disks.clone(),
                host.snapshot.per_job.executions.clone(),
            )
        };
        // Even an ended job may still be awaiting teardown. Deletion must wait
        // for the helper to prove every possible disk holder has stopped.
        for (job, execution) in &executions {
            if execution.boot_started {
                self.control.stop_job(job)?;
            }
        }
        for (id, disk) in &disks {
            if disk.deleted {
                continue;
            }
            if let Some(recovery) = &disk.recovery {
                self.control.stop_job(&recovery.id)?;
            }
            self.control.delete_workspace(&disk.sub, id)?;
            let mut host = self.host.lock().expect("host lock");
            let mut next = host.snapshot.clone();
            let disk = next.per_job.disks.get_mut(id).expect("rollback disk");
            disk.deleted = true;
            disk.have.clear();
            disk.queue.clear();
            disk.attached_job = None;
            disk.recovery = None;
            disk.quarantined = false;
            if let Some(storage) = &mut disk.storage {
                storage.growth_reservation_bytes = 0;
            }
            host.save(next)?;
        }
        for tenant in tenants {
            let operations = self.tenant_operations(&tenant.tenant)?;
            let _operation = operations.lock().expect("tenant operations lock");
            self.control.discard_tenant_snapshot(&tenant.tenant)?;
            self.host
                .lock()
                .expect("host lock")
                .guest_unready(&tenant.tenant);
            self.ensure_locked(&tenant.tenant, unix_ms())?;
            self.control
                .install_tenant_cache(&tenant.tenant, &tenant.sub)?;
        }
        let mut host = self.host.lock().expect("host lock");
        let mut next = host.snapshot.clone();
        next.per_job.tenant_rollback_pending = false;
        // Keep the operator's drain: undrain is safe only after every install
        // and its durable completion, never after a partially successful loop.
        host.save(next)?;
        Ok(())
    }
}
