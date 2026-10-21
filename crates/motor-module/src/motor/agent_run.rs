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
        returns: Option<(crate::ReturnOptions, String)>,
    ) -> io::Result<JobStream> {
        let Some((returns, cwd)) = returns else {
            return self.run_attributed(request, attribution);
        };
        let stream =
            self.run_classified(request, (attribution, None), false, false, false, returns)?;
        // The runner sends its return result only after the command ends, long
        // after this write, so the pump always finds the cwd beside the result.
        self.storage.return_cwd(stream.job_id, cwd)?;
        Ok(stream)
    }
}
