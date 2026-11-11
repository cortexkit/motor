//! Host-owned bundle generations. No guest filesystem image is opened here.
use crate::{
    helper::{self, Control},
    state::Host,
};
use anyhow::{ensure, Context, Result};
use motor_protocol::{FillSource, Frame, FILL_SOURCE_EXPORT_TIMEOUT_S, FILL_SOURCE_MAX_BYTES};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::{
    fs::{self, File, OpenOptions},
    io::{Read, Write},
    path::{Path, PathBuf},
    sync::{Arc, Mutex},
    time::{Duration, Instant},
};

pub const REFILL_INTERVAL_MS: u64 = 6 * 60 * 60 * 1000;

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Request {
    pub account: String,
    pub repository_root: String,
    pub integration_tip: String,
    pub mem_mib: u64,
    pub observed_ms: u64,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Slot {
    pub request: Request,
    pub generation: String,
    pub source: FillSource,
    pub bytes: u64,
    pub last_fill_started_ms: Option<u64>,
}

pub fn repository_key(root: &str) -> String {
    format!("{:x}", Sha256::digest(root.as_bytes()))
}
pub fn key(account: &str, root: &str) -> String {
    format!(
        "{:x}",
        Sha256::digest(format!("{account}\0{root}").as_bytes())
    )
}
pub fn directory(journal: &Path, generation: &str) -> PathBuf {
    journal.join("fill-sources").join(generation)
}

pub(crate) fn remove_unreferenced(
    journal: &Path,
    slots: &std::collections::BTreeMap<String, Slot>,
) -> std::io::Result<()> {
    let root = journal.join("fill-sources");
    let entries = match fs::read_dir(&root) {
        Ok(entries) => entries,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(error) => return Err(error),
    };
    for entry in entries {
        let entry = entry?;
        if !slots
            .values()
            .any(|slot| entry.file_name() == slot.generation.as_str())
        {
            if entry.file_type()?.is_dir() {
                fs::remove_dir_all(entry.path())?;
            } else {
                fs::remove_file(entry.path())?;
            }
        }
    }
    File::open(root)?.sync_all()
}

pub struct Incoming {
    path: PathBuf,
    source: FillSource,
    current: usize,
    offset: u64,
    hash: Sha256,
    file: Option<File>,
}
impl Incoming {
    pub fn new(path: PathBuf, source: FillSource) -> Result<Self> {
        source.validate(FILL_SOURCE_MAX_BYTES)?;
        fs::create_dir_all(path.parent().context("source parent")?)?;
        fs::create_dir(&path)?;
        let mut result = Self {
            path,
            source,
            current: 0,
            offset: 0,
            hash: Sha256::new(),
            file: None,
        };
        result.open()?;
        Ok(result)
    }
    fn open(&mut self) -> Result<()> {
        if self.current < self.source.bundles.len() {
            self.file = Some(
                OpenOptions::new()
                    .write(true)
                    .create_new(true)
                    .open(self.path.join(self.current.to_string()))?,
            );
        }
        Ok(())
    }
    pub fn complete(&self) -> bool {
        self.current == self.source.bundles.len()
    }
    pub fn chunk(&mut self, frame: Frame) -> Result<()> {
        let Frame::BundleChunk {
            index,
            offset,
            data,
        } = frame
        else {
            anyhow::bail!("expected source chunk")
        };
        ensure!(
            !self.complete()
                && index as usize == self.current
                && offset == self.offset
                && !data.0.is_empty(),
            "noncontiguous source chunk"
        );
        let bundle = &self.source.bundles[self.current];
        ensure!(
            data.0.len() as u64 <= bundle.length - self.offset,
            "source chunk overrun"
        );
        self.file
            .as_mut()
            .context("source file")?
            .write_all(&data.0)?;
        self.hash.update(&data.0);
        self.offset += data.0.len() as u64;
        if self.offset == bundle.length {
            ensure!(
                format!("{:x}", self.hash.clone().finalize()) == bundle.sha256,
                "source digest mismatch"
            );
            self.file.take().unwrap().sync_all()?;
            self.current += 1;
            self.offset = 0;
            self.hash = Sha256::new();
            self.open()?;
        }
        Ok(())
    }
    pub fn publish(self, journal: &Path, generation: &str) -> Result<()> {
        ensure!(self.complete(), "incomplete source");
        File::open(&self.path)?.sync_all()?;
        let published = directory(journal, generation);
        fs::rename(&self.path, &published)?;
        if let Err(error) =
            File::open(journal.join("fill-sources")).and_then(|file| file.sync_all())
        {
            if let Err(cleanup) = fs::remove_dir_all(&published) {
                tracing::error!(%cleanup, "fill_source_failed_publish_cleanup");
            }
            return Err(error.into());
        }
        Ok(())
    }
}
impl Drop for Incoming {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.path);
    }
}

impl Host {
    pub(crate) fn note_fill_source(
        &mut self,
        job: &str,
        run: &motor_protocol::Run,
        now_ms: u64,
    ) -> Result<()> {
        if run.integration_tip.is_empty() {
            return Ok(());
        }
        let execution = self
            .snapshot
            .per_job
            .executions
            .get(job)
            .context("export execution")?;
        let request = Request {
            account: execution.claims.sub.clone(),
            repository_root: run.repository_root.clone(),
            integration_tip: run.integration_tip.clone(),
            mem_mib: execution
                .claims
                .mem_mib
                .max(crate::state::jobs::fills::MEMORY_FLOOR_MIB),
            observed_ms: now_ms,
        };
        let slot_key = key(&request.account, &request.repository_root);
        let due = self
            .snapshot
            .per_job
            .fill_sources
            .get(&slot_key)
            .is_none_or(|slot| {
                slot.last_fill_started_ms
                    .is_none_or(|at| now_ms.saturating_sub(at.min(now_ms)) >= REFILL_INTERVAL_MS)
                    && (slot.request.integration_tip != request.integration_tip
                        || slot.last_fill_started_ms.is_none())
            });
        if !due
            || self
                .snapshot
                .per_job
                .export_tips
                .values()
                .any(|r| key(&r.account, &r.repository_root) == slot_key)
        {
            return Ok(());
        }
        let mut next = self.snapshot.clone();
        next.per_job.export_tips.insert(job.into(), request);
        // Reserve the entire replacement alongside the retained older slot.
        // No bytes are read from the guest unless that promise was admitted.
        if self
            .reserve_storage(&mut next, &execution.disk_id.clone(), now_ms)
            .is_err()
        {
            tracing::warn!(jti = job, "fill_export_refused_storage");
            return Ok(());
        }
        self.save(next)?;
        Ok(())
    }
}

pub fn export(
    host: &Arc<Mutex<Host>>,
    control: &dyn Control,
    job: &str,
    deadline: Instant,
) -> Result<()> {
    export_with(host, control, job, deadline, || {})
}

#[cfg(test)]
pub fn export_before_commit(
    host: &Arc<Mutex<Host>>,
    control: &dyn Control,
    job: &str,
    deadline: Instant,
    before_commit: impl FnOnce(),
) -> Result<()> {
    export_with(host, control, job, deadline, before_commit)
}

struct UncommittedGeneration(Option<PathBuf>);
impl Drop for UncommittedGeneration {
    fn drop(&mut self) {
        if let Some(path) = &self.0 {
            if let Err(error) = fs::remove_dir_all(path) {
                tracing::error!(%error, "fill_source_failed_publish_cleanup");
            }
        }
    }
}

fn export_with(
    host: &Arc<Mutex<Host>>,
    control: &dyn Control,
    job: &str,
    deadline: Instant,
    before_commit: impl FnOnce(),
) -> Result<()> {
    let (request, journal) = {
        let host = host.lock().expect("host lock");
        let Some(request) = host.snapshot.per_job.export_tips.get(job).cloned() else {
            return Ok(());
        };
        (
            request,
            host.journal_directory().context("fill source journal")?,
        )
    };
    let result = (|| -> Result<()> {
        ensure!(Instant::now() < deadline, "fill export timed out");
        let mut socket = control.job_relay(job, 54)?;
        socket.set_write_timeout(Some(Duration::from_secs(FILL_SOURCE_EXPORT_TIMEOUT_S)))?;
        socket.set_read_timeout(Some(Duration::from_secs(FILL_SOURCE_EXPORT_TIMEOUT_S)))?;
        helper::write_json(
            &mut socket,
            &serde_json::json!({"op":"export_tip","job_id":job,"max_bytes":FILL_SOURCE_MAX_BYTES}),
        )?;
        let reply = helper::read_json(&mut Deadline {
            socket: &mut socket,
            deadline,
        })?;
        ensure!(
            reply["ok"] == true,
            "guest export refused: {}",
            reply["error"]
        );
        let source: FillSource = serde_json::from_value(reply["source"].clone())?;
        ensure!(
            source.repository_root == request.repository_root
                && source.integration_tip == request.integration_tip,
            "export identity mismatch"
        );
        let bytes = source.validate(FILL_SOURCE_MAX_BYTES)?;
        let generation = uuid::Uuid::now_v7().to_string();
        let mut incoming = Incoming::new(
            directory(&journal, &format!("incoming-{generation}")),
            source.clone(),
        )?;
        while !incoming.complete() {
            let remaining = deadline
                .checked_duration_since(Instant::now())
                .context("fill export timed out")?;
            socket.set_read_timeout(Some(remaining))?;
            incoming.chunk(
                motor_protocol::read_frame(&mut Deadline {
                    socket: &mut socket,
                    deadline,
                })?
                .context("incomplete source")?,
            )?;
        }
        before_commit();
        let mut host = host.lock().expect("host lock");
        ensure!(Instant::now() < deadline, "fill export timed out");
        ensure!(
            host.snapshot.per_job.export_tips.get(job) == Some(&request),
            "fill export expired"
        );
        let mut next = host.snapshot.clone();
        let slot_key = key(&request.account, &request.repository_root);
        let old = next.per_job.fill_sources.get(&slot_key).cloned();
        if old
            .as_ref()
            .is_some_and(|old| old.request.observed_ms > request.observed_ms)
        {
            return Ok(());
        }
        // The reservation and deadline must still hold when the generation is
        // renamed. Until the journal commits, every failure removes its files.
        incoming.publish(&journal, &generation)?;
        let mut unpublished = UncommittedGeneration(Some(directory(&journal, &generation)));
        ensure!(Instant::now() < deadline, "fill export timed out");
        next.per_job.fill_sources.insert(
            slot_key,
            Slot {
                request: request.clone(),
                generation,
                source,
                bytes,
                last_fill_started_ms: old.as_ref().and_then(|s| s.last_fill_started_ms),
            },
        );
        host.save(next)?;
        unpublished.0 = None;
        tracing::info!(
            jti = job,
            tip = request.integration_tip,
            bundle_bytes = bytes,
            "fill_source_published"
        );
        if let Some(old) = old {
            let _ = fs::remove_dir_all(directory(&journal, &old.generation));
        }
        Ok(())
    })();
    let mut host = host.lock().expect("host lock");
    let mut next = host.snapshot.clone();
    next.per_job.export_tips.remove(job);
    host.save(next)?;
    result
}

struct Deadline<'a> {
    socket: &'a mut std::os::unix::net::UnixStream,
    deadline: Instant,
}
impl Read for Deadline<'_> {
    fn read(&mut self, buffer: &mut [u8]) -> std::io::Result<usize> {
        let remaining = self
            .deadline
            .checked_duration_since(Instant::now())
            .filter(|d| !d.is_zero())
            .ok_or_else(|| {
                std::io::Error::new(std::io::ErrorKind::TimedOut, "fill export timed out")
            })?;
        self.socket.set_read_timeout(Some(remaining))?;
        self.socket.read(buffer)
    }
}

impl Host {
    pub(crate) fn clean_fill_sources(&mut self) -> Result<()> {
        let mut next = self.snapshot.clone();
        next.per_job
            .export_tips
            .retain(|job, _| next.per_job.executions.get(job).is_some_and(|e| !e.ended));
        if next != self.snapshot {
            self.save(next)?;
        }
        if !self.snapshot.per_job.export_tips.is_empty() {
            return Ok(());
        }
        let Some(journal) = self.journal_directory() else {
            return Ok(());
        };
        remove_unreferenced(&journal, &self.snapshot.per_job.fill_sources)?;
        Ok(())
    }
}

pub fn send(socket: &mut impl Write, source: &FillSource, files: &mut [File]) -> Result<()> {
    for (bundle, file) in source.bundles.iter().zip(files) {
        let mut offset = 0;
        let mut buffer = vec![0; 65536];
        loop {
            let n = file.read(&mut buffer)?;
            if n == 0 {
                break;
            }
            motor_protocol::write_frame(
                socket,
                &Frame::BundleChunk {
                    index: bundle.index,
                    offset,
                    data: motor_protocol::BytePayload(buffer[..n].to_vec()),
                },
            )?;
            offset += n as u64;
        }
    }
    Ok(())
}
