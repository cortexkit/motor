//! The helper owns machine-wide leases, not the runner. A connection retains a
//! slot across fetch, job and scan VMs; only explicit release makes it available.
use serde::{Deserialize, Serialize};
use std::{collections::BTreeMap, fmt, os::fd::OwnedFd};
use uuid::Uuid;

pub mod config;
#[cfg(target_os = "linux")]
pub mod linux;
pub mod tenant;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Error(pub String);
impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.0.fmt(f)
    }
}
impl std::error::Error for Error {}
impl From<std::io::Error> for Error {
    fn from(e: std::io::Error) -> Self {
        Self(e.to_string())
    }
}
pub type Result<T> = std::result::Result<T, Error>;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Kind {
    Job,
    Scan,
    Fetch,
    Warm,
}

/// The wire schema has no caller-supplied paths, commands, or launch arguments.
#[derive(Debug, Deserialize)]
#[serde(tag = "op", rename_all = "snake_case", deny_unknown_fields)]
pub enum Request {
    Acquire {
        instance: String,
        job_id: String,
    },
    Start {
        slot: usize,
        instance: String,
        job_id: String,
        kind: Kind,
    },
    Stop {
        slot: usize,
        instance: String,
        job_id: String,
        export: bool,
    },
    Release {
        slot: usize,
        instance: String,
        job_id: String,
    },
    List {
        instance: String,
    },
    Ensure {
        tenant: String,
    },
    #[serde(alias = "attach-disk")]
    AttachDisk {
        tenant: String,
    },
    Suspend {
        tenant: String,
    },
    Restore {
        tenant: String,
    },
    #[serde(alias = "stop-tenant")]
    StopTenant {
        tenant: String,
    },
    #[serde(alias = "set-cpu-max")]
    SetCpuMax {
        tenant: String,
        vcpus: u32,
    },
    #[serde(alias = "set-egress")]
    SetEgress {
        tenant: String,
    },
    #[serde(alias = "open-relay")]
    OpenRelay {
        tenant: String,
        port: u32,
    },
    Adopt {
        tenant: String,
    },
    TenantMetrics {
        tenant: String,
    },
    RecoveryComplete {
        tenant: String,
    },
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Identity {
    pub instance: String,
    pub job_id: Uuid,
}
impl Identity {
    fn parse(instance: String, job_id: &str) -> Result<Self> {
        validate_instance(&instance)?;
        let job_id = Uuid::parse_str(job_id).map_err(|_| Error("invalid job_id UUID".into()))?;
        Ok(Self { instance, job_id })
    }
}
pub fn validate_instance(instance: &str) -> Result<()> {
    if !(1..=32).contains(&instance.len())
        || !instance
            .bytes()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-')
    {
        return Err(Error("invalid instance name".into()));
    }
    Ok(())
}

#[derive(Debug, Clone, Copy)]
pub struct Peer {
    pub uid: u32,
    pub gid: u32,
}
#[derive(Debug, Clone)]
pub struct Lease {
    pub identity: Identity,
    pub peer: Peer,
    connection: u64,
    pub running: Option<Kind>,
}
#[derive(Debug, Clone)]
enum Slot {
    Free,
    Leased(Lease),
    Dirty(Lease),
}
#[derive(Debug, Serialize)]
#[serde(tag = "state", rename_all = "snake_case")]
pub enum SlotView {
    Leased {
        slot: usize,
        identity: Identity,
        running: Option<Kind>,
    },
    Dirty {
        slot: usize,
        identity: Identity,
    },
}
#[derive(Debug, Serialize)]
#[serde(tag = "result", rename_all = "snake_case")]
pub enum Reply {
    Acquired {
        slot: Option<usize>,
    },
    Started {
        fd_count: usize,
    },
    Ok,
    List {
        slots: Vec<SlotView>,
    },
    Error {
        message: String,
    },
    Tenant {
        running: bool,
        restored: bool,
        generation: u64,
        #[serde(skip_serializing_if = "std::ops::Not::not")]
        recovery_required: bool,
    },
    Relay {
        fd_count: usize,
    },
    TenantMetrics {
        memory_bytes: u64,
        egress_bytes: u64,
    },
}
pub struct Response {
    pub reply: Reply,
    pub fds: Vec<OwnedFd>,
}
impl Response {
    fn new(reply: Reply) -> Self {
        Self {
            reply,
            fds: Vec::new(),
        }
    }
}

/// Cleanup must include all slot-uid processes, mounts, chroots and cgroups.
/// A failed cleanup is never evidence that the slot is free.
pub trait Machine {
    fn reconcile(&mut self) -> Result<()>;
    fn start(&mut self, slot: usize, lease: &Lease, kind: Kind) -> Result<Vec<OwnedFd>>;
    fn stop(&mut self, slot: usize, lease: &Lease, export: bool) -> Result<()>;
    fn cleanup(&mut self, slot: usize) -> Result<()>;
    fn alive(&mut self, slot: usize) -> bool;
    fn tenant(&mut self, _tenant: &str, _op: tenant::Operation) -> Result<Response> {
        Err(Error("tenant VMs are not configured".into()))
    }
}

pub struct Helper<M: Machine> {
    pub machine: M,
    slots: Vec<Slot>,
    allowed_uids: Vec<u32>,
    connections: BTreeMap<u64, Peer>,
    next_connection: u64,
}
impl<M: Machine> Helper<M> {
    /// Reconciliation is a barrier: a server may bind its socket only after this
    /// returns, including when the previous helper was killed mid-job.
    pub fn new(mut machine: M, count: usize, allowed_uids: Vec<u32>) -> Result<Self> {
        if count == 0 || allowed_uids.is_empty() {
            return Err(Error("slots and authorised uids must not be empty".into()));
        }
        machine.reconcile()?;
        Ok(Self {
            machine,
            slots: vec![Slot::Free; count],
            allowed_uids,
            connections: BTreeMap::new(),
            next_connection: 0,
        })
    }
    pub fn connect(&mut self, peer: Peer) -> Result<u64> {
        if !self.allowed_uids.contains(&peer.uid) {
            return Err(Error("unauthorised peer uid".into()));
        }
        self.next_connection = self
            .next_connection
            .checked_add(1)
            .ok_or_else(|| Error("connection IDs exhausted".into()))?;
        self.connections.insert(self.next_connection, peer);
        Ok(self.next_connection)
    }
    fn matching(&self, connection: u64, slot: usize, identity: &Identity) -> Result<Lease> {
        match self.slots.get(slot) {
            Some(Slot::Leased(lease))
                if lease.connection == connection && lease.identity == *identity =>
            {
                Ok(lease.clone())
            }
            None => Err(Error("slot outside configured range".into())),
            _ => Err(Error("lease identity or connection does not match".into())),
        }
    }
    pub fn handle(&mut self, connection: u64, request: Request) -> Result<Response> {
        let peer = *self
            .connections
            .get(&connection)
            .ok_or_else(|| Error("connection is not authorised".into()))?;
        match request {
            Request::Acquire { instance, job_id } => {
                let identity = Identity::parse(instance, &job_id)?;
                if self.slots.iter().any(
                    |s| matches!(s, Slot::Leased(l) | Slot::Dirty(l) if l.identity == identity),
                ) {
                    return Err(Error("job already holds a lease".into()));
                }
                let slot = self.slots.iter().position(|s| matches!(s, Slot::Free));
                if let Some(n) = slot {
                    self.slots[n] = Slot::Leased(Lease {
                        identity,
                        peer,
                        connection,
                        running: None,
                    });
                }
                Ok(Response::new(Reply::Acquired { slot }))
            }
            Request::Start {
                slot,
                instance,
                job_id,
                kind,
            } => {
                let identity = Identity::parse(instance, &job_id)?;
                let lease = self.matching(connection, slot, &identity)?;
                if lease.running.is_some() {
                    return Err(Error("slot already has a running VM".into()));
                }
                match self.machine.start(slot, &lease, kind) {
                    Ok(fds) => {
                        let mut lease = lease;
                        lease.running = Some(kind);
                        self.slots[slot] = Slot::Leased(lease);
                        Ok(Response {
                            reply: Reply::Started {
                                fd_count: fds.len(),
                            },
                            fds,
                        })
                    }
                    Err(e) => {
                        if self.machine.cleanup(slot).is_err() {
                            self.slots[slot] = Slot::Dirty(lease);
                        }
                        Err(e)
                    }
                }
            }
            Request::Stop {
                slot,
                instance,
                job_id,
                export,
            } => {
                let identity = Identity::parse(instance, &job_id)?;
                let mut lease = self.matching(connection, slot, &identity)?;
                if lease.running.is_none() {
                    return Err(Error("slot has no running VM".into()));
                }
                if let Err(e) = self.machine.stop(slot, &lease, export) {
                    self.slots[slot] = Slot::Dirty(lease);
                    return Err(e);
                }
                lease.running = None;
                self.slots[slot] = Slot::Leased(lease);
                Ok(Response::new(Reply::Ok))
            }
            Request::Release {
                slot,
                instance,
                job_id,
            } => {
                let identity = Identity::parse(instance, &job_id)?;
                let lease = self.matching(connection, slot, &identity)?;
                if lease.running.is_some() {
                    return Err(Error("stop the VM before releasing its lease".into()));
                }
                self.slots[slot] = Slot::Free;
                Ok(Response::new(Reply::Ok))
            }
            Request::List { instance } => {
                validate_instance(&instance)?;
                let slots = self
                    .slots
                    .iter()
                    .enumerate()
                    .filter_map(|(slot, state)| match state {
                        Slot::Leased(l) if l.identity.instance == instance => {
                            Some(SlotView::Leased {
                                slot,
                                identity: l.identity.clone(),
                                running: l.running,
                            })
                        }
                        Slot::Dirty(l) if l.identity.instance == instance => {
                            Some(SlotView::Dirty {
                                slot,
                                identity: l.identity.clone(),
                            })
                        }
                        _ => None,
                    })
                    .collect();
                Ok(Response::new(Reply::List { slots }))
            }
            Request::Ensure { tenant } => self.tenant(&tenant, tenant::Operation::Ensure),
            Request::AttachDisk { tenant } => self.tenant(&tenant, tenant::Operation::AttachDisk),
            Request::Suspend { tenant } => self.tenant(&tenant, tenant::Operation::Suspend),
            Request::Restore { tenant } => self.tenant(&tenant, tenant::Operation::Restore),
            Request::StopTenant { tenant } => self.tenant(&tenant, tenant::Operation::Stop),
            Request::SetCpuMax { tenant, vcpus } => {
                self.tenant(&tenant, tenant::Operation::Cpu(vcpus))
            }
            Request::SetEgress { tenant } => self.tenant(&tenant, tenant::Operation::Egress),
            Request::OpenRelay { tenant, port } => {
                self.tenant(&tenant, tenant::Operation::Relay(port))
            }
            Request::Adopt { tenant } => self.tenant(&tenant, tenant::Operation::Adopt),
            Request::TenantMetrics { tenant } => self.tenant(&tenant, tenant::Operation::Metrics),
            Request::RecoveryComplete { tenant } => {
                self.tenant(&tenant, tenant::Operation::RecoveryComplete)
            }
        }
    }
    fn tenant(&mut self, name: &str, op: tenant::Operation) -> Result<Response> {
        tenant::validate_name(name)?;
        if let tenant::Operation::Relay(port) = op {
            tenant::validate_port(port)?;
        }
        self.machine.tenant(name, op)
    }
    pub fn disconnect(&mut self, connection: u64) {
        self.connections.remove(&connection);
        for state in &mut self.slots {
            if let Slot::Leased(l) = state {
                if l.connection == connection {
                    *state = Slot::Dirty(l.clone());
                }
            }
        }
    }
    /// Called periodically, independently of runner connections. Unexpected
    /// child death revokes the lease and kills the remaining slot processes.
    pub fn maintain(&mut self) {
        for n in 0..self.slots.len() {
            if let Slot::Leased(l) = &self.slots[n] {
                if l.running.is_some() && !self.machine.alive(n) {
                    self.slots[n] = Slot::Dirty(l.clone());
                }
            }
            if matches!(self.slots[n], Slot::Dirty(_)) && self.machine.cleanup(n).is_ok() {
                self.slots[n] = Slot::Free;
            }
        }
    }
}

#[cfg(test)]
mod tests;
