use crate::journal::Journal;
use motor_lease::{Claims, Purpose, Usage, UsageOutcome};
use motor_protocol::{RefusedCause, TenantEvent, TenantEventKind};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::io;

mod advertise;
pub use advertise::{MemoryCase, MEMORY_FRESH_MS, MEMORY_PROBE_MS, START_RETRY_MS};

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct Tenant {
    pub tenant: String,
    pub sub: String,
    pub vcpu_ceiling: u32,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_weight: Option<u32>,
    pub guest_ram_mib: u64,
    pub idle_suspend_s: u64,
    pub host_floor_bytes: u64,
}
impl Tenant {
    pub fn budget(&self) -> u32 {
        self.max_weight.unwrap_or(self.vcpu_ceiling)
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Deserialize, Serialize)]
pub enum LeaseState {
    Reserved,
    Released,
    Admitted,
    Started,
    Ended,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct Lease {
    pub claims: Claims,
    pub tenant: String,
    pub state: LeaseState,
    pub delivered: bool,
    pub replacing: bool,
    pub started_at_ms: Option<u64>,
    pub ended_at_ms: Option<u64>,
    pub highest_seq: u64,
    #[serde(default)]
    pub refusal: Option<Refusal>,
}

#[derive(Clone, Debug, Default, Deserialize, Serialize)]
pub struct Vm {
    pub initialized_generation: Option<u64>,
    pub stop_pending: bool,
    pub event_seq: u64,
}

#[derive(Clone, Debug, Default, Deserialize, Serialize)]
pub struct Snapshot {
    pub leases: BTreeMap<String, Lease>,
    pub nonces: BTreeMap<String, u64>,
    pub vms: BTreeMap<String, Vm>,
    /// No-start reports are durable inputs to the signed usage outbox.
    pub zero_usage: BTreeMap<String, Usage>,
    pub pending_cancels: BTreeMap<String, String>,
    #[serde(default)]
    pub billing: BTreeMap<String, crate::billing::JobBilling>,
    #[serde(default)]
    pub outbox: Vec<crate::billing::Request>,
    #[serde(default)]
    pub expired_outbox: Vec<crate::billing::Request>,
    /// Requests the broker refused as malformed (400), or that broke the
    /// contract grammar before signing. Resending the same body cannot
    /// succeed, so they are set aside here for diagnosis, newest last.
    #[serde(default)]
    pub rejected_outbox: Vec<crate::billing::Request>,
    #[serde(default)]
    pub counter: u64,
    #[serde(default)]
    pub last_alive: u64,
    /// Tenants whose accounts the broker reported closed, by tenant name.
    #[serde(default)]
    pub closed: BTreeMap<String, crate::closure::ClosedTenant>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Deserialize, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Refusal {
    Auth,
    Expired,
    HostFull,
    Capacity,
    InternalFailure,
    /// The host could not record the lease because its disk is full.
    StorageFull,
}
impl Refusal {
    pub fn wire(self) -> &'static str {
        match self {
            Self::Auth => "auth",
            Self::Expired => "expired",
            Self::HostFull => "host_full",
            Self::Capacity => "capacity",
            Self::InternalFailure => "internal_failure",
            Self::StorageFull => "storage_full",
        }
    }
}

#[derive(Debug)]
pub enum Error {
    Refused(Refusal),
    Journal(io::Error),
}
impl From<io::Error> for Error {
    fn from(value: io::Error) -> Self {
        Self::Journal(value)
    }
}
impl std::fmt::Display for Error {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Refused(r) => f.write_str(r.wire()),
            Self::Journal(e) => write!(f, "journal: {e}"),
        }
    }
}
impl std::error::Error for Error {}
impl Error {
    /// The refusal a requester receives when this error ends its admission.
    /// A journal write that ran out of space is `storage_full`; any other
    /// journal failure is `internal_failure`.
    pub fn refusal(&self) -> Refusal {
        match self {
            Self::Refused(reason) => *reason,
            Self::Journal(error) if error.kind() == io::ErrorKind::StorageFull => {
                Refusal::StorageFull
            }
            Self::Journal(_) => Refusal::InternalFailure,
        }
    }
}
fn refuse<T>(r: Refusal) -> Result<T, Error> {
    Err(Error::Refused(r))
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Admission {
    Ready,
    Waiting,
}
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ConnectionState {
    Waiting,
    Ready,
    Live(LeaseState),
    Refused(Refusal),
    Closed,
}
#[derive(Clone, Debug)]
struct Connection {
    id: u64,
    job: String,
    purpose: Purpose,
    updates: tokio::sync::watch::Sender<ConnectionState>,
}
#[derive(Default)]
struct LiveVm {
    preludes: u32,
    ready: bool,
    suspending: bool,
    quota_generation: u64,
    quota_target: Option<u32>,
    applied_quota_generation: u64,
    runner_idle: bool,
    space_logged: bool,
    idle_since_ms: Option<u64>,
    suspend_failures: u32,
    suspend_retry_after_ms: u64,
    events_since_ms: Option<u64>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct QuotaUpdate {
    pub tenant: String,
    pub generation: u64,
    pub vcpus: u32,
}

/// All callers serialize this owner. Persisting a candidate before publishing it
/// prevents a failed fsync from authorizing a relay or a lifecycle acknowledgement.
pub struct Host {
    pub tenants: BTreeMap<String, Tenant>,
    pub snapshot: Snapshot,
    journal: Box<dyn Journal>,
    connections: Vec<Connection>,
    live: BTreeMap<String, LiveVm>,
    next_connection: u64,
    observed_at_ms: u64,
    pub(crate) restart_ms: u64,
    pub(crate) prior_alive_ms: u64,
    pub(crate) provider_managed: bool,
    /// When the next billing-record retention pass is due; see `retention_tick`.
    pub(crate) retention_due_ms: u64,
    storage: advertise::StorageLatch,
    /// The capacity file as last read, `None` while it is missing or invalid.
    capacity: Option<crate::capacity::Capacity>,
    advert: BTreeMap<String, advertise::TenantAdvert>,
}
impl Host {
    pub fn new(
        tenants: BTreeMap<String, Tenant>,
        snapshot: Snapshot,
        journal: Box<dyn Journal>,
    ) -> Self {
        Self {
            tenants,
            snapshot,
            journal,
            connections: Vec::new(),
            live: BTreeMap::new(),
            next_connection: 0,
            observed_at_ms: 0,
            restart_ms: 0,
            prior_alive_ms: 0,
            provider_managed: false,
            retention_due_ms: 0,
            storage: Default::default(),
            capacity: None,
            advert: BTreeMap::new(),
        }
    }
    /// Replaces the journal, so tests can make writes fail the way a full
    /// disk does. Production never swaps the journal of a running host.
    #[doc(hidden)]
    pub fn replace_journal(&mut self, journal: Box<dyn Journal>) {
        self.journal = journal;
    }
    pub(crate) fn sandbox(ceiling: u32, snapshot: Snapshot, journal: Box<dyn Journal>) -> Self {
        let mut host = Self::new(
            BTreeMap::from([(
                "sandbox".into(),
                Tenant {
                    tenant: "sandbox".into(),
                    sub: String::new(),
                    vcpu_ceiling: ceiling,
                    max_weight: None,
                    guest_ram_mib: 0,
                    idle_suspend_s: u64::MAX,
                    host_floor_bytes: 0,
                },
            )]),
            snapshot,
            journal,
        );
        host.provider_managed = true;
        host
    }
    pub(crate) fn save(&mut self, mut next: Snapshot) -> Result<(), Error> {
        if !self.provider_managed {
            crate::billing::mark_overlap(&mut next);
        }
        self.journal.store(&next)?;
        self.snapshot = next;
        self.publish_connections();
        Ok(())
    }
    fn publish_connections(&self) {
        for connection in &self.connections {
            let current = *connection.updates.borrow();
            if self
                .snapshot
                .leases
                .get(&connection.job)
                .is_some_and(|lease| lease.refusal == Some(Refusal::Auth))
            {
                finish_connection(connection, ConnectionState::Refused(Refusal::Auth));
                continue;
            }
            let next = match current {
                ConnectionState::Closed | ConnectionState::Refused(_) => continue,
                ConnectionState::Waiting | ConnectionState::Ready
                    if connection.purpose == Purpose::Run =>
                {
                    match self.pending(&connection.job) {
                        Ok(Admission::Waiting) => ConnectionState::Waiting,
                        Ok(Admission::Ready) => ConnectionState::Ready,
                        Err(Error::Refused(reason)) => ConnectionState::Refused(reason),
                        Err(Error::Journal(_)) => continue,
                    }
                }
                _ => match self.snapshot.leases.get(&connection.job) {
                    Some(lease) => ConnectionState::Live(lease.state),
                    None => ConnectionState::Refused(Refusal::Auth),
                },
            };
            connection.updates.send_if_modified(|state| {
                if *state == next {
                    return false;
                }
                *state = next;
                true
            });
        }
    }
    pub fn watch_connection(
        &self,
        id: u64,
    ) -> Result<tokio::sync::watch::Receiver<ConnectionState>, Error> {
        self.connections
            .iter()
            .find(|c| c.id == id)
            .map(|c| c.updates.subscribe())
            .ok_or(Error::Refused(Refusal::Auth))
    }
    pub fn set_stop_pending(&mut self, tenant: &str, pending: bool) -> Result<(), Error> {
        let mut next = self.snapshot.clone();
        next.vms.entry(tenant.to_owned()).or_default().stop_pending = pending;
        self.save(next)
    }
    pub fn binding(&self, claims: &Claims) -> Result<&Tenant, Error> {
        // Sandbox placement is authenticated against its freshly read identity
        // before this shared reservation state is reached. The broker, not a
        // second local tenant registry, binds the subject to that sandbox.
        let entry = if self.provider_managed {
            self.tenants.get("sandbox")
        } else {
            match claims.placement.vm.as_deref() {
                Some(vm) => self.tenants.get(vm),
                // A cold placement names no VM: the broker picked this host,
                // not one of its VMs. The subject then selects the tenant, and
                // only when exactly one VM on this host belongs to it, so a
                // token can never pick between two tenants' VMs.
                None => {
                    let mut owned = self.tenants.values().filter(|t| t.sub == claims.sub);
                    match (owned.next(), owned.next()) {
                        (Some(only), None) => Some(only),
                        _ => None,
                    }
                }
            }
        }
        .ok_or(Error::Refused(Refusal::Auth))?;
        if !self.provider_managed && entry.sub != claims.sub {
            return refuse(Refusal::Auth);
        }
        if let Some(lease) = self.snapshot.leases.get(&claims.jti) {
            if lease.tenant != entry.tenant
                || lease.claims.sub != claims.sub
                || lease.claims.placement != claims.placement
            {
                return refuse(Refusal::Auth);
            }
        }
        Ok(entry)
    }
    pub fn consume_nonce(&mut self, nonce: String, ts: u64, now_s: u64) -> Result<(), Error> {
        if now_s.abs_diff(ts) > 60 || self.snapshot.nonces.contains_key(&nonce) {
            return refuse(Refusal::Auth);
        }
        let mut next = self.snapshot.clone();
        next.nonces.retain(|_, until| *until >= now_s);
        next.nonces.insert(nonce, ts.saturating_add(60));
        self.save(next)
    }
    pub fn share(&self, tenant: &str, now_ms: u64) -> u32 {
        self.checked_share(tenant, now_ms).unwrap_or(u32::MAX)
    }
    fn checked_share(&self, tenant: &str, now_ms: u64) -> Option<u32> {
        self.snapshot
            .leases
            .values()
            .filter(|l| l.tenant == tenant && counted(l, now_ms))
            .map(|l| l.claims.vcpus)
            .try_fold(0u32, u32::checked_add)
    }
    /// The VM's CPU quota in vCPUs, from its counted lease claims, capped at
    /// the tenant's ceiling and never below one. A guest's fill report does
    /// not affect it.
    pub fn quota(&self, tenant: &str, now_ms: u64) -> u32 {
        if !self.live.get(tenant).is_some_and(|v| v.ready) {
            1
        } else {
            let ceiling = self
                .tenants
                .values()
                .find(|t| t.tenant == tenant)
                .map_or(1, |t| t.vcpu_ceiling);
            self.share(tenant, now_ms).min(ceiling).max(1)
        }
    }
    pub fn is_ready(&self, tenant: &str) -> bool {
        self.live.get(tenant).is_some_and(|v| v.ready)
    }
    pub(crate) fn is_suspending(&self, tenant: &str) -> bool {
        self.live.get(tenant).is_some_and(|v| v.suspending)
    }
    pub(crate) fn quota_update(
        &mut self,
        tenant: &str,
        now_ms: u64,
    ) -> Result<Option<QuotaUpdate>, Error> {
        if !self.is_ready(tenant) || self.is_suspending(tenant) {
            return Ok(None);
        }
        let vcpus = self.quota(tenant, now_ms);
        let live = self.live.entry(tenant.to_owned()).or_default();
        if live.quota_target != Some(vcpus) {
            live.quota_generation = live
                .quota_generation
                .checked_add(1)
                .ok_or(Error::Refused(Refusal::Capacity))?;
            live.quota_target = Some(vcpus);
        }
        Ok(
            (live.applied_quota_generation < live.quota_generation).then(|| QuotaUpdate {
                tenant: tenant.to_owned(),
                generation: live.quota_generation,
                vcpus,
            }),
        )
    }
    pub(crate) fn quota_applied(&mut self, update: &QuotaUpdate) {
        let live = self.live.entry(update.tenant.clone()).or_default();
        live.applied_quota_generation = live.applied_quota_generation.max(update.generation);
    }
    pub fn guest_ready(&mut self, tenant: &str, generation: u64) -> Result<(), Error> {
        let mut next = self.snapshot.clone();
        next.vms
            .entry(tenant.to_owned())
            .or_default()
            .initialized_generation = Some(generation);
        self.save(next)?;
        let live = self.live.entry(tenant.to_owned()).or_default();
        live.ready = true;
        live.quota_target = None;
        self.start_succeeded(tenant);
        self.reset_memory(tenant);
        Ok(())
    }
    pub fn guest_unready(&mut self, tenant: &str) {
        let live = self.live.entry(tenant.to_owned()).or_default();
        live.ready = false;
        live.quota_target = None;
        self.reset_memory(tenant);
    }
    pub fn events_connected(&mut self, tenant: &str, now_ms: Option<u64>) {
        self.live
            .entry(tenant.to_owned())
            .or_default()
            .events_since_ms = now_ms;
    }
    pub fn connection_live(&self, id: u64) -> bool {
        self.connections.iter().any(|c| c.id == id)
    }
    pub fn claim(&mut self, claims: Claims, now_ms: u64) -> Result<(u64, Admission), Error> {
        self.observed_at_ms = now_ms;
        let tenant = self.binding(&claims)?.clone();
        if now_ms / 1000 >= claims.exp {
            return refuse(Refusal::Expired);
        }
        if self.is_suspending(&tenant.tenant) {
            // The current first-party TLS connector budgets 60 seconds for admission and
            // runner hello. A memory snapshot can outlast that entire budget;
            // retain a typed retryable refusal instead of a silent client timeout.
            return refuse(Refusal::Capacity);
        }
        // A runner cleaning old state can take minutes to listen. Do not
        // admit another lease that would wait behind that startup handshake.
        if self.is_waking(&tenant.tenant) {
            return refuse(Refusal::HostFull);
        }
        let old = self.snapshot.leases.get(&claims.jti).cloned();
        // A heartbeat can revoke this host's placement while the old run token
        // is still valid. Its signature no longer authorizes admission here.
        if old
            .as_ref()
            .is_some_and(|l| l.refusal == Some(Refusal::Auth))
        {
            return refuse(Refusal::Auth);
        }
        let live = self
            .connections
            .iter()
            .any(|c| c.job == claims.jti && c.purpose == claims.purpose);
        let mut next = self.snapshot.clone();
        let mut admission = Admission::Ready;
        if claims.purpose == Purpose::Run && self.is_closed(&tenant.tenant) {
            // A closed account starts no new job. Attach stays open so a
            // client can still collect a result already running or finished.
            return refuse(Refusal::Auth);
        }
        if claims.purpose == Purpose::Attach {
            if live || old.is_none() {
                return refuse(Refusal::Auth);
            }
        } else {
            if claims.vcpus > tenant.vcpu_ceiling {
                return refuse(Refusal::HostFull);
            }
            // Low host storage stops new work. A lease already delivered to
            // the runner may be replaced, because its job can already be in
            // the guest.
            if self.storage_refusing() && !old.as_ref().is_some_and(|l| l.delivered) {
                return refuse(Refusal::HostFull);
            }
            // A tenant the helper refused to start gets no lease until a
            // start succeeds; motor-host retries the start on its own.
            if self.start_retry_at(&tenant.tenant).is_some() {
                return refuse(Refusal::HostFull);
            }
            // Released leases still appear in heartbeats, even though they no
            // longer consume weight. Reusing a job id needs no additional entry.
            if old.is_none()
                && self
                    .snapshot
                    .leases
                    .values()
                    .filter(|l| l.state != LeaseState::Ended)
                    .count()
                    >= crate::wire::MAX_ENTRIES
            {
                return refuse(Refusal::HostFull);
            }
            if next.vms.get(&tenant.tenant).is_some_and(|v| v.stop_pending) {
                return refuse(Refusal::Capacity);
            }
            let mut replacing = false;
            let mut delivered = false;
            if let Some(ref lease) = old {
                if matches!(
                    lease.state,
                    LeaseState::Admitted | LeaseState::Started | LeaseState::Ended
                ) {
                    return refuse(Refusal::Auth);
                }
                if !same_reservation(&lease.claims, &claims) {
                    return refuse(Refusal::Auth);
                }
                let reconnect =
                    lease.state == LeaseState::Released && !live && lease.claims == claims;
                if claims.iat <= lease.claims.iat && !reconnect {
                    return refuse(Refusal::Auth);
                }
                delivered = lease.delivered;
                replacing = delivered;
            }
            let previous_share = old
                .as_ref()
                .filter(|l| counted(l, now_ms))
                .map_or(0, |l| l.claims.vcpus);
            let share = self
                .checked_share(&tenant.tenant, now_ms)
                .and_then(|share| share.checked_sub(previous_share))
                .and_then(|share| share.checked_add(claims.vcpus));
            if share.is_none_or(|share| share > tenant.budget()) {
                return refuse(Refusal::HostFull);
            }
            if replacing {
                admission = Admission::Waiting;
            }
            next.leases.insert(
                claims.jti.clone(),
                Lease {
                    claims: claims.clone(),
                    tenant: tenant.tenant.clone(),
                    state: LeaseState::Reserved,
                    delivered,
                    replacing,
                    started_at_ms: None,
                    ended_at_ms: None,
                    highest_seq: old.as_ref().map_or(0, |l| l.highest_seq),
                    refusal: None,
                },
            );
            self.save(next)?;
            for connection in self
                .connections
                .iter()
                .filter(|c| c.job == claims.jti && c.purpose == Purpose::Run)
            {
                finish_connection(connection, ConnectionState::Closed);
            }
            self.connections
                .retain(|c| c.job != claims.jti || c.purpose != Purpose::Run);
        }
        self.next_connection += 1;
        let id = self.next_connection;
        self.connections.push(Connection {
            id,
            job: claims.jti,
            purpose: claims.purpose,
            updates: tokio::sync::watch::channel(if admission == Admission::Waiting {
                ConnectionState::Waiting
            } else {
                ConnectionState::Ready
            })
            .0,
        });
        self.live.entry(tenant.tenant).or_default().idle_since_ms = None;
        Ok((id, admission))
    }
    pub fn pending(&self, job: &str) -> Result<Admission, Error> {
        let lease = self
            .snapshot
            .leases
            .get(job)
            .ok_or(Error::Refused(Refusal::Auth))?;
        if let Some(reason) = lease.refusal {
            return refuse(reason);
        }
        match lease.state {
            LeaseState::Released => return refuse(Refusal::Capacity),
            LeaseState::Admitted | LeaseState::Started | LeaseState::Ended => {
                return refuse(Refusal::Auth)
            }
            LeaseState::Reserved => {}
        }
        let tenant = self.binding(&lease.claims)?;
        if self.is_suspending(&tenant.tenant) {
            return refuse(Refusal::Capacity);
        }
        if self
            .snapshot
            .vms
            .get(&lease.tenant)
            .is_some_and(|v| v.stop_pending)
        {
            return refuse(Refusal::Capacity);
        }
        if lease.claims.vcpus > tenant.vcpu_ceiling
            || self
                .checked_share(&lease.tenant, self.observed_at_ms)
                .is_none_or(|share| share > tenant.budget())
        {
            return refuse(Refusal::HostFull);
        }
        Ok(if lease.replacing {
            Admission::Waiting
        } else {
            Admission::Ready
        })
    }
    pub fn delivered(&mut self, id: u64) -> Result<(), Error> {
        let connection = self
            .connections
            .iter()
            .find(|c| c.id == id)
            .cloned()
            .ok_or(Error::Refused(Refusal::Auth))?;
        let mut next = self.snapshot.clone();
        let lease = next
            .leases
            .get_mut(&connection.job)
            .ok_or(Error::Refused(Refusal::Auth))?;
        if connection.purpose == Purpose::Attach {
            connection
                .updates
                .send_replace(ConnectionState::Live(lease.state));
            return Ok(());
        }
        if lease.refusal == Some(Refusal::Auth)
            || lease.replacing
            || lease.state != LeaseState::Reserved
        {
            return refuse(Refusal::Auth);
        }
        lease.delivered = true;
        self.save(next)?;
        connection
            .updates
            .send_replace(ConnectionState::Live(LeaseState::Reserved));
        Ok(())
    }
    pub fn close(&mut self, id: u64) -> Result<(), Error> {
        let Some(connection) = self.connections.iter().find(|c| c.id == id).cloned() else {
            return Ok(());
        };
        if connection.purpose == Purpose::Run {
            let mut next = self.snapshot.clone();
            if let Some(lease) = next
                .leases
                .get_mut(&connection.job)
                .filter(|l| l.state == LeaseState::Reserved && !l.delivered)
            {
                lease.state = LeaseState::Released;
                self.save(next)?;
            }
        }
        finish_connection(&connection, ConnectionState::Closed);
        self.connections.retain(|c| c.id != id);
        Ok(())
    }
    pub fn watermark(&mut self, job: &str, seq: u64) -> Result<(), Error> {
        if self
            .snapshot
            .billing
            .get(job)
            .is_some_and(|b| b.host_settled)
        {
            return Ok(());
        }
        let Some(lease) = self.snapshot.leases.get(job) else {
            return refuse(Refusal::Auth);
        };
        if seq > lease.highest_seq {
            let mut next = self.snapshot.clone();
            next.leases
                .get_mut(job)
                .ok_or(Error::Refused(Refusal::Auth))?
                .highest_seq = seq;
            self.save(next)?;
        }
        Ok(())
    }
    pub fn event(&mut self, tenant: &str, event: &TenantEvent, now_ms: u64) -> Result<(), Error> {
        self.event_observed(tenant, event, now_ms, true)
    }
    pub fn event_observed(
        &mut self,
        tenant: &str,
        event: &TenantEvent,
        now_ms: u64,
        live_event: bool,
    ) -> Result<(), Error> {
        self.observed_at_ms = now_ms;
        let mut next = self.snapshot.clone();
        let vm = next.vms.entry(tenant.to_owned()).or_default();
        if event.event_seq <= vm.event_seq {
            return Ok(());
        }
        vm.event_seq = event.event_seq;
        if event.event == TenantEventKind::Idle {
            if event.jti.is_some() {
                return refuse(Refusal::Auth);
            }
        } else if let Some(job) = event.jti.map(|j| j.to_string()) {
            if let Some(lease) = next.leases.get_mut(&job).filter(|l| l.tenant == tenant) {
                if next.billing.get(&job).is_some_and(|b| b.host_settled) {
                    tracing::warn!(jti = job, "host_settled_runner_event_ignored");
                }
                match event.event {
                    TenantEventKind::Admitted
                        if lease.state == LeaseState::Reserved && lease.delivered =>
                    {
                        lease.state = LeaseState::Admitted;
                        lease.replacing = false;
                    }
                    TenantEventKind::Started if lease.state == LeaseState::Admitted => {
                        lease.state = LeaseState::Started;
                        lease.started_at_ms =
                            Some(if live_event { now_ms } else { self.restart_ms });
                        if !self.provider_managed {
                            next.billing.entry(job.clone()).or_default().started_live = live_event;
                        }
                    }
                    TenantEventKind::Terminal
                        if matches!(lease.state, LeaseState::Admitted | LeaseState::Started) =>
                    {
                        if lease.started_at_ms.is_none() {
                            end_zero(&mut next, &job, now_ms, None)?;
                        } else {
                            let lease = next
                                .leases
                                .get_mut(&job)
                                .ok_or(Error::Refused(Refusal::Auth))?;
                            lease.state = LeaseState::Ended;
                            lease.ended_at_ms = Some(if live_event {
                                now_ms
                            } else {
                                self.prior_alive_ms
                            });
                        }
                        if !self.provider_managed {
                            let billing = next.billing.entry(job.clone()).or_default();
                            billing.terminal_live = live_event;
                            billing.ended_observed_ms = Some(now_ms);
                            billing.mem_peak_mib = event.mem_peak_mib.filter(|v| *v < (1 << 53));
                        }
                    }
                    TenantEventKind::Refused
                        if lease.state == LeaseState::Reserved && lease.delivered =>
                    {
                        if lease.replacing {
                            lease.replacing = false;
                            lease.delivered = false;
                            if !self
                                .connections
                                .iter()
                                .any(|c| c.job == job && c.purpose == Purpose::Run)
                            {
                                lease.state = LeaseState::Released;
                            }
                        } else {
                            match event.cause {
                                Some(RefusedCause::Disconnect) => {
                                    lease.delivered = false;
                                    lease.state = LeaseState::Released;
                                }
                                Some(
                                    RefusedCause::Mismatch
                                    | RefusedCause::Rejected
                                    | RefusedCause::Expired,
                                ) => end_zero(
                                    &mut next,
                                    &job,
                                    now_ms,
                                    (event.cause == Some(RefusedCause::Expired))
                                        .then_some(Refusal::Expired),
                                )?,
                                None => return refuse(Refusal::Auth),
                            }
                        }
                    }
                    _ => {}
                }
            }
        } else {
            return refuse(Refusal::Auth);
        }
        self.save(next)?;
        let live = self.live.entry(tenant.to_owned()).or_default();
        live.runner_idle = event.event == TenantEventKind::Idle;
        if !live.runner_idle {
            live.idle_since_ms = None;
        }
        Ok(())
    }
    /// Expiry is not a no-start proof for a delivered connection. Wait another
    /// 60 seconds with port 53 continuously connected before settling zero, so
    /// an admission queued before expiry can retain the reservation.
    pub fn expire(&mut self, now_ms: u64) -> Result<Vec<(String, String)>, Error> {
        self.observed_at_ms = now_ms;
        let mut next = self.snapshot.clone();
        let mut cancel = Vec::new();
        let mut close_jobs = std::collections::BTreeSet::new();
        for (job, lease) in &self.snapshot.leases {
            if matches!(lease.state, LeaseState::Started | LeaseState::Ended) {
                continue;
            }
            let exp_ms = lease.claims.exp.saturating_mul(1000);
            // Accepted jobs can queue behind other work in the guest. Keep them
            // until the reservation's overall deadline, not the delivery deadline.
            // The broker uses the original issue time; a pre-admission replacement
            // updates these claims, so this safety backstop can run slightly later.
            let unstarted_deadline = if lease.state == LeaseState::Admitted {
                lease
                    .claims
                    .iat
                    .saturating_add(lease.claims.max_s)
                    .saturating_add(600)
                    .saturating_mul(1000)
            } else {
                lease.claims.iat.saturating_add(360).saturating_mul(1000)
            };
            let continuous = self
                .live
                .get(&lease.tenant)
                .and_then(|v| v.events_since_ms)
                .is_some_and(|since| since <= exp_ms);
            let expired = matches!(lease.state, LeaseState::Reserved | LeaseState::Released)
                && now_ms >= exp_ms;
            if expired || now_ms >= unstarted_deadline {
                close_jobs.insert(job.clone());
            }
            if now_ms >= unstarted_deadline {
                cancel.push((lease.tenant.clone(), job.clone()));
                next.pending_cancels
                    .insert(job.clone(), lease.tenant.clone());
                end_zero(&mut next, job, now_ms, expired.then_some(Refusal::Expired))?;
            } else if expired
                && ((!lease.delivered && !lease.replacing)
                    || (continuous && now_ms >= exp_ms.saturating_add(60_000)))
            {
                end_zero(&mut next, job, now_ms, Some(Refusal::Expired))?;
            }
        }
        if !cancel.is_empty() || next.zero_usage.len() != self.snapshot.zero_usage.len() {
            self.save(next)?;
        }
        self.publish_connections();
        self.connections.retain(|c| {
            if c.purpose == Purpose::Attach {
                return true;
            }
            if close_jobs.contains(&c.job) {
                finish_connection(c, ConnectionState::Refused(Refusal::Expired));
                false
            } else {
                true
            }
        });
        Ok(self
            .snapshot
            .pending_cancels
            .iter()
            .map(|(job, tenant)| (tenant.clone(), job.clone()))
            .collect())
    }
    pub fn cancel_delivered(&mut self, job: &str) -> Result<(), Error> {
        let mut next = self.snapshot.clone();
        next.pending_cancels.remove(job);
        self.save(next)
    }
    pub fn prelude(&mut self, tenant: &str, begin: bool) {
        let live = self.live.entry(tenant.to_owned()).or_default();
        if begin {
            live.preludes += 1;
            live.idle_since_ms = None;
        } else {
            live.preludes = live.preludes.saturating_sub(1);
        }
    }
    /// A relay or a connection setup for one of the tenant's jobs is open.
    pub(crate) fn tenant_busy(&self, tenant: &str) -> bool {
        self.live.get(tenant).is_some_and(|v| v.preludes > 0)
            || self.connections.iter().any(|connection| {
                self.snapshot
                    .leases
                    .get(&connection.job)
                    .is_some_and(|lease| lease.tenant == tenant)
            })
    }
    fn tenant_idle(&self, tenant: &str) -> bool {
        let Some(live) = self.live.get(tenant) else {
            return false;
        };
        let busy = self.snapshot.leases.values().any(|lease| {
            lease.tenant == tenant
                && matches!(
                    lease.state,
                    LeaseState::Reserved | LeaseState::Admitted | LeaseState::Started
                )
        });
        let relay = self.connections.iter().any(|connection| {
            self.snapshot
                .leases
                .get(&connection.job)
                .is_some_and(|lease| lease.tenant == tenant)
        });
        live.runner_idle && live.preludes == 0 && !busy && !relay
    }
    pub(crate) fn suspend_candidate(&mut self, tenant: &str, now_ms: u64) -> bool {
        let Some(idle_suspend_s) = self
            .tenants
            .values()
            .find(|t| t.tenant == tenant)
            .map(|t| t.idle_suspend_s)
        else {
            return false;
        };
        let idle = self.is_ready(tenant) && !self.is_suspending(tenant) && self.tenant_idle(tenant);
        let live = self.live.entry(tenant.to_owned()).or_default();
        if idle_suspend_s == 0 || now_ms < live.suspend_retry_after_ms {
            return false;
        }
        if !idle {
            live.idle_since_ms = None;
            live.space_logged = false;
            return false;
        }
        let since = *live.idle_since_ms.get_or_insert(now_ms);
        now_ms.saturating_sub(since) >= idle_suspend_s.saturating_mul(1000)
    }
    pub fn suspend_due(&mut self, tenant: &str, now_ms: u64, available_bytes: u64) -> bool {
        if !self.suspend_candidate(tenant, now_ms) {
            return false;
        }
        let Some(config) = self.tenants.values().find(|t| t.tenant == tenant) else {
            return false;
        };
        let required = config
            .guest_ram_mib
            .checked_mul(1_048_576)
            .and_then(|ram| ram.checked_add(config.host_floor_bytes));
        let space = required.is_some_and(|bytes| available_bytes >= bytes);
        let live = self.live.entry(tenant.to_owned()).or_default();
        if !space && !live.space_logged {
            tracing::warn!(tenant, "suspend_skipped_space");
            live.space_logged = true;
        }
        if space {
            live.space_logged = false;
        }
        space
    }
    pub(crate) fn begin_suspend(
        &mut self,
        tenant: &str,
        now_ms: u64,
        available_bytes: u64,
    ) -> bool {
        if !self.suspend_due(tenant, now_ms, available_bytes) {
            return false;
        }
        self.live.entry(tenant.to_owned()).or_default().suspending = true;
        self.publish_connections();
        true
    }
    pub(crate) fn suspend_still_idle(&self, tenant: &str) -> bool {
        self.is_suspending(tenant) && self.is_ready(tenant) && self.tenant_idle(tenant)
    }
    pub(crate) fn finish_suspend(&mut self, tenant: &str, succeeded: bool) -> bool {
        let still_idle = self.tenant_idle(tenant);
        // A successful snapshot stopped the VM even if a new prelude arrived.
        // Preserve that activity; the next admitted connection must restore it.
        if succeeded {
            self.guest_unready(tenant);
        }
        let live = self.live.entry(tenant.to_owned()).or_default();
        if succeeded {
            live.suspend_failures = 0;
            live.suspend_retry_after_ms = 0;
        }
        live.suspending = false;
        live.idle_since_ms = None;
        self.publish_connections();
        still_idle
    }
    pub(crate) fn suspend_failed(&mut self, tenant: &str, now_ms: u64) {
        let live = self.live.entry(tenant.to_owned()).or_default();
        live.suspend_failures = live.suspend_failures.saturating_add(1);
        // Snapshot failures can saturate shared storage. Activity does not erase
        // this cooldown: a busy/idle cycle must not trigger another large write.
        let delay_ms = (30 * 60 * 1000u64)
            .saturating_mul(1 << live.suspend_failures.saturating_sub(1).min(4))
            .min(6 * 60 * 60 * 1000);
        live.suspend_retry_after_ms = now_ms.saturating_add(delay_ms);
        tracing::info!(
            tenant,
            failures = live.suspend_failures,
            retry_after_ms = live.suspend_retry_after_ms,
            delay_ms,
            "suspend_backoff"
        );
    }
}
fn counted(l: &Lease, now_ms: u64) -> bool {
    match l.state {
        LeaseState::Reserved | LeaseState::Admitted => true,
        LeaseState::Started => l
            .started_at_ms
            .is_some_and(|s| now_ms < s.saturating_add(l.claims.max_s.saturating_mul(1000))),
        _ => false,
    }
}
fn same_reservation(a: &Claims, b: &Claims) -> bool {
    a.jti == b.jti
        && a.sub == b.sub
        && a.placement == b.placement
        && a.vcpus == b.vcpus
        && a.mem_mib == b.mem_mib
        && a.max_s == b.max_s
        && a.net == b.net
        && a.platform == b.platform
}
fn end_zero(
    next: &mut Snapshot,
    job: &str,
    now_ms: u64,
    refusal: Option<Refusal>,
) -> Result<(), Error> {
    let lease = next
        .leases
        .get_mut(job)
        .ok_or(Error::Refused(Refusal::Auth))?;
    lease.state = LeaseState::Ended;
    lease.ended_at_ms = Some(now_ms);
    lease.refusal = refusal;
    next.zero_usage
        .entry(job.to_owned())
        .or_insert_with(|| Usage {
            job_id: job.to_owned(),
            vcpus: lease.claims.vcpus,
            started_at_ms: now_ms,
            ended_at_ms: now_ms,
            mem_peak_mib: None,
            egress_bytes: None,
            outcome: UsageOutcome::Cancelled,
        });
    Ok(())
}

fn finish_connection(connection: &Connection, next: ConnectionState) {
    // Cleanup can close the sender before its receiver reads a refusal. Keep
    // the terminal cause available even after the connection record is removed.
    connection.updates.send_if_modified(|state| {
        if matches!(
            *state,
            ConnectionState::Closed | ConnectionState::Refused(_)
        ) {
            return false;
        }
        *state = next;
        true
    });
}
