//! Starting an `exec-remote/v1` job whose caller asked for returned files.
use super::{JobStream, Motor};
use crate::storage::JobAttribution;
use motor_protocol::RunRequest;
use std::io;

impl Motor {
    /// Start an agent job with attribution and optional returned files. With
    /// returns, `cwd` is the request's working directory: it is stored with
    /// the job so the agent route can forward the runner's return result with
    /// cwd-relative paths, including after a restart or a reattach.
    pub(crate) fn run_agent(
        &self,
        request: RunRequest,
        attribution: Option<JobAttribution>,
        returns: Option<crate::agent_route::ReturnRequest>,
    ) -> io::Result<JobStream> {
        self.run_agent_recorded(request, attribution, returns, None)
    }
    pub(super) fn run_agent_recorded(
        &self,
        request: RunRequest,
        attribution: Option<JobAttribution>,
        returns: Option<crate::agent_route::ReturnRequest>,
        key: Option<super::submission_key::KeyRecord>,
    ) -> io::Result<JobStream> {
        let Some(crate::agent_route::ReturnRequest {
            options,
            cwd,
            requested,
        }) = returns
        else {
            return self.run_classified_keyed(
                request,
                (attribution, None),
                false,
                false,
                false,
                crate::ReturnOptions::default(),
                key,
            );
        };
        let stream = self.run_classified_keyed(
            request,
            (attribution, None),
            false,
            false,
            false,
            options,
            key,
        )?;
        if let Some(requested) = requested {
            self.storage.return_requested(stream.job_id, requested)?;
        }
        // The job ID is minted inside run_classified, so the cwd is stored
        // just after the job task starts. A return result exists only once
        // that task has snapshotted, uploaded and run the command remotely,
        // so in practice the cwd is stored long before the pump looks for it.
        self.storage.return_cwd(stream.job_id, cwd)?;
        Ok(stream)
    }
}
