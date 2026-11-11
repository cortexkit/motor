//! Root's independent admission decision. Tokens and keys never enter boot requests.
use crate::{config::Config, job::validate_start_window, Error, Result};
use motor_lease::{verify_token, AuthError, Jwks, KeySet, Purpose, JWKS_URL};
use std::{
    collections::{BTreeMap, BTreeSet},
    io::Read,
    sync::{Arc, Mutex},
    time::{Duration, SystemTime, UNIX_EPOCH},
};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Grant {
    pub sub: String,
    pub jti: String,
    pub workspace_key_hash: String,
}

impl Grant {
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

/// Shared only inside the helper. Network work holds this lock, never the
/// global lifecycle lock, so a slow broker cannot delay VM stop or memory controls.
pub type Authority = Arc<Mutex<Grants>>;

/// Memory belongs to the root helper, not the host ledger. Restart loses grants
/// and therefore falls back to read-only rather than trusting restored host state.
pub struct Grants {
    host_id: Option<String>,
    jwks_url: String,
    keys: KeySet,
    keys_ready: bool,
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
            keys: KeySet::default(),
            keys_ready: false,
            grants: BTreeMap::new(),
            consumed: BTreeSet::new(),
        }
    }

    pub fn admit(&mut self, token: &str) -> Result<()> {
        self.admit_with_clock(token, SystemTime::now)
    }

    fn admit_with_clock(&mut self, token: &str, clock: impl Fn() -> SystemTime) -> Result<()> {
        self.prune_at(clock());
        let host = self
            .host_id
            .clone()
            .ok_or_else(|| Error("helper host_id is not configured".into()))?;
        self.refresh(token, clock())?;
        // Fetching can take long enough for a short-lived token to expire.
        // Check against root's clock after the fetch, not the caller's timestamp.
        let now = clock();
        let claims = verify_token(token, &self.keys, &host, None, Purpose::Run, seconds(now)?)
            .map_err(|_| Error("cache-write token verification refused".into()))?;
        if claims.cache_write != Some(true) {
            return Err(Error("cache-write token does not grant writes".into()));
        }
        validate_start_window(&claims.jti, now)?;
        let grant = Grant {
            sub: claims.sub,
            jti: claims.jti,
            workspace_key_hash: claims
                .workspace_key_hash
                .ok_or_else(|| Error("cache-write token has no workspace hash".into()))?,
        };
        if let Some(existing) = self.grants.get(&grant.jti) {
            if existing != &grant {
                return Err(Error(
                    "cache-write grant already recorded with another binding".into(),
                ));
            }
            return Ok(());
        }
        self.grants.insert(grant.jti.clone(), grant);
        Ok(())
    }

    /// A queued boot uses admission's decision, not the token's later expiry.
    /// This only exposes a grant; the boot path must still bind disk and account.
    pub fn get(&self, jti: &str) -> Option<&Grant> {
        self.get_at(jti, SystemTime::now())
    }

    fn get_at(&self, jti: &str, now: SystemTime) -> Option<&Grant> {
        validate_start_window(jti, now).ok()?;
        if self.consumed.contains(jti) {
            return None;
        }
        self.grants.get(jti)
    }

    /// Call only after the durable start intent excludes another VM for this id.
    pub fn consume(&mut self, jti: &str) {
        if self.grants.contains_key(jti) {
            self.consumed.insert(jti.into());
        }
    }

    pub fn maintain(&mut self) {
        self.prune_at(SystemTime::now());
        if self.host_id.is_some() {
            let _ = self.refresh("", SystemTime::now());
        }
    }

    fn prune_at(&mut self, now: SystemTime) {
        self.grants
            .retain(|jti, _| validate_start_window(jti, now).is_ok());
        self.consumed.retain(|jti| self.grants.contains_key(jti));
    }

    fn refresh(&mut self, token: &str, now: SystemTime) -> Result<()> {
        if self.keys.should_refetch(token, seconds(now)?) {
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
            self.keys_ready = self
                .keys
                .refresh(fetched.map_err(|_| AuthError::Auth))
                .is_ok();
        }
        if !self.keys_ready {
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
