use super::*;

impl Runner {
    fn qualification_marker(&self) -> PathBuf {
        self.config().state_dir().join("qualification-required")
    }

    pub(crate) fn qualification_required(&self) -> bool {
        // An unreadable marker must not accidentally reopen fleet admission.
        !matches!(fs::symlink_metadata(self.qualification_marker()), Err(error) if error.kind() == io::ErrorKind::NotFound)
    }

    pub(crate) fn admission_blocked(&self) -> Option<String> {
        self.qualification_required()
            .then(|| "runner awaiting qualification".into())
    }

    pub(super) fn qualification_refusal(
        &self,
        qualification: bool,
    ) -> Option<(RefusalReason, String)> {
        (!qualification)
            .then(|| self.admission_blocked())
            .flatten()
            .map(|detail| (RefusalReason::RunnerFull, detail))
    }

    // Only the ACL-protected qualification listener may call this for a peer.
    // Ordinary Resume clears a drain, never the durable qualification marker.
    pub(crate) fn resume_qualification(&self) -> io::Result<()> {
        match fs::remove_file(self.qualification_marker()) {
            Ok(()) => Ok(()),
            Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(()),
            Err(error) => Err(error),
        }
    }
}
