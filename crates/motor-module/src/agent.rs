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
    time::Duration,
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
    failure: Arc<Mutex<Option<&'static str>>>,
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
        let failure = Arc::new(Mutex::new(None));
        let fault = Arc::clone(&failure);
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
                            let fault = Arc::clone(&fault);
                            clients.spawn(serve(stream, key, key_id, resolver, fault));
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
            failure,
            stop: Mutex::new(Some(stop)),
            task: AsyncMutex::new(Some(task)),
        })
    }
    pub fn refusal(&self) -> Option<&'static str> {
        self.failure
            .lock()
            .unwrap()
            .or_else(|| (!self.alive.load(Ordering::Acquire)).then_some("runner_agent_stopped"))
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
    fault: Arc<Mutex<Option<&'static str>>>,
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
                    if fault.lock().unwrap().is_none() {
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
                                } else {
                                    *fault.lock().unwrap() = Some("runner_sign_invalid_response");
                                }
                            }
                            Err(error) => {
                                *fault.lock().unwrap() = Some(match error.class {
                                    CredentialErrorClass::CredentialAbsent => "runner_sign_missing",
                                    CredentialErrorClass::Unauthorized
                                    | CredentialErrorClass::AuthRequired => {
                                        "runner_sign_grant_refused"
                                    }
                                    _ => "runner_sign_unavailable",
                                });
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
