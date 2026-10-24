//! A public-only SSH agent bridges authentication requests to the vault signer.
use base64::Engine;
use cortexkit_claustrum_client::{CredentialErrorClass, CredentialPublicKey, CredentialResolver};
use std::{
    fs, io,
    os::unix::fs::DirBuilderExt,
    path::{Path, PathBuf},
    sync::{
        atomic::{AtomicBool, Ordering},
        Arc, Mutex,
    },
    time::{Duration, Instant},
};
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    net::{UnixListener, UnixStream},
    sync::{oneshot, Mutex as AsyncMutex},
    task::{JoinHandle, JoinSet},
};

const ALGORITHM: &[u8] = b"ssh-ed25519";
const MAX_PACKET: usize = 256 * 1024;
const FAILURE: u8 = 5;
/// How long the agent waits before asking the vault again after any sign
/// failure other than a missing credential or a refused grant: the vault was
/// unavailable, busy, restarting or timed out, or sent a reply the agent could
/// not use.
pub(crate) const TRANSIENT_SIGN_BACKOFF: Duration = Duration::from_secs(2);
/// How long the agent waits before asking the vault again after the signing
/// credential was missing or its grant was refused. An operator fixes those by
/// re-creating the credential or re-approving the grant, without restarting
/// ck-motor, so they are retried too, just less often.
pub(crate) const REPAIRABLE_SIGN_BACKOFF: Duration = Duration::from_secs(30);

struct SignFault {
    code: &'static str,
    /// The vault is not asked to sign again before this instant.
    retry_at: Instant,
}

/// The most recent vault signing failure, shared by every connection to one
/// agent. A failure never latches: it is cleared by the next successful sign,
/// and once its backoff has passed one sign request asks the vault again.
pub(crate) struct SignState {
    fault: Mutex<Option<SignFault>>,
    /// A stopped clock that replaces the real one. Always `None` outside tests,
    /// which stop it so that only `advance` moves this agent's time.
    paused_at: Mutex<Option<Instant>>,
}
impl SignState {
    fn new() -> Self {
        Self {
            fault: Mutex::new(None),
            paused_at: Mutex::new(None),
        }
    }
    fn now(&self) -> Instant {
        self.paused_at.lock().unwrap().unwrap_or_else(Instant::now)
    }
    #[cfg(test)]
    pub(crate) fn pause(&self) {
        self.paused_at
            .lock()
            .unwrap()
            .get_or_insert_with(Instant::now);
    }
    #[cfg(test)]
    pub(crate) fn advance(&self, by: Duration) {
        let mut paused_at = self.paused_at.lock().unwrap();
        *paused_at = Some(paused_at.expect("pause the sign clock before advancing it") + by);
    }
    fn backoff(code: &str) -> Duration {
        match code {
            "runner_sign_missing" | "runner_sign_grant_refused" => REPAIRABLE_SIGN_BACKOFF,
            _ => TRANSIENT_SIGN_BACKOFF,
        }
    }
    /// Whether a sign request may call the vault now. While failing, only the
    /// first request after the backoff is admitted, and admitting it restarts
    /// the backoff, so a burst of jobs against a dead vault makes one call.
    fn admit(&self) -> bool {
        let now = self.now();
        match self.fault.lock().unwrap().as_mut() {
            None => true,
            Some(fault) if now >= fault.retry_at => {
                fault.retry_at = now + Self::backoff(fault.code);
                true
            }
            Some(_) => false,
        }
    }
    fn succeeded(&self) {
        if let Some(previous) = self.fault.lock().unwrap().take() {
            tracing::info!(previous = previous.code, "runner signing recovered");
        }
    }
    fn failed(&self, code: &'static str, class: CredentialErrorClass) {
        let retry_at = self.now() + Self::backoff(code);
        let previous = self
            .fault
            .lock()
            .unwrap()
            .replace(SignFault { code, retry_at })
            .map(|fault| fault.code);
        // Log state changes only, so a dead vault does not flood the log. The
        // code and error class are safe; the vault's message is not logged.
        if previous != Some(code) {
            tracing::warn!(
                code,
                class = class.classifier_code(),
                retry_after_ms = Self::backoff(code).as_millis() as u64,
                "runner signing failed"
            );
        }
    }
    /// The failure that refuses new jobs: held only until its backoff passes,
    /// so the next job reaches SSH and its sign request asks the vault again.
    fn refusal(&self) -> Option<&'static str> {
        let now = self.now();
        self.fault
            .lock()
            .unwrap()
            .as_ref()
            .filter(|fault| now < fault.retry_at)
            .map(|fault| fault.code)
    }
    /// How many milliseconds a caller refused with `code` should wait before
    /// trying again: the time left until the current failure's backoff ends,
    /// at least 1 and at most `TRANSIENT_SIGN_BACKOFF`. Only an unavailable
    /// vault or an unusable vault reply clears on its own, so only those two
    /// codes get a hint. A missing credential or a refused grant waits for an
    /// operator, and a hint would invite a caller to retry it in a loop.
    /// `None` too when `code` is no longer the current failure.
    fn retry_after_ms(&self, code: &str) -> Option<u64> {
        if !matches!(
            code,
            "runner_sign_unavailable" | "runner_sign_invalid_response"
        ) {
            return None;
        }
        let now = self.now();
        let fault = self.fault.lock().unwrap();
        let fault = fault.as_ref().filter(|fault| fault.code == code)?;
        let left = fault.retry_at.saturating_duration_since(now);
        // Round up, so a hint never ends before the backoff does.
        let left_ms = u64::try_from(left.as_micros().div_ceil(1000)).unwrap_or(u64::MAX);
        Some(left_ms.clamp(1, TRANSIENT_SIGN_BACKOFF.as_millis() as u64))
    }
    /// The failure health reports: held until a sign succeeds.
    fn current(&self) -> Option<&'static str> {
        self.fault.lock().unwrap().as_ref().map(|fault| fault.code)
    }
}

struct RuntimeDir(PathBuf);
impl Drop for RuntimeDir {
    fn drop(&mut self) {
        for name in ["s", "m", "identity.pub"] {
            let _ = fs::remove_file(self.0.join(name));
        }
        let _ = fs::remove_dir(&self.0);
    }
}

pub(crate) struct VaultAgent {
    pub socket: PathBuf,
    pub identity: PathBuf,
    pub control_path: PathBuf,
    alive: Arc<AtomicBool>,
    signing: Arc<SignState>,
    stop: Mutex<Option<oneshot::Sender<()>>>,
    task: AsyncMutex<Option<JoinHandle<()>>>,
}
impl VaultAgent {
    pub async fn start(
        root: &Path,
        public: CredentialPublicKey,
        resolver: Arc<dyn CredentialResolver>,
    ) -> io::Result<Self> {
        let raw = decode_hex(&public.public_key_hex, 32)?;
        if public.algorithm != "ed25519" {
            return Err(io::Error::other("runner key must be Ed25519"));
        }
        let mut key_blob = Vec::new();
        string(&mut key_blob, ALGORITHM);
        string(&mut key_blob, &raw);
        // Keep the suffix short: Unix domain socket paths have a small OS limit.
        let nonce = motor_protocol::Uuid::now_v7().simple().to_string();
        let path = root.join(format!("a{}", &nonce[26..]));
        fs::DirBuilder::new().mode(0o700).create(&path)?;
        // Own cleanup only after creation succeeds; never remove a colliding dir.
        let runtime = RuntimeDir(path);
        let socket = runtime.0.join("s");
        let identity = runtime.0.join("identity.pub");
        let control_path = runtime.0.join("m");
        let listener = UnixListener::bind(&socket)?;
        fs::write(
            &identity,
            format!(
                "ssh-ed25519 {}\n",
                base64::engine::general_purpose::STANDARD.encode(&key_blob)
            ),
        )?;
        let alive = Arc::new(AtomicBool::new(true));
        let running = Arc::clone(&alive);
        let signing = Arc::new(SignState::new());
        let shared = Arc::clone(&signing);
        let (stop, mut stopped) = oneshot::channel();
        let task = tokio::spawn(async move {
            let mut clients = JoinSet::new();
            loop {
                tokio::select! {
                    _ = &mut stopped => break,
                    accepted = listener.accept() => match accepted {
                        Ok((stream, _)) => {
                            let key = key_blob.clone();
                            let key_id = public.key_id.clone();
                            let resolver = Arc::clone(&resolver);
                            let signing = Arc::clone(&shared);
                            clients.spawn(serve(stream, key, key_id, resolver, signing));
                        }
                        Err(_) => break,
                    },
                    _ = clients.join_next(), if !clients.is_empty() => {},
                }
            }
            clients.abort_all();
            while clients.join_next().await.is_some() {}
            running.store(false, Ordering::Release);
            drop(listener);
            drop(runtime);
        });
        Ok(Self {
            socket,
            identity,
            control_path,
            alive,
            signing,
            stop: Mutex::new(Some(stop)),
            task: AsyncMutex::new(Some(task)),
        })
    }
    /// The code that refuses new jobs before start. A sign failure refuses
    /// only during its backoff; after it, jobs try again.
    pub fn refusal(&self) -> Option<&'static str> {
        self.signing.refusal().or_else(|| self.stopped_refusal())
    }
    /// The code health reports: a sign failure stays visible until a sign
    /// succeeds, even after its backoff has passed.
    pub fn health_refusal(&self) -> Option<&'static str> {
        self.signing.current().or_else(|| self.stopped_refusal())
    }
    /// The `retry_after_ms` hint for a job refused with `code`; see
    /// `SignState::retry_after_ms`.
    pub fn retry_after_ms(&self, code: &str) -> Option<u64> {
        self.signing.retry_after_ms(code)
    }
    fn stopped_refusal(&self) -> Option<&'static str> {
        (!self.alive.load(Ordering::Acquire)).then_some("runner_agent_stopped")
    }
    #[cfg(test)]
    pub(crate) fn signing(&self) -> &SignState {
        &self.signing
    }
    pub async fn shutdown(&self) {
        self.alive.store(false, Ordering::Release);
        self.stop.lock().unwrap().take();
        if let Some(task) = self.task.lock().await.take() {
            let _ = task.await;
        }
    }
}
impl Drop for VaultAgent {
    fn drop(&mut self) {
        self.stop.lock().unwrap().take();
    }
}

fn decode_hex(text: &str, size: usize) -> io::Result<Vec<u8>> {
    if text.len() != size * 2 || !text.is_ascii() {
        return Err(io::Error::other("invalid vault response length"));
    }
    (0..text.len())
        .step_by(2)
        .map(|i| u8::from_str_radix(&text[i..i + 2], 16).map_err(io::Error::other))
        .collect()
}
fn string(out: &mut Vec<u8>, bytes: &[u8]) {
    out.extend_from_slice(&(bytes.len() as u32).to_be_bytes());
    out.extend_from_slice(bytes);
}
fn take_string<'a>(bytes: &mut &'a [u8]) -> Option<&'a [u8]> {
    let (size, rest) = bytes.split_at_checked(4)?;
    let size = u32::from_be_bytes(size.try_into().ok()?) as usize;
    let (value, rest) = rest.split_at_checked(size)?;
    *bytes = rest;
    Some(value)
}
fn sign_request<'a>(packet: &'a [u8], key: &[u8]) -> Option<&'a [u8]> {
    let mut fields = packet.get(1..)?;
    if take_string(&mut fields)? != key {
        return None;
    }
    let data = take_string(&mut fields)?;
    // Ed25519 has no RSA hash-selection flags. Reject malformed/foreign requests.
    (fields == [0, 0, 0, 0]).then_some(data)
}
async fn serve(
    mut stream: UnixStream,
    key: Vec<u8>,
    key_id: String,
    resolver: Arc<dyn CredentialResolver>,
    signing: Arc<SignState>,
) {
    loop {
        let packet = tokio::time::timeout(Duration::from_secs(30), async {
            let len = stream.read_u32().await? as usize;
            if len == 0 || len > MAX_PACKET {
                return Err(io::Error::other("invalid agent packet size"));
            }
            let mut bytes = vec![0; len];
            stream.read_exact(&mut bytes).await?;
            Ok::<_, io::Error>(bytes)
        })
        .await;
        let Ok(Ok(packet)) = packet else { break };
        let mut response = vec![FAILURE];
        match packet[0] {
            11 if packet.len() == 1 => {
                response = vec![12]; // SSH2_AGENT_IDENTITIES_ANSWER
                response.extend_from_slice(&1u32.to_be_bytes());
                string(&mut response, &key);
                string(&mut response, b"");
            }
            13 => {
                if let Some(data) = sign_request(&packet, &key) {
                    if signing.admit() {
                        match resolver.sign(crate::RUNNER_CREDENTIAL_ID, data).await {
                            Ok(signed) => {
                                if let Ok(signature) = decode_hex(&signed.signature_hex, 64)
                                    .and_then(|signature| {
                                        if signed.key_id == key_id {
                                            Ok(signature)
                                        } else {
                                            Err(io::Error::other("vault signing key changed"))
                                        }
                                    })
                                {
                                    let mut blob = Vec::new();
                                    string(&mut blob, ALGORITHM);
                                    string(&mut blob, &signature);
                                    response = vec![14]; // SSH2_AGENT_SIGN_RESPONSE
                                    string(&mut response, &blob);
                                    signing.succeeded();
                                } else {
                                    signing.failed(
                                        "runner_sign_invalid_response",
                                        CredentialErrorClass::InvalidResponse,
                                    );
                                }
                            }
                            Err(error) => {
                                let code = match error.class {
                                    CredentialErrorClass::CredentialAbsent => "runner_sign_missing",
                                    CredentialErrorClass::Unauthorized
                                    | CredentialErrorClass::AuthRequired => {
                                        "runner_sign_grant_refused"
                                    }
                                    _ => "runner_sign_unavailable",
                                };
                                signing.failed(code, error.class);
                            }
                        }
                    }
                }
            }
            _ => {}
        }
        // Do not format requests, vault responses, key bytes or signatures into
        // diagnostics. SSH receives only its protocol response; health gets a code.
        let sent = tokio::time::timeout(Duration::from_secs(5), async {
            stream.write_u32(response.len() as u32).await?;
            stream.write_all(&response).await
        })
        .await;
        if !matches!(sent, Ok(Ok(()))) {
            break;
        }
    }
}

#[cfg(test)]
mod retry_hint_tests {
    use super::*;

    // The hint is the time left in the failure's own backoff, read from the
    // sign clock, so it shrinks as the clock moves and never drops below 1 ms.
    #[test]
    fn transient_sign_failures_hint_the_time_left_in_their_backoff() {
        for (code, class) in [
            ("runner_sign_unavailable", CredentialErrorClass::Unavailable),
            (
                "runner_sign_invalid_response",
                CredentialErrorClass::InvalidResponse,
            ),
        ] {
            let state = SignState::new();
            state.pause();
            assert_eq!(state.retry_after_ms(code), None, "no failure yet");
            state.failed(code, class);
            assert_eq!(state.retry_after_ms(code), Some(2000), "{code}");
            state.advance(Duration::from_millis(750));
            assert_eq!(state.retry_after_ms(code), Some(1250), "{code}");
            // A partial millisecond rounds up, so the hint never ends early.
            state.advance(Duration::from_micros(1_249_500));
            assert_eq!(state.retry_after_ms(code), Some(1), "{code}");
            // Past the backoff the failure still holds until a sign succeeds,
            // so the hint stays at its 1 ms floor rather than disappearing.
            state.advance(TRANSIENT_SIGN_BACKOFF);
            assert_eq!(state.retry_after_ms(code), Some(1), "{code}");
            // A hint is only for the failure that is current.
            let other = if code == "runner_sign_unavailable" {
                "runner_sign_invalid_response"
            } else {
                "runner_sign_unavailable"
            };
            assert_eq!(state.retry_after_ms(other), None, "{code}");
            state.succeeded();
            assert_eq!(state.retry_after_ms(code), None, "{code}");
        }
    }

    // A missing credential or a refused grant waits for an operator. A hint
    // would tell a caller to retry in a loop, so neither carries one, even
    // inside its backoff.
    #[test]
    fn repairable_sign_failures_carry_no_hint() {
        for (code, class) in [
            (
                "runner_sign_missing",
                CredentialErrorClass::CredentialAbsent,
            ),
            (
                "runner_sign_grant_refused",
                CredentialErrorClass::Unauthorized,
            ),
        ] {
            let state = SignState::new();
            state.pause();
            state.failed(code, class);
            assert_eq!(state.refusal(), Some(code));
            assert_eq!(state.retry_after_ms(code), None, "{code}");
        }
    }
}
