//! Store-free, direct-operator exercise of unconnected hosted bookings.

use super::{account::AccountTokens, placement::Lease, Client, Error, HostedJobState};
use crate::Config;
use motor_lease::{LeaseRequest, Placement, RequesterJwk};
use motor_protocol::Uuid;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::{sync::Arc, time::Duration};

const WAIT: Duration = Duration::from_secs(180);
pub(crate) const CALL_TIMEOUT: Duration = Duration::from_secs(270);

#[derive(Clone, Copy, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub(crate) enum Mode {
    Release,
    Replace,
}

#[derive(Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct Request {
    pub mode: Mode,
    pub host: String,
}

#[derive(Debug, Deserialize, Serialize, PartialEq, Eq)]
pub(crate) struct Reply {
    status: u16,
    error: Option<String>,
}

#[derive(Default, Deserialize, Serialize)]
pub(crate) struct Replies {
    lease: Vec<Reply>,
    replace: Vec<Reply>,
    release: Vec<Reply>,
}

impl Replies {
    fn for_path(&mut self, path: &str) -> &mut Vec<Reply> {
        if path == "/v1/leases" {
            &mut self.lease
        } else if path.ends_with("/replace") {
            &mut self.replace
        } else {
            &mut self.release
        }
    }
}

#[derive(Deserialize, Serialize)]
pub(crate) struct Report {
    pub status: String,
    job_id: Uuid,
    placement: Option<Placement>,
    replacement_placement: Option<Placement>,
    replace_pending: usize,
    error: Option<String>,
    cleanup_error: Option<String>,
    replies: Replies,
}

impl Report {
    fn refuse(&mut self, code: &str) {
        self.status = "refused".into();
        self.error = Some(code.into());
    }
}

// The task owns cleanup even if the local CLI disconnects or its RPC is cancelled.
// No job record or persisted hosted state is needed for this unconnected booking.
pub(crate) async fn run(
    config: Config,
    tokens: Option<Arc<AccountTokens>>,
    request: Request,
) -> std::io::Result<Report> {
    tokio::spawn(probe(config, tokens, request, WAIT))
        .await
        .map_err(std::io::Error::other)
}

async fn probe(
    config: Config,
    tokens: Option<Arc<AccountTokens>>,
    request: Request,
    wait: Duration,
) -> Report {
    let id = Uuid::now_v7();
    let mut report = Report {
        status: "ok".into(),
        job_id: id,
        placement: None,
        replacement_placement: None,
        replace_pending: 0,
        error: None,
        cleanup_error: None,
        replies: Replies::default(),
    };
    if config.broker_url.is_none() {
        report.refuse("broker_not_configured");
        return report;
    }
    let hint = Placement {
        host: request.host,
        vm: config
            .hosted_repositories_placement_hint
            .as_ref()
            .and_then(|hint| hint.vm.clone()),
    };
    if !motor_lease::valid_name(&hint.host) || !super::valid_hint(&hint) {
        report.refuse("invalid_probe_placement");
        return report;
    }
    let key = match super::requester() {
        Ok(key) => key,
        Err(error) => {
            report.refuse(error.code());
            return report;
        }
    };
    let body = LeaseRequest {
        job_id: id.to_string(),
        vcpus: 2,
        platform: super::lease_platform(None)
            .expect("linux lease platform")
            .into(),
        net: false,
        // Fixed probe identities must not reveal or depend on a local workspace.
        workspace_key_hash: super::keyed_hash_input(&[0; 32], "ck-hosted-fence-probe-workspace"),
        repo_hash: super::keyed_hash_input(&[0; 32], "ck-hosted-fence-probe-repository"),
        placement_hint: Some(hint),
        avoid_host: None,
        timeout_s: *super::LEASE_TIMEOUT_S.start(),
        allow_spill: Some(false),
        requester_jwk: RequesterJwk::from_key(&key.verifying_key()),
        mem_mib: None,
    };
    let mut client = Client::new(HostedJobState::new(&config)).with_account_tokens(tokens);
    client.probe_replies = Some(std::sync::Mutex::new(Replies::default()));
    let deadline = tokio::time::Instant::now() + wait;
    let bearer = match tokio::time::timeout_at(deadline, client.bearer()).await {
        Ok(Ok(bearer)) => bearer,
        result => {
            report.refuse(
                result
                    .ok()
                    .and_then(Result::err)
                    .unwrap_or(Error::Broker)
                    .code(),
            );
            return report;
        }
    };
    let result = async {
        let lease: Lease = client
            .post_ready_authorized("/v1/leases", &body, bearer, deadline, Error::Broker)
            .await?;
        report.placement = Some(lease.placement.clone());
        if request.mode == Mode::Replace {
            let bearer = tokio::time::timeout_at(deadline, client.bearer())
                .await
                .map_err(|_| Error::Broker)??;
            let next: Lease = client
                .post_ready_authorized(
                    &format!("/v1/leases/{id}/replace"),
                    &serde_json::json!({}),
                    bearer,
                    deadline,
                    Error::Broker,
                )
                .await?;
            report.replacement_placement = Some(next.placement.clone());
        }
        Ok::<_, Error>(())
    }
    .await;
    if let Err(error) = result {
        report.refuse(error.code());
    }
    // A lost or malformed lease/replace reply can still have booked capacity.
    // Release by the requested job id on success, refusal and deadline expiry,
    // outside the operation's deadline so an expired wait cannot skip cleanup.
    if let Err(error) = client.release(id).await {
        report.status = "refused".into();
        report.cleanup_error = Some(error.code().into());
    }
    report.replies = client.probe_replies.unwrap().into_inner().unwrap();
    report.replace_pending = report
        .replies
        .replace
        .iter()
        .filter(|reply| reply.status == 503 && reply.error.as_deref() == Some("replace_pending"))
        .count();
    report
}

impl Client {
    pub(super) fn observe_probe_reply(&self, path: &str, status: u16) {
        if let Some(replies) = &self.probe_replies {
            replies.lock().unwrap().for_path(path).push(Reply {
                status,
                error: None,
            });
        }
    }

    pub(super) fn observe_probe_error(&self, path: &str, body: Option<&Value>) {
        if let Some(replies) = &self.probe_replies {
            if let Some(reply) = replies.lock().unwrap().for_path(path).last_mut() {
                reply.error = body
                    .and_then(|body| body.get("error"))
                    .and_then(Value::as_str)
                    .filter(|code| motor_lease::valid_name(code))
                    .map(str::to_owned);
            }
        }
    }

    pub(super) async fn discard_probe_reply(&self, path: &str, reply: reqwest::Response) {
        if self.probe_replies.is_some() {
            self.observe_probe_error(path, reply.json::<Value>().await.ok().as_ref());
        }
    }
}

#[cfg(test)]
mod tests;
