use fs2::FileExt;
use motor_protocol::*;
use serde::{Deserialize, Serialize};
use std::{
    collections::BTreeMap,
    fs::{self, File, OpenOptions},
    io::{self, Write},
    path::{Path, PathBuf},
    sync::{Arc, Mutex, Weak},
    time::{SystemTime, UNIX_EPOCH},
};

// Keep routing and local refusal history for at least the runner's replay horizon,
// starting at successful terminal delivery or the caller's latest access.
const RETENTION_MS: u64 = 24 * 60 * 60 * 1000;
const MAX_VERIFIED_TIPS: usize = 256;

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
    jobs: BTreeMap<Uuid, Job>,
    snapshots: BTreeMap<String, BTreeMap<String, String>>,
    #[serde(default)]
    snapshot_used_at_ms: BTreeMap<String, u64>,
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    verified_tips: BTreeMap<String, VerifiedTip>,
    #[serde(skip)]
    active_jobs: BTreeMap<Uuid, usize>,
}

#[derive(Clone, Serialize, Deserialize)]
pub(crate) struct VerifiedTip {
    pub remote_url: String,
    pub branch: String,
    pub commit: String,
    pub verified_at_ms: u64,
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
                || job.terminal_observed_at_ms.is_none()
                || job.terminal_delivered_at_ms.is_none()
                || job.retain_until_ms.is_none_or(|until| until > now)
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
    pub fn verify_tip(&self, repository: String, mut tip: VerifiedTip) -> io::Result<()> {
        tip.verified_at_ms = now_ms();
        self.update(|state| {
            state.verified_tips.insert(repository, tip);
        })
    }
    pub fn job(&self, id: Uuid) -> Option<Job> {
        self.state.lock().unwrap().jobs.get(&id).cloned()
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
    use super::*;
    use serde_json::json;

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
}
