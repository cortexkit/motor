//! Durable inode seals for source files. The record outlives both the daemon
//! and pathnames a job can rename; only a proven stopped unit may be unsealed.
use crate::{caller_path as paths, Config};
use motor_protocol::{Run, Uuid};
use serde::{Deserialize, Serialize};
use std::{
    collections::BTreeMap,
    ffi::CString,
    fs::{self, File, OpenOptions},
    io::{self, Read, Write},
    os::{
        fd::{AsRawFd, FromRawFd},
        unix::{
            ffi::OsStrExt,
            fs::{MetadataExt, OpenOptionsExt},
        },
    },
    path::{Component, Path, PathBuf},
    process::Stdio,
    sync::Mutex,
};

/// A real sandbox must receive either a trusted snapshot source or the
/// explicit compile-cache writer exemption. Resolution happens at its launch
/// boundary, not in callers that only construct or inspect a launch plan.
pub(crate) enum Sources<'a> {
    Snapshot(Snapshot<'a>),
    CompileCacheWriter,
}

pub(crate) struct Snapshot<'a> {
    config: &'a Config,
    trees: &'a crate::workspace::Trees,
    request: &'a Run,
}

impl<'a> Sources<'a> {
    pub(crate) fn snapshot(
        config: &'a Config,
        trees: &'a crate::workspace::Trees,
        request: &'a Run,
    ) -> Self {
        Self::Snapshot(Snapshot {
            config,
            trees,
            request,
        })
    }

    pub(crate) fn seal(
        self,
        id: Uuid,
        writer: bool,
        prepared: &[PathBuf],
    ) -> io::Result<Option<ActiveSeal>> {
        match self {
            Self::CompileCacheWriter if writer => Ok(None),
            Self::Snapshot(snapshot) if !writer => {
                let mirror = snapshot.trees.mirror(
                    paths::request(snapshot.request)
                        .caller_repository_root
                        .text(),
                );
                let tree = step(
                    &mirror,
                    "enumerate snapshot",
                    crate::workspace::snapshot_tree(&mirror, &snapshot.request.tree_hash),
                )?;
                let root = paths::host_run(snapshot.config, snapshot.request).host_workspace;
                let mut plan = step(
                    &root,
                    "compute sealed set",
                    plan(snapshot.config, snapshot.request, prepared, &tree),
                )?;
                plan.unit = id;
                let job_id = snapshot.request.job_id;
                snapshot
                    .config
                    .source_seals
                    .seal(&snapshot.trees.state, job_id, plan)?;
                Ok(Some(ActiveSeal {
                    runtime: snapshot.config.source_seals.clone(),
                    state: snapshot.trees.state.clone(),
                    id: job_id,
                    unit: id,
                }))
            }
            _ => Err(io::Error::other(
                "sandbox role does not match required source protection",
            )),
        }
    }
}

pub(crate) struct ActiveSeal {
    runtime: std::sync::Arc<Runtime>,
    state: PathBuf,
    id: Uuid,
    unit: Uuid,
}

impl ActiveSeal {
    pub(crate) fn failed_launch(self) -> io::Result<()> {
        let process = crate::platform::ProcessIdentity {
            pid: 0,
            start: 0,
            unit: Some(self.unit),
        };
        if crate::platform::unit_alive(Some(process))? != Some(false) {
            return Err(io::Error::other(
                "failed launch unit is not proven stopped; source seal retained",
            ));
        }
        self.runtime.unseal(&self.state, self.id)
    }
}

#[derive(Default)]
pub(crate) struct Runtime(Mutex<BTreeMap<Uuid, Plan>>, Mutex<()>);

#[derive(Clone, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct Plan {
    unit: Uuid,
    workspace: PathBuf,
    caller_workspace: PathBuf,
    #[serde(with = "path_list_bytes")]
    files: Vec<PathBuf>,
}

#[derive(Serialize, Deserialize)]
struct Record {
    plan: Plan,
    handles: Vec<Handle>,
}

#[derive(Serialize, Deserialize)]
struct Handle {
    #[serde(with = "path_bytes")]
    path: PathBuf,
    kind: i32,
    bytes: Vec<u8>,
}

mod path_bytes {
    use super::*;
    use base64::Engine;
    use std::os::unix::ffi::OsStringExt;

    pub(super) fn encode(path: &Path) -> String {
        base64::engine::general_purpose::STANDARD.encode(path.as_os_str().as_bytes())
    }
    pub(super) fn decode(value: String) -> Result<PathBuf, base64::DecodeError> {
        base64::engine::general_purpose::STANDARD
            .decode(value)
            .map(|bytes| PathBuf::from(std::ffi::OsString::from_vec(bytes)))
    }
    pub(super) fn serialize<S: serde::Serializer>(
        path: &Path,
        serializer: S,
    ) -> Result<S::Ok, S::Error> {
        encode(path).serialize(serializer)
    }
    pub(super) fn deserialize<'de, D: serde::Deserializer<'de>>(
        deserializer: D,
    ) -> Result<PathBuf, D::Error> {
        decode(String::deserialize(deserializer)?).map_err(serde::de::Error::custom)
    }
}

mod path_list_bytes {
    use super::*;
    pub(super) fn serialize<S: serde::Serializer>(
        paths: &[PathBuf],
        serializer: S,
    ) -> Result<S::Ok, S::Error> {
        paths
            .iter()
            .map(|path| path_bytes::encode(path))
            .collect::<Vec<_>>()
            .serialize(serializer)
    }
    pub(super) fn deserialize<'de, D: serde::Deserializer<'de>>(
        deserializer: D,
    ) -> Result<Vec<PathBuf>, D::Error> {
        Vec::<String>::deserialize(deserializer)?
            .into_iter()
            .map(|value| path_bytes::decode(value).map_err(serde::de::Error::custom))
            .collect()
    }
}

pub(crate) fn record_path(state: &Path, id: Uuid) -> PathBuf {
    state.join("jobs").join(id.to_string()).join("seal")
}

fn intent_path(state: &Path, id: Uuid) -> PathBuf {
    record_path(state, id).with_file_name("seal-intent")
}

fn operation_lock(state: &Path, id: Uuid) -> io::Result<File> {
    let file = OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .custom_flags(libc::O_NOFOLLOW)
        .mode(0o600)
        .open(record_path(state, id).with_file_name("seal-lock"))?;
    fs2::FileExt::lock_exclusive(&file)?;
    Ok(file)
}

fn overlap(a: &Path, b: &Path) -> bool {
    a.starts_with(b) || b.starts_with(a)
}

impl Runtime {
    pub(crate) fn load(&self, state: &Path) -> io::Result<()> {
        let mut pending = BTreeMap::new();
        let entries = match fs::read_dir(state.join("jobs")) {
            Ok(entries) => entries,
            Err(error) if error.kind() == io::ErrorKind::NotFound => {
                self.0.lock().unwrap().clear();
                return Ok(());
            }
            Err(error) => return Err(error),
        };
        for entry in entries {
            let entry = entry?;
            let Ok(id) = entry.file_name().to_string_lossy().parse() else {
                continue;
            };
            match fs::read(entry.path().join("seal")) {
                Ok(bytes) => {
                    let record: Record = serde_json::from_slice(&bytes)?;
                    pending.insert(id, record.plan);
                }
                Err(error) if error.kind() == io::ErrorKind::NotFound => {
                    match fs::read(intent_path(state, id)) {
                        Ok(bytes) => {
                            pending.insert(id, serde_json::from_slice(&bytes)?);
                        }
                        Err(error) if error.kind() == io::ErrorKind::NotFound => {}
                        Err(error) => return Err(error),
                    }
                }
                Err(error) => return Err(error),
            }
        }
        *self.0.lock().unwrap() = pending;
        Ok(())
    }

    pub(crate) fn ids(&self) -> Vec<Uuid> {
        self.0.lock().unwrap().keys().copied().collect()
    }

    pub(crate) fn unit(&self, id: Uuid) -> Option<Uuid> {
        self.0.lock().unwrap().get(&id).map(|plan| plan.unit)
    }

    pub(crate) fn blocks_caller(&self, path: &str) -> bool {
        self.0
            .lock()
            .unwrap()
            .values()
            .any(|plan| overlap(&plan.caller_workspace, Path::new(path)))
    }

    pub(crate) fn roots(&self) -> Vec<PathBuf> {
        self.0
            .lock()
            .unwrap()
            .values()
            .map(|plan| plan.workspace.clone())
            .collect()
    }

    pub(crate) fn check_host(&self, path: &Path) -> io::Result<()> {
        if self.roots().iter().any(|root| overlap(root, path)) {
            Err(io::Error::other(format!(
                "source seal pending for {}",
                path.display()
            )))
        } else {
            Ok(())
        }
    }

    pub(crate) fn seal(&self, state: &Path, id: Uuid, plan: Plan) -> io::Result<()> {
        let _operation = self.1.lock().unwrap();
        if self.0.lock().unwrap().contains_key(&id) {
            return Err(io::Error::other("source seal already pending for job"));
        }
        self.0.lock().unwrap().insert(id, plan.clone());
        // An intent covers the helper's pre-record interval across daemon
        // crashes. A per-job kernel lock serializes a surviving old helper
        // with startup unseal, and a late helper must still find this intent.
        if let Err(error) = step(
            &intent_path(state, id),
            "sync seal intent",
            crate::storage::atomic_json_private(&intent_path(state, id), &plan),
        ) {
            if !intent_path(state, id).try_exists().unwrap_or(true) {
                self.0.lock().unwrap().remove(&id);
            }
            return Err(error);
        }
        let result = step(
            &plan.workspace,
            "apply source seal",
            privileged("seal", state, id, Some(&plan)),
        );
        if let Err(error) = result {
            if record_path(state, id).try_exists().unwrap_or(true)
                || intent_path(state, id).try_exists().unwrap_or(true)
            {
                if let Err(cleanup) = privileged("unseal", state, id, None) {
                    return Err(io::Error::other(format!("{error}; rollback: {cleanup}")));
                }
            }
            self.0.lock().unwrap().remove(&id);
            return Err(error);
        }
        Ok(())
    }

    pub(crate) fn unseal(&self, state: &Path, id: Uuid) -> io::Result<()> {
        let _operation = self.1.lock().unwrap();
        privileged("unseal", state, id, None)?;
        self.0.lock().unwrap().remove(&id);
        Ok(())
    }
}

pub(crate) fn plan(
    config: &Config,
    request: &Run,
    prepared: &[PathBuf],
    tree: &[u8],
) -> io::Result<Plan> {
    let workspace = paths::host_run(config, request).host_workspace;
    // The collector strips the caller workspace prefix, not cwd. Requests
    // already contain absolute caller return paths, including a subfolder cwd.
    let caller = paths::run(request).workspace.checkout().caller.text();
    let returns = paths::run(request)
        .caller_return_paths
        .iter()
        .map(|path| {
            let relative = Path::new(path)
                .strip_prefix(caller)
                .map_err(|_| io::Error::other("return path escapes workspace"))?;
            validate_relative(relative)?;
            let host = paths::host_path(config, path);
            if host.strip_prefix(&workspace).ok() != Some(relative) {
                return Err(io::Error::other(
                    "return path mapping differs from workspace mapping",
                ));
            }
            Ok(relative.to_path_buf())
        })
        .collect::<io::Result<Vec<_>>>()?;
    let mut files = Vec::new();
    for entry in tree
        .split(|byte| *byte == 0)
        .filter(|entry| !entry.is_empty())
    {
        let tab = entry
            .iter()
            .position(|byte| *byte == b'\t')
            .ok_or_else(|| io::Error::other("invalid snapshot tree entry"))?;
        let header = &entry[..tab];
        if !header.starts_with(b"100644 blob ") && !header.starts_with(b"100755 blob ") {
            continue;
        }
        let path = Path::new(std::ffi::OsStr::from_bytes(&entry[tab + 1..]));
        validate_relative(path)?;
        if returns.iter().any(|returned| returned == path)
            || prepared
                .iter()
                .any(|directory| path.starts_with(directory.join("node_modules")))
        {
            continue;
        }
        files.push(path.to_path_buf());
    }
    Ok(Plan {
        unit: request.job_id,
        workspace,
        caller_workspace: PathBuf::from(caller),
        files,
    })
}

fn validate_relative(path: &Path) -> io::Result<()> {
    if path.as_os_str().is_empty()
        || path
            .components()
            .any(|part| !matches!(part, Component::Normal(_)))
    {
        return Err(io::Error::other(format!(
            "nonphysical source path {}",
            path.display()
        )));
    }
    Ok(())
}

fn privileged(action: &str, state: &Path, id: Uuid, plan: Option<&Plan>) -> io::Result<()> {
    if unsafe { libc::geteuid() } == 0 {
        return match plan {
            Some(plan) => seal(state, id, plan.clone()),
            None => unseal(state, id),
        };
    }
    let mut command = crate::permissions::seal_command(action, state, id)?;
    let mut child = command
        .stdin(Stdio::piped())
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .spawn()?;
    let sent = if let Some(plan) = plan {
        let bytes = serde_json::to_vec(plan)?;
        child.stdin.take().unwrap().write_all(&bytes)
    } else {
        drop(child.stdin.take());
        Ok(())
    };
    let output = child.wait_with_output()?;
    sent?;
    if !output.status.success() {
        return Err(io::Error::other(format!(
            "source {action}: {}",
            String::from_utf8_lossy(&output.stderr)
        )));
    }
    Ok(())
}

pub(crate) fn helper(action: &str, args: &[String]) -> io::Result<()> {
    if unsafe { libc::geteuid() } != 0 || args.len() != 2 {
        return Err(io::Error::other(
            "source seal helper requires root, job id and state root",
        ));
    }
    let id = args[0].parse::<Uuid>().map_err(io::Error::other)?;
    let state = Path::new(&args[1]);
    crate::permissions::physical_root(state)?;
    match action {
        "seal" => {
            let mut bytes = Vec::new();
            io::stdin().take(64 * 1024 * 1024).read_to_end(&mut bytes)?;
            seal(state, id, serde_json::from_slice(&bytes)?)
        }
        "unseal" => unseal(state, id),
        _ => Err(io::Error::other("unknown source seal operation")),
    }
}

fn step<T>(path: &Path, operation: &str, result: io::Result<T>) -> io::Result<T> {
    result.map_err(|error| {
        io::Error::other(format!(
            "source seal {}: {operation}: {error}",
            path.display()
        ))
    })
}

fn open_mount(path: &Path) -> io::Result<File> {
    let pinned = crate::permissions::physical_root(path)?;
    // open_by_handle_at needs a real mount descriptor, not the O_PATH
    // descriptor used only to pin the physical workspace ancestry.
    let fd = unsafe {
        libc::openat(
            pinned.as_raw_fd(),
            c".".as_ptr(),
            libc::O_RDONLY
                | libc::O_DIRECTORY
                | libc::O_NOFOLLOW
                | libc::O_CLOEXEC
                | libc::O_NONBLOCK,
        )
    };
    if fd < 0 {
        Err(io::Error::last_os_error())
    } else {
        Ok(unsafe { File::from_raw_fd(fd) })
    }
}

fn open_source(root: &File, path: &Path) -> io::Result<File> {
    validate_relative(path)?;
    let name = CString::new(path.as_os_str().as_bytes())?;
    let mut how: libc::open_how = unsafe { std::mem::zeroed() };
    how.flags = (libc::O_RDONLY | libc::O_NOFOLLOW | libc::O_NONBLOCK | libc::O_CLOEXEC) as u64;
    how.resolve = 0x08 | 0x04 | 0x01;
    // BENEATH and NO_SYMLINKS prevent replacement paths escaping the pinned
    // tree. NO_XDEV also guarantees every handle is on the workspace mount.
    let fd = unsafe {
        libc::syscall(
            libc::SYS_openat2,
            root.as_raw_fd(),
            name.as_ptr(),
            &how,
            std::mem::size_of_val(&how),
        )
    };
    if fd < 0 {
        return Err(io::Error::last_os_error());
    }
    let file = unsafe { File::from_raw_fd(fd as i32) };
    if !file.metadata()?.is_file() {
        return Err(io::Error::other("not a regular file"));
    }
    Ok(file)
}

#[repr(C)]
struct KernelHandle {
    size: u32,
    kind: i32,
    bytes: [u8; 128],
}

fn handle(file: &File, path: PathBuf) -> io::Result<Handle> {
    let mut value = KernelHandle {
        size: 128,
        kind: 0,
        bytes: [0; 128],
    };
    let mut mount = 0;
    let result = unsafe {
        libc::syscall(
            libc::SYS_name_to_handle_at,
            file.as_raw_fd(),
            c"".as_ptr(),
            &mut value,
            &mut mount,
            libc::AT_EMPTY_PATH,
        )
    };
    if result < 0 {
        return Err(io::Error::last_os_error());
    }
    if value.size > 128 {
        return Err(io::Error::other("oversized inode handle"));
    }
    Ok(Handle {
        path,
        kind: value.kind,
        bytes: value.bytes[..value.size as usize].to_vec(),
    })
}

fn open_handle(mount: &File, handle: &Handle) -> io::Result<File> {
    if handle.bytes.len() > 128 {
        return Err(io::Error::other("oversized inode handle"));
    }
    let mut value = KernelHandle {
        size: handle.bytes.len() as u32,
        kind: handle.kind,
        bytes: [0; 128],
    };
    value.bytes[..handle.bytes.len()].copy_from_slice(&handle.bytes);
    let fd = unsafe {
        libc::syscall(
            libc::SYS_open_by_handle_at,
            mount.as_raw_fd(),
            &value,
            libc::O_RDONLY | libc::O_NONBLOCK | libc::O_CLOEXEC,
        )
    };
    if fd < 0 {
        Err(io::Error::last_os_error())
    } else {
        let file = unsafe { File::from_raw_fd(fd as i32) };
        if !file.metadata()?.is_file() {
            return Err(io::Error::other("inode handle is not a regular file"));
        }
        Ok(file)
    }
}

const IMMUTABLE: libc::c_long = 0x10;
fn file_flags(file: &File) -> io::Result<libc::c_long> {
    let mut flags: libc::c_long = 0;
    if unsafe { libc::ioctl(file.as_raw_fd(), libc::FS_IOC_GETFLAGS, &mut flags) } != 0 {
        return Err(io::Error::other(format!(
            "FS_IOC_GETFLAGS: {}",
            io::Error::last_os_error()
        )));
    }
    Ok(flags)
}

fn immutable(file: &File, enabled: bool) -> io::Result<()> {
    let mut flags = file_flags(file)?;
    flags = if enabled {
        flags | IMMUTABLE
    } else {
        flags & !IMMUTABLE
    };
    if unsafe { libc::ioctl(file.as_raw_fd(), libc::FS_IOC_SETFLAGS, &flags) } != 0 {
        return Err(io::Error::other(format!(
            "FS_IOC_SETFLAGS: {}",
            io::Error::last_os_error()
        )));
    }
    Ok(())
}

fn seal(state: &Path, id: Uuid, plan: Plan) -> io::Result<()> {
    let _operation = operation_lock(state, id)?;
    let intent: Plan = serde_json::from_slice(&fs::read(intent_path(state, id))?)?;
    if intent != plan {
        return Err(io::Error::other(
            "source seal intent differs from helper request",
        ));
    }
    let started = std::time::Instant::now();
    let mount = step(
        &plan.workspace,
        "pin workspace",
        open_mount(&plan.workspace),
    )?;
    let mut handles = Vec::new();
    for path in &plan.files {
        let file = step(path, "openat2/fstat", open_source(&mount, path))?;
        handles.push(step(
            path,
            "name_to_handle_at",
            handle(&file, path.clone()),
        )?);
    }
    let record = Record { plan, handles };
    let path = record_path(state, id);
    if path.try_exists()? {
        return Err(io::Error::other(format!(
            "source seal record already exists: {}",
            path.display()
        )));
    }
    let directory = File::open(path.parent().unwrap())?;
    // Publish a complete synced record atomically, without replacing any old
    // recovery obligation. A crash while writing the staging file set no flags.
    let staging = path.with_extension("new");
    let mut durable = step(
        &staging,
        "create record",
        OpenOptions::new()
            .write(true)
            .create(true)
            .truncate(true)
            .custom_flags(libc::O_NOFOLLOW)
            .mode(0o600)
            .open(&staging),
    )?;
    let saved = step(
        &path,
        "sync record",
        (|| {
            // The unprivileged daemon must read recovery records written by
            // its root helper; job accounts cannot traverse the state root.
            let owner = directory.metadata()?;
            if unsafe { libc::fchown(durable.as_raw_fd(), owner.uid(), owner.gid()) } != 0 {
                return Err(io::Error::last_os_error());
            }
            durable.write_all(&serde_json::to_vec(&record)?)?;
            durable.sync_all()?;
            fs::hard_link(&staging, &path)?;
            directory.sync_all()?;
            fs::remove_file(&staging)
        })(),
    );
    if let Err(error) = saved {
        if path.exists() {
            if let Err(cleanup) = unseal_locked(state, id) {
                return Err(io::Error::other(format!("{error}; rollback: {cleanup}")));
            }
        }
        return Err(error);
    }
    for entry in &record.handles {
        // Reopen the recorded inode instead of retaining one descriptor per
        // source file, so large snapshots do not exhaust the helper's fd limit.
        let set = step(
            &entry.path,
            "open_by_handle_at/fstat",
            open_handle(&mount, entry),
        )
        .and_then(|file| immutable(&file, true));
        if let Err(error) = step(&entry.path, "set FS_IMMUTABLE_FL", set) {
            if let Err(cleanup) = unseal_locked(state, id) {
                return Err(io::Error::other(format!("{error}; rollback: {cleanup}")));
            }
            return Err(error);
        }
    }
    tracing::info!(job_id = %id, files = record.handles.len(), elapsed_us = started.elapsed().as_micros(), "source sealed");
    Ok(())
}

fn unseal(state: &Path, id: Uuid) -> io::Result<()> {
    let _operation = operation_lock(state, id)?;
    unseal_locked(state, id)
}

fn remove_intent(state: &Path, id: Uuid) -> io::Result<()> {
    match fs::remove_file(intent_path(state, id)) {
        Ok(()) => {}
        Err(error) if error.kind() == io::ErrorKind::NotFound => {}
        Err(error) => return Err(error),
    }
    File::open(record_path(state, id).parent().unwrap())?.sync_all()
}

fn unseal_locked(state: &Path, id: Uuid) -> io::Result<()> {
    let started = std::time::Instant::now();
    let path = record_path(state, id);
    let bytes = match fs::read(&path) {
        Ok(bytes) => bytes,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return remove_intent(state, id),
        Err(error) => return Err(error),
    };
    let record: Record = serde_json::from_slice(&bytes)?;
    let mount = step(
        &record.plan.workspace,
        "pin unseal mount",
        open_mount(&record.plan.workspace),
    )?;
    let mut failed = None;
    for handle in &record.handles {
        let clear = match open_handle(&mount, handle) {
            Ok(file) => immutable(&file, false),
            Err(error) if error.raw_os_error() == Some(libc::ESTALE) => Ok(()),
            Err(error) => Err(error),
        };
        if let Err(error) = step(
            &handle.path,
            "open_by_handle_at/clear FS_IMMUTABLE_FL",
            clear,
        ) {
            failed.get_or_insert(error);
        }
    }
    if let Some(error) = failed {
        return Err(error);
    }
    fs::remove_file(&path)?;
    remove_intent(state, id)?;
    tracing::info!(job_id = %id, files = record.handles.len(), elapsed_us = started.elapsed().as_micros(), "source unsealed");
    Ok(())
}

#[cfg(test)]
mod tests;
