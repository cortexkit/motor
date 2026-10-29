use super::{
    tree_fs::{self, Directory, Entries},
    *,
};
use std::{
    ffi::OsStr,
    os::{
        fd::AsRawFd,
        unix::{
            fs::{OpenOptionsExt, PermissionsExt},
            process::CommandExt,
        },
    },
    sync::atomic::{AtomicBool, Ordering},
};

static COPY_FALLBACK_LOGGED: AtomicBool = AtomicBool::new(false);

struct Frame {
    source: Directory,
    entries: Entries,
    destination: PathBuf,
}

pub(super) fn prepare(shared: &Path, private: &Path, commits: &[String]) -> io::Result<()> {
    // Follow Cargo's lock order. These shared locks exclude downloads and GC
    // only while mutable metadata is copied, never for a running job's lifetime.
    let mut locks = Vec::new();
    for name in [".package-cache-mutate", ".package-cache"] {
        let file = OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .mode(0o600)
            .custom_flags(libc::O_NOFOLLOW | libc::O_CLOEXEC)
            .open(shared.join(name))?;
        let started = Instant::now();
        loop {
            match fs2::FileExt::try_lock_shared(&file) {
                Ok(()) => break,
                Err(error)
                    if error.kind() == io::ErrorKind::WouldBlock
                        && started.elapsed() < Duration::from_secs(30) =>
                {
                    std::thread::sleep(Duration::from_millis(25))
                }
                Err(error) => {
                    return Err(io::Error::other(format!(
                        "cargo_seed_cache_lock_refused: {error}"
                    )))
                }
            }
        }
        locks.push(file);
    }
    fs::create_dir_all(private)?;
    match Directory::open(&shared.join("registry/index")) {
        Ok(source) => clone_tree(source, &private.join("index"))?,
        Err(error) if error.kind() == io::ErrorKind::NotFound => {
            fs::create_dir(private.join("index"))?
        }
        Err(error) => return Err(error),
    }
    fs::create_dir(private.join("src"))?;
    fs::create_dir(private.join("cache"))?;
    for name in ["git/db", "git/checkouts"] {
        fs::create_dir_all(private.join(name))?;
    }
    if commits.is_empty() {
        return Ok(());
    }
    let databases = match Directory::open(&shared.join("git/db")) {
        Ok(directory) => directory,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(()),
        Err(error) => return Err(error),
    };
    let mut entries = databases.entries()?;
    let mut found = std::collections::BTreeSet::new();
    while let Some(entry) = entries.next(&databases)? {
        if entry.stat.st_mode & libc::S_IFMT != libc::S_IFDIR {
            continue;
        }
        let database = databases.child(&entry.name)?;
        let mut wanted = Vec::new();
        for commit in commits {
            if git_revision(&database, true, commit)?.as_deref() == Some(commit) {
                wanted.push(commit.clone());
                found.insert(commit.clone());
            }
        }
        if wanted.is_empty() {
            continue;
        }
        let private_db = private.join("git/db").join(&entry.name);
        clone_tree(database, &private_db)?;
        let copied_db = Directory::open(&private_db)?;
        for commit in &wanted {
            if git_revision(&copied_db, true, commit)?.as_deref() != Some(commit) {
                return Err(io::Error::other(
                    "cargo_seed_source_changed: copied Git database lacks locked commit",
                ));
            }
        }
        let checkouts = match Directory::open(&shared.join("git/checkouts").join(&entry.name)) {
            Ok(directory) => directory,
            Err(error) if error.kind() == io::ErrorKind::NotFound => continue,
            Err(error) => return Err(error),
        };
        let destination = private.join("git/checkouts").join(&entry.name);
        fs::create_dir(&destination)?;
        let mut revisions = checkouts.entries()?;
        while let Some(revision) = revisions.next(&checkouts)? {
            if revision.stat.st_mode & libc::S_IFMT != libc::S_IFDIR {
                continue;
            }
            let checkout = checkouts.child(&revision.name)?;
            if checkout.stat_at(OsStr::new(".cargo-ok")).is_err() {
                continue;
            }
            if git_revision(&checkout, false, "HEAD")?.is_some_and(|head| wanted.contains(&head)) {
                // Include the complete ready checkout, including initialized
                // submodules. Cargo can otherwise need the network even offline.
                clone_tree(checkout, &destination.join(revision.name))?;
            }
        }
    }
    for commit in commits.iter().filter(|commit| !found.contains(*commit)) {
        tracing::warn!(
            commit,
            "locked Git commit absent from shared cache; no private seed available"
        );
    }
    Ok(())
}

fn git_revision(directory: &Directory, bare: bool, revision: &str) -> io::Result<Option<String>> {
    let fd = directory.fd();
    let descriptor = format!("/proc/self/fd/{fd}");
    let mut command = Command::new("/usr/bin/git");
    command
        .env_clear()
        .env("PATH", "/usr/bin:/bin")
        .env("HOME", "/nonexistent")
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_OPTIONAL_LOCKS", "0");
    if bare {
        command.arg(format!("--git-dir={descriptor}"));
    } else {
        command.args(["-C", &descriptor]);
    }
    command.args(["rev-parse", "--verify", &format!("{revision}^{{commit}}")]);
    // Only this pinned directory is inherited, and only by the read-only query.
    unsafe {
        command.pre_exec(move || {
            if libc::fcntl(fd, libc::F_SETFD, 0) < 0 {
                return Err(io::Error::last_os_error());
            }
            Ok(())
        });
    }
    let output = command.output()?;
    if !output.status.success() {
        return Ok(None);
    }
    Ok(Some(
        String::from_utf8_lossy(&output.stdout)
            .trim()
            .to_ascii_lowercase(),
    ))
}

fn clone_tree(source: Directory, destination: &Path) -> io::Result<()> {
    if source.stat()?.st_uid != unsafe { libc::geteuid() } {
        return Err(io::Error::other(
            "cargo_seed_foreign_owner: shared cache root changed owner",
        ));
    }
    fs::create_dir(destination)?;
    let mut stack = vec![Frame {
        entries: source.entries()?,
        source,
        destination: destination.to_path_buf(),
    }];
    while let Some(frame) = stack.last_mut() {
        let Some(entry) = frame.entries.next(&frame.source)? else {
            stack.pop();
            continue;
        };
        if entry.stat.st_uid != unsafe { libc::geteuid() } {
            return Err(io::Error::other(
                "cargo_seed_foreign_owner: shared cache owner changed",
            ));
        }
        let target = frame.destination.join(&entry.name);
        match entry.stat.st_mode & libc::S_IFMT {
            libc::S_IFDIR => {
                let source = frame.source.child(&entry.name)?;
                let opened = source.stat()?;
                if opened.st_uid != entry.stat.st_uid || !tree_fs::same_inode(&opened, &entry.stat)
                {
                    return Err(io::Error::other("cargo_seed_source_changed"));
                }
                fs::create_dir(&target)?;
                stack.push(Frame {
                    entries: source.entries()?,
                    source,
                    destination: target,
                });
            }
            libc::S_IFREG => {
                let mut source = frame.source.file(&entry.name)?;
                let mut stat = std::mem::MaybeUninit::uninit();
                if unsafe { libc::fstat(source.as_raw_fd(), stat.as_mut_ptr()) } != 0 {
                    return Err(io::Error::last_os_error());
                }
                let opened = unsafe { stat.assume_init() };
                if opened.st_uid != entry.stat.st_uid || !tree_fs::same_inode(&opened, &entry.stat)
                {
                    return Err(io::Error::other("cargo_seed_source_changed"));
                }
                let mut output = OpenOptions::new()
                    .write(true)
                    .create_new(true)
                    .mode(0o600)
                    .open(&target)?;
                if unsafe { libc::ioctl(output.as_raw_fd(), 0x40049409, source.as_raw_fd()) } != 0 {
                    let error = io::Error::last_os_error();
                    if !matches!(
                        error.raw_os_error(),
                        Some(libc::EOPNOTSUPP | libc::ENOTTY | libc::EXDEV | libc::EINVAL)
                    ) {
                        return Err(error);
                    }
                    if !COPY_FALLBACK_LOGGED.swap(true, Ordering::Relaxed) {
                        tracing::warn!(%error, "FICLONE unsupported for Cargo seed; using a private plain copy");
                    }
                    output.set_len(0)?;
                    io::copy(&mut source, &mut output)?;
                }
                output.set_permissions(fs::Permissions::from_mode(entry.stat.st_mode & 0o777))?;
            }
            libc::S_IFLNK => {
                std::os::unix::fs::symlink(frame.source.read_link(&entry.name)?, &target)?
            }
            _ => return Err(io::Error::other("cargo_seed_unsafe_entry")),
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    fn git(path: &Path, args: &[&str]) -> String {
        let output = Command::new("/usr/bin/git")
            .args([
                "-c",
                "user.name=cache-test",
                "-c",
                "user.email=cache@example.invalid",
                "-C",
            ])
            .arg(path)
            .args(args)
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
        String::from_utf8(output.stdout).unwrap().trim().to_owned()
    }
    #[test]
    fn private_locked_git_seed_is_relocatable_and_excludes_unrelated_data() {
        let scratch = cortexkit_test_support::ScratchDir::new("private-locked-git-seed");
        let repo = scratch.path().join("repo");
        fs::create_dir_all(repo.join("src")).unwrap();
        fs::write(
            repo.join("Cargo.toml"),
            "[package]\nname='seed-dependency'\nversion='0.0.1'\nedition='2021'\n",
        )
        .unwrap();
        fs::write(repo.join("src/lib.rs"), "pub fn value() {}\n").unwrap();
        git(&repo, &["init", "-q"]);
        git(&repo, &["add", "."]);
        git(&repo, &["commit", "-qm", "seed"]);
        let commit = git(&repo, &["rev-parse", "HEAD"]);
        let client = scratch.path().join("client");
        fs::create_dir_all(client.join("src")).unwrap();
        fs::write(client.join("src/lib.rs"), "").unwrap();
        fs::write(client.join("Cargo.toml"), format!("[package]\nname='seed-client'\nversion='0.0.1'\nedition='2021'\n[dependencies]\nseed-dependency={{git='{}',rev='{commit}'}}\n", url::Url::from_file_path(&repo).unwrap())).unwrap();
        let shared = scratch.path().join("shared");
        fs::create_dir(&shared).unwrap();
        let fetch = |home: &Path, offline: bool| {
            let mut command = Command::new("cargo");
            command
                .args(["fetch", "--manifest-path"])
                .arg(client.join("Cargo.toml"))
                .current_dir(scratch.path())
                .env("CARGO_HOME", home)
                .env("CARGO_CACHE_AUTO_CLEAN_FREQUENCY", "never")
                .env("RUSTC_WRAPPER", "")
                .env("RUSTC_WORKSPACE_WRAPPER", "")
                .env("CARGO_NET_OFFLINE", if offline { "true" } else { "false" });
            if offline {
                command.args(["--locked", "--offline"]);
            }
            let output = command.output().unwrap();
            assert!(
                output.status.success(),
                "{}",
                String::from_utf8_lossy(&output.stderr)
            );
        };
        fetch(&shared, false);
        fs::create_dir_all(shared.join("git/db/unrelated")).unwrap();
        fs::write(shared.join("git/db/unrelated/must-not-copy"), "unrelated").unwrap();
        let private = scratch.path().join("private");
        prepare(&shared, &private, &[commit]).unwrap();
        assert!(!private.join("git/db/unrelated").exists());
        assert!(fs::read_dir(private.join("src")).unwrap().next().is_none());
        fs::create_dir(private.join("registry")).unwrap();
        for name in ["index", "src", "cache"] {
            fs::rename(private.join(name), private.join("registry").join(name)).unwrap();
        }
        fs::rename(&repo, scratch.path().join("hidden-repo")).unwrap();
        fs::rename(shared.join("git"), shared.join("hidden-git")).unwrap();
        fetch(&private, true);
    }
}
