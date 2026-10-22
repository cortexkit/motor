//! Closed customer accounts: the broker lists them, the host stops admitting
//! their jobs, and once their last job has drained it has the VM helper
//! remove the tenant (see motor-vm-helper's `removal` module).
//!
//! The broker's heartbeat reply carries `closed_subjects`, the account
//! subjects (`acct:<ULID>`) of the closed accounts among the tenants this
//! host's heartbeat reported, sorted, and an empty array when none is closed.
//! A broker older than account closure omits the member; that reads as no
//! closed accounts, never as a failed heartbeat. Closure is permanent, so a
//! subject once listed is never reopened. `org:<ULID>` subjects are accepted
//! too, matching the grammar provisioning accepts for a tenant's subject.
//! The host maps a subject to a tenant only through its own binding
//! map, which provisioning writes; a subject naming no tenant here is ignored.
use crate::state::{Error, Host, LeaseState};
use serde::{Deserialize, Serialize};
use serde_json::Value;

/// How long after a closed tenant's last job ended the host waits before
/// removing it: the runner keeps a finished job's record and output this long
/// (motor-runner's `RETENTION_MS`), so a client can still fetch its result.
pub const RUNNER_RETENTION_MS: u64 = 24 * 60 * 60 * 1000;
/// A removal still pending this long after the account closed is logged once
/// as an overdue error, and every failed attempt after it as an error too.
/// The public privacy page promises the tenant disk is deleted within 30 days
/// of closure; the helper deletes quarantined data 7 days after removal, so
/// removal must happen well before day 23.
pub const REMOVAL_OVERDUE_MS: u64 = 7 * 24 * 60 * 60 * 1000;
/// The first retry of a failed removal waits this long; each further failure
/// doubles the wait up to `REMOVAL_RETRY_MAX_MS`.
pub const REMOVAL_RETRY_MS: u64 = 60 * 1000;
pub const REMOVAL_RETRY_MAX_MS: u64 = 6 * 60 * 60 * 1000;

/// A tenant whose account the broker reported closed.
#[derive(Clone, Debug, Default, Deserialize, Serialize, PartialEq, Eq)]
#[serde(default)]
pub struct ClosedTenant {
    /// The account subject the broker reported, as bound to this tenant.
    pub sub: String,
    /// When this host first learned of the closure.
    pub since_ms: u64,
    pub attempts: u32,
    /// No removal is attempted before this time (backoff after a failure).
    pub retry_at_ms: u64,
    /// When the helper removed the tenant; `None` while removal is pending.
    pub removed_ms: Option<u64>,
    /// The helper's quarantine folder for the removed data.
    pub quarantine: Option<String>,
    /// Whether the once-only overdue error has been logged.
    pub overdue_logged: bool,
}

/// The well-formed subjects in a heartbeat reply's `closed_subjects`. Any
/// other shape (a missing or non-array field, non-string or malformed entries,
/// a reply that is not an object) yields nothing for that part: a malformed
/// reply must never fail the heartbeat that carried it.
pub fn closed_subjects(reply: &Value) -> Vec<String> {
    let Some(entries) = reply.get("closed_subjects").and_then(Value::as_array) else {
        return Vec::new();
    };
    entries
        .iter()
        .filter_map(Value::as_str)
        .filter(|subject| valid_subject(subject))
        .map(str::to_owned)
        .collect()
}

/// `acct:` or `org:` followed by a 26-character Crockford base32 ULID, the
/// same grammar provisioning accepts for a tenant's subject.
fn valid_subject(subject: &str) -> bool {
    let Some(ulid) = subject
        .strip_prefix("acct:")
        .or_else(|| subject.strip_prefix("org:"))
    else {
        return false;
    };
    ulid.len() == 26
        && ulid
            .bytes()
            .all(|b| b.is_ascii_digit() || (b.is_ascii_uppercase() && !b"ILOU".contains(&b)))
}

impl Host {
    /// Records the tenants bound to these subjects as closed, durably, and
    /// returns the newly closed tenant names. Sandbox hosts have no tenants of
    /// their own to remove and ignore the list.
    pub fn close_subjects(
        &mut self,
        subjects: &[String],
        now_ms: u64,
    ) -> Result<Vec<String>, Error> {
        if self.provider_managed {
            return Ok(Vec::new());
        }
        let mut next = self.snapshot.clone();
        let mut closed = Vec::new();
        for subject in subjects {
            for tenant in self.tenants.values().filter(|t| &t.sub == subject) {
                if next.closed.contains_key(&tenant.tenant) {
                    continue;
                }
                next.closed.insert(
                    tenant.tenant.clone(),
                    ClosedTenant {
                        sub: subject.clone(),
                        since_ms: now_ms,
                        ..ClosedTenant::default()
                    },
                );
                tracing::warn!(tenant = tenant.tenant, "tenant_account_closed");
                closed.push(tenant.tenant.clone());
            }
        }
        if !closed.is_empty() {
            self.save(next)?;
        }
        Ok(closed)
    }
    pub fn is_closed(&self, tenant: &str) -> bool {
        self.snapshot.closed.contains_key(tenant)
    }
    pub fn is_removed(&self, tenant: &str) -> bool {
        self.snapshot
            .closed
            .get(tenant)
            .is_some_and(|c| c.removed_ms.is_some())
    }
    /// Closed tenants ready for removal now: no lease still open, no relay or
    /// connection setup in flight, no suspend running, the last job ended at
    /// least `RUNNER_RETENTION_MS` ago, and no failed attempt backing off.
    /// Also logs, once, each closed tenant still present after
    /// `REMOVAL_OVERDUE_MS`.
    pub fn removals_due(&mut self, now_ms: u64) -> Result<Vec<String>, Error> {
        let mut overdue = Vec::new();
        let mut due = Vec::new();
        for (tenant, closed) in &self.snapshot.closed {
            if closed.removed_ms.is_some() {
                continue;
            }
            if now_ms.saturating_sub(closed.since_ms) >= REMOVAL_OVERDUE_MS
                && !closed.overdue_logged
            {
                overdue.push(tenant.clone());
            }
            if now_ms >= closed.retry_at_ms && self.removal_ready(tenant, now_ms) {
                due.push(tenant.clone());
            }
        }
        if !overdue.is_empty() {
            let mut next = self.snapshot.clone();
            for tenant in &overdue {
                let closed = next.closed.get_mut(tenant).expect("closed tenant");
                tracing::error!(
                    tenant,
                    since_ms = closed.since_ms,
                    attempts = closed.attempts,
                    "tenant_removal_overdue: a closed account's tenant disk is still on this host"
                );
                closed.overdue_logged = true;
            }
            self.save(next)?;
        }
        Ok(due)
    }
    /// The removal conditions other than backoff; rechecked under the
    /// tenant's operation lock just before the helper is asked.
    pub fn removal_ready(&self, tenant: &str, now_ms: u64) -> bool {
        if !self.is_closed(tenant) || self.is_removed(tenant) || self.is_suspending(tenant) {
            return false;
        }
        let leases: Vec<_> = self
            .snapshot
            .leases
            .values()
            .filter(|l| l.tenant == tenant)
            .collect();
        let open = leases.iter().any(|l| l.state != LeaseState::Ended);
        let last_end = leases.iter().filter_map(|l| l.ended_at_ms).max();
        let drained = last_end.is_none_or(|end| now_ms.saturating_sub(end) >= RUNNER_RETENTION_MS);
        !open && drained && !self.tenant_busy(tenant)
    }
    /// Records a failed removal attempt and when to retry it.
    pub fn removal_failed(&mut self, tenant: &str, now_ms: u64, error: &str) -> Result<(), Error> {
        let mut next = self.snapshot.clone();
        let Some(closed) = next.closed.get_mut(tenant) else {
            return Ok(());
        };
        closed.attempts = closed.attempts.saturating_add(1);
        let wait = REMOVAL_RETRY_MS
            .saturating_mul(1 << closed.attempts.saturating_sub(1).min(20))
            .min(REMOVAL_RETRY_MAX_MS);
        closed.retry_at_ms = now_ms.saturating_add(wait);
        if now_ms.saturating_sub(closed.since_ms) >= REMOVAL_OVERDUE_MS {
            tracing::error!(tenant, attempts = closed.attempts, error, retry_in_ms = wait,
                "tenant_removal_failed_overdue: a closed account's tenant disk is still on this host");
        } else {
            tracing::warn!(
                tenant,
                attempts = closed.attempts,
                error,
                retry_in_ms = wait,
                "tenant_removal_failed"
            );
        }
        self.save(next)
    }
    /// Records a completed removal and forgets the tenant: its binding is
    /// gone, so no token can be admitted for it again.
    pub fn tenant_removed(
        &mut self,
        tenant: &str,
        quarantine: &str,
        now_ms: u64,
    ) -> Result<(), Error> {
        let mut next = self.snapshot.clone();
        let Some(closed) = next.closed.get_mut(tenant) else {
            return Ok(());
        };
        closed.removed_ms = Some(now_ms);
        closed.quarantine = Some(quarantine.to_owned());
        next.vms.remove(tenant);
        self.save(next)?;
        self.tenants.retain(|_, t| t.tenant != tenant);
        self.guest_unready(tenant);
        tracing::info!(tenant, quarantine, "tenant_removed");
        Ok(())
    }
}
