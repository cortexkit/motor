//! What the `exec-remote/v1` agent route adds to a job's stream: the hosted
//! placement and billing on its terminal record, and the files its caller
//! asked to have returned.
//!
//! The direct route (`ck motor run`) attaches its own JSON fields instead; see
//! `pump_direct_secret` in `module.rs`.
use crate::{module::forward, storage::Storage, HostedJobState, JobStream, ReturnOptions};
use base64::{engine::general_purpose::STANDARD, Engine};
use cortexkit_exec_remote_types as shared;
use motor_protocol::{
    BytePayload, HostedUsage, JobStatusReply, RunRequest, StreamRecord, TerminalRecord, Uuid,
    RETURN_MAX_BYTES,
};
use std::{io, path::Path};
use subc_client_rs::RequestCtx;

/// Why an agent `exec.run` request's returned-file fields were refused.
#[derive(Debug, PartialEq, Eq)]
pub(crate) enum ReturnRequestError {
    /// Refused as `invalid_params` before any job exists; the message names
    /// the offending field.
    InvalidParams(String),
    /// Refused before start as `request_rejected` with this detail code, the
    /// same code the direct route uses for paths outside its lexical bounds.
    Rejected(&'static str),
}

/// Take the returned-file fields out of an agent request, so they never reach
/// storage, logs or the runner in their caller-facing form.
///
/// The caller names paths relative to `cwd`. The runner protocol, like the
/// direct route, takes absolute paths under the workspace, so each path is
/// joined to `cwd`. Lexical bounds (no `..`, at most 64 paths, inside the
/// workspace) are then checked by the same validation the direct route uses,
/// before any lease or upload.
///
/// An empty `return_paths` disables collection: the shared contract makes a
/// non-empty list the only opt-in to the `return_result` record, and older
/// callers cannot send the field at all. The byte cap is still checked, as the
/// contract requires an over-cap value to be refused whether or not paths
/// accompany it.
///
/// Returns the runner-side options and the `cwd` to store with the job, or
/// `None` when the caller asked for no files.
pub(crate) fn take_return_request(
    request: &mut RunRequest,
) -> Result<Option<(ReturnOptions, String)>, ReturnRequestError> {
    let paths = std::mem::take(&mut request.return_paths);
    let cap = request.return_max_bytes.take();
    if let Some(cap) = cap.filter(|cap| *cap > shared::RETURN_MAX_BYTES_CAP) {
        return Err(ReturnRequestError::InvalidParams(format!(
            "return_max_bytes {cap} exceeds the {} byte cap",
            shared::RETURN_MAX_BYTES_CAP
        )));
    }
    if paths.is_empty() {
        return Ok(None);
    }
    let cwd = Path::new(&request.cwd);
    let mut absolute = Vec::with_capacity(paths.len());
    for path in &paths {
        // Joining an absolute path would replace `cwd` rather than extend it.
        if Path::new(path).is_absolute() {
            return Err(ReturnRequestError::Rejected("return_paths_exceeded"));
        }
        absolute.push(cwd.join(path).to_string_lossy().into_owned());
    }
    Ok(Some((
        ReturnOptions {
            return_paths: absolute,
            return_max_bytes: cap.unwrap_or(RETURN_MAX_BYTES),
        },
        request.cwd.clone(),
    )))
}

/// The hosted placement and billing to report on a job's terminal record.
///
/// Only a job whose Run frame was sent to a hosted runner ran hosted. A job
/// whose lease never reached that point, or a spill sent back to the build
/// server, reports nothing. Billing is reported once settled; while it is
/// pending, or if settlement failed, `billed_vcpu_ms` is absent. The terminal
/// record is never held back for billing: a later `exec.attach` or
/// `exec.status` reads the settled figure.
pub(crate) fn hosted_usage(state: Option<&HostedJobState>) -> Option<HostedUsage> {
    let state = state.filter(|state| state.run_sent)?;
    let usage = HostedUsage::new(state.placement.as_ref()?.host.clone());
    Some(
        match state
            .billed_vcpu_ms
            .filter(|_| state.billing == crate::BillingStatus::Available)
        {
            Some(billed) => usage.with_billed_vcpu_ms(billed),
            None => usage,
        },
    )
}

fn with_hosted(terminal: TerminalRecord, usage: Option<HostedUsage>) -> TerminalRecord {
    match usage {
        Some(usage) => terminal.with_hosted(usage),
        None => terminal,
    }
}

/// Add hosted placement and billing to an `exec.status` reply's terminal.
pub(crate) fn status_with_hosted(
    mut reply: JobStatusReply,
    state: Option<&HostedJobState>,
) -> JobStatusReply {
    reply.terminal = reply
        .terminal
        .map(|terminal| with_hosted(terminal, hosted_usage(state)));
    reply
}

/// The caller-facing form of the runner's return result: cwd-relative paths
/// and decoded bytes. `None` only if stored file data is not valid base64,
/// which the runner never produces.
pub(crate) fn return_record(
    result: motor_protocol::ReturnResult,
    cwd: &str,
) -> Option<shared::ReturnResult> {
    if let Some(files) = result.returned_files {
        let mut returned = Vec::with_capacity(files.len());
        for file in files {
            let Ok(data) = STANDARD.decode(&file.data) else {
                tracing::warn!(job_id = %result.job_id, "stored return result has invalid file data; not forwarded");
                return None;
            };
            let path = Path::new(&file.path)
                .strip_prefix(cwd)
                .map_or(file.path.clone(), |relative| {
                    relative.to_string_lossy().into_owned()
                });
            returned.push(shared::ReturnedFile::new(
                path,
                file.sha256,
                BytePayload(data),
            ));
        }
        return Some(shared::ReturnResult::with_files(
            result.job_id,
            result.seq,
            returned,
        ));
    }
    let failed = result.return_paths_failed?;
    // Both protocols spell causes in snake_case; the caller's set is open, so
    // a cause added to the runner later still reaches it as written.
    let cause = serde_json::to_value(failed.cause)
        .ok()
        .and_then(|cause| cause.as_str().map(str::to_owned))
        .unwrap_or_default();
    Some(shared::ReturnResult::failed(
        result.job_id,
        result.seq,
        shared::ReturnPathsFailed::new(
            failed.measured_bytes,
            failed.cap_bytes,
            shared::ReturnPathsFailureCause::from(cause),
        ),
    ))
}

/// The records an agent caller receives for one record from the job stream.
/// Only the terminal record changes: it gains the hosted usage, and when the
/// caller asked for returned files the return result goes immediately before
/// it. `from_seq` is the caller's resume cursor, so a reattach does not repeat
/// a return result the caller already received.
pub(crate) fn agent_records(
    storage: &Storage,
    id: Uuid,
    from_seq: u64,
    record: StreamRecord,
) -> Vec<StreamRecord> {
    let StreamRecord::Terminal(terminal) = record else {
        return vec![record];
    };
    let job = storage.job(id);
    let mut records = Vec::with_capacity(2);
    if let Some(job) = &job {
        if let (Some(result), Some(cwd)) = (&job.return_result, &job.return_cwd) {
            if result.seq >= from_seq {
                records.extend(return_record(result.clone(), cwd).map(StreamRecord::ReturnResult));
            }
        }
    }
    let usage = hosted_usage(job.and_then(|job| job.hosted_route()).as_ref());
    records.push(StreamRecord::Terminal(with_hosted(terminal, usage)));
    records
}

/// Forward an agent job's stream to its caller until the terminal record.
pub(crate) async fn pump(ctx: &RequestCtx, stream: JobStream, from_seq: u64) -> io::Result<()> {
    let id = stream.job_id;
    let storage = stream.storage();
    forward(ctx.cancelled(), stream, |record| {
        let records = agent_records(&storage, id, from_seq, record);
        async move {
            for record in records {
                ctx.emit(serde_json::to_vec(&record).map_err(io::Error::other)?)
                    .await
                    .map_err(io::Error::other)?;
            }
            Ok(())
        }
    })
    .await
}

/// The records `pump` would send an agent caller for this stream, through its
/// terminal record, for tests that run a `Motor` without a module connection.
#[cfg(test)]
pub(crate) async fn agent_stream(mut stream: JobStream, from_seq: u64) -> Vec<StreamRecord> {
    let storage = stream.storage();
    let mut records = Vec::new();
    loop {
        let record =
            tokio::time::timeout(std::time::Duration::from_secs(30), stream.records.recv())
                .await
                .expect("timed out waiting for a job record")
                .expect("stream ended without a terminal record")
                .unwrap();
        let terminal = matches!(record, StreamRecord::Terminal(_));
        records.extend(agent_records(&storage, stream.job_id, from_seq, record));
        if terminal {
            return records;
        }
    }
}

#[cfg(test)]
#[path = "agent_route_tests.rs"]
mod tests;
