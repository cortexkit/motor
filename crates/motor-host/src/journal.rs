use crate::state::Snapshot;
use std::collections::BTreeSet;
use std::fs::{self, File, OpenOptions};
use std::io::{self, Write};
use std::path::{Path, PathBuf};

pub trait Journal: Send {
    fn store(&mut self, snapshot: &Snapshot) -> io::Result<()>;
}

/// A rename publishes a complete snapshot; syncing the directory makes the
/// rename durable as well as the contents. No credential or signature is stored.
///
/// Every save rewrites the whole state, so this suits only a small state that
/// stays small: the in-sandbox endpoint, which serves one job. A registered
/// host keeps its state in SQLite ([`crate::store::SqliteJournal`]) and uses
/// this format only to import an older host's state and to export it for a
/// rollback.
pub struct FileJournal {
    path: PathBuf,
    /// Names in `outbox-expired`, once read; see `remove_unlisted_archives`.
    archived: Option<BTreeSet<String>>,
}

impl FileJournal {
    pub fn open(directory: &Path) -> io::Result<(Self, Snapshot)> {
        fs::create_dir_all(directory)?;
        let path = directory.join("journal.json");
        let snapshot = match fs::read(&path) {
            Ok(bytes) => serde_json::from_slice(&bytes).map_err(io::Error::other)?,
            Err(e) if e.kind() == io::ErrorKind::NotFound => Snapshot::default(),
            Err(e) => return Err(e),
        };
        Ok((
            Self {
                path,
                archived: None,
            },
            snapshot,
        ))
    }
}

/// Archived requests are named by creation time and a digest of their route
/// and body, never by queue position: retention deletes requests from the
/// front of the set-aside list, and a position-based name would then point at
/// a different request than the file holds.
fn archive_name(request: &crate::billing::Request) -> String {
    use sha2::{Digest, Sha256};
    let mut digest = Sha256::new();
    digest.update(request.path.as_bytes());
    digest.update([0]);
    digest.update(&request.body);
    let hash = digest.finalize();
    let hex: String = hash[..8].iter().map(|b| format!("{b:02x}")).collect();
    format!("{}-{hex}.json", request.created_ms)
}

impl Journal for FileJournal {
    fn store(&mut self, snapshot: &Snapshot) -> io::Result<()> {
        let directory = self
            .path
            .parent()
            .expect("journal parent")
            .join("outbox-expired");
        let wanted: BTreeSet<String> = snapshot.expired_outbox.iter().map(archive_name).collect();
        if !snapshot.expired_outbox.is_empty() {
            fs::create_dir_all(&directory)?;
            for request in &snapshot.expired_outbox {
                let path = directory.join(archive_name(request));
                if !path.exists() {
                    let mut options = OpenOptions::new();
                    options.write(true).create_new(true);
                    #[cfg(unix)]
                    {
                        use std::os::unix::fs::OpenOptionsExt;
                        options.mode(0o600).custom_flags(libc::O_NOFOLLOW);
                    }
                    let mut file = options.open(path)?;
                    serde_json::to_writer(&mut file, request).map_err(io::Error::other)?;
                    file.sync_all()?;
                }
            }
            File::open(&directory)?.sync_all()?;
        }
        let temporary = self.path.with_extension("new");
        let mut options = OpenOptions::new();
        options.create(true).truncate(true).write(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            options.mode(0o600).custom_flags(libc::O_NOFOLLOW);
        }
        let mut file = options.open(&temporary)?;
        serde_json::to_writer(&mut file, snapshot).map_err(io::Error::other)?;
        file.flush()?;
        file.sync_all()?;
        fs::rename(&temporary, &self.path)?;
        File::open(self.path.parent().expect("journal parent"))?.sync_all()?;
        // The snapshot is published: failing this store now would leave the
        // caller's state behind the disk. A failed deletion is retried by
        // the next store instead.
        if let Err(error) = self.remove_unlisted_archives(&directory, wanted) {
            tracing::warn!(%error, "outbox_archive_cleanup_failed");
        }
        Ok(())
    }
}

impl FileJournal {
    /// Deletes archived requests the just-published snapshot no longer lists
    /// (retention removed them, or an older release named them by position).
    /// This runs only after the snapshot is durable, so a crash before or
    /// during it leaves at worst a file the next store deletes, never a
    /// snapshot naming a request whose archive is gone. The directory is read
    /// once per process; after that the known names are tracked in memory.
    fn remove_unlisted_archives(
        &mut self,
        directory: &Path,
        wanted: BTreeSet<String>,
    ) -> io::Result<()> {
        let archived = match self.archived.take() {
            Some(archived) => archived,
            None => match fs::read_dir(directory) {
                Ok(entries) => entries
                    .map(|entry| entry.map(|e| e.file_name().to_string_lossy().into_owned()))
                    .collect::<io::Result<BTreeSet<_>>>()?,
                Err(e) if e.kind() == io::ErrorKind::NotFound => BTreeSet::new(),
                Err(e) => return Err(e),
            },
        };
        if archived.is_empty() && wanted.is_empty() {
            self.archived = Some(wanted);
            return Ok(());
        }
        let mut removed = false;
        for name in archived.difference(&wanted) {
            if !name.ends_with(".json") {
                continue;
            }
            match fs::remove_file(directory.join(name)) {
                Ok(()) => removed = true,
                Err(e) if e.kind() == io::ErrorKind::NotFound => {}
                Err(e) => {
                    // Forget what is on disk so the next store reads the
                    // directory again and retries every leftover name.
                    self.archived = None;
                    return Err(e);
                }
            }
        }
        if removed {
            File::open(directory)?.sync_all()?;
        }
        self.archived = Some(wanted);
        Ok(())
    }
}
