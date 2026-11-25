use super::{macos_hold::RunHoldCheck, *};

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
            .or_else(|| {
                (self.config().backend == BackendKind::MacosVm && self.is_draining())
                    .then(|| "runner_draining".into())
            })
            .or_else(|| self.macos_hold_status())
    }

    pub(super) fn qualification_refusal(
        &self,
        qualification: bool,
    ) -> Option<(RefusalReason, String)> {
        (!qualification)
            .then(|| {
                self.qualification_required()
                    .then(|| "runner awaiting qualification".into())
            })
            .flatten()
            .map(|detail| (RefusalReason::RunnerFull, detail))
    }

    pub(super) fn run_refusal(
        &self,
        inner: &Inner,
        request: &Run,
        access: &crate::workspace_access::Repairs,
    ) -> Option<(RefusalReason, String)> {
        self.check_run_refusal(inner, request, access, None)
    }

    pub(super) fn refuse_run_admission(
        &self,
        job: &mut Job,
        request: &Run,
        (reason, error): &(RefusalReason, String),
        disk_refused: bool,
        hold: &RunHoldCheck,
    ) {
        let detail = if disk_refused {
            error.clone()
        } else if *reason == RefusalReason::RunnerDraining {
            hold.refusal_detail().into()
        } else {
            self.safe_refusal_detail(error, request.env.values().cloned())
        };
        refuse_before_start(
            &self.core.health,
            job,
            reason.clone(),
            error,
            Some(detail),
            (*reason == RefusalReason::RunnerDraining).then(|| self.drain_retry_after_ms()),
            self.now(),
        );
        job.state = JobState::Tombstone;
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
