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
// Only records whose stored keys are exactly local_terminal, pending_cancel,
// and queue_wait_limit_s use this migration horizon. Agent jobs can run without
// a timeout, so mint time alone cannot expire current job records.
const LEGACY_RETENTION_MS: u64 = 49 * 60 * 60 * 1000;
const MAX_VERIFIED_TIPS: usize = 256;
const MAX_FAILED_TIPS: usize = 256;
const FAILED_TIP_TTL: Duration = Duration::from_secs(60);

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
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub direct_secret_hash: Option<[u8; 32]>,
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub gate_job: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub runner_instance: Option<String>,
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
}
impl Job {
    fn lacks_newer_fields(&self) -> bool {
        self.hosted.is_none()
            && self.return_result.is_none()
            && self.direct_secret_hash.is_none()
            && !self.gate_job
            && self.runner_instance.is_none()
            && self.attribution.is_none()
            && self.terminal_observed_at_ms.is_none()
            && self.terminal_delivered_at_ms.is_none()
            && self.retain_until_ms.is_none()
            && self.workspace_key.is_none()
            && self.build_server_job_id.is_none()
    }

    fn legacy_expired(&self, id: Uuid, now: u64) -> bool {
        self.legacy_shape
            && self.lacks_newer_fields()
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
            let legacy_shape = value.as_object().is_some_and(|fields| {
                fields.len() == 3
                    && ["local_terminal", "pending_cancel", "queue_wait_limit_s"]
                        .iter()
                        .all(|key| fields.contains_key(*key))
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
    state: Mutex<State>,
    tip_checks: Mutex<BTreeMap<String, Weak<Mutex<()>>>>,
    failed_tips: Mutex<BTreeMap<VerificationKey, Instant>>,
    _lease: File,
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
        let mut state: State = match fs::read(root.join("state.json")) {
            Ok(bytes) => serde_json::from_slice(&bytes).map_err(io::Error::other)?,
            Err(e) if e.kind() == io::ErrorKind::NotFound => State::default(),
            Err(e) => return Err(e),
        };
        let pruned = state.prune(now_ms());
        let storage = Self {
            root: root.into(),
            state: Mutex::new(state),
            tip_checks: Mutex::new(BTreeMap::new()),
            failed_tips: Mutex::new(BTreeMap::new()),
            _lease: lease,
        };
        if pruned {
            storage.update(|_| {})?;
        }
        Ok(storage)
    }
    fn update(&self, f: impl FnOnce(&mut State)) -> io::Result<()> {
        let mut state = self.state.lock().unwrap();
        let mut next = state.clone();
        f(&mut next);
        next.prune(now_ms());
        let path = self.root.join("state.tmp");
        let mut file = File::create(&path)?;
        file.write_all(&serde_json::to_vec(&next).map_err(io::Error::other)?)?;
        file.sync_all()?;
        fs::rename(path, self.root.join("state.json"))?;
        File::open(&self.root)?.sync_all()?;
        *state = next;
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
    pub fn job(&self, id: Uuid) -> Option<Job> {
        self.state.lock().unwrap().jobs.get(&id).cloned()
    }
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
        self.update(|s| {
            let Some(job) = s.jobs.get_mut(&id) else {
                return;
            };
            job.retain_until_ms = Some(
                job.retain_until_ms
                    .unwrap_or_default()
                    .max(now_ms().saturating_add(RETENTION_MS)),
            );
            if key.is_some() {
                job.workspace_key = key;
            }
        })
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
    pub fn create(
        &self,
        id: Uuid,
        wait: u64,
        attribution: Option<JobAttribution>,
        runner_instance: Option<String>,
        gate_job: bool,
    ) -> io::Result<()> {
        self.update(|s| {
            s.jobs.entry(id).or_insert(Job {
                gate_job,
                queue_wait_limit_s: wait,
                attribution,
                runner_instance,
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
        let state = fs::read_to_string(scratch.join("state.json")).unwrap();
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
        let mut state: State = serde_json::from_value(json!({
            "jobs": {old.to_string(): legacy, fresh.to_string(): legacy},
            "snapshots": {},
        }))
        .unwrap();

        state.prune(now);
        assert!(
            !state.jobs.contains_key(&old),
            "old legacy history must expire"
        );
        assert!(
            state.jobs.contains_key(&fresh),
            "fresh history must survive"
        );
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
