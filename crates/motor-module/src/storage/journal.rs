//! Record-sized durable transactions with legacy-readable JSON checkpoints.
use super::State;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use sha2::{Digest, Sha256};
use std::{
    collections::BTreeMap,
    fs::{self, File, OpenOptions},
    io::{self, Write},
    path::{Path, PathBuf},
    sync::{
        atomic::{AtomicU64, Ordering},
        Arc,
    },
    time::{Duration, Instant},
};
use uuid::Uuid;

#[cfg(test)]
#[path = "journal_delta_tests.rs"]
mod delta_tests;

pub(super) const SOFT_BYTES: u64 = 1024 * 1024;
pub(super) const HARD_BYTES: u64 = 8 * SOFT_BYTES;
pub(super) const MAX_AGE: Duration = Duration::from_secs(5 * 60);

#[derive(Default)]
pub(super) struct Metrics {
    pub append_bytes: AtomicU64,
    pub checkpoint_bytes: AtomicU64,
    pub syncs: AtomicU64,
}

fn write(file: &mut File, mut bytes: &[u8], counter: &AtomicU64) -> io::Result<()> {
    while !bytes.is_empty() {
        let n = file.write(bytes)?;
        if n == 0 {
            return Err(io::ErrorKind::WriteZero.into());
        }
        counter.fetch_add(n as u64, Ordering::Relaxed);
        bytes = &bytes[n..];
    }
    Ok(())
}

fn invalid(message: impl Into<String>) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, message.into())
}

#[derive(Clone, Serialize, Deserialize)]
pub(super) struct Position {
    version: u8,
    epoch: Uuid,
    seq: u64,
    #[serde(default)]
    checkpoint_at_ms: u64,
}

// All durable maps participate, including deletions caused by retention pruning.
#[derive(Serialize, Deserialize)]
struct Delta(BTreeMap<String, BTreeMap<String, Option<Value>>>);
impl Delta {
    fn between(before: &State, after: &State) -> io::Result<Self> {
        let mut delta = BTreeMap::new();
        Self::map_changes(&mut delta, "jobs", &before.jobs, &after.jobs)?;
        Self::map_changes(&mut delta, "snapshots", &before.snapshots, &after.snapshots)?;
        Self::map_changes(
            &mut delta,
            "snapshot_used_at_ms",
            &before.snapshot_used_at_ms,
            &after.snapshot_used_at_ms,
        )?;
        Self::map_changes(
            &mut delta,
            "verified_tips",
            &before.verified_tips,
            &after.verified_tips,
        )?;
        Ok(Self(delta))
    }

    fn map_changes<K: Ord + ToString, V: PartialEq + Serialize>(
        delta: &mut BTreeMap<String, BTreeMap<String, Option<Value>>>,
        name: &str,
        before: &BTreeMap<K, V>,
        after: &BTreeMap<K, V>,
    ) -> io::Result<()> {
        let mut changes = BTreeMap::new();
        // Compare stored records without allocating JSON trees for unchanged entries.
        for (key, value) in after {
            if before.get(key) != Some(value) {
                changes.insert(
                    key.to_string(),
                    Some(serde_json::to_value(value).map_err(io::Error::other)?),
                );
            }
        }
        for key in before.keys() {
            if !after.contains_key(key) {
                changes.insert(key.to_string(), None);
            }
        }
        if !changes.is_empty() {
            delta.insert(name.to_owned(), changes);
        }
        Ok(())
    }
    fn apply(self, state: &mut Value) -> io::Result<()> {
        for (name, changes) in self.0 {
            if !["jobs", "snapshots", "snapshot_used_at_ms", "verified_tips"]
                .contains(&name.as_str())
            {
                return Err(invalid("unknown journal map"));
            }
            let map = state
                .as_object_mut()
                .ok_or_else(|| invalid("invalid checkpoint"))?
                .entry(name)
                .or_insert_with(|| Value::Object(Default::default()))
                .as_object_mut()
                .ok_or_else(|| invalid("invalid journal map"))?;
            for (key, value) in changes {
                if let Some(value) = value {
                    map.insert(key, value);
                } else {
                    map.remove(&key);
                }
            }
        }
        Ok(())
    }
}

#[derive(Serialize, Deserialize)]
struct Frame {
    epoch: Uuid,
    seq: u64,
    delta: Delta,
}
struct Segment {
    path: PathBuf,
    first: u64,
    bytes: u64,
}

pub(super) struct Journal {
    position: Position,
    checkpoint_seq: u64,
    file: File,
    segments: Vec<Segment>,
    pub last_checkpoint: Instant,
    pub poisoned: bool,
}

pub(super) enum Backend {
    Legacy,
    Journal(Journal),
}
impl Backend {
    pub fn pending(&self) -> bool {
        matches!(self, Self::Journal(j) if j.position.seq != j.checkpoint_seq)
    }
    pub fn bytes(&self) -> u64 {
        match self {
            Self::Legacy => 0,
            Self::Journal(j) => j.segments.iter().map(|s| s.bytes).sum(),
        }
    }
    pub fn due(&self) -> bool {
        match self {
            Self::Legacy => false,
            Self::Journal(j) => {
                self.pending()
                    && (self.bytes() >= SOFT_BYTES || j.last_checkpoint.elapsed() >= MAX_AGE)
            }
        }
    }
    pub fn append(
        &mut self,
        root: &Path,
        before: &State,
        after: &State,
        metrics: &Metrics,
    ) -> io::Result<()> {
        let Self::Journal(j) = self else {
            return snapshot(root, after, None, metrics);
        };
        if j.poisoned {
            return Err(io::Error::other(
                "storage journal requires reopening after failed append",
            ));
        }
        let delta = Delta::between(before, after)?;
        j.append_delta(delta, metrics)
    }
}

impl Journal {
    fn append_delta(&mut self, delta: Delta, metrics: &Metrics) -> io::Result<()> {
        if delta.0.is_empty() {
            return Ok(());
        }
        let seq = self
            .position
            .seq
            .checked_add(1)
            .ok_or_else(|| invalid("journal sequence exhausted"))?;
        let payload = serde_json::to_vec(&Frame {
            epoch: self.position.epoch,
            seq,
            delta,
        })
        .map_err(io::Error::other)?;
        let mut bytes = format!("{:x}\t", Sha256::digest(&payload)).into_bytes();
        bytes.extend(payload);
        bytes.push(b'\n');
        let old_len = self.segments.last().unwrap().bytes;
        let result = write(&mut self.file, &bytes, &metrics.append_bytes)
            .and_then(|()| self.file.sync_all());
        if let Err(error) = result {
            // Never put a valid later frame behind an uncertain append.
            if self
                .file
                .set_len(old_len)
                .and_then(|()| self.file.sync_all())
                .is_err()
            {
                self.poisoned = true;
            }
            return Err(error);
        }
        metrics.syncs.fetch_add(1, Ordering::Relaxed);
        self.position.seq = seq;
        self.segments.last_mut().unwrap().bytes += bytes.len() as u64;
        Ok(())
    }
}

pub(super) fn snapshot(
    root: &Path,
    state: &State,
    position: Option<&Position>,
    metrics: &Metrics,
) -> io::Result<()> {
    let mut value = serde_json::to_value(state).map_err(io::Error::other)?;
    if let Some(position) = position {
        value.as_object_mut().unwrap().insert(
            "_journal".into(),
            serde_json::to_value(position).map_err(io::Error::other)?,
        );
    }
    let path = root.join("state.tmp");
    let mut file = File::create(&path)?;
    write(
        &mut file,
        &serde_json::to_vec(&value).map_err(io::Error::other)?,
        &metrics.checkpoint_bytes,
    )?;
    file.sync_all()?;
    fs::rename(path, root.join("state.json"))?;
    File::open(root)?.sync_all()?;
    metrics.syncs.fetch_add(2, Ordering::Relaxed);
    Ok(())
}

fn create_segment(root: &Path, epoch: Uuid, first: u64) -> io::Result<(File, Segment)> {
    let path = root.join(format!("journal-{epoch}-{first:020}.jsonl"));
    let file = OpenOptions::new()
        .create_new(true)
        .read(true)
        .append(true)
        .open(&path)?;
    file.sync_all()?;
    File::open(root)?.sync_all()?;
    Ok((
        file,
        Segment {
            path,
            first,
            bytes: 0,
        },
    ))
}

pub(super) fn open(root: &Path, metrics: &Arc<Metrics>) -> io::Result<(State, Backend)> {
    let mut value = match fs::read(root.join("state.json")) {
        Ok(bytes) => serde_json::from_slice::<Value>(&bytes).map_err(io::Error::other)?,
        Err(e) if e.kind() == io::ErrorKind::NotFound => {
            serde_json::to_value(State::default()).map_err(io::Error::other)?
        }
        Err(e) => return Err(e),
    };
    let position = value
        .get("_journal")
        .cloned()
        .map(serde_json::from_value::<Position>)
        .transpose()
        .map_err(io::Error::other)?;
    if let Some(position) = position {
        if position.version != 1 {
            return Err(invalid("unsupported storage journal version"));
        }
        let checkpoint_seq = position.seq;
        let mut j = recover(root, &mut value, position)?;
        j.checkpoint_seq = checkpoint_seq;
        let state = serde_json::from_value(value).map_err(io::Error::other)?;
        Ok((state, Backend::Journal(j)))
    } else {
        let state: State = serde_json::from_value(value).map_err(io::Error::other)?;
        // An old writer drops unknown checkpoint metadata. Its JSON is authoritative;
        // stale journal epochs must never be replayed over its subsequent changes.
        let epoch = Uuid::now_v7();
        let migrated = (|| {
            let (file, segment) = create_segment(root, epoch, 1)?;
            let position = Position {
                version: 1,
                epoch,
                seq: 0,
                checkpoint_at_ms: super::now_ms(),
            };
            snapshot(root, &state, Some(&position), metrics)?;
            if let Err(error) = remove_other_epochs(root, epoch) {
                tracing::warn!(%error, "could not remove obsolete storage journal epochs");
            }
            Ok::<_, io::Error>(Backend::Journal(Journal {
                position,
                checkpoint_seq: 0,
                file,
                segments: vec![segment],
                last_checkpoint: Instant::now(),
                poisoned: false,
            }))
        })();
        let backend = migrated.unwrap_or_else(|error| {
            tracing::warn!(%error, "journal migration failed; continuing with synchronous JSON storage");
            Backend::Legacy
        });
        Ok((state, backend))
    }
}

fn segments(root: &Path, epoch: Uuid) -> io::Result<Vec<Segment>> {
    let prefix = format!("journal-{epoch}-");
    let mut segments = Vec::new();
    for entry in fs::read_dir(root)? {
        let entry = entry?;
        let name = entry.file_name();
        let Some(name) = name.to_str() else { continue };
        if let Some(first) = name
            .strip_prefix(&prefix)
            .and_then(|s| s.strip_suffix(".jsonl"))
        {
            let first = first
                .parse()
                .map_err(|_| invalid("invalid journal segment name"))?;
            segments.push(Segment {
                path: entry.path(),
                first,
                bytes: entry.metadata()?.len(),
            });
        }
    }
    segments.sort_by_key(|s| s.first);
    Ok(segments)
}

fn replay(
    root: &Path,
    value: &mut Value,
    mut position: Position,
    repair: bool,
) -> io::Result<(Position, Vec<Segment>)> {
    let checkpoint_seq = position.seq;
    let mut segments = segments(root, position.epoch)?;
    let count = segments.len();
    for (index, segment) in segments.iter_mut().enumerate() {
        if segment.first <= checkpoint_seq {
            continue;
        }
        if segment.first
            != position
                .seq
                .checked_add(1)
                .ok_or_else(|| invalid("journal sequence exhausted"))?
        {
            return Err(invalid("gap between journal segments"));
        }
        let bytes = fs::read(&segment.path)?;
        let mut valid = 0;
        for line in bytes.split_inclusive(|b| *b == b'\n') {
            let last = index + 1 == count && valid + line.len() == bytes.len();
            let payload = line.get(65..line.len().saturating_sub(1));
            let valid_checksum = line.last() == Some(&b'\n')
                && line.get(64) == Some(&b'\t')
                && payload
                    .is_some_and(|p| format!("{:x}", Sha256::digest(p)).as_bytes() == &line[..64]);
            if !valid_checksum {
                if !last {
                    return Err(invalid("corrupt interior journal frame"));
                }
                if repair {
                    let file = OpenOptions::new().write(true).open(&segment.path)?;
                    file.set_len(valid as u64)?;
                    file.sync_all()?;
                }
                break;
            }
            let frame: Frame =
                serde_json::from_slice(payload.unwrap()).map_err(io::Error::other)?;
            if frame.epoch != position.epoch || Some(frame.seq) != position.seq.checked_add(1) {
                return Err(invalid("journal epoch or sequence mismatch"));
            }
            frame.delta.apply(value)?;
            position.seq = frame.seq;
            valid += line.len();
        }
        segment.bytes = valid as u64;
    }
    Ok((position, segments))
}

fn recover(root: &Path, value: &mut Value, position: Position) -> io::Result<Journal> {
    let checkpoint_seq = position.seq;
    let (position, mut segments) = replay(root, value, position, true)?;
    let file = if let Some(segment) = segments.last().filter(|s| s.first > checkpoint_seq) {
        OpenOptions::new()
            .read(true)
            .append(true)
            .open(&segment.path)?
    } else {
        let first = position
            .seq
            .checked_add(1)
            .ok_or_else(|| invalid("journal sequence exhausted"))?;
        let (file, segment) = create_segment(root, position.epoch, first)?;
        segments.push(segment);
        file
    };
    let age = Duration::from_millis(super::now_ms().saturating_sub(position.checkpoint_at_ms))
        .min(MAX_AGE);
    let mut journal = Journal {
        position,
        checkpoint_seq,
        file,
        segments,
        last_checkpoint: Instant::now() - age,
        poisoned: false,
    };
    journal.remove_covered(root)?;
    Ok(journal)
}

impl Journal {
    #[cfg(test)]
    pub fn set_append_readonly(&mut self, readonly: bool) {
        self.file = OpenOptions::new()
            .read(true)
            .append(!readonly)
            .open(&self.segments.last().unwrap().path)
            .unwrap();
        self.poisoned = false;
    }
    pub fn begin_checkpoint(&mut self, root: &Path) -> io::Result<Option<Position>> {
        if self.poisoned {
            return Err(io::Error::other(
                "cannot checkpoint an uncertain journal append",
            ));
        }
        if self.position.seq == self.checkpoint_seq {
            self.remove_covered(root)?;
            self.last_checkpoint = Instant::now();
            return Ok(None);
        }
        // Rotate before releasing the state mutex. New transitions go only to
        // the suffix, so publishing the captured snapshot never rewrites them.
        if self.segments.last().unwrap().bytes != 0 {
            let first = self
                .position
                .seq
                .checked_add(1)
                .ok_or_else(|| invalid("journal sequence exhausted"))?;
            let (file, segment) = match create_segment(root, self.position.epoch, first) {
                Ok(segment) => segment,
                Err(error) => {
                    // Creation may have left an empty segment on disk. Do not
                    // append later sequences to the old segment behind it.
                    self.poisoned = true;
                    return Err(error);
                }
            };
            self.file = file;
            self.segments.push(segment);
        }
        let mut position = self.position.clone();
        position.checkpoint_at_ms = super::now_ms();
        Ok(Some(position))
    }
    pub fn finish_checkpoint(&mut self, root: &Path, position: &Position) -> io::Result<()> {
        self.checkpoint_seq = position.seq;
        self.last_checkpoint = Instant::now();
        self.remove_covered(root)
    }
    fn remove_covered(&mut self, root: &Path) -> io::Result<()> {
        let mut error = None;
        self.segments.retain(|s| {
            if s.first > self.checkpoint_seq {
                return true;
            }
            match fs::remove_file(&s.path) {
                Ok(()) => false,
                Err(e) if e.kind() == io::ErrorKind::NotFound => false,
                Err(e) => {
                    error = Some(e);
                    true
                }
            }
        });
        File::open(root)?.sync_all()?;
        error.map_or(Ok(()), Err)
    }
}

// Tests inspect bytes already on disk at the network-effect boundary, not a
// clone of the live in-memory state or a shutdown-generated checkpoint.
#[cfg(test)]
pub(crate) fn durable_value(root: &Path) -> io::Result<Value> {
    let mut value: Value =
        serde_json::from_slice(&fs::read(root.join("state.json"))?).map_err(io::Error::other)?;
    if let Some(position) = value.get("_journal").cloned() {
        replay(
            root,
            &mut value,
            serde_json::from_value(position).map_err(io::Error::other)?,
            false,
        )?;
    }
    Ok(value)
}

fn remove_other_epochs(root: &Path, epoch: Uuid) -> io::Result<()> {
    for entry in fs::read_dir(root)? {
        let entry = entry?;
        let name = entry.file_name();
        let Some((old, sequence)) = name
            .to_str()
            .and_then(|n| n.strip_prefix("journal-"))
            .and_then(|n| n.strip_suffix(".jsonl"))
            .and_then(|n| n.rsplit_once('-'))
        else {
            continue;
        };
        if sequence.parse::<u64>().is_ok() && Uuid::parse_str(old).is_ok_and(|old| old != epoch) {
            fs::remove_file(entry.path())?;
        }
    }
    File::open(root)?.sync_all()
}
