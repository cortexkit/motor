mod compaction;
mod journal;
#[cfg(test)]
mod journal_tests;
#[cfg(test)]
pub(crate) use journal::durable_value;

use fs2::FileExt;
use motor_protocol::*;
use serde::{Deserialize, Serialize};
use std::{
    collections::BTreeMap,
    fs::{self, File, OpenOptions},
    io::{self, Write},
    path::{Path, PathBuf},
    sync::{Arc, Mutex, Weak},
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};

// Keep routing and local refusal history for at least the runner's replay horizon,
// starting at successful terminal delivery or the caller's latest access.
const RETENTION_MS: u64 = 24 * 60 * 60 * 1000;
const RETENTION_RENEWAL_MS: u64 = 60 * 1000;
// Only records whose stored keys are exactly local_terminal, pending_cancel,
// and queue_wait_limit_s use this migration horizon. Agent jobs can run without
// a timeout, so mint time alone cannot expire current job records.
const LEGACY_RETENTION_MS: u64 = 49 * 60 * 60 * 1000;
const MAX_VERIFIED_TIPS: usize = 256;
const MAX_FAILED_TIPS: usize = 256;
const FAILED_TIP_TTL: Duration = Duration::from_secs(60);
/// The stored `routed_direct_reason` of a job the broker answered `no_host_fits`.
const NO_HOST_FITS: &str = "no_host_fits";

fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis()
        .try_into()
        .unwrap_or(u64::MAX)
}

#[derive(Clone, Default, Serialize, Deserialize)]
pub(crate) struct Job {
    #[serde(skip)]
    pub(crate) legacy_shape: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub hosted: Option<crate::hosted::HostedJobState>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub return_result: Option<ReturnResult>,
    /// The `cwd` of an `exec-remote/v1` request that asked for returned files.
    /// Only such requests set it. It marks the stored return result as one the
    /// agent route forwards, and turns the absolute paths the runner reports
    /// back into the cwd-relative paths the caller asked for.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub return_cwd: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub direct_secret_hash: Option<[u8; 32]>,
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub gate_job: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub runner_instance: Option<String>,
    /// Snapshot the endpoint as well as the instance: names can repeat across hosts,
    /// and reordering or editing configuration must not move an admitted job.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub windows_runner: Option<crate::WindowsRunner>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub attribution: Option<JobAttribution>,
    pub queue_wait_limit_s: u64,
    pub pending_cancel: bool,
    pub local_terminal: Option<TerminalRecord>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub terminal_observed_at_ms: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub terminal_delivered_at_ms: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub retain_until_ms: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub workspace_key: Option<String>,
    /// Set when a job spilled to a hosted runner, its hosted route failed
    /// before any Run frame reached a hosted runner, and ck-motor resubmitted
    /// it to the build server. The build server
    /// keeps the withdrawn first submission as a tombstone under the caller's
    /// job ID and never admits that ID again, so the resubmission runs there
    /// under this ID. The caller keeps using its original job ID throughout.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub build_server_job_id: Option<Uuid>,
    /// Why a job first routed to hosted runners by `hosted_repositories` was
    /// sent to the build server instead, before any hosted placement. Set
    /// together with removing the job's hosted route, so a restart or a later
    /// attach treats it as an ordinary build-server job. The only reason so
    /// far is `no_host_fits`: no hosted host could ever hold its vCPUs.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub routed_direct_reason: Option<String>,
}
impl Job {
    fn lacks_newer_fields(&self) -> bool {
        self.hosted.is_none()
            && self.return_result.is_none()
            && self.return_cwd.is_none()
            && self.direct_secret_hash.is_none()
            && !self.gate_job
            && self.runner_instance.is_none()
            && self.windows_runner.is_none()
            && self.attribution.is_none()
            && self.terminal_observed_at_ms.is_none()
            && self.terminal_delivered_at_ms.is_none()
            && self.retain_until_ms.is_none()
            && self.workspace_key.is_none()
            && self.build_server_job_id.is_none()
            && self.routed_direct_reason.is_none()
    }

    fn legacy_expired(&self, id: Uuid, now: u64) -> bool {
        // Legacy shapes predate terminal tracking; the later of the two also
        // carries runner_instance, so that field alone doesn't exclude it.
        self.legacy_shape
            && Job {
                runner_instance: None,
                ..self.clone()
            }
            .lacks_newer_fields()
            && id.get_version_num() == 7
            && id.get_timestamp().is_some_and(|timestamp| {
                let (seconds, nanos) = timestamp.to_unix();
                let minted = seconds * 1000 + u64::from(nanos) / 1_000_000;
                now.checked_sub(minted)
                    .is_some_and(|age| age >= LEGACY_RETENTION_MS)
            })
    }

    /// The hosted route that governs this job, if any. A resubmitted spill
    /// keeps its route record, but the job now runs on the build server, so it
    /// is no longer routed hosted. The record stays because an older ck-motor
    /// reading this store would otherwise attach to the withdrawn tombstone and
    /// report "refused, never ran" for a job that may be running; with the
    /// record it reports the hosted outcome as unknown instead.
    pub(crate) fn hosted_route(&self) -> Option<crate::hosted::HostedJobState> {
        self.hosted
            .clone()
            .filter(|_| self.build_server_job_id.is_none())
    }
}

#[derive(Clone, Serialize, Deserialize, Debug, PartialEq, Eq)]
pub(crate) struct JobAttribution {
    pub session: String,
    pub preset: Option<String>,
    pub scope_ref: String,
    pub scope_epoch: u64,
}

#[derive(Clone, Default, Serialize, Deserialize)]
struct State {
    #[serde(deserialize_with = "deserialize_jobs")]
    jobs: BTreeMap<Uuid, Job>,
    snapshots: BTreeMap<String, BTreeMap<String, String>>,
    #[serde(default)]
    snapshot_used_at_ms: BTreeMap<String, u64>,
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    verified_tips: BTreeMap<String, VerifiedTip>,
    #[serde(skip)]
    active_jobs: BTreeMap<Uuid, usize>,
}

fn deserialize_jobs<'de, D: serde::Deserializer<'de>>(
    deserializer: D,
) -> Result<BTreeMap<Uuid, Job>, D::Error> {
    // Inspect the stored keys, not just deserialized defaults: a newer field
    // explicitly set to null or false still excludes a record from migration.
    struct StoredJob(Job);
    impl<'de> Deserialize<'de> for StoredJob {
        fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
            let value = serde_json::Value::deserialize(deserializer)?;
            // Records written before terminal tracking existed come in two
            // shapes: the original three keys, and the same three plus a
            // runner_instance name (added with Windows routing). Current ck-motor
            // always writes retain_until_ms and workspace_key, so it never
            // produces either shape.
            let legacy_shape = value.as_object().is_some_and(|fields| {
                let base = ["local_terminal", "pending_cancel", "queue_wait_limit_s"];
                base.iter().all(|key| fields.contains_key(*key))
                    && (fields.len() == 3
                        || (fields.len() == 4
                            && fields.get("runner_instance").is_some_and(|v| v.is_string())))
            });
            let mut job: Job = serde_json::from_value(value).map_err(serde::de::Error::custom)?;
            job.legacy_shape = legacy_shape;
            // Default-valued newer fields disappear on serialization. Keep a
            // retention marker so a rewrite cannot turn a newer record into a
            // three-field legacy record eligible for the age-only migration.
            if !legacy_shape && job.lacks_newer_fields() {
                job.retain_until_ms = Some(now_ms().saturating_add(RETENTION_MS));
            }
            Ok(Self(job))
        }
    }
    BTreeMap::<Uuid, StoredJob>::deserialize(deserializer)
        .map(|jobs| jobs.into_iter().map(|(id, job)| (id, job.0)).collect())
}

#[derive(Clone, Serialize, Deserialize)]
pub(crate) struct VerifiedTip {
    pub remote_url: String,
    pub branch: String,
    pub commit: String,
    pub verified_at_ms: u64,
}

#[derive(Clone, PartialEq, Eq, PartialOrd, Ord)]
pub(crate) struct VerificationKey {
    pub repository: String,
    pub remote_url: String,
    pub branch: String,
    pub commit: String,
}

impl VerifiedTip {
    fn fresh(&self, now: u64) -> bool {
        now >= self.verified_at_ms && now - self.verified_at_ms < RETENTION_MS
    }
}

impl State {
    fn prune(&mut self, now: u64) -> bool {
        let before = (
            self.jobs.len(),
            self.snapshots.len(),
            self.snapshot_used_at_ms.len(),
            self.verified_tips.len(),
        );
        self.verified_tips.retain(|_, tip| tip.fresh(now));
        while self.verified_tips.len() > MAX_VERIFIED_TIPS {
            let oldest = self
                .verified_tips
                .iter()
                .min_by_key(|(_, tip)| tip.verified_at_ms)
                .map(|(key, _)| key.clone())
                .unwrap();
            self.verified_tips.remove(&oldest);
        }
        self.jobs.retain(|id, job| {
            self.active_jobs.contains_key(id)
                || job.pending_cancel
                || job
                    .hosted
                    .as_ref()
                    .is_some_and(|s| s.pending_finish.is_some())
                || (!job.legacy_expired(*id, now)
                    && (job.terminal_observed_at_ms.is_none()
                        || job.terminal_delivered_at_ms.is_none()
                        || job.retain_until_ms.is_none_or(|until| until > now)))
        });
        // Legacy cache entries get a full horizon on first load. Snapshot entries
        // are transfer hints, but keep those named by retained jobs as well.
        self.snapshots.retain(|key, _| {
            let used = self.snapshot_used_at_ms.entry(key.clone()).or_insert(now);
            used.saturating_add(RETENTION_MS) > now
                || self
                    .jobs
                    .values()
                    .any(|job| job.workspace_key.as_ref() == Some(key))
        });
        self.snapshot_used_at_ms
            .retain(|key, _| self.snapshots.contains_key(key));
        before
            != (
                self.jobs.len(),
                self.snapshots.len(),
                self.snapshot_used_at_ms.len(),
                self.verified_tips.len(),
            )
    }
}

/// Atomic, fsynced module storage next to the daemon's delivered store path.
/// One process owns the lease; cancel intent is durable before a network attempt.
pub(crate) struct Storage {
    root: PathBuf,
    state: Arc<Mutex<State>>,
    persistence: Arc<compaction::Persistence>,
    worker: Mutex<Option<std::thread::JoinHandle<()>>>,
    tip_checks: Mutex<BTreeMap<String, Weak<Mutex<()>>>>,
    failed_tips: Mutex<BTreeMap<VerificationKey, Instant>>,
    _lease: File,
    #[cfg(test)]
    write_count: std::sync::atomic::AtomicUsize,
}

impl Drop for Storage {
    fn drop(&mut self) {
        if let Err(error) = self.shutdown() {
            tracing::error!(%error, root = %self.root.display(), "storage shutdown checkpoint failed; run ck-motor store-checkpoint before rollback");
        }
    }
}

pub(crate) struct JobLease {
    storage: Arc<Storage>,
    id: Uuid,
}
impl Drop for JobLease {
    fn drop(&mut self) {
        let mut state = self.storage.state.lock().unwrap();
        if let Some(count) = state.active_jobs.get_mut(&self.id) {
            *count -= 1;
            if *count == 0 {
                state.active_jobs.remove(&self.id);
            }
        }
    }
}

impl Storage {
    pub(crate) fn shutdown(&self) -> io::Result<()> {
        // Freeze mutation before exporting; the lease outlives both the worker
        // and the final checkpoint, including when shutdown is called twice.
        {
            let _state = self.state.lock().unwrap();
            self.persistence.stop();
        }
        if let Some(worker) = self.worker.lock().unwrap().take() {
            worker
                .join()
                .map_err(|_| io::Error::other("storage compactor panicked"))?;
        }
        self.persistence.checkpoint(&self.state)
    }
    pub(crate) fn pending_finishes(&self) -> Vec<(Uuid, crate::HostedJobState)> {
        self.state
            .lock()
            .unwrap()
            .jobs
            .iter()
            .filter_map(|(id, job)| {
                let state = job.hosted.as_ref()?;
                if state.finish_done
                    || (state.pending_finish.is_none()
                        && (state.terminal.is_none()
                            || state
                                .placement
                                .as_ref()
                                .is_none_or(|p| !p.host.starts_with("boat:"))))
                {
                    return None;
                }
                Some((*id, state.clone()))
            })
            .collect()
    }
    pub fn return_result(&self, result: ReturnResult) -> io::Result<()> {
        let id = result.job_id;
        self.update(|state| {
            state.jobs.entry(id).or_default().return_result = Some(result);
        })
    }
    pub(crate) fn return_cwd(&self, id: Uuid, cwd: String) -> io::Result<()> {
        self.update(|state| {
            state.jobs.entry(id).or_default().return_cwd = Some(cwd);
        })
    }
    pub(crate) fn hosted_update(
        &self,
        id: Uuid,
        f: impl FnOnce(&mut crate::hosted::HostedJobState),
    ) -> io::Result<()> {
        self.update(|s| {
            if let Some(state) = s.jobs.get_mut(&id).and_then(|j| j.hosted.as_mut()) {
                f(state);
            }
        })
    }
    pub(crate) fn has_live_stream(&self, id: Uuid) -> bool {
        self.state.lock().unwrap().active_jobs.contains_key(&id)
    }
    pub(crate) fn last_hosted(&self) -> Option<crate::HostedJobState> {
        self.state
            .lock()
            .unwrap()
            .jobs
            .values()
            .rev()
            .filter_map(|j| j.hosted.clone())
            .find(|h| h.lease_attempted)
    }
    pub(crate) fn hosted_routed_jobs_1h(&self) -> usize {
        self.hosted_routed_jobs_1h_at(now_ms())
    }
    fn hosted_routed_jobs_1h_at(&self, now: u64) -> usize {
        self.jobs_minted_in_last_hour(now, |job| {
            job.hosted
                .as_ref()
                .is_some_and(|route| route.repository_routed)
        })
    }
    /// Jobs selected by `hosted_repositories` in the last hour that the broker
    /// answered `no_host_fits`, and that therefore ran on the build server.
    pub(crate) fn hosted_no_host_fits_1h(&self) -> usize {
        self.hosted_no_host_fits_1h_at(now_ms())
    }
    fn hosted_no_host_fits_1h_at(&self, now: u64) -> usize {
        self.jobs_minted_in_last_hour(now, |job| {
            job.routed_direct_reason.as_deref() == Some(NO_HOST_FITS)
        })
    }
    /// Counts the jobs matching `matches` whose job ID was minted in the hour
    /// before `now`. Job IDs are UUIDv7, so the ID carries its creation time.
    fn jobs_minted_in_last_hour(&self, now: u64, matches: impl Fn(&Job) -> bool) -> usize {
        self.state
            .lock()
            .unwrap()
            .jobs
            .iter()
            .filter(|(id, job)| {
                matches(job)
                    && id.get_timestamp().is_some_and(|timestamp| {
                        let (seconds, nanos) = timestamp.to_unix();
                        let minted = seconds.saturating_mul(1000) + u64::from(nanos) / 1_000_000;
                        now.checked_sub(minted)
                            .is_some_and(|age| age < 60 * 60 * 1000)
                    })
            })
            .count()
    }
    /// Durably send a repository-routed job to the build server because the
    /// broker answered `no_host_fits` before any placement. The hosted route
    /// is removed, so nothing later releases or finishes a lease the broker
    /// never booked, and a restart attaches to the build server instead of
    /// asking for a lease again.
    ///
    /// The build server's queue-wait limit starts when it receives the Run
    /// frame, and both it and ck-motor enforce the stored limit. The time
    /// already spent asking for a lease is taken off that limit here, once,
    /// so the job waits no longer in total than its limit allowed. It is
    /// rounded down to whole seconds, and a positive limit stays at least one
    /// second, because the build server reads zero as "refuse if it has to
    /// queue at all".
    pub(crate) fn routed_direct_no_host_fits(&self, id: Uuid, spent_ms: u64) -> io::Result<()> {
        self.update(|s| {
            if let Some(job) = s.jobs.get_mut(&id) {
                job.hosted = None;
                job.routed_direct_reason = Some(NO_HOST_FITS.into());
                if job.queue_wait_limit_s > 0 {
                    job.queue_wait_limit_s = job
                        .queue_wait_limit_s
                        .saturating_sub(spent_ms / 1000)
                        .max(1);
                }
            }
        })
    }
    pub(crate) fn hosted_route(
        &self,
        id: Uuid,
        route: crate::hosted::HostedJobState,
    ) -> io::Result<()> {
        self.update(|s| {
            s.jobs.entry(id).or_default().hosted = Some(route);
        })
    }
    pub(crate) fn hosted_placement(&self, key: &str) -> Option<motor_lease::Placement> {
        self.state
            .lock()
            .unwrap()
            .jobs
            .values()
            .rev()
            .find(|j| {
                j.workspace_key.as_deref() == Some(key)
                    && j.hosted.as_ref().is_some_and(|h| h.placement.is_some())
            })
            .and_then(|j| j.hosted.as_ref()?.placement.clone())
    }
    pub(crate) fn hosted_salt(&self) -> io::Result<[u8; 32]> {
        use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
        let _guard = self.state.lock().unwrap();
        let path = self.root.join("hosted-salt");
        match fs::read(&path) {
            Ok(bytes) => {
                if fs::symlink_metadata(&path)?.file_type().is_symlink()
                    || fs::metadata(&path)?.permissions().mode() & 0o777 != 0o600
                {
                    return Err(io::Error::other("unsafe hosted salt permissions"));
                }
                bytes
                    .try_into()
                    .map_err(|_| io::Error::other("invalid hosted salt"))
            }
            Err(e) if e.kind() == io::ErrorKind::NotFound => {
                let mut salt = [0; 32];
                getrandom::fill(&mut salt).map_err(io::Error::other)?;
                let mut file = OpenOptions::new()
                    .write(true)
                    .create_new(true)
                    .mode(0o600)
                    .open(&path)?;
                file.write_all(&salt)?;
                file.sync_all()?;
                File::open(&self.root)?.sync_all()?;
                Ok(salt)
            }
            Err(e) => Err(e),
        }
    }
    pub fn open(root: &Path) -> io::Result<Self> {
        fs::create_dir_all(root)?;
        let lease = OpenOptions::new()
            .create(true)
            .truncate(false)
            .read(true)
            .write(true)
            .open(root.join("lease"))?;
        lease.try_lock_exclusive().map_err(|error| {
            if error.raw_os_error() == fs2::lock_contended_error().raw_os_error() {
                io::Error::new(
                    io::ErrorKind::WouldBlock,
                    "module storage lease is held by another process",
                )
            } else {
                error
            }
        })?;
        let metrics = Arc::new(journal::Metrics::default());
        let (state, backend) = journal::open(root, &metrics)?;
        let state = Arc::new(Mutex::new(state));
        let persistence = Arc::new(compaction::Persistence::new(root, backend, metrics));
        let worker = compaction::start(state.clone(), persistence.clone())?;
        let storage = Self {
            root: root.into(),
            state,
            persistence,
            worker: Mutex::new(Some(worker)),
            tip_checks: Mutex::new(BTreeMap::new()),
            failed_tips: Mutex::new(BTreeMap::new()),
            _lease: lease,
            #[cfg(test)]
            write_count: std::sync::atomic::AtomicUsize::new(0),
        };
        // Pruning belongs inside the transaction so its deletions enter the log.
        let needs_pruning = storage.state.lock().unwrap().clone().prune(now_ms());
        if needs_pruning {
            storage.update(|_| {})?;
        }
        Ok(storage)
    }
    fn update(&self, f: impl FnOnce(&mut State)) -> io::Result<()> {
        self.update_when(|_| true, f)
    }
    fn update_when(
        &self,
        needed: impl Fn(&State) -> bool,
        f: impl FnOnce(&mut State),
    ) -> io::Result<()> {
        let mut state = loop {
            let state = self.state.lock().unwrap();
            self.persistence.check_running()?;
            if !needed(&state) {
                return Ok(());
            }
            if self.persistence.has_room() {
                break state;
            }
            drop(state);
            self.persistence.wait_for_compaction()?;
        };
        let mut next = state.clone();
        f(&mut next);
        next.prune(now_ms());
        self.persistence.append(&state, &next)?;
        #[cfg(test)]
        self.write_count
            .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        *state = next;
        drop(state);
        self.persistence.request_if_due();
        Ok(())
    }
    pub fn tip_check(&self, repository: &str) -> Arc<Mutex<()>> {
        let mut checks = self.tip_checks.lock().unwrap();
        checks.retain(|_, check| check.strong_count() > 0);
        if let Some(check) = checks.get(repository).and_then(Weak::upgrade) {
            return check;
        }
        let check = Arc::new(Mutex::new(()));
        checks.insert(repository.into(), Arc::downgrade(&check));
        check
    }
    pub fn verified_tip(&self, repository: &str) -> Option<VerifiedTip> {
        self.state
            .lock()
            .unwrap()
            .verified_tips
            .get(repository)
            .filter(|tip| tip.fresh(now_ms()))
            .cloned()
    }
    pub fn tip_failed_recently(&self, key: &VerificationKey, now: Instant) -> bool {
        let mut failed = self.failed_tips.lock().unwrap();
        failed.retain(|_, at| {
            now.checked_duration_since(*at)
                .is_some_and(|age| age < FAILED_TIP_TTL)
        });
        failed.contains_key(key)
    }
    pub fn tip_failed(&self, key: VerificationKey, now: Instant) {
        let mut failed = self.failed_tips.lock().unwrap();
        failed.insert(key, now);
        while failed.len() > MAX_FAILED_TIPS {
            let oldest = failed
                .iter()
                .min_by_key(|(_, at)| **at)
                .map(|(key, _)| key.clone())
                .unwrap();
            failed.remove(&oldest);
        }
    }
    pub fn verify_tip(&self, repository: String, mut tip: VerifiedTip) -> io::Result<()> {
        tip.verified_at_ms = now_ms();
        self.update(|state| {
            state.verified_tips.insert(repository, tip);
        })
    }
    pub(crate) fn bind_windows(&self, id: Uuid, runner: crate::WindowsRunner) -> io::Result<()> {
        self.update(|state| {
            let job = state.jobs.get_mut(&id).expect("job exists before binding");
            job.runner_instance = Some(runner.instance.clone());
            job.windows_runner = Some(runner);
        })
    }

    pub fn job(&self, id: Uuid) -> Option<Job> {
        self.state.lock().unwrap().jobs.get(&id).cloned()
    }
    #[cfg(test)]
    pub fn set_direct_secret_hash(&self, id: Uuid, hash: [u8; 32]) -> io::Result<()> {
        self.update(|state| {
            state.jobs.entry(id).or_default().direct_secret_hash = Some(hash);
        })
    }
    pub fn direct_authorized(&self, id: Uuid, secret: Option<&str>) -> bool {
        let hash = self
            .state
            .lock()
            .unwrap()
            .jobs
            .get(&id)
            .and_then(|job| job.direct_secret_hash);
        crate::direct_secret::matches(hash, secret)
    }
    pub fn lease_job(self: &Arc<Self>, id: Uuid) -> JobLease {
        *self
            .state
            .lock()
            .unwrap()
            .active_jobs
            .entry(id)
            .or_default() += 1;
        JobLease {
            storage: self.clone(),
            id,
        }
    }
    pub fn retain_job(&self, id: Uuid, key: Option<String>) -> io::Result<()> {
        self.retain_job_at(id, key, now_ms())
    }
    fn retain_job_at(&self, id: Uuid, key: Option<String>, now: u64) -> io::Result<()> {
        // Round up so even a skipped renewal has a durable deadline at least
        // one full retention horizon after this access, including after a crash.
        let until = now
            .saturating_add(RETENTION_MS)
            .div_ceil(RETENTION_RENEWAL_MS)
            .saturating_mul(RETENTION_RENEWAL_MS);
        self.update_when(
            |s| {
                s.jobs.get(&id).is_some_and(|job| {
                    job.retain_until_ms.is_none_or(|saved| saved < until)
                        || key
                            .as_ref()
                            .is_some_and(|key| job.workspace_key.as_ref() != Some(key))
                })
            },
            |s| {
                let job = s.jobs.get_mut(&id).unwrap();
                job.retain_until_ms = Some(job.retain_until_ms.unwrap_or_default().max(until));
                if key.is_some() {
                    job.workspace_key = key.clone();
                }
            },
        )
    }
    pub fn terminal_observed(&self, id: Uuid) -> io::Result<()> {
        self.update(|s| {
            s.jobs
                .entry(id)
                .or_default()
                .terminal_observed_at_ms
                .get_or_insert(now_ms());
        })
    }
    pub fn terminal_delivered(&self, id: Uuid) -> io::Result<()> {
        self.update(|s| {
            if let Some(job) = s
                .jobs
                .get_mut(&id)
                .filter(|job| job.terminal_observed_at_ms.is_some() || job.local_terminal.is_some())
            {
                let now = now_ms();
                job.terminal_observed_at_ms.get_or_insert(now);
                job.terminal_delivered_at_ms = Some(now);
                job.retain_until_ms = Some(
                    job.retain_until_ms
                        .unwrap_or_default()
                        .max(now.saturating_add(RETENTION_MS)),
                );
            }
        })
    }
    #[cfg(test)]
    pub fn create(
        &self,
        id: Uuid,
        wait: u64,
        attribution: Option<JobAttribution>,
        runner_instance: Option<String>,
        gate_job: bool,
    ) -> io::Result<()> {
        self.create_authorized(id, wait, attribution, runner_instance, gate_job, None)
    }
    pub fn create_authorized(
        &self,
        id: Uuid,
        wait: u64,
        attribution: Option<JobAttribution>,
        runner_instance: Option<String>,
        gate_job: bool,
        direct_secret_hash: Option<[u8; 32]>,
    ) -> io::Result<()> {
        self.update(|s| {
            s.jobs.entry(id).or_insert(Job {
                gate_job,
                queue_wait_limit_s: wait,
                attribution,
                runner_instance,
                direct_secret_hash,
                ..Job::default()
            });
        })
    }
    pub fn cancel(&self, id: Uuid, pending: bool) -> io::Result<()> {
        self.update(|s| {
            let job = s.jobs.entry(id).or_default();
            job.pending_cancel = pending;
            if pending {
                job.retain_until_ms = Some(
                    job.retain_until_ms
                        .unwrap_or_default()
                        .max(now_ms().saturating_add(RETENTION_MS)),
                );
            }
        })
    }
    /// Durably record the build-server ID of a resubmitted spill before its
    /// Run frame is sent, so a restart attaches to that submission instead of
    /// submitting again or reporting the withdrawn first submission.
    pub(crate) fn resubmitted(
        &self,
        id: Uuid,
        build_server_job_id: Uuid,
        hosted_code: &str,
    ) -> io::Result<()> {
        self.update(|s| {
            let job = s.jobs.entry(id).or_default();
            job.build_server_job_id = Some(build_server_job_id);
            if let Some(route) = job.hosted.as_mut() {
                route.hosted_code = Some(hosted_code.into());
            }
        })
    }
    pub fn local_terminal(&self, t: TerminalRecord) -> io::Result<()> {
        let id = t.job_id;
        self.update(|s| {
            let job = s.jobs.entry(id).or_default();
            job.local_terminal = Some(t);
            job.terminal_observed_at_ms.get_or_insert(now_ms());
        })
    }
    pub fn previous(&self, key: &str) -> BTreeMap<String, String> {
        self.state
            .lock()
            .unwrap()
            .snapshots
            .get(key)
            .cloned()
            .unwrap_or_default()
    }
    pub fn accepted(&self, key: String, commits: BTreeMap<String, String>) -> io::Result<()> {
        self.update(|s| {
            s.snapshot_used_at_ms.insert(key.clone(), now_ms());
            s.snapshots.insert(key, commits);
        })
    }
}

#[cfg(test)]
mod retention_tests {
    #[test]
    fn direct_secret_hash_survives_reopen_and_expires_with_its_job() {
        use super::*;
        let scratch = cortexkit_test_support::ScratchDir::new("motor-direct-retention");
        let storage = Storage::open(&scratch).unwrap();
        let id = Uuid::now_v7();
        let secret = crate::direct_secret::DirectSecret::mint().unwrap();
        storage.create(id, 60, None, None, true).unwrap();
        storage.set_direct_secret_hash(id, secret.hash()).unwrap();
        assert!(storage.direct_authorized(id, Some(secret.expose())));
        assert!(!storage.direct_authorized(id, None));
        assert!(!storage.direct_authorized(Uuid::now_v7(), Some(secret.expose())));
        drop(storage);
        let storage = Storage::open(&scratch).unwrap();
        assert!(storage.direct_authorized(id, Some(secret.expose())));
        storage.terminal_observed(id).unwrap();
        storage.terminal_delivered(id).unwrap();
        storage
            .state
            .lock()
            .unwrap()
            .jobs
            .get_mut(&id)
            .unwrap()
            .retain_until_ms = Some(1);
        storage.update(|_| {}).unwrap();
        assert!(storage.job(id).is_none());
        assert!(!storage.direct_authorized(id, Some(secret.expose())));
        let state = durable_value(&scratch).unwrap().to_string();
        assert!(!state.contains("direct_secret_hash"));
        assert!(!state.contains(secret.expose()));
        let legacy: Job = serde_json::from_str(
            r#"{"queue_wait_limit_s":60,"pending_cancel":false,"local_terminal":null}"#,
        )
        .unwrap();
        assert!(legacy.direct_secret_hash.is_none());
        assert!(!serde_json::to_string(&legacy)
            .unwrap()
            .contains("direct_secret_hash"));
    }
    use super::*;
    use serde_json::json;

    #[test]
    fn legacy_jobs_expire_by_mint_time_without_terminal_timestamps() {
        let old = Uuid::now_v7();
        let (seconds, nanos) = old.get_timestamp().unwrap().to_unix();
        let minted = seconds * 1000 + u64::from(nanos) / 1_000_000;
        let now = minted + 49 * 60 * 60 * 1000;
        let fresh = Uuid::new_v7(uuid::Timestamp::from_unix(
            uuid::NoContext,
            now / 1000,
            ((now % 1000) * 1_000_000) as u32,
        ));
        let legacy = json!({
            "queue_wait_limit_s": 60, "pending_cancel": false, "local_terminal": null,
        });
        // The later legacy shape: the same three keys plus runner_instance.
        let routed = json!({
            "runner_instance": "main", "queue_wait_limit_s": 300,
            "pending_cancel": false, "local_terminal": null,
        });
        for record in [legacy, routed] {
            let mut state: State = serde_json::from_value(json!({
                "jobs": {old.to_string(): record, fresh.to_string(): record},
                "snapshots": {},
            }))
            .unwrap();

            state.prune(now);
            assert!(
                !state.jobs.contains_key(&old),
                "old legacy history must expire: {record}"
            );
            assert!(
                state.jobs.contains_key(&fresh),
                "fresh history must survive: {record}"
            );
        }
    }

    #[test]
    fn legacy_migration_preserves_richer_records_and_live_jobs() {
        let id = Uuid::now_v7();
        let (seconds, nanos) = id.get_timestamp().unwrap().to_unix();
        let minted = seconds * 1000 + u64::from(nanos) / 1_000_000;
        let now = minted + LEGACY_RETENTION_MS;
        let legacy = json!({
            "queue_wait_limit_s": 60, "pending_cancel": false, "local_terminal": null,
        });
        let load = |record| {
            serde_json::from_value::<State>(json!({
                "jobs": {id.to_string(): record}, "snapshots": {},
            }))
            .unwrap()
        };

        let mut state = load(legacy.clone());
        state.prune(now - 1);
        assert!(state.jobs.contains_key(&id), "the full horizon is retained");
        state.active_jobs.insert(id, 1);
        state.prune(now);
        assert!(state.jobs.contains_key(&id), "live streams fence migration");
        state.active_jobs.clear();
        state.jobs.get_mut(&id).unwrap().pending_cancel = true;
        state.prune(now);
        assert!(state.jobs.contains_key(&id), "cancel intent must survive");
        state.jobs.get_mut(&id).unwrap().pending_cancel = false;
        state.jobs.get_mut(&id).unwrap().retain_until_ms = Some(now + RETENTION_MS);
        state.prune(now);
        assert!(
            state.jobs.contains_key(&id),
            "caller access extends retention"
        );

        for (field, value) in [
            ("hosted", json!(null)),
            ("return_result", json!(null)),
            ("direct_secret_hash", json!(null)),
            ("gate_job", json!(false)),
            ("runner_instance", json!(null)),
            ("attribution", json!(null)),
            ("terminal_observed_at_ms", json!(null)),
            ("terminal_delivered_at_ms", json!(null)),
            ("retain_until_ms", json!(null)),
            ("workspace_key", json!(null)),
            ("build_server_job_id", json!(null)),
            ("unknown_future_field", json!(null)),
        ] {
            let mut record = legacy.clone();
            record[field] = value;
            let mut state = load(record);
            state.prune(now);
            assert!(
                state.jobs.contains_key(&id),
                "newer field {field} fences migration"
            );
            let mut rewritten: State =
                serde_json::from_slice(&serde_json::to_vec(&state).unwrap()).unwrap();
            rewritten.prune(now);
            assert!(
                rewritten.jobs.contains_key(&id),
                "newer field {field} survives rewrite"
            );
        }

        let mut state = load(legacy);
        state.prune(minted - 1);
        assert!(state.jobs.contains_key(&id), "future IDs must survive");
        let job = state.jobs.remove(&id).unwrap();
        state.jobs.insert(Uuid::nil(), job);
        state.prune(now);
        assert!(
            state.jobs.contains_key(&Uuid::nil()),
            "non-v7 IDs must survive"
        );
    }

    #[test]
    fn pruning_removes_only_delivered_expired_jobs_and_inactive_snapshots() {
        let temp = cortexkit_test_support::ScratchDir::new("motor-storage-retention");
        let now = now_ms();
        let expired = Uuid::now_v7();
        let undelivered = Uuid::now_v7();
        let attachable = Uuid::now_v7();
        let cancelling = Uuid::now_v7();
        let legacy = Uuid::now_v7();
        let job = |delivered, until, pending| {
            json!({
                "queue_wait_limit_s": 60, "pending_cancel": pending, "local_terminal": null,
                "terminal_observed_at_ms": 1, "terminal_delivered_at_ms": delivered,
                "retain_until_ms": until, "workspace_key": "active",
            })
        };
        fs::write(temp.join("state.json"), serde_json::to_vec(&json!({
            "jobs": {
                expired.to_string(): job(json!(1), 1, false),
                undelivered.to_string(): job(serde_json::Value::Null, 1, false),
                attachable.to_string(): job(json!(1), now + 60_000, false),
                cancelling.to_string(): job(json!(1), 1, true),
                legacy.to_string(): {"queue_wait_limit_s": 60, "pending_cancel": false, "local_terminal": null},
            },
            "snapshots": {"active": {"repo":"commit"}, "inactive": {"repo":"old"}},
            "snapshot_used_at_ms": {"active": 1, "inactive": 1},
        })).unwrap()).unwrap();
        let storage = Storage::open(&temp).unwrap();
        assert!(
            storage.job(expired).is_none(),
            "delivered, expired history must be pruned"
        );
        for id in [undelivered, attachable, cancelling, legacy] {
            assert!(
                storage.job(id).is_some(),
                "unsettled or attachable history must survive"
            );
        }
        assert!(!storage.previous("active").is_empty());
        assert!(storage.previous("inactive").is_empty());
        drop(storage);
        let reopened = Storage::open(&temp).unwrap();
        assert!(reopened.job(expired).is_none());
        assert!(reopened.job(undelivered).is_some());
    }

    #[test]
    fn caller_access_and_live_streams_fence_pruning() {
        let temp = cortexkit_test_support::ScratchDir::new("motor-storage-active");
        let storage = Arc::new(Storage::open(&temp).unwrap());
        let id = Uuid::now_v7();
        storage
            .create(id, 60, None, Some("windows".into()), false)
            .unwrap();
        storage.terminal_observed(id).unwrap();
        storage.terminal_delivered(id).unwrap();
        storage
            .state
            .lock()
            .unwrap()
            .jobs
            .get_mut(&id)
            .unwrap()
            .retain_until_ms = Some(1);
        storage.retain_job(id, None).unwrap();
        assert!(storage.job(id).unwrap().retain_until_ms.unwrap() > now_ms());
        assert_eq!(
            storage.job(id).unwrap().runner_instance.as_deref(),
            Some("windows")
        );

        let lease = storage.lease_job(id);
        storage
            .state
            .lock()
            .unwrap()
            .jobs
            .get_mut(&id)
            .unwrap()
            .retain_until_ms = Some(1);
        storage.update(|_| {}).unwrap();
        assert!(
            storage.job(id).is_some(),
            "an attached stream keeps its routing record"
        );
        drop(lease);
        storage.update(|_| {}).unwrap();
        assert!(storage.job(id).is_none());
    }

    #[test]
    fn legacy_local_terminal_can_be_acknowledged_without_shortening_retention() {
        let temp = cortexkit_test_support::ScratchDir::new("motor-storage-legacy-delivery");
        let id = Uuid::now_v7();
        let terminal = TerminalRecord::new(
            id,
            Outcome::RefusedBeforeStart {
                reason: RefusalReason::RunnerFull,
            },
            0,
            0,
            0,
        )
        .with_ran(Ran::None);
        fs::write(
            temp.join("state.json"),
            serde_json::to_vec(&json!({
                "jobs": {id.to_string(): {
                    "queue_wait_limit_s": 60, "pending_cancel": false, "local_terminal": terminal,
                }}, "snapshots": {},
            }))
            .unwrap(),
        )
        .unwrap();
        let storage = Storage::open(&temp).unwrap();
        assert!(storage.job(id).unwrap().terminal_delivered_at_ms.is_none());
        storage.terminal_delivered(id).unwrap();
        let job = storage.job(id).unwrap();
        assert!(job.terminal_observed_at_ms.is_some());
        assert!(job.terminal_delivered_at_ms.is_some());
        storage
            .state
            .lock()
            .unwrap()
            .jobs
            .get_mut(&id)
            .unwrap()
            .retain_until_ms = Some(u64::MAX);
        storage.retain_job(id, None).unwrap();
        storage.cancel(id, true).unwrap();
        storage.terminal_delivered(id).unwrap();
        assert_eq!(storage.job(id).unwrap().retain_until_ms, Some(u64::MAX));
    }
}

#[cfg(test)]
mod retention_renewal_tests {
    use super::*;
    use std::sync::atomic::Ordering;

    #[test]
    fn status_poll_retention_writes_once_per_job_per_minute() {
        const JOBS: usize = 8;
        let scratch = cortexkit_test_support::ScratchDir::new("motor-retention-polls");
        let storage = Storage::open(&scratch).unwrap();
        let ids: Vec<_> = (0..JOBS).map(|_| Uuid::now_v7()).collect();
        for id in &ids {
            storage.create(*id, 60, None, None, false).unwrap();
        }
        let start = now_ms().div_ceil(RETENTION_RENEWAL_MS) * RETENTION_RENEWAL_MS;
        let before = storage.write_count.load(Ordering::SeqCst);
        for second in 1..=120 {
            for id in &ids {
                storage
                    .retain_job_at(*id, None, start + second * 1000)
                    .unwrap();
            }
        }
        let writes = storage.write_count.load(Ordering::SeqCst) - before;
        eprintln!(
            "{JOBS} jobs x 120 status polls: {writes} durable state writes (expected {})",
            JOBS * 2
        );
        assert_eq!(writes, JOBS * 2);

        drop(storage);
        let reopened = Storage::open(&scratch).unwrap();
        for id in ids {
            assert_eq!(
                reopened.job(id).unwrap().retain_until_ms,
                Some(start + 120_000 + RETENTION_MS),
                "the last poll's full horizon must survive a restart"
            );
        }
    }

    #[test]
    fn covered_retention_skips_io_but_new_deadlines_and_bindings_are_synchronous() {
        let scratch = cortexkit_test_support::ScratchDir::new("motor-retention-io");
        let storage = Storage::open(&scratch).unwrap();
        let id = Uuid::now_v7();
        storage.create(id, 60, None, None, false).unwrap();
        let start = now_ms().div_ceil(RETENTION_RENEWAL_MS) * RETENTION_RENEWAL_MS;
        storage
            .retain_job_at(id, Some("original".into()), start + 1)
            .unwrap();
        let until = start + RETENTION_RENEWAL_MS + RETENTION_MS;
        assert_eq!(storage.job(id).unwrap().retain_until_ms, Some(until));
        let before = storage.write_count.load(Ordering::SeqCst);

        // A read-only journal descriptor makes any actual append fail. Covered
        // deadlines must still succeed without reaching the persistence backend.
        if let journal::Backend::Journal(j) = &mut *storage.persistence.backend.lock().unwrap() {
            j.set_append_readonly(true);
        } else {
            panic!("test requires the journal backend");
        }
        storage
            .retain_job_at(id, None, start + RETENTION_RENEWAL_MS - 1)
            .unwrap();
        storage
            .retain_job_at(id, Some("original".into()), start + RETENTION_RENEWAL_MS)
            .unwrap();
        storage
            .retain_job_at(Uuid::now_v7(), None, start + RETENTION_RENEWAL_MS)
            .unwrap();
        assert!(storage
            .retain_job_at(id, None, start + RETENTION_RENEWAL_MS + 1)
            .is_err());
        assert!(storage
            .retain_job_at(id, Some("changed".into()), start + 2)
            .is_err());
        assert!(storage.cancel(id, true).is_err());
        assert!(storage
            .resubmitted(id, Uuid::now_v7(), "unreachable")
            .is_err());
        assert_eq!(storage.write_count.load(Ordering::SeqCst), before);
        let job = storage.job(id).unwrap();
        assert_eq!(job.retain_until_ms, Some(until));
        assert_eq!(job.workspace_key.as_deref(), Some("original"));
        assert!(!job.pending_cancel);
        assert!(job.build_server_job_id.is_none());

        if let journal::Backend::Journal(j) = &mut *storage.persistence.backend.lock().unwrap() {
            j.set_append_readonly(false);
        }
        storage
            .retain_job_at(id, Some("changed".into()), start + 2)
            .unwrap();
        assert_eq!(storage.write_count.load(Ordering::SeqCst), before + 1);
        drop(storage);
        let reopened = Storage::open(&scratch).unwrap();
        let job = reopened.job(id).unwrap();
        assert_eq!(job.workspace_key.as_deref(), Some("changed"));
        assert_eq!(job.retain_until_ms, Some(until));
    }

    #[test]
    fn retention_rounding_saturates_and_never_shortens_a_longer_deadline() {
        let scratch = cortexkit_test_support::ScratchDir::new("motor-retention-overflow");
        let storage = Storage::open(&scratch).unwrap();
        let id = Uuid::now_v7();
        storage.create(id, 60, None, None, false).unwrap();
        storage.retain_job_at(id, None, u64::MAX).unwrap();
        assert_eq!(storage.job(id).unwrap().retain_until_ms, Some(u64::MAX));
        let before = storage.write_count.load(Ordering::SeqCst);
        storage.retain_job_at(id, None, now_ms()).unwrap();
        assert_eq!(storage.write_count.load(Ordering::SeqCst), before);
        drop(storage);
        let reopened = Storage::open(&scratch).unwrap();
        assert_eq!(reopened.job(id).unwrap().retain_until_ms, Some(u64::MAX));
    }
}

#[cfg(test)]
mod verified_tip_tests {
    use super::*;
    use cortexkit_test_support::ScratchDir;

    fn record(at: u64) -> VerifiedTip {
        VerifiedTip {
            remote_url: "https://example.invalid/repo.git".into(),
            branch: "refs/heads/main".into(),
            commit: "a".repeat(40),
            verified_at_ms: at,
        }
    }

    #[test]
    fn verified_tip_age_is_bounded_and_future_timestamps_are_refused() {
        let tip = record(100);
        assert!(!tip.fresh(99));
        assert!(tip.fresh(100));
        assert!(tip.fresh(100 + RETENTION_MS - 1));
        assert!(!tip.fresh(100 + RETENTION_MS));
        let mut state = State::default();
        state.verified_tips.insert("expired".into(), record(100));
        assert!(state.prune(100 + RETENTION_MS));
        assert!(state.verified_tips.is_empty());
    }

    #[test]
    fn verified_tips_keep_only_256_repositories_and_one_tip_each() {
        let mut state = State::default();
        for n in 0..258 {
            state.verified_tips.insert(n.to_string(), record(n + 1));
        }
        assert!(state.prune(300));
        assert_eq!(state.verified_tips.len(), MAX_VERIFIED_TIPS);
        assert!(!state.verified_tips.contains_key("0"));
        assert!(!state.verified_tips.contains_key("1"));
        assert!(state.verified_tips.contains_key("257"));

        let scratch = ScratchDir::new("motor-verified-tip-state");
        let storage = Storage::open(&scratch).unwrap();
        storage.verify_tip("repo".into(), record(0)).unwrap();
        let mut next = record(0);
        next.commit = "b".repeat(40);
        storage.verify_tip("repo".into(), next).unwrap();
        assert_eq!(storage.state.lock().unwrap().verified_tips.len(), 1);
        let verified = storage.verified_tip("repo").unwrap();
        assert_eq!(verified.commit, "b".repeat(40));
        assert_eq!(
            storage.verified_tip("repo").unwrap().verified_at_ms,
            verified.verified_at_ms
        );
    }

    #[test]
    fn negative_cache_keys_all_verification_inputs_and_is_bounded() {
        let scratch = ScratchDir::new("motor-failed-tip-state");
        let storage = Storage::open(&scratch).unwrap();
        let now = Instant::now();
        let key = VerificationKey {
            repository: "repo".into(),
            remote_url: "origin".into(),
            branch: "refs/heads/main".into(),
            commit: "a".repeat(40),
        };
        storage.tip_failed(key.clone(), now);
        assert!(storage.tip_failed_recently(&key, now));
        for field in 0..4 {
            let mut changed = key.clone();
            match field {
                0 => changed.repository.push_str("-other"),
                1 => changed.remote_url.push_str("-other"),
                2 => changed.branch.push_str("-other"),
                _ => changed.commit = "b".repeat(40),
            }
            assert!(!storage.tip_failed_recently(&changed, now));
        }
        for n in 0..MAX_FAILED_TIPS {
            let mut next = key.clone();
            next.repository = n.to_string();
            storage.tip_failed(next, now + Duration::from_millis(n as u64 + 1));
        }
        assert_eq!(storage.failed_tips.lock().unwrap().len(), MAX_FAILED_TIPS);
        assert!(!storage.tip_failed_recently(&key, now + Duration::from_secs(1)));
    }
}

#[cfg(test)]
mod hosted_routing_health_tests {
    use super::*;

    #[test]
    fn hosted_routed_counter_only_counts_repository_routes_in_the_last_hour() {
        let scratch = cortexkit_test_support::ScratchDir::new("hosted-route-health");
        let storage = Storage::open(&scratch).unwrap();
        let now = now_ms();
        for (age, routed) in [(0, true), (3_599_999, true), (3_600_000, true), (0, false)] {
            let minted = now - age;
            let id = Uuid::new_v7(uuid::Timestamp::from_unix(
                uuid::NoContext,
                minted / 1000,
                ((minted % 1000) * 1_000_000) as u32,
            ));
            storage.create(id, 30, None, None, false).unwrap();
            storage
                .hosted_route(
                    id,
                    crate::HostedJobState {
                        repository_routed: routed,
                        ..crate::HostedJobState::default()
                    },
                )
                .unwrap();
        }
        assert_eq!(storage.hosted_routed_jobs_1h_at(now), 2);
        assert_eq!(storage.hosted_routed_jobs_1h_at(now + 3_600_000), 0);
        drop(storage);
        let storage = Storage::open(&scratch).unwrap();
        assert_eq!(storage.hosted_routed_jobs_1h_at(now), 2);
    }

    /// Sending a routed job to the build server removes its hosted route,
    /// records why, and takes the whole seconds already spent off its
    /// queue-wait limit exactly once; all of it survives reopening the store.
    #[test]
    fn no_host_fits_decision_trims_queue_wait_once_and_is_counted_for_an_hour() {
        let scratch = cortexkit_test_support::ScratchDir::new("hosted-no-host-fits");
        let storage = Storage::open(&scratch).unwrap();
        let now = now_ms();
        let mut ids = Vec::new();
        for (age, limit, spent_ms, remaining) in [
            (0, 300, 2_999, 298),
            (3_599_999, 2, 5_000, 1),
            (3_600_000, 300, 0, 300),
            (0, 0, 5_000, 0),
        ] {
            let minted = now - age;
            let id = Uuid::new_v7(uuid::Timestamp::from_unix(
                uuid::NoContext,
                minted / 1000,
                ((minted % 1000) * 1_000_000) as u32,
            ));
            storage.create(id, limit, None, None, false).unwrap();
            storage
                .hosted_route(
                    id,
                    crate::HostedJobState {
                        repository_routed: true,
                        lease_attempted: true,
                        ..crate::HostedJobState::default()
                    },
                )
                .unwrap();
            storage.routed_direct_no_host_fits(id, spent_ms).unwrap();
            ids.push((id, remaining, age));
        }
        // Not a no_host_fits decision: a routed job still on its hosted route.
        let routed = Uuid::new_v7(uuid::Timestamp::from_unix(
            uuid::NoContext,
            now / 1000,
            ((now % 1000) * 1_000_000) as u32,
        ));
        storage.create(routed, 300, None, None, false).unwrap();
        storage
            .hosted_route(
                routed,
                crate::HostedJobState {
                    repository_routed: true,
                    ..crate::HostedJobState::default()
                },
            )
            .unwrap();
        assert_eq!(storage.hosted_no_host_fits_1h_at(now), 3);
        assert_eq!(storage.hosted_no_host_fits_1h_at(now + 3_600_000), 0);
        drop(storage);
        // A process another test forks at the wrong moment can briefly hold
        // the store's lock file after it is closed here.
        let storage = (0..100)
            .find_map(|_| match Storage::open(&scratch) {
                Err(error) if error.kind() == io::ErrorKind::WouldBlock => {
                    std::thread::sleep(Duration::from_millis(20));
                    None
                }
                other => Some(other.unwrap()),
            })
            .expect("store lock released");
        assert_eq!(storage.hosted_no_host_fits_1h_at(now), 3);
        assert_eq!(storage.hosted_routed_jobs_1h_at(now), 1);
        for (id, remaining, age) in ids {
            let job = storage.job(id).unwrap();
            assert!(job.hosted.is_none(), "age {age}");
            assert_eq!(job.routed_direct_reason.as_deref(), Some("no_host_fits"));
            assert_eq!(job.queue_wait_limit_s, remaining, "age {age}");
        }
    }
}
