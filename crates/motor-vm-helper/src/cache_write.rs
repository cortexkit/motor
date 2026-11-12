//! Root's independent admission decision. Tokens and keys never enter boot requests.
use crate::{config::Config, job::validate_start_window, Error, Result};
use motor_lease::{verify_token, AuthError, Jwks, KeySet, Purpose, JWKS_URL};
use std::{
    collections::{BTreeMap, BTreeSet},
    io::Read,
    sync::{Arc, Condvar, Mutex},
    time::{Duration, SystemTime, UNIX_EPOCH},
};

// Bound trust in a revoked signing key when the broker cannot be refreshed.
const MAX_JWKS_AGE_S: u64 = 60 * 60;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Grant {
    pub sub: String,
    pub jti: String,
    pub workspace_key_hash: String,
    pub exp: u64,
}

impl Grant {
    pub(crate) fn valid_at(&self, now: SystemTime) -> bool {
        seconds(now).is_ok_and(|now| now < self.exp)
    }

    #[cfg(any(target_os = "linux", test))]
    pub(crate) fn permits(&self, start: &crate::job::Start, disk: &crate::job::Workspace) -> bool {
        start.kind == crate::job::VmKind::Job
            && !start.cache_write_blocked
            && disk.role == crate::job::WorkspaceRole::Job
            && self.jti == start.job_id
            && self.sub == disk.account
            && start.account == disk.account
            && start.workspace_disk_id == disk.workspace_disk_id
            && disk.workspace_key_hash.as_ref() == Some(&self.workspace_key_hash)
    }
}

/// Network refresh and grant lookup use separate state. Lifecycle callers only
/// try the grant lock, so a slow broker cannot delay VM stop or memory controls.
pub type Authority = Arc<Grants>;

#[cfg(target_os = "linux")]
pub(crate) fn disk_grant(authority: &Authority, id: Option<&str>) -> Option<Grant> {
    authority.get(id?)
}

/// Memory belongs to the root helper, not the host ledger. Restart loses grants
/// and therefore falls back to read-only rather than trusting restored host state.
pub struct Grants {
    host_id: Option<String>,
    jwks_url: String,
    keys: Mutex<Keys>,
    refreshed: Condvar,
    records: Mutex<Records>,
}

#[derive(Default)]
struct Keys {
    keys: KeySet,
    keys_ready: bool,
    fetching: bool,
    retry_at: Option<u64>,
    retry_delay_s: u64,
    last_success_s: Option<u64>,
}

impl Keys {
    fn usable_at(&self, now_s: u64) -> bool {
        self.keys_ready
            && self
                .last_success_s
                .is_some_and(|last| now_s.saturating_sub(last) <= MAX_JWKS_AGE_S)
    }
}

#[derive(Default)]
struct Records {
    grants: BTreeMap<String, Grant>,
    consumed: BTreeSet<String>,
}

impl Grants {
    pub fn new(config: &Config) -> Self {
        if config.host_id.is_none() {
            eprintln!("helper cache writes disabled: host_id is not configured");
        }
        Self {
            host_id: config.host_id.clone(),
            jwks_url: config.jwks_url.clone().unwrap_or_else(|| JWKS_URL.into()),
            keys: Mutex::default(),
            refreshed: Condvar::new(),
            records: Mutex::default(),
        }
    }

    pub fn admit(&self, token: &str) -> Result<()> {
        self.admit_with_clock(token, SystemTime::now)
    }

    fn admit_with_clock(&self, token: &str, clock: impl Fn() -> SystemTime) -> Result<()> {
        self.prune_at(clock());
        let host = self
            .host_id
            .clone()
            .ok_or_else(|| Error("helper host_id is not configured".into()))?;
        self.refresh(token, clock())?;
        // Fetching can take long enough for a short-lived token to expire.
        // Check against root's clock after the fetch, not the caller's timestamp.
        let now = clock();
        let keys = self
            .keys
            .lock()
            .map_err(|_| Error("cache key lock poisoned".into()))?;
        if !keys.usable_at(seconds(now)?) {
            return Err(Error("helper JWKS unavailable or stale".into()));
        }
        let claims = verify_token(token, &keys.keys, &host, None, Purpose::Run, seconds(now)?)
            .map_err(|_| Error("cache-write token verification refused".into()))?;
        drop(keys);
        if claims.cache_write != Some(true) {
            return Err(Error("cache-write token does not grant writes".into()));
        }
        validate_start_window(&claims.jti, now)?;
        let grant = Grant {
            exp: claims.exp,
            sub: claims.sub,
            jti: claims.jti,
            workspace_key_hash: claims
                .workspace_key_hash
                .ok_or_else(|| Error("cache-write token has no workspace hash".into()))?,
        };
        let mut records = self
            .records
            .lock()
            .map_err(|_| Error("cache grant lock poisoned".into()))?;
        if let Some(existing) = records.grants.get(&grant.jti) {
            if existing != &grant {
                return Err(Error(
                    "cache-write grant already recorded with another binding".into(),
                ));
            }
            return Ok(());
        }
        records.grants.insert(grant.jti.clone(), grant);
        Ok(())
    }

    /// This only exposes an unexpired grant; the boot path must still bind disk
    /// and account, consume it once, and recheck expiry when staging credentials.
    pub fn get(&self, jti: &str) -> Option<Grant> {
        self.get_at(jti, SystemTime::now())
    }

    fn get_at(&self, jti: &str, now: SystemTime) -> Option<Grant> {
        validate_start_window(jti, now).ok()?;
        let records = self.records.try_lock().ok()?;
        if records.consumed.contains(jti) {
            return None;
        }
        records
            .grants
            .get(jti)
            .filter(|grant| grant.valid_at(now))
            .cloned()
    }

    /// Call only after the durable start intent excludes another VM for this id.
    pub fn consume(&self, jti: &str) -> Option<Grant> {
        self.consume_at(jti, SystemTime::now())
    }

    fn consume_at(&self, jti: &str, now: SystemTime) -> Option<Grant> {
        let mut records = self.records.try_lock().ok()?;
        if records.consumed.contains(jti) {
            return None;
        }
        let grant = records.grants.get(jti)?.clone();
        records.consumed.insert(jti.into());
        (validate_start_window(jti, now).is_ok() && grant.valid_at(now)).then_some(grant)
    }

    pub fn maintain(&self) {
        self.prune_at(SystemTime::now());
        if self.host_id.is_some() {
            let _ = self.refresh("", SystemTime::now());
        }
    }

    fn prune_at(&self, now: SystemTime) {
        let Ok(mut records) = self.records.try_lock() else {
            return;
        };
        let Records { grants, consumed } = &mut *records;
        grants.retain(|jti, _| validate_start_window(jti, now).is_ok());
        consumed.retain(|jti| grants.contains_key(jti));
    }

    fn refresh(&self, token: &str, now: SystemTime) -> Result<()> {
        let mut keys = self
            .keys
            .lock()
            .map_err(|_| Error("cache key lock poisoned".into()))?;
        while keys.fetching && !keys.keys_ready {
            keys = self
                .refreshed
                .wait(keys)
                .map_err(|_| Error("cache key lock poisoned".into()))?;
        }
        let now_s = seconds(now)?;
        let due = keys.retry_at.map_or_else(
            || keys.keys.should_refetch(token, now_s),
            |retry| now_s >= retry,
        );
        if !keys.fetching && due {
            keys.fetching = true;
            drop(keys);
            let fetched = (|| -> Result<Jwks> {
                let response = reqwest::blocking::Client::builder()
                    .connect_timeout(Duration::from_secs(5))
                    .timeout(Duration::from_secs(10))
                    .redirect(reqwest::redirect::Policy::none())
                    .no_proxy()
                    .build()
                    .map_err(|_| Error("helper JWKS client unavailable".into()))?
                    .get(&self.jwks_url)
                    .send()
                    .and_then(reqwest::blocking::Response::error_for_status)
                    .map_err(|_| Error("helper JWKS fetch failed".into()))?;
                let mut bytes = Vec::new();
                response.take(65537).read_to_end(&mut bytes)?;
                if bytes.len() > 65536 {
                    return Err(Error("helper JWKS exceeds size limit".into()));
                }
                serde_json::from_slice(&bytes).map_err(|_| Error("helper JWKS is invalid".into()))
            })();
            keys = self
                .keys
                .lock()
                .map_err(|_| Error("cache key lock poisoned".into()))?;
            let mut replacement = KeySet::default();
            if replacement
                .refresh(fetched.map_err(|_| AuthError::Auth))
                .is_ok()
            {
                replacement.should_refetch("", now_s);
                keys.keys = replacement;
                keys.keys_ready = true;
                keys.last_success_s = Some(now_s);
                keys.retry_at = None;
                keys.retry_delay_s = 0;
            } else {
                // A failed refresh cannot revoke previously verified keys.
                // Token expiry is still checked for every admission.
                keys.retry_delay_s = (keys.retry_delay_s * 2).clamp(5, 60);
                keys.retry_at = Some(now_s.saturating_add(keys.retry_delay_s));
            }
            keys.fetching = false;
            self.refreshed.notify_all();
        }
        if !keys.usable_at(now_s) {
            return Err(Error("helper JWKS unavailable".into()));
        }
        Ok(())
    }
}

fn seconds(now: SystemTime) -> Result<u64> {
    now.duration_since(UNIX_EPOCH)
        .map(|time| time.as_secs())
        .map_err(|_| Error("helper clock precedes Unix epoch".into()))
}

#[cfg(test)]
#[path = "cache_write_tests.rs"]
pub(crate) mod tests;
