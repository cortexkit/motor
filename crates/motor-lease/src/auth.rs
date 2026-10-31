use std::collections::{BTreeMap, BTreeSet};
use std::fmt;

use base64::{engine::general_purpose::URL_SAFE_NO_PAD, Engine};
use ed25519_dalek::{Signature, Signer, SigningKey, VerifyingKey};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use crate::{
    valid_job_id, valid_name, Challenge, Claims, Purpose, RequesterJwk, CLOCK_SKEW_S, ISSUER,
    MAX_SAFE_INTEGER, MAX_TOKEN_LIFETIME_S,
};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum AuthError {
    Auth,
    Expired,
    MissingWorkspaceKeyHash,
    CounterOverflow,
}
impl fmt::Display for AuthError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::Auth => "auth",
            Self::Expired => "expired",
            Self::MissingWorkspaceKeyHash => "missing workspace_key_hash",
            Self::CounterOverflow => "host counter overflow",
        })
    }
}
impl std::error::Error for AuthError {}

pub fn base64url(bytes: &[u8]) -> String {
    URL_SAFE_NO_PAD.encode(bytes)
}

pub fn sha256_hex(bytes: &[u8]) -> String {
    Sha256::digest(bytes)
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect()
}

fn lowercase_hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}
fn decode_hex<const N: usize>(text: &str) -> Result<[u8; N], AuthError> {
    if text.len() != N * 2
        || !text
            .bytes()
            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
    {
        return Err(AuthError::Auth);
    }
    let mut result = [0; N];
    for (i, value) in result.iter_mut().enumerate() {
        *value = u8::from_str_radix(&text[i * 2..i * 2 + 2], 16).map_err(|_| AuthError::Auth)?;
    }
    Ok(result)
}

fn decode<const N: usize>(text: &str) -> Result<[u8; N], AuthError> {
    URL_SAFE_NO_PAD
        .decode(text)
        .map_err(|_| AuthError::Auth)?
        .try_into()
        .map_err(|_| AuthError::Auth)
}

impl RequesterJwk {
    pub fn from_key(key: &VerifyingKey) -> Self {
        Self {
            kty: "OKP".into(),
            crv: "Ed25519".into(),
            x: base64url(key.as_bytes()),
        }
    }

    pub fn verifying_key(&self) -> Result<VerifyingKey, AuthError> {
        if self.kty != "OKP" || self.crv != "Ed25519" {
            return Err(AuthError::Auth);
        }
        let key = VerifyingKey::from_bytes(&decode(&self.x)?).map_err(|_| AuthError::Auth)?;
        if key.is_weak() {
            return Err(AuthError::Auth);
        }
        Ok(key)
    }

    pub fn thumbprint(&self) -> Result<String, AuthError> {
        self.verifying_key()?;
        // RFC 7638 hashes only these members, in lexicographic order.
        let canonical = format!(
            "{{\"crv\":\"Ed25519\",\"kty\":\"OKP\",\"x\":\"{}\"}}",
            self.x
        );
        Ok(base64url(&Sha256::digest(canonical.as_bytes())))
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct JwksKey {
    #[serde(default)]
    pub kid: String,
    pub kty: String,
    #[serde(default)]
    pub crv: String,
    #[serde(default)]
    pub x: String,
    #[serde(default)]
    pub alg: Option<String>,
    #[serde(default, rename = "use")]
    pub key_use: Option<String>,
}
impl JwksKey {
    pub fn from_key(kid: impl Into<String>, key: &VerifyingKey) -> Self {
        let jwk = RequesterJwk::from_key(key);
        Self {
            kid: kid.into(),
            kty: jwk.kty,
            crv: jwk.crv,
            x: jwk.x,
            alg: Some("EdDSA".into()),
            key_use: Some("sig".into()),
        }
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Jwks {
    pub keys: Vec<JwksKey>,
}

/// A supplied JWKS cache. Refreshes retain usable, unambiguous Ed25519 keys;
/// unsupported or invalid entries are skipped. An empty result keeps the old set.
#[derive(Default)]
pub struct KeySet {
    keys: BTreeMap<String, VerifyingKey>,
    last_fetch_s: Option<u64>,
}
impl KeySet {
    /// True until a refresh has stored at least one usable key.
    pub fn is_empty(&self) -> bool {
        self.keys.is_empty()
    }
    /// Call before fetching JWKS. Record attempts, including failed fetches, so an
    /// unknown signing key cannot trigger more than one network request a minute.
    pub fn should_refetch(&mut self, token: &str, now_s: u64) -> bool {
        let kid = token
            .split('.')
            .next()
            .and_then(|part| URL_SAFE_NO_PAD.decode(part).ok())
            .and_then(|bytes| serde_json::from_slice::<Header>(&bytes).ok())
            .map(|header| header.kid);
        let elapsed = self.last_fetch_s.map(|last| now_s.saturating_sub(last));
        let fetch = elapsed.is_none_or(|elapsed| elapsed >= 600)
            || (elapsed.is_some_and(|elapsed| elapsed >= 60)
                && kid.is_some_and(|kid| !self.keys.contains_key(&kid)));
        if fetch {
            self.last_fetch_s = Some(now_s);
        }
        fetch
    }
    pub fn refresh(&mut self, fetched: Result<Jwks, AuthError>) -> Result<(), AuthError> {
        let jwks = fetched?;
        let mut keys = BTreeMap::new();
        let mut conflicts = BTreeSet::new();
        for key in jwks.keys {
            if key.kty != "OKP"
                || key.crv != "Ed25519"
                || key.kid.is_empty()
                || key.alg.as_deref().is_some_and(|alg| alg != "EdDSA")
                || key.key_use.as_deref().is_some_and(|usage| usage != "sig")
            {
                continue;
            }
            let Ok(public) = (RequesterJwk {
                kty: key.kty,
                crv: key.crv,
                x: key.x,
            })
            .verifying_key() else {
                continue;
            };
            if keys
                .insert(key.kid.clone(), public)
                .is_some_and(|previous| previous != public)
            {
                conflicts.insert(key.kid);
            }
        }
        for kid in conflicts {
            keys.remove(&kid);
        }
        if keys.is_empty() {
            return Err(AuthError::Auth);
        }
        self.keys = keys;
        Ok(())
    }
}

#[derive(Serialize, Deserialize)]
struct Header {
    alg: String,
    kid: String,
    typ: String,
    #[serde(flatten)]
    extra: BTreeMap<String, serde_json::Value>,
}

pub fn sign_token(claims: &Claims, kid: &str, key: &SigningKey) -> Result<String, AuthError> {
    let header = Header {
        alg: "EdDSA".into(),
        kid: kid.into(),
        typ: "ck-job+jwt".into(),
        extra: BTreeMap::new(),
    };
    let header = serde_json::to_vec(&header).map_err(|_| AuthError::Auth)?;
    let claims = serde_json::to_vec(claims).map_err(|_| AuthError::Auth)?;
    let signing_input = format!("{}.{}", base64url(&header), base64url(&claims));
    Ok(format!(
        "{signing_input}.{}",
        base64url(&key.sign(signing_input.as_bytes()).to_bytes())
    ))
}

/// Verify at connect time for the required credential purpose. `expected_job`
/// additionally binds a token when the caller already knows the job id.
pub fn verify_token(
    token: &str,
    keys: &KeySet,
    host_id: &str,
    expected_job: Option<&str>,
    required_purpose: Purpose,
    now_s: u64,
) -> Result<Claims, AuthError> {
    check_token(token, keys, host_id, expected_job, required_purpose, now_s)
        .map_err(|refusal| refusal.error)
}

/// Why [`check_token`] refused a job token. It never holds the token or a key,
/// so it is safe to log.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TokenRefusal {
    /// What the host reports to the requester.
    pub error: AuthError,
    /// The first check that failed, such as `kid`, `signature` or `mem_mib`.
    pub check: &'static str,
    /// The token's job id, present only once the broker's signature over it
    /// has verified, so a forged token cannot choose what is logged.
    pub jti: Option<String>,
}

/// Runs the same checks as [`verify_token`], but a refusal also names the
/// first check that failed so the host can log why it refused.
pub fn check_token(
    token: &str,
    keys: &KeySet,
    host_id: &str,
    expected_job: Option<&str>,
    required_purpose: Purpose,
    now_s: u64,
) -> Result<Claims, TokenRefusal> {
    let refuse = |check| TokenRefusal {
        error: AuthError::Auth,
        check,
        jti: None,
    };
    let parts: Vec<_> = token.split('.').collect();
    if parts.len() != 3 {
        return Err(refuse("format"));
    }
    let header: Header = URL_SAFE_NO_PAD
        .decode(parts[0])
        .ok()
        .and_then(|bytes| serde_json::from_slice(&bytes).ok())
        .ok_or(refuse("header"))?;
    if header.alg != "EdDSA" || header.typ != "ck-job+jwt" || !header.extra.is_empty() {
        return Err(refuse("header"));
    }
    let key = keys.keys.get(&header.kid).ok_or(refuse("kid"))?;
    let signature = Signature::from_bytes(&decode(parts[2]).map_err(|_| refuse("signature"))?);
    key.verify_strict(format!("{}.{}", parts[0], parts[1]).as_bytes(), &signature)
        .map_err(|_| refuse("signature"))?;
    let claims: Claims = URL_SAFE_NO_PAD
        .decode(parts[1])
        .ok()
        .and_then(|bytes| serde_json::from_slice(&bytes).ok())
        .ok_or(refuse("claims"))?;
    // A Boat sandbox's identity is `boat:<sandbox id>`. The colon cannot occur
    // in a registered host id, so only a sandbox endpoint can take this branch.
    let boat = host_id.starts_with("boat:");
    let memory_ok = if boat {
        crate::boat_memory_range(claims.vcpus).contains(&claims.mem_mib)
    } else {
        crate::first_party_mem_mib_accepted(claims.vcpus, claims.mem_mib)
    };
    // Checks run in this order, and the first failure is the one reported.
    let failed = [
        ("iss", claims.iss != ISSUER),
        ("aud", claims.aud != host_id),
        ("placement_host", claims.placement.host != host_id),
        (
            "placement_vm",
            claims
                .placement
                .vm
                .as_deref()
                .is_some_and(|vm| !valid_name(vm)),
        ),
        ("jti", !valid_job_id(&claims.jti)),
        ("aud", !valid_audience(&claims.aud)),
        ("jti", expected_job.is_some_and(|job| job != claims.jti)),
        ("purpose", claims.purpose != required_purpose),
        ("sub", !valid_subject(&claims.sub)),
        ("iat", claims.iat > now_s.saturating_add(CLOCK_SKEW_S)),
        (
            "lifetime",
            claims.exp <= claims.iat
                || claims.exp.saturating_sub(claims.iat) > MAX_TOKEN_LIFETIME_S,
        ),
        ("vcpus", claims.vcpus == 0 || claims.vcpus > 64),
        (
            "lifetime",
            claims.iat > MAX_SAFE_INTEGER || claims.exp > MAX_SAFE_INTEGER,
        ),
        ("mem_mib", !memory_ok),
        (
            "max_s",
            claims.max_s == 0 || claims.max_s > if boat { 7200 } else { 21600 },
        ),
        ("platform", !valid_name(&claims.platform)),
        ("cnf", decode::<32>(&claims.cnf.jkt).is_err()),
        (
            "workspace_key_hash_missing",
            !boat && claims.workspace_key_hash.is_none(),
        ),
        (
            "workspace_key_hash",
            claims.workspace_key_hash.as_deref().is_some_and(|hash| {
                hash.len() != 43
                    || !hash
                        .bytes()
                        .all(|b| b.is_ascii_alphanumeric() || b == b'_' || b == b'-')
            }),
        ),
    ]
    .into_iter()
    .find_map(|(check, failed)| failed.then_some(check));
    let error = if failed == Some("workspace_key_hash_missing") {
        Some((
            AuthError::MissingWorkspaceKeyHash,
            "workspace_key_hash_missing",
        ))
    } else if let Some(check) = failed {
        Some((AuthError::Auth, check))
    } else if now_s >= claims.exp {
        // An already expired credential cannot open a new connection, even
        // during skew.
        Some((AuthError::Expired, "exp"))
    } else {
        None
    };
    match error {
        Some((error, check)) => Err(TokenRefusal {
            error,
            check,
            jti: Some(claims.jti).filter(|jti| valid_job_id(jti)),
        }),
        None => Ok(claims),
    }
}

fn valid_audience(value: &str) -> bool {
    // Provider-scoped Boat identities are not host registry names. Only this
    // namespace is allowed; an arbitrary URI must not become a valid audience.
    valid_name(value.strip_prefix("boat:").unwrap_or(value))
}

pub(crate) fn valid_subject(subject: &str) -> bool {
    let Some(id) = subject.strip_prefix("acct:") else {
        return false;
    };
    id.len() == 26
        && id.as_bytes()[0] <= b'7'
        && id
            .bytes()
            .all(|c| b"0123456789ABCDEFGHJKMNPQRSTVWXYZ".contains(&c))
}

pub fn bind_requester(claims: &Claims, jwk: &RequesterJwk) -> Result<VerifyingKey, AuthError> {
    if jwk.thumbprint()? != claims.cnf.jkt {
        return Err(AuthError::Auth);
    }
    jwk.verifying_key()
}

pub fn challenge_bytes(challenge: &Challenge, claims: &Claims) -> Result<Vec<u8>, AuthError> {
    if challenge.host_id != claims.placement.host
        || challenge.nonce.len() != 64
        || !challenge
            .nonce
            .bytes()
            .all(|c| c.is_ascii_digit() || (b'a'..=b'f').contains(&c))
    {
        return Err(AuthError::Auth);
    }
    Ok(format!(
        "motor-host-challenge-v1\n{}\n{}\n{}\n{}\n",
        challenge.nonce, claims.placement.host, claims.jti, challenge.ts
    )
    .into_bytes())
}

pub fn sign_challenge(
    challenge: &Challenge,
    claims: &Claims,
    key: &SigningKey,
    now_s: u64,
) -> Result<String, AuthError> {
    if now_s.abs_diff(challenge.ts) > CLOCK_SKEW_S {
        return Err(AuthError::Auth);
    }
    Ok(base64url(
        &key.sign(&challenge_bytes(challenge, claims)?).to_bytes(),
    ))
}

/// The host retains this replay set across connections, not one set per socket.
#[derive(Default)]
pub struct Nonces {
    used: BTreeMap<String, u64>,
}
impl Nonces {
    /// Keep twice the accepted clock-skew window so pruning cannot reopen a
    /// replay while its challenge timestamp could still pass verification.
    pub fn prune(&mut self, now_s: u64) -> usize {
        let before = self.used.len();
        self.used
            .retain(|_, ts| now_s.saturating_sub(*ts) <= 2 * CLOCK_SKEW_S);
        before - self.used.len()
    }

    pub fn verify(
        &mut self,
        challenge: &Challenge,
        claims: &Claims,
        requester: &RequesterJwk,
        signature: &str,
        now_s: u64,
    ) -> Result<(), AuthError> {
        self.prune(now_s);
        if now_s.abs_diff(challenge.ts) > CLOCK_SKEW_S || self.used.contains_key(&challenge.nonce) {
            return Err(AuthError::Auth);
        }
        bind_requester(claims, requester)?
            .verify_strict(
                &challenge_bytes(challenge, claims)?,
                &Signature::from_bytes(&decode(signature)?),
            )
            .map_err(|_| AuthError::Auth)?;
        self.used.insert(challenge.nonce.clone(), challenge.ts);
        Ok(())
    }
}

pub fn seed_counter(persisted: u64, unix_ms: u64) -> Result<u64, AuthError> {
    Ok(persisted
        .checked_add(1)
        .ok_or(AuthError::CounterOverflow)?
        .max(
            unix_ms
                .checked_mul(1000)
                .ok_or(AuthError::CounterOverflow)?,
        ))
}

pub fn host_message_bytes(
    method: &str,
    path: &str,
    body: &[u8],
    timestamp: u64,
    counter: u64,
) -> Vec<u8> {
    format!(
        "ck-host-v1\n{method}\n{path}\n{}\n{timestamp}\n{counter}",
        sha256_hex(body)
    )
    .into_bytes()
}

#[derive(Clone, Serialize, Deserialize)]
pub struct HostHeaders {
    pub host: String,
    pub timestamp: u64,
    pub counter: u64,
    pub signature: String,
}
impl HostHeaders {
    /// The caller must durably reserve `counter` before calling this function.
    pub fn sign(
        host: &str,
        method: &str,
        path: &str,
        body: &[u8],
        timestamp: u64,
        counter: u64,
        key: &SigningKey,
    ) -> Result<Self, AuthError> {
        if counter == 0
            || counter > MAX_SAFE_INTEGER
            || timestamp == 0
            || timestamp > MAX_SAFE_INTEGER
        {
            return Err(AuthError::Auth);
        }
        Ok(Self {
            host: host.into(),
            timestamp,
            counter,
            signature: lowercase_hex(
                &key.sign(&host_message_bytes(method, path, body, timestamp, counter))
                    .to_bytes(),
            ),
        })
    }
}

#[derive(Default)]
pub struct HostAuthenticator {
    hosts: BTreeMap<String, (VerifyingKey, u64)>,
}
impl HostAuthenticator {
    pub fn register(&mut self, host: String, key: VerifyingKey) {
        self.hosts.insert(host, (key, 0));
    }

    pub fn verify(
        &mut self,
        method: &str,
        path: &str,
        body: &[u8],
        headers: &HostHeaders,
        now_ms: u64,
    ) -> Result<(), AuthError> {
        let (key, counter) = self.hosts.get_mut(&headers.host).ok_or(AuthError::Auth)?;
        if headers.counter == 0
            || headers.counter > MAX_SAFE_INTEGER
            || headers.timestamp == 0
            || headers.timestamp > MAX_SAFE_INTEGER
            || headers.counter <= *counter
        {
            return Err(AuthError::Auth);
        }
        key.verify_strict(
            &host_message_bytes(method, path, body, headers.timestamp, headers.counter),
            &Signature::from_bytes(&decode_hex(&headers.signature)?),
        )
        .map_err(|_| AuthError::Auth)?;
        *counter = headers.counter;
        // A valid signature consumes its counter even if the timestamp is stale.
        if headers.timestamp.abs_diff(now_ms) > CLOCK_SKEW_S * 1000 {
            return Err(AuthError::Auth);
        }
        Ok(())
    }
}
