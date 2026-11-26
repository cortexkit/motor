use motor_protocol::Uuid;
use std::{
    collections::BTreeSet,
    ffi::OsString,
    fs, io,
    path::Path,
    sync::Mutex,
    time::{Duration, SystemTime},
};

// Keep logs long enough to inspect a failed job the next day, even after the
// 24-hour job history has expired.
const GUEST_EVIDENCE_RETENTION: Duration = Duration::from_secs(72 * 60 * 60);

#[derive(Default)]
pub(super) struct GuestEvidence {
    // Partial deletion can update a directory's mtime. Retry it anyway: a job
    // UUID is not reused for new evidence.
    failed_removals: Mutex<BTreeSet<OsString>>,
}

impl GuestEvidence {
    pub(super) fn prune(&self, layout_root: &Path, held: &BTreeSet<Uuid>, now: SystemTime) {
        let (folders, bytes_freed) =
            self.prune_with(layout_root, held, now, |path| fs::remove_dir_all(path));
        if folders != 0 {
            tracing::info!(
                folders,
                bytes_freed,
                "pruned expired Windows guest evidence"
            );
        }
    }

    fn prune_with(
        &self,
        layout_root: &Path,
        held: &BTreeSet<Uuid>,
        now: SystemTime,
        remove: impl Fn(&Path) -> io::Result<()>,
    ) -> (usize, u64) {
        let mut removed = (0, 0u64);
        let evidence = layout_root.join("evidence");
        let entries = match read_evidence(layout_root, &evidence) {
            Ok(entries) => entries,
            Err(error) if error.kind() == io::ErrorKind::NotFound => return removed,
            Err(error) => {
                tracing::warn!(path = %evidence.display(), %error, "Windows guest evidence could not be scanned");
                return removed;
            }
        };
        let mut failed_removals = self.failed_removals.lock().unwrap();
        for entry in entries {
            let entry = match entry {
                Ok(entry) => entry,
                Err(error) => {
                    tracing::warn!(path = %evidence.display(), %error, "Windows guest evidence entry could not be read");
                    continue;
                }
            };
            let name = entry.file_name();
            if !name
                .to_str()
                .and_then(guest_job_id)
                .is_some_and(|id| !held.contains(&id))
            {
                continue;
            }
            let path = entry.path();
            let result = match expired_bytes(&path, now, failed_removals.contains(&name)) {
                Ok(Some(bytes)) => match remove(&path) {
                    Ok(()) => {
                        failed_removals.remove(&name);
                        removed.0 += 1;
                        removed.1 = removed.1.saturating_add(bytes);
                        Ok(())
                    }
                    Err(error) => {
                        failed_removals.insert(name);
                        Err(error)
                    }
                },
                Ok(None) => Ok(()),
                Err(error) => Err(error),
            };
            if let Err(error) = result {
                tracing::warn!(path = %path.display(), %error, "Windows guest evidence could not be pruned; will retry");
            }
        }
        failed_removals.retain(|name| {
            !matches!(fs::symlink_metadata(evidence.join(name)), Err(error) if error.kind() == io::ErrorKind::NotFound)
        });
        removed
    }
}

fn guest_job_id(name: &str) -> Option<Uuid> {
    let id = name.strip_suffix("-guest")?;
    if id.len() != uuid::fmt::Hyphenated::LENGTH {
        return None;
    }
    Uuid::parse_str(id).ok()
}

fn read_evidence(layout_root: &Path, evidence: &Path) -> io::Result<fs::ReadDir> {
    refuse_link(layout_root, &fs::symlink_metadata(layout_root)?)?;
    refuse_link(evidence, &fs::symlink_metadata(evidence)?)?;
    fs::read_dir(evidence)
}

fn refuse_link(path: &Path, metadata: &fs::Metadata) -> io::Result<()> {
    let linked = metadata.file_type().is_symlink();
    #[cfg(windows)]
    let linked = {
        use std::os::windows::fs::MetadataExt;
        use windows_sys::Win32::Storage::FileSystem::FILE_ATTRIBUTE_REPARSE_POINT;
        linked || metadata.file_attributes() & FILE_ATTRIBUTE_REPARSE_POINT != 0
    };
    if linked {
        return Err(io::Error::other(format!(
            "symlink or reparse point at {}; leaving evidence in place",
            path.display()
        )));
    }
    Ok(())
}

fn expired_bytes(path: &Path, now: SystemTime, retry: bool) -> io::Result<Option<u64>> {
    let metadata = fs::symlink_metadata(path)?;
    refuse_link(path, &metadata)?;
    if !metadata.is_dir()
        || (!retry
            && !now
                .duration_since(metadata.modified()?)
                .is_ok_and(|age| age > GUEST_EVIDENCE_RETENTION))
    {
        return Ok(None);
    }

    // Skip the entire folder if it contains a link. Check entries before
    // descending, then let remove_dir_all do the no-follow deletion.
    let mut dirs = vec![path.to_owned()];
    let mut bytes = 0u64;
    while let Some(dir) = dirs.pop() {
        for entry in fs::read_dir(dir)? {
            let path = entry?.path();
            let metadata = fs::symlink_metadata(&path)?;
            refuse_link(&path, &metadata)?;
            if metadata.is_dir() {
                dirs.push(path);
            } else if metadata.is_file() {
                bytes = bytes.saturating_add(metadata.len());
            }
        }
    }
    Ok(Some(bytes))
}

#[cfg(test)]
mod tests;
