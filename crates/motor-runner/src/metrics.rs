//! Outbound-only monitor telemetry, independent of job admission and execution.
mod identity;
mod sample;
#[cfg(test)]
mod tests;

use crate::Config;
use ed25519_dalek::SigningKey;
use motor_lease::{metrics::*, HostHeaders};
use serde::{Deserialize, Serialize};
use std::{io, io::Read, path::PathBuf, time::Duration};

const TIMEOUT: Duration = Duration::from_secs(3);

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Settings {
    #[serde(skip_serializing_if = "std::ops::Not::not")]
    pub enabled: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub endpoint: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub host_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub key_file: Option<PathBuf>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub counter_journal: Option<PathBuf>,
}

impl Settings {
    pub fn check(&self) -> io::Result<()> {
        if !self.enabled {
            return Ok(());
        }
        let endpoint = self
            .endpoint
            .as_deref()
            .ok_or_else(|| missing("endpoint"))?;
        let url = url::Url::parse(endpoint).map_err(io::Error::other)?;
        if url.scheme() != "https"
            || url.host_str().is_none()
            || !url.username().is_empty()
            || url.password().is_some()
            || url.query().is_some()
            || url.fragment().is_some()
            || url.path() != "/"
        {
            return Err(io::Error::other("metrics endpoint must be an HTTPS origin"));
        }
        if !self.host_id.as_deref().is_some_and(motor_lease::valid_name) {
            return Err(missing("host-id ([A-Za-z0-9._-]{1,64})"));
        }
        Ok(())
    }
}

fn missing(field: &str) -> io::Error {
    io::Error::other(format!("metrics-push requires --metrics-{field}"))
}

fn identity_paths(config: &Config) -> io::Result<(PathBuf, PathBuf)> {
    if !motor_lease::valid_name(&config.instance) || matches!(config.instance.as_str(), "." | "..")
    {
        return Err(io::Error::other(
            "metrics instance must match [A-Za-z0-9._-]{1,64}",
        ));
    }
    // Only filenames are configurable: confining both files to the private
    // monitor directory keeps these files separate from job state and the
    // motor-host signing key that authenticates placement traffic.
    let root = config.state_dir().join("monitor");
    let filename = |name: &Option<PathBuf>, default: &str| -> io::Result<PathBuf> {
        let path = name.clone().unwrap_or_else(|| default.into());
        if path.components().count() != 1
            || !matches!(
                path.components().next(),
                Some(std::path::Component::Normal(_))
            )
        {
            return Err(io::Error::other(
                "metrics identity paths must be filenames under state/INSTANCE/monitor",
            ));
        }
        Ok(root.join(path))
    };
    let key = filename(&config.metrics.key_file, "host-key")?;
    let counter = filename(&config.metrics.counter_journal, "counter")?;
    if key == counter {
        return Err(io::Error::other("metrics key and counter must be distinct"));
    }
    Ok((key, counter))
}

/// Create once, then print the stable public identity without opening a listener.
pub fn public_key(config: &Config) -> io::Result<String> {
    let (key, _) = identity_paths(config)?;
    Ok(identity::key(&key)?
        .verifying_key()
        .as_bytes()
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect())
}

pub fn start(config: &Config) -> io::Result<()> {
    if !config.metrics.enabled {
        return Ok(());
    }
    config.metrics.check()?;
    let config = config.clone();
    std::thread::Builder::new()
        .name("metrics-push".into())
        .spawn(move || {
            if run(&config).is_err() {
                // Raw errors can contain paths or transport credentials.
                tracing::debug!(code = "monitor_start_failed", "monitor_metrics_stopped");
            }
        })?;
    Ok(())
}

/// The standalone mode does not construct a Runner, backend, or listener.
pub fn run(config: &Config) -> io::Result<()> {
    if !config.metrics.enabled {
        return Err(io::Error::other(
            "metrics-push requires explicit --metrics-push",
        ));
    }
    config.metrics.check()?;
    let (key_path, counter_path) = identity_paths(config)?;
    let key = identity::key(&key_path)?;
    let mut counter = identity::Counter::open(&counter_path)?;
    let client = client()?;
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()?;
    let mut sampler = sample::Sampler::default();
    let mut last_error: Option<&'static str> = None;
    let mut deadline = std::time::Instant::now();
    loop {
        let now = unix_ms();
        match sampler.sample(now) {
            Ok(Some(mut record)) => {
                match runtime.block_on(local_runner(config)) {
                    Ok(mut runner) => {
                        if let Some(code) = last_error {
                            runner.detail = Some(code.to_owned());
                        }
                        record.runners.push(runner);
                    }
                    Err(_) => last_error = Some("runner_health_unavailable"),
                }
                if send(&client, &config.metrics, &key, &mut counter, &record).is_err() {
                    last_error = Some("metrics_push_failed");
                    tracing::debug!(code = "metrics_push_failed", "monitor_metrics_dropped");
                }
            }
            Ok(None) => {}
            Err(_) => {
                last_error = Some("host_sample_failed");
                tracing::debug!(code = "host_sample_failed", "monitor_metrics_dropped");
            }
        }
        // Prime CPU deltas without publishing an uptime average; subsequent
        // starts are one minute apart, with missed deadlines skipped.
        deadline += Duration::from_secs(60);
        while deadline <= std::time::Instant::now() {
            deadline += Duration::from_secs(60);
        }
        std::thread::sleep(deadline.saturating_duration_since(std::time::Instant::now()));
    }
}

fn unix_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis()
        .try_into()
        .unwrap_or(u64::MAX)
}

fn client() -> io::Result<reqwest::blocking::Client> {
    reqwest::blocking::Client::builder()
        .timeout(TIMEOUT)
        .retry(reqwest::retry::never())
        .redirect(reqwest::redirect::Policy::none())
        .build()
        .map_err(io::Error::other)
}

fn signed(
    record: &HostMetrics,
    host: &str,
    key: &SigningKey,
    counter: u64,
    now: u64,
) -> io::Result<(String, Vec<u8>, HostHeaders)> {
    let body = record.encode(now).map_err(io::Error::other)?;
    let path = format!("/v1/hosts/{host}/metrics");
    let headers = HostHeaders::sign(host, "POST", &path, &body, now, counter, key)
        .map_err(io::Error::other)?;
    Ok((path, body, headers))
}

fn send(
    client: &reqwest::blocking::Client,
    settings: &Settings,
    key: &SigningKey,
    counter: &mut identity::Counter,
    record: &HostMetrics,
) -> io::Result<()> {
    let now = unix_ms();
    record.encode(now).map_err(io::Error::other)?;
    let reserved = counter.reserve(now)?;
    let (path, body, headers) = signed(
        record,
        settings
            .host_id
            .as_deref()
            .ok_or_else(|| missing("host-id"))?,
        key,
        reserved,
        now,
    )?;
    let response = client
        .post(format!(
            "{}{}",
            settings
                .endpoint
                .as_deref()
                .ok_or_else(|| missing("endpoint"))?
                .trim_end_matches('/'),
            path
        ))
        .header("CK-Host", headers.host)
        .header("CK-Timestamp", headers.timestamp.to_string())
        .header("CK-Counter", headers.counter.to_string())
        .header("CK-Signature", headers.signature)
        .header("Content-Type", "application/json")
        .body(body)
        .send()
        .map_err(io::Error::other)?;
    let status = response.status().as_u16();
    let mut body = Vec::new();
    response.take(512).read_to_end(&mut body)?;
    match parse_reply(status, &body) {
        MetricsReply::Accepted => Ok(()),
        MetricsReply::BadRequest { field } => {
            tracing::debug!(status, field, "monitor_metrics_dropped");
            Err(io::Error::other("metrics_bad_request"))
        }
        _ => {
            tracing::debug!(status, "monitor_metrics_dropped");
            Err(io::Error::other("metrics_refused"))
        }
    }
}

async fn local_runner(config: &Config) -> io::Result<Runner> {
    use motor_protocol::*;
    tokio::time::timeout(TIMEOUT, async {
        let mut socket = crate::local_transport::connect(&config.socket_path()).await?;
        write_frame_async(
            &mut socket,
            &Frame::Hello {
                return_paths: false,
                return_chunked_max_bytes: None,
                return_chunked: false,
                sibling_paths: false,
                protocol_version: PROTOCOL_VERSION,
                started: true,
                default_branch_tip: false,
                gate_jobs: false,
                platforms: vec![],
            },
        )
        .await?;
        loop {
            match read_frame_async(&mut socket).await? {
                Some(Frame::Hello {
                    protocol_version: PROTOCOL_VERSION,
                    ..
                }) => break,
                Some(Frame::Heartbeat { .. }) => {}
                _ => return Err(io::Error::other("metrics runner hello unavailable")),
            }
        }
        write_frame_async(&mut socket, &Frame::Status).await?;
        loop {
            match read_frame_async(&mut socket).await? {
                Some(Frame::StatusReply(reply)) => return runner_record(&reply),
                Some(Frame::Heartbeat { .. }) => {}
                _ => return Err(io::Error::other("metrics runner status unavailable")),
            }
        }
    })
    .await
    .map_err(io::Error::other)?
}

fn runner_record(reply: &motor_protocol::RunnerStatusReply) -> io::Result<Runner> {
    let health = reply
        .health
        .as_deref()
        .ok_or_else(|| io::Error::other("metrics health missing"))?;
    Ok(Runner {
        name: "direct".into(),
        state: if reply.status.server_reachable {
            RunnerState::Ready
        } else {
            RunnerState::Error
        },
        slots_total: u64::from(
            health
                .permits_cap
                .ok_or_else(|| io::Error::other("metrics slots missing"))?,
        ),
        slots_used: u64::from(
            health
                .permits_held
                .ok_or_else(|| io::Error::other("metrics permits missing"))?,
        ),
        queued: u64::from(health.queue_depth),
        oldest_wait_s: health
            .oldest_wait_s
            .ok_or_else(|| io::Error::other("metrics queue age missing"))?,
        detail: (!reply.status.server_reachable).then(|| "runner_not_ready".into()),
    })
}
