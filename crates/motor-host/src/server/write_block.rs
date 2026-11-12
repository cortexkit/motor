use super::*;

impl Runtime {
    pub(super) fn admit_job_cache_write(&self, first_claim: bool, claims: &Claims, token: &str) {
        if !first_claim
            || claims.purpose != Purpose::Run
            || claims.cache_write != Some(true)
            || self.sandbox_identity.is_some()
        {
            return;
        }
        // Serialize the check and helper request with the admin operation that
        // sets the write block, so it cannot be set between the two.
        let _operation = self
            .write_block_operation
            .lock()
            .expect("write block operation");
        if self.remote_cache_write_blocked() {
            return;
        }
        if let Err(error) = self.control.admit_cache_write(token) {
            // Cache authorization is optional: a helper refusal leaves the
            // execution read-only, not refused as a compute job.
            tracing::warn!(jti = %claims.jti, %error, "helper_cache_write_admission_refused");
        }
    }

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

    /// Read the persisted deny while serialized with block changes, before any
    /// admission relay. A dedicated Job VM must acknowledge it or be stopped;
    /// its boot may already have received a write key before the block was set.
    pub(super) fn sync_remote_cache_write_block(&self, id: &str, job: bool) -> Result<()> {
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
            return Ok(());
        }
        let request = json!({"op":"set_remote_cache_write_block","blocked":true});
        for _ in 0..if job { 3 } else { 1 } {
            let reply = if job {
                self.control.job_admin(id, &request)
            } else {
                self.control.admin(id, &request)
            };
            if reply.is_ok_and(|reply| reply["ok"] == true && reply["blocked"] == true) {
                return Ok(());
            }
            tracing::warn!(
                runner = id,
                blocked = true,
                "remote_cache_write_block_not_acknowledged"
            );
        }
        drop(_operation);
        if job {
            self.stop_job(id, true, true)?;
            anyhow::bail!("job VM did not acknowledge the cache write block after 3 attempts");
        }
        Ok(())
    }
}
