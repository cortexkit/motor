use crate::state::Snapshot;
use std::fs::{self, File, OpenOptions};
use std::io::{self, Write};
use std::path::{Path, PathBuf};

pub trait Journal: Send {
    fn store(&mut self, snapshot: &Snapshot) -> io::Result<()>;
}

/// A rename publishes a complete snapshot; syncing the directory makes the
/// rename durable as well as the contents. No credential or signature is stored.
pub struct FileJournal {
    path: PathBuf,
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
        Ok((Self { path }, snapshot))
    }
}

impl Journal for FileJournal {
    fn store(&mut self, snapshot: &Snapshot) -> io::Result<()> {
        if !snapshot.expired_outbox.is_empty() {
            let directory = self
                .path
                .parent()
                .expect("journal parent")
                .join("outbox-expired");
            fs::create_dir_all(&directory)?;
            for (index, request) in snapshot.expired_outbox.iter().enumerate() {
                let path = directory.join(format!("{}-{index}.json", request.created_ms));
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
            File::open(directory)?.sync_all()?;
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
        File::open(self.path.parent().expect("journal parent"))?.sync_all()
    }
}
