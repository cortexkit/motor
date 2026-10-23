//! Local installation of returned files, bounded by the submitting checkout.
use cortexkit_exec_remote_types as shared;
use sha2::{Digest, Sha256};
use std::{
    ffi::CString,
    fs::File,
    io::{self, Write},
    os::{
        fd::{AsRawFd, FromRawFd},
        unix::ffi::OsStrExt,
    },
    path::{Component, Path},
};

pub(super) struct Returns {
    root: Option<std::path::PathBuf>,
    requested: Vec<String>,
    from_run: bool,
    workspace_from_job: bool,
    cap: u64,
    assembler: Option<motor_protocol::ReturnAssembler>,
    chunks: Vec<Vec<u8>>,
    result: Option<Vec<Result<Vec<u8>, String>>>,
    invalid: Option<String>,
}

impl Returns {
    pub(super) fn new(
        root: Option<std::path::PathBuf>,
        requested: Option<Vec<String>>,
        cap: u64,
    ) -> Self {
        Self {
            root,
            from_run: requested.is_some(),
            workspace_from_job: false,
            requested: requested.unwrap_or_default(),
            cap,
            assembler: None,
            chunks: Vec::new(),
            result: None,
            invalid: None,
        }
    }

    pub(super) fn requested(&mut self, paths: Vec<String>) {
        if !self.from_run && self.requested.is_empty() && paths.len() <= 64 {
            self.requested = paths;
        }
    }

    pub(super) fn job_workspace(&mut self, workspace: &str) {
        if self.from_run || self.workspace_from_job {
            return;
        }
        self.workspace_from_job = true;
        self.root = super::workspace_root(Some(Path::new(workspace)))
            .ok()
            .filter(|root| root == Path::new(workspace));
    }

    pub(super) fn restart(&mut self) {
        // Reattachment replays from sequence zero. Discard incomplete assembly,
        // and install only after the terminal so a replay never writes twice.
        self.assembler = None;
        self.chunks.clear();
        self.result = None;
        self.invalid = None;
    }

    pub(super) fn chunk(&mut self, chunk: &shared::ReturnChunk) {
        if self.invalid.is_some() {
            return;
        }
        let assembler = self.assembler.get_or_insert_with(|| {
            self.chunks = vec![Vec::new(); self.requested.len()];
            motor_protocol::ReturnAssembler::new(&self.requested, self.cap)
        });
        match assembler.accept_chunk(chunk) {
            Ok(bytes) => self.chunks[chunk.path_index as usize].extend_from_slice(bytes),
            Err(error) => self.invalid = Some(format!("read_error ({error})")),
        }
    }

    pub(super) fn manifest(&mut self, manifest: &shared::ReturnManifest) {
        self.requested(
            manifest
                .files
                .iter()
                .map(|entry| entry.path().to_owned())
                .collect(),
        );
        if self.invalid.is_some() {
            return;
        }
        let assembler = self
            .assembler
            .take()
            .unwrap_or_else(|| motor_protocol::ReturnAssembler::new(&self.requested, self.cap));
        match assembler.finish(manifest) {
            Ok(verified) => {
                self.result = Some(
                    verified
                        .into_iter()
                        .map(|entry| match entry.outcome() {
                            shared::ReturnManifestOutcome::Returned { .. } => Ok(self
                                .chunks
                                .get_mut(entry.path_index() as usize)
                                .map(std::mem::take)
                                .unwrap_or_default()),
                            shared::ReturnManifestOutcome::Failed { cause, .. } => {
                                Err(String::from(cause.clone()))
                            }
                            _ => Err("read_error (unknown manifest outcome)".into()),
                        })
                        .collect(),
                );
            }
            Err(error) => self.invalid = Some(format!("read_error ({error})")),
        }
    }

    pub(super) fn legacy(&mut self, result: &shared::ReturnResult) {
        if let Some(files) = result.returned_files() {
            self.requested(files.iter().map(|file| file.path().to_owned()).collect());
            let total: u64 = files.iter().map(|file| file.data().len() as u64).sum();
            if total > self.cap.min(shared::RETURN_MAX_BYTES_CAP) {
                self.invalid = Some("over_cap".into());
                return;
            }
            self.result = Some(
                self.requested
                    .iter()
                    .map(|path| {
                        let file = files
                            .iter()
                            .find(|file| file.path() == path)
                            .ok_or_else(|| "missing".to_string())?;
                        if format!("{:x}", Sha256::digest(file.data())) != file.sha256() {
                            return Err("read_error (digest mismatch)".into());
                        }
                        Ok(file.data().to_vec())
                    })
                    .collect(),
            );
        } else if let Some(failure) = result.return_paths_failed() {
            self.invalid = Some(String::from(failure.cause().clone()));
        }
    }

    pub(super) fn finish(&mut self, err: &mut dyn Write) {
        let results = self.result.take().unwrap_or_else(|| {
            self.requested
                .iter()
                .map(|_| Err("read_error (no return result received)".into()))
                .collect()
        });
        for (path, result) in self.requested.iter().zip(results) {
            let result = match &self.invalid {
                Some(cause) => Err(cause.clone()),
                None => result,
            }
            .and_then(|data| {
                let root = self
                    .root
                    .as_deref()
                    .ok_or_else(|| "escape (local workspace unavailable)".to_string())?;
                install(root, Path::new(path), &data)?;
                Ok(data.len())
            });
            match result {
                Ok(size) => {
                    let _ = writeln!(err, "ck motor run: returned {path} ({size} bytes)");
                }
                Err(cause) => {
                    let hint = if cause == "missing" {
                        " (the job didn't create it, or it is outside the workspace)"
                    } else {
                        ""
                    };
                    let _ = writeln!(err, "ck motor run: not returned {path}: {cause}{hint}");
                }
            }
        }
    }
}

/// Reject traversal rather than normalising it: the same spelling must remain
/// safe when directories are created or replaced while the job runs.
pub(super) fn relative_path<'a>(root: &Path, path: &'a Path) -> Result<&'a Path, &'static str> {
    let relative = path.strip_prefix(root).map_err(|_| "escape")?;
    if !path.is_absolute()
        || relative.as_os_str().is_empty()
        || path.components().any(|c| matches!(c, Component::ParentDir))
    {
        return Err("escape");
    }
    Ok(relative)
}

fn name(path: &std::ffi::OsStr) -> io::Result<CString> {
    CString::new(path.as_bytes()).map_err(io::Error::other)
}

fn open_directory(parent: &File, name: &CString) -> io::Result<File> {
    // Each step is relative to an already-open directory; no path lookup may
    // follow a symlink, including a concurrently substituted parent.
    let fd = unsafe {
        libc::openat(
            parent.as_raw_fd(),
            name.as_ptr(),
            libc::O_RDONLY | libc::O_DIRECTORY | libc::O_NOFOLLOW | libc::O_CLOEXEC,
        )
    };
    if fd < 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(unsafe { File::from_raw_fd(fd) })
}

pub(super) fn install(root: &Path, path: &Path, data: &[u8]) -> Result<(), String> {
    let relative = relative_path(root, path)?;
    let install = || -> io::Result<()> {
        let mut directory = File::open("/")?;
        for component in root.components() {
            if let Component::Normal(component) = component {
                directory = open_directory(&directory, &name(component)?)?;
            }
        }
        for component in relative.parent().unwrap().components() {
            let Component::Normal(component) = component else {
                continue;
            };
            let component = name(component)?;
            match open_directory(&directory, &component) {
                Ok(child) => directory = child,
                Err(error) if error.kind() == io::ErrorKind::NotFound => {
                    let result =
                        unsafe { libc::mkdirat(directory.as_raw_fd(), component.as_ptr(), 0o755) };
                    if result < 0
                        && io::Error::last_os_error().kind() != io::ErrorKind::AlreadyExists
                    {
                        return Err(io::Error::last_os_error());
                    }
                    directory = open_directory(&directory, &component)?;
                }
                Err(error) => return Err(error),
            }
        }
        let destination = name(relative.file_name().unwrap())?;
        let mut stat = std::mem::MaybeUninit::<libc::stat>::uninit();
        let result = unsafe {
            libc::fstatat(
                directory.as_raw_fd(),
                destination.as_ptr(),
                stat.as_mut_ptr(),
                libc::AT_SYMLINK_NOFOLLOW,
            )
        };
        // Keep an existing file's permission bits; a new file gets the usual
        // 0666 minus the umask, as if the user had written it themselves.
        let mut existing_mode = None;
        if result == 0 {
            let stat = unsafe { stat.assume_init() };
            if stat.st_mode & libc::S_IFMT != libc::S_IFREG {
                return Err(io::Error::other(
                    "destination is not a regular file (symlinks are refused)",
                ));
            }
            existing_mode = Some(stat.st_mode & 0o7777);
        } else if io::Error::last_os_error().kind() != io::ErrorKind::NotFound {
            return Err(io::Error::last_os_error());
        }
        let temporary = CString::new(format!(
            ".ck-motor-return-{}",
            motor_protocol::Uuid::now_v7()
        ))
        .unwrap();
        let fd = unsafe {
            libc::openat(
                directory.as_raw_fd(),
                temporary.as_ptr(),
                libc::O_WRONLY | libc::O_CREAT | libc::O_EXCL | libc::O_NOFOLLOW | libc::O_CLOEXEC,
                0o666 as libc::c_uint,
            )
        };
        if fd < 0 {
            return Err(io::Error::last_os_error());
        }
        let mut file = unsafe { File::from_raw_fd(fd) };
        let result = (|| {
            if let Some(mode) = existing_mode {
                if unsafe { libc::fchmod(file.as_raw_fd(), mode as libc::mode_t) } < 0 {
                    return Err(io::Error::last_os_error());
                }
            }
            file.write_all(data)?;
            file.sync_all()?;
            // renameat replaces the directory entry, never the target of a
            // symlink swapped in after the no-follow destination check.
            if unsafe {
                libc::renameat(
                    directory.as_raw_fd(),
                    temporary.as_ptr(),
                    directory.as_raw_fd(),
                    destination.as_ptr(),
                )
            } < 0
            {
                return Err(io::Error::last_os_error());
            }
            directory.sync_all()
        })();
        if result.is_err() {
            unsafe {
                libc::unlinkat(directory.as_raw_fd(), temporary.as_ptr(), 0);
            }
        }
        result
    };
    install().map_err(|error| format!("read_error (local write refused: {error})"))
}
