//! Account bearers are requested per broker call. Only the vault owns their cache.
use super::Error;
use serde::Deserialize;
use serde_json::{json, Value};
use std::{future::Future, io, path::PathBuf, pin::Pin, sync::Arc, time::Duration};
use subc_client_rs::{CallOptions, ConsumerIdentity, ConsumerOptions, SubcConsumer};
use subc_protocol::{BindIdentity, RouteTarget};

pub(super) const BROKER_TIMEOUT: Duration = Duration::from_secs(15);
// Cover the HTTP deadline with room for dispatch between the vault and broker.
const MIN_TTL_MS: u64 = 30_000;
const AUDIENCE: &str = "cortexkit-runners";

type Reply<'a> = Pin<Box<dyn Future<Output = io::Result<Value>> + Send + 'a>>;
pub(crate) trait Target: Send + Sync {
    fn request(&self, request: Value) -> Reply<'_>;
}

/// The account-token transport, separate from SSH credentials and saved routes.
/// It holds no bearer; every broker request consults the vault again.
pub struct AccountTokens {
    target: Arc<dyn Target>,
}
impl AccountTokens {
    pub fn new(connection: PathBuf, workspace: PathBuf, module_id: String) -> Self {
        Self {
            target: Arc::new(Vault {
                connection,
                workspace,
                module_id,
            }),
        }
    }
    #[cfg(test)]
    pub(crate) fn with_target(target: Arc<dyn Target>) -> Self {
        Self { target }
    }
    pub(crate) async fn bearer(&self) -> Result<String, Error> {
        let reply = self
            .target
            .request(json!({
                "method": "credential.account_token",
                "params": {"audience": AUDIENCE, "min_ttl_ms": MIN_TTL_MS}
            }))
            .await
            .map_err(|error| match error.kind() {
                io::ErrorKind::InvalidData | io::ErrorKind::PermissionDenied => {
                    Error::AccountInvalid
                }
                _ => Error::AccountTransient(None),
            })?;
        decode(reply)
    }
}

struct Vault {
    connection: PathBuf,
    workspace: PathBuf,
    module_id: String,
}
impl Target for Vault {
    fn request(&self, request: Value) -> Reply<'_> {
        Box::pin(async move {
            let nonce = subc_os::launch_nonce()
                .map_err(|_| {
                    io::Error::new(
                        io::ErrorKind::PermissionDenied,
                        "account route identity unavailable",
                    )
                })?
                .filter(|nonce| !nonce.value().is_empty())
                .ok_or_else(|| {
                    io::Error::new(
                        io::ErrorKind::PermissionDenied,
                        "account route identity unavailable",
                    )
                })?;
            self.request_with_nonce(request, nonce.value()).await
        })
    }
}
impl Vault {
    async fn request_with_nonce(&self, request: Value, nonce: &str) -> io::Result<Value> {
        let consumer = SubcConsumer::connect(
            &self.connection,
            ConsumerOptions {
                handshake_timeout: Duration::from_secs(2),
                ..ConsumerOptions::default()
            },
        )
        .await
        .map_err(|_| io::Error::other("account route unavailable"))?;
        let reply = consumer
            .call(
                RouteTarget::ManagementSurface {
                    module_id: "claustrum".into(),
                },
                BindIdentity::new(&self.workspace, &self.module_id, "motor-account-token"),
                serde_json::to_vec(&request).expect("serializable account request"),
                CallOptions {
                    timeout: Duration::from_secs(10),
                    consumer_identity: Some(ConsumerIdentity {
                        module_id: self.module_id.clone(),
                        launch_nonce: nonce.to_owned(),
                    }),
                    ..CallOptions::default()
                },
            )
            .await;
        consumer.close().await;
        // Transport errors can contain remote text: never copy it to a job or log.
        let bytes = reply.map_err(|_| io::Error::other("account request unavailable"))?;
        serde_json::from_slice(&bytes)
            .map_err(|_| io::Error::new(io::ErrorKind::InvalidData, "account reply unreadable"))
    }
}

#[derive(Deserialize)]
struct Token {
    jwt: String,
    expires_at_ms: u64,
    audience: String,
    account: String,
}
fn decode(reply: Value) -> Result<String, Error> {
    let result = reply.get("result").ok_or(Error::AccountInvalid)?;
    if let Some(error) = result.get("error") {
        if !error.get("code").is_some_and(Value::is_string) {
            return Err(Error::AccountInvalid);
        }
        return Err(match error.get("class").and_then(Value::as_str) {
            Some("not_found") => Error::AccountNotFound,
            Some("auth_required") => Error::AccountAuthRequired,
            Some("permanent") => Error::AccountPermanent,
            Some("transient") => Error::AccountTransient(match error.get("retry_after_ms") {
                None | Some(Value::Null) => None,
                Some(delay) => Some(delay.as_u64().ok_or(Error::AccountInvalid)?),
            }),
            _ => Error::AccountInvalid,
        });
    }
    let token: Token = serde_json::from_value(result.clone()).map_err(|_| Error::AccountInvalid)?;
    let now_ms = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis();
    if token.audience != AUDIENCE
        || token.account.is_empty()
        || token.jwt.is_empty()
        || token
            .jwt
            .bytes()
            .any(|byte| byte.is_ascii_control() || byte == b' ')
        || u128::from(token.expires_at_ms) < now_ms + u128::from(MIN_TTL_MS)
    {
        return Err(Error::AccountInvalid);
    }
    Ok(token.jwt)
}

#[cfg(test)]
pub(crate) mod tests;
#[cfg(test)]
mod wire_tests;
