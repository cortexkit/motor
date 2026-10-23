//! Durable VM ownership, serialized admission, and conservative restart recovery.
use super::{fs::Dir, Cancellation, Control, Staging, Tools, PREP_TIMEOUT};
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
        }
    }
    fn all(&self) -> [&Path; 6] {
        [
            &self.clone,
            &self.nonce,
            &self.cargo,
            &self.tree,
            &self.prep,
            &self.target_tmp,
        ]
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
}
pub(super) struct Owned {
    pub record: Record,
    pub slot: Option<File>,
    pub workspace_lock: Option<File>,
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
/// Process observations are injectable, but every real kill still requires an exact start timestamp.
pub(super) trait Host: Send + Sync {
    fn services(&self) -> io::Result<Vec<Service>>;
    fn alive(&self, process: ProcessIdentity) -> io::Result<bool>;
    fn kill(&self, process: ProcessIdentity) -> io::Result<()>;
    fn release_proven(&self, record: &Record) -> io::Result<bool>;
    fn delete(&self, path: &Path) -> io::Result<()> {
        remove(path)
    }
}
struct Native;
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
                let mut fields = line.split_whitespace();
                let pid = fields
                    .next()
                    .ok_or_else(|| invalid("missing process pid"))?
                    .parse()
                    .map_err(invalid)?;
                let uid: u32 = fields
                    .next()
                    .ok_or_else(|| invalid("missing process uid"))?
                    .parse()
                    .map_err(invalid)?;
                if fields.collect::<Vec<_>>().join(" ").rsplit('/').next()
                    == Some("com.apple.Virtualization.VirtualMachine")
                {
                    match motor_macvm::process_identity(pid) {
                        Ok(identity) => services.push(Service {
                            identity,
                            owned: uid == unsafe { libc::geteuid() },
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
        if self.alive(process)? && unsafe { libc::kill(process.pid, libc::SIGKILL) } != 0 {
            let e = io::Error::last_os_error();
            if e.raw_os_error() != Some(libc::ESRCH) {
                return Err(e);
            }
        }
        Ok(())
    }
    fn release_proven(&self, record: &Record) -> io::Result<bool> {
        #[cfg(target_os = "macos")]
        if let Some(service) = record.service {
            return motor_macvm::process::release_proven(
                service,
                &record.paths.clone,
                &record.paths.nonce,
            );
        }
        let _ = record;
        Ok(true)
    }
}

pub(crate) struct Runtime {
    pub(super) jobs: Mutex<BTreeMap<Uuid, Arc<Entry>>>,
    pub(super) host: Arc<dyn Host>,
    cancelled: Mutex<std::collections::BTreeSet<Uuid>>,
    runner_lock: Mutex<Option<File>>,
}
impl Default for Runtime {
    fn default() -> Self {
        Self {
            jobs: Mutex::new(BTreeMap::new()),
            host: Arc::new(Native),
            cancelled: Mutex::default(),
            runner_lock: Mutex::default(),
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
        if let Some(entry) = jobs.get(&id) {
            return Ok(entry.clone());
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
        self.entry(id)
            .is_some_and(|e| !e.active.load(Ordering::Acquire))
    }
    pub fn workspace_busy(&self, workspace: &str, except: Uuid) -> bool {
        self.jobs
            .lock()
            .unwrap()
            .iter()
            .any(|(id, e)| *id != except && e.owned.lock().unwrap().record.workspace == workspace)
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
        let record = {
            let mut owned = entry.owned.lock().unwrap();
            if owned.record.stop_proven {
                return Ok(());
            }
            if let Some(helper) = owned.helper.as_mut() {
                helper.close_gate();
                if helper.child.try_wait()?.is_none() {
                    helper.child.kill()?;
                }
            }
            owned.record.clone()
        };
        for process in [record.helper, record.service].into_iter().flatten() {
            self.host.kill(process)?;
        }
        let deadline = Instant::now() + Duration::from_millis(1400);
        loop {
            // Reap our child before testing its identity: a zombie still has a PID.
            let mut alive = match entry.owned.lock().unwrap().helper.as_mut() {
                Some(helper) => helper.child.try_wait()?.is_none(),
                None => false,
            };
            for process in [record.helper, record.service].into_iter().flatten() {
                alive |= self.host.alive(process)?;
            }
            if !alive {
                break;
            }
            if Instant::now() >= deadline {
                return Err(invalid("macOS VM stop is not proven"));
            }
            std::thread::park_timeout(Duration::from_millis(5));
        }
        if record.gate_open && record.service.is_none() && self.unrecorded_owned()? {
            return Err(invalid(
                "unrecorded motor-owned VirtualMachine service; stop unproven",
            ));
        }
        if !self.host.release_proven(&record)? {
            return Err(invalid("VM resources are still open; stop unproven"));
        }
        let mut owned = entry.owned.lock().unwrap();
        owned.record.stop_proven = true;
        Self::save(root, &owned.record)
    }
    pub(super) fn cleanup(&self, root: &Path, entry: &Entry) -> io::Result<()> {
        let record = entry.owned.lock().unwrap().record.clone();
        if record.paths != Paths::new(root, record.id) {
            return Err(invalid("macOS record paths do not match job identity"));
        }
        self.stop(root, entry)?;
        for path in record.paths.all() {
            self.host.delete(path)?;
        }
        remove(&Self::record_path(root, record.id))?;
        File::open(root.join("macvm-records"))?.sync_all()?;
        {
            let mut owned = entry.owned.lock().unwrap();
            owned.slot.take();
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
            entry.failed.store(true, Ordering::Release);
            tracing::warn!(%error, "macOS cleanup failed; degraded, retaining record and slot");
        }
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
            fs2::FileExt::try_lock_exclusive(&file)
                .map_err(|e| invalid(format!("macOS state root is already served: {e}")))?;
            *runner_lock = Some(file);
        }
        drop(runner_lock);
        let records = Dir::open(&root.join("macvm-records"), true)?;
        Dir::open(&root.join("macvm-locks"), true)?;
        for name in records.names()? {
            if !name.to_string_lossy().ends_with(".json") {
                return Err(invalid("unexpected macOS job record"));
            }
            let record: Record =
                serde_json::from_reader(records.file(&name, false)?).map_err(invalid)?;
            if name != format!("{}.json", record.id).as_str()
                || record.paths != Paths::new(root, record.id)
                || record.slot.is_some_and(|s| s > 1)
            {
                return Err(invalid("invalid macOS ownership record"));
            }
            super::mapped(&record.workspace)?;
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
    format!("{:x}", Sha256::digest(workspace.as_bytes()))
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
            if fs2::FileExt::try_lock_exclusive(&lock).is_ok() {
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
                if fs2::FileExt::try_lock_exclusive(&lock).is_ok() {
                    targets.remove(&name)?;
                }
            }
        }
        if probe()? < 40 << 30 {
            let lock = lock_file(&root.join("prep.lock"))?;
            if fs2::FileExt::try_lock_exclusive(&lock).is_ok() {
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
