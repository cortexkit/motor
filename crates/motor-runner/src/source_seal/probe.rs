//! Fail deployment before accepting jobs if a workspace filesystem cannot seal.
use super::*;

pub(crate) fn startup(config: &Config) -> io::Result<()> {
    if !config.isolate_jobs || config.vm_instance() {
        return Ok(());
    }
    if unsafe { libc::geteuid() } != 0 {
        authority::Authority::load()?.runner(config)?;
    }
    let mut seen = Vec::new();
    for root in authority::roots(config) {
        let result = (|| {
            fs::create_dir_all(&root)?;
            let mount = open_mount(&root)?;
            let identity = Filesystem::read(&mount)?;
            if seen.contains(&identity) {
                return Ok(());
            }
            probe(config, &root, &mount)?;
            seen.push(identity);
            Ok(())
        })();
        step(&root, "source read-only startup probe", result)?;
    }
    Ok(())
}

fn probe(config: &Config, root: &Path, mount: &File) -> io::Result<()> {
    let id = Uuid::now_v7();
    let name = CString::new(format!(".motor-source-probe-{id}"))?;
    if unsafe { libc::mkdirat(mount.as_raw_fd(), name.as_ptr(), 0o700) } != 0 {
        return Err(io::Error::last_os_error());
    }
    let workspace = root.join(name.to_str().map_err(io::Error::other)?);
    let fd = open_mount(&workspace)?;
    let file_fd = unsafe {
        libc::openat(
            fd.as_raw_fd(),
            c"source".as_ptr(),
            libc::O_CREAT | libc::O_EXCL | libc::O_RDWR | libc::O_NOFOLLOW | libc::O_CLOEXEC,
            0o600 as libc::mode_t,
        )
    };
    if file_fd < 0 {
        return Err(io::Error::last_os_error());
    }
    let mut file = unsafe { File::from_raw_fd(file_fd) };
    file.write_all(b"source seal startup probe\n")?;
    if unsafe { libc::geteuid() } == 0 {
        let user = crate::isolation::job_identity()?;
        if unsafe { libc::fchown(file.as_raw_fd(), user.uid, user.gid) } != 0 {
            return Err(io::Error::last_os_error());
        }
    }
    let directory = config.state_dir().join("jobs").join(id.to_string());
    fs::create_dir_all(&directory)?;
    // A crash during the probe must leave an ordinary recoverable job record,
    // not an unreadable job that startup would quarantine before unsealing.
    let mut job = crate::storage::Job::new(id, None, 0, 0, 0);
    job.terminal(motor_protocol::Outcome::OutcomeUnknown, 0);
    crate::storage::atomic_job_json(&directory.join("record.json"), &job)?;
    let plan = Plan {
        unit: id,
        workspace: workspace.clone(),
        caller_workspace: workspace.clone(),
        files: vec!["source".into()],
    };
    config.source_seals.seal(&config.state_dir(), id, plan)?;
    if file_flags(&file)? & IMMUTABLE == 0 {
        return Err(io::Error::other("probe inode did not become immutable"));
    }
    config.source_seals.unseal(&config.state_dir(), id)?;
    if file_flags(&file)? & IMMUTABLE != 0 {
        return Err(io::Error::other("probe inode remained immutable"));
    }
    fs::remove_dir_all(&workspace)?;
    fs::remove_dir_all(directory)?;
    Ok(())
}
