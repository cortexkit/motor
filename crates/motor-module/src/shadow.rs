//! Best-effort, memory-only comparisons. No shadow state enters the job store.
use crate::Config;
use motor_protocol::{Outcome, Output, OutputStream, RunRequest, TerminalRecord, Uuid};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::{
    collections::HashMap,
    fs::{self, OpenOptions},
    io::{self, BufRead, Write},
    path::{Path, PathBuf},
    sync::{
        atomic::{AtomicUsize, Ordering},
        Arc, Mutex,
    },
};
use tokio::sync::{oneshot, watch};

const TAIL_BYTES: usize = 16 * 1024;
const LOG_BYTES: u64 = 4 * 1024 * 1024;

pub(crate) fn eligible(config: &Config, request: &RunRequest, direct: bool, returns: bool) -> bool {
    config.shadow_to_hosted.enabled
        && config.spill_avoid_host.is_some()
        && !direct
        && !returns
        && request.network.is_none()
        && matches!(
            request.platform,
            None | Some(motor_protocol::Platform::Linux)
        )
        && request.weight_hint.unwrap_or(2) <= 8
        && crate::hosted::validate(request).is_ok()
}

fn sampled(rate: f64) -> bool {
    let mut bytes = [0; 8];
    getrandom::fill(&mut bytes).is_ok()
        && (u64::from_le_bytes(bytes) as f64 / (u64::MAX as f64 + 1.0)) < rate
}
fn digest(bytes: &[u8]) -> String {
    format!("{:x}", Sha256::digest(bytes))
}

#[derive(Default)]
struct Stream {
    hash: Sha256,
    bytes: u64,
    tail: Vec<u8>,
}
impl Stream {
    fn push(&mut self, bytes: &[u8]) {
        self.hash.update(bytes);
        self.bytes = self.bytes.saturating_add(bytes.len() as u64);
        if bytes.len() >= TAIL_BYTES {
            self.tail.clear();
            self.tail
                .extend_from_slice(&bytes[bytes.len() - TAIL_BYTES..]);
        } else {
            let remove = (self.tail.len() + bytes.len()).saturating_sub(TAIL_BYTES);
            self.tail.drain(..remove);
            self.tail.extend_from_slice(bytes);
        }
    }
    fn finish(self) -> StreamSummary {
        StreamSummary {
            sha256: format!("{:x}", self.hash.finalize()),
            bytes: self.bytes,
            tail: Some(String::from_utf8_lossy(&self.tail).into_owned()),
        }
    }
}
#[derive(Default)]
pub(crate) struct Capture {
    stdout: Stream,
    stderr: Stream,
}
impl Capture {
    pub(crate) fn output(&mut self, output: &Output) {
        match output.stream {
            OutputStream::Stdout => self.stdout.push(&output.bytes.0),
            OutputStream::Stderr => self.stderr.push(&output.bytes.0),
            _ => {}
        }
    }
    pub(crate) fn finish(self, terminal: &TerminalRecord) -> Side {
        Side {
            outcome: terminal.outcome.clone(),
            exit_code: match terminal.outcome {
                Outcome::Exit { code } => Some(code),
                _ => None,
            },
            run_ms: terminal.wall_ms,
            stdout: self.stdout.finish(),
            stderr: self.stderr.finish(),
        }
    }
}
#[derive(Debug, Serialize, Deserialize)]
pub(crate) struct StreamSummary {
    pub sha256: String,
    pub bytes: u64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tail: Option<String>,
}
#[derive(Debug, Serialize, Deserialize)]
pub(crate) struct Side {
    pub outcome: Outcome,
    pub exit_code: Option<i32>,
    pub run_ms: u64,
    pub stdout: StreamSummary,
    pub stderr: StreamSummary,
}
#[derive(Debug, Serialize, Deserialize)]
pub(crate) struct Record {
    pub timestamp_s: u64,
    pub job_id: Uuid,
    pub repository_hash: String,
    pub workspace_key_hash: String,
    pub command_sha256: String,
    pub build_server: Side,
    pub shadow: Side,
    pub mismatch: Vec<String>,
    pub shadow_failure: Option<String>,
}
impl Record {
    pub(crate) fn compare(
        id: Uuid,
        request: &RunRequest,
        primary: Side,
        shadow: Side,
        failure: Option<String>,
    ) -> Self {
        let mut mismatch = Vec::new();
        if matches!(shadow.outcome, Outcome::RefusedBeforeStart { .. }) {
            mismatch.push("shadow_refused_before_start".into());
        }
        if std::mem::discriminant(&primary.outcome) != std::mem::discriminant(&shadow.outcome) {
            mismatch.push("outcome_kind_differs".into());
        } else if primary.outcome != shadow.outcome {
            mismatch.push(
                if primary.exit_code != shadow.exit_code {
                    "exit_differs"
                } else {
                    "outcome_differs"
                }
                .into(),
            );
        }
        if primary.stdout.sha256 != shadow.stdout.sha256
            || primary.stderr.sha256 != shadow.stderr.sha256
        {
            mismatch.push("output_differs".into());
        }
        if failure.is_some() && mismatch.is_empty() {
            mismatch.push("shadow_failure".into());
        }
        let mut record = Self {
            timestamp_s: crate::hosted::now_s(),
            job_id: id,
            repository_hash: digest(request.repository_root.as_bytes()),
            workspace_key_hash: digest(request.workspace_key.as_bytes()),
            command_sha256: digest(request.command.as_bytes()),
            build_server: primary,
            shadow,
            mismatch,
            shadow_failure: failure,
        };
        if record.mismatch.is_empty() {
            record.build_server.stdout.tail = None;
            record.build_server.stderr.tail = None;
            record.shadow.stdout.tail = None;
            record.shadow.stderr.tail = None;
        }
        record
    }
}

struct Primary {
    capture: Capture,
    terminal: oneshot::Sender<Side>,
    last_output_seq: Option<u64>,
}
pub(crate) struct Manager {
    root: PathBuf,
    primary: Mutex<HashMap<Uuid, Primary>>,
    running: Arc<AtomicUsize>,
    stop: watch::Sender<bool>,
    tasks: Mutex<Vec<tokio::task::JoinHandle<()>>>,
    log: Mutex<()>,
}
pub(crate) struct Permit(Arc<AtomicUsize>);
impl Drop for Permit {
    fn drop(&mut self) {
        self.0.fetch_sub(1, Ordering::Relaxed);
    }
}
impl Manager {
    pub(crate) fn new(root: &Path) -> Self {
        Self {
            root: root.to_owned(),
            primary: Mutex::new(HashMap::new()),
            running: Arc::new(AtomicUsize::new(0)),
            stop: watch::channel(false).0,
            tasks: Mutex::new(Vec::new()),
            log: Mutex::new(()),
        }
    }
    pub(crate) fn begin(
        &self,
        id: Uuid,
        config: &Config,
    ) -> Option<(Permit, oneshot::Receiver<Side>, watch::Receiver<bool>)> {
        if *self.stop.borrow() || !sampled(config.shadow_to_hosted.sample_rate) {
            return None;
        }
        let mut current = self.running.load(Ordering::Relaxed);
        loop {
            if current >= config.shadow_to_hosted.max_concurrent {
                return None;
            }
            match self.running.compare_exchange_weak(
                current,
                current + 1,
                Ordering::Relaxed,
                Ordering::Relaxed,
            ) {
                Ok(_) => break,
                Err(n) => current = n,
            }
        }
        let (tx, rx) = oneshot::channel();
        self.primary.lock().unwrap().insert(
            id,
            Primary {
                capture: Capture::default(),
                terminal: tx,
                last_output_seq: None,
            },
        );
        Some((Permit(self.running.clone()), rx, self.stop.subscribe()))
    }
    pub(crate) fn track(&self, task: tokio::task::JoinHandle<()>) {
        let mut tasks = self.tasks.lock().unwrap();
        tasks.retain(|task| !task.is_finished());
        tasks.push(task);
    }
    #[cfg(test)]
    pub(crate) async fn join_for_test(&self) {
        let tasks = std::mem::take(&mut *self.tasks.lock().unwrap());
        for task in tasks {
            task.await.unwrap();
        }
    }
    pub(crate) fn output(&self, id: Uuid, output: &Output) {
        if let Some(primary) = self.primary.lock().unwrap().get_mut(&id) {
            // Concurrent caller attaches can replay the same primary output.
            // Hash each sequence once rather than comparing replay duplicates.
            if primary.last_output_seq.is_none_or(|seq| output.seq > seq) {
                primary.last_output_seq = Some(output.seq);
                primary.capture.output(output);
            }
        }
    }
    pub(crate) fn terminal(&self, id: Uuid, terminal: &TerminalRecord) {
        if let Some(primary) = self.primary.lock().unwrap().remove(&id) {
            let _ = primary.terminal.send(primary.capture.finish(terminal));
        }
    }
    pub(crate) fn forget(&self, id: Uuid) {
        self.primary.lock().unwrap().remove(&id);
    }
    pub(crate) fn append(&self, record: &Record) -> io::Result<()> {
        let _lock = self.log.lock().unwrap();
        append(&self.root, record, LOG_BYTES)
    }
    pub(crate) fn health(&self) -> io::Result<serde_json::Value> {
        health(&self.root)
    }
    pub(crate) fn report(&self, since: u64) -> io::Result<Vec<Record>> {
        records(&self.root, since).map(|records| {
            records
                .into_iter()
                .filter(|r| !r.mismatch.is_empty())
                .collect()
        })
    }
    pub(crate) async fn shutdown(&self) {
        self.stop.send_replace(true);
        let tasks = std::mem::take(&mut *self.tasks.lock().unwrap());
        for mut task in tasks {
            if tokio::time::timeout(std::time::Duration::from_secs(8), &mut task)
                .await
                .is_err()
            {
                task.abort();
                let _ = task.await;
                tracing::warn!("shadow cleanup exceeded shutdown deadline");
            }
        }
        self.primary.lock().unwrap().clear();
    }
}

fn append(root: &Path, record: &Record, cap: u64) -> io::Result<()> {
    let mut bytes = serde_json::to_vec(record).map_err(io::Error::other)?;
    bytes.push(b'\n');
    let path = root.join("shadow.jsonl");
    fs::create_dir_all(root)?;
    if fs::metadata(&path).is_ok_and(|m| m.len().saturating_add(bytes.len() as u64) > cap) {
        fs::rename(&path, root.join("shadow.jsonl.1"))?;
    }
    let mut options = OpenOptions::new();
    options.create(true).append(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    options.open(path)?.write_all(&bytes)
}
pub(crate) fn records(root: &Path, since: u64) -> io::Result<Vec<Record>> {
    let mut records = Vec::new();
    for name in ["shadow.jsonl.1", "shadow.jsonl"] {
        let file = match fs::File::open(root.join(name)) {
            Ok(file) => file,
            Err(e) if e.kind() == io::ErrorKind::NotFound => continue,
            Err(e) => return Err(e),
        };
        for line in io::BufReader::new(file).lines() {
            // An interrupted final append must not hide the rest of the report.
            if let Ok(record) = serde_json::from_str::<Record>(&line?) {
                if record.timestamp_s >= since {
                    records.push(record);
                }
            }
        }
    }
    Ok(records)
}
fn health(root: &Path) -> io::Result<serde_json::Value> {
    let records = records(root, crate::hosted::now_s().saturating_sub(3600))?;
    Ok(serde_json::json!({ "window_s": 3600, "runs": records.len(),
        "matches": records.iter().filter(|r| r.mismatch.is_empty()).count(),
        "mismatches": records.iter().filter(|r| !r.mismatch.is_empty()).count(),
        "shadow_failures": records.iter().filter(|r| r.shadow_failure.is_some()).count() }))
}

#[cfg(test)]
mod tests {
    use super::*;
    use cortexkit_test_support::ScratchDir;
    fn request() -> RunRequest {
        RunRequest::new(
            "/home/test/repo",
            "/home/test/repo",
            "/home/test/repo",
            "true",
        )
        .with_timeout(30)
    }
    fn config() -> Config {
        Config {
            shadow_to_hosted: crate::ShadowConfig {
                enabled: true,
                sample_rate: 1.0,
                max_concurrent: 2,
            },
            spill_avoid_host: Some("build-server".into()),
            ..Config::default()
        }
    }
    fn terminal(code: i32) -> TerminalRecord {
        TerminalRecord::new(Uuid::now_v7(), Outcome::Exit { code }, 0, 123, 0)
    }
    fn side(code: i32, text: &[u8]) -> Side {
        let mut capture = Capture::default();
        capture.output(&Output::new(
            0,
            OutputStream::Stdout,
            motor_protocol::BytePayload(text.to_vec()),
        ));
        capture.output(&Output::new(
            1,
            OutputStream::Stderr,
            motor_protocol::BytePayload(text.to_vec()),
        ));
        capture.finish(&terminal(code))
    }
    #[test]
    fn shadow_defaults_omit_config_and_validate_rate() {
        assert!(serde_json::to_value(Config::default())
            .unwrap()
            .get("shadow_to_hosted")
            .is_none());
        let mut config = config();
        for rate in [-0.1, 1.1, f64::NAN] {
            config.shadow_to_hosted.sample_rate = rate;
            assert!(config.validate().is_err());
        }
        config.shadow_to_hosted.sample_rate = 0.0;
        assert!(config.validate().is_ok());
        config.shadow_to_hosted.max_concurrent = 0;
        assert!(config.validate().is_err());
    }
    #[test]
    fn shadow_eligibility_excludes_net_windows_oversize_secrets_returns_and_unknown_host() {
        let mut config = config();
        let mut request = request();
        assert!(eligible(&config, &request, false, false));
        assert!(!eligible(&config, &request, true, false));
        assert!(!eligible(&config, &request, false, true));
        request.network = Some(motor_protocol::Network::Outbound);
        assert!(!eligible(&config, &request, false, false));
        request.network = None;
        request.platform = Some(motor_protocol::Platform::Windows);
        assert!(!eligible(&config, &request, false, false));
        request.platform = None;
        request.weight_hint = Some(9);
        assert!(!eligible(&config, &request, false, false));
        request.weight_hint = Some(8);
        assert!(eligible(&config, &request, false, false));
        config.spill_avoid_host = None;
        assert!(!eligible(&config, &request, false, false));
    }
    #[tokio::test]
    async fn shadow_concurrency_cap_and_zero_sampling_are_nonblocking() {
        let dir = ScratchDir::new("shadow-cap");
        let manager = Manager::new(&dir);
        let mut config = config();
        config.shadow_to_hosted.sample_rate = 0.0;
        assert!(manager.begin(Uuid::now_v7(), &config).is_none());
        config.shadow_to_hosted.sample_rate = 1.0;
        let first = manager.begin(Uuid::now_v7(), &config).unwrap();
        let second = manager.begin(Uuid::now_v7(), &config).unwrap();
        assert!(manager.begin(Uuid::now_v7(), &config).is_none());
        drop(first);
        assert!(manager.begin(Uuid::now_v7(), &config).is_some());
        drop(second);
        manager.shutdown().await;
        assert!(manager.begin(Uuid::now_v7(), &config).is_none());
        assert_eq!(
            fs::read_dir(&dir).unwrap().count(),
            0,
            "selection never writes the store"
        );
    }
    #[test]
    fn mismatch_has_both_stream_tails_hashes_and_byte_counts() {
        let primary = vec![b'a'; TAIL_BYTES + 123];
        let shadow = vec![b'b'; TAIL_BYTES + 456];
        let record = Record::compare(
            Uuid::now_v7(),
            &request(),
            side(0, &primary),
            side(7, &shadow),
            None,
        );
        assert_eq!(record.mismatch, ["exit_differs", "output_differs"]);
        for stream in [&record.build_server.stdout, &record.build_server.stderr] {
            assert_eq!(stream.tail.as_ref().unwrap().as_bytes(), &primary[123..]);
            assert_eq!(stream.bytes, primary.len() as u64);
            assert_eq!(stream.sha256, digest(&primary));
        }
        for stream in [&record.shadow.stdout, &record.shadow.stderr] {
            assert_eq!(stream.tail.as_ref().unwrap().as_bytes(), &shadow[456..]);
            assert_eq!(stream.bytes, shadow.len() as u64);
            assert_eq!(stream.sha256, digest(&shadow));
        }
        let matched = Record::compare(
            Uuid::now_v7(),
            &request(),
            side(0, b"ok"),
            side(0, b"ok"),
            None,
        );
        assert!(matched.mismatch.is_empty());
        assert!(matched.build_server.stdout.tail.is_none());
        assert!(matched.shadow.stderr.tail.is_none());
    }
    #[test]
    fn shadow_log_rotates_and_report_since_and_health_read_both_files() {
        let dir = ScratchDir::new("shadow-log");
        let mut record = Record::compare(
            Uuid::now_v7(),
            &request(),
            side(0, b"left"),
            side(1, b"right"),
            Some("refused".into()),
        );
        record.timestamp_s = crate::hosted::now_s() - 20;
        append(&dir, &record, 1).unwrap();
        append(&dir, &record, 1).unwrap();
        append(&dir, &record, 1).unwrap();
        assert_eq!(records(&dir, 0).unwrap().len(), 2);
        assert_eq!(records(&dir, record.timestamp_s + 1).unwrap().len(), 0);
        let health = health(&dir).unwrap();
        assert_eq!(health["runs"], 2);
        assert_eq!(health["mismatches"], 2);
        assert_eq!(health["shadow_failures"], 2);
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            assert_eq!(
                fs::metadata(dir.join("shadow.jsonl"))
                    .unwrap()
                    .permissions()
                    .mode()
                    & 0o777,
                0o600
            );
        }
    }
}
