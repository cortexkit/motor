//! Durable runner frames. The index is the commit record: data is synced first,
//! then an atomic, synced index publishes it to readers and the output watermark.
use anyhow::{ensure, Context, Result};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::BTreeMap;
use std::ffi::{CStr, CString};
use std::fs::File;
use std::io::{Read, Seek, SeekFrom, Write};
use std::os::fd::{AsRawFd, FromRawFd};
use std::path::Path;

pub(crate) const SEGMENT_BYTES: u64 = 1024 * 1024;

#[derive(Clone, Default, Deserialize, Serialize)]
struct Index {
    version: u32,
    entries: Vec<Entry>,
    last_seq: Option<u64>,
    terminal: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    terminal_delivery: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    next_id: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    truncated_before_seq: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    return_limit: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    return_bytes: Option<u64>,
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    failed_returns: BTreeMap<u64, bool>,
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    return_chunks: BTreeMap<u64, u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    chunks_expired: Option<bool>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    dropped: Vec<Dropped>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    refused_tail: Option<RefusedTail>,
}
#[derive(Clone, Deserialize, Serialize)]
struct Dropped {
    // Shared files can retain holes from evicted output or a failed return path.
    // Recording the original ranges lets recovery reject unrelated missing bytes.
    piece: Piece,
    seq: Option<u64>,
    return_path: Option<u64>,
}
#[derive(Clone, Deserialize, Serialize)]
struct RefusedTail {
    // Rejected payloads still advance ingestion. Keep their sequence interval
    // and failed paths so a higher watermark cannot hide unexplained data loss.
    first_seq: u64,
    last_seq: u64,
    paths: std::collections::BTreeSet<u64>,
}
#[derive(Clone, Deserialize, Serialize)]
struct Entry {
    seq: Option<u64>,
    pieces: Vec<Piece>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    id: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    kind: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    payload: Option<ChunkPayload>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    return_path: Option<u64>,
}
#[derive(Clone, Deserialize, Serialize)]
struct ChunkPayload {
    prefix: Vec<u8>,
    suffix: Vec<u8>,
}
#[derive(Clone, Deserialize, Serialize)]
struct Piece {
    chunk: bool,
    number: u64,
    offset: u64,
    len: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    record: Option<bool>,
}
impl Piece {
    fn name(&self) -> String {
        format!(
            "{}-{:016x}",
            if self.record == Some(true) {
                "record"
            } else if self.chunk {
                "chunk"
            } else {
                "segment"
            },
            self.number
        )
    }
}

impl Index {
    fn valid_file_gaps(&self) -> bool {
        let mut ends = BTreeMap::<String, u64>::new();
        for piece in self.entries.iter().flat_map(|entry| &entry.pieces) {
            let previous = ends.entry(piece.name()).or_default();
            if *previous > piece.offset || !self.explains_gap(piece, *previous, piece.offset) {
                return false;
            }
            let Some(end) = piece.offset.checked_add(piece.len) else {
                return false;
            };
            *previous = end;
        }
        true
    }
    fn explains_gap(&self, piece: &Piece, begin: u64, end: u64) -> bool {
        if begin == end {
            return true;
        }
        let mut ranges: Vec<_> = self
            .dropped
            .iter()
            .filter(|drop| {
                drop.piece.name() == piece.name()
                    && match drop.return_path {
                        Some(path) => drop.piece.chunk && self.failed_returns.contains_key(&path),
                        None => {
                            !drop.piece.chunk
                                && drop.piece.record != Some(true)
                                && drop
                                    .seq
                                    .zip(self.truncated_before_seq)
                                    .is_some_and(|(seq, floor)| seq < floor)
                        }
                    }
            })
            .collect();
        ranges.sort_by_key(|drop| drop.piece.offset);
        let mut cursor = begin;
        for drop in ranges {
            if drop.piece.offset > cursor {
                break;
            }
            let Some(limit) = drop.piece.offset.checked_add(drop.piece.len) else {
                return false;
            };
            cursor = cursor.max(limit);
            if cursor >= end {
                return true;
            }
        }
        false
    }

    fn explains_watermark(&self, retained: Option<u64>) -> bool {
        if self.last_seq == retained {
            return self.refused_tail.is_none();
        }
        let (Some(last), Some(tail)) = (self.last_seq, &self.refused_tail) else {
            return false;
        };
        tail.last_seq == last
            && tail.first_seq <= last
            && retained.is_none_or(|seq| seq.checked_add(1) == Some(tail.first_seq))
            && !tail.paths.is_empty()
            && tail
                .paths
                .iter()
                .all(|path| self.failed_returns.contains_key(path))
    }

    fn discard(&mut self, mut remove: impl FnMut(&Entry) -> bool) {
        let dropped = &mut self.dropped;
        self.entries.retain(|entry| {
            if !remove(entry) {
                return true;
            }
            dropped.extend(entry.pieces.iter().cloned().map(|piece| Dropped {
                piece,
                seq: entry.seq,
                return_path: entry.return_path,
            }));
            false
        });
    }

    fn repair_refused_tail(&mut self) {
        let retained = self.entries.iter().rev().find_map(|entry| entry.seq);
        self.refused_tail = self
            .last_seq
            .filter(|last| Some(*last) != retained)
            .map(|last| RefusedTail {
                first_seq: retained
                    .map(|seq| seq.saturating_add(1))
                    .or_else(|| self.refused_tail.as_ref().map(|tail| tail.first_seq))
                    .or_else(|| {
                        self.dropped
                            .iter()
                            .filter(|drop| drop.return_path.is_some())
                            .filter_map(|drop| drop.seq)
                            .min()
                    })
                    .unwrap_or(last),
                last_seq: last,
                paths: self.failed_returns.keys().copied().collect(),
            });
    }

    fn refuse(&mut self, seq: Option<u64>) -> Result<()> {
        let seq = seq.context("return chunk without sequence")?;
        ensure!(
            self.last_seq
                .is_none_or(|last| last.checked_add(1) == Some(seq)),
            "unexplained refused sequence gap"
        );
        self.last_seq = Some(seq);
        self.repair_refused_tail();
        self.trim_dropped();
        Ok(())
    }

    fn trim_dropped(&mut self) {
        // Forget deleted files and overwritten ranges, and merge adjacent holes;
        // truncation history must not grow with the lifetime volume of output.
        let mut holes = Vec::new();
        for drop in self.dropped.drain(..) {
            let occupied: Vec<_> = self
                .entries
                .iter()
                .flat_map(|entry| &entry.pieces)
                .filter(|piece| piece.name() == drop.piece.name())
                .collect();
            let end = occupied
                .iter()
                .map(|piece| piece.offset + piece.len)
                .max()
                .unwrap_or(0)
                .min(drop.piece.offset + drop.piece.len);
            let mut ranges = if drop.piece.offset < end {
                vec![(drop.piece.offset, end)]
            } else {
                Vec::new()
            };
            for piece in occupied {
                ranges = ranges
                    .into_iter()
                    .flat_map(|(begin, end)| {
                        let mut rest = Vec::new();
                        if begin < piece.offset {
                            rest.push((begin, end.min(piece.offset)));
                        }
                        if end > piece.offset + piece.len {
                            rest.push((begin.max(piece.offset + piece.len), end));
                        }
                        rest
                    })
                    .collect();
            }
            for (begin, end) in ranges {
                let mut hole = drop.clone();
                hole.piece.offset = begin;
                hole.piece.len = end - begin;
                holes.push(hole);
            }
        }
        holes.sort_by_key(|drop| (drop.piece.name(), drop.return_path, drop.piece.offset));
        for hole in holes {
            if let Some(last) = self.dropped.last_mut().filter(|last| {
                last.piece.name() == hole.piece.name()
                    && last.return_path == hole.return_path
                    && last.piece.offset + last.piece.len == hole.piece.offset
            }) {
                last.piece.len += hole.piece.len;
                last.seq = last.seq.max(hole.seq);
            } else {
                self.dropped.push(hole);
            }
        }
    }
}

pub(crate) struct Store {
    directory: File,
    index: Index,
    #[cfg(test)]
    crash_before_index: bool,
}

fn name(value: &str) -> Result<CString> {
    ensure!(!value.contains('/'), "invalid replay file name");
    Ok(CString::new(value)?)
}
fn open(directory: &File, file: &str, flags: i32) -> Result<File> {
    let file = name(file)?;
    // Every descendant is opened relative to a held directory descriptor, with
    // no symlink traversal, even if a directory name is replaced concurrently.
    let fd = unsafe {
        libc::openat(
            directory.as_raw_fd(),
            file.as_ptr(),
            flags | libc::O_NOFOLLOW | libc::O_CLOEXEC | libc::O_NONBLOCK,
            0o600,
        )
    };
    if fd < 0 {
        return Err(std::io::Error::last_os_error().into());
    }
    let file = unsafe { File::from_raw_fd(fd) };
    let metadata = file.metadata()?;
    use std::os::unix::fs::MetadataExt;
    ensure!(
        metadata.uid() == unsafe { libc::geteuid() }
            && metadata.mode() & 0o777
                == if flags & libc::O_DIRECTORY != 0 {
                    0o700
                } else {
                    0o600
                },
        "insecure replay file permissions"
    );
    ensure!(
        if flags & libc::O_DIRECTORY != 0 {
            metadata.is_dir()
        } else {
            metadata.is_file()
        },
        "invalid replay file type"
    );
    Ok(file)
}
fn subdirectory(parent: &File, child: &str) -> Result<File> {
    let child_name = name(child)?;
    let result = unsafe { libc::mkdirat(parent.as_raw_fd(), child_name.as_ptr(), 0o700) };
    if result != 0 && std::io::Error::last_os_error().kind() != std::io::ErrorKind::AlreadyExists {
        return Err(std::io::Error::last_os_error().into());
    }
    parent.sync_all()?;
    open(parent, child, libc::O_RDONLY | libc::O_DIRECTORY)
}

fn data_files(directory: &File) -> Result<Vec<String>> {
    let fd = directory.try_clone()?;
    use std::os::fd::IntoRawFd;
    let raw = fd.into_raw_fd();
    let dir = unsafe { libc::fdopendir(raw) };
    if dir.is_null() {
        unsafe {
            libc::close(raw);
        }
        return Err(std::io::Error::last_os_error().into());
    }
    let mut files = Vec::new();
    unsafe {
        libc::rewinddir(dir);
    }
    loop {
        let entry = unsafe { libc::readdir(dir) };
        if entry.is_null() {
            break;
        }
        let file = unsafe { CStr::from_ptr((*entry).d_name.as_ptr()) }.to_string_lossy();
        if file == "terminal-delivery"
            || file
                .strip_prefix("index-")
                .is_some_and(|id| uuid::Uuid::parse_str(id).is_ok())
        {
            files.push(file.into_owned());
        } else if let Some(number) = file
            .strip_prefix("segment-")
            .or_else(|| file.strip_prefix("chunk-"))
            .or_else(|| file.strip_prefix("record-"))
        {
            if number.len() == 16 && u64::from_str_radix(number, 16).is_ok() {
                files.push(file.into_owned());
            }
        }
    }
    unsafe {
        libc::closedir(dir);
    }
    Ok(files)
}

/// Allocation only: admission never parses a guest frame or creates a replay.
pub(crate) fn allocated_bytes(journal: &Path, job: &str) -> Result<u64> {
    use std::os::unix::fs::MetadataExt;
    ensure!(
        uuid::Uuid::parse_str(job)?.to_string() == job,
        "invalid replay identity"
    );
    let root = File::open(journal)?;
    let replay = open(&root, "replay", libc::O_RDONLY | libc::O_DIRECTORY)?;
    let directory = open(&replay, job, libc::O_RDONLY | libc::O_DIRECTORY)?;
    let mut files = data_files(&directory)?;
    files.push("index.json".into());
    let mut allocated = 0u64;
    for name in files {
        let file = open(&directory, &name, libc::O_RDONLY)?;
        let bytes = file
            .metadata()?
            .blocks()
            .checked_mul(512)
            .context("replay allocation overflow")?;
        allocated = allocated
            .checked_add(bytes)
            .context("replay allocation overflow")?;
    }
    Ok(allocated)
}

pub(crate) fn remove(journal: &Path, job: &str) -> Result<()> {
    ensure!(
        uuid::Uuid::parse_str(job)?.to_string() == job,
        "invalid replay identity"
    );
    let root = File::open(journal)?;
    let missing = |error: &anyhow::Error| {
        error
            .downcast_ref::<std::io::Error>()
            .is_some_and(|e| e.kind() == std::io::ErrorKind::NotFound)
    };
    let replay = match open(&root, "replay", libc::O_RDONLY | libc::O_DIRECTORY) {
        Ok(directory) => directory,
        Err(error) if missing(&error) => return Ok(()),
        Err(error) => return Err(error),
    };
    let directory = match open(&replay, job, libc::O_RDONLY | libc::O_DIRECTORY) {
        Ok(directory) => directory,
        Err(error) if missing(&error) => return Ok(()),
        Err(error) => return Err(error),
    };
    let store = Store {
        directory,
        index: Index {
            version: 1,
            ..Default::default()
        },
        #[cfg(test)]
        crash_before_index: false,
    };
    // Expiration must also work for quarantined jobs whose index is unreadable.
    // Publish an empty index before unlinking frames; the same retention pass
    // removes the lease, and an attach without a lease is refused.
    store.commit(&store.index)?;
    store.clean_files()
}

impl Store {
    pub(crate) fn open(journal: &Path, job: &str) -> Result<Self> {
        let id = uuid::Uuid::parse_str(job)?;
        ensure!(
            id.to_string() == job,
            "replay job id is not a canonical UUID"
        );
        let root = File::open(journal)?;
        let replay = subdirectory(&root, "replay")?;
        let directory = subdirectory(&replay, job)?;
        let index = match open(&directory, "index.json", libc::O_RDONLY) {
            Ok(file) => serde_json::from_reader(file).context("damaged replay index")?,
            Err(error)
                if error
                    .downcast_ref::<std::io::Error>()
                    .is_some_and(|e| e.kind() == std::io::ErrorKind::NotFound) =>
            {
                Index {
                    version: 1,
                    ..Default::default()
                }
            }
            Err(error) => return Err(error),
        };
        ensure!(
            index.version == 1,
            "unknown replay index version {}",
            index.version
        );
        let mut store = Self {
            directory,
            index,
            #[cfg(test)]
            crash_before_index: false,
        };
        for (id, entry) in store.index.entries.iter_mut().enumerate() {
            entry.id.get_or_insert(id as u64);
        }
        store.index.next_id = Some(
            store
                .index
                .next_id
                .unwrap_or(store.index.entries.len() as u64),
        );
        let mut ends = BTreeMap::<String, u64>::new();
        let mut last = None;
        let mut terminal = false;
        let mut kinds = Vec::new();
        let mut return_paths = Vec::new();
        let mut migrated_records = Vec::new();
        let mut returned_bytes = 0u64;
        let mut return_chunks = BTreeMap::<u64, u32>::new();
        for entry in &store.index.entries {
            ensure!(!terminal, "frames after replay terminal");
            if let Some(seq) = entry.seq {
                ensure!(last.is_none_or(|n| seq > n), "unordered replay index");
                last = Some(seq);
            }
            for piece in &entry.pieces {
                let end = piece
                    .offset
                    .checked_add(piece.len)
                    .context("replay offset overflow")?;
                ensure!(
                    piece.chunk || piece.record == Some(true) || end <= SEGMENT_BYTES,
                    "oversized replay segment"
                );
                let previous = ends.entry(piece.name()).or_default();
                ensure!(*previous <= piece.offset, "overlapping replay index");
                ensure!(
                    store.index.explains_gap(piece, *previous, piece.offset),
                    "unexplained replay gap"
                );
                *previous = end;
            }
            let bytes = store.read_entry(entry)?;
            let value = decode(&bytes)?;
            if value["type"] == "return_chunk" {
                *return_chunks
                    .entry(value["path_index"].as_u64().unwrap_or(0))
                    .or_default() += 1;
                returned_bytes = returned_bytes
                    .checked_add(
                        serde_json::from_value::<motor_protocol::BytePayload>(
                            value.get("data").unwrap_or(&value["bytes"]).clone(),
                        )?
                        .0
                        .len() as u64,
                    )
                    .context("return size overflow")?;
            }
            kinds.push(
                value["type"]
                    .as_str()
                    .context("replay frame type")?
                    .to_owned(),
            );
            if entry.kind.is_none()
                && !matches!(value["type"].as_str(), Some("output" | "return_chunk"))
            {
                migrated_records.push((entry.id.context("replay entry identity")?, bytes));
            }
            ensure!(
                value["seq"].as_u64() == entry.seq,
                "replay sequence mismatch"
            );
            terminal = value["type"] == "terminal";
            return_paths.push(
                (value["type"] == "return_chunk")
                    .then(|| value["path_index"].as_u64().unwrap_or(0)),
            );
        }
        ensure!(
            store.index.explains_watermark(last) && store.index.terminal == terminal,
            "inconsistent replay index"
        );
        if let Some(len) = store.index.terminal_delivery {
            ensure!(terminal, "terminal delivery without terminal");
            let bytes = store.delivery(len)?;
            ensure!(
                decode(&bytes)?["type"] == "terminal",
                "invalid terminal delivery"
            );
            ends.insert("terminal-delivery".into(), len);
        }
        for (entry, kind) in store.index.entries.iter_mut().zip(kinds) {
            entry.kind = Some(kind);
        }
        for (entry, path) in store.index.entries.iter_mut().zip(return_paths) {
            entry.return_path = path;
        }
        ensure!(
            returned_bytes <= crate::state::jobs::storage::RETURN_BYTES,
            "legacy replay exceeds return reservation"
        );
        if store.index.return_bytes.is_none() && returned_bytes != 0 {
            store.index.return_bytes = Some(returned_bytes);
        }
        if store.index.return_chunks.is_empty() {
            store.index.return_chunks = return_chunks;
        }
        for (id, bytes) in migrated_records {
            let piece = Piece {
                chunk: false,
                number: id,
                offset: 0,
                len: bytes.len() as u64,
                record: Some(true),
            };
            store.write_piece(&piece, &bytes)?;
            let entry = store
                .index
                .entries
                .iter_mut()
                .find(|entry| entry.id == Some(id))
                .context("legacy replay entry")?;
            store
                .index
                .dropped
                .extend(entry.pieces.iter().cloned().map(|piece| Dropped {
                    piece,
                    seq: entry.seq,
                    return_path: None,
                }));
            entry.pieces = vec![piece];
        }
        if !store.index.dropped.is_empty() && store.index.truncated_before_seq.is_none() {
            store.index.truncated_before_seq = store
                .index
                .entries
                .iter()
                .find(|entry| entry.kind.as_deref() == Some("output"))
                .and_then(|entry| entry.seq);
        }
        let mut bounded = store.index.clone();
        store.evict_output(&mut bounded);
        ensure!(
            bounded.valid_file_gaps(),
            "unexplained replay gap after recovery"
        );
        ensure!(
            serde_json::to_vec(&bounded)?.len() <= 1024 * 1024,
            "replay index reservation exceeded"
        );
        store.commit(&bounded)?;
        store.index = bounded;
        store.clean_files()?;
        // Recompute committed file ends after upgrading legacy record locations
        // or evicting output; interrupted writes must not become visible.
        ends.clear();
        for piece in store.index.entries.iter().flat_map(|entry| &entry.pieces) {
            ends.entry(piece.name())
                .and_modify(|end| *end = (*end).max(piece.offset + piece.len))
                .or_insert(piece.offset + piece.len);
        }
        if let Some(len) = store.index.terminal_delivery {
            ends.insert("terminal-delivery".into(), len);
        }
        for file in data_files(&store.directory)? {
            let end = ends.get(&file).copied().unwrap_or(0);
            if end == 0 {
                store.unlink(&file)?;
            } else {
                let file = open(&store.directory, &file, libc::O_RDWR)?;
                file.set_len(end)?;
                file.sync_all()?;
            }
        }
        store.directory.sync_all()?;
        Ok(store)
    }

    pub(crate) fn last_seq(&self) -> Option<u64> {
        self.index.last_seq
    }
    pub(crate) fn ended(&self) -> bool {
        self.index.terminal
    }
    pub(crate) fn len(&self) -> usize {
        self.index.entries.len()
    }
    pub(crate) fn next_id(&self) -> u64 {
        self.index.next_id.unwrap_or(self.len() as u64)
    }
    pub(crate) fn cursor_frame(
        &self,
        cursor: u64,
        from_seq: u64,
    ) -> Result<Option<(u64, Vec<u8>)>> {
        let has_manifest = self
            .index
            .entries
            .iter()
            .any(|entry| entry.kind.as_deref() == Some("return_manifest"));
        let Some((position, entry)) = self.index.entries.iter().enumerate().find(|(_, entry)| {
            entry.id.unwrap_or(0) >= cursor
                && entry.seq.is_none_or(|seq| seq >= from_seq)
                && !(self.ended() && !has_manifest && entry.kind.as_deref() == Some("return_chunk"))
        }) else {
            return Ok(None);
        };
        // A file can cross the reservation halfway through its chunks. Do not
        // publish any chunks until the manifest fixes which complete files fit;
        // callers cannot retract a partially received file after an over-cap error.
        if !has_manifest && entry.kind.as_deref() == Some("return_chunk") {
            return Ok(None);
        }
        let mut bytes = self.frame(position)?.context("replay cursor")?;
        if entry.kind.as_deref() == Some("output")
            && entry.seq == self.index.truncated_before_seq
            && entry.seq.is_some_and(|seq| from_seq < seq)
        {
            let mut frame = decode(&bytes)?;
            frame["truncated_before_seq"] = serde_json::json!(entry.seq);
            bytes = encode(&frame)?;
        }
        Ok(Some((entry.id.unwrap_or(0) + 1, bytes)))
    }
    pub(crate) fn set_return_limit(&mut self, limit: u64) -> Result<()> {
        ensure!(
            limit <= crate::state::jobs::storage::RETURN_BYTES,
            "invalid return reservation"
        );
        if self.index.return_limit == Some(limit) {
            return Ok(());
        }
        let mut next = self.index.clone();
        // The host records this once from its own relayed Run, never a runner hint.
        ensure!(
            next.return_limit.is_none(),
            "return reservation already fixed"
        );
        next.return_limit = Some(limit);
        if next.return_bytes.unwrap_or(0) > limit {
            for path in next.return_chunks.keys() {
                next.failed_returns.insert(*path, true);
            }
            next.discard(|entry| entry.kind.as_deref() == Some("return_chunk"));
            next.return_chunks.clear();
            next.repair_refused_tail();
            next.trim_dropped();
        }
        self.commit(&next)?;
        self.index = next;
        self.clean_files()
    }
    pub(crate) fn expire_chunks(&mut self) -> Result<()> {
        if self.index.chunks_expired == Some(true) {
            return Ok(());
        }
        let mut next = self.index.clone();
        next.chunks_expired = Some(true);
        next.discard(|entry| entry.kind.as_deref() == Some("return_chunk"));
        next.trim_dropped();
        self.commit(&next)?;
        self.index = next;
        self.clean_files()
    }
    fn unlink(&self, file: &str) -> Result<()> {
        if unsafe { libc::unlinkat(self.directory.as_raw_fd(), name(file)?.as_ptr(), 0) } != 0 {
            return Err(std::io::Error::last_os_error().into());
        }
        Ok(())
    }
    fn clean_files(&self) -> Result<()> {
        let used: std::collections::BTreeSet<_> = self
            .index
            .entries
            .iter()
            .flat_map(|entry| entry.pieces.iter().map(Piece::name))
            .collect();
        for file in data_files(&self.directory)? {
            if !used.contains(&file)
                && !(file == "terminal-delivery" && self.index.terminal_delivery.is_some())
            {
                self.unlink(&file)?;
            }
        }
        self.directory.sync_all()?;
        Ok(())
    }
    pub(crate) fn frame(&self, position: usize) -> Result<Option<Vec<u8>>> {
        if position + 1 == self.len() && self.index.terminal {
            if let Some(len) = self.index.terminal_delivery {
                return Ok(Some(self.delivery(len)?));
            }
        }
        let Some(bytes) = self.raw_frame(position)? else {
            return Ok(None);
        };
        let mut value = decode(&bytes)?;
        if value["type"] == "return_manifest"
            && (!self.index.failed_returns.is_empty() || self.index.chunks_expired == Some(true))
        {
            use motor_protocol::{ReturnManifest, ReturnManifestOutcome, ReturnPathsFailureCause};
            let mut manifest: ReturnManifest = serde_json::from_value(value.clone())?;
            let mut total = 0;
            for (i, file) in manifest.files.iter_mut().enumerate() {
                if let ReturnManifestOutcome::Returned { size, .. } = file.outcome {
                    let failed = self.index.failed_returns.contains_key(&(i as u64));
                    if failed || self.index.chunks_expired == Some(true) {
                        file.outcome = ReturnManifestOutcome::Failed {
                            cause: if self.index.chunks_expired == Some(true) {
                                ReturnPathsFailureCause::Expired
                            } else {
                                ReturnPathsFailureCause::OverCap
                            },
                            size: Some(size),
                        };
                        if failed && self.index.chunks_expired != Some(true) {
                            *file = file.clone().with_chunks(
                                self.index
                                    .return_chunks
                                    .get(&(i as u64))
                                    .copied()
                                    .unwrap_or(0),
                            );
                        }
                    } else {
                        total += size;
                    }
                }
            }
            value["files"] = serde_json::to_value(manifest.files)?;
            value["total_bytes"] = serde_json::json!(total);
            return Ok(Some(encode(&value)?));
        }
        Ok(Some(bytes))
    }
    pub(crate) fn raw_frame(&self, position: usize) -> Result<Option<Vec<u8>>> {
        self.index
            .entries
            .get(position)
            .map(|entry| self.read_entry(entry))
            .transpose()
    }
    fn delivery(&self, len: u64) -> Result<Vec<u8>> {
        ensure!(
            len <= crate::helper::FRAME_LIMIT as u64 + 4,
            "oversized terminal delivery"
        );
        let mut file = open(&self.directory, "terminal-delivery", libc::O_RDONLY)?;
        let mut bytes = vec![0; len as usize];
        file.read_exact(&mut bytes)?;
        Ok(bytes)
    }
    fn read_entry(&self, entry: &Entry) -> Result<Vec<u8>> {
        let mut bytes = Vec::new();
        for piece in &entry.pieces {
            ensure!(
                piece.len <= crate::helper::FRAME_LIMIT as u64 + 4,
                "oversized replay frame"
            );
            let mut file = open(&self.directory, &piece.name(), libc::O_RDONLY)?;
            file.seek(SeekFrom::Start(piece.offset))?;
            let old_len = bytes.len();
            ensure!(
                old_len
                    .checked_add(piece.len as usize)
                    .is_some_and(|len| len <= crate::helper::FRAME_LIMIT + 4),
                "oversized replay frame"
            );
            bytes.resize(old_len + piece.len as usize, 0);
            file.read_exact(&mut bytes[old_len..])?;
        }
        if let Some(payload) = &entry.payload {
            Ok([
                payload.prefix.as_slice(),
                serde_json::to_value(motor_protocol::BytePayload(bytes))?
                    .as_str()
                    .context("chunk encoding")?
                    .as_bytes(),
                payload.suffix.as_slice(),
            ]
            .concat())
        } else {
            Ok(bytes)
        }
    }

    pub(crate) fn append(&mut self, bytes: &[u8]) -> Result<bool> {
        self.append_delivery(bytes, None)
    }
    pub(crate) fn append_delivery(
        &mut self,
        bytes: &[u8],
        delivery: Option<&[u8]>,
    ) -> Result<bool> {
        let mut value = decode(bytes)?;
        let seq = value["seq"].as_u64();
        if seq.is_some_and(|n| self.index.last_seq.is_some_and(|last| n <= last)) {
            return Ok(false);
        }
        if self.ended() {
            return Ok(false);
        }
        let mut next = self.index.clone();
        let chunk = value["type"] == "return_chunk";
        let output = value["type"] == "output";
        if !matches!(
            value["type"].as_str(),
            Some(
                "output"
                    | "return_chunk"
                    | "return_result"
                    | "return_manifest"
                    | "started"
                    | "terminal"
            )
        ) {
            return Ok(false);
        }
        if !output
            && !chunk
            && next
                .entries
                .iter()
                .any(|e| e.kind.as_deref() == value["type"].as_str())
        {
            return Ok(false);
        }
        let id = self.next_id();
        next.next_id = Some(id + 1);
        let mut payload = None;
        let mut decoded = Vec::new();
        let mut replacement = Vec::new();
        if chunk {
            if next
                .entries
                .iter()
                .any(|entry| entry.kind.as_deref() == Some("return_manifest"))
            {
                return Ok(false);
            }
            let encoded = value
                .get("data")
                .unwrap_or(&value["bytes"])
                .as_str()
                .context("return chunk bytes")?;
            decoded = serde_json::from_value::<motor_protocol::BytePayload>(
                value.get("data").unwrap_or(&value["bytes"]).clone(),
            )?
            .0;
            let limit = next
                .return_limit
                .unwrap_or(crate::state::jobs::storage::RETURN_BYTES);
            let path = value["path_index"].as_u64().unwrap_or(0);
            ensure!(path < 64, "invalid returned path index");
            if next
                .return_bytes
                .unwrap_or(0)
                .saturating_add(decoded.len() as u64)
                > limit
                || next.failed_returns.contains_key(&path)
            {
                next.failed_returns.insert(path, true);
                next.discard(|entry| entry.return_path == Some(path));
                next.return_chunks.remove(&path);
                next.refuse(seq)?;
                self.commit(&next)?;
                self.index = next;
                self.clean_files()?;
                return Ok(false);
            }
            next.return_bytes = Some(next.return_bytes.unwrap_or(0) + decoded.len() as u64);
            *next.return_chunks.entry(path).or_default() += 1;
            let literal = serde_json::to_vec(encoded)?;
            let start = bytes
                .windows(literal.len())
                .position(|window| window == literal)
                .context("return chunk encoding")?
                + 1;
            payload = Some(ChunkPayload {
                prefix: bytes[..start].to_vec(),
                suffix: bytes[start + literal.len() - 2..].to_vec(),
            });
        } else if value["type"] == "return_result" {
            let size = value["returned_files"]
                .as_array()
                .into_iter()
                .flatten()
                .filter_map(|f| f["data"].as_str())
                .try_fold(0u64, |size, data| -> Result<u64> {
                    Ok(size
                        + serde_json::from_value::<motor_protocol::BytePayload>(Value::String(
                            data.into(),
                        ))?
                        .0
                        .len() as u64)
                })?;
            if next.return_bytes.unwrap_or(0).saturating_add(size)
                > next
                    .return_limit
                    .unwrap_or(crate::state::jobs::storage::RETURN_BYTES)
            {
                value["returned_files"] = Value::Null;
                value["return_paths_failed"] = serde_json::json!({"cause":"over_cap", "cap_bytes":next.return_limit.unwrap_or(crate::state::jobs::storage::RETURN_BYTES),"measured_bytes":size});
                replacement = encode(&value)?;
            } else {
                next.return_bytes = Some(next.return_bytes.unwrap_or(0) + size);
            }
        }
        let bytes = if replacement.is_empty() {
            bytes
        } else {
            &replacement
        };
        let (mut number, mut offset) = if !output && !chunk {
            (id, 0)
        } else {
            next.entries
                .iter()
                .rev()
                .flat_map(|e| e.pieces.iter().rev())
                .find(|p| p.chunk == chunk && p.record != Some(true))
                .map(|p| (p.number, p.offset + p.len))
                .unwrap_or((0, 0))
        };
        let mut remaining = if chunk { decoded.as_slice() } else { bytes };
        let mut pieces = Vec::new();
        while !remaining.is_empty() {
            if offset == SEGMENT_BYTES && (chunk || output) {
                number += 1;
                offset = 0;
            }
            let len = if chunk || output {
                remaining.len().min((SEGMENT_BYTES - offset) as usize)
            } else {
                remaining.len()
            };
            let piece = Piece {
                chunk,
                number,
                offset,
                len: len as u64,
                record: (!chunk && !output).then_some(true),
            };
            pieces.push(piece);
            offset += len as u64;
            remaining = &remaining[len..];
        }
        next.entries.push(Entry {
            seq,
            pieces,
            id: Some(id),
            kind: value["type"].as_str().map(str::to_owned),
            payload,
            return_path: chunk.then(|| value["path_index"].as_u64().unwrap_or(0)),
        });
        next.last_seq = seq.or(next.last_seq);
        if seq.is_some() {
            next.refused_tail = None;
        }
        next.terminal = value["type"] == "terminal";
        self.evict_output(&mut next);
        if serde_json::to_vec(&next)?.len() > 768 * 1024 && chunk {
            let mut refused = self.index.clone();
            refused
                .failed_returns
                .insert(value["path_index"].as_u64().unwrap_or(0), true);
            let path = value["path_index"].as_u64().unwrap_or(0);
            refused.discard(|entry| entry.return_path == Some(path));
            refused.return_chunks.remove(&path);
            refused.refuse(seq)?;
            refused.next_id = Some(id + 1);
            self.commit(&refused)?;
            self.index = refused;
            self.clean_files()?;
            return Ok(false);
        }
        ensure!(
            serde_json::to_vec(&next)?.len() <= 1024 * 1024,
            "replay index reservation exceeded"
        );
        let records: u64 = next
            .entries
            .iter()
            .flat_map(|e| &e.pieces)
            .filter(|p| p.record == Some(true))
            .map(|p| p.len.div_ceil(4096) * 4096)
            .sum();
        if !next.terminal && records > 2 * 1024 * 1024 {
            return Ok(false);
        }
        ensure!(
            records <= 4 * 1024 * 1024,
            "replay records reservation exceeded"
        );
        if let Some(entry) = next.entries.iter().find(|entry| entry.id == Some(id)) {
            let mut data = if chunk { decoded.as_slice() } else { bytes };
            for piece in &entry.pieces {
                self.write_piece(piece, &data[..piece.len as usize])?;
                data = &data[piece.len as usize..];
            }
        }
        if let Some(delivery) = delivery.filter(|delivery| *delivery != bytes) {
            ensure!(
                next.terminal && decode(delivery)?["type"] == "terminal",
                "invalid terminal delivery"
            );
            let mut file = match open(
                &self.directory,
                "terminal-delivery",
                libc::O_RDWR | libc::O_CREAT | libc::O_EXCL,
            ) {
                Ok(file) => file,
                Err(error)
                    if error
                        .downcast_ref::<std::io::Error>()
                        .is_some_and(|e| e.kind() == std::io::ErrorKind::AlreadyExists) =>
                {
                    open(&self.directory, "terminal-delivery", libc::O_RDWR)?
                }
                Err(error) => return Err(error),
            };
            file.set_len(0)?;
            file.write_all(delivery)?;
            file.set_len(delivery.len() as u64)?;
            file.sync_all()?;
            next.terminal_delivery = Some(delivery.len() as u64);
        }
        self.commit(&next)?;
        self.index = next;
        self.clean_files()?;
        Ok(true)
    }
    fn evict_output(&self, next: &mut Index) {
        use crate::state::jobs::storage::REPLAY_BYTES;
        loop {
            let mut segments = BTreeMap::<u64, u64>::new();
            for piece in next
                .entries
                .iter()
                .flat_map(|e| &e.pieces)
                .filter(|p| !p.chunk && p.record != Some(true))
            {
                segments
                    .entry(piece.number)
                    .and_modify(|end| *end = (*end).max(piece.offset + piece.len))
                    .or_insert(piece.offset + piece.len);
            }
            if segments.values().sum::<u64>() <= REPLAY_BYTES
                && serde_json::to_vec(next).is_ok_and(|bytes| bytes.len() <= 768 * 1024)
            {
                break;
            }
            let Some(oldest) = segments.keys().next().copied() else {
                break;
            };
            let newest = next
                .entries
                .iter()
                .rev()
                .find(|entry| entry.kind.as_deref() == Some("output"))
                .and_then(|entry| entry.id);
            next.discard(|entry| {
                entry.kind.as_deref() == Some("output")
                    && entry.id != newest
                    && entry
                        .pieces
                        .iter()
                        .any(|p| !p.chunk && p.record != Some(true) && p.number == oldest)
            });
            // Retain the newest output even when metadata pressure retires the
            // rest of its segment; its unused prefix is recorded as dropped.
            let still_pinned = next.entries.iter().any(|entry| {
                entry
                    .pieces
                    .iter()
                    .any(|p| !p.chunk && p.record != Some(true) && p.number == oldest)
            });
            if still_pinned {
                break;
            }
        }
        let earliest = next
            .entries
            .iter()
            .find(|e| e.kind.as_deref() == Some("output"))
            .and_then(|e| e.seq);
        if next.truncated_before_seq.is_some()
            || self
                .index
                .entries
                .iter()
                .filter(|e| e.kind.as_deref() == Some("output"))
                .any(|entry| !next.entries.iter().any(|kept| kept.id == entry.id))
        {
            next.truncated_before_seq = earliest
                .or(next.truncated_before_seq)
                .or_else(|| next.last_seq.map(|seq| seq.saturating_add(1)));
        }
        next.trim_dropped();
    }
    fn write_piece(&self, piece: &Piece, bytes: &[u8]) -> Result<()> {
        let mut file = open(&self.directory, &piece.name(), libc::O_RDWR | libc::O_CREAT)?;
        file.set_len(piece.offset)?;
        file.seek(SeekFrom::Start(piece.offset))?;
        file.write_all(bytes)?;
        // XFS can reserve speculative extents past EOF while appending. Trim
        // those too: admission promises allocated blocks, not just file lengths.
        file.set_len(piece.offset + piece.len)?;
        file.sync_all()?;
        Ok(())
    }
    fn commit(&self, next: &Index) -> Result<()> {
        let temporary = format!("index-{}", uuid::Uuid::from_bytes(rand::random()));
        let mut file = open(
            &self.directory,
            &temporary,
            libc::O_WRONLY | libc::O_CREAT | libc::O_EXCL,
        )?;
        serde_json::to_writer(&mut file, next)?;
        file.set_len(file.metadata()?.len())?;
        file.sync_all()?;
        #[cfg(test)]
        if self.crash_before_index {
            unsafe {
                libc::kill(libc::getpid(), libc::SIGKILL);
            }
            unreachable!();
        }
        let temporary = name(&temporary)?;
        let target = name("index.json")?;
        let fd = self.directory.as_raw_fd();
        if unsafe { libc::renameat(fd, temporary.as_ptr(), fd, target.as_ptr()) } != 0 {
            return Err(std::io::Error::last_os_error().into());
        }
        self.directory.sync_all()?;
        Ok(())
    }
}

pub(crate) fn decode(bytes: &[u8]) -> Result<Value> {
    ensure!(
        bytes.len() >= 4 && bytes.len() <= crate::helper::FRAME_LIMIT + 4,
        "invalid stored frame size"
    );
    ensure!(
        u32::from_be_bytes(bytes[..4].try_into()?) as usize == bytes.len() - 4,
        "invalid stored frame length"
    );
    Ok(serde_json::from_slice(&bytes[4..])?)
}

fn encode(value: &Value) -> Result<Vec<u8>> {
    let bytes = serde_json::to_vec(value)?;
    Ok([(bytes.len() as u32).to_be_bytes().as_slice(), &bytes].concat())
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    const JOB: &str = "00000000-0000-7000-8000-000000000001";
    fn scratch() -> tempfile::TempDir {
        let root = cortexkit_test_support::scratch_root();
        std::fs::create_dir_all(&root).unwrap();
        tempfile::tempdir_in(root).unwrap()
    }
    fn raw(text: &[u8]) -> Vec<u8> {
        [(text.len() as u32).to_be_bytes().as_slice(), text].concat()
    }
    fn frame(value: Value) -> Vec<u8> {
        raw(&serde_json::to_vec(&value).unwrap())
    }

    #[path = "bounds_tests.rs"]
    mod bounds_tests;

    #[test]
    fn raw_frames_survive_reopen_and_duplicates_do_not_enter_the_index() {
        let root = scratch();
        let frames = [
            raw(br#"{ "type":"started", "seq":0, "job_id":"00000000-0000-7000-8000-000000000001" }"#),
            raw(br#"{ "type":"output", "seq":1, "stream":"stdout", "bytes":"eA==" }"#),
            raw(br#"{ "type":"return_chunk", "seq":2, "bytes":"YWJj", "extension":"preserved" }"#),
            frame(json!({"type":"return_manifest","seq":3,"files":[]})),
            frame(json!({"type":"terminal","job_id":JOB})),
        ];
        let mut store = Store::open(root.path(), JOB).unwrap();
        for bytes in &frames {
            assert!(store.append(bytes).unwrap());
        }
        assert!(!store.append(&frames[1]).unwrap());
        drop(store);
        let store = Store::open(root.path(), JOB).unwrap();
        assert_eq!(store.len(), frames.len());
        assert!(store.ended());
        for (i, bytes) in frames.iter().enumerate() {
            assert_eq!(store.frame(i).unwrap().as_ref(), Some(bytes));
        }
        assert_eq!(
            store.cursor_frame(0, 2).unwrap(),
            Some((3, frames[2].clone()))
        );
        assert_eq!(data_files(&store.directory).unwrap().len(), 5);
        use std::os::unix::fs::PermissionsExt;
        assert_eq!(
            std::fs::metadata(root.path().join("replay").join(JOB))
                .unwrap()
                .permissions()
                .mode()
                & 0o777,
            0o700
        );
        assert_eq!(
            std::fs::metadata(
                root.path()
                    .join("replay")
                    .join(JOB)
                    .join("chunk-0000000000000000")
            )
            .unwrap()
            .permissions()
            .mode()
                & 0o777,
            0o600
        );
    }

    #[test]
    fn output_crosses_fixed_size_segments_without_changing_bytes() {
        let root = scratch();
        let bytes = frame(json!({"type":"output","seq":1,"bytes":"a".repeat(700_000)}));
        let second = frame(json!({"type":"output","seq":2,"bytes":"b".repeat(700_000)}));
        let mut store = Store::open(root.path(), JOB).unwrap();
        store.append(&bytes).unwrap();
        store.append(&second).unwrap();
        assert_eq!(
            open(&store.directory, "segment-0000000000000000", libc::O_RDONLY)
                .unwrap()
                .metadata()
                .unwrap()
                .len(),
            SEGMENT_BYTES
        );
        drop(store);
        assert_eq!(
            Store::open(root.path(), JOB).unwrap().frame(0).unwrap(),
            Some(bytes)
        );
        assert_eq!(
            Store::open(root.path(), JOB).unwrap().frame(1).unwrap(),
            Some(second)
        );
    }

    #[test]
    fn unsafe_names_symlinks_and_unknown_or_damaged_indexes_are_refused() {
        let root = scratch();
        assert!(Store::open(root.path(), "../escape").is_err());
        assert!(Store::open(root.path(), "00000000-0000-7000-8000-00000000000A").is_err());
        let mut store = Store::open(root.path(), JOB).unwrap();
        store
            .append(&frame(json!({"type":"output","seq":1})))
            .unwrap();
        let path = root.path().join("replay").join(JOB);
        drop(store);
        let index = std::fs::read(path.join("index.json")).unwrap();
        std::fs::write(path.join("index.json"), b"{broken").unwrap();
        assert!(Store::open(root.path(), JOB).is_err());
        let mut unknown: Value = serde_json::from_slice(&index).unwrap();
        unknown["version"] = json!(999);
        std::fs::write(
            path.join("index.json"),
            serde_json::to_vec(&unknown).unwrap(),
        )
        .unwrap();
        assert!(Store::open(root.path(), JOB)
            .err()
            .unwrap()
            .to_string()
            .contains("unknown replay index version"));
        std::fs::write(path.join("index.json"), &index).unwrap();
        std::fs::rename(
            path.join("segment-0000000000000000"),
            root.path().join("outside"),
        )
        .unwrap();
        std::os::unix::fs::symlink(
            root.path().join("outside"),
            path.join("segment-0000000000000000"),
        )
        .unwrap();
        assert!(Store::open(root.path(), JOB).is_err());
        std::fs::remove_file(path.join("index.json")).unwrap();
        std::os::unix::fs::symlink(root.path().join("outside"), path.join("index.json")).unwrap();
        assert!(Store::open(root.path(), JOB).is_err());
        std::fs::rename(root.path().join("replay"), root.path().join("old")).unwrap();
        std::os::unix::fs::symlink(root.path().join("old"), root.path().join("replay")).unwrap();
        assert!(Store::open(root.path(), JOB).is_err());
    }

    #[test]
    fn crash_writer() {
        let Some(path) = std::env::var_os("MOTOR_REPLAY_CRASH_ROOT") else {
            return;
        };
        let mut store = Store::open(Path::new(&path), JOB).unwrap();
        store.crash_before_index = true;
        store
            .append(&frame(
                json!({"type":"output","seq":2,"bytes":"uncommitted"}),
            ))
            .unwrap();
        panic!("crash hook did not kill writer");
    }

    #[test]
    fn kill_after_data_sync_before_index_rename_does_not_publish_the_frame() {
        let root = scratch();
        let first = frame(json!({"type":"output","seq":1,"bytes":"committed"}));
        let mut store = Store::open(root.path(), JOB).unwrap();
        store.append(&first).unwrap();
        drop(store);
        let result = std::process::Command::new(std::env::current_exe().unwrap())
            .args(["--exact", "replay::tests::crash_writer", "--nocapture"])
            .env("MOTOR_REPLAY_CRASH_ROOT", root.path())
            .status()
            .unwrap();
        use std::os::unix::process::ExitStatusExt;
        assert_eq!(result.signal(), Some(libc::SIGKILL));
        let store = Store::open(root.path(), JOB).unwrap();
        assert_eq!(store.last_seq(), Some(1));
        assert_eq!(store.len(), 1);
        assert_eq!(store.frame(0).unwrap(), Some(first.clone()));
        assert_eq!(
            open(&store.directory, "segment-0000000000000000", libc::O_RDONLY)
                .unwrap()
                .metadata()
                .unwrap()
                .len(),
            first.len() as u64
        );
    }
}
