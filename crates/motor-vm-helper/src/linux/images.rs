//! Offline publication from root-owned staging files verified against pinned
//! manifests. These operations accept image identities, never workspace disks.
use super::fs;
use crate::{
    images::{Artifact, Manifest, Operation, STAGING},
    job, Error, Reply, Response, Result,
};
use std::{
    fs::{File, OpenOptions, Permissions},
    io::{Read, Write},
    os::{
        fd::AsRawFd,
        unix::fs::{MetadataExt, OpenOptionsExt, PermissionsExt},
    },
    path::{Path, PathBuf},
    process::{Command, Stdio},
    time::{Duration, Instant},
};

fn directory(path: &Path) -> Result<()> {
    let mut current = PathBuf::from("/");
    for component in path.components() {
        match component {
            std::path::Component::RootDir => continue,
            std::path::Component::Normal(name) => current.push(name),
            _ => return Err(Error("image directory must be absolute".into())),
        }
        match std::fs::create_dir(&current) {
            Ok(()) => {}
            Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {}
            Err(e) => return Err(e.into()),
        }
        let meta = std::fs::symlink_metadata(&current)?;
        fs::trusted(&current, &meta)?;
        if !meta.is_dir() {
            return Err(Error("image directory is not physical".into()));
        }
    }
    Ok(())
}

fn lock(root: &Path, id: &str) -> Result<File> {
    directory(root)?;
    let locks = root.join("locks");
    directory(&locks)?;
    let file = OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .mode(0o600)
        .custom_flags(libc::O_NOFOLLOW | libc::O_CLOEXEC)
        .open(locks.join(id))?;
    fs::trusted(&locks.join(id), &file.metadata()?)?;
    if unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) } != 0 {
        return Err(Error(
            "image work is already in progress; retry later".into(),
        ));
    }
    Ok(file)
}

fn manifest(root: &Path) -> Result<Manifest> {
    let manifest: Manifest =
        serde_json::from_reader(fs::trusted_file(&root.join("manifest.json"))?)
            .map_err(|e| Error(format!("invalid image manifest: {e}")))?;
    manifest.validate()?;
    Ok(manifest)
}

fn pin(staging: &Path, root: &Path, id: &str) -> Result<Manifest> {
    let staged = manifest(&staging.join(id))?;
    let pins = root.join("pins");
    directory(&pins)?;
    let path = pins.join(format!("{id}.json"));
    if path.try_exists()? {
        let previous: Manifest =
            serde_json::from_reader(fs::trusted_file(&path)?).map_err(|e| Error(e.to_string()))?;
        if previous != staged {
            return Err(Error("pinned generation manifest changed".into()));
        }
    } else {
        super::tenant::durable_write(
            &path,
            &serde_json::to_vec(&staged).map_err(|e| Error(e.to_string()))?,
        )?;
        seal(&path)?;
    }
    Ok(staged)
}

pub(super) fn digest(file: &mut File) -> Result<Artifact> {
    digest_until(file, None)
}

fn prepare_deadline(deadline: Option<Instant>) -> Result<()> {
    if deadline.is_some_and(|end| Instant::now() >= end) {
        return Err(Error("account image preparation deadline exceeded".into()));
    }
    Ok(())
}

fn digest_until(file: &mut File, deadline: Option<Instant>) -> Result<Artifact> {
    use std::io::{Seek, SeekFrom};
    prepare_deadline(deadline)?;
    file.seek(SeekFrom::Start(0))?;
    let mut child = Command::new("/usr/bin/sha256sum")
        .stdin(file.try_clone()?)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()?;
    loop {
        if child.try_wait()?.is_some() {
            break;
        }
        if let Err(error) = prepare_deadline(deadline) {
            let _ = child.kill();
            let _ = child.wait();
            return Err(error);
        }
        std::thread::sleep(Duration::from_millis(10));
    }
    let output = child.wait_with_output()?;
    prepare_deadline(deadline)?;
    if !output.status.success() {
        return Err(Error("cannot hash image artifact".into()));
    }
    let sha256 = String::from_utf8_lossy(&output.stdout)
        .split_whitespace()
        .next()
        .filter(|s| s.len() == 64 && s.bytes().all(|b| b.is_ascii_hexdigit()))
        .ok_or_else(|| Error("invalid image digest".into()))?
        .to_owned();
    Ok(Artifact {
        bytes: file.metadata()?.len(),
        sha256,
    })
}

fn verify(path: &Path, expected: &Artifact) -> Result<File> {
    verify_until(path, expected, None)
}

fn verify_until(path: &Path, expected: &Artifact, deadline: Option<Instant>) -> Result<File> {
    let mut file = fs::trusted_file(path)?;
    if &digest_until(&mut file, deadline)? != expected {
        return Err(Error(format!(
            "pinned image hash mismatch: {}",
            path.display()
        )));
    }
    Ok(file)
}

// Use buffered reads and writes, not std::fs::copy, copy_file_range or FICLONE.
// Then verify full exclusive allocation: even zero-filled ranges must have
// their own blocks, so account images cannot share mapped toolchain pages.
fn physical_copy(source: &mut File, target: &Path) -> Result<()> {
    physical_copy_until(source, target, None)
}

fn physical_copy_until(source: &mut File, target: &Path, deadline: Option<Instant>) -> Result<()> {
    use std::io::{Seek, SeekFrom};
    prepare_deadline(deadline)?;
    source.seek(SeekFrom::Start(0))?;
    let mut out = OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .custom_flags(libc::O_NOFOLLOW | libc::O_CLOEXEC)
        .open(target)?;
    let mut buffer = vec![0; 1024 * 1024];
    loop {
        prepare_deadline(deadline)?;
        let n = source.read(&mut buffer)?;
        if n == 0 {
            break;
        }
        out.write_all(&buffer[..n])?;
    }
    out.sync_all()?;
    require_physical_copy(&out)?;
    prepare_deadline(deadline)?;
    Ok(())
}

fn require_physical_copy(file: &File) -> Result<()> {
    let metadata = file.metadata()?;
    if metadata.nlink() != 1 || super::job::image_copy_exclusive_bytes(file)? < metadata.len() {
        return Err(Error(
            "physical image copy has hard links, sparse or shared extents".into(),
        ));
    }
    Ok(())
}

struct Partial(PathBuf);
impl Drop for Partial {
    fn drop(&mut self) {
        if let Ok(mounts) = std::fs::read_to_string("/proc/self/mountinfo") {
            if mounts
                .lines()
                .filter_map(|l| l.split_whitespace().nth(4))
                .any(|p| Path::new(p).starts_with(&self.0))
            {
                return;
            }
        }
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

fn seal(path: &Path) -> Result<()> {
    std::fs::set_permissions(path, Permissions::from_mode(0o444))?;
    File::open(path)?.sync_all()?;
    Ok(())
}

pub fn handle(op: Operation) -> Result<Response> {
    op.validate()?;
    if let Operation::Readiness {
        job_vms_configured,
        memory_max_bytes,
        workspace_disk_max_bytes,
    } = op
    {
        if job_vms_configured {
            super::cgroup::prepare_job_slice(
                memory_max_bytes
                    .ok_or_else(|| Error("job slice memory ceiling is required".into()))?,
            )?;
        }
        return Ok(Response::new(Reply::PerJobReadiness {
            job_vms_configured,
            workspace_disk_max_bytes,
            published_generations: if job_vms_configured {
                published_generations(Path::new(job::IMAGES))?
            } else {
                Vec::new()
            },
        }));
    }
    let id = match &op {
        Operation::Inspect(id) | Operation::Publish(id) | Operation::Remove(id) => id,
        Operation::Prepare { generation, .. } => generation,
        Operation::Readiness { .. } => unreachable!(),
    };
    let root = Path::new(job::IMAGES);
    let _lock = lock(root, id)?;
    if matches!(op, Operation::Inspect(_)) {
        return Ok(Response::new(Reply::JobImages {
            manifest: pin(Path::new(STAGING), root, id)?,
        }));
    }
    match &op {
        Operation::Publish(id) => publish(Path::new(STAGING), root, id, provision)?,
        Operation::Prepare {
            generation,
            account,
        } => prepare(root, generation, account)?,
        Operation::Remove(id) => {
            for (path, suffix) in [
                (root.join(id), "deleted"),
                (root.join(format!(".{id}.partial")), "aborted"),
            ] {
                if path.try_exists()? {
                    fs::trusted(&path, &std::fs::symlink_metadata(&path)?)?;
                    let mounts = std::fs::read_to_string("/proc/self/mountinfo")?;
                    if mounts
                        .lines()
                        .filter_map(|l| l.split_whitespace().nth(4))
                        .any(|p| Path::new(p).starts_with(&path))
                    {
                        return Err(Error("publication image is still mounted".into()));
                    }
                    std::fs::rename(path, root.join(format!(".{id}.{suffix}")))?;
                }
            }
            File::open(root)?.sync_all()?;
        }
        Operation::Inspect(_) | Operation::Readiness { .. } => unreachable!(),
    }
    Ok(Response::new(Reply::Ok))
}

fn published_generations(root: &Path) -> Result<Vec<String>> {
    if !root.try_exists()? {
        return Ok(Vec::new());
    }
    fs::trusted(root, &std::fs::symlink_metadata(root)?)?;
    let mut ready = Vec::new();
    for entry in std::fs::read_dir(root)? {
        let entry = entry?;
        let Some(id) = entry.file_name().to_str().map(str::to_owned) else {
            continue;
        };
        if crate::images::validate_generation(&id).is_err() || !entry.file_type()?.is_dir() {
            continue;
        }
        let verified = (|| -> Result<()> {
            let _lock = lock(root, &id)?;
            let manifest = manifest(&entry.path())?;
            let source: Manifest =
                serde_json::from_reader(fs::trusted_file(&entry.path().join("source.json"))?)
                    .map_err(|e| Error(e.to_string()))?;
            let pinned: Manifest = serde_json::from_reader(fs::trusted_file(
                &root.join("pins").join(format!("{id}.json")),
            )?)
            .map_err(|e| Error(e.to_string()))?;
            source.validate()?;
            if source != pinned
                || source.kernel != manifest.kernel
                || source.rootfs != manifest.rootfs
                || source.hello != manifest.hello
                || source.guest_init != manifest.guest_init
            {
                return Err(Error("published generation does not match its pins".into()));
            }
            for (name, artifact) in [
                ("kernel", &manifest.kernel),
                ("rootfs.ext4", &manifest.rootfs),
                ("toolchain.ext4", &manifest.toolchain),
            ] {
                let file = verify(&entry.path().join(name), artifact)?;
                if file.metadata()?.mode() & 0o222 != 0 || file.metadata()?.mode() & 0o004 == 0 {
                    return Err(Error(
                        "published image is not sealed for jailed VMMs".into(),
                    ));
                }
            }
            Ok(())
        })();
        if verified.is_ok() {
            ready.push(id);
        }
    }
    ready.sort();
    Ok(ready)
}

pub(super) fn finish_remove(id: &str) -> Result<()> {
    crate::images::validate_generation(id)?;
    let root = Path::new(job::IMAGES);
    let _lock = lock(root, id)?;
    for suffix in ["deleted", "aborted"] {
        let path = root.join(format!(".{id}.{suffix}"));
        if path.try_exists()? {
            fs::trusted(&path, &std::fs::symlink_metadata(&path)?)?;
            std::fs::remove_dir_all(path)?;
        }
    }
    File::open(root)?.sync_all()?;
    Ok(())
}

fn publish(
    staging: &Path,
    root: &Path,
    id: &str,
    provision: fn(&Path, &Path) -> Result<()>,
) -> Result<()> {
    let source = staging.join(id);
    for suffix in ["deleted", "aborted"] {
        if root.join(format!(".{id}.{suffix}")).try_exists()? {
            return Err(Error(
                "image deletion must finish before publication".into(),
            ));
        }
    }
    let pinned = pin(staging, root, id)?;
    let dest = root.join(id);
    if dest.try_exists()? {
        let previous: Manifest =
            serde_json::from_reader(fs::trusted_file(&dest.join("source.json"))?)
                .map_err(|e| Error(e.to_string()))?;
        if previous != pinned {
            return Err(Error("generation identities cannot be reused".into()));
        }
        return Ok(());
    }
    // Verify every source before writing any destination. Provisioning changes
    // only the private toolchain copy; the source pins remain the rollout input.
    let mut kernel = verify(&source.join("kernel"), &pinned.kernel)?;
    let mut rootfs = verify(&source.join("rootfs.ext4"), &pinned.rootfs)?;
    let mut toolchain = verify(&source.join("toolchain.ext4"), &pinned.toolchain)?;
    let partial = Partial(root.join(format!(".{id}.partial")));
    if partial.0.try_exists()? {
        std::fs::remove_dir_all(&partial.0)?;
    }
    directory(&partial.0)?;
    physical_copy(&mut kernel, &partial.0.join("kernel"))?;
    physical_copy(&mut rootfs, &partial.0.join("rootfs.ext4"))?;
    physical_copy(&mut toolchain, &partial.0.join("toolchain.ext4"))?;
    verify(&partial.0.join("kernel"), &pinned.kernel)?;
    verify(&partial.0.join("rootfs.ext4"), &pinned.rootfs)?;
    verify(&partial.0.join("toolchain.ext4"), &pinned.toolchain)?;
    provision(
        &partial.0.join("rootfs.ext4"),
        &partial.0.join("toolchain.ext4"),
    )?;
    let mut sealed = pinned.clone();
    sealed.toolchain = digest(&mut fs::trusted_file(&partial.0.join("toolchain.ext4"))?)?;
    for name in ["kernel", "rootfs.ext4", "toolchain.ext4"] {
        seal(&partial.0.join(name))?;
    }
    for (name, value) in [("source.json", &pinned), ("manifest.json", &sealed)] {
        let path = partial.0.join(name);
        let mut file = OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o444)
            .custom_flags(libc::O_NOFOLLOW)
            .open(path)?;
        file.write_all(&serde_json::to_vec(value).map_err(|e| Error(e.to_string()))?)?;
        file.sync_all()?;
    }
    directory(&partial.0.join("accounts"))?;
    File::open(&partial.0)?.sync_all()?;
    std::fs::rename(&partial.0, &dest)?;
    File::open(root)?.sync_all()?;
    Ok(())
}

fn prepare(root: &Path, id: &str, account: &str) -> Result<()> {
    let started = Instant::now();
    let image = root.join(id);
    let pinned = manifest(&image)?;
    let deadline = Some(started + crate::images::account_prepare_timeout(pinned.toolchain.bytes));
    let accounts = image.join("accounts");
    directory(&accounts)?;
    let target = accounts.join(account);
    if target.try_exists()? {
        let copy = verify_until(&target.join("toolchain.ext4"), &pinned.toolchain, deadline)?;
        require_physical_copy(&copy)?;
        return Ok(());
    }
    let partial = Partial(accounts.join(format!(".{account}.partial")));
    if partial.0.try_exists()? {
        std::fs::remove_dir_all(&partial.0)?;
    }
    directory(&partial.0)?;
    let mut source = verify_until(&image.join("toolchain.ext4"), &pinned.toolchain, deadline)?;
    let path = partial.0.join("toolchain.ext4");
    physical_copy_until(&mut source, &path, deadline)?;
    verify_until(&path, &pinned.toolchain, deadline)?;
    seal(&path)?;
    File::open(&partial.0)?.sync_all()?;
    prepare_deadline(deadline)?;
    std::fs::rename(&partial.0, target)?;
    File::open(accounts)?.sync_all()?;
    Ok(())
}

struct Mount(PathBuf);
impl Mount {
    fn open(image: &Path, target: &Path, read_only: bool) -> Result<Self> {
        directory(target)?;
        let options = if read_only {
            "loop,ro,noload,nodev,nosuid,noexec"
        } else {
            "loop,rw,nodev,nosuid,noexec"
        };
        let status = Command::new("/usr/bin/mount")
            .args(["-t", "ext4", "-o", options])
            .arg(image)
            .arg(target)
            .status()?;
        if !status.success() {
            return Err(Error("cannot mount trusted publication image".into()));
        }
        Ok(Self(target.into()))
    }
    fn close(self) -> Result<()> {
        fs::unmount(&self.0)?;
        std::fs::remove_dir(&self.0)?;
        Ok(())
    }
}
impl Drop for Mount {
    fn drop(&mut self) {
        let _ = fs::unmount(&self.0);
    }
}

fn provision(rootfs: &Path, toolchain: &Path) -> Result<()> {
    let parent = rootfs.parent().unwrap();
    let root = Mount::open(rootfs, &parent.join("root-mount"), true)?;
    let mut passwd = String::new();
    fs::trusted_file(&root.0.join("etc/passwd"))?.read_to_string(&mut passwd)?;
    let uid = passwd
        .lines()
        .find_map(|line| {
            let fields: Vec<_> = line.split(':').collect();
            (fields.first() == Some(&"motor-job"))
                .then(|| fields.get(2)?.parse::<u32>().ok())
                .flatten()
        })
        .filter(|uid| *uid != 0)
        .ok_or_else(|| Error("root image lacks motor-job uid".into()))?;
    root.close()?;
    let tools = Mount::open(toolchain, &parent.join("toolchain-mount"), false)?;
    for name in ["registry", "git"] {
        provision_tree(&tools.0.join("usr/local/lib/motor/cargo").join(name), uid)?;
    }
    tools.close()
}

fn provision_tree(path: &Path, uid: u32) -> Result<()> {
    use std::{ffi::CString, os::unix::ffi::OsStrExt};
    let metadata = std::fs::symlink_metadata(path)?;
    if metadata.file_type().is_symlink() {
        return Ok(());
    }
    if metadata.uid() == uid || !(metadata.is_dir() || metadata.is_file()) {
        return Err(Error(
            "Cargo sources must not be owned by motor-job or contain special files".into(),
        ));
    }
    let directory = metadata.is_dir();
    let mode = metadata.mode();
    let read = if directory || mode & 0o111 != 0 {
        5u16
    } else {
        4u16
    };
    let encode = |entries: &[(u16, u16, u32)]| {
        let mut bytes = 2u32.to_le_bytes().to_vec();
        for (tag, perm, id) in entries {
            bytes.extend(tag.to_le_bytes());
            bytes.extend(perm.to_le_bytes());
            bytes.extend(id.to_le_bytes());
        }
        bytes
    };
    let set = |name: &str, entries: &[(u16, u16, u32)]| -> Result<()> {
        let bytes = encode(entries);
        let path = CString::new(path.as_os_str().as_bytes()).map_err(|e| Error(e.to_string()))?;
        let name = CString::new(name).unwrap();
        if unsafe {
            libc::lsetxattr(
                path.as_ptr(),
                name.as_ptr(),
                bytes.as_ptr().cast(),
                bytes.len(),
                0,
            )
        } != 0
        {
            return Err(std::io::Error::last_os_error().into());
        }
        Ok(())
    };
    let none = u32::MAX;
    set(
        "system.posix_acl_access",
        &[
            (1, ((mode >> 6) & 7) as u16, none),
            (2, read, uid),
            (4, read, none),
            (16, read, none),
            (32, read, none),
        ],
    )?;
    if directory {
        set(
            "system.posix_acl_default",
            &[
                (1, 7, none),
                (2, 5, uid),
                (4, 5, none),
                (16, 5, none),
                (32, 5, none),
            ],
        )?;
        for entry in std::fs::read_dir(path)? {
            provision_tree(&entry?.path(), uid)?;
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests;
