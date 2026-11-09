//! Durable inode seals for source files. The record outlives both the daemon
//! and pathnames a job can rename; only a proven stopped unit may be unsealed.
use crate::{caller_path as paths, Config};
use motor_protocol::{Run, Uuid};
use serde::{Deserialize, Serialize};
use std::{
    collections::BTreeMap,
    ffi::{CStr, CString},
    fs::{self, File},
    io::{self, Read, Write},
    os::{
        fd::{AsRawFd, FromRawFd},
        unix::{ffi::OsStrExt, fs::MetadataExt},
    },
    path::{Component, Path, PathBuf},
    process::Stdio,
    sync::Mutex,
};

pub(crate) mod authority;
mod probe;
pub(crate) mod refresh;
pub(crate) use probe::startup;

#[derive(Clone, Copy)]
enum Owners {
    // A runner already executing as root may have root-owned checkouts.
    // Only delegation through the helper needs an owner restriction.
    DirectRoot,
    Helper { runner: u32, job: u32 },
}

impl Owners {
    fn permits(self, uid: u32) -> bool {
        match self {
            Self::DirectRoot => true,
            Self::Helper { runner, job } => uid != 0 && (uid == runner || uid == job),
        }
    }

    fn helper() -> io::Result<Self> {
        let uid = std::env::var("SUDO_UID")
            .map_err(io::Error::other)?
            .parse::<u32>()
            .map_err(io::Error::other)?;
        if uid == 0 {
            return Err(io::Error::other(
                "source seal helper requires a non-root SUDO_UID",
            ));
        }
        let mut entry: libc::passwd = unsafe { std::mem::zeroed() };
        let mut found = std::ptr::null_mut();
        let mut buffer = vec![0u8; 64 * 1024];
        let result = unsafe {
            libc::getpwuid_r(
                uid,
                &mut entry,
                buffer.as_mut_ptr().cast(),
                buffer.len(),
                &mut found,
            )
        };
        if result != 0 || found.is_null() || entry.pw_uid != uid {
            return Err(io::Error::other(
                "source seal helper SUDO_UID is not a real non-root account",
            ));
        }
        Ok(Self::Helper {
            runner: uid,
            job: crate::isolation::job_identity()?.uid,
        })
    }
}

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
    preserve_seed_times: bool,
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
            preserve_seed_times: false,
        })
    }

    /// Build-base seeding calls this at launch only after this setup seeded and
    /// time-set the checkout, never for build folders retained from earlier jobs.
    #[allow(dead_code)]
    pub(crate) fn preserve_seed_times(mut self) -> Self {
        if let Self::Snapshot(snapshot) = &mut self {
            snapshot.preserve_seed_times = true;
        }
        self
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
                step(
                    &root,
                    "rematerialize sources linked by preparation",
                    refresh::planned_sources(&plan, &tree, &mirror, snapshot.preserve_seed_times),
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
pub(crate) struct Runtime(Mutex<BTreeMap<Uuid, Plan>>);

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
    #[serde(default)]
    filesystem: Option<Filesystem>,
}

#[derive(Serialize, Deserialize)]
struct Handle {
    #[serde(with = "path_bytes")]
    path: PathBuf,
    kind: i32,
    bytes: Vec<u8>,
    #[serde(default)]
    prior_flags: libc::c_int,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
struct Filesystem {
    device: u64,
    fsid: Vec<u8>,
}

impl Filesystem {
    fn read(mount: &File) -> io::Result<Self> {
        let mut stat: libc::statfs = unsafe { std::mem::zeroed() };
        if unsafe { libc::fstatfs(mount.as_raw_fd(), &mut stat) } != 0 {
            return Err(io::Error::last_os_error());
        }
        let fsid = unsafe {
            std::slice::from_raw_parts(
                (&stat.f_fsid as *const libc::fsid_t).cast::<u8>(),
                std::mem::size_of_val(&stat.f_fsid),
            )
        }
        .to_vec();
        Ok(Self {
            device: mount.metadata()?.dev(),
            fsid,
        })
    }
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

struct JobDirectory {
    directory: File,
    path: PathBuf,
}

impl JobDirectory {
    fn open(state: &Path, id: Uuid) -> io::Result<Self> {
        let root = crate::permissions::physical_root(state)?;
        let relative = CString::new(format!("jobs/{id}"))?;
        let mut how: libc::open_how = unsafe { std::mem::zeroed() };
        how.flags = (libc::O_RDONLY
            | libc::O_DIRECTORY
            | libc::O_NOFOLLOW
            | libc::O_CLOEXEC
            | libc::O_NONBLOCK) as u64;
        how.resolve = 0x08 | 0x04; // RESOLVE_BENEATH | RESOLVE_NO_SYMLINKS
        let fd = unsafe {
            libc::syscall(
                libc::SYS_openat2,
                root.as_raw_fd(),
                relative.as_ptr(),
                &how,
                std::mem::size_of_val(&how),
            )
        };
        if fd < 0 {
            return Err(io::Error::last_os_error());
        }
        Ok(Self {
            directory: unsafe { File::from_raw_fd(fd as i32) },
            path: state.join("jobs").join(id.to_string()),
        })
    }

    fn file(&self, name: &CStr, flags: i32) -> io::Result<File> {
        let fd = unsafe {
            libc::openat(
                self.directory.as_raw_fd(),
                name.as_ptr(),
                flags | libc::O_NOFOLLOW | libc::O_CLOEXEC | libc::O_NONBLOCK,
                0o600 as libc::mode_t,
            )
        };
        if fd < 0 {
            return Err(io::Error::last_os_error());
        }
        let file = unsafe { File::from_raw_fd(fd) };
        if !file.metadata()?.is_file() {
            return Err(io::Error::other("source seal state is not a regular file"));
        }
        Ok(file)
    }

    fn read(&self, name: &CStr) -> io::Result<Vec<u8>> {
        let mut bytes = Vec::new();
        self.file(name, libc::O_RDONLY)?.read_to_end(&mut bytes)?;
        Ok(bytes)
    }

    fn exists(&self, name: &CStr) -> io::Result<bool> {
        let mut metadata: libc::stat = unsafe { std::mem::zeroed() };
        if unsafe {
            libc::fstatat(
                self.directory.as_raw_fd(),
                name.as_ptr(),
                &mut metadata,
                libc::AT_SYMLINK_NOFOLLOW,
            )
        } == 0
        {
            return Ok(true);
        }
        let error = io::Error::last_os_error();
        if error.kind() == io::ErrorKind::NotFound {
            Ok(false)
        } else {
            Err(error)
        }
    }

    fn unlink(&self, name: &CStr) -> io::Result<()> {
        if unsafe { libc::unlinkat(self.directory.as_raw_fd(), name.as_ptr(), 0) } == 0 {
            return Ok(());
        }
        let error = io::Error::last_os_error();
        if error.kind() == io::ErrorKind::NotFound {
            Ok(())
        } else {
            Err(error)
        }
    }

    fn publish(&self, file: &File) -> io::Result<()> {
        // Link the created inode, not the runner-owned staging name: replacing
        // seal.new while the helper writes must not publish a different file.
        if unsafe {
            libc::linkat(
                file.as_raw_fd(),
                c"".as_ptr(),
                self.directory.as_raw_fd(),
                c"seal".as_ptr(),
                libc::AT_EMPTY_PATH,
            )
        } != 0
        {
            return Err(io::Error::last_os_error());
        }
        self.directory.sync_all()
    }

    fn lock(&self) -> io::Result<File> {
        let file = self.file(c"seal-lock", libc::O_RDWR | libc::O_CREAT)?;
        fs2::FileExt::lock_exclusive(&file)?;
        Ok(file)
    }
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

    pub(crate) fn caller_paths_excluding(
        &self,
        excluded: &std::collections::BTreeSet<Uuid>,
    ) -> Vec<String> {
        self.0
            .lock()
            .unwrap()
            .iter()
            .filter(|(id, _)| !excluded.contains(id))
            .map(|(_, plan)| plan.caller_workspace.to_string_lossy().into_owned())
            .collect()
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

    fn claim(&self, id: Uuid, plan: Plan) -> io::Result<()> {
        let mut pending = self.0.lock().unwrap();
        match pending.entry(id) {
            std::collections::btree_map::Entry::Vacant(entry) => {
                entry.insert(plan);
                Ok(())
            }
            std::collections::btree_map::Entry::Occupied(_) => {
                Err(io::Error::other("source seal already pending for job"))
            }
        }
    }

    pub(crate) fn seal(&self, state: &Path, id: Uuid, plan: Plan) -> io::Result<()> {
        self.claim(id, plan.clone())?;
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
    fs::write("/proc/self/oom_score_adj", "900")?;
    let authority = authority::Authority::load()?;
    let owners = Owners::helper()?;
    authority.roots(state)?;
    match action {
        "seal" => {
            let mut bytes = Vec::new();
            io::stdin().take(64 * 1024 * 1024).read_to_end(&mut bytes)?;
            let plan: Plan = serde_json::from_slice(&bytes)?;
            authority.workspace(state, &plan.workspace)?;
            seal_owned(state, id, plan, owners)
        }
        "unseal" => unseal_checked(state, id, Some(&authority), owners),
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

#[cfg(test)]
fn open_source(root: &File, path: &Path) -> io::Result<File> {
    reopen_regular(&pin_source(root, path)?)
}

fn pin_source(root: &File, path: &Path) -> io::Result<File> {
    validate_relative(path)?;
    let name = CString::new(path.as_os_str().as_bytes())?;
    let mut how: libc::open_how = unsafe { std::mem::zeroed() };
    how.flags = (libc::O_PATH | libc::O_NOFOLLOW | libc::O_CLOEXEC) as u64;
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

fn reopen_regular(pin: &File) -> io::Result<File> {
    use std::os::unix::fs::OpenOptionsExt;
    let before = pin.metadata()?;
    if !before.is_file() {
        return Err(io::Error::other("not a regular file"));
    }
    // Follow only the kernel's proc fd link, after type checking the pinned
    // inode. O_NOFOLLOW here would reject that magic link with ELOOP.
    let file = fs::OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NONBLOCK | libc::O_CLOEXEC)
        .open(format!("/proc/self/fd/{}", pin.as_raw_fd()))?;
    let after = file.metadata()?;
    if (before.dev(), before.ino()) != (after.dev(), after.ino()) || !after.is_file() {
        return Err(io::Error::other(
            "source inode changed during descriptor reopen",
        ));
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
        prior_flags: 0,
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
            libc::O_PATH | libc::O_CLOEXEC,
        )
    };
    if fd < 0 {
        Err(io::Error::last_os_error())
    } else {
        let file = unsafe { File::from_raw_fd(fd as i32) };
        if !file.metadata()?.is_file() {
            return Err(io::Error::other("inode handle is not a regular file"));
        }
        reopen_regular(&file)
    }
}

const IMMUTABLE: libc::c_int = 0x10;
fn file_flags(file: &File) -> io::Result<libc::c_int> {
    let mut flags: libc::c_int = 0;
    if unsafe { libc::ioctl(file.as_raw_fd(), libc::FS_IOC_GETFLAGS, &mut flags) } != 0 {
        return Err(io::Error::other(format!(
            "FS_IOC_GETFLAGS: {}",
            io::Error::last_os_error()
        )));
    }
    Ok(flags)
}

fn immutable(file: &File, enabled: bool) -> io::Result<()> {
    if enabled {
        let metadata = file.metadata()?;
        if metadata.nlink() != 1 {
            return Err(io::Error::other(
                "refusing immutable flag on hard-linked inode",
            ));
        }
    }
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
    seal_owned(state, id, plan, Owners::DirectRoot)
}

fn seal_owned(state: &Path, id: Uuid, plan: Plan, owners: Owners) -> io::Result<()> {
    let directory = JobDirectory::open(state, id)?;
    let _operation = directory.lock()?;
    let intent: Plan = serde_json::from_slice(&directory.read(c"seal-intent")?)?;
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
        let pin = step(path, "openat2/fstat", pin_source(&mount, path))?;
        let metadata = pin.metadata()?;
        if !owners.permits(metadata.uid()) || metadata.nlink() != 1 {
            return Err(io::Error::other(format!(
                "source seal {}: requires authorized owner and exactly one hard link",
                path.display()
            )));
        }
        let file = step(path, "reopen regular inode", reopen_regular(&pin))?;
        let flags = file_flags(&file)?;
        if flags & IMMUTABLE != 0 {
            tracing::warn!(job_id = %id, path = %path.display(), "source inode already immutable; leaving it untouched");
            continue;
        }
        let mut entry = step(path, "name_to_handle_at", handle(&file, path.clone()))?;
        entry.prior_flags = flags;
        handles.push(entry);
    }
    let record = Record {
        plan,
        handles,
        filesystem: Some(Filesystem::read(&mount)?),
    };
    let path = directory.path.join("seal");
    if directory.exists(c"seal")? {
        return Err(io::Error::other(format!(
            "source seal record already exists: {}",
            path.display()
        )));
    }
    // Publish a complete synced record atomically, without replacing any old
    // recovery obligation. A crash while writing the staging file set no flags.
    let staging = path.with_extension("new");
    let mut durable = step(
        &staging,
        "create record",
        directory.file(c"seal.new", libc::O_WRONLY | libc::O_CREAT | libc::O_EXCL),
    )?;
    let saved = step(
        &path,
        "sync record",
        (|| {
            if unsafe { libc::fchmod(durable.as_raw_fd(), 0o644) } != 0 {
                return Err(io::Error::last_os_error());
            }
            durable.write_all(&serde_json::to_vec(&record)?)?;
            durable.sync_all()?;
            directory.publish(&durable)?;
            directory.unlink(c"seal.new")
        })(),
    );
    if let Err(error) = saved {
        if directory.exists(c"seal")? {
            if let Err(cleanup) = unseal_locked(&directory, id, None, owners) {
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
        .and_then(|file| {
            if !owners.permits(file.metadata()?.uid()) {
                return Err(io::Error::other(
                    "source inode owner changed before sealing",
                ));
            }
            immutable(&file, true)
        });
        if let Err(error) = step(&entry.path, "set FS_IMMUTABLE_FL", set) {
            if let Err(cleanup) = unseal_locked(&directory, id, None, owners) {
                return Err(io::Error::other(format!("{error}; rollback: {cleanup}")));
            }
            return Err(error);
        }
    }
    tracing::info!(job_id = %id, files = record.handles.len(), elapsed_us = started.elapsed().as_micros(), "source sealed");
    Ok(())
}

fn unseal(state: &Path, id: Uuid) -> io::Result<()> {
    unseal_checked(state, id, None, Owners::DirectRoot)
}

fn unseal_checked(
    state: &Path,
    id: Uuid,
    authority: Option<&authority::Authority>,
    owners: Owners,
) -> io::Result<()> {
    let directory = JobDirectory::open(state, id)?;
    let _operation = directory.lock()?;
    unseal_locked(&directory, id, authority, owners)
}

fn remove_intent(directory: &JobDirectory) -> io::Result<()> {
    // Recovery owns the per-job lock and has cleared the immutable bit on
    // every inode in the seal record, if a record existed.
    // Unlink a crash's stale staging name without opening its target.
    directory.unlink(c"seal.new")?;
    directory.unlink(c"seal-intent")?;
    directory.directory.sync_all()
}

fn unseal_locked(
    directory: &JobDirectory,
    id: Uuid,
    authority: Option<&authority::Authority>,
    owners: Owners,
) -> io::Result<()> {
    let started = std::time::Instant::now();
    let mut record_file = match directory.file(c"seal", libc::O_RDONLY) {
        Ok(file) => file,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return remove_intent(directory),
        Err(error) => return Err(error),
    };
    let metadata = record_file.metadata()?;
    if !trusted_record(metadata.uid(), metadata.mode()) {
        return Err(io::Error::other(
            "source seal record must be root-owned and not writable by the runner",
        ));
    }
    let mut bytes = Vec::new();
    record_file.read_to_end(&mut bytes)?;
    let record: Record = serde_json::from_slice(&bytes)?;
    if let Some(authority) = authority {
        let state = directory
            .path
            .parent()
            .and_then(Path::parent)
            .ok_or_else(|| io::Error::other("invalid seal state"))?;
        authority.workspace(state, &record.plan.workspace)?;
    }
    let mount = step(
        &record.plan.workspace,
        "pin unseal mount",
        open_mount(&record.plan.workspace),
    )?;
    if record.filesystem.as_ref() != Some(&Filesystem::read(&mount)?) {
        return Err(io::Error::other(
            "source seal filesystem identity mismatch; retaining record",
        ));
    }
    clear_handles(&record.handles, |handle| {
        match open_handle(&mount, handle) {
            Ok(file) if owners.permits(file.metadata()?.uid()) => {
                clear_inode(&file, handle, File::sync_all)
            }
            Ok(_) => Err(io::Error::other(
                "source inode owner is not authorized for helper unseal",
            )),
            Err(error) => Err(error),
        }
    })?;
    directory.unlink(c"seal")?;
    remove_intent(directory)?;
    tracing::info!(job_id = %id, files = record.handles.len(), elapsed_us = started.elapsed().as_micros(), "source unsealed");
    Ok(())
}

fn trusted_record(uid: u32, mode: u32) -> bool {
    uid == 0 && mode & 0o022 == 0
}

fn clear_inode(
    file: &File,
    handle: &Handle,
    sync: impl FnOnce(&File) -> io::Result<()>,
) -> io::Result<()> {
    if handle.prior_flags & IMMUTABLE != 0 {
        return Err(io::Error::other(
            "seal record claims an already immutable inode",
        ));
    }
    immutable(file, false)?;
    sync(file)
}

fn clear_handles(
    handles: &[Handle],
    mut clear: impl FnMut(&Handle) -> io::Result<()>,
) -> io::Result<()> {
    let mut failed = None;
    for handle in handles {
        if let Err(error) = step(
            &handle.path,
            "open_by_handle_at/clear FS_IMMUTABLE_FL",
            clear(handle),
        ) {
            failed.get_or_insert(error);
        }
    }
    if let Some(error) = failed {
        return Err(error);
    }
    Ok(())
}

#[cfg(test)]
mod tests;
