//! The macOS admission hold outlives the launchd-managed runner process.
use std::{fs::File, io, os::unix::fs::OpenOptionsExt, path::Path};

const MARKER: &str = "macvm-drained";

pub(crate) fn load(root: &Path) -> bool {
    match std::fs::symlink_metadata(root.join(MARKER)) {
        Ok(_) => true,
        Err(error) if error.kind() == io::ErrorKind::NotFound => false,
        Err(error) => {
            tracing::warn!(%error, "cannot inspect macOS drain marker; admission stays closed");
            true
        }
    }
}

pub(crate) fn store(root: &Path) -> io::Result<()> {
    match std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .custom_flags(libc::O_NOFOLLOW | libc::O_CLOEXEC)
        .open(root.join(MARKER))
    {
        Ok(file) => file.sync_all()?,
        Err(error) if error.kind() == io::ErrorKind::AlreadyExists => {}
        Err(error) => return Err(error),
    }
    File::open(root)?.sync_all()
}

pub(crate) fn clear(root: &Path) -> io::Result<()> {
    match std::fs::remove_file(root.join(MARKER)) {
        Ok(()) => {}
        Err(error) if error.kind() == io::ErrorKind::NotFound => {}
        Err(error) => return Err(error),
    }
    File::open(root)?.sync_all()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_helpers::CanonicalScratchDir as ScratchDir;

    #[test]
    fn marker_survives_repeated_drains_and_only_resume_clears_it() {
        let root = ScratchDir::new("macos-drain-marker");
        assert!(!load(&root));
        store(&root).unwrap();
        store(&root).unwrap();
        assert!(load(&root));
        clear(&root).unwrap();
        clear(&root).unwrap();
        assert!(!load(&root));
    }

    #[test]
    fn unreadable_marker_state_is_closed_and_failed_clear_is_an_error() {
        let root = ScratchDir::new("macos-drain-error");
        let file = root.join("not-a-directory");
        std::fs::write(&file, "").unwrap();
        assert!(load(&file));
        std::fs::create_dir(root.join(MARKER)).unwrap();
        assert!(clear(&root).is_err());
        assert!(load(&root));
    }
}
