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

const SEGMENT_BYTES: u64 = 1024 * 1024;

#[derive(Clone, Default, Deserialize, Serialize)]
struct Index {
    version: u32,
    entries: Vec<Entry>,
    last_seq: Option<u64>,
    terminal: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    terminal_delivery: Option<u64>,
}
#[derive(Clone, Deserialize, Serialize)]
struct Entry {
    seq: Option<u64>,
    pieces: Vec<Piece>,
}
#[derive(Clone, Deserialize, Serialize)]
struct Piece {
    chunk: bool,
    number: u64,
    offset: u64,
    len: u64,
}
impl Piece {
    fn name(&self) -> String {
        format!(
            "{}-{:016x}",
            if self.chunk { "chunk" } else { "segment" },
            self.number
        )
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
        if file == "terminal-delivery" {
            files.push(file.into_owned());
        } else if let Some(number) = file
            .strip_prefix("segment-")
            .or_else(|| file.strip_prefix("chunk-"))
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
        let store = Self {
            directory,
            index,
            #[cfg(test)]
            crash_before_index: false,
        };
        let mut ends = BTreeMap::<String, u64>::new();
        let mut last = None;
        let mut terminal = false;
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
                    piece.chunk || end <= SEGMENT_BYTES,
                    "oversized replay segment"
                );
                let previous = ends.entry(piece.name()).or_default();
                ensure!(*previous == piece.offset, "noncontiguous replay index");
                *previous = end;
            }
            let bytes = store.read_entry(entry)?;
            let value = decode(&bytes)?;
            ensure!(
                value["seq"].as_u64() == entry.seq,
                "replay sequence mismatch"
            );
            terminal = value["type"] == "terminal";
        }
        ensure!(
            store.index.last_seq == last && store.index.terminal == terminal,
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
        for file in data_files(&store.directory)? {
            let end = ends.get(&file).copied().unwrap_or(0);
            let file = open(&store.directory, &file, libc::O_RDWR)?;
            file.set_len(end)?;
            file.sync_all()?;
        }
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
    pub(crate) fn cursor_for_seq(&self, seq: u64) -> usize {
        self.index
            .entries
            .iter()
            .position(|e| e.seq.is_none_or(|n| n >= seq))
            .unwrap_or(self.len())
    }
    pub(crate) fn frame(&self, position: usize) -> Result<Option<Vec<u8>>> {
        if position + 1 == self.len() && self.index.terminal {
            if let Some(len) = self.index.terminal_delivery {
                return Ok(Some(self.delivery(len)?));
            }
        }
        self.raw_frame(position)
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
        Ok(bytes)
    }

    pub(crate) fn append(&mut self, bytes: &[u8]) -> Result<bool> {
        self.append_delivery(bytes, None)
    }
    pub(crate) fn append_delivery(
        &mut self,
        bytes: &[u8],
        delivery: Option<&[u8]>,
    ) -> Result<bool> {
        let value = decode(bytes)?;
        let seq = value["seq"].as_u64();
        if seq.is_some_and(|n| self.index.last_seq.is_some_and(|last| n <= last)) {
            return Ok(false);
        }
        if self.ended() {
            return Ok(false);
        }
        let mut next = self.index.clone();
        let chunk = value["type"] == "return_chunk";
        let (mut number, mut offset) = if chunk {
            (next.entries.len() as u64, 0)
        } else {
            next.entries
                .iter()
                .rev()
                .flat_map(|e| e.pieces.iter().rev())
                .find(|p| !p.chunk)
                .map(|p| (p.number, p.offset + p.len))
                .unwrap_or((0, 0))
        };
        let mut remaining = bytes;
        let mut pieces = Vec::new();
        while !remaining.is_empty() {
            if !chunk && offset == SEGMENT_BYTES {
                number += 1;
                offset = 0;
            }
            let len = if chunk {
                remaining.len()
            } else {
                remaining.len().min((SEGMENT_BYTES - offset) as usize)
            };
            let piece = Piece {
                chunk,
                number,
                offset,
                len: len as u64,
            };
            let mut file = match open(
                &self.directory,
                &piece.name(),
                libc::O_RDWR | libc::O_CREAT | libc::O_EXCL,
            ) {
                Ok(file) => file,
                Err(error)
                    if error
                        .downcast_ref::<std::io::Error>()
                        .is_some_and(|e| e.kind() == std::io::ErrorKind::AlreadyExists) =>
                {
                    open(&self.directory, &piece.name(), libc::O_RDWR)?
                }
                Err(error) => return Err(error),
            };
            file.set_len(offset)?;
            file.seek(SeekFrom::Start(offset))?;
            file.write_all(&remaining[..len])?;
            file.sync_all()?;
            pieces.push(piece);
            offset += len as u64;
            remaining = &remaining[len..];
        }
        next.entries.push(Entry { seq, pieces });
        next.last_seq = seq.or(next.last_seq);
        next.terminal = value["type"] == "terminal";
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
            file.sync_all()?;
            next.terminal_delivery = Some(delivery.len() as u64);
        }
        // Apply output eviction, the admitted returned-file byte limit and file
        // expiry at this commit boundary, before publishing a new index. These
        // limits are separate from the frame store's durability guarantees.
        self.commit(&next)?;
        self.index = next;
        Ok(true)
    }
    fn commit(&self, next: &Index) -> Result<()> {
        let temporary = format!("index-{}", uuid::Uuid::from_bytes(rand::random()));
        let mut file = open(
            &self.directory,
            &temporary,
            libc::O_WRONLY | libc::O_CREAT | libc::O_EXCL,
        )?;
        serde_json::to_writer(&mut file, next)?;
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
        assert_eq!(store.cursor_for_seq(2), 2);
        assert_eq!(data_files(&store.directory).unwrap().len(), 2);
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
                    .join("chunk-0000000000000002")
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
