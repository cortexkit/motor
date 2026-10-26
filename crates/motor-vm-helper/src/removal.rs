//! Removing a departed customer's tenant VM data.
//!
//! Removal is two steps, so neither a mistaken nor a compromised caller can
//! destroy a disk at once. `begin` and `finish` move the tenant's data into a
//! root-only quarantine folder and drop its records; `purge` deletes
//! quarantine folders once they are `QUARANTINE_S` old. Every path is derived
//! here from the tenant name: nothing a caller sends names a file.
//!
//! `script/tenant-provision/provision.py remove-tenant` removes the same set,
//! and `script/tenant-provision/removal-layout.json` pins that both agree.
use crate::{Error, Result};
use std::{
    fs::{self, File, OpenOptions},
    io::Write,
    os::{
        fd::{AsRawFd, FromRawFd},
        unix::{
            ffi::OsStrExt,
            fs::{OpenOptionsExt, PermissionsExt},
        },
    },
    path::{Component, Path, PathBuf},
};

/// How long a removed tenant's data waits in quarantine before it is deleted.
/// The public privacy page promises that a departed customer's tenant disk is
/// deleted within 30 days of the account closing. Seven days of quarantine
/// (time to undo a mistaken removal) plus the host's wait for the tenant's
/// last job to drain (about a day) stays well inside that promise.
pub const QUARANTINE_S: u64 = 7 * 24 * 60 * 60;
const COMPLETE: &str = "complete";

/// Where a tenant's data and records live. Production uses `live()`; tests
/// point every path into a scratch directory.
#[derive(Debug, Clone)]
pub struct Paths {
    /// `/var/lib/motor-vm`: tenant homes, the quarantine and the provision lock.
    pub root: PathBuf,
    /// The helper registry directory, one `<tenant>.json` per tenant.
    pub registry: PathBuf,
    /// motor-host's binding map, `{vm: {tenant, sub}}`.
    pub bindings: PathBuf,
    /// The operator's provisioning records, one `<tenant>.json` per tenant.
    pub provision: PathBuf,
}

impl Paths {
    pub fn live() -> Self {
        Self {
            root: PathBuf::from("/var/lib/motor-vm"),
            registry: PathBuf::from(crate::tenant::REGISTRY),
            bindings: PathBuf::from("/etc/motor-host/tenants.json"),
            provision: PathBuf::from("/etc/motor-host/provision"),
        }
    }
    pub fn home(&self, tenant: &str) -> PathBuf {
        self.root.join("tenants").join(tenant)
    }
    pub fn removed(&self) -> PathBuf {
        self.root.join("removed")
    }
    fn registry_file(&self, tenant: &str) -> PathBuf {
        self.registry.join(format!("{tenant}.json"))
    }
    fn provision_file(&self, tenant: &str) -> PathBuf {
        self.provision.join(format!("{tenant}.json"))
    }
}

/// What a removal touches, as `removal-layout.json` spells it with
/// `{tenant}`, `{unix_s}` and `{tap}` placeholders. Tests compare this with
/// the file, and the provisioning script's copy with the same file.
pub fn layout(paths: &Paths) -> serde_json::Value {
    let tenant = "{tenant}";
    let mut records = serde_json::Map::new();
    records.insert(
        paths.registry_file(tenant).display().to_string(),
        "records/registry.json".into(),
    );
    records.insert(
        paths.provision_file(tenant).display().to_string(),
        "records/provision.json".into(),
    );
    serde_json::json!({
        "lock": paths.root.join("provision.lock"),
        "quarantine": paths.removed().join("{tenant}-{unix_s}"),
        "moved": {paths.home(tenant).display().to_string(): "home"},
        "records": records,
        "binding": {"file": paths.bindings, "saved_as": "records/binding.json"},
        "network": [
            "tap mtap{tap}",
            "nft chain tenant_mtap{tap}_input",
            "nft chain tenant_mtap{tap}_forward",
            "nft chain tenant_mtap{tap}_nat",
            "nft counter tenant_mtap{tap}_egress",
            "ufw route motor-tenant-mtap{tap}",
        ],
        "purge_age_s": QUARANTINE_S,
    })
}

/// Holds the provisioning lock that `provision.py` takes, so a removal never
/// rewrites the binding map while provisioning writes it. A busy lock is a
/// retryable refusal: provisioning can take minutes, and the helper must not
/// wait for it while serving other tenants.
pub fn lock(paths: &Paths) -> Result<File> {
    let file = OpenOptions::new()
        .append(true)
        .create(true)
        .mode(0o600)
        .custom_flags(libc::O_NOFOLLOW)
        .open(paths.root.join("provision.lock"))?;
    // SAFETY: flock on an owned descriptor has no memory preconditions.
    if unsafe {
        libc::flock(
            std::os::fd::AsRawFd::as_raw_fd(&file),
            libc::LOCK_EX | libc::LOCK_NB,
        )
    } != 0
    {
        return Err(Error(
            "provisioning is in progress; retry the tenant removal later".into(),
        ));
    }
    Ok(file)
}

/// Creates the quarantine folder and saves copies of the tenant's records in
/// it, so an operator can undo the removal. Nothing is deleted yet.
pub fn begin(paths: &Paths, tenant: &str, unix_s: u64) -> Result<PathBuf> {
    crate::tenant::validate_name(tenant)?;
    let _home_parent = directory_no_symlinks(&paths.root.join("tenants"))?;
    let removed = paths.removed();
    match fs::symlink_metadata(&removed) {
        Ok(meta) if meta.is_dir() && !meta.file_type().is_symlink() => {}
        Ok(_) => return Err(Error("removed-tenant folder is not a directory".into())),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            fs::create_dir(&removed)?;
            fs::set_permissions(&removed, fs::Permissions::from_mode(0o700))?;
            sync_dir(&paths.root)?;
        }
        Err(e) => return Err(e.into()),
    }
    let quarantine = removed.join(format!("{tenant}-{unix_s}"));
    fs::create_dir(&quarantine)?;
    fs::set_permissions(&quarantine, fs::Permissions::from_mode(0o700))?;
    let records = quarantine.join("records");
    fs::create_dir(&records)?;
    let name = std::ffi::CString::new(tenant).map_err(|e| Error(e.to_string()))?;
    durable_write(
        &records.join("home-identity.json"),
        &serde_json::to_vec(&home_identity(&_home_parent, &name)?)
            .map_err(|e| Error(e.to_string()))?,
        0o600,
    )?;
    for (source, saved) in [
        (paths.registry_file(tenant), "registry.json"),
        (paths.provision_file(tenant), "provision.json"),
    ] {
        match fs::read(&source) {
            Ok(bytes) => durable_write(&records.join(saved), &bytes, 0o600)?,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
            Err(e) => return Err(e.into()),
        }
    }
    let binding: serde_json::Map<_, _> = read_bindings(paths)?
        .into_iter()
        .filter(|(_, entry)| entry["tenant"] == tenant)
        .collect();
    if !binding.is_empty() {
        let bytes = serde_json::to_vec_pretty(&binding).map_err(|e| Error(e.to_string()))?;
        durable_write(&records.join("binding.json"), &bytes, 0o600)?;
    }
    sync_dir(&records)?;
    sync_dir(&removed)?;
    Ok(quarantine)
}

/// Drops the tenant's binding, provisioning record and registry entry, then
/// moves its home (disk, generation, suspend snapshots, set-aside stale
/// snapshots and stop markers) into the quarantine folder with one rename.
///
/// The registry entry goes before the home moves: a helper restarted in
/// between must not find a registry entry whose disk is gone, which would
/// fail its startup reconciliation. If the move itself is interrupted,
/// `pending` finds the quarantine folder and a retry completes it.
pub fn finish(paths: &Paths, tenant: &str, quarantine: &Path) -> Result<()> {
    crate::tenant::validate_name(tenant)?;
    // Pin both parents before deleting records. renameat never resolves an
    // intermediate symlink, even if a spelling is changed after these opens.
    let home_parent = directory_no_symlinks(&paths.root.join("tenants"))?;
    let destination = directory_no_symlinks(quarantine)?;
    let name = std::ffi::CString::new(tenant).map_err(|e| Error(e.to_string()))?;
    let identity = home_identity(&home_parent, &name)?;
    check_home_identity(quarantine, &identity)?;
    let present = !identity.is_null();
    if !present {
        // A successful removal with no home must not revive when the same
        // tenant name is provisioned again later.
        durable_write(&quarantine.join(COMPLETE), b"1\n", 0o600)?;
    }
    let bindings = read_bindings(paths)?;
    if bindings.values().any(|entry| entry["tenant"] == tenant) {
        let kept: serde_json::Map<_, _> = bindings
            .into_iter()
            .filter(|(_, entry)| entry["tenant"] != tenant)
            .collect();
        let mut bytes = serde_json::to_vec_pretty(&kept).map_err(|e| Error(e.to_string()))?;
        bytes.push(b'\n');
        // motor-host reads the binding map, so it stays world-readable.
        durable_write(&paths.bindings, &bytes, 0o644)?;
    }
    for record in [paths.provision_file(tenant), paths.registry_file(tenant)] {
        match fs::remove_file(&record) {
            Ok(()) => sync_dir(record.parent().unwrap())?,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
            Err(e) => return Err(e.into()),
        }
    }
    if present {
        if unsafe {
            libc::renameat(
                home_parent.as_raw_fd(),
                name.as_ptr(),
                destination.as_raw_fd(),
                c"home".as_ptr(),
            )
        } != 0
        {
            return Err(std::io::Error::last_os_error().into());
        }
        home_parent.sync_all()?;
        destination.sync_all()?;
    }
    durable_write(&quarantine.join(COMPLETE), b"1\n", 0o600)?;
    Ok(())
}

fn home_identity(parent: &File, name: &std::ffi::CStr) -> Result<serde_json::Value> {
    let mut metadata = std::mem::MaybeUninit::<libc::stat>::uninit();
    if unsafe {
        libc::fstatat(
            parent.as_raw_fd(),
            name.as_ptr(),
            metadata.as_mut_ptr(),
            libc::AT_SYMLINK_NOFOLLOW,
        )
    } != 0
    {
        let error = std::io::Error::last_os_error();
        return if error.kind() == std::io::ErrorKind::NotFound {
            Ok(serde_json::Value::Null)
        } else {
            Err(error.into())
        };
    }
    let metadata = unsafe { metadata.assume_init() };
    if metadata.st_mode & libc::S_IFMT != libc::S_IFDIR {
        return Err(Error("tenant home is not a directory".into()));
    }
    Ok(serde_json::json!({"dev":metadata.st_dev,"ino":metadata.st_ino}))
}

fn check_home_identity(quarantine: &Path, current: &serde_json::Value) -> Result<()> {
    if current.is_null() {
        return Ok(());
    }
    let saved: serde_json::Value =
        serde_json::from_slice(&fs::read(quarantine.join("records/home-identity.json"))?)
            .map_err(|e| Error(e.to_string()))?;
    if &saved != current {
        return Err(Error(
            "tenant home no longer matches removal journal; manual recovery required".into(),
        ));
    }
    Ok(())
}

fn directory_no_symlinks(path: &Path) -> Result<File> {
    if !path.is_absolute() {
        return Err(Error("removal directory must be absolute".into()));
    }
    let mut directory = OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_DIRECTORY | libc::O_NOFOLLOW | libc::O_CLOEXEC)
        .open("/")?;
    for component in path.components() {
        let name = match component {
            Component::RootDir | Component::CurDir => continue,
            Component::Normal(name) => {
                std::ffi::CString::new(name.as_bytes()).map_err(|e| Error(e.to_string()))?
            }
            _ => return Err(Error("invalid removal directory component".into())),
        };
        let fd = unsafe {
            libc::openat(
                directory.as_raw_fd(),
                name.as_ptr(),
                libc::O_RDONLY | libc::O_DIRECTORY | libc::O_NOFOLLOW | libc::O_CLOEXEC,
            )
        };
        if fd < 0 {
            return Err(std::io::Error::last_os_error().into());
        }
        directory = unsafe { File::from_raw_fd(fd) };
    }
    Ok(directory)
}

/// Replay network cleanup from the saved registry before finishing either a
/// normal removal or an interrupted one. Records never outrun network cleanup.
pub fn finish_with_network(
    paths: &Paths,
    tenant: &str,
    quarantine: &Path,
    cleanup: impl FnOnce(&crate::tenant::Registry) -> Result<()>,
) -> Result<()> {
    let registry: crate::tenant::Registry =
        serde_json::from_slice(&fs::read(quarantine.join("records/registry.json"))?)
            .map_err(|e| Error(e.to_string()))?;
    registry.validate(tenant)?;
    cleanup(&registry)?;
    finish(paths, tenant, quarantine)
}

/// Resume through the online removal path, checking current index ownership
/// under the provisioning lock before any saved record can authorize cleanup.
pub fn resume_with_network(
    paths: &Paths,
    tenant: &str,
    mut read_registry: impl FnMut(&Path) -> Result<crate::tenant::Registry>,
    cleanup: impl FnOnce(&crate::tenant::Registry) -> Result<()>,
) -> Result<Option<PathBuf>> {
    let _lock = lock(paths)?;
    let Some(quarantine) = pending(paths, tenant)? else {
        return Ok(None);
    };
    let saved = read_registry(&quarantine.join("records/registry.json"))?;
    saved.validate(tenant)?;
    let parent = directory_no_symlinks(&paths.root.join("tenants"))?;
    let name = std::ffi::CString::new(tenant).map_err(|e| Error(e.to_string()))?;
    check_home_identity(&quarantine, &home_identity(&parent, &name)?)?;
    if let Some(owner) = reassigned_tap_owner(paths, tenant, &saved, false, &mut read_registry)? {
        eprintln!("tenant {tenant}: network cleanup refused because TAP index belongs to {owner}; completing data only");
    } else {
        cleanup(&saved)?;
    }
    finish(paths, tenant, &quarantine)?;
    Ok(Some(quarantine))
}

/// An index identifies host network objects and the uid, not an incarnation.
/// Saved records must never authorize deletion after another tenant reused it.
pub fn refuse_reassigned_tap(
    paths: &Paths,
    tenant: &str,
    saved: &crate::tenant::Registry,
    allow_current: bool,
    mut read_registry: impl FnMut(&Path) -> Result<crate::tenant::Registry>,
) -> Result<()> {
    if let Some(name) =
        reassigned_tap_owner(paths, tenant, saved, allow_current, &mut read_registry)?
    {
        return Err(Error(format!(
            "cannot replay removal: tenant TAP index has been reassigned to {name}"
        )));
    }
    Ok(())
}

fn reassigned_tap_owner(
    paths: &Paths,
    tenant: &str,
    saved: &crate::tenant::Registry,
    allow_current: bool,
    mut read_registry: impl FnMut(&Path) -> Result<crate::tenant::Registry>,
) -> Result<Option<String>> {
    let entries = match fs::read_dir(&paths.registry) {
        Ok(entries) => entries,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(error.into()),
    };
    let mut owner = None;
    for entry in entries {
        let entry = entry?;
        let path = entry.path();
        if path.extension() != Some(std::ffi::OsStr::new("json")) {
            continue;
        }
        let name = path
            .file_stem()
            .and_then(|name| name.to_str())
            .ok_or_else(|| Error("invalid registry filename".into()))?;
        crate::tenant::validate_name(name)?;
        if allow_current && name == tenant {
            continue;
        }
        let current = read_registry(&path)?;
        current.validate(name)?;
        if current.tap_index == saved.tap_index {
            if name == tenant {
                return Err(Error("tenant registry reappeared during removal".into()));
            }
            owner = Some(name.to_owned());
        }
    }
    Ok(owner)
}

/// A removal whose records are already gone but whose home was not yet moved:
/// the newest quarantine folder for this tenant that has saved records and
/// no `home`, while the tenant's home still exists.
pub fn pending(paths: &Paths, tenant: &str) -> Result<Option<PathBuf>> {
    crate::tenant::validate_name(tenant)?;
    if paths.registry_file(tenant).exists() || !paths.home(tenant).exists() {
        return Ok(None);
    }
    let mut newest: Option<(u64, PathBuf)> = None;
    for (path, name, unix_s) in quarantined(paths)? {
        if name == tenant
            && !path.join(COMPLETE).exists()
            && path.join("records").is_dir()
            && !path.join("home").exists()
            && newest.as_ref().is_none_or(|(at, _)| unix_s > *at)
        {
            newest = Some((unix_s, path));
        }
    }
    Ok(newest.map(|(_, path)| path))
}

/// Deletes quarantine folders at least `QUARANTINE_S` old, judged by the
/// removal time in their name, and returns them. A folder whose move never
/// completed (`pending`) is kept and reported instead: deleting its saved
/// records would leave the tenant's data unregistered and unremovable.
pub fn purge(paths: &Paths, now_s: u64) -> Result<Vec<PathBuf>> {
    let mut purged = Vec::new();
    for (path, tenant, unix_s) in quarantined(paths)? {
        if now_s.saturating_sub(unix_s) < QUARANTINE_S {
            continue;
        }
        if !path.join(COMPLETE).exists()
            && !path.join("home").exists()
            && paths.home(&tenant).exists()
        {
            eprintln!(
                "tenant {tenant}: removal at {unix_s} never moved its home; data is still at {}",
                paths.home(&tenant).display()
            );
            continue;
        }
        fs::remove_dir_all(&path)?;
        eprintln!(
            "tenant {tenant}: purged quarantined data removed at {unix_s}: {}",
            path.display()
        );
        purged.push(path);
    }
    if !purged.is_empty() {
        sync_dir(&paths.removed())?;
    }
    Ok(purged)
}

/// Quarantine folders: real directories named `<tenant>-<unix seconds>`.
fn quarantined(paths: &Paths) -> Result<Vec<(PathBuf, String, u64)>> {
    let entries = match fs::read_dir(paths.removed()) {
        Ok(entries) => entries,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(e) => return Err(e.into()),
    };
    let mut found = Vec::new();
    for entry in entries {
        let entry = entry?;
        let meta = fs::symlink_metadata(entry.path())?;
        if !meta.is_dir() || meta.file_type().is_symlink() {
            continue;
        }
        let name = entry.file_name().to_string_lossy().into_owned();
        let Some((tenant, unix_s)) = name.rsplit_once('-') else {
            continue;
        };
        let (Ok(unix_s), Ok(())) = (unix_s.parse(), crate::tenant::validate_name(tenant)) else {
            continue;
        };
        found.push((entry.path(), tenant.to_owned(), unix_s));
    }
    Ok(found)
}

fn read_bindings(paths: &Paths) -> Result<serde_json::Map<String, serde_json::Value>> {
    match fs::read(&paths.bindings) {
        Ok(bytes) => serde_json::from_slice(&bytes).map_err(|e| Error(format!("binding map: {e}"))),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(Default::default()),
        Err(e) => Err(e.into()),
    }
}

fn sync_dir(path: &Path) -> Result<()> {
    File::open(path)?.sync_all()?;
    Ok(())
}

fn durable_write(path: &Path, bytes: &[u8], mode: u32) -> Result<()> {
    let tmp = path.with_extension("removal-pending");
    let mut file = OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(true)
        .mode(mode)
        .custom_flags(libc::O_NOFOLLOW)
        .open(&tmp)?;
    file.set_permissions(fs::Permissions::from_mode(mode))?;
    file.write_all(bytes)?;
    file.sync_all()?;
    fs::rename(&tmp, path)?;
    sync_dir(path.parent().unwrap())
}

#[cfg(test)]
mod tests;
