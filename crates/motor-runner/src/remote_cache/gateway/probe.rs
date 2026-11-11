use super::*;
use motor_protocol::{
    CreateOnlyProbeCase, CreateOnlyProbeDetail as Detail, CreateOnlyProbeReport,
    CREATE_ONLY_PROBE_CASES,
};
use std::sync::atomic::AtomicU16;

// sccache requests compiler-output hashes and `.sccache_check` for its startup
// check. Keep probe objects in a separate namespace so jobs never read them.
const PREFIX: &str = ".motor-create-only-probe/";
const CASE_TIMEOUT: Duration = Duration::from_secs(10);

struct Probe<'a> {
    gateway: &'a Gateway,
    account: &'a str,
    job: String,
    bucket: String,
    prefix: String,
}

impl Drop for Probe<'_> {
    fn drop(&mut self) {
        // Also discard transient counters if the admin caller disconnects and
        // cancels the probe before normal finalization.
        self.gateway.take_job_put_totals(&self.job);
        self.gateway.shared.jobs.lock().unwrap().remove(&self.job);
    }
}

#[derive(Default)]
struct Reply {
    store: u16,
    client: u16,
    body: Vec<u8>,
    sent: usize,
}

impl Gateway {
    pub(crate) async fn create_only_probe(&self, account: &str) -> CreateOnlyProbeReport {
        if self.shared.job_write_block.blocked() {
            return CreateOnlyProbeReport::refused(Detail::RemoteCacheWriteBlocked);
        }
        let Some(generation) = self.shared.generations.admit() else {
            return CreateOnlyProbeReport::refused(Detail::RemoteCacheUnavailable);
        };
        if generation.tenant != account {
            return CreateOnlyProbeReport::refused(Detail::RemoteCacheAccountMismatch);
        }
        if generation.write_key.is_none() {
            return CreateOnlyProbeReport::refused(Detail::RemoteCacheWriteKeyMissing);
        }
        let nonce = motor_protocol::Uuid::now_v7();
        let probe = Probe {
            gateway: self,
            account,
            job: format!("create-only-probe-{nonce}"),
            bucket: generation.bucket.clone(),
            prefix: format!("{PREFIX}{nonce}"),
        };
        let mut cases = Vec::new();
        for name in CREATE_ONLY_PROBE_CASES {
            let result = tokio::time::timeout(CASE_TIMEOUT, async {
                match name {
                    "concurrent_creates" => probe.concurrent_creates().await,
                    "existing_object" => probe.existing_object().await,
                    "interrupted_body" => probe.interrupted_body().await,
                    "fill_race" => probe.fill_race().await,
                    _ => unreachable!(),
                }
            })
            .await;
            let detail = match result {
                Ok(true) => Detail::Passed,
                Ok(false) => Detail::UnexpectedStatusOrBytes,
                Err(_) => Detail::TimedOut,
            };
            cases.push(CreateOnlyProbeCase {
                name: name.into(),
                ok: detail == Detail::Passed,
                detail,
            });
        }
        // Discard only in-memory admission/accounting state. Objects deliberately
        // remain in the bucket for its lifecycle rule; the gateway cannot DELETE.
        self.finish_job(&probe.job).await;
        CreateOnlyProbeReport::new(cases)
    }
}

impl Probe<'_> {
    fn request<'a>(
        &'a self,
        name: &'a str,
        method: &'a str,
        body: &'a [u8],
        fill: bool,
        interrupted: bool,
    ) -> std::pin::Pin<Box<dyn std::future::Future<Output = Reply> + Send + 'a>> {
        // Each gateway exchange owns streaming buffers. Keep concurrent
        // exchanges on the heap rather than multiplying the caller's stack.
        Box::pin(async move {
            let mut binding = if fill {
                self.gateway
                    .write_binding(self.account, Tool::Sccache, Adapter::S3)
            } else {
                self.gateway.job_create_binding(&self.job, self.account)
            };
            let status = Arc::new(AtomicU16::new(0));
            let sent = Arc::new(AtomicUsize::new(0));
            Arc::get_mut(&mut binding).unwrap().probe_status = Some(status.clone());
            Arc::get_mut(&mut binding).unwrap().probe_sent = Some(sent.clone());
            // Serve an in-process connection, not a socket made visible to jobs.
            // Admission, account binding, signing, limits and the emergency
            // job-write block all remain in effect.
            let (mut client, server) = tokio::io::duplex(1024);
            let Some(serve) = self.gateway.admit_connection(&binding, server) else {
                return Reply::default();
            };
            let exchange = async {
                let head = format!(
                "{method} /{}/{}/{name} HTTP/1.1\r\nhost: probe\r\nconnection: close\r\ncontent-length: {}\r\n\r\n",
                self.bucket, self.prefix, body.len()
            );
                if client.write_all(head.as_bytes()).await.is_err() {
                    return Reply::default();
                }
                let sent = if interrupted {
                    &body[..body.len() / 2]
                } else {
                    body
                };
                if client.write_all(sent).await.is_err() || interrupted {
                    return Reply::default();
                }
                let deadline = Instant::now() + CASE_TIMEOUT;
                let mut conn = Conn::new(Box::new(client));
                let Ok(head) = conn.read_head(deadline, deadline, method == "HEAD").await else {
                    return Reply::default();
                };
                let length = if method == "HEAD" {
                    0
                } else {
                    head.content_length.unwrap_or(0)
                };
                if length > 256 * 1024 {
                    return Reply::default();
                }
                let mut body = vec![0; length as usize];
                let mut offset = 0;
                while offset < body.len() {
                    match conn.read_body(&mut body[offset..]).await {
                        Ok(0) | Err(_) => return Reply::default(),
                        Ok(n) => offset += n,
                    }
                }
                Reply {
                    client: head.status,
                    body,
                    store: 0,
                    sent: 0,
                }
            };
            // Dropping a timed-out case drops both futures and the upstream stream.
            // An interrupted request finishes closing upstream before the next HEAD.
            let (mut reply, ()) = tokio::join!(exchange, serve);
            reply.store = status.load(Ordering::Acquire);
            reply.sent = sent.load(Ordering::Acquire);
            reply
        })
    }

    async fn put(&self, name: &str, body: &[u8]) -> Reply {
        self.request(name, "PUT", body, false, false).await
    }

    async fn matches(&self, name: &str, body: &[u8]) -> bool {
        let reply = self.request(name, "GET", &[], false, false).await;
        reply.store == 200 && reply.client == 200 && reply.body == body
    }

    async fn concurrent_creates(&self) -> bool {
        let bodies = [
            b"winner-a".as_slice(),
            b"winner-b",
            b"winner-c",
            b"winner-d",
        ];
        let (a, b, c, d) = tokio::join!(
            self.put("concurrent", bodies[0]),
            self.put("concurrent", bodies[1]),
            self.put("concurrent", bodies[2]),
            self.put("concurrent", bodies[3]),
        );
        let replies = [a, b, c, d];
        let winners: Vec<_> = replies
            .iter()
            .enumerate()
            .filter(|(_, reply)| (200..300).contains(&reply.store))
            .collect();
        if winners.len() != 1
            || replies.iter().any(|reply| {
                !(200..300).contains(&reply.store) && !matches!(reply.store, 412 | 409)
            })
        {
            return false;
        }
        self.matches("concurrent", bodies[winners[0].0]).await
    }

    async fn existing_object(&self) -> bool {
        let first = self.put("existing", b"original").await;
        let second = self.put("existing", b"different").await;
        (200..300).contains(&first.store)
            && second.store == 412
            && self.matches("existing", b"original").await
    }

    async fn interrupted_body(&self) -> bool {
        let body = vec![b'I'; 256 * 1024];
        let interrupted = self.request("interrupted", "PUT", &body, false, true).await;
        let head = self.request("interrupted", "HEAD", &[], false, false).await;
        let later = self.put("interrupted", b"complete").await;
        interrupted.sent > 0
            && interrupted.sent < body.len()
            && head.store == 404
            && head.client == 404
            && (200..300).contains(&later.store)
            && self.matches("interrupted", b"complete").await
    }

    async fn fill_race(&self) -> bool {
        let job = vec![b'J'; 32 * 1024 + 17];
        let fill = vec![b'F'; 64 * 1024 + 31];
        let (created, filled) = tokio::join!(
            self.put("race", &job),
            self.request("race", "PUT", &fill, true, false),
        );
        let result = self.request("race", "GET", &[], false, false).await;
        ((200..300).contains(&created.store) || matches!(created.store, 412 | 409))
            && (200..300).contains(&filled.store)
            && result.store == 200
            && result.client == 200
            && (result.body == job || result.body == fill)
    }
}
