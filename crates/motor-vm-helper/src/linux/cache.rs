//! Root-only account credentials. Neither store paths nor key bytes are part of
//! the host/helper protocol; the helper stages a fresh drive in one VM's jail.
use crate::{job::VmKind, Error, Result};
use motor_protocol::{
    RemoteCacheRequest, SecretJson, REMOTE_CACHE_DRIVE_MAGIC, REMOTE_CACHE_DRIVE_MAX_BYTES,
    REMOTE_CACHE_DRIVE_NONCE_BYTES,
};
use std::{
    ffi::{CString, OsString},
    fs::File,
    io::{Read, Write},
    os::{
        fd::{AsRawFd, FromRawFd},
        unix::fs::MetadataExt,
    },
    path::{Component, Path},
};
use zeroize::Zeroizing;

mod store;

#[cfg(feature = "linux-root-tests")]
pub(super) fn install_fixture(path: &Path, account: &str, set: &RemoteCacheRequest) -> Result<()> {
    store::install(path, account, set)
}
pub(super) const STORE: &str = "/var/lib/motor-vm/remote-cache";
pub(super) const IMAGE: &str = "remote-cache.img";
const FILE_LIMIT: u64 = 65536;
const NAMES: [&str; 3] = ["config.json", "read-key", "write-key"];

pub(super) fn install_tenant(
    store: &Path,
    registry: &crate::tenant::Registry,
    connect: impl FnOnce() -> Result<std::os::unix::net::UnixStream>,
) -> Result<()> {
    crate::tenant::validate_account_binding(&registry.account)?;
    let set = store::load(store, &registry.account)?
        .ok_or_else(|| Error("tenant_cache_set_missing".into()))?;
    // The guest is untrusted: neither a rejected reply nor a parse error may
    // quote credentials back into the host's reply or the helper's log.
    let exchange = || -> Result<()> {
        let mut stream = connect()?;
        let timeout =
            motor_protocol::TENANT_ATTACH_READ_DEADLINE + std::time::Duration::from_secs(120);
        let deadline = std::time::Instant::now() + timeout;
        stream.set_read_timeout(Some(timeout))?;
        stream.set_write_timeout(Some(timeout))?;
        let bytes = Zeroizing::new(serde_json::to_vec(&set).map_err(|_| invalid())?);
        stream.write_all(&(bytes.len() as u32).to_be_bytes())?;
        stream.write_all(&bytes)?;
        let mut header = [0; 4];
        super::tenant::read_deadline(&mut stream, &mut header, deadline)?;
        let len = u32::from_be_bytes(header) as usize;
        if len == 0 || len > REMOTE_CACHE_DRIVE_MAX_BYTES {
            return Err(invalid());
        }
        let mut bytes = Zeroizing::new(vec![0; len]);
        super::tenant::read_deadline(&mut stream, &mut bytes, deadline)?;
        let reply = SecretJson::new(serde_json::from_slice(&bytes).map_err(|_| invalid())?);
        if reply.value()["ok"] != true {
            return Err(invalid());
        }
        Ok(())
    };
    exchange().map_err(|_| Error("tenant_cache_install_failed".into()))
}

fn invalid() -> Error {
    Error("invalid remote-cache credentials".into())
}

pub fn disable_core_dumps() -> Result<()> {
    let limit = libc::rlimit {
        rlim_cur: 0,
        rlim_max: 0,
    };
    if unsafe { libc::setrlimit(libc::RLIMIT_CORE, &limit) } != 0 {
        return Err(std::io::Error::last_os_error().into());
    }
    Ok(())
}

fn root_only() -> Result<()> {
    if unsafe { libc::geteuid() } != 0 {
        return Err(Error("remote-cache administration requires root".into()));
    }
    Ok(())
}

fn cstr(name: &std::ffi::OsStr) -> Result<CString> {
    CString::new(name.as_encoded_bytes()).map_err(|_| invalid())
}

fn open_at(dir: &File, name: &std::ffi::OsStr, flags: i32) -> std::io::Result<File> {
    let name = CString::new(name.as_encoded_bytes())?;
    let fd = unsafe {
        libc::openat(
            dir.as_raw_fd(),
            name.as_ptr(),
            flags | libc::O_NOFOLLOW | libc::O_CLOEXEC | libc::O_NONBLOCK,
            0o600,
        )
    };
    if fd < 0 {
        return Err(std::io::Error::last_os_error());
    }
    Ok(unsafe { File::from_raw_fd(fd) })
}

fn trusted(file: &File, directory: bool, private: bool) -> Result<()> {
    let meta = file.metadata()?;
    if meta.uid() != 0
        || meta.mode() & 0o022 != 0
        || (directory && !meta.is_dir())
        || (!directory && !meta.is_file())
        || (private && meta.mode() & 0o777 != if directory { 0o700 } else { 0o600 })
    {
        return Err(Error(
            "unsafe remote-cache owner, type or permissions".into(),
        ));
    }
    Ok(())
}

// Walk from a pinned root descriptor, never following a symlink at any level.
fn directory(path: &Path) -> Result<File> {
    if !path.is_absolute() {
        return Err(Error("remote-cache paths must be absolute".into()));
    }
    let mut dir = File::open("/")?;
    trusted(&dir, true, false)?;
    for component in path.components() {
        match component {
            Component::RootDir => (),
            Component::Normal(name) => {
                dir = open_at(&dir, name, libc::O_RDONLY | libc::O_DIRECTORY)?;
                trusted(&dir, true, false)?;
            }
            _ => return Err(invalid()),
        }
    }
    Ok(dir)
}

fn read_file(file: File, key: bool, store_file: bool) -> Result<Zeroizing<String>> {
    trusted(&file, false, store_file)?;
    if key && file.metadata()?.mode() & 0o077 != 0 {
        return Err(invalid());
    }
    let mut text = Zeroizing::new(String::new());
    file.take(FILE_LIMIT + 1)
        .read_to_string(&mut text)
        .map_err(|_| invalid())?;
    if text.is_empty() || text.len() as u64 > FILE_LIMIT {
        return Err(invalid());
    }
    Ok(text)
}

fn source(path: &Path, key: bool) -> Result<Zeroizing<String>> {
    let parent = directory(path.parent().ok_or_else(invalid)?)?;
    let file = open_at(
        &parent,
        path.file_name().ok_or_else(invalid)?,
        libc::O_RDONLY,
    )?;
    read_file(file, key, false)
}

fn validate(account: &str, set: &RemoteCacheRequest) -> Result<()> {
    crate::job::validate_account(account)?;
    let mut parsed = Vec::new();
    for (index, text) in [&set.config, &set.read_key]
        .into_iter()
        .chain(set.write_key.iter())
        .enumerate()
    {
        if text.is_empty() || text.len() as u64 > FILE_LIMIT {
            return Err(invalid());
        }
        let value = SecretJson::new(serde_json::from_str(text).map_err(|_| invalid())?);
        let object = value.value().as_object().ok_or_else(invalid)?;
        let expected: &[&str] = if index == 0 {
            &[
                "generation",
                "tenant",
                "endpoint",
                "region",
                "bucket",
                "prefix",
                "storage_class",
                "enabled",
            ]
        } else {
            &["generation", "access_key_id", "secret_access_key"]
        };
        if object.len() != expected.len()
            || expected.iter().any(|key| !object.contains_key(*key))
            || !value.value()["generation"]
                .as_str()
                .is_some_and(|v| !v.is_empty())
        {
            return Err(invalid());
        }
        parsed.push(value);
    }
    let config = parsed[0].value();
    if config["tenant"].as_str() != Some(account)
        || config["prefix"].as_str() != Some(&format!("tenant/{}/", account.replace(':', "-")))
        || parsed
            .iter()
            .any(|value| value.value()["generation"] != config["generation"])
    {
        return Err(invalid());
    }
    Ok(())
}

fn random_nonce() -> Result<[u8; REMOTE_CACHE_DRIVE_NONCE_BYTES]> {
    let mut nonce = [0; REMOTE_CACHE_DRIVE_NONCE_BYTES];
    File::open("/dev/urandom")?.read_exact(&mut nonce)?;
    Ok(nonce)
}

fn hex(nonce: &[u8]) -> String {
    nonce.iter().map(|byte| format!("{byte:02x}")).collect()
}

pub fn install_command(args: Vec<OsString>) -> Result<()> {
    install_command_at(args, Path::new(STORE))
}

pub(super) fn install_command_at(args: Vec<OsString>, store: &Path) -> Result<()> {
    root_only()?;
    let mut account = None;
    let mut config = None;
    let mut read = None;
    let mut write = None;
    let mut args = args.into_iter();
    while let Some(flag) = args.next() {
        let slot = match flag.to_str() {
            Some("--account") => &mut account,
            Some("--config") => &mut config,
            Some("--read-key") => &mut read,
            Some("--write-key") => &mut write,
            _ => return Err(Error("unknown remote-cache option".into())),
        };
        if slot.is_some() {
            return Err(Error("duplicate remote-cache option".into()));
        }
        *slot = Some(args.next().ok_or_else(invalid)?);
    }
    let account = account
        .and_then(|value| value.into_string().ok())
        .ok_or_else(invalid)?;
    crate::job::validate_account(&account)?;
    let set = RemoteCacheRequest {
        op: "set_remote_cache".into(),
        config: source(Path::new(&config.ok_or_else(invalid)?), false)?,
        read_key: source(Path::new(&read.ok_or_else(invalid)?), true)?,
        write_key: write
            .map(|path| source(Path::new(&path), true))
            .transpose()?,
    };
    store::install(store, &account, &set)
}

#[cfg(all(test, feature = "linux-root-tests"))]
pub(super) fn stage(
    store: &Path,
    account: &str,
    kind: VmKind,
    jail: &File,
    uid: u32,
) -> Result<Option<String>> {
    let role = if kind == VmKind::Fill {
        crate::job::WorkspaceRole::Fill
    } else {
        crate::job::WorkspaceRole::Job
    };
    stage_with_write(store, account, kind, role, jail, uid, false).map(|(nonce, _)| nonce)
}

#[allow(clippy::too_many_arguments)]
pub(super) fn stage_with_write(
    store: &Path,
    account: &str,
    kind: VmKind,
    role: crate::job::WorkspaceRole,
    jail: &File,
    uid: u32,
    job_write: bool,
) -> Result<(Option<String>, bool)> {
    if kind == VmKind::Recovery {
        return Ok((None, false));
    }
    let Some(set) = store::load(store, account)? else {
        return Ok((None, false));
    };
    let (set, job_write) = credentials_for_boot(set, kind, role, job_write);
    let nonce = random_nonce()?;
    let payload = Zeroizing::new(serde_json::to_vec(&set).map_err(|_| invalid())?);
    let length = REMOTE_CACHE_DRIVE_MAGIC.len() + nonce.len() + 4 + payload.len();
    if length > REMOTE_CACHE_DRIVE_MAX_BYTES {
        return Err(invalid());
    }
    let mut file = open_at(
        jail,
        std::ffi::OsStr::new(IMAGE),
        libc::O_WRONLY | libc::O_CREAT | libc::O_EXCL,
    )?;
    file.write_all(REMOTE_CACHE_DRIVE_MAGIC)?;
    file.write_all(&nonce)?;
    file.write_all(&(payload.len() as u32).to_le_bytes())?;
    file.write_all(&payload)?;
    file.set_len(length.div_ceil(512) as u64 * 512)?;
    file.sync_all()?;
    if unsafe { libc::fchown(file.as_raw_fd(), uid, uid) } != 0 {
        return Err(std::io::Error::last_os_error().into());
    }
    jail.sync_all()?;
    Ok((Some(hex(&nonce)), job_write))
}

fn credentials_for_boot(
    mut set: RemoteCacheRequest,
    kind: VmKind,
    role: crate::job::WorkspaceRole,
    job_write: bool,
) -> (RemoteCacheRequest, bool) {
    let granted_job = kind == VmKind::Job && role == crate::job::WorkspaceRole::Job && job_write;
    let fill = kind == VmKind::Fill && role == crate::job::WorkspaceRole::Fill;
    if !granted_job && !fill {
        set.write_key = None;
    }
    let job_write = granted_job && set.write_key.is_some();
    (set, job_write)
}

#[cfg(test)]
pub(crate) mod tests;
