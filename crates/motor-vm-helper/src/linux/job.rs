//! Cold boots use ordinary guest RAM, not the legacy userfaultfd snapshot pool.
use super::{cache, cgroup, fs, process, tenant};
use crate::{
    config::{Config, JobVms},
    job::{self, ExitReport, Operation, Start, Workspace},
    Error, Peer, Reply, Response, Result,
};
use motor_protocol::{REMOTE_CACHE_DRIVE_NONCE_BYTES, REMOTE_CACHE_NONCE_CMDLINE_KEY};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::{
    collections::{BTreeMap, BTreeSet},
    fs::{File, OpenOptions},
    os::{
        fd::AsRawFd,
        unix::{
            fs::{MetadataExt, OpenOptionsExt, PermissionsExt},
            process::CommandExt,
        },
    },
    path::{Path, PathBuf},
    process::{Child, Command, Stdio},
    time::{Duration, SystemTime},
};

const JOB_OOM_SCORE: i32 = 1000;
const JOB_BOOT_CMDLINE_MAX_BYTES: usize = 1536;
const TOMBSTONE_RETENTION: Duration = Duration::from_secs(48 * 60 * 60);
const TOMBSTONE_PRUNE_INTERVAL: Duration = Duration::from_secs(60);
// Without huge pages, the guest defaults to reporting only whole 4 MiB blocks.
// Report 128 KiB blocks so fragmented pages freed from file cache reach the host.
const JOB_BOOT_ARGS: &str = "console=ttyS0 reboot=k panic=1 pci=off root=/dev/vda ro init=/sbin/motor-init net.ifnames=0 systemd.unified_cgroup_hierarchy=1 motor.per_job=1 page_reporting.page_reporting_order=5";

mod conntrack;
mod memory;
mod namespace;
pub(super) mod network;
#[cfg(feature = "linux-root-tests")]
#[doc(hidden)]
pub mod qualification;
mod workspace;

pub(super) fn image_copy_exclusive_bytes(file: &File) -> Result<u64> {
    workspace::exclusive_bytes(file)
}
use super::tenant_network::Tap;

pub(super) fn network_allocations() -> Result<Vec<Tap>> {
    let records = network::read_records(Path::new(job::RECORDS))?;
    let taps: Vec<_> = records.iter().filter_map(Record::tap).collect();
    super::tenant_network::validate_taps(&taps)?;
    Ok(taps)
}

#[derive(Debug, Clone, Deserialize, Serialize)]
// Rollbacks must be able to read records written by a newer helper.
struct Record {
    start: Start,
    uid: u32,
    disk_device: u64,
    disk_inode: u64,
    released: bool,
    oom_kill: Option<u64>,
    #[serde(default)]
    exit_acknowledged: bool,
    #[serde(default)]
    tap_index: Option<u8>,
    #[serde(default)]
    egress_bytes: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    fill_exit_success: Option<bool>,
}

impl Record {
    fn is_tombstone(&self) -> bool {
        self.released && self.exit_acknowledged && self.tap_index.is_none()
    }

    fn tap(&self) -> Option<Tap> {
        self.tap_index.map(|tap_index| Tap {
            tap_index,
            uid: self.uid,
        })
    }

    fn pending_exit(&self) -> Option<ExitReport> {
        self.oom_kill
            .filter(|_| !self.exit_acknowledged)
            .map(|oom_kill| ExitReport {
                job_id: self.start.job_id.clone(),
                oom_kill,
                egress_bytes: self.egress_bytes,
            })
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ExitStep {
    Captured,
    Recorded,
    Removed,
    Delivery,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
enum CollectionFailure {
    Reap,
    Cgroup,
    Credentials,
    Evidence,
    Network,
    MissingEgress,
}

pub struct LinuxJobs {
    pub(super) cache_write_grants: crate::cache_write::Authority,
    settings: Option<JobVms>,
    groups: PathBuf,
    records_path: PathBuf,
    jail: PathBuf,
    cache_store: PathBuf,
    workspaces: PathBuf,
    records: BTreeMap<String, Record>,
    children: BTreeMap<String, Child>,
    collection_failures: BTreeMap<String, BTreeSet<CollectionFailure>>,
    next_prune: Option<SystemTime>,
    network: Box<dyn network::Access>,
    #[cfg(test)]
    exit_fault: Option<ExitStep>,
    #[cfg(test)]
    remove_test_group: Option<fn(&Path) -> std::io::Result<()>>,
}

impl LinuxJobs {
    pub fn images(&mut self, op: crate::images::Operation) -> Result<Response> {
        op.validate()?;
        if let crate::images::Operation::Remove(id) = &op {
            if self
                .records
                .values()
                .any(|r| !r.released && &r.start.image_generation_id == id)
            {
                return Err(Error("image generation is held by a VM".into()));
            }
        }
        super::images::handle(op)
    }
    pub fn create_workspace(&self, request: job::CreateWorkspace) -> Result<()> {
        self.check_workspace_size(request.size_bytes)?;
        let grant = self.disk_grant(request.cache_write_job_id.as_deref());
        workspace::create_bound(
            &self.workspaces,
            request,
            job::WorkspaceRole::Job,
            grant.as_ref(),
        )
    }
    fn disk_grant(&self, id: Option<&str>) -> Option<crate::cache_write::Grant> {
        self.cache_write_grants.lock().ok()?.get(id?).cloned()
    }
    fn check_workspace_size(&self, size: u64) -> Result<()> {
        let maximum = self
            .settings
            .as_ref()
            .and_then(|s| s.workspace_disk_max_bytes)
            .ok_or_else(|| Error("workspace disk maximum is required".into()))?;
        if size == 0 || !size.is_multiple_of(512) || size > maximum {
            return Err(Error(
                "workspace disk size exceeds configured maximum or is invalid".into(),
            ));
        }
        Ok(())
    }
    /// A seed/clone holds this shared lease until it has synced its new disk.
    /// Base eviction takes the exclusive lease under the same helper mutex.
    pub fn lease_base(&self, account: &str, disk_id: &str) -> Result<File> {
        workspace::base_lease(&self.workspaces, account, disk_id, false)
    }
    pub fn new(config: &Config) -> Self {
        Self {
            cache_write_grants: std::sync::Arc::new(std::sync::Mutex::new(
                crate::cache_write::Grants::new(config),
            )),
            settings: config.job_vms.clone(),
            groups: cgroup::JOB_SLICE.into(),
            records_path: job::RECORDS.into(),
            jail: Path::new(super::BUILD_ROOT).join("jail/jobs"),
            cache_store: cache::STORE.into(),
            workspaces: job::WORKSPACES.into(),
            records: BTreeMap::new(),
            children: BTreeMap::new(),
            collection_failures: BTreeMap::new(),
            next_prune: None,
            network: Box::new(network::System(config.clone())),
            #[cfg(test)]
            exit_fault: None,
            #[cfg(test)]
            remove_test_group: None,
        }
    }

    fn group(&self, id: &str) -> PathBuf {
        self.groups.join(format!("job-{id}"))
    }
    fn boundary(&self, id: &str) -> PathBuf {
        self.jail.join(id)
    }
    fn root(&self, id: &str) -> PathBuf {
        self.boundary(id).join(format!("firecracker/job-{id}/root"))
    }
    fn save(&self, record: &Record) -> Result<()> {
        tenant::durable_write(
            &self
                .records_path
                .join(format!("{}.json", record.start.job_id)),
            &serde_json::to_vec(record).map_err(|e| Error(e.to_string()))?,
        )
    }

    pub fn reconcile(&mut self) -> Result<()> {
        self.reconcile_records(network::read_records(&self.records_path)?)?;
        let abandoned: Vec<_> = self
            .records
            .values()
            .filter(|r| r.start.kind == job::VmKind::Fill)
            .filter_map(|r| match self.pid(r) {
                Ok(None) => Some(Ok(r.start.clone())),
                Ok(Some(_)) => None,
                Err(e) => Some(Err(e)),
            })
            .collect::<Result<_>>()?;
        for start in abandoned {
            if let Ok((registry, _)) =
                workspace::open_disk(&self.workspaces, &start.account, &start.workspace_disk_id)
            {
                if registry.role == job::WorkspaceRole::Fill {
                    let fill = workspace::read_fill(&self.workspaces, &start.workspace_disk_id)?;
                    self.finish_fill(&fill, false)?;
                }
            }
        }
        if self.workspaces.try_exists()? {
            for entry in std::fs::read_dir(&self.workspaces)? {
                let entry = entry?;
                let Some(id) = entry.file_name().to_str().map(str::to_owned) else {
                    continue;
                };
                if !id.starts_with("fill-") {
                    continue;
                }
                // A live fill survives a helper restart just like a job.
                if self
                    .records
                    .values()
                    .any(|r| !r.released && r.start.workspace_disk_id == id)
                {
                    continue;
                }
                if !entry.path().join("registry.json").try_exists()? {
                    trusted_directory(&entry.path())?;
                    std::fs::remove_dir_all(entry.path())?;
                    File::open(&self.workspaces)?.sync_all()?;
                    continue;
                }
                let registry: Workspace =
                    serde_json::from_reader(fs::trusted_file(&entry.path().join("registry.json"))?)
                        .map_err(|e| Error(e.to_string()))?;
                if registry.role == job::WorkspaceRole::Fill {
                    workspace::delete_role(
                        &self.workspaces,
                        &registry.account,
                        &id,
                        job::WorkspaceRole::Fill,
                    )?;
                }
            }
        }
        workspace::finish_deletions(&self.workspaces)
    }

    fn reconcile_records(&mut self, records: Vec<Record>) -> Result<()> {
        let tenants = self.network.tenants()?;
        if let Some(settings) = &self.settings {
            network::check_capacity(settings.uid_count, tenants.len())?;
        }
        for record in records {
            let settings = self
                .settings
                .as_ref()
                .ok_or_else(|| Error("job VM settings required to adopt jobs".into()))?;
            if !(settings.uid_base..settings.uid_base + settings.uid_count).contains(&record.uid) {
                return Err(Error("job record uid outside configured range".into()));
            }
            self.adopt_record(record)?;
        }
        let mut taps = tenants;
        taps.extend(self.records.values().filter_map(Record::tap));
        super::tenant_network::validate_taps(&taps)?;
        self.maintain()?;
        let retained = self
            .records
            .values()
            .filter(|r| !r.is_tombstone())
            .map(|r| r.start.job_id.clone())
            .collect();
        self.network.reconcile_namespaces(&retained)
    }

    fn adopt_record(&mut self, record: Record) -> Result<()> {
        // A trusted, fresh cgroup identifies the process even if the helper
        // died between spawning jailer and recording its pid. Never signal a
        // stale saved pid or move an unrelated process into this group.
        if !record.released && self.pid(&record)?.is_some() && record.oom_kill.is_some() {
            return Err(Error("exited job record has a running VMM".into()));
        }
        if record.exit_acknowledged && record.oom_kill.is_none() {
            return Err(Error("job acknowledgement has no exit evidence".into()));
        }
        if record
            .tap_index
            .is_some_and(|index| !(1..=254).contains(&index))
        {
            return Err(Error("job record TAP outside range 1..254".into()));
        }
        self.records.insert(record.start.job_id.clone(), record);
        Ok(())
    }

    fn pid(&self, record: &Record) -> Result<Option<u32>> {
        if record.released {
            return Ok(None);
        }
        let path = self.group(&record.start.job_id);
        let text = match std::fs::read_to_string(path.join("cgroup.procs")) {
            Ok(text) => text,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
            Err(e) => return Err(e.into()),
        };
        let pids: Vec<_> = text.split_whitespace().collect();
        if pids.len() > 1 {
            return Err(Error("unexpected processes in job VM cgroup".into()));
        }
        let Some(pid) = pids.first() else {
            return Ok(None);
        };
        let pid = pid
            .parse::<u32>()
            .map_err(|_| Error("invalid job VMM pid".into()))?;
        let proc = PathBuf::from(format!("/proc/{pid}"));
        let comm = match std::fs::read_to_string(proc.join("comm")) {
            Ok(comm) => comm,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
            Err(e) => return Err(e.into()),
        };
        if comm.trim() != "firecracker" || effective_uid(pid)? != record.uid {
            return Err(Error("job cgroup process identity mismatch".into()));
        }
        Ok(Some(pid))
    }

    fn reap(&mut self) {
        let mut ended = Vec::new();
        let mut failed = Vec::new();
        for (id, child) in &mut self.children {
            match child.try_wait() {
                Ok(Some(status)) => ended.push((id.clone(), status.success(), status)),
                Ok(None) => (),
                Err(error) => failed.push((id.clone(), error)),
            }
        }
        for (id, success, status) in ended {
            eprintln!("job {id} VMM exited: {status}");
            log_console_tail(&id, &self.boundary(&id));
            let mut record = self.records[&id].clone();
            if record.start.kind == job::VmKind::Fill {
                record.fill_exit_success = Some(success);
                if let Err(error) = self.save(&record) {
                    self.log_collection_failure(&id, CollectionFailure::Evidence, &error);
                    continue;
                }
                self.records.insert(id.clone(), record);
            }
            self.children.remove(&id);
        }
        for (id, error) in failed {
            self.log_collection_failure(&id, CollectionFailure::Reap, &error);
        }
    }

    fn log_collection_failure(
        &mut self,
        id: &str,
        kind: CollectionFailure,
        error: &dyn std::fmt::Display,
    ) -> bool {
        if self
            .collection_failures
            .entry(id.into())
            .or_default()
            .insert(kind)
        {
            eprintln!("job {id} exit evidence {kind:?}: {error}");
            true
        } else {
            false
        }
    }

    pub fn collect_exits(&mut self) -> Result<()> {
        self.reap();
        let ids: Vec<_> = self
            .records
            .values()
            .filter(|record| !record.is_tombstone())
            .map(|record| record.start.job_id.clone())
            .collect();
        for id in ids {
            // A failed record stays reserved and retries independently. It must
            // never disable another VM's stop, relay, start or exit delivery.
            let _ = self.collect_exit(&id);
        }
        Ok(())
    }

    pub fn maintain(&mut self) -> Result<()> {
        self.maintain_at(SystemTime::now())
    }

    fn maintain_at(&mut self, now: SystemTime) -> Result<()> {
        let collection = self.collect_exits();
        // Maintenance runs every 100 ms. Retention needs only a minute's
        // cadence, not a metadata lookup per tombstone on every tick.
        let pruning = if self.next_prune.is_none_or(|deadline| now >= deadline) {
            self.prune_records(now).map(|()| {
                self.next_prune = now.checked_add(TOMBSTONE_PRUNE_INTERVAL);
            })
        } else {
            Ok(())
        };
        collection.and(pruning)
    }

    fn prune_records(&mut self, now: SystemTime) -> Result<()> {
        let ids: Vec<_> = self
            .records
            .values()
            .filter(|record| record.is_tombstone())
            .map(|record| record.start.job_id.clone())
            .collect();
        for id in ids {
            let path = self.records_path.join(format!("{id}.json"));
            // Atomic rewrites refresh mtime, so the full replay window begins
            // only after both release and acknowledgement have been recorded.
            let modified = std::fs::metadata(&path)?.modified()?;
            if now
                .duration_since(modified)
                .is_ok_and(|age| age >= TOMBSTONE_RETENTION)
            {
                std::fs::remove_file(&path)?;
                File::open(&self.records_path)?.sync_all()?;
                self.records.remove(&id);
                self.collection_failures.remove(&id);
            }
        }
        Ok(())
    }

    fn collect_exit(&mut self, id: &str) -> Result<()> {
        let mut kind = CollectionFailure::Cgroup;
        let result = self.collect_exit_inner(id, &mut kind);
        if let Err(error) = &result {
            self.log_collection_failure(id, kind, error);
        }
        result
    }

    fn collect_exit_inner(&mut self, id: &str, kind: &mut CollectionFailure) -> Result<()> {
        let group = self.group(id);
        if !group.try_exists()? {
            // A reboot can erase cgroups. No count is better than an invented 0.
            *kind = CollectionFailure::Credentials;
            self.remove_credentials(id)?;
            *kind = CollectionFailure::Network;
            return self.remove_network(id);
        }
        if !std::fs::read_to_string(group.join("cgroup.procs"))?
            .trim()
            .is_empty()
        {
            return Ok(());
        }
        *kind = CollectionFailure::Credentials;
        self.remove_credentials(id)?;
        let mut record = self.records.get(id).unwrap().clone();
        if record.oom_kill.is_none() {
            *kind = CollectionFailure::Evidence;
            record.oom_kill = Some(read_oom_kill(&group)?);
            // No live VMM can add traffic after the empty-cgroup proof. Capture
            // both counters in one durable write before destroying either one.
            record.egress_bytes = record
                .tap()
                .map(|tap| self.network.egress_bytes(&tap))
                .transpose()?
                .flatten();
            if record.tap_index.is_some() && record.egress_bytes.is_none() {
                self.log_collection_failure(
                    id,
                    CollectionFailure::MissingEgress,
                    &"counter missing; egress unmeasured",
                );
            }
            self.exit_checkpoint(ExitStep::Captured)?;
            self.save(&record)?;
            self.records.insert(id.into(), record);
            self.exit_checkpoint(ExitStep::Recorded)?;
        }
        *kind = CollectionFailure::Network;
        self.remove_network(id)?;
        *kind = CollectionFailure::Cgroup;
        self.remove_empty_group(&group)?;
        self.exit_checkpoint(ExitStep::Removed)
    }

    fn remove_network(&mut self, id: &str) -> Result<()> {
        let mut record = self.records.get(id).unwrap().clone();
        if let Some(tap) = record.tap() {
            self.network.remove(id, &tap)?;
            self.network.remove_namespace(id)?;
            // A crash before this write retries idempotent removal, never the
            // measurement. Until it succeeds the allocation cannot be reused.
            record.tap_index = None;
            self.save(&record)?;
            self.records.insert(id.into(), record);
        } else {
            self.network.remove_namespace(id)?;
        }
        Ok(())
    }

    fn remove_credentials(&self, id: &str) -> Result<()> {
        let source = self.boundary(id).join(cache::IMAGE);
        if !source.try_exists()? {
            return Ok(());
        }
        let target = self.root(id).join(cache::IMAGE);
        let mounts = std::fs::read_to_string("/proc/self/mountinfo")?;
        if mounts
            .lines()
            .filter_map(|line| line.split_whitespace().nth(4))
            .any(|path| super::unescape_mount(path) == target)
        {
            fs::unmount(&target)?;
        }
        if target.try_exists()? {
            std::fs::remove_file(target)?;
        }
        std::fs::remove_file(source)?;
        File::open(self.boundary(id))?.sync_all()?;
        Ok(())
    }

    fn remove_empty_group(&self, group: &Path) -> Result<()> {
        // Unit fixtures model cgroup control files with ordinary files. Only
        // deletion is replaced; emptiness and evidence checks run unchanged.
        #[cfg(test)]
        if let Some(remove) = self.remove_test_group {
            return Ok(remove(group)?);
        }
        Ok(std::fs::remove_dir(group)?)
    }

    fn exit_checkpoint(&mut self, step: ExitStep) -> Result<()> {
        #[cfg(test)]
        if self.exit_fault == Some(step) {
            self.exit_fault = None;
            return Err(Error(format!("injected exit fault at {step:?}")));
        }
        let _ = step;
        Ok(())
    }

    fn acknowledge_exit(&mut self, report: ExitReport) -> Result<Response> {
        let mut record = self
            .records
            .get(&report.job_id)
            .cloned()
            .ok_or_else(|| Error("unknown job VM".into()))?;
        if record.oom_kill != Some(report.oom_kill) || record.egress_bytes != report.egress_bytes {
            return Err(Error(
                "job exit report does not match durable evidence".into(),
            ));
        }
        if !record.exit_acknowledged {
            record.exit_acknowledged = true;
            self.save(&record)?;
            self.records.insert(report.job_id, record);
        }
        Ok(Response::new(Reply::Ok))
    }

    pub fn handle(&mut self, op: Operation, peer: Peer) -> Result<Response> {
        op.validate()?;
        // Invalid acknowledgements must not change any evidence or cgroups.
        if let Operation::AckExit(report) = op {
            return self.acknowledge_exit(report);
        }
        // Stop must not even wait for collection of an unrelated record.
        if !matches!(op, Operation::Stop(_)) {
            self.collect_exits()?;
        }
        match op {
            Operation::PrepareFill(fill) => {
                self.check_workspace_size(fill.size_bytes)?;
                self.disk_available_identity(&fill.disk_id())?;
                workspace::prepare_fill(&self.workspaces, &fill)?;
                Ok(Response::new(Reply::Ok))
            }
            Operation::FinishFill { fill, published } => {
                let published = self.finish_fill(&fill, published)?;
                Ok(Response::new(Reply::FillFinished { published }))
            }
            Operation::SeedWorkspace(seed) => {
                self.disk_available_identity(&seed.disk_id)?;
                self.disk_available_identity(&seed.base_id)?;
                let grant = self.disk_grant(seed.cache_write_job_id.as_deref());
                workspace::seed_bound(&self.workspaces, &seed, grant.as_ref())?;
                Ok(Response::new(Reply::Ok))
            }
            Operation::WorkspaceUsage { account, disk_id } => {
                let exclusive_bytes = (|| -> Result<u64> {
                    let (_, disk) = workspace::open_disk(&self.workspaces, &account, &disk_id)?;
                    let meta = disk.metadata()?;
                    if let Some(record) = self
                        .records
                        .values()
                        .find(|r| !r.released && r.start.workspace_disk_id == disk_id)
                    {
                        if record.start.account != account
                            || (meta.dev(), meta.ino()) != (record.disk_device, record.disk_inode)
                            || ![0, record.uid].contains(&meta.uid())
                        {
                            return Err(Error("workspace usage identity changed".into()));
                        }
                    } else if meta.uid() != 0 {
                        return Err(Error("idle workspace is not root owned".into()));
                    }
                    workspace::exclusive_bytes(&disk)
                })()
                .ok();
                Ok(Response::new(Reply::WorkspaceUsage { exclusive_bytes }))
            }
            Operation::EnsureWorkspace {
                account,
                disk_id,
                size_bytes,
                cache_write_job_id,
            } => {
                self.check_workspace_size(size_bytes)?;
                self.disk_available_identity(&disk_id)?;
                let grant = self.disk_grant(cache_write_job_id.as_deref());
                workspace::ensure_bound(
                    &self.workspaces,
                    job::CreateWorkspace {
                        account,
                        workspace_disk_id: disk_id,
                        size_bytes,
                        cache_write_job_id,
                    },
                    grant.as_ref(),
                )?;
                Ok(Response::new(Reply::Ok))
            }
            Operation::EvictBase { account, disk_id } => {
                self.disk_available_identity(&disk_id)?;
                if self.workspaces.join(&disk_id).try_exists()? {
                    let _lease = workspace::base_lease(&self.workspaces, &account, &disk_id, true)?;
                    workspace::delete_role(
                        &self.workspaces,
                        &account,
                        &disk_id,
                        job::WorkspaceRole::Base,
                    )?;
                } else {
                    workspace::finish_deletions(&self.workspaces)?;
                }
                Ok(Response::new(Reply::Ok))
            }
            Operation::DeleteWorkspace { account, disk_id } => {
                self.disk_available_identity(&disk_id)?;
                workspace::delete(&self.workspaces, &account, &disk_id)?;
                Ok(Response::new(Reply::Ok))
            }
            Operation::Start(start) => self.start(start, peer),
            Operation::Adopt(id) => self.status(&id),
            Operation::Memory(id) => self.memory(&id),
            Operation::Balloon(id, amount_mib) => self.balloon(&id, amount_mib),
            Operation::Stop(id) => {
                self.stop(&id)?;
                self.status(&id)
            }
            Operation::Relay(id, port) => {
                let record = self
                    .records
                    .get(&id)
                    .ok_or_else(|| Error("unknown job VM".into()))?;
                if self.pid(record)?.is_none() {
                    return Err(Error("job VM is not running".into()));
                }
                Ok(Response {
                    reply: Reply::Relay { fd_count: 1 },
                    fds: vec![tenant::connect_relay(&self.root(&id), record.uid, port)?],
                })
            }
            Operation::Exits => {
                let exits = self
                    .records
                    .values()
                    .filter_map(Record::pending_exit)
                    .collect();
                self.exit_checkpoint(ExitStep::Delivery)?;
                Ok(Response::new(Reply::JobExits { exits }))
            }
            Operation::AckExit(_) => unreachable!(),
        }
    }

    fn disk_available_identity(&self, disk_id: &str) -> Result<()> {
        if self
            .records
            .values()
            .any(|r| !r.released && r.start.workspace_disk_id == disk_id)
        {
            return Err(Error("workspace disk still held by a VM".into()));
        }
        Ok(())
    }

    fn finish_fill(&mut self, fill: &job::Fill, mut published: bool) -> Result<bool> {
        let id = fill.disk_id();
        if self.workspaces.join(&id).try_exists()? {
            let (registry, _) = workspace::open_disk(&self.workspaces, &fill.account, &id)?;
            if workspace::read_fill(&self.workspaces, &id)? != *fill {
                return Err(Error("fill provenance mismatch".into()));
            }
            if registry.role == job::WorkspaceRole::Base {
                return if published {
                    Ok(true)
                } else {
                    Err(Error("published base cannot be discarded as a fill".into()))
                };
            }
        } else {
            published = false;
        }
        let record = self.records.get(&fill.fill_id).cloned();
        if published {
            published = if let Some(record) = record.as_ref() {
                self.pid(record)?.is_none()
                    && record.oom_kill == Some(0)
                    && record.fill_exit_success == Some(true)
            } else {
                false
            };
        }
        if let Some(record) = record {
            if record.start.kind != job::VmKind::Fill
                || record.start.account != fill.account
                || record.start.workspace_disk_id != id
            {
                return Err(Error("fill execution identity mismatch".into()));
            }
            self.stop(&fill.fill_id)?;
        }
        self.disk_available_identity(&id)?;
        if published {
            workspace::promote(&self.workspaces, fill)?;
        } else if self.workspaces.join(&id).try_exists()? {
            workspace::delete_role(
                &self.workspaces,
                &fill.account,
                &id,
                job::WorkspaceRole::Fill,
            )?;
        }
        Ok(published)
    }

    fn status(&self, id: &str) -> Result<Response> {
        let Some(record) = self.records.get(id) else {
            self.prove_absent(id)?;
            return Ok(Response::new(Reply::Job {
                known: false,
                running: false,
                vmm_pid: None,
                tap_index: None,
                exit_report: None,
            }));
        };
        let pid = self.pid(record)?;
        Ok(Response::new(Reply::Job {
            known: true,
            running: pid.is_some(),
            vmm_pid: pid,
            tap_index: record.tap_index,
            exit_report: record.pending_exit(),
        }))
    }

    fn prove_absent(&self, id: &str) -> Result<()> {
        job::validate_job_id(id)?;
        match std::fs::read_to_string(self.group(id).join("cgroup.procs")) {
            Ok(procs) if !procs.trim().is_empty() => {
                return Err(Error("unrecorded job cgroup still holds processes".into()));
            }
            Ok(_) => {}
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => return Err(error.into()),
        }
        // Every jailer inherits this job's cgroup. Without its durable record,
        // do not guess process ownership or clean a surviving jail: require
        // both an empty cgroup and no jail before reporting an absent VM.
        match std::fs::symlink_metadata(self.boundary(id)) {
            Ok(_) => Err(Error("unrecorded job jail requires reconciliation".into())),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
            Err(error) => Err(error.into()),
        }
    }

    fn disk_available(&self, start: &Start, device: u64, inode: u64) -> Result<()> {
        if self.records.values().any(|r| {
            !r.released
                && ((r.start.account == start.account
                    && r.start.workspace_disk_id == start.workspace_disk_id)
                    || (r.disk_device == device && r.disk_inode == inode))
        }) {
            return Err(Error(
                "workspace disk already attached or awaiting cleanup".into(),
            ));
        }
        Ok(())
    }

    fn allocate_uid(&self, settings: &JobVms) -> Result<u32> {
        // Uids are separate from tenant VMMs, legacy slots, and host accounts.
        // Never reuse one while its previous jail still owns disk resources.
        for uid in settings.uid_base..settings.uid_base + settings.uid_count {
            if self.records.values().any(|r| !r.released && r.uid == uid) {
                continue;
            }
            process::refuse_passwd_entries(uid, 1)?;
            let mut occupied = false;
            for entry in std::fs::read_dir("/proc")? {
                let entry = entry?;
                if entry.file_name().to_string_lossy().parse::<u32>().is_ok() {
                    match effective_uid(entry.file_name().to_string_lossy().parse().unwrap()) {
                        Ok(actual) => occupied |= actual == uid,
                        Err(_) if !entry.path().exists() => {}
                        Err(e) => return Err(e),
                    }
                }
            }
            if !occupied {
                return Ok(uid);
            }
        }
        Err(Error("job VMM uid capacity exhausted".into()))
    }

    fn start(&mut self, start: Start, peer: Peer) -> Result<Response> {
        start.validate_fresh_at(SystemTime::now())?;
        let settings = self
            .settings
            .clone()
            .ok_or_else(|| Error("job VM settings are required".into()))?;
        if self.records.contains_key(&start.job_id) {
            return Err(Error("job id already booted; use adopt_job".into()));
        }
        // Keep a start for an already attached identity out of all filesystem
        // preparation, including opening a disk owned by its running VMM.
        self.disk_available(&start, 0, 0)?;
        if self.group(&start.job_id).try_exists()? || self.boundary(&start.job_id).try_exists()? {
            return Err(Error(
                "stale job cgroup or jail requires reconciliation".into(),
            ));
        }
        let workspace = self.workspaces.join(&start.workspace_disk_id);
        let registry: Workspace =
            serde_json::from_reader(fs::trusted_file(&workspace.join("registry.json"))?)
                .map_err(|e| Error(format!("invalid workspace registry: {e}")))?;
        job::validate_account(&registry.account)?;
        job::validate_id(&registry.workspace_disk_id)?;
        if registry.account != start.account
            || registry.workspace_disk_id != start.workspace_disk_id
        {
            return Err(Error("workspace disk identity or account mismatch".into()));
        }
        if !registry.role.permits(start.kind)
            || (registry.role == job::WorkspaceRole::Base
                && workspace.join("fill.json").try_exists()?)
        {
            return Ok(Response::new(Reply::StartRefused {
                case: crate::tenant::StartRefusalCase::WorkspaceRole,
                message: "workspace disk role does not permit requested VM kind".into(),
            }));
        }
        if start.kind == job::VmKind::Fill {
            let fill = workspace::read_fill(&self.workspaces, &start.workspace_disk_id)?;
            if fill.account != start.account
                || fill.fill_id != start.job_id
                || fill.disk_id() != start.workspace_disk_id
            {
                return Err(Error("fill start provenance mismatch".into()));
            }
        }
        let disk = fs::trusted_file(&workspace.join("workspace.img"))?;
        let meta = disk.metadata()?;
        early_boot_command_line(
            &start,
            meta.len(),
            fixed_path_policy(&settings, &registry, start.kind),
        )?;
        self.disk_available(&start, meta.dev(), meta.ino())?;
        // The helper is a singleton; its serialized requests and durable intent
        // records exclude concurrent use by both workspace id and device/inode.
        // A local flock would expire when start returns, not when the VM exits.
        let image = Path::new(job::IMAGES).join(&start.image_generation_id);
        let hashes: crate::images::Manifest =
            serde_json::from_reader(fs::trusted_file(&image.join("manifest.json"))?)
                .map_err(|e| Error(format!("invalid sealed image manifest: {e}")))?;
        hashes.validate()?;
        let inputs = [
            (fs::trusted_file(&image.join("kernel"))?, "kernel", true),
            (
                fs::trusted_file(&image.join("rootfs.ext4"))?,
                "rootfs.ext4",
                true,
            ),
            (
                fs::trusted_file(
                    &image
                        .join("accounts")
                        .join(&start.account)
                        .join("toolchain.ext4"),
                )?,
                "toolchain.ext4",
                true,
            ),
        ];
        for (file, _, _) in &inputs {
            if file.metadata()?.mode() & 0o222 != 0 || file.metadata()?.mode() & 0o004 == 0 {
                return Err(Error("job images must be readable by jailed VMMs".into()));
            }
        }
        eprintln!("job_backing_files job_id={} generation={} kernel_sha256={} rootfs_sha256={} toolchain_sha256={}", start.job_id, start.image_generation_id, hashes.kernel.sha256, hashes.rootfs.sha256, hashes.toolchain.sha256);
        let binaries = Path::new("/usr/local/lib/firecracker/1.17.0");
        for name in ["jailer", "firecracker"] {
            fs::trusted_file(&binaries.join(name))?;
            let output = Command::new(binaries.join(name))
                .arg("--version")
                .output()?;
            if !output.status.success()
                || !String::from_utf8_lossy(&output.stdout).contains("1.17.0")
            {
                return Err(Error(
                    "job VMs require Firecracker and jailer 1.17.0".into(),
                ));
            }
        }
        let uid = self.allocate_uid(&settings)?;
        let tenants = self.network.tenants()?;
        network::check_capacity(settings.uid_count, tenants.len())?;
        let tap_index = if start.kind == job::VmKind::Recovery {
            None
        } else {
            Some(network::allocate(&tenants, &self.records)?)
        };
        let host_score = host_oom_score(peer.uid)?;
        let helper_score = read_oom_score(std::process::id())?;
        check_oom_order(host_score, helper_score)?;
        cgroup::prepare_job_slice(settings.memory_max_bytes)?;
        for path in [&self.records_path, &self.jail] {
            trusted_directory(path)?;
        }
        let record = Record {
            start: start.clone(),
            uid,
            disk_device: meta.dev(),
            disk_inode: meta.ino(),
            released: false,
            oom_kill: None,
            exit_acknowledged: false,
            tap_index,
            egress_bytes: None,
            fill_exit_success: None,
        };
        // The intent is durable before any jail or VMM can open the disk. A
        // crash in setup keeps it reserved until explicit stop has cleaned up.
        self.save(&record)?;
        self.records.insert(start.job_id.clone(), record);
        // Durable intent already prevents another VM, including after restart.
        // Failures before intent leave the admission grant reusable by a retry.
        let job_write = take_job_write_grant(&self.cache_write_grants, &start, &registry)?;
        let id = &start.job_id;
        let boundary = self.boundary(id);
        let root = self.root(id);
        let group = self.group(id);
        let launch = (|| -> Result<()> {
            let netns = self.network.create_namespace(id)?;
            if let Some(tap_index) = tap_index {
                self.prepare_network(id, tenants, Tap { tap_index, uid })?;
            }
            cgroup::create_job(&group, start.vcpus)?;
            super::create_boundary(&boundary)?;
            let boundary_fd = OpenOptions::new()
                .read(true)
                .custom_flags(libc::O_DIRECTORY | libc::O_NOFOLLOW)
                .open(&boundary)?;
            let (nonce, requests) = stage_boot(
                &self.cache_store,
                &start,
                &registry,
                &settings,
                &boundary_fd,
                uid,
                tap_index,
                meta.len(),
                job_write,
            )?;
            std::fs::create_dir_all(&root)?;
            std::fs::set_permissions(&root, std::fs::Permissions::from_mode(0o700))?;
            fs::chown(&File::open(&root)?, uid)?;
            for (file, name, readonly) in &inputs {
                tenant::bind(file, &root.join(name), *readonly)?;
            }
            fs::chown(&disk, uid)?;
            tenant::bind(&disk, &root.join("workspace.img"), false)?;
            if nonce.is_some() {
                let image = OpenOptions::new()
                    .read(true)
                    .custom_flags(libc::O_NOFOLLOW)
                    .open(boundary.join(cache::IMAGE))?;
                tenant::bind(&image, &root.join(cache::IMAGE), true)?;
            }
            let procs = OpenOptions::new()
                .write(true)
                .open(group.join("cgroup.procs"))?;
            // The guest's serial console (kernel and systemd messages) is the
            // only record of why a VM halted or died. Keep it in the root-only
            // boundary, outside the jail, until the VM ends; its tail then goes
            // to the helper journal before the boundary is removed.
            let console = OpenOptions::new()
                .write(true)
                .create_new(true)
                .mode(0o600)
                .custom_flags(libc::O_NOFOLLOW | libc::O_APPEND)
                .open(boundary.join(CONSOLE_LOG))?;
            let mut command = jailer_command(binaries, &boundary, &netns, id, uid);
            command.stdout(console);
            // v1.17.0 keeps inherited membership with its default cgroup-version
            // 1 and no --cgroup arguments (jailer/src/env.rs:213-270, 658-661).
            // Do not add --cgroup-version 2: its no-argument branch can move the
            // jailer out of this job group, invalidating the empty-group proof.
            unsafe {
                command.pre_exec(move || super::raw_place(procs.as_raw_fd()));
            }
            let child = command.spawn()?;
            let pid = child.id();
            self.children.insert(id.clone(), child);
            // A /proc/fd alias avoids Unix-domain socket path length limits.
            let root_fd = File::open(&root)?;
            let short_root = PathBuf::from(format!("/proc/self/fd/{}", root_fd.as_raw_fd()));
            drop(super::wait_socket(
                &short_root.join("api.sock"),
                Duration::from_secs(10),
            )?);
            set_job_oom_score(pid, host_score, helper_score)?;
            for (path, body) in requests {
                tenant::api(&short_root, "PUT", path, &body)?;
            }
            Ok(())
        })();
        if let Err(error) = launch {
            if let Err(cleanup) = self.stop(id) {
                return Err(Error(format!("{error}; job cleanup failed: {cleanup}")));
            }
            return Err(error);
        }
        self.status(id)
    }

    fn prepare_network(&mut self, id: &str, mut tenants: Vec<Tap>, tap: Tap) -> Result<()> {
        tenants.extend(self.records.values().filter_map(Record::tap));
        self.network.install(&tenants)?;
        self.network.ensure(id, &tap)
    }

    #[cfg(all(test, feature = "linux-root-tests"))]
    fn stage_start_cache(
        &self,
        start: &Start,
        boundary: &File,
        uid: u32,
    ) -> Result<Option<String>> {
        cache::stage(&self.cache_store, &start.account, start.kind, boundary, uid)
    }

    fn stop(&mut self, id: &str) -> Result<()> {
        let Some(record) = self.records.get(id).cloned() else {
            return self.prove_absent(id);
        };
        if record.released {
            return Ok(());
        }
        let group = self.group(id);
        if group.try_exists()? {
            std::fs::write(group.join("cgroup.kill"), "1")?;
            if let Some(mut child) = self.children.remove(id) {
                let status = child.wait()?;
                eprintln!("job {id} VMM stopped by the helper: {status}");
                log_console_tail(id, &self.boundary(id));
            }
            tenant::wait_empty(&group)?;
        }
        self.collect_exit(id)?;
        let mut record = self.records.get(id).unwrap().clone();
        tenant::cleanup_boundary(&self.boundary(id))?;
        let disk_path = self
            .workspaces
            .join(&record.start.workspace_disk_id)
            .join("workspace.img");
        let disk = open_owned_disk(&disk_path, &record)?;
        disk.set_permissions(std::fs::Permissions::from_mode(0o600))?;
        fs::chown(&disk, 0)?;
        record.released = true;
        self.save(&record)?;
        self.records.insert(id.into(), record);
        Ok(())
    }
}

const CONSOLE_LOG: &str = "console.log";
/// The console's last bytes are what explain a halt or crash; a guest can write
/// as much as it likes, so only this much is ever read back.
const CONSOLE_TAIL_BYTES: u64 = 16 * 1024;

#[doc(hidden)]
pub type BootRequests = Vec<(&'static str, Value)>;

fn take_job_write_grant(
    authority: &crate::cache_write::Authority,
    start: &Start,
    registry: &Workspace,
) -> Result<bool> {
    let mut grants = authority
        .lock()
        .map_err(|_| Error("cache grant lock poisoned".into()))?;
    let permitted = grants
        .get(&start.job_id)
        .is_some_and(|grant| grant.permits(start, registry));
    grants.consume(&start.job_id);
    Ok(permitted)
}

#[allow(clippy::too_many_arguments)]
fn stage_boot(
    store: &Path,
    start: &Start,
    registry: &Workspace,
    settings: &JobVms,
    boundary: &File,
    uid: u32,
    tap: Option<u8>,
    workspace_bytes: u64,
    permitted: bool,
) -> Result<(Option<String>, BootRequests)> {
    let (nonce, job_write) =
        cache::stage_with_write(store, &start.account, start.kind, boundary, uid, permitted)?;
    let mut requests = recorded_boot_requests(
        start,
        tap,
        nonce.as_deref(),
        workspace_bytes,
        registry,
        settings,
    )?;
    authorize_job_write_boot(&mut requests, start, nonce.as_deref(), job_write)?;
    Ok((nonce, requests))
}

/// Copies the tail of a job VM's serial console to the helper journal, one
/// line per entry with control characters replaced, so guest-written bytes
/// cannot forge journal fields or terminal sequences.
fn log_console_tail(id: &str, boundary: &Path) {
    use std::io::{Read, Seek, SeekFrom};
    let Ok(mut file) = OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NOFOLLOW)
        .open(boundary.join(CONSOLE_LOG))
    else {
        return;
    };
    let length = file.metadata().map_or(0, |meta| meta.len());
    let start = length.saturating_sub(CONSOLE_TAIL_BYTES);
    let mut tail = Vec::new();
    if file.seek(SeekFrom::Start(start)).is_err()
        || file
            .take(CONSOLE_TAIL_BYTES)
            .read_to_end(&mut tail)
            .is_err()
    {
        return;
    }
    for line in String::from_utf8_lossy(&tail)
        .lines()
        .skip(usize::from(start > 0))
    {
        let line: String = line
            .chars()
            .take(300)
            .map(|c| if c.is_control() { '?' } else { c })
            .collect();
        if !line.trim().is_empty() {
            eprintln!("job {id} console: {line}");
        }
    }
}

fn jailer_command(binaries: &Path, boundary: &Path, netns: &Path, id: &str, uid: u32) -> Command {
    let mut command = Command::new(binaries.join("jailer"));
    command
        .args(["--id", &format!("job-{id}"), "--exec-file"])
        .arg(binaries.join("firecracker"))
        .args([
            "--uid",
            &uid.to_string(),
            "--gid",
            &uid.to_string(),
            "--chroot-base-dir",
        ])
        .arg(boundary)
        .arg("--netns")
        .arg(netns)
        .args(["--", "--api-sock", "/api.sock"])
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::inherit())
        .env_clear();
    command
}

fn trusted_directory(path: &Path) -> Result<()> {
    for ancestor in path.ancestors() {
        if ancestor.try_exists()? {
            fs::trusted(ancestor, &std::fs::symlink_metadata(ancestor)?)?;
        }
    }
    std::fs::create_dir_all(path)?;
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o700))?;
    for ancestor in path.ancestors() {
        fs::trusted(ancestor, &std::fs::symlink_metadata(ancestor)?)?;
    }
    Ok(())
}

fn open_owned_disk(path: &Path, record: &Record) -> Result<File> {
    for ancestor in path.parent().unwrap().ancestors() {
        fs::trusted(ancestor, &std::fs::symlink_metadata(ancestor)?)?;
    }
    let disk = OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK)
        .open(path)?;
    let meta = disk.metadata()?;
    if !meta.is_file()
        || ![0, record.uid].contains(&meta.uid())
        || (meta.dev(), meta.ino()) != (record.disk_device, record.disk_inode)
    {
        return Err(Error("job workspace inode or owner changed".into()));
    }
    Ok(disk)
}

fn read_oom_kill(group: &Path) -> Result<u64> {
    std::fs::read_to_string(group.join("memory.events"))?
        .lines()
        .find_map(|line| line.strip_prefix("oom_kill "))
        .and_then(|n| n.parse().ok())
        .ok_or_else(|| Error("missing final job oom_kill count".into()))
}

fn read_oom_score(pid: u32) -> Result<i32> {
    std::fs::read_to_string(format!("/proc/{pid}/oom_score_adj"))?
        .trim()
        .parse()
        .map_err(|_| Error("invalid oom_score_adj".into()))
}

fn effective_uid(pid: u32) -> Result<u32> {
    // /proc/<pid>'s inode can be root-owned for a non-dumpable process even
    // when its effective uid is unprivileged. Read credentials, not ownership.
    std::fs::read_to_string(format!("/proc/{pid}/status"))?
        .lines()
        .find_map(|line| line.strip_prefix("Uid:"))
        .and_then(|uids| uids.split_whitespace().nth(1))
        .and_then(|uid| uid.parse().ok())
        .ok_or_else(|| Error("missing process credentials".into()))
}

fn check_oom_order(host_score: i32, helper_score: i32) -> Result<()> {
    if JOB_OOM_SCORE <= host_score || JOB_OOM_SCORE <= helper_score {
        return Err(Error(
            "job VMM OOM score must exceed host and helper".into(),
        ));
    }
    Ok(())
}

fn host_oom_score(uid: u32) -> Result<i32> {
    let mut highest = -1000;
    // SO_PEERCRED authenticates the host uid. Checking all its processes also
    // covers a host in the middle of restarting, without trusting a caller pid.
    for entry in std::fs::read_dir("/proc")? {
        let entry = entry?;
        let Ok(pid) = entry.file_name().to_string_lossy().parse::<u32>() else {
            continue;
        };
        match effective_uid(pid) {
            Ok(actual) if actual == uid => match read_oom_score(pid) {
                Ok(score) => highest = highest.max(score),
                Err(_) if !entry.path().exists() => {}
                Err(e) => return Err(e),
            },
            Ok(_) => {}
            Err(_) if !entry.path().exists() => {}
            Err(e) => return Err(e),
        }
    }
    Ok(highest)
}

fn set_job_oom_score(pid: u32, host_score: i32, helper_score: i32) -> Result<()> {
    check_oom_order(host_score, helper_score)?;
    if std::fs::read_to_string(format!("/proc/{pid}/comm"))?.trim() != "firecracker" {
        return Err(Error("expected job firecracker process".into()));
    }
    std::fs::write(
        format!("/proc/{pid}/oom_score_adj"),
        JOB_OOM_SCORE.to_string(),
    )?;
    if read_oom_score(pid)? != JOB_OOM_SCORE {
        return Err(Error("job VMM OOM score readback mismatch".into()));
    }
    Ok(())
}

// These arguments are deliberately not secret: job code can read /proc/cmdline.
// Disk policy matches tenant provisioning; the parity test pins its constants.
fn runner_args(
    start: &Start,
    disk_bytes: u64,
    remote_cache: bool,
    fixed_paths: Option<(&str, &[String])>,
) -> Result<String> {
    start.validate()?;
    if disk_bytes == 0 || !disk_bytes.is_multiple_of(512) {
        return Err(Error("invalid job workspace disk length".into()));
    }
    let cache = (disk_bytes / (4 * 1024_u64.pow(3))).min(200);
    let floor = disk_bytes / 10;
    let eviction = disk_bytes / 100 * 35 + disk_bytes % 100 * 35 / 100;
    let remote = if remote_cache {
        "--remote-cache true "
    } else {
        ""
    };
    let mut text = format!(
        "--backend host --tenant-vm --job-network veth --state-root /tenant/state \
         --max-weight {0} --rustc-slots {0} \
         --return-chunked-max-bytes 134217728 --return-budget-bytes 2147483648 \
         --sccache-size-gib {cache} --free-space-floor-bytes {floor} \
         --free-space-eviction-bytes {eviction} {remote}\
         --workspace-root /Users --owned-workspace-root /Users \
         --workspace-root /home --owned-workspace-root /home",
        start.vcpus
    );
    if let Some((account, roots)) = fixed_paths.filter(|(_, roots)| !roots.is_empty()) {
        text.push_str(&format!(" --account-subject {account}"));
        for root in roots {
            text.push_str(&format!(" --fixed-path-repository {root}"));
        }
    }
    crate::tenant::activation::validate_runner_args(&text)?;
    Ok(text)
}

fn early_boot_command_line(
    start: &Start,
    workspace_bytes: u64,
    fixed_paths: Option<(&str, &[String])>,
) -> Result<String> {
    // Credential staging happens later. Budget for its longest possible line
    // now so command-line refusal precedes any namespace, cgroup or VM work.
    let nonce = "0".repeat(REMOTE_CACHE_DRIVE_NONCE_BYTES * 2);
    let line = boot_command_line(start, Some(&nonce), workspace_bytes, fixed_paths)?;
    if start.kind == job::VmKind::Fill {
        checked_boot_command_line(format!("{line} motor.fill=1"))
    } else if start.kind == job::VmKind::Job {
        checked_boot_command_line(format!("{line} motor.cache_write=1"))?;
        Ok(line)
    } else {
        Ok(line)
    }
}

fn fixed_path_policy<'a>(
    settings: &'a JobVms,
    registry: &'a Workspace,
    kind: job::VmKind,
) -> Option<(&'a str, &'a [String])> {
    if !matches!(kind, job::VmKind::Job | job::VmKind::Fill) {
        return None;
    }
    // Neither the start request nor guest-exported source metadata chooses roots.
    // The account comes from the root-owned disk registry, not the host agent.
    settings
        .fixed_path_repositories
        .get(&registry.account)
        .filter(|roots| !roots.is_empty())
        .map(|roots| (registry.account.as_str(), roots.as_slice()))
}

fn checked_boot_command_line(line: String) -> Result<String> {
    // Both Firecracker and x86 Linux allow 2048 bytes including NUL. Reserve
    // 512 bytes for Firecracker's appended virtio-mmio device declarations.
    if line.len() >= JOB_BOOT_CMDLINE_MAX_BYTES {
        return Err(Error(
            "job VM boot arguments exceed command-line budget".into(),
        ));
    }
    Ok(line)
}

fn authorize_fill_boot(requests: &mut [(&str, Value)], role: job::WorkspaceRole) -> Result<()> {
    // Only the root-owned disk registry grants fill authority. In particular,
    // Start.kind alone must never give a job disk access to the fill operation.
    if role == job::WorkspaceRole::Fill {
        let (_, boot) = requests
            .iter_mut()
            .find(|(path, _)| *path == "/boot-source")
            .ok_or_else(|| Error("missing boot source".into()))?;
        let args = boot["boot_args"]
            .as_str()
            .ok_or_else(|| Error("missing boot arguments".into()))?;
        boot["boot_args"] = checked_boot_command_line(format!("{args} motor.fill=1"))?.into();
    }
    Ok(())
}

fn authorize_job_write_boot(
    requests: &mut [(&str, Value)],
    start: &Start,
    nonce: Option<&str>,
    job_write: bool,
) -> Result<()> {
    if job_write {
        if start.kind != job::VmKind::Job || start.cache_write_blocked || nonce.is_none() {
            return Err(Error(
                "job cache write boot lacks staged credentials".into(),
            ));
        }
        let (_, boot) = requests
            .iter_mut()
            .find(|(path, _)| *path == "/boot-source")
            .ok_or_else(|| Error("missing boot source".into()))?;
        let args = boot["boot_args"]
            .as_str()
            .ok_or_else(|| Error("missing boot arguments".into()))?;
        boot["boot_args"] =
            checked_boot_command_line(format!("{args} motor.cache_write=1"))?.into();
    }
    Ok(())
}

/// Boot settings derived from the disk registry opened by the root helper.
/// Unlike `Start.kind`, that record is not an input from the host agent.
#[doc(hidden)]
pub fn recorded_boot_requests(
    start: &Start,
    tap_index: Option<u8>,
    nonce: Option<&str>,
    workspace_bytes: u64,
    registry: &job::Workspace,
    settings: &JobVms,
) -> Result<Vec<(&'static str, Value)>> {
    if registry.account != start.account
        || registry.workspace_disk_id != start.workspace_disk_id
        || !registry.role.permits(start.kind)
    {
        return Err(Error("workspace registry does not permit boot".into()));
    }
    settings.validate_fixed_path_repositories()?;
    let mut requests = boot_requests_with_fixed_paths(
        start,
        tap_index,
        nonce,
        workspace_bytes,
        fixed_path_policy(settings, registry, start.kind),
    )?;
    authorize_fill_boot(&mut requests, registry.role)?;
    Ok(requests)
}

fn boot_command_line(
    start: &Start,
    nonce: Option<&str>,
    workspace_bytes: u64,
    fixed_paths: Option<(&str, &[String])>,
) -> Result<String> {
    if nonce.is_some_and(|nonce| {
        nonce.len() != REMOTE_CACHE_DRIVE_NONCE_BYTES * 2
            || !nonce
                .bytes()
                .all(|byte| matches!(byte, b'0'..=b'9' | b'a'..=b'f'))
    }) {
        return Err(Error("invalid job cache nonce".into()));
    }
    let nonce = if start.kind == job::VmKind::Recovery {
        None
    } else {
        nonce
    };
    let args = if start.kind == job::VmKind::Recovery {
        format!("{JOB_BOOT_ARGS} motor.recovery=1")
    } else {
        let text = runner_args(start, workspace_bytes, nonce.is_some(), fixed_paths)?;
        let encoded: String = text
            .as_bytes()
            .iter()
            .map(|byte| format!("{byte:02x}"))
            .collect();
        format!("{JOB_BOOT_ARGS} motor.runner_args={encoded}")
    };
    let boot_args = match nonce {
        Some(nonce) => format!("{args} {REMOTE_CACHE_NONCE_CMDLINE_KEY}={nonce}"),
        None => args,
    };
    checked_boot_command_line(boot_args)
}

/// Client Git bundles stream to the guest runner over vsock, never a drive.
/// Credentials use a read-only drive, bound to this boot by a nonce rather
/// than a device letter or virtio serial.
#[doc(hidden)]
pub fn boot_requests(
    start: &Start,
    tap_index: Option<u8>,
    nonce: Option<&str>,
    workspace_bytes: u64,
) -> Result<Vec<(&'static str, Value)>> {
    boot_requests_with_fixed_paths(start, tap_index, nonce, workspace_bytes, None)
}

fn boot_requests_with_fixed_paths(
    start: &Start,
    tap_index: Option<u8>,
    nonce: Option<&str>,
    workspace_bytes: u64,
    fixed_paths: Option<(&str, &[String])>,
) -> Result<Vec<(&'static str, Value)>> {
    let boot_args = boot_command_line(start, nonce, workspace_bytes, fixed_paths)?;
    let (tap_index, nonce) = if start.kind == job::VmKind::Recovery {
        (None, None)
    } else {
        (tap_index, nonce)
    };
    let mut requests = vec![
        (
            "/machine-config",
            json!({"vcpu_count":start.vcpus,"mem_size_mib":start.memory_mib,"smt":false}),
        ),
        (
            "/boot-source",
            json!({"kernel_image_path":"/kernel","boot_args":boot_args}),
        ),
        (
            "/drives/root",
            json!({"drive_id":"root","path_on_host":"/rootfs.ext4","is_root_device":true,"is_read_only":true}),
        ),
        (
            "/pmem/toolchain",
            json!({"id":"toolchain","path_on_host":"/toolchain.ext4","read_only":true}),
        ),
        (
            "/drives/workspace",
            json!({"drive_id":"workspace","path_on_host":"/workspace.img","is_root_device":false,"is_read_only":false}),
        ),
        ("/vsock", json!({"guest_cid":3,"uds_path":"/vsock.sock"})),
        (
            "/balloon",
            json!({"amount_mib":0,"deflate_on_oom":true,"free_page_reporting":true,"stats_polling_interval_s":0}),
        ),
    ];
    if let Some(tap_index) = tap_index {
        requests.push((
            "/network-interfaces/eth0",
            json!({"iface_id":"eth0","host_dev_name":format!("mtap{tap_index}"),"guest_mac":format!("02:fc:00:00:{tap_index:02x}:02")}),
        ));
    }
    if nonce.is_some() {
        // Firecracker refuses resource IDs with anything but alphanumerics and
        // underscores (400 InvalidID). The guest finds this drive by its nonce,
        // so the ID only has to be valid.
        requests.push(("/drives/remote_cache", json!({"drive_id":"remote_cache","path_on_host":"/remote-cache.img","is_root_device":false,"is_read_only":true})));
    }
    requests.push(("/actions", json!({"action_type":"InstanceStart"})));
    Ok(requests)
}

#[cfg(test)]
mod tests;
