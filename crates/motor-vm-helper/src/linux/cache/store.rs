use super::*;

fn lock(dir: &File, operation: i32) -> Result<()> {
    if unsafe { libc::flock(dir.as_raw_fd(), operation) } != 0 {
        return Err(std::io::Error::last_os_error().into());
    }
    Ok(())
}

fn open_store(path: &Path, create: bool) -> Result<Option<File>> {
    let parent = directory(path.parent().ok_or_else(invalid)?)?;
    let name = cstr(path.file_name().ok_or_else(invalid)?)?;
    if create && unsafe { libc::mkdirat(parent.as_raw_fd(), name.as_ptr(), 0o700) } != 0 {
        let error = std::io::Error::last_os_error();
        if error.kind() != std::io::ErrorKind::AlreadyExists {
            return Err(error.into());
        }
    }
    let dir = match open_at(
        &parent,
        path.file_name().unwrap(),
        libc::O_RDONLY | libc::O_DIRECTORY,
    ) {
        Ok(dir) => dir,
        Err(error) if !create && error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(error.into()),
    };
    trusted(&dir, true, true)?;
    if create {
        parent.sync_all()?;
    }
    Ok(Some(dir))
}

pub(super) fn load(path: &Path, account: &str) -> Result<Option<RemoteCacheRequest>> {
    root_only()?;
    crate::job::validate_id(account)?;
    let Some(store) = open_store(path, false)? else {
        return Ok(None);
    };
    lock(&store, libc::LOCK_SH)?;
    let dir = match open_at(&store, account.as_ref(), libc::O_RDONLY | libc::O_DIRECTORY) {
        Ok(dir) => dir,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(error.into()),
    };
    trusted(&dir, true, true)?;
    let read = |index: usize| -> Result<Zeroizing<String>> {
        read_file(
            open_at(&dir, NAMES[index].as_ref(), libc::O_RDONLY)?,
            index != 0,
            true,
        )
    };
    let write_key = match open_at(&dir, NAMES[2].as_ref(), libc::O_RDONLY) {
        Ok(file) => Some(read_file(file, true, true)?),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => None,
        Err(error) => return Err(error.into()),
    };
    let set = RemoteCacheRequest {
        op: "set_remote_cache".into(),
        config: read(0)?,
        read_key: read(1)?,
        write_key,
    };
    validate(account, &set)?;
    Ok(Some(set))
}

pub(super) fn install(path: &Path, account: &str, set: &RemoteCacheRequest) -> Result<()> {
    root_only()?;
    validate(account, set)?;
    let store = open_store(path, true)?.unwrap();
    lock(&store, libc::LOCK_EX)?;
    let temp = format!(".install-{}", hex(&random_nonce()?));
    let name = cstr(temp.as_ref())?;
    let account_name = cstr(account.as_ref())?;
    if unsafe { libc::mkdirat(store.as_raw_fd(), name.as_ptr(), 0o700) } != 0 {
        return Err(std::io::Error::last_os_error().into());
    }
    let result: Result<()> = (|| {
        let dir = open_at(&store, temp.as_ref(), libc::O_RDONLY | libc::O_DIRECTORY)?;
        trusted(&dir, true, true)?;
        for (filename, text) in NAMES.iter().zip([
            Some(&set.config),
            Some(&set.read_key),
            set.write_key.as_ref(),
        ]) {
            if let Some(text) = text {
                let mut file = open_at(
                    &dir,
                    filename.as_ref(),
                    libc::O_WRONLY | libc::O_CREAT | libc::O_EXCL,
                )?;
                file.write_all(text.as_bytes())?;
                file.sync_all()?;
            }
        }
        dir.sync_all()?;
        // The lock serializes readers and installers; rename-exchange also
        // keeps one complete set at the account name across a process crash.
        let exists = match open_at(&store, account.as_ref(), libc::O_RDONLY | libc::O_DIRECTORY) {
            Ok(old) => {
                trusted(&old, true, true)?;
                true
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => false,
            Err(error) => return Err(error.into()),
        };
        let flags = if exists {
            libc::RENAME_EXCHANGE
        } else {
            libc::RENAME_NOREPLACE
        };
        if unsafe {
            libc::renameat2(
                store.as_raw_fd(),
                name.as_ptr(),
                store.as_raw_fd(),
                account_name.as_ptr(),
                flags,
            )
        } != 0
        {
            return Err(std::io::Error::last_os_error().into());
        }
        store.sync_all()?;
        Ok(())
    })();
    let cleanup = remove_set(&store, &temp);
    result?;
    cleanup?;
    store.sync_all()?;
    Ok(())
}

fn remove_set(store: &File, name: &str) -> Result<()> {
    let dir = match open_at(store, name.as_ref(), libc::O_RDONLY | libc::O_DIRECTORY) {
        Ok(dir) => dir,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(error) => return Err(error.into()),
    };
    trusted(&dir, true, true)?;
    for filename in NAMES {
        let filename = cstr(filename.as_ref())?;
        if unsafe { libc::unlinkat(dir.as_raw_fd(), filename.as_ptr(), 0) } != 0 {
            let error = std::io::Error::last_os_error();
            if error.kind() != std::io::ErrorKind::NotFound {
                return Err(error.into());
            }
        }
    }
    let name = cstr(name.as_ref())?;
    if unsafe { libc::unlinkat(store.as_raw_fd(), name.as_ptr(), libc::AT_REMOVEDIR) } != 0 {
        return Err(std::io::Error::last_os_error().into());
    }
    Ok(())
}
