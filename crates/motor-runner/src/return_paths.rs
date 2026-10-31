use crate::storage::{ReturnedChunk, ReturnedFiles, RETURNED_DIRECTORY_PREFIX};
use base64::{engine::general_purpose::STANDARD, Engine};
use motor_protocol::{
    Frame, ReturnFailureCause as Cause, ReturnManifest, ReturnManifestEntry, ReturnManifestOutcome,
    ReturnPathsFailed, ReturnPathsFailureCause, ReturnResult, ReturnedFile, Run, Uuid,
    MAX_FRAME_BYTES, RETURN_CHUNK_MAX_BYTES,
};
use sha2::{Digest, Sha256};
#[cfg(unix)]
use std::{
    ffi::CString,
    os::{
        fd::{AsRawFd, FromRawFd},
        unix::ffi::OsStrExt,
    },
};
use std::{
    fs::{self, File},
    io::{self, Read as _, Write},
    path::{Path, PathBuf},
};

// Leave descriptor headroom for serving connections and other jobs, including
// on hosts whose service limit is only 256. Expanded link paths also need a
// work limit: repeatedly entering and leaving a directory need not grow depth.
#[cfg(unix)]
const MAX_RETURN_DIRECTORY_DESCRIPTORS: usize = 64;
#[cfg(unix)]
const MAX_RETURN_PATH_COMPONENTS: usize = 4096;

// Resolve symlinks first for an explicit escape result, then walk the resolved
// path through directory descriptors without following replacements. No target
// outside the workspace is opened, even if a command left a racing descendant.
// Follow links through pinned directory descriptors, never through openat.
// Resolving '..' against the descriptor stack prevents a link from leaving the
// workspace, including a dangling link whose outside target does not exist.
#[cfg(unix)]
fn open_file(root: &Path, directory: &File, relative: &Path) -> Result<File, Cause> {
    walk_file(root, directory, relative, |_, _| {})
}

#[cfg(unix)]
fn walk_file(
    root: &Path,
    directory: &File,
    relative: &Path,
    mut observe: impl FnMut(usize, usize),
) -> Result<File, Cause> {
    let mut directories = vec![directory.try_clone().map_err(|_| Cause::Missing)?];
    let mut pending: std::collections::VecDeque<_> = relative
        .components()
        .map(|c| c.as_os_str().to_owned())
        .collect();
    let mut links = 0;
    let mut processed = 0;
    observe(processed, directories.len());
    while let Some(component) = pending.pop_front() {
        // Complexity limits are confinement limits, not a claim that the target
        // is missing or non-regular. Stop before opening another component.
        if processed >= MAX_RETURN_PATH_COMPONENTS {
            return Err(Cause::Escape);
        }
        processed += 1;
        observe(processed, directories.len());
        if component == "." {
            continue;
        }
        if component == ".." {
            if directories.len() == 1 {
                return Err(Cause::Escape);
            }
            directories.pop();
            continue;
        }
        let name = CString::new(component.as_bytes()).map_err(|_| Cause::Missing)?;
        let parent = directories.last().unwrap();
        let fd = unsafe {
            libc::openat(
                parent.as_raw_fd(),
                name.as_ptr(),
                libc::O_RDONLY | libc::O_CLOEXEC | libc::O_NOFOLLOW | libc::O_NONBLOCK,
            )
        };
        if fd < 0 {
            if io::Error::last_os_error().raw_os_error() != Some(libc::ELOOP) {
                return Err(Cause::Missing);
            }
            links += 1;
            if links > 40 {
                return Err(Cause::Missing);
            }
            let mut target = vec![0u8; 4097];
            let size = unsafe {
                libc::readlinkat(
                    parent.as_raw_fd(),
                    name.as_ptr(),
                    target.as_mut_ptr().cast(),
                    target.len(),
                )
            };
            if size < 0 || size as usize == target.len() {
                return Err(Cause::Missing);
            }
            target.truncate(size as usize);
            let target = Path::new(std::ffi::OsStr::from_bytes(&target));
            let relative = if target.is_absolute() {
                directories.truncate(1);
                target.strip_prefix(root).map_err(|_| Cause::Escape)?
            } else {
                target
            };
            for component in relative.components().rev() {
                pending.push_front(component.as_os_str().to_owned());
            }
            continue;
        }
        let file = unsafe { File::from_raw_fd(fd) };
        let metadata = file.metadata().map_err(|_| Cause::Missing)?;
        if pending.is_empty() {
            return if metadata.is_file() {
                Ok(file)
            } else {
                Err(Cause::NotRegular)
            };
        }
        if !metadata.is_dir() {
            return Err(Cause::NotRegular);
        }
        if directories.len() >= MAX_RETURN_DIRECTORY_DESCRIPTORS {
            return Err(Cause::Escape);
        }
        directories.push(file);
        observe(processed, directories.len());
    }
    Err(Cause::NotRegular)
}

#[cfg(unix)]
fn open_directory(path: &Path) -> Result<(File, PathBuf), Cause> {
    let parent = path
        .parent()
        .filter(|parent| !parent.as_os_str().is_empty())
        .unwrap_or_else(|| Path::new("."));
    let name = path.file_name().ok_or(Cause::Escape)?;
    let root_name = name.to_owned();
    let parent = fs::canonicalize(parent).map_err(|_| Cause::Escape)?;
    let directory = File::open(&parent).map_err(|_| Cause::Escape)?;
    let name = CString::new(name.as_bytes()).map_err(|_| Cause::Escape)?;
    let fd = unsafe {
        libc::openat(
            directory.as_raw_fd(),
            name.as_ptr(),
            libc::O_RDONLY | libc::O_CLOEXEC | libc::O_NOFOLLOW | libc::O_DIRECTORY,
        )
    };
    if fd < 0 {
        return Err(Cause::Escape);
    }
    let directory = unsafe { File::from_raw_fd(fd) };
    let root = parent.join(root_name);
    Ok((directory, root))
}

// Windows jobs collect through the guest protocol, never through host paths.
#[cfg(windows)]
fn open_file(_: &Path, _: &File, _: &Path) -> Result<File, Cause> {
    Err(Cause::Escape)
}
#[cfg(windows)]
fn open_directory(_: &Path) -> Result<(File, PathBuf), Cause> {
    Err(Cause::Escape)
}

#[cfg(test)]
pub(crate) fn collect(request: &Run, seq: u64) -> ReturnResult {
    collect_after_measure(request, seq, || {})
}

pub(crate) fn collect_authorized(config: &crate::Config, request: &Run, seq: u64) -> ReturnResult {
    collect_with_root(request, seq, collection_root(Some(config), request), || {})
}

/// Collection handed from VM shutdown to the engine's finished job-record save.
/// Chunk bytes already live outside disposable staging; sequence numbers are
/// assigned when the engine saves the chunk metadata, manifest and terminal
/// record together.
pub(crate) enum Collection {
    Legacy(ReturnResult),
    Chunked(ChunkedCollection),
}

#[cfg(unix)]
pub(crate) fn collect_staged(
    request: &Run,
    staged_workspace: &Path,
    job_directory: &Path,
) -> Collection {
    // The host descriptor pins the share, while absolute symlinks are interpreted
    // in the guest namespace. Never open a guest absolute pathname on the host.
    let root = open_mapped_directory(staged_workspace).map(|directory| {
        (
            directory,
            PathBuf::from(
                crate::caller_path::request(request)
                    .caller_workspace_key
                    .text(),
            ),
        )
    });
    collect_from_root(request, job_directory, root)
}

pub(crate) fn unread(request: &Run, job_directory: &Path) -> Collection {
    collect_from_root(request, job_directory, Err(Cause::ReadError))
}

fn collect_from_root(
    request: &Run,
    job_directory: &Path,
    root: Result<(File, PathBuf), Cause>,
) -> Collection {
    if request.return_chunked {
        Collection::Chunked(collect_chunked_with_root(
            request,
            job_directory,
            root,
            |_| {},
        ))
    } else {
        Collection::Legacy(collect_with_root(request, 0, root, || {}))
    }
}

fn collection_root(
    config: Option<&crate::Config>,
    request: &Run,
) -> Result<(File, PathBuf), Cause> {
    let caller = crate::caller_path::request(request)
        .caller_workspace_key
        .text();
    if let Some(config) = config.filter(|config| !config.caller_roots.is_empty()) {
        let host =
            crate::caller_path::authorize(config, caller, false).map_err(|_| Cause::Escape)?;
        if crate::caller_path::mapped_root(config, caller).is_some() {
            return open_mapped_directory(&host).map(|file| (file, PathBuf::from(caller)));
        }
    }
    open_directory(
        crate::caller_path::request(request)
            .host_workspace
            .as_path(),
    )
}

#[cfg(unix)]
fn open_mapped_directory(host: &Path) -> Result<File, Cause> {
    let mut directory = File::open("/").map_err(|_| Cause::Escape)?;
    for component in host.components().skip(1) {
        let name = CString::new(component.as_os_str().as_bytes()).map_err(|_| Cause::Escape)?;
        let fd = unsafe {
            libc::openat(
                directory.as_raw_fd(),
                name.as_ptr(),
                libc::O_RDONLY | libc::O_CLOEXEC | libc::O_NOFOLLOW | libc::O_DIRECTORY,
            )
        };
        if fd < 0 {
            return Err(Cause::Escape);
        }
        directory = unsafe { File::from_raw_fd(fd) };
    }
    Ok(directory)
}

#[cfg(not(unix))]
fn open_mapped_directory(_: &Path) -> Result<File, Cause> {
    Err(Cause::Escape)
}

pub(crate) fn set_seq(result: &mut ReturnResult, seq: u64, cap: u64) {
    result.seq = seq;
    if serde_json::to_vec(&Frame::ReturnResult(result.clone()))
        .is_ok_and(|json| json.len() > MAX_FRAME_BYTES)
    {
        let measured_bytes = result.returned_files.as_ref().map_or(0, |files| {
            files
                .iter()
                .map(|file| {
                    // Each padded base64 quartet represents three bytes, less its '=' suffix.
                    let padding = file.data.bytes().rev().take_while(|b| *b == b'=').count();
                    (file.data.len() / 4 * 3 - padding) as u64
                })
                .sum()
        });
        result.returned_files = None;
        result.return_paths_failed = Some(ReturnPathsFailed {
            measured_bytes,
            cap_bytes: cap,
            cause: Cause::FrameLimit,
        });
    }
}

#[cfg(test)]
fn collect_after_measure(request: &Run, seq: u64, before_read: impl FnOnce()) -> ReturnResult {
    collect_with_root(request, seq, collection_root(None, request), before_read)
}

fn collect_with_root(
    request: &Run,
    seq: u64,
    root: Result<(File, PathBuf), Cause>,
    before_read: impl FnOnce(),
) -> ReturnResult {
    let mut result = ReturnResult {
        job_id: request.job_id,
        seq,
        returned_files: None,
        return_paths_failed: None,
    };
    let mut measured = 0u64;
    let collected = (|| -> Result<Vec<ReturnedFile>, Cause> {
        let (directory, root) = root?;
        let mut files = Vec::new();
        for path in crate::caller_path::run(request).caller_return_paths {
            let relative = Path::new(path)
                .strip_prefix(
                    crate::caller_path::request(request)
                        .caller_workspace_key
                        .text(),
                )
                .map_err(|_| Cause::Escape)?;
            let file = open_file(&root, &directory, relative)?;
            measured = measured.saturating_add(file.metadata().map_err(|_| Cause::Missing)?.len());
            files.push(file);
        }
        if measured > request.return_max_bytes {
            return Err(Cause::OverCap);
        }
        before_read();
        let mut returned = Vec::new();
        let mut read_bytes = 0u64;
        for (path, mut file) in crate::caller_path::run(request)
            .caller_return_paths
            .iter()
            .zip(&files)
        {
            let mut bytes = Vec::new();
            (&mut file)
                .take(request.return_max_bytes - read_bytes)
                .read_to_end(&mut bytes)
                .map_err(|_| Cause::Missing)?;
            read_bytes += bytes.len() as u64;
            returned.push(ReturnedFile {
                path: path.clone(),
                sha256: format!("{:x}", Sha256::digest(&bytes)),
                data: STANDARD.encode(bytes),
            });
        }
        // Recheck the open files, not their pathnames, to detect growth while
        // reading without ever reading a byte beyond the caller's total cap.
        measured = files.iter().try_fold(0u64, |sum, file| {
            file.metadata()
                .map(|m| sum.saturating_add(m.len()))
                .map_err(|_| Cause::Missing)
        })?;
        if measured > request.return_max_bytes {
            return Err(Cause::OverCap);
        }
        result.returned_files = Some(returned.clone());
        if serde_json::to_vec(&Frame::ReturnResult(result.clone()))
            .map_err(|_| Cause::FrameLimit)?
            .len()
            > MAX_FRAME_BYTES
        {
            return Err(Cause::FrameLimit);
        }
        Ok(returned)
    })();
    match collected {
        Ok(files) => result.returned_files = Some(files),
        Err(cause) => {
            result.returned_files = None;
            result.return_paths_failed = Some(ReturnPathsFailed {
                measured_bytes: measured,
                cap_bytes: request.return_max_bytes,
                cause,
            });
        }
    }
    result
}

/// A finished chunked collection whose chunk bytes are written and synced in
/// their own directory, waiting for seqs and the record save that commits it.
pub(crate) struct ChunkedCollection {
    /// Name of the job-directory subdirectory holding the chunk files.
    pub directory: String,
    /// Chunks in send order, with seq still unassigned.
    pub chunks: Vec<ReturnedChunk>,
    pub files: Vec<ReturnManifestEntry>,
    pub total_bytes: u64,
    pub cap_bytes: u64,
}

impl ChunkedCollection {
    /// Give the chunks, then the manifest, the next seqs, as they go to replay
    /// after the job's output and before its terminal record.
    pub(crate) fn commit(&self, job_id: Uuid, next_seq: &mut u64) -> ReturnedFiles {
        let mut chunks = self.chunks.clone();
        let mut counts = vec![0u32; self.files.len()];
        for chunk in &mut chunks {
            chunk.seq = *next_seq;
            *next_seq += 1;
            counts[chunk.path_index as usize] += 1;
        }
        let manifest = ReturnManifest::new(
            job_id,
            *next_seq,
            self.files
                .iter()
                .cloned()
                .enumerate()
                .map(|(index, entry)| entry.with_chunks(counts[index]))
                .collect(),
            self.total_bytes,
            self.cap_bytes,
        );
        *next_seq += 1;
        ReturnedFiles {
            directory: self.directory.clone(),
            chunks,
            manifest,
            expired: false,
        }
    }
}

/// What reading one admitted file produced.
enum Read {
    Returned {
        sha256: String,
    },
    /// The file could not be read, changed size, or its path no longer named
    /// the opened file afterwards. Its chunks are dropped.
    Changed,
}

/// Collect a `return_chunked` request's files into chunk files under a new
/// subdirectory of `job_directory`, at most `RETURN_CHUNK_MAX_BYTES` each.
///
/// Every path is opened through the same no-follow walk as `collect`, and its
/// size decides, in request order, whether it fits under the cap, before any
/// file is read. A file that does not fit is `over_cap` and none of its bytes
/// are read. Reading a file consumes at most one byte beyond its admitted size,
/// to detect growth. Growth, shrinking, or its path naming a different file
/// once read (for example after being replaced by a symlink) make it
/// `read_error`, and its chunks are dropped.
#[cfg(test)]
pub(crate) fn collect_chunked(request: &Run, job_directory: &Path) -> ChunkedCollection {
    collect_chunked_before_read(request, job_directory, |_| {})
}

pub(crate) fn collect_chunked_authorized(
    config: &crate::Config,
    request: &Run,
    job_directory: &Path,
) -> ChunkedCollection {
    collect_chunked_with_root(
        request,
        job_directory,
        collection_root(Some(config), request),
        |_| {},
    )
}

#[cfg(test)]
fn collect_chunked_before_read(
    request: &Run,
    job_directory: &Path,
    before_read: impl FnMut(usize),
) -> ChunkedCollection {
    collect_chunked_with_root(
        request,
        job_directory,
        collection_root(None, request),
        before_read,
    )
}

fn collect_chunked_with_root(
    request: &Run,
    job_directory: &Path,
    root: Result<(File, PathBuf), Cause>,
    mut before_read: impl FnMut(usize),
) -> ChunkedCollection {
    let cap = request.return_max_bytes;
    let directory = format!("{RETURNED_DIRECTORY_PREFIX}{}", Uuid::now_v7());
    let mut outcomes: Vec<Option<ReturnManifestOutcome>> =
        vec![None; crate::caller_path::run(request).caller_return_paths.len()];
    let fail = |cause, size| ReturnManifestOutcome::Failed { cause, size };
    let mut admitted = Vec::new();
    let mut total = 0u64;
    match root {
        Err(cause) => {
            for outcome in &mut outcomes {
                *outcome = Some(fail(shared_cause(cause), None));
            }
        }
        Ok((root_directory, root)) => {
            for (index, path) in crate::caller_path::run(request)
                .caller_return_paths
                .iter()
                .enumerate()
            {
                let opened = Path::new(path)
                    .strip_prefix(
                        crate::caller_path::request(request)
                            .caller_workspace_key
                            .text(),
                    )
                    .map_err(|_| Cause::Escape)
                    .and_then(|relative| {
                        let file = open_file(&root, &root_directory, relative)?;
                        let size = file.metadata().map_err(|_| Cause::Missing)?.len();
                        Ok((relative.to_owned(), file, size))
                    });
                match opened {
                    Err(cause) => outcomes[index] = Some(fail(shared_cause(cause), None)),
                    // Decided before any byte of any file is read.
                    Ok((_, _, size)) if total.saturating_add(size) > cap => {
                        outcomes[index] = Some(fail(ReturnPathsFailureCause::OverCap, Some(size)));
                    }
                    Ok((relative, file, size)) => {
                        total += size;
                        admitted.push((index, relative, file, size));
                    }
                }
            }
            let written = (|| -> io::Result<Vec<ReturnedChunk>> {
                let chunk_directory = job_directory.join(&directory);
                fs::create_dir_all(&chunk_directory)?;
                let mut chunks = Vec::new();
                for (index, relative, file, size) in &admitted {
                    before_read(*index);
                    let first = chunks.len();
                    let read =
                        read_chunks(file, *size, *index as u32, &chunk_directory, &mut chunks)?;
                    let read = match read {
                        Read::Returned { sha256 }
                            if same_file(file, open_file(&root, &root_directory, relative)) =>
                        {
                            Read::Returned { sha256 }
                        }
                        _ => Read::Changed,
                    };
                    outcomes[*index] = Some(match read {
                        Read::Returned { sha256 } => ReturnManifestOutcome::Returned {
                            size: *size,
                            sha256,
                        },
                        Read::Changed => {
                            for position in first..chunks.len() {
                                fs::remove_file(chunk_directory.join(format!("chunk-{position}")))?;
                            }
                            chunks.truncate(first);
                            total -= size;
                            fail(ReturnPathsFailureCause::ReadError, Some(*size))
                        }
                    });
                }
                crate::filesystem::sync_directory(&chunk_directory)?;
                crate::filesystem::sync_directory(job_directory)?;
                Ok(chunks)
            })();
            match written {
                Ok(chunks) => {
                    return finish(request, directory, chunks, outcomes, total);
                }
                Err(error) => {
                    // The bytes cannot be kept for replay, so none are sent.
                    tracing::warn!(job_id = %request.job_id, %error, "returned files could not be stored");
                    let _ = fs::remove_dir_all(job_directory.join(&directory));
                    for (index, _, _, size) in &admitted {
                        outcomes[*index] =
                            Some(fail(ReturnPathsFailureCause::ReadError, Some(*size)));
                    }
                    total = 0;
                }
            }
        }
    }
    finish(request, directory, Vec::new(), outcomes, total)
}

fn finish(
    request: &Run,
    directory: String,
    chunks: Vec<ReturnedChunk>,
    outcomes: Vec<Option<ReturnManifestOutcome>>,
    total_bytes: u64,
) -> ChunkedCollection {
    let files = crate::caller_path::run(request)
        .caller_return_paths
        .iter()
        .zip(outcomes)
        .map(
            |(path, outcome)| match outcome.expect("every requested path has an outcome") {
                ReturnManifestOutcome::Returned { size, sha256 } => {
                    ReturnManifestEntry::returned(path.clone(), size, sha256)
                }
                ReturnManifestOutcome::Failed { cause, size } => {
                    let entry = ReturnManifestEntry::failed(path.clone(), cause);
                    match size {
                        Some(size) => entry.with_size(size),
                        None => entry,
                    }
                }
                _ => unreachable!("collection produces only returned and failed entries"),
            },
        )
        .collect();
    ChunkedCollection {
        directory,
        chunks,
        files,
        total_bytes,
        cap_bytes: request.return_max_bytes,
    }
}

fn shared_cause(cause: Cause) -> ReturnPathsFailureCause {
    match cause {
        Cause::OverCap => ReturnPathsFailureCause::OverCap,
        Cause::Missing => ReturnPathsFailureCause::Missing,
        Cause::Escape => ReturnPathsFailureCause::Escape,
        Cause::NotRegular => ReturnPathsFailureCause::NotRegular,
        Cause::FrameLimit => ReturnPathsFailureCause::FrameLimit,
        Cause::ReadError => ReturnPathsFailureCause::ReadError,
    }
}

/// Read exactly `size` bytes of `file` into chunk files, appending to
/// `chunks`. Metadata detects growth without reading beyond the admitted size.
fn read_chunks(
    mut file: &File,
    size: u64,
    path_index: u32,
    chunk_directory: &Path,
    chunks: &mut Vec<ReturnedChunk>,
) -> io::Result<Read> {
    let mut hasher = Sha256::new();
    let mut offset = 0u64;
    let mut buffer = vec![0u8; RETURN_CHUNK_MAX_BYTES];
    while offset < size {
        let want = (size - offset).min(RETURN_CHUNK_MAX_BYTES as u64) as usize;
        let Ok(length) = fill(&mut file, &mut buffer[..want]) else {
            return Ok(Read::Changed);
        };
        if length < want {
            return Ok(Read::Changed);
        }
        let data = &buffer[..length];
        hasher.update(data);
        let position = chunks.len();
        store_chunk(chunk_directory, position, data)?;
        chunks.push(ReturnedChunk {
            seq: 0,
            path_index,
            offset,
            length: length as u64,
        });
        offset += length as u64;
    }
    let resized = !file.metadata().is_ok_and(|m| m.len() == size);
    if resized {
        return Ok(Read::Changed);
    }
    Ok(Read::Returned {
        sha256: format!("{:x}", hasher.finalize()),
    })
}

fn store_chunk(directory: &Path, position: usize, data: &[u8]) -> io::Result<()> {
    let mut chunk = File::create(directory.join(format!("chunk-{position}")))?;
    chunk.write_all(data)?;
    chunk.sync_all()
}

/// Receives a guest stream into the same durable spool as the host collector.
/// Incomplete files and abandoned streams are deleted before their reservation
/// can be released by the engine's existing cleanup pass.
pub(crate) struct StreamedCollection {
    request: Run,
    job_directory: PathBuf,
    directory: String,
    chunks: Vec<ReturnedChunk>,
    outcomes: Vec<Option<ReturnManifestOutcome>>,
    buffer: Vec<u8>,
    hasher: Sha256,
    current: u64,
    total: u64,
    discarded: u64,
    first: usize,
    finished: bool,
    failed: bool,
}

impl StreamedCollection {
    pub(crate) fn new(request: &Run, job_directory: &Path) -> io::Result<Self> {
        let directory = format!("{RETURNED_DIRECTORY_PREFIX}{}", Uuid::now_v7());
        fs::create_dir_all(job_directory.join(&directory))?;
        Ok(Self {
            request: request.clone(),
            job_directory: job_directory.to_owned(),
            directory,
            chunks: Vec::new(),
            outcomes: Vec::new(),
            buffer: Vec::with_capacity(RETURN_CHUNK_MAX_BYTES),
            hasher: Sha256::new(),
            current: 0,
            total: 0,
            discarded: 0,
            first: 0,
            finished: false,
            failed: false,
        })
    }

    pub(crate) fn push(&mut self, mut bytes: &[u8]) -> io::Result<()> {
        if self.failed
            || self.outcomes.len()
                >= crate::caller_path::run(&self.request)
                    .caller_return_paths
                    .len()
            || self.total + self.discarded + self.current + bytes.len() as u64
                > self.request.return_max_bytes
        {
            self.failed = true;
            return Err(io::Error::other(
                "guest exceeded returned-file cap or count",
            ));
        }
        self.hasher.update(bytes);
        self.current += bytes.len() as u64;
        while !bytes.is_empty() {
            let n = bytes.len().min(RETURN_CHUNK_MAX_BYTES - self.buffer.len());
            self.buffer.extend_from_slice(&bytes[..n]);
            bytes = &bytes[n..];
            if self.buffer.len() == RETURN_CHUNK_MAX_BYTES {
                self.flush()?;
            }
        }
        Ok(())
    }

    fn flush(&mut self) -> io::Result<()> {
        if self.buffer.is_empty() {
            return Ok(());
        }
        let position = self.chunks.len();
        store_chunk(
            &self.job_directory.join(&self.directory),
            position,
            &self.buffer,
        )?;
        let offset = self.chunks[self.first..].iter().map(|c| c.length).sum();
        self.chunks.push(ReturnedChunk {
            seq: 0,
            path_index: self.outcomes.len() as u32,
            offset,
            length: self.buffer.len() as u64,
        });
        self.buffer.clear();
        Ok(())
    }

    pub(crate) fn end_file(
        &mut self,
        outcome: motor_winagent::returned::Outcome,
    ) -> io::Result<()> {
        use motor_winagent::returned::{Failure, Outcome};
        if self.failed
            || self.outcomes.len()
                >= crate::caller_path::run(&self.request)
                    .caller_return_paths
                    .len()
        {
            self.failed = true;
            return Err(io::Error::other("too many returned-file outcomes"));
        }
        let outcome = match outcome {
            Outcome::Returned { size, sha256 } => {
                if size != self.current || sha256 != format!("{:x}", self.hasher.clone().finalize())
                {
                    self.failed = true;
                    return Err(io::Error::other(
                        "guest returned-file digest or size mismatch",
                    ));
                }
                self.flush()?;
                self.total += size;
                ReturnManifestOutcome::Returned { size, sha256 }
            }
            Outcome::Failed { cause, size } => {
                self.discarded += self.current;
                for position in self.first..self.chunks.len() {
                    fs::remove_file(
                        self.job_directory
                            .join(&self.directory)
                            .join(format!("chunk-{position}")),
                    )?;
                }
                self.chunks.truncate(self.first);
                self.buffer.clear();
                let cause = match cause {
                    Failure::OverCap => ReturnPathsFailureCause::OverCap,
                    Failure::Missing => ReturnPathsFailureCause::Missing,
                    Failure::Escape => ReturnPathsFailureCause::Escape,
                    Failure::NotRegular => ReturnPathsFailureCause::NotRegular,
                    Failure::ReadError => ReturnPathsFailureCause::ReadError,
                };
                ReturnManifestOutcome::Failed { cause, size }
            }
        };
        self.outcomes.push(Some(outcome));
        self.first = self.chunks.len();
        self.current = 0;
        self.hasher = Sha256::new();
        Ok(())
    }

    pub(crate) fn finish(mut self) -> io::Result<Collection> {
        if self.failed
            || self.outcomes.len()
                != crate::caller_path::run(&self.request)
                    .caller_return_paths
                    .len()
            || self.current != 0
        {
            return Err(io::Error::other("incomplete guest returned-file stream"));
        }
        crate::filesystem::sync_directory(&self.job_directory.join(&self.directory))?;
        crate::filesystem::sync_directory(&self.job_directory)?;
        let collection = finish(
            &self.request,
            self.directory.clone(),
            std::mem::take(&mut self.chunks),
            std::mem::take(&mut self.outcomes),
            self.total,
        );
        if self.request.return_chunked {
            self.finished = true;
            Ok(Collection::Chunked(collection))
        } else {
            let result = legacy_from_chunks(&self.request, &self.job_directory, &collection);
            // Drop removes the temporary spool; legacy inline results are
            // bounded by the legacy frame limit.
            Ok(Collection::Legacy(result))
        }
    }
}

impl Drop for StreamedCollection {
    fn drop(&mut self) {
        if !self.finished {
            let _ = fs::remove_dir_all(self.job_directory.join(&self.directory));
        }
    }
}

fn legacy_from_chunks(request: &Run, job: &Path, collection: &ChunkedCollection) -> ReturnResult {
    let mut result = ReturnResult {
        job_id: request.job_id,
        seq: 0,
        returned_files: None,
        return_paths_failed: None,
    };
    let files = (|| -> Result<Vec<ReturnedFile>, Cause> {
        // Bound even temporary raw and base64 allocations before reading disk.
        if collection.total_bytes > (MAX_FRAME_BYTES / 4 * 3) as u64 {
            return Err(Cause::FrameLimit);
        }
        let mut files = Vec::new();
        for (index, entry) in collection.files.iter().enumerate() {
            let sha256 = match entry.outcome() {
                ReturnManifestOutcome::Returned { sha256, .. } => sha256.clone(),
                ReturnManifestOutcome::Failed { cause, .. } => {
                    return Err(match cause {
                        ReturnPathsFailureCause::OverCap => Cause::OverCap,
                        ReturnPathsFailureCause::Missing => Cause::Missing,
                        ReturnPathsFailureCause::Escape => Cause::Escape,
                        ReturnPathsFailureCause::NotRegular => Cause::NotRegular,
                        _ => Cause::ReadError,
                    })
                }
                _ => return Err(Cause::ReadError),
            };
            let mut bytes = Vec::new();
            for (position, chunk) in collection
                .chunks
                .iter()
                .enumerate()
                .filter(|(_, c)| c.path_index == index as u32)
            {
                let mut file = File::open(
                    job.join(&collection.directory)
                        .join(format!("chunk-{position}")),
                )
                .map_err(|_| Cause::ReadError)?;
                (&mut file)
                    .take(chunk.length)
                    .read_to_end(&mut bytes)
                    .map_err(|_| Cause::ReadError)?;
            }
            files.push(ReturnedFile {
                path: crate::caller_path::run(request).caller_return_paths[index].clone(),
                sha256,
                data: STANDARD.encode(bytes),
            });
        }
        Ok(files)
    })();
    match files {
        Ok(files) => {
            result.returned_files = Some(files);
            set_seq(&mut result, 0, request.return_max_bytes);
        }
        Err(cause) => {
            result.return_paths_failed = Some(ReturnPathsFailed {
                measured_bytes: collection.total_bytes,
                cap_bytes: request.return_max_bytes,
                cause,
            })
        }
    }
    result
}

/// Read until `buffer` is full or the file ends; returns the bytes read.
fn fill(file: &mut &File, buffer: &mut [u8]) -> io::Result<usize> {
    let mut length = 0;
    while length < buffer.len() {
        match file.read(&mut buffer[length..]) {
            Ok(0) => break,
            Ok(n) => length += n,
            Err(error) if error.kind() == io::ErrorKind::Interrupted => {}
            Err(error) => return Err(error),
        }
    }
    Ok(length)
}

/// Whether the path, walked again without following links out of the
/// workspace, still names the file that was read.
#[cfg(unix)]
fn same_file(read: &File, reopened: Result<File, Cause>) -> bool {
    use std::os::unix::fs::MetadataExt;
    let (Ok(read), Ok(reopened)) = (
        read.metadata(),
        reopened.and_then(|f| f.metadata().map_err(|_| Cause::Missing)),
    ) else {
        return false;
    };
    read.dev() == reopened.dev() && read.ino() == reopened.ino()
}
#[cfg(windows)]
fn same_file(_: &File, _: Result<File, Cause>) -> bool {
    false
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_helpers::CanonicalScratchDir;
    use cortexkit_test_support::ScratchDir;
    use motor_protocol::{ReturnAssembler, RETURN_CHUNKED_MAX_BYTES_CAP};

    #[test]
    fn windows_return_stream_128_mib_has_bounded_guest_and_host_memory() {
        #[cfg(windows)]
        let _identity = crate::windows::returned_tests::impersonate_self();
        #[cfg(target_os = "linux")]
        {
            const CHILD: &str = "MOTOR_RETURN_MEMORY_CHILD";
            if std::env::var_os(CHILD).is_none() {
                let output = std::process::Command::new(std::env::current_exe().unwrap())
                    .args(["--exact", "return_paths::tests::windows_return_stream_128_mib_has_bounded_guest_and_host_memory", "--nocapture", "--test-threads=1"])
                    .env(CHILD, "1").output().unwrap();
                assert!(
                    output.status.success(),
                    "{}\n{}",
                    String::from_utf8_lossy(&output.stdout),
                    String::from_utf8_lossy(&output.stderr)
                );
                return;
            }
            // An accidental whole-file Vec cannot fit, in either collector.
            // Use a subprocess so the address-space policy cannot affect peers.
            let limit = libc::rlimit {
                rlim_cur: 32 * 1024 * 1024,
                rlim_max: 32 * 1024 * 1024,
            };
            assert_eq!(unsafe { libc::setrlimit(libc::RLIMIT_DATA, &limit) }, 0);
        }
        let source = ScratchDir::new("windows-return-source");
        let job = ScratchDir::new("windows-return-spool");
        let cap = 128 * 1024 * 1024;
        File::create(source.path().join("artifact"))
            .unwrap()
            .set_len(cap)
            .unwrap();
        let mut run = request(
            Path::new("/workspace"),
            &[Path::new("/workspace/artifact")],
            cap,
        );
        run.return_chunked = true;
        let mut sink = StreamedCollection::new(&run, job.path()).unwrap();
        let mut guest_chunks = 0;
        motor_winagent::returned::collect(
            source.path(),
            "",
            &motor_winagent::returned::Request {
                paths: vec!["artifact".into()],
                max_bytes: cap,
            },
            |frame| {
                match frame {
                    motor_winagent::Frame::ReturnChunk { bytes } => {
                        assert!(bytes.len() <= 64 * 1024);
                        assert!(bytes.capacity() <= 64 * 1024);
                        guest_chunks += 1;
                        sink.push(&bytes)?;
                        assert!(sink.buffer.capacity() <= RETURN_CHUNK_MAX_BYTES);
                    }
                    motor_winagent::Frame::ReturnFileEnd { outcome } => sink.end_file(outcome)?,
                    motor_winagent::Frame::ReturnEnd {} => (),
                    _ => panic!("unexpected return frame"),
                }
                Ok(())
            },
        )
        .unwrap();
        assert_eq!(guest_chunks, 2048);
        let Collection::Chunked(collection) = sink.finish().unwrap() else {
            panic!("expected chunk spool");
        };
        assert_eq!(collection.total_bytes, cap);
        assert_eq!(
            collection.chunks.len(),
            cap.div_ceil(RETURN_CHUNK_MAX_BYTES as u64) as usize
        );
        assert_eq!(
            collection.files[0].outcome(),
            &ReturnManifestOutcome::Returned {
                size: cap,
                sha256: "254bcc3fc4f27172636df4bf32de9f107f620d559b20d760197e452b97453917".into(),
            }
        );
        assert_eq!(collection.chunks.iter().map(|c| c.length).sum::<u64>(), cap);
        assert!(collection
            .chunks
            .iter()
            .all(|c| c.length <= RETURN_CHUNK_MAX_BYTES as u64));
    }

    #[test]
    fn windows_return_stream_failed_file_is_deleted_and_abandoned_spool_is_removed() {
        let job = ScratchDir::new("windows-return-rollback");
        let mut run = request(
            Path::new("/workspace"),
            &[Path::new("/workspace/bad"), Path::new("/workspace/good")],
            1024 * 1024,
        );
        run.return_chunked = true;
        let mut sink = StreamedCollection::new(&run, job.path()).unwrap();
        let directory = job.path().join(&sink.directory);
        sink.push(&vec![0; RETURN_CHUNK_MAX_BYTES]).unwrap();
        sink.end_file(motor_winagent::returned::Outcome::Failed {
            cause: motor_winagent::returned::Failure::ReadError,
            size: Some(RETURN_CHUNK_MAX_BYTES as u64),
        })
        .unwrap();
        assert_eq!(fs::read_dir(&directory).unwrap().count(), 0);
        sink.push(b"good").unwrap();
        assert!(sink
            .end_file(motor_winagent::returned::Outcome::Returned {
                size: 4,
                sha256: "invalid".into()
            })
            .is_err());
        drop(sink);
        assert!(!directory.exists());
    }

    #[test]
    fn windows_return_spool_enforces_cap_independently_of_guest() {
        let job = ScratchDir::new("windows-return-cap");
        let run = request(Path::new("/workspace"), &[Path::new("/workspace/file")], 2);
        let mut sink = StreamedCollection::new(&run, job.path()).unwrap();
        assert!(sink.push(b"abc").is_err());
        assert_eq!(sink.current, 0);
        assert!(sink.chunks.is_empty());
    }

    #[test]
    fn windows_return_discarded_chunks_spend_the_spool_allowance_and_poison_overflow() {
        let job = ScratchDir::new("windows-return-discard-budget");
        let cap = 2 * RETURN_CHUNK_MAX_BYTES as u64;
        let run = request(
            Path::new("/workspace"),
            &[
                Path::new("/workspace/one"),
                Path::new("/workspace/two"),
                Path::new("/workspace/three"),
            ],
            cap,
        );
        let mut sink = StreamedCollection::new(&run, job.path()).unwrap();
        let directory = job.path().join(&sink.directory);
        for _ in 0..2 {
            sink.push(&vec![0; RETURN_CHUNK_MAX_BYTES]).unwrap();
            assert_eq!(fs::read_dir(&directory).unwrap().count(), 1);
            sink.end_file(motor_winagent::returned::Outcome::Failed {
                cause: motor_winagent::returned::Failure::ReadError,
                size: Some(RETURN_CHUNK_MAX_BYTES as u64),
            })
            .unwrap();
            assert_eq!(fs::read_dir(&directory).unwrap().count(), 0);
        }
        assert_eq!(sink.total, 0);
        assert!(
            sink.push(&[1]).is_err(),
            "discarded bytes must remain charged against the stream cap"
        );
        assert!(
            sink.end_file(motor_winagent::returned::Outcome::Failed {
                cause: motor_winagent::returned::Failure::ReadError,
                size: None,
            })
            .is_err(),
            "a poisoned stream must not accept another file outcome"
        );
        assert!(
            sink.finish().is_err(),
            "an over-cap stream must not publish a partial manifest"
        );
        assert!(!directory.exists());
    }

    #[test]
    fn windows_return_stream_keeps_bounded_legacy_inline_contract() {
        let job = ScratchDir::new("windows-return-inline");
        let run = request(
            Path::new("/workspace"),
            &[Path::new("/workspace/file")],
            1024 * 1024,
        );
        for length in [2, MAX_FRAME_BYTES] {
            let mut sink = StreamedCollection::new(&run, job.path()).unwrap();
            let directory = job.path().join(&sink.directory);
            let bytes = vec![b'x'; length];
            sink.push(&bytes).unwrap();
            sink.end_file(motor_winagent::returned::Outcome::Returned {
                size: length as u64,
                sha256: format!("{:x}", Sha256::digest(&bytes)),
            })
            .unwrap();
            let Collection::Legacy(result) = sink.finish().unwrap() else {
                panic!("expected legacy result");
            };
            if length == 2 {
                assert_eq!(result.returned_files.unwrap()[0].data, "eHg=");
            } else {
                assert_eq!(result.return_paths_failed.unwrap().cause, Cause::FrameLimit);
            }
            assert!(!directory.exists());
        }
    }

    fn collect(request: &Run) -> ReturnResult {
        super::collect(request, 0)
    }

    fn request(root: &Path, paths: &[&Path], cap: u64) -> Run {
        let mut run: Run = match serde_json::from_str::<Frame>(include_str!(
            "../../../test-vectors/exec-remote-v1/frames/run.json"
        ))
        .unwrap()
        {
            Frame::Run(run) => run,
            _ => unreachable!(),
        };
        run.workspace_key = root.to_str().unwrap().into();
        run.return_paths = paths.iter().map(|p| p.to_str().unwrap().into()).collect();
        run.return_max_bytes = cap;
        run
    }

    fn failed(result: ReturnResult, cause: Cause, measured: u64, cap: u64) {
        assert_eq!(result.returned_files, None);
        assert_eq!(
            result.return_paths_failed,
            Some(ReturnPathsFailed {
                cause,
                measured_bytes: measured,
                cap_bytes: cap
            })
        );
    }

    #[test]
    fn return_paths_one_byte_has_sha256_and_base64() {
        let dir = ScratchDir::new("return-paths");
        let file = dir.join("one");
        fs::write(&file, b"x").unwrap();
        let result = collect(&request(dir.path(), &[&file], 1));
        assert_eq!(result.return_paths_failed, None);
        assert_eq!(
            result.returned_files.unwrap(),
            vec![ReturnedFile {
                path: file.to_str().unwrap().into(),
                sha256: "2d711642b726b04401627ca9fbac32f5c8530fb1903cc4db02258717921a4881".into(),
                data: "eA==".into(),
            }]
        );
    }

    #[cfg(unix)]
    #[test]
    fn return_paths_resolver_bounds_live_directory_stack() {
        use std::os::unix::fs::symlink;
        let scratch = ScratchDir::new("return-paths-depth-budget");
        let root = scratch.path();
        let mut parent = root.to_owned();
        // Each short link expands into forty real directories, so checking only
        // the caller pathname length or the number of links cannot bound FDs.
        for link in ["first", "second"] {
            let mut next = parent.clone();
            for _ in 0..40 {
                next.push("d");
                fs::create_dir(&next).unwrap();
            }
            let target =
                PathBuf::from("d/".repeat(40)).join(if link == "first" { "second" } else { "out" });
            symlink(target, parent.join(link)).unwrap();
            parent = next;
        }
        fs::write(parent.join("out"), b"unreachable").unwrap();
        let (directory, _) = open_directory(root).unwrap();
        let mut max_depth = 0;
        let mut processed = 0;
        let result = walk_file(root, &directory, Path::new("first"), |work, depth| {
            max_depth = max_depth.max(depth);
            processed = work;
        });
        assert_eq!(result.unwrap_err(), Cause::Escape);
        assert_eq!(max_depth, MAX_RETURN_DIRECTORY_DESCRIPTORS);
        assert!(
            processed < MAX_RETURN_PATH_COMPONENTS,
            "the depth limit fires first"
        );
    }

    #[cfg(unix)]
    #[test]
    fn return_paths_resolver_bounds_total_component_expansion() {
        use std::os::unix::fs::symlink;
        let scratch = ScratchDir::new("return-paths-work-budget");
        let root = scratch.path();
        fs::create_dir(root.join("d")).unwrap();
        for index in 0..24 {
            let next = if index == 23 {
                "out".into()
            } else {
                format!("link-{}", index + 1)
            };
            symlink(
                format!("{}{next}", "d/../".repeat(100)),
                root.join(format!("link-{index}")),
            )
            .unwrap();
        }
        fs::write(root.join("out"), b"unreachable").unwrap();
        let (directory, _) = open_directory(root).unwrap();
        let mut max_depth = 0;
        let mut processed = 0;
        let result = walk_file(root, &directory, Path::new("link-0"), |work, depth| {
            max_depth = max_depth.max(depth);
            processed = work;
        });
        assert_eq!(result.unwrap_err(), Cause::Escape);
        assert_eq!(processed, MAX_RETURN_PATH_COMPONENTS);
        assert_eq!(
            max_depth, 2,
            "component expansion need not grow the descriptor stack"
        );
    }

    #[cfg(unix)]
    fn staged_collection(run: &Run, host: &Path, job: &Path) -> ChunkedCollection {
        let Collection::Chunked(collection) = collect_staged(run, host, job) else {
            panic!("expected chunked collection")
        };
        collection
    }

    #[cfg(unix)]
    #[test]
    fn staged_returns_use_share_not_canonical_checkout() {
        let scratch = CanonicalScratchDir::new("staged-return-root");
        let canonical = scratch.join("canonical");
        let staged = scratch.join("staged");
        let job = scratch.join("job");
        for path in [&canonical, &staged, &job] {
            fs::create_dir_all(path).unwrap();
        }
        fs::write(staged.join("artifact"), b"x").unwrap();
        assert!(!canonical.join("artifact").exists());
        let mut run = request(&canonical, &[&canonical.join("artifact")], 1);
        let Collection::Legacy(result) = collect_staged(&run, &staged, &job) else {
            panic!("expected legacy collection")
        };
        assert_eq!(result.returned_files.unwrap()[0].data, "eA==");
        run.return_chunked = true;
        let collection = staged_collection(&run, &staged, &job);
        assert_eq!(collection.total_bytes, 1);
        assert_eq!(
            collection.files[0].sha256(),
            Some("2d711642b726b04401627ca9fbac32f5c8530fb1903cc4db02258717921a4881")
        );
        fs::remove_dir_all(&staged).unwrap();
        assert_eq!(
            fs::read(job.join(collection.directory).join("chunk-0")).unwrap(),
            b"x"
        );
    }

    #[cfg(unix)]
    #[test]
    fn staged_returns_hostile_links_never_spool_host_sentinel() {
        use std::os::unix::fs::symlink;
        let scratch = CanonicalScratchDir::new("staged-return-links");
        let staged = scratch.join("staged");
        let job = scratch.join("job");
        fs::create_dir_all(&staged).unwrap();
        fs::create_dir_all(&job).unwrap();
        let sentinel = b"host-secret-must-not-be-returned";
        fs::write(scratch.join("secret"), sentinel).unwrap();
        fs::write(staged.join("out"), b"safe").unwrap();
        let logical = Path::new("/Users/guest/workspace");
        for (name, target) in [
            ("absolute", scratch.join("secret")),
            ("parent", PathBuf::from("../secret")),
            ("dangling", scratch.join("absent")),
            ("sibling", PathBuf::from("/Users/guest/sibling/secret")),
            ("relative-ok", PathBuf::from("out")),
            ("absolute-ok", logical.join("out")),
        ] {
            symlink(target, staged.join(name)).unwrap();
        }
        let names = [
            "absolute",
            "parent",
            "dangling",
            "sibling",
            "relative-ok",
            "absolute-ok",
        ];
        let paths: Vec<_> = names.iter().map(|name| logical.join(name)).collect();
        let refs: Vec<_> = paths.iter().map(PathBuf::as_path).collect();
        let mut run = request(logical, &refs, 1024);
        run.return_chunked = true;
        let collection = staged_collection(&run, &staged, &job);
        for entry in &collection.files[..4] {
            assert_eq!(entry.cause(), Some(&ReturnPathsFailureCause::Escape));
        }
        assert_eq!(collection.total_bytes, 8);
        for entry in &collection.files[4..] {
            assert_eq!(entry.size(), Some(4));
            assert!(entry.sha256().is_some());
        }
        for index in 0..collection.chunks.len() {
            let bytes = fs::read(
                job.join(&collection.directory)
                    .join(format!("chunk-{index}")),
            )
            .unwrap();
            assert!(!bytes
                .windows(sentinel.len())
                .any(|window| window == sentinel));
            assert_eq!(bytes, b"safe");
        }
        run.return_chunked = false;
        for path in &paths[..4] {
            run.return_paths = vec![path.to_str().unwrap().into()];
            let Collection::Legacy(result) = collect_staged(&run, &staged, &job) else {
                unreachable!()
            };
            failed(result, Cause::Escape, 0, 1024);
        }
    }

    #[cfg(unix)]
    #[test]
    fn staged_returns_pin_root_and_refuse_symlink_replacement() {
        use std::os::unix::fs::symlink;
        let scratch = CanonicalScratchDir::new("staged-return-replacement");
        let staged = scratch.join("staged");
        let saved = scratch.join("saved");
        let job = scratch.join("job");
        fs::create_dir_all(&staged).unwrap();
        fs::create_dir_all(&job).unwrap();
        fs::write(staged.join("out"), b"safe").unwrap();
        let logical = Path::new("/Users/guest/workspace");
        let mut run = request(logical, &[&logical.join("out")], 4);
        run.return_chunked = true;
        let pinned = open_mapped_directory(&staged).unwrap();
        fs::rename(&staged, &saved).unwrap();
        symlink(&saved, &staged).unwrap();
        let collection =
            collect_chunked_with_root(&run, &job, Ok((pinned, logical.into())), |_| {});
        assert_eq!(collection.total_bytes, 4);
        let collection = staged_collection(&run, &staged, &job);
        assert_eq!(
            collection.files[0].cause(),
            Some(&ReturnPathsFailureCause::Escape)
        );
        assert!(collection.chunks.is_empty());
    }

    #[cfg(unix)]
    #[test]
    fn staged_returns_exact_ten_mib_and_over_cap_admission_without_read() {
        let scratch = CanonicalScratchDir::new("staged-return-cap");
        let staged = scratch.join("staged");
        let job = scratch.join("job");
        fs::create_dir_all(&staged).unwrap();
        fs::create_dir_all(&job).unwrap();
        let cap = RETURN_CHUNKED_MAX_BYTES_CAP;
        File::create(staged.join("large"))
            .unwrap()
            .set_len(cap)
            .unwrap();
        File::create(staged.join("too-large"))
            .unwrap()
            .set_len(cap + 1)
            .unwrap();
        fs::write(staged.join("small"), b"x").unwrap();
        let logical = Path::new("/Users/guest/workspace");
        let mut run = request(
            logical,
            &[
                &logical.join("too-large"),
                &logical.join("large"),
                &logical.join("small"),
            ],
            cap,
        );
        run.return_chunked = true;
        let mut reads = Vec::new();
        let root = open_mapped_directory(&staged).map(|file| (file, logical.into()));
        let collection = collect_chunked_with_root(&run, &job, root, |index| reads.push(index));
        assert_eq!(reads, vec![1]);
        assert_eq!(collection.total_bytes, cap);
        assert_eq!(
            collection.files[0].cause(),
            Some(&ReturnPathsFailureCause::OverCap)
        );
        assert_eq!(collection.files[1].size(), Some(cap));
        assert_eq!(collection.chunks.len(), 20);
        assert_eq!(
            collection.files[2].cause(),
            Some(&ReturnPathsFailureCause::OverCap)
        );
        run.return_paths.remove(1);
        let collection = staged_collection(&run, &staged, &job);
        assert_eq!(
            collection.total_bytes, 1,
            "later fitting files remain eligible"
        );
        assert_eq!(collection.chunks.len(), 1);
        File::create(staged.join("legacy"))
            .unwrap()
            .set_len(motor_protocol::RETURN_MAX_BYTES)
            .unwrap();
        run.return_chunked = false;
        run.return_max_bytes = motor_protocol::RETURN_MAX_BYTES;
        run.return_paths = vec![logical.join("legacy").to_str().unwrap().into()];
        let Collection::Legacy(result) = collect_staged(&run, &staged, &job) else {
            unreachable!()
        };
        assert_eq!(
            STANDARD
                .decode(&result.returned_files.unwrap()[0].data)
                .unwrap()
                .len() as u64,
            motor_protocol::RETURN_MAX_BYTES
        );
    }

    #[test]
    fn return_paths_measure_all_files_before_reading_and_keep_no_partial_bytes() {
        let dir = ScratchDir::new("return-paths");
        let one = dir.join("one");
        let two = dir.join("two");
        fs::write(&one, b"x").unwrap();
        fs::write(&two, b"xy").unwrap();
        failed(
            collect(&request(dir.path(), &[&one, &two], 2)),
            Cause::OverCap,
            3,
            2,
        );
        failed(
            collect(&request(dir.path(), &[&two], 1)),
            Cause::OverCap,
            2,
            1,
        );
        failed(
            collect(&request(dir.path(), &[&dir.join("missing")], 1)),
            Cause::Missing,
            0,
            1,
        );
        failed(
            collect(&request(dir.path(), &[dir.path()], 1)),
            Cause::NotRegular,
            0,
            1,
        );
    }

    #[test]
    fn return_paths_detect_growth_between_measurement_and_read() {
        let dir = ScratchDir::new("return-paths");
        let file = dir.join("growing");
        fs::write(&file, b"x").unwrap();
        failed(
            collect_after_measure(&request(dir.path(), &[&file], 1), 0, || {
                fs::write(&file, b"xy").unwrap()
            }),
            Cause::OverCap,
            2,
            1,
        );
    }

    #[test]
    fn return_paths_escape_and_replaced_symlink_never_return_target_bytes() {
        use std::os::unix::fs::symlink;
        let dir = ScratchDir::new("return-paths");
        let root = dir.join("workspace");
        fs::create_dir(&root).unwrap();
        let secret = dir.join("secret");
        fs::write(&secret, b"private").unwrap();
        let link = root.join("link");
        symlink(&secret, &link).unwrap();
        failed(
            collect(&request(&root, &[&link], 100)),
            Cause::Escape,
            0,
            100,
        );
        fs::remove_file(&secret).unwrap();
        failed(
            collect(&request(&root, &[&link], 100)),
            Cause::Escape,
            0,
            100,
        );
        fs::write(&secret, b"private").unwrap();
        fs::remove_file(&link).unwrap();
        fs::write(&link, b"x").unwrap();
        let result = collect_after_measure(&request(&root, &[&link], 1), 0, || {
            fs::remove_file(&link).unwrap();
            symlink(&secret, &link).unwrap();
        });
        assert_eq!(result.returned_files.unwrap()[0].data, "eA==");
    }

    // This fixture needs Linux's longer filesystem paths to exceed the JSON frame cap.
    #[cfg(target_os = "linux")]
    #[test]
    fn return_paths_frame_limit_accounts_for_json_path_escaping() {
        let dir = ScratchDir::new("return-paths");
        let mut path = dir.path().to_path_buf();
        for _ in 0..34 {
            path.push("\u{1}".repeat(100));
        }
        fs::create_dir_all(&path).unwrap();
        path.push("out");
        fs::write(&path, b"x").unwrap();
        let run = request(
            dir.path(),
            &vec![path.as_path(); 64],
            motor_protocol::RETURN_MAX_BYTES,
        );
        assert!(motor_protocol::validate_return_paths(
            &run.workspace_key,
            &run.return_paths,
            run.return_max_bytes
        )
        .is_ok());
        failed(
            collect(&run),
            Cause::FrameLimit,
            64,
            motor_protocol::RETURN_MAX_BYTES,
        );
    }

    #[test]
    fn return_paths_frame_size_uses_final_sequence_and_escaped_paths() {
        let mut result = ReturnResult {
            job_id: motor_protocol::Uuid::now_v7(),
            seq: 0,
            returned_files: Some(vec![
                ReturnedFile {
                    path: format!("/workspace/{}", "\u{1}".repeat(3500)),
                    sha256: "a".repeat(64),
                    data: "eA==".into()
                };
                64
            ]),
            return_paths_failed: None,
        };
        set_seq(&mut result, u64::MAX, 64);
        assert_eq!(result.seq, u64::MAX);
        failed(result, Cause::FrameLimit, 64, 64);
    }

    #[test]
    fn return_paths_follow_internal_links_but_refuse_replaced_workspace_root() {
        use std::os::unix::fs::symlink;
        let dir = ScratchDir::new("return-paths");
        let canonical = fs::canonicalize(dir.path()).unwrap();
        let root = canonical.join("workspace");
        fs::create_dir_all(root.join("sub")).unwrap();
        fs::write(root.join("one"), b"x").unwrap();
        symlink("../one", root.join("sub/link")).unwrap();
        symlink(root.join("one"), root.join("absolute")).unwrap();
        let run = request(&root, &[&root.join("sub/link"), &root.join("absolute")], 2);
        let result = collect(&run);
        assert_eq!(result.return_paths_failed, None);
        assert!(result
            .returned_files
            .unwrap()
            .iter()
            .all(|f| f.data == "eA=="));
        fs::rename(&root, canonical.join("old-workspace")).unwrap();
        symlink(canonical.join("old-workspace"), &root).unwrap();
        failed(collect(&run), Cause::Escape, 0, 2);
    }

    #[test]
    fn return_paths_allow_a_symlinked_workspace_ancestor() {
        use std::os::unix::fs::symlink;
        let dir = ScratchDir::new("return-paths");
        let base = fs::canonicalize(dir.path()).unwrap();
        let actual_parent = base.join("actual-parent");
        let linked_parent = base.join("linked-parent");
        fs::create_dir_all(actual_parent.join("workspace")).unwrap();
        symlink(&actual_parent, &linked_parent).unwrap();
        let root = linked_parent.join("workspace");
        let file = root.join("returned");
        fs::write(&file, b"x").unwrap();

        let result = collect(&request(&root, &[&file], 1));

        assert_eq!(result.return_paths_failed, None);
        assert_eq!(result.returned_files.unwrap()[0].data, "eA==");
    }

    /// Bytes that differ at every position, so a misplaced chunk changes the
    /// digest.
    fn pattern(length: usize, seed: u8) -> Vec<u8> {
        (0..length)
            .map(|index| (index as u32).wrapping_mul(2_654_435_761).to_le_bytes()[3] ^ seed)
            .collect()
    }

    fn chunked(root: &Path, paths: &[&Path], cap: u64) -> Run {
        let mut run = request(root, paths, cap);
        run.return_chunked = true;
        run
    }

    /// Commit a collection as the engine does and check the stream it
    /// replays with the shared caller-side checker. Returns each path's
    /// verified bytes, or `None` for a failed path, and the manifest.
    fn verify(
        run: &Run,
        job_directory: &Path,
        collection: &ChunkedCollection,
    ) -> (Vec<Option<Vec<u8>>>, ReturnManifest) {
        let mut next_seq = 5;
        let returned = collection.commit(run.job_id, &mut next_seq);
        assert_eq!(next_seq, 5 + returned.chunks.len() as u64 + 1);
        assert_eq!(returned.manifest.seq, next_seq - 1);
        let mut assembler = ReturnAssembler::new(
            crate::caller_path::run(run).caller_return_paths,
            run.return_max_bytes,
        );
        let mut bytes = vec![Vec::new(); run.return_paths.len()];
        for (position, chunk) in returned.chunks.iter().enumerate() {
            let data = fs::read(returned.chunk_path(job_directory, position)).unwrap();
            assert_eq!(data.len() as u64, chunk.length);
            assert!(data.len() <= RETURN_CHUNK_MAX_BYTES);
            let record = motor_protocol::ReturnChunk::new(
                run.job_id,
                chunk.seq,
                chunk.path_index,
                chunk.offset,
                motor_protocol::BytePayload(data),
            );
            // Every chunk frame fits the runner protocol's frame cap.
            assert!(
                serde_json::to_vec(&Frame::ReturnChunk(record.clone()))
                    .unwrap()
                    .len()
                    <= MAX_FRAME_BYTES
            );
            let accepted = assembler.accept_chunk(&record).unwrap();
            bytes[chunk.path_index as usize].extend_from_slice(accepted);
        }
        let verified = assembler.finish(&returned.manifest).unwrap();
        let files = verified
            .iter()
            .zip(bytes)
            .map(|(file, bytes)| file.is_returned().then_some(bytes))
            .collect();
        (files, returned.manifest)
    }

    fn cause(
        manifest: &ReturnManifest,
        index: usize,
    ) -> Option<(ReturnPathsFailureCause, Option<u64>)> {
        let entry = &manifest.files[index];
        entry.cause().map(|cause| (cause.clone(), entry.size()))
    }

    #[test]
    fn return_chunked_430_kb_file_round_trips_with_its_sha256() {
        let dir = ScratchDir::new("return-chunked");
        let root = dir.join("workspace");
        let job = dir.join("job");
        fs::create_dir_all(&root).unwrap();
        fs::create_dir_all(&job).unwrap();
        let file = root.join("generated.bin");
        let content = pattern(430 * 1024, 7);
        fs::write(&file, &content).unwrap();
        let run = chunked(&root, &[&file], RETURN_CHUNKED_MAX_BYTES_CAP);
        let collection = collect_chunked(&run, &job);
        assert_eq!(collection.chunks.len(), 1, "430 KiB fits one 512 KiB chunk");
        let (files, manifest) = verify(&run, &job, &collection);
        assert_eq!(files, vec![Some(content.clone())]);
        assert_eq!(
            manifest.files[0].sha256(),
            Some(format!("{:x}", Sha256::digest(&content)).as_str())
        );
        assert_eq!(manifest.files[0].path(), file.to_str().unwrap());
        assert_eq!(manifest.total_bytes, content.len() as u64);
        assert_eq!(manifest.cap_bytes, RETURN_CHUNKED_MAX_BYTES_CAP);
    }

    #[test]
    fn return_chunked_ten_mib_over_several_files_is_returned_in_order() {
        let dir = ScratchDir::new("return-chunked");
        let root = dir.join("workspace");
        let job = dir.join("job");
        fs::create_dir_all(&root).unwrap();
        fs::create_dir_all(&job).unwrap();
        let quarter = RETURN_CHUNKED_MAX_BYTES_CAP as usize / 4;
        let paths: Vec<_> = (0..4u8)
            .map(|index| {
                let path = root.join(format!("part-{index}"));
                fs::write(&path, pattern(quarter, index)).unwrap();
                path
            })
            .collect();
        let empty = root.join("empty");
        fs::write(&empty, b"").unwrap();
        let mut requested: Vec<&Path> = paths.iter().map(PathBuf::as_path).collect();
        requested.push(&empty);
        let run = chunked(&root, &requested, RETURN_CHUNKED_MAX_BYTES_CAP);
        let collection = collect_chunked(&run, &job);
        let (files, manifest) = verify(&run, &job, &collection);
        for index in 0..4u8 {
            assert_eq!(
                files[index as usize].as_deref(),
                Some(pattern(quarter, index).as_slice())
            );
        }
        assert_eq!(
            files[4].as_deref(),
            Some(&[][..]),
            "a zero-byte file has no chunks"
        );
        assert_eq!(manifest.total_bytes, RETURN_CHUNKED_MAX_BYTES_CAP);
        // 2.5 MiB per file in 512 KiB chunks, grouped by path.
        assert_eq!(collection.chunks.len(), 4 * 5);
        assert!(collection
            .chunks
            .windows(2)
            .all(|pair| pair[0].path_index <= pair[1].path_index));
    }

    #[test]
    fn return_chunked_one_byte_over_ten_mib_refuses_that_file_before_any_chunk() {
        let dir = ScratchDir::new("return-chunked");
        let root = dir.join("workspace");
        let job = dir.join("job");
        fs::create_dir_all(&root).unwrap();
        fs::create_dir_all(&job).unwrap();
        let cap = RETURN_CHUNKED_MAX_BYTES_CAP;
        let whole = root.join("whole");
        fs::write(&whole, pattern(cap as usize + 1, 1)).unwrap();
        let run = chunked(&root, &[&whole], cap);
        let collection = collect_chunked(&run, &job);
        assert!(
            collection.chunks.is_empty(),
            "no partial chunks for an over-cap file"
        );
        let (files, manifest) = verify(&run, &job, &collection);
        assert_eq!(files, vec![None]);
        assert_eq!(manifest.files[0].chunks(), Some(0));
        assert_eq!(
            cause(&manifest, 0),
            Some((ReturnPathsFailureCause::OverCap, Some(cap + 1)))
        );
        assert_eq!(manifest.total_bytes, 0);
        let written: Vec<_> = fs::read_dir(job.join(&collection.directory))
            .unwrap()
            .collect();
        assert!(written.is_empty(), "no chunk bytes were stored");

        // Two files that reach the cap exactly, then one byte more: only the
        // last file is refused, and it has no chunks.
        let first = root.join("first");
        let second = root.join("second");
        let extra = root.join("extra");
        fs::write(&first, pattern(cap as usize / 2, 2)).unwrap();
        fs::write(&second, pattern(cap as usize / 2, 3)).unwrap();
        fs::write(&extra, b"x").unwrap();
        let run = chunked(&root, &[&first, &second, &extra], cap);
        let collection = collect_chunked(&run, &job);
        assert!(collection.chunks.iter().all(|chunk| chunk.path_index < 2));
        let (files, manifest) = verify(&run, &job, &collection);
        assert!(files[0].is_some() && files[1].is_some());
        assert_eq!(files[2], None);
        assert_eq!(manifest.files[2].chunks(), Some(0));
        assert_eq!(
            cause(&manifest, 2),
            Some((ReturnPathsFailureCause::OverCap, Some(1)))
        );
        assert_eq!(manifest.total_bytes, cap);
    }

    #[test]
    fn large_return_over_cap_is_not_read_and_growth_never_reads_past_admitted_size() {
        use std::io::Seek;
        let dir = ScratchDir::new("return-large-read-bound");
        let root = dir.join("workspace");
        let job = dir.join("job");
        fs::create_dir_all(&root).unwrap();
        fs::create_dir_all(&job).unwrap();
        let cap = motor_protocol::RETURN_LARGE_MAX_BYTES_CAP;
        let path = root.join("oversized");
        File::create(&path).unwrap().set_len(cap + 1).unwrap();
        let run = chunked(&root, &[&path], cap);
        let collection = collect_chunked_with_root(&run, &job, open_directory(&root), |_| {
            panic!("over-cap file was read")
        });
        assert!(collection.chunks.is_empty());
        assert!(matches!(
            collection.files[0].outcome,
            ReturnManifestOutcome::Failed {
                cause: ReturnPathsFailureCause::OverCap,
                ..
            }
        ));

        let mut file = File::open(&path).unwrap();
        let chunks_dir = job.join("chunks");
        fs::create_dir_all(&chunks_dir).unwrap();
        let mut chunks = Vec::new();
        assert!(matches!(
            read_chunks(&file, cap, 0, &chunks_dir, &mut chunks).unwrap(),
            Read::Changed
        ));
        assert_eq!(file.stream_position().unwrap(), cap);
    }

    #[test]
    fn return_chunked_failures_are_per_path_and_keep_the_other_files() {
        use std::os::unix::fs::symlink;
        let dir = ScratchDir::new("return-chunked");
        let root = dir.join("workspace");
        let job = dir.join("job");
        fs::create_dir_all(root.join("directory")).unwrap();
        fs::create_dir_all(&job).unwrap();
        let secret = dir.join("secret");
        fs::write(&secret, b"private").unwrap();
        symlink(&secret, root.join("escape")).unwrap();
        fs::write(root.join("kept"), b"kept").unwrap();
        let run = chunked(
            &root,
            &[
                &root.join("missing"),
                &root.join("directory"),
                &root.join("escape"),
                &root.join("kept"),
            ],
            RETURN_CHUNKED_MAX_BYTES_CAP,
        );
        let collection = collect_chunked(&run, &job);
        let (files, manifest) = verify(&run, &job, &collection);
        assert_eq!(files, vec![None, None, None, Some(b"kept".to_vec())]);
        assert_eq!(
            cause(&manifest, 0),
            Some((ReturnPathsFailureCause::Missing, None))
        );
        assert_eq!(
            cause(&manifest, 1),
            Some((ReturnPathsFailureCause::NotRegular, None))
        );
        assert_eq!(
            cause(&manifest, 2),
            Some((ReturnPathsFailureCause::Escape, None))
        );
    }

    #[test]
    fn return_chunked_growth_while_reading_is_a_read_error_without_chunks() {
        let dir = ScratchDir::new("return-chunked");
        let root = dir.join("workspace");
        let job = dir.join("job");
        fs::create_dir_all(&root).unwrap();
        fs::create_dir_all(&job).unwrap();
        let growing = root.join("growing");
        let kept = root.join("kept");
        fs::write(&growing, pattern(600 * 1024, 4)).unwrap();
        fs::write(&kept, b"kept").unwrap();
        let run = chunked(&root, &[&growing, &kept], RETURN_CHUNKED_MAX_BYTES_CAP);
        let collection = collect_chunked_before_read(&run, &job, |index| {
            if index == 0 {
                let mut file = fs::OpenOptions::new().append(true).open(&growing).unwrap();
                file.write_all(b"more").unwrap();
            }
        });
        assert!(collection.chunks.iter().all(|chunk| chunk.path_index == 1));
        let (files, manifest) = verify(&run, &job, &collection);
        assert_eq!(files, vec![None, Some(b"kept".to_vec())]);
        assert_eq!(
            cause(&manifest, 0),
            Some((ReturnPathsFailureCause::ReadError, Some(600 * 1024)))
        );
        assert_eq!(manifest.total_bytes, 4);
        assert_eq!(manifest.files[0].chunks(), Some(0));
        assert_eq!(manifest.files[1].chunks(), Some(1));
        // Only the kept file's chunk remains on disk.
        assert_eq!(
            fs::read_dir(job.join(&collection.directory))
                .unwrap()
                .count(),
            1
        );
    }

    #[test]
    fn return_chunked_manifest_counts_only_committed_chunks_for_each_path() {
        let dir = ScratchDir::new("return-chunked-counts");
        let root = dir.join("workspace");
        let job = dir.join("job");
        fs::create_dir_all(&root).unwrap();
        fs::create_dir_all(&job).unwrap();
        let empty = root.join("empty");
        let kept = root.join("kept");
        let growing = root.join("growing");
        let content = pattern(RETURN_CHUNK_MAX_BYTES * 3 / 2, 5);
        fs::write(&empty, b"").unwrap();
        fs::write(&kept, &content).unwrap();
        fs::write(&growing, &content).unwrap();
        let run = chunked(
            &root,
            &[&empty, &kept, &growing],
            RETURN_CHUNKED_MAX_BYTES_CAP,
        );
        let collection = collect_chunked_before_read(&run, &job, |index| {
            if index == 2 {
                fs::OpenOptions::new()
                    .append(true)
                    .open(&growing)
                    .unwrap()
                    .write_all(b"more")
                    .unwrap();
            }
        });
        let (files, manifest) = verify(&run, &job, &collection);
        assert_eq!(files, vec![Some(Vec::new()), Some(content.clone()), None]);
        assert_eq!(
            manifest
                .files
                .iter()
                .map(|entry| entry.chunks())
                .collect::<Vec<_>>(),
            vec![Some(0), Some(2), Some(0)]
        );
        assert_eq!(
            cause(&manifest, 2),
            Some((
                ReturnPathsFailureCause::ReadError,
                Some(content.len() as u64)
            ))
        );
        assert_eq!(collection.chunks.len(), 2);
        assert!(collection.chunks.iter().all(|chunk| chunk.path_index == 1));
        assert_eq!(manifest.total_bytes, content.len() as u64);
    }

    #[test]
    fn return_chunked_legacy_expired_manifest_omits_all_counts() {
        let stored = ReturnedFiles {
            directory: "returned-legacy".into(),
            chunks: Vec::new(),
            manifest: ReturnManifest::new(
                Uuid::nil(),
                3,
                vec![
                    ReturnManifestEntry::returned("kept", 1, "digest"),
                    ReturnManifestEntry::failed("missing", ReturnPathsFailureCause::Missing),
                ],
                1,
                RETURN_CHUNKED_MAX_BYTES_CAP,
            ),
            expired: false,
        };
        let wire = serde_json::to_value(&stored).unwrap();
        assert!(wire["manifest"]["files"]
            .as_array()
            .unwrap()
            .iter()
            .all(|entry| entry.get("chunks").is_none()));
        let restored: ReturnedFiles = serde_json::from_value(wire).unwrap();
        let expired = restored.expired_manifest();
        assert!(expired.files.iter().all(|entry| entry.chunks().is_none()));
        assert_eq!(
            expired.files[0].cause(),
            Some(&ReturnPathsFailureCause::Expired)
        );
        assert_eq!(expired.files[0].size(), Some(1));
        assert_eq!(expired.files[1], stored.manifest.files[1]);
        assert_eq!(expired.total_bytes, 0);
    }

    #[test]
    fn return_chunked_file_swapped_to_a_symlink_mid_collection_is_refused() {
        use std::os::unix::fs::symlink;
        let dir = ScratchDir::new("return-chunked");
        let root = dir.join("workspace");
        let job = dir.join("job");
        fs::create_dir_all(&root).unwrap();
        fs::create_dir_all(&job).unwrap();
        let secret = dir.join("secret");
        fs::write(&secret, b"private").unwrap();
        let first = root.join("first");
        let swapped = root.join("swapped");
        fs::write(&first, b"first").unwrap();
        fs::write(&swapped, b"x").unwrap();
        let run = chunked(&root, &[&first, &swapped], RETURN_CHUNKED_MAX_BYTES_CAP);
        // Both files were measured; the second is replaced by a link to a
        // file outside the workspace while the first is being read.
        let collection = collect_chunked_before_read(&run, &job, |index| {
            if index == 0 {
                fs::remove_file(&swapped).unwrap();
                symlink(&secret, &swapped).unwrap();
            }
        });
        let (files, manifest) = verify(&run, &job, &collection);
        assert_eq!(files, vec![Some(b"first".to_vec()), None]);
        assert_eq!(
            cause(&manifest, 1),
            Some((ReturnPathsFailureCause::ReadError, Some(1)))
        );
        for entry in fs::read_dir(job.join(&collection.directory)).unwrap() {
            assert_ne!(fs::read(entry.unwrap().path()).unwrap(), b"private");
        }
    }
}
