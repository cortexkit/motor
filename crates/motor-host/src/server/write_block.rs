use super::*;

impl Runtime {
    pub(super) fn remote_cache_write_blocked(&self) -> bool {
        self.host
            .lock()
            .expect("host lock")
            .snapshot
            .remote_cache_write_blocked
    }

    pub(super) fn set_remote_cache_write_block(&self, request: &Value) -> Result<Value> {
        let blocked = request["blocked"]
            .as_bool()
            .context("blocked must be Boolean")?;
        ensure!(
            request == &json!({"op":"set_remote_cache_write_block","blocked":blocked}),
            "invalid write block request"
        );
        let _operation = self
            .write_block_operation
            .lock()
            .expect("write block operation");
        let (tenants, jobs) = {
            let mut host = self.host.lock().expect("host lock");
            host.set_remote_cache_write_block(blocked)?;
            let tenants = host
                .tenants
                .values()
                .map(|t| t.tenant.clone())
                .collect::<BTreeSet<_>>();
            let mut jobs = host
                .snapshot
                .per_job
                .executions
                .iter()
                .filter(|(_, job)| job.boot_started && !job.ended)
                .map(|(id, _)| id.clone())
                .collect::<BTreeSet<_>>();
            jobs.extend(host.snapshot.per_job.fills.keys().cloned());
            jobs.extend(
                host.snapshot
                    .per_job
                    .disks
                    .values()
                    .filter_map(|disk| disk.recovery.as_ref())
                    .map(|recovery| recovery.id.clone()),
            );
            (tenants, jobs)
        };
        let mut not_acknowledged = Vec::new();
        for (id, job) in tenants
            .into_iter()
            .map(|id| (id, false))
            .chain(jobs.into_iter().map(|id| (id, true)))
        {
            let running = if job {
                self.control.adopt_job(&id)
            } else {
                self.control.adopt(&id)
            };
            if matches!(running, Ok(false)) {
                continue;
            }
            let reply = if running.is_err() {
                Err(anyhow::anyhow!("runner state unavailable"))
            } else if job {
                self.control.job_admin(&id, request)
            } else {
                self.control.admin(&id, request)
            };
            if !reply.is_ok_and(|reply| reply["ok"] == true && reply["blocked"] == blocked) {
                tracing::warn!(
                    runner = id,
                    blocked,
                    "remote_cache_write_block_not_acknowledged"
                );
                not_acknowledged.push(id);
            }
        }
        Ok(json!({"ok":true,"blocked":blocked,"not_acknowledged":not_acknowledged}))
    }

    /// Re-send the persisted deny before relaying an admission, even if the
    /// runner restarted without the host noticing. Old runners may refuse the
    /// control but still serve read-only jobs; the persisted host deny must
    /// also be checked independently by write eligibility at admission.
    pub(super) fn sync_remote_cache_write_block(&self, id: &str, job: bool) {
        let _operation = self
            .write_block_operation
            .lock()
            .expect("write block operation");
        if !self
            .host
            .lock()
            .expect("host lock")
            .snapshot
            .remote_cache_write_blocked
        {
            return;
        }
        let request = json!({"op":"set_remote_cache_write_block","blocked":true});
        let reply = if job {
            self.control.job_admin(id, &request)
        } else {
            self.control.admin(id, &request)
        };
        if !reply.is_ok_and(|reply| reply["ok"] == true && reply["blocked"] == true) {
            tracing::warn!(
                runner = id,
                blocked = true,
                "remote_cache_write_block_not_acknowledged"
            );
        }
    }
}
