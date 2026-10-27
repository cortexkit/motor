//! Durable VM ownership, serialized admission, and conservative restart recovery.
use super::{fs::Dir, Cancellation, Control, ScopedLock, Staging, Tools, PREP_TIMEOUT};
use crate::{
    engine::Config,
    storage::{atomic_json, invalid},
};
use motor_macvm::{JobHelper, ProcessIdentity};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::{
    collections::BTreeMap,
    fs::File,
    io,
    path::{Path, PathBuf},
    sync::{
        atomic::{AtomicBool, Ordering},
        Arc, Mutex,
    },
    time::{Duration, Instant},
};
use uuid::Uuid;

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct VmConfig {
    pub image: PathBuf,
    pub helper: PathBuf,
    pub tools: Tools,
    pub toolchain_version: String,
}
impl VmConfig {
    pub fn load(path: &Path) -> io::Result<Self> {
        let value: Self = toml::from_str(&std::fs::read_to_string(path)?).map_err(invalid)?;
        for path in [
            &value.image,
            &value.helper,
            &value.tools.cargo.path,
            &value.tools.rustc.path,
            &value.tools.bun.path,
            &value.tools.git.path,
        ] {
            if !path.is_absolute()
                || path.components().any(|p| {
                    !matches!(
                        p,
                        std::path::Component::RootDir | std::path::Component::Normal(_)
                    )
                })
            {
                return Err(invalid(
                    "macOS VM configuration requires normalized absolute paths",
                ));
            }
        }
        Ok(value)
    }
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub(super) struct Paths {
    pub clone: PathBuf,
    pub nonce: PathBuf,
    pub tree: PathBuf,
    pub cargo: PathBuf,
    pub prep: PathBuf,
    pub target_tmp: PathBuf,
    #[serde(default)]
    pub quarantine: PathBuf,
}
impl Paths {
    fn new(root: &Path, id: Uuid) -> Self {
        let tree = root.join("jobs").join(id.to_string());
        Self {
            clone: root.join("clones").join(id.to_string()),
            nonce: root.join("nonces").join(id.to_string()),
            cargo: tree.join("cargo-home"),
            tree,
            prep: root.join("prep").join(id.to_string()),
            target_tmp: root.join("targets").join(format!(".{id}.tmp")),
            quarantine: root.join("quarantine").join(id.to_string()),
        }
    }
    fn primary(&self) -> [&Path; 6] {
        [
            &self.clone,
            &self.nonce,
            &self.cargo,
            &self.tree,
            &self.prep,
            &self.target_tmp,
        ]
    }
    fn all(&self) -> [&Path; 7] {
        let p = self.primary();
        [p[0], p[1], p[2], p[3], p[4], p[5], &self.quarantine]
    }
}
#[derive(Clone, Serialize, Deserialize)]
pub(super) struct Record {
    pub id: Uuid,
    pub workspace: String,
    pub paths: Paths,
    pub slot: Option<usize>,
    pub helper: Option<ProcessIdentity>,
    pub gate_open: bool,
    pub service: Option<ProcessIdentity>,
    pub stop_proven: bool,
    #[serde(default)]
    pub proof_version: u8,
    #[serde(default)]
    pub quarantined: bool,
}
pub(super) struct Owned {
    pub record: Record,
    pub slot: Option<File>,
    pub workspace_lock: Option<ScopedLock>,
    pub helper: Option<JobHelper>,
}
pub(super) struct Entry {
    pub owned: Mutex<Owned>,
    pub cancel: Cancellation,
    pub active: AtomicBool,
    pub failed: AtomicBool,
    cleanup: Mutex<()>,
    #[cfg(test)]
    pub output_blocked: AtomicBool,
}
impl Entry {
    pub fn check(&self) -> io::Result<()> {
        if self.cancel.0.load(Ordering::Acquire) {
            Err(invalid("macOS job cancelled"))
        } else {
            Ok(())
        }
    }
}

#[derive(Clone, Copy)]
pub(super) struct Service {
    pub identity: ProcessIdentity,
    pub owned: bool,
}
/// Process observations are injectable. Numeric-PID signals are timestamp-checked,
/// not identity-atomic; retained child handles provide the stronger helper path.
pub(super) trait Host: Send + Sync {
    fn services(&self) -> io::Result<Vec<Service>>;
    fn alive(&self, process: ProcessIdentity) -> io::Result<bool>;
    fn kill(&self, process: ProcessIdentity) -> io::Result<()>;
    fn release_proven(&self, record: &Record) -> io::Result<bool>;
    fn stop_unrecorded(&self, _record: &Record) -> io::Result<()> {
        Ok(())
    }
    fn delete(&self, path: &Path) -> io::Result<()> {
        remove(path)
    }
}
struct Native;
const RELEASE_PROOF_VERSION: u8 = 2;
fn release_slot(owned: &mut Owned) -> io::Result<()> {
    if !owned.record.stop_proven
        || owned.record.proof_version != RELEASE_PROOF_VERSION
        || owned.record.slot.is_some()
    {
        return Err(invalid(
            "slot release requires a persisted stop proof and cleared reservation",
        ));
    }
    if let Some(slot) = &owned.slot {
        super::unlock_file(slot)?;
    }
    owned.slot.take();
    Ok(())
}
fn release_after_gate(
    record: &Record,
    scan: impl FnOnce() -> io::Result<bool>,
) -> io::Result<bool> {
    if !record.gate_open {
        return Ok(true);
    }
    scan()
}
/// Darwin has no pidfd signal API. Recheck immediately before the numeric-PID
/// signal, but an exit/reuse between that check and kill remains possible.
/// Only retained, unreaped direct children have identity-bound termination.
fn signal_if_current(
    process: ProcessIdentity,
    mut current: impl FnMut() -> io::Result<bool>,
    signal: impl FnOnce() -> io::Result<()>,
) -> io::Result<()> {
    if !current()? {
        return Ok(());
    }
    if !current()? {
        tracing::debug!(
            pid = process.pid,
            "process changed before signal; not signalling"
        );
        return Ok(());
    }
    signal()
}
/// Parses one `ps -o pid=,uid=,comm=` row and returns its pid and uid when the
/// process is Virtualization.framework's VM service. macOS prints some system
/// accounts' uids as negative numbers (nobody is -2), so the uid is signed; no
/// such uid is ever the runner's own.
#[cfg(any(target_os = "macos", test))]
fn vm_service_row(line: &str) -> io::Result<Option<(i32, i64)>> {
    let mut fields = line.split_whitespace();
    let pid = fields
        .next()
        .ok_or_else(|| invalid("missing process pid"))?
        .parse()
        .map_err(invalid)?;
    let uid = fields
        .next()
        .ok_or_else(|| invalid("missing process uid"))?
        .parse()
        .map_err(invalid)?;
    let vm = fields.collect::<Vec<_>>().join(" ").rsplit('/').next()
        == Some("com.apple.Virtualization.VirtualMachine");
    Ok(vm.then_some((pid, uid)))
}
impl Host for Native {
    fn services(&self) -> io::Result<Vec<Service>> {
        #[cfg(target_os = "macos")]
        {
            let output = std::process::Command::new("/bin/ps")
                .args(["-wwaxo", "pid=,uid=,comm="])
                .env_clear()
                .output()?;
            if !output.status.success() {
                return Err(invalid("cannot enumerate VirtualMachine service processes"));
            }
            let text = std::str::from_utf8(&output.stdout).map_err(invalid)?;
            let mut services = Vec::new();
            for line in text.lines() {
                if let Some((pid, uid)) = vm_service_row(line)? {
                    match motor_macvm::process_identity(pid) {
                        Ok(identity) => services.push(Service {
                            identity,
                            owned: uid == i64::from(unsafe { libc::geteuid() }),
                        }),
                        Err(e) if e.raw_os_error() == Some(libc::ESRCH) => {}
                        Err(e) => return Err(e),
                    }
                }
            }
            Ok(services)
        }
        #[cfg(not(target_os = "macos"))]
        Err(invalid("VirtualMachine observation requires macOS"))
    }
    fn alive(&self, process: ProcessIdentity) -> io::Result<bool> {
        #[cfg(target_os = "macos")]
        return motor_macvm::process::identity_alive(process);
        #[cfg(not(target_os = "macos"))]
        {
            let _ = process;
            Err(invalid("VM process identity requires macOS"))
        }
    }
    fn kill(&self, process: ProcessIdentity) -> io::Result<()> {
        signal_if_current(
            process,
            || self.alive(process),
            || {
                if unsafe { libc::kill(process.pid, libc::SIGKILL) } != 0 {
                    let e = io::Error::last_os_error();
                    if e.raw_os_error() != Some(libc::ESRCH) {
                        return Err(e);
                    }
                }
                Ok(())
            },
        )
    }
    fn release_proven(&self, record: &Record) -> io::Result<bool> {
        release_after_gate(record, || {
            #[cfg(target_os = "macos")]
            {
                motor_macvm::process::release_proven(
                    record.service,
                    &record.paths.clone,
                    &record.paths.nonce,
                )
            }
            #[cfg(not(target_os = "macos"))]
            {
                Err(invalid("VM disk-holder inspection requires macOS"))
            }
        })
    }
    fn stop_unrecorded(&self, record: &Record) -> io::Result<()> {
        #[cfg(target_os = "macos")]
        for process in motor_macvm::process::disk_services(&record.paths.clone)? {
            self.kill(process)?;
        }
        let _ = record;
        Ok(())
    }
}

pub(crate) struct Runtime {
    pub(super) jobs: Mutex<BTreeMap<Uuid, Arc<Entry>>>,
    pub(super) host: Arc<dyn Host>,
    cancelled: Mutex<std::collections::BTreeSet<Uuid>>,
    runner_lock: Mutex<Option<ScopedLock>>,
    grace: Duration,
    stop_timeout: Duration,
}
impl Default for Runtime {
    fn default() -> Self {
        Self {
            jobs: Mutex::new(BTreeMap::new()),
            host: Arc::new(Native),
            cancelled: Mutex::default(),
            runner_lock: Mutex::default(),
            grace: Duration::from_millis(600),
            stop_timeout: Duration::from_millis(1400),
        }
    }
}
impl Runtime {
    fn record_path(root: &Path, id: Uuid) -> PathBuf {
        root.join("macvm-records").join(format!("{id}.json"))
    }
    pub(super) fn save(root: &Path, record: &Record) -> io::Result<()> {
        atomic_json(&Self::record_path(root, record.id), record)
    }
    pub(super) fn begin(&self, root: &Path, id: Uuid, workspace: &str) -> io::Result<Arc<Entry>> {
        for name in [
            "macvm-records",
            "clones",
            "nonces",
            "targets",
            "macvm-locks",
        ] {
            Dir::open(&root.join(name), true)?;
        }
        let mut jobs = self.jobs.lock().unwrap();
        if jobs.contains_key(&id) {
            return Err(io::Error::new(
                io::ErrorKind::AlreadyExists,
                "job identifier still owns resources; never reuse a quarantined inventory",
            ));
        }
        let record = Record {
            id,
            workspace: workspace.into(),
            paths: Paths::new(root, id),
            slot: None,
            helper: None,
            gate_open: false,
            service: None,
            stop_proven: false,
            proof_version: RELEASE_PROOF_VERSION,
            quarantined: false,
        };
        // This durable inventory precedes staging, including the first job directory.
        Self::save(root, &record)?;
        let entry = Arc::new(Entry {
            owned: Mutex::new(Owned {
                record,
                slot: None,
                workspace_lock: None,
                helper: None,
            }),
            cancel: Cancellation::default(),
            active: AtomicBool::new(true),
            failed: AtomicBool::new(false),
            cleanup: Mutex::new(()),
            #[cfg(test)]
            output_blocked: AtomicBool::new(false),
        });
        if self.cancelled.lock().unwrap().contains(&id) {
            entry.cancel.cancel();
        }
        jobs.insert(id, entry.clone());
        Ok(entry)
    }
    pub(super) fn entry(&self, id: Uuid) -> Option<Arc<Entry>> {
        self.jobs.lock().unwrap().get(&id).cloned()
    }
    pub fn cancel(&self, id: Uuid) {
        self.cancelled.lock().unwrap().insert(id);
        if let Some(entry) = self.entry(id) {
            entry.cancel.cancel();
        }
    }
    pub fn interrupt(&self, id: Uuid) {
        if let Some(entry) = self.entry(id) {
            self.cancelled.lock().unwrap().insert(id);
            entry.cancel.cancel();
        }
    }
    pub fn cancelled(&self, id: Uuid) -> bool {
        self.cancelled.lock().unwrap().contains(&id)
    }
    pub fn prune_cancelled(&self, live: &std::collections::BTreeSet<Uuid>) {
        self.cancelled
            .lock()
            .unwrap()
            .retain(|id| live.contains(id));
    }
    pub fn cleanup_pending(&self, id: Uuid) -> bool {
        self.entry(id).is_some_and(|e| {
            !e.active.load(Ordering::Acquire) && !e.owned.lock().unwrap().record.quarantined
        })
    }
    pub fn workspace_busy(&self, workspace: &str, except: Uuid) -> bool {
        self.jobs.lock().unwrap().iter().any(|(id, e)| {
            let owned = e.owned.lock().unwrap();
            *id != except
                && !owned.record.quarantined
                && owned.record.workspace.eq_ignore_ascii_case(workspace)
        })
    }
    pub fn degraded(&self) -> bool {
        self.jobs
            .lock()
            .unwrap()
            .values()
            .any(|e| e.failed.load(Ordering::Acquire))
    }
    pub(super) fn workspace(&self, root: &Path, entry: &Entry) -> io::Result<()> {
        let workspace = entry.owned.lock().unwrap().record.workspace.clone();
        let file = control(entry.cancel.clone()).lock(
            &root
                .join("macvm-locks")
                .join(format!("workspace-{}.lock", key(&workspace))),
        )?;
        entry.owned.lock().unwrap().workspace_lock = Some(file);
        Ok(())
    }
    pub(super) fn admit(&self, root: &Path, entry: &Entry) -> io::Result<bool> {
        let _admission =
            control(entry.cancel.clone()).lock(&root.join("macvm-locks/admission.lock"))?;
        let jobs: Vec<_> = self.jobs.lock().unwrap().values().cloned().collect();
        let mut in_use = 0;
        let mut recorded = Vec::new();
        for e in jobs {
            let owned = e.owned.lock().unwrap();
            in_use += usize::from(owned.record.slot.is_some());
            recorded.extend(owned.record.service);
        }
        in_use += self
            .host
            .services()?
            .iter()
            .filter(|s| !recorded.contains(&s.identity))
            .count();
        if in_use >= 2 {
            return Ok(false);
        }
        for slot in 0..2 {
            let file = lock_file(&root.join("macvm-locks").join(format!("slot-{slot}.lock")))?;
            match fs2::FileExt::try_lock_exclusive(&file) {
                Ok(()) => {
                    let mut owned = entry.owned.lock().unwrap();
                    owned.slot = Some(file);
                    owned.record.slot = Some(slot);
                    Self::save(root, &owned.record)?;
                    return Ok(true);
                }
                Err(e) if e.kind() == io::ErrorKind::WouldBlock => {}
                Err(e) => return Err(e),
            }
        }
        Ok(false)
    }
    fn unrecorded_owned(&self) -> io::Result<bool> {
        let jobs: Vec<_> = self.jobs.lock().unwrap().values().cloned().collect();
        let recorded: Vec<_> = jobs
            .iter()
            .filter(|e| e.active.load(Ordering::Acquire))
            .filter_map(|e| e.owned.lock().unwrap().record.service)
            .collect();
        Ok(self
            .host
            .services()?
            .iter()
            .any(|s| s.owned && !recorded.contains(&s.identity)))
    }
    pub(super) fn stop(&self, root: &Path, entry: &Entry) -> io::Result<()> {
        let started = Instant::now();
        let record = {
            let mut owned = entry.owned.lock().unwrap();
            if owned.record.stop_proven && owned.record.proof_version == RELEASE_PROOF_VERSION {
                return Ok(());
            }
            owned.record.stop_proven = false;
            if let Some(helper) = owned.helper.as_mut() {
                helper.close_gate();
            }
            owned.record.clone()
        };
        let helper_alive = || -> io::Result<bool> {
            let mut owned = entry.owned.lock().unwrap();
            if let Some(helper) = owned.helper.as_mut() {
                return Ok(helper.child.try_wait()?.is_none());
            }
            record
                .helper
                .map(|process| self.host.alive(process))
                .transpose()
                .map(|alive| alive.unwrap_or(false))
        };
        // Each step names itself in its error, so a stop that cannot be proven
        // says which check failed instead of a bare OS error.
        let step = |name: &'static str| {
            move |e: io::Error| io::Error::new(e.kind(), format!("{name}: {e}"))
        };
        while helper_alive().map_err(step("helper liveness"))? && started.elapsed() < self.grace {
            std::thread::park_timeout(Duration::from_millis(5));
        }
        if helper_alive().map_err(step("helper liveness"))? {
            let mut owned = entry.owned.lock().unwrap();
            if let Some(helper) = owned.helper.as_mut() {
                helper.child.kill().map_err(step("helper kill"))?;
            } else if let Some(process) = record.helper {
                self.host
                    .kill(process)
                    .map_err(step("recorded helper kill"))?;
            }
        }
        if let Some(service) = record.service {
            self.host.kill(service).map_err(step("service kill"))?;
        }
        loop {
            if !helper_alive().map_err(step("helper liveness"))? {
                if record.gate_open {
                    self.host
                        .stop_unrecorded(&record)
                        .map_err(step("unrecorded service stop"))?;
                }
                let mut service_alive = false;
                if let Some(service) = record.service {
                    service_alive = self.host.alive(service).map_err(step("service liveness"))?;
                }
                // The scan is mandatory even when no service identity was ever
                // reported. A process with another name can still hold the disks.
                let released = self
                    .host
                    .release_proven(&record)
                    .map_err(step("disk release scan"))?;
                let unrecorded = record.gate_open
                    && record.service.is_none()
                    && self
                        .unrecorded_owned()
                        .map_err(step("unrecorded service scan"))?;
                if !service_alive && released && !unrecorded {
                    break;
                }
            }
            if started.elapsed() >= self.stop_timeout {
                return Err(invalid("macOS VM stop or disk release is unproven"));
            }
            std::thread::park_timeout(Duration::from_millis(5));
        }
        let mut owned = entry.owned.lock().unwrap();
        owned.record.stop_proven = true;
        owned.record.proof_version = RELEASE_PROOF_VERSION;
        Self::save(root, &owned.record)
    }
    pub(super) fn cleanup(&self, root: &Path, entry: &Entry) -> io::Result<()> {
        let record = entry.owned.lock().unwrap().record.clone();
        if record.paths != Paths::new(root, record.id) {
            return Err(invalid("macOS record paths do not match job identity"));
        }
        self.stop(root, entry)?;
        {
            let mut owned = entry.owned.lock().unwrap();
            owned.record.slot = None;
            Self::save(root, &owned.record)?;
            release_slot(&mut owned)?;
        }
        for path in record.paths.all() {
            self.host.delete(path)?;
        }
        remove(&Self::record_path(root, record.id))?;
        File::open(root.join("macvm-records"))?.sync_all()?;
        {
            let mut owned = entry.owned.lock().unwrap();
            owned.workspace_lock.take();
        }
        self.jobs.lock().unwrap().remove(&record.id);
        Ok(())
    }
    pub(super) fn finish(&self, root: &Path, entry: &Entry) {
        let Ok(_cleanup) = entry.cleanup.try_lock() else {
            return;
        };
        entry.active.store(false, Ordering::Release);
        if let Err(error) = self.cleanup(root, entry) {
            let stopped = {
                let owned = entry.owned.lock().unwrap();
                owned.record.stop_proven
                    && owned.record.proof_version == RELEASE_PROOF_VERSION
                    && owned.record.paths == Paths::new(root, owned.record.id)
            };
            if stopped {
                if let Err(quarantine_error) = self.quarantine(root, entry) {
                    tracing::warn!(%quarantine_error,"stopped job quarantine relocation incomplete; inventory retained");
                }
            }
            entry.failed.store(true, Ordering::Release);
            tracing::warn!(%error, stopped, "macOS cleanup failed; record retained, VM slots retained only for unproven stops");
        }
    }
    fn quarantine(&self, root: &Path, entry: &Entry) -> io::Result<()> {
        let record = {
            let mut owned = entry.owned.lock().unwrap();
            owned.record.quarantined = true;
            owned.record.slot = None;
            Self::save(root, &owned.record)?;
            release_slot(&mut owned)?;
            owned.workspace_lock.take();
            owned.record.clone()
        };
        let destination = Dir::open(&record.paths.quarantine, true)?;
        for (index, path) in record.paths.primary().iter().enumerate() {
            let parent = match Dir::open(path.parent().unwrap(), false) {
                Ok(parent) => parent,
                Err(e) if e.kind() == io::ErrorKind::NotFound => continue,
                Err(e) => return Err(e),
            };
            if parent.contains(path.file_name().unwrap())? {
                parent.move_to(
                    path.file_name().unwrap(),
                    &destination,
                    index.to_string().as_ref(),
                )?;
            }
        }
        Ok(())
    }
    pub fn retry_cleanups(&self, config: &Config) {
        let entries: Vec<_> = self
            .jobs
            .lock()
            .unwrap()
            .values()
            .filter(|e| !e.active.load(Ordering::Acquire))
            .cloned()
            .collect();
        for entry in entries {
            self.finish(&config.state_root, &entry);
        }
    }
    pub fn recover(&self, root: &Path) -> io::Result<()> {
        let mut runner_lock = self.runner_lock.lock().unwrap();
        if runner_lock.is_none() {
            let file = lock_file(&root.join("macvm-locks/runner.lock"))?;
            *runner_lock = Some(
                ScopedLock::try_exclusive(file)
                    .map_err(|e| invalid(format!("macOS state root is already served: {e}")))?,
            );
        }
        drop(runner_lock);
        let records = Dir::open(&root.join("macvm-records"), true)?;
        Dir::open(&root.join("macvm-locks"), true)?;
        for name in records.names()? {
            let Some(stem) = name.to_str().and_then(|n| n.strip_suffix(".json")) else {
                continue;
            };
            if Uuid::parse_str(stem).is_err() {
                continue;
            }
            let mut record: Record = match records
                .file(&name, false)
                .and_then(|file| serde_json::from_reader(file).map_err(invalid))
            {
                Ok(record) => record,
                Err(error) => {
                    tracing::warn!(?name,%error,"ignoring unreadable stray macOS record file");
                    continue;
                }
            };
            if record.paths.quarantine.as_os_str().is_empty() {
                record.paths.quarantine = Paths::new(root, record.id).quarantine;
            }
            if name != format!("{}.json", record.id).as_str() {
                tracing::warn!(?name, "ignoring misnamed stray macOS ownership record");
                continue;
            }
            if record.paths != Paths::new(root, record.id) || record.slot.is_some_and(|s| s > 1) {
                return Err(invalid("invalid macOS ownership record"));
            }
            super::mapped(&record.workspace)?;
            if record.stop_proven && record.proof_version == RELEASE_PROOF_VERSION {
                record.slot = None;
                Self::save(root, &record)?;
            } else if record.gate_open && record.slot.is_none() {
                return Err(invalid("unproven opened gate has no recorded VM slot"));
            }
            let slot = record
                .slot
                .map(|s| lock_file(&root.join("macvm-locks").join(format!("slot-{s}.lock"))))
                .transpose()?;
            // A surviving helper can still hold this lock. The record reserves
            // the slot even until recovery has killed and reaped that helper.
            if let Some(file) = &slot {
                let _ = fs2::FileExt::try_lock_exclusive(file);
            }
            let id = record.id;
            self.jobs.lock().unwrap().insert(
                id,
                Arc::new(Entry {
                    owned: Mutex::new(Owned {
                        record,
                        slot,
                        workspace_lock: None,
                        helper: None,
                    }),
                    cancel: Cancellation::default(),
                    active: AtomicBool::new(false),
                    failed: AtomicBool::new(false),
                    cleanup: Mutex::new(()),
                    #[cfg(test)]
                    output_blocked: AtomicBool::new(false),
                }),
            );
        }
        let entries: Vec<_> = self.jobs.lock().unwrap().values().cloned().collect();
        for entry in entries {
            self.finish(root, &entry);
        }
        Ok(())
    }
}
pub(super) fn key(workspace: &str) -> String {
    format!(
        "{:x}",
        Sha256::digest(workspace.to_ascii_lowercase().as_bytes())
    )
}
pub(super) fn control(cancel: Cancellation) -> Control {
    Control {
        cancel,
        deadline: Instant::now() + PREP_TIMEOUT,
    }
}
fn lock_file(path: &Path) -> io::Result<File> {
    let dir = Dir::open(path.parent().ok_or_else(|| invalid("lock parent"))?, true)?;
    match dir.file(path.file_name().unwrap(), true) {
        Ok(f) => Ok(f),
        Err(e) if e.kind() == io::ErrorKind::AlreadyExists => {
            dir.file(path.file_name().unwrap(), false)
        }
        Err(e) => Err(e),
    }
}

pub(crate) fn evict(
    config: &Config,
    mirrors: PathBuf,
) -> io::Result<Option<motor_protocol::RunnerDiskFull>> {
    let root = &config.state_root;
    let probe = || (config.free_space_probe)(root);
    let floor = 20 << 30;
    if probe()? < 40 << 30 {
        let staging = staging(config, mirrors)?;
        let mut candidates = Vec::new();
        fn canonical(
            path: &Path,
            logical: &str,
            candidates: &mut Vec<(PathBuf, String)>,
        ) -> io::Result<()> {
            let dir = match Dir::open(path, false) {
                Ok(dir) => dir,
                Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(()),
                Err(e) => return Err(e),
            };
            if dir.contains(".git".as_ref())? {
                candidates.push((path.to_path_buf(), logical.into()));
                return Ok(());
            }
            for name in dir.names()? {
                if dir.kind(&name)? == libc::S_IFDIR {
                    let name = name
                        .to_str()
                        .ok_or_else(|| invalid("non-UTF8 canonical path"))?;
                    canonical(&path.join(name), &format!("{logical}/{name}"), candidates)?;
                }
            }
            Ok(())
        }
        canonical(&root.join("canonical"), "", &mut candidates)?;
        for (path, logical) in candidates {
            if probe()? >= 40 << 30 {
                break;
            }
            let lock = lock_file(&staging.path_lock(&logical))?;
            if let Ok(_lock) = ScopedLock::try_exclusive(lock) {
                remove(&path)?;
            }
        }
        if let Ok(targets) = Dir::open(&root.join("targets"), false) {
            for name in targets.names()? {
                if probe()? >= 40 << 30 {
                    break;
                }
                let Some(key) = name
                    .to_str()
                    .filter(|s| s.len() == 64 && s.bytes().all(|b| b.is_ascii_hexdigit()))
                else {
                    continue;
                };
                let lock = lock_file(
                    &root
                        .join("macvm-locks")
                        .join(format!("workspace-{key}.lock")),
                )?;
                if let Ok(_lock) = ScopedLock::try_exclusive(lock) {
                    targets.remove(&name)?;
                }
            }
        }
        if probe()? < 40 << 30 {
            let lock = lock_file(&root.join("prep.lock"))?;
            if let Ok(_lock) = ScopedLock::try_exclusive(lock) {
                let state = Dir::open(root, false)?;
                state.remove("prep-cargo".as_ref())?;
                state.remove("prep-bun".as_ref())?;
            }
        }
    }
    let free = probe()?;
    Ok((free < floor).then_some(motor_protocol::RunnerDiskFull {
        free_bytes: free,
        floor_bytes: floor,
    }))
}
pub(super) fn remove(path: &Path) -> io::Result<()> {
    match Dir::open(
        path.parent().ok_or_else(|| invalid("remove parent"))?,
        false,
    ) {
        Ok(dir) => dir.remove(path.file_name().ok_or_else(|| invalid("remove name"))?),
        Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(()),
        Err(e) => Err(e),
    }
}
pub(super) fn staging(config: &Config, mirrors: PathBuf) -> io::Result<Staging> {
    Staging::new(
        config.state_root.clone(),
        // The engine's state directory, where it keeps Git dependency mirrors.
        config.state_dir(),
        mirrors,
        config
            .macos
            .as_ref()
            .ok_or_else(|| invalid("missing macOS config"))?
            .tools
            .clone(),
    )
}

#[cfg(test)]
#[path = "lifecycle_tests.rs"]
mod tests;
