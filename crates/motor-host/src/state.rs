use crate::journal::Journal;
use motor_lease::{Claims, Purpose, Usage, UsageOutcome};
use motor_protocol::{RefusedCause, TenantEvent, TenantEventKind};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::io;

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct Tenant {
    pub tenant: String,
    pub sub: String,
    pub vcpu_ceiling: u32,
    pub guest_ram_mib: u64,
    pub idle_suspend_s: u64,
    pub host_floor_bytes: u64,
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
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Refusal {
    Auth,
    Expired,
    HostFull,
    Capacity,
}
impl Refusal {
    pub fn wire(self) -> &'static str {
        match self {
            Self::Auth => "auth",
            Self::Expired => "expired",
            Self::HostFull => "host_full",
            Self::Capacity => "capacity",
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
fn refuse<T>(r: Refusal) -> Result<T, Error> {
    Err(Error::Refused(r))
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Admission {
    Ready,
    Waiting,
}
#[derive(Clone, Debug)]
struct Connection {
    id: u64,
    job: String,
    purpose: Purpose,
}
#[derive(Default)]
struct LiveVm {
    preludes: u32,
    ready: bool,
    runner_idle: bool,
    space_logged: bool,
    idle_since_ms: Option<u64>,
    events_since_ms: Option<u64>,
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
        }
    }
    fn save(&mut self, next: Snapshot) -> Result<(), Error> {
        self.journal.store(&next)?;
        self.snapshot = next;
        Ok(())
    }
    pub fn binding(&self, claims: &Claims) -> Result<&Tenant, Error> {
        let entry = self
            .tenants
            .get(&claims.placement.vm)
            .ok_or(Error::Refused(Refusal::Auth))?;
        if entry.sub != claims.sub {
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
        self.snapshot
            .leases
            .values()
            .filter(|l| l.tenant == tenant && counted(l, now_ms))
            .map(|l| l.claims.vcpus)
            .sum()
    }
    pub fn quota(&self, tenant: &str, now_ms: u64) -> u32 {
        if !self.live.get(tenant).is_some_and(|v| v.ready) {
            1
        } else {
            self.share(tenant, now_ms).max(1)
        }
    }
    pub fn is_ready(&self, tenant: &str) -> bool {
        self.live.get(tenant).is_some_and(|v| v.ready)
    }
    pub fn guest_ready(&mut self, tenant: &str, generation: u64) -> Result<(), Error> {
        let mut next = self.snapshot.clone();
        next.vms
            .entry(tenant.to_owned())
            .or_default()
            .initialized_generation = Some(generation);
        self.save(next)?;
        self.live.entry(tenant.to_owned()).or_default().ready = true;
        Ok(())
    }
    pub fn guest_unready(&mut self, tenant: &str) {
        self.live.entry(tenant.to_owned()).or_default().ready = false;
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
        let tenant = self.binding(&claims)?.clone();
        if now_ms / 1000 >= claims.exp {
            return refuse(Refusal::Expired);
        }
        let old = self.snapshot.leases.get(&claims.jti).cloned();
        let live = self
            .connections
            .iter()
            .any(|c| c.job == claims.jti && c.purpose == claims.purpose);
        let mut next = self.snapshot.clone();
        let mut admission = Admission::Ready;
        if claims.purpose == Purpose::Attach {
            if live || old.is_none() {
                return refuse(Refusal::Auth);
            }
        } else {
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
            if u64::from(self.share(&tenant.tenant, now_ms)) - u64::from(previous_share)
                + u64::from(claims.vcpus)
                > u64::from(tenant.vcpu_ceiling)
            {
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
                },
            );
            self.save(next)?;
            self.connections
                .retain(|c| c.job != claims.jti || c.purpose != Purpose::Run);
        }
        self.next_connection += 1;
        let id = self.next_connection;
        self.connections.push(Connection {
            id,
            job: claims.jti,
            purpose: claims.purpose,
        });
        self.live.entry(tenant.tenant).or_default().idle_since_ms = None;
        Ok((id, admission))
    }
    pub fn pending(&self, job: &str) -> Result<Admission, Error> {
        let lease = &self.snapshot.leases[job];
        if lease.state != LeaseState::Reserved {
            return refuse(Refusal::Auth);
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
            .ok_or(Error::Refused(Refusal::Auth))?;
        if connection.purpose == Purpose::Attach {
            return Ok(());
        }
        let mut next = self.snapshot.clone();
        let lease = next
            .leases
            .get_mut(&connection.job)
            .expect("reserved lease");
        if lease.replacing || lease.state != LeaseState::Reserved {
            return refuse(Refusal::Auth);
        }
        lease.delivered = true;
        self.save(next)
    }
    pub fn close(&mut self, id: u64) -> Result<(), Error> {
        let Some(index) = self.connections.iter().position(|c| c.id == id) else {
            return Ok(());
        };
        let connection = self.connections[index].clone();
        if connection.purpose == Purpose::Run {
            let mut next = self.snapshot.clone();
            let lease = next
                .leases
                .get_mut(&connection.job)
                .expect("reserved lease");
            if lease.state == LeaseState::Reserved && !lease.delivered {
                lease.state = LeaseState::Released;
                self.save(next)?;
            }
        }
        self.connections.remove(index);
        Ok(())
    }
    pub fn watermark(&mut self, job: &str, seq: u64) -> Result<(), Error> {
        let Some(lease) = self.snapshot.leases.get(job) else {
            return refuse(Refusal::Auth);
        };
        if seq > lease.highest_seq {
            let mut next = self.snapshot.clone();
            next.leases.get_mut(job).expect("lease").highest_seq = seq;
            self.save(next)?;
        }
        Ok(())
    }
    pub fn event(&mut self, tenant: &str, event: &TenantEvent, now_ms: u64) -> Result<(), Error> {
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
                match event.event {
                    TenantEventKind::Admitted
                        if lease.state == LeaseState::Reserved && lease.delivered =>
                    {
                        lease.state = LeaseState::Admitted;
                        lease.replacing = false;
                    }
                    TenantEventKind::Started if lease.state == LeaseState::Admitted => {
                        lease.state = LeaseState::Started;
                        lease.started_at_ms = Some(now_ms);
                    }
                    TenantEventKind::Terminal
                        if matches!(lease.state, LeaseState::Admitted | LeaseState::Started) =>
                    {
                        if lease.started_at_ms.is_none() {
                            end_zero(&mut next, &job, now_ms);
                        } else {
                            let lease = next.leases.get_mut(&job).expect("lease");
                            lease.state = LeaseState::Ended;
                            lease.ended_at_ms = Some(now_ms);
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
                                ) => end_zero(&mut next, &job, now_ms),
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
        let mut next = self.snapshot.clone();
        let mut cancel = Vec::new();
        let mut close_jobs = std::collections::BTreeSet::new();
        for (job, lease) in &self.snapshot.leases {
            if matches!(lease.state, LeaseState::Started | LeaseState::Ended) {
                continue;
            }
            let exp_ms = lease.claims.exp.saturating_mul(1000);
            let admission_deadline = lease.claims.iat.saturating_add(360).saturating_mul(1000);
            let continuous = self
                .live
                .get(&lease.tenant)
                .and_then(|v| v.events_since_ms)
                .is_some_and(|since| since <= exp_ms);
            let expired = matches!(lease.state, LeaseState::Reserved | LeaseState::Released)
                && now_ms >= exp_ms;
            if expired || now_ms >= admission_deadline {
                close_jobs.insert(job.clone());
            }
            if now_ms >= admission_deadline {
                cancel.push((lease.tenant.clone(), job.clone()));
                next.pending_cancels
                    .insert(job.clone(), lease.tenant.clone());
                end_zero(&mut next, job, now_ms);
            } else if expired
                && ((!lease.delivered && !lease.replacing)
                    || (continuous && now_ms >= exp_ms.saturating_add(60_000)))
            {
                end_zero(&mut next, job, now_ms);
            }
        }
        if !cancel.is_empty() || next.zero_usage.len() != self.snapshot.zero_usage.len() {
            self.save(next)?;
        }
        self.connections.retain(|c| {
            if c.purpose == Purpose::Attach {
                return true;
            }
            !close_jobs.contains(&c.job)
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
    pub fn suspend_due(&mut self, tenant: &str, now_ms: u64, available_bytes: u64) -> bool {
        let Some(config) = self.tenants.values().find(|t| t.tenant == tenant) else {
            return false;
        };
        let busy = self.snapshot.leases.values().any(|l| {
            l.tenant == tenant
                && matches!(
                    l.state,
                    LeaseState::Reserved | LeaseState::Admitted | LeaseState::Started
                )
        });
        let relay = self.connections.iter().any(|c| {
            self.snapshot
                .leases
                .get(&c.job)
                .is_some_and(|l| l.tenant == tenant)
        });
        let live = self.live.entry(tenant.to_owned()).or_default();
        if !live.ready || !live.runner_idle || live.preludes > 0 || busy || relay {
            live.idle_since_ms = None;
            live.space_logged = false;
            return false;
        }
        let since = *live.idle_since_ms.get_or_insert(now_ms);
        let required = config
            .guest_ram_mib
            .checked_mul(1_048_576)
            .and_then(|ram| ram.checked_add(config.host_floor_bytes));
        let due = now_ms.saturating_sub(since) >= config.idle_suspend_s.saturating_mul(1000);
        let space = required.is_some_and(|bytes| available_bytes >= bytes);
        if due && !space && !live.space_logged {
            eprintln!("suspend_skipped_space");
            live.space_logged = true;
        }
        if space {
            live.space_logged = false;
        }
        due && space
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
fn end_zero(next: &mut Snapshot, job: &str, now_ms: u64) {
    let lease = next.leases.get_mut(job).expect("lease");
    lease.state = LeaseState::Ended;
    lease.ended_at_ms = Some(now_ms);
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
}
