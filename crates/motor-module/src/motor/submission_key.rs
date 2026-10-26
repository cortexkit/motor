//! Caller keys share the job's journal record and its submission/attach fence.
use super::{JobStream, Motor};
use crate::storage::JobAttribution;
use motor_protocol::{
    allowed_env, Accepted, AttachRequest, Outcome, Ran, RefusalReason, RunRequest, StreamRecord,
};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::io;
use uuid::Uuid;

#[cfg(test)]
mod tests;

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct KeyRecord {
    pub principal: String,
    pub key: String,
    pub digest: String,
    #[serde(default)]
    pub run_sent: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub accepted: Option<Accepted>,
}

fn digest(request: &RunRequest) -> io::Result<String> {
    let mut caller = request.clone();
    caller.resume_from_seq = None;
    caller.submission_key = None;
    caller.env.retain(|name, _| allowed_env(name));
    let canonical = serde_json_canonicalizer::to_vec(&caller).map_err(io::Error::other)?;
    Ok(format!("{:x}", Sha256::digest(canonical)))
}

impl Motor {
    pub(crate) fn has_submission(&self, principal: &str, request: &RunRequest) -> io::Result<bool> {
        match request.submission_key.as_deref() {
            Some(key) => Ok(self.storage.keyed_job(principal, key)?.is_some()),
            None => Ok(false),
        }
    }
    /// Compare the caller request's digest to the stored digest before
    /// take_return_request translates paths or run_job snapshots the checkout.
    /// keyed_admissions serializes lookup through record_submission's durable
    /// write and run_classified's insertion into the submissions attach fence.
    pub(crate) async fn submit_agent(
        &self,
        mut request: RunRequest,
        attribution: Option<JobAttribution>,
        principal: &str,
    ) -> io::Result<JobStream> {
        let Some(key) = request.submission_key.clone() else {
            let returns = match crate::agent_route::take_return_request(&mut request) {
                Ok(returns) => returns,
                Err(crate::agent_route::ReturnRequestError::InvalidParams(message)) => {
                    return Err(io::Error::new(io::ErrorKind::InvalidInput, message))
                }
                Err(crate::agent_route::ReturnRequestError::Rejected(detail)) => {
                    return self
                        .refuse_before_start(RefusalReason::RequestRejected, Some(detail.into()))
                }
            };
            return self.run_agent(request, attribution, returns);
        };
        let cursor = request.resume_from_seq;
        let digest = digest(&request)?;
        let known = {
            let _admission = self.keyed_admissions.lock().unwrap();
            if let Some((id, job)) = self.storage.keyed_job(principal, &key)? {
                if job.submission.as_ref().unwrap().digest != digest {
                    return self.refuse_before_start(
                        RefusalReason::RequestRejected,
                        Some("submission_key_mismatch".into()),
                    );
                }
                Some((id, job))
            } else {
                let record = KeyRecord {
                    principal: principal.into(),
                    key,
                    digest,
                    run_sent: false,
                    accepted: None,
                };
                let returns = match crate::agent_route::take_return_request(&mut request) {
                    Ok(returns) => returns,
                    Err(crate::agent_route::ReturnRequestError::InvalidParams(message)) => {
                        return Err(io::Error::new(io::ErrorKind::InvalidInput, message))
                    }
                    Err(crate::agent_route::ReturnRequestError::Rejected(detail)) => {
                        let stream = self.refuse_before_start(
                            RefusalReason::RequestRejected,
                            Some(detail.into()),
                        )?;
                        self.storage.record_submission(stream.job_id, record)?;
                        return Ok(stream);
                    }
                };
                request.submission_key = None;
                request.resume_from_seq = None;
                return self.run_agent_recorded(request, attribution, returns, Some(record));
            }
        };
        let (id, job) = known.unwrap();
        let record = job.submission.unwrap();
        if record.accepted.is_none()
            && job.local_terminal.as_ref().is_some_and(|terminal| {
                matches!(terminal.outcome, Outcome::RefusedBeforeStart { .. })
                    && terminal.ran == Some(Ran::None)
            })
        {
            return self.never_accepted();
        }
        // An entry in submissions means run_job can still send Run or move a
        // spill. After that entry is removed, absent acceptance and unset
        // run_sent markers prove that no runner received the command.
        if self.submission_never_sent(id) {
            return self.never_accepted();
        }
        self.attach_submission(id, cursor, record.accepted).await
    }

    fn submission_never_sent(&self, id: Uuid) -> bool {
        if self.submissions.lock().unwrap().contains_key(&id) {
            return false;
        }
        // Read after observing the open fence: run_job may have persisted Run
        // and acceptance since keyed_job returned its earlier snapshot.
        self.storage.job(id).is_some_and(|job| {
            job.submission
                .is_some_and(|record| record.accepted.is_none() && !record.run_sent)
                && !job.hosted.is_some_and(|route| route.run_sent)
        })
    }

    fn never_accepted(&self) -> io::Result<JobStream> {
        self.refuse_before_start(RefusalReason::SubmissionNeverAccepted, None)
    }

    async fn attach_submission(
        &self,
        id: Uuid,
        cursor: Option<u64>,
        mut accepted: Option<Accepted>,
    ) -> io::Result<JobStream> {
        let from = cursor.map_or(0, |seq| seq.saturating_add(1));
        let mut source = self.attach(AttachRequest::new(id, from))?;
        let first = if accepted.is_none() {
            let first = source.records.recv().await.transpose()?.ok_or_else(|| {
                io::Error::new(
                    io::ErrorKind::UnexpectedEof,
                    "attach closed before its first record",
                )
            })?;
            accepted = self
                .storage
                .job(id)
                .and_then(|job| job.submission)
                .and_then(|record| record.accepted);
            if let StreamRecord::Terminal(terminal) = &first {
                if accepted.is_none()
                    && matches!(terminal.outcome, Outcome::RefusedBeforeStart { .. })
                    && terminal.ran == Some(Ran::None)
                {
                    self.storage.local_terminal(terminal.clone())?;
                    return self.never_accepted();
                }
            }
            Some(first)
        } else {
            None
        };
        let mut accepted = accepted.unwrap_or_else(|| Accepted::new(id, 0));
        accepted.job_id = id;
        self.storage.submission_accepted(id, accepted.clone())?;
        accepted.attached = true;
        let (stream, tx, _) = self.stream(id);
        tokio::spawn(async move {
            if tx.send(Ok(StreamRecord::Accepted(accepted))).await.is_err() {
                return;
            }
            if let Some(first) = first {
                if after_cursor(&first, cursor) && tx.send(Ok(first)).await.is_err() {
                    return;
                }
            }
            while let Some(record) = source.records.recv().await {
                if record
                    .as_ref()
                    .is_ok_and(|record| !after_cursor(record, cursor))
                {
                    continue;
                }
                if tx.send(record).await.is_err() {
                    break;
                }
            }
        });
        Ok(stream)
    }

    pub(crate) fn submission_owned(&self, id: Uuid, principal: &str) -> bool {
        self.storage
            .job(id)
            .and_then(|job| job.submission)
            .is_none_or(|record| record.principal == principal)
    }
}

pub(crate) fn after_cursor(record: &StreamRecord, cursor: Option<u64>) -> bool {
    let seq = match record {
        StreamRecord::Started(record) => Some(record.seq),
        StreamRecord::Output(record) => Some(record.seq),
        StreamRecord::ReturnResult(record) => Some(record.seq),
        StreamRecord::ReturnChunk(record) => Some(record.seq),
        StreamRecord::ReturnManifest(record) => Some(record.seq),
        _ => None,
    };
    cursor.is_none_or(|cursor| seq.is_none_or(|seq| seq > cursor))
}
