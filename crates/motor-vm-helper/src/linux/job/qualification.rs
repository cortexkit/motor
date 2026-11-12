//! Root-only real-guest qualification against private scratch, not live helper
//! IPC or systemd Start. All authorization, disk binding and staging below use
//! the same functions as the production helper; no permission checks are stubbed.
use super::*;

pub struct Fixture {
    jobs: LinuxJobs,
}

impl Fixture {
    pub fn new(config: &Config, scratch: &Path) -> Result<Self> {
        if unsafe { libc::geteuid() } != 0 {
            return Err(Error("real-guest qualification requires root".into()));
        }
        let mut jobs = LinuxJobs::new(config);
        jobs.workspaces = scratch.join("workspaces");
        jobs.cache_store = scratch.join("cache-store");
        Ok(Self { jobs })
    }

    pub fn admit(&self, token: &str) -> Result<()> {
        self.jobs.cache_write_grants.admit(token)
    }

    /// Production's EnsureWorkspace, so a case that meets an existing disk keeps
    /// it and its original binding exactly as a real host's later job would.
    pub fn create(&self, request: job::CreateWorkspace) -> Result<()> {
        self.jobs.ensure_workspace(request)
    }

    pub fn install(&self, account: &str, set: &motor_protocol::RemoteCacheRequest) -> Result<()> {
        cache::install_fixture(&self.jobs.cache_store, account, set)
    }

    /// The caller owns the private VMM lifecycle and must not reuse a live disk.
    /// Production consumes the grant only after durably recording the Start
    /// request, so a second VM cannot open the same disk even after a crash.
    pub fn boot(
        &self,
        start: &Start,
        jail: &File,
        uid: u32,
        tap: u8,
    ) -> Result<(File, BootRequests)> {
        start.validate_fresh_at(SystemTime::now())?;
        let (registry, disk) = workspace::open_disk(
            &self.jobs.workspaces,
            &start.account,
            &start.workspace_disk_id,
        )?;
        let granted = take_job_write_grant(&self.jobs.cache_write_grants, start, &registry);
        let (_, requests) = stage_boot(
            &self.jobs.cache_store,
            start,
            &registry,
            self.jobs
                .settings
                .as_ref()
                .ok_or_else(|| Error("missing Job settings".into()))?,
            jail,
            uid,
            Some(tap),
            disk.metadata()?.len(),
            granted.as_ref(),
        )?;
        Ok((disk, requests))
    }
}
