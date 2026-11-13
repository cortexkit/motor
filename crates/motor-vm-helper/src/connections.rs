use crate::{Error, Peer, Result};
use std::{
    collections::BTreeMap,
    sync::{
        atomic::{AtomicBool, Ordering},
        Mutex,
    },
};

struct Connections {
    allowed_uids: Vec<u32>,
    peers: BTreeMap<u64, Peer>,
    next: u64,
}

/// Authorization is independent of VM work so even a busy tenant cannot block
/// registration or job requests. Never acquire another lock while holding the
/// connection-gate lock.
pub(crate) struct ConnectionGate {
    connections: Mutex<Connections>,
    stopping: AtomicBool,
}

impl ConnectionGate {
    pub(crate) fn new(allowed_uids: Vec<u32>) -> Self {
        Self {
            connections: Mutex::new(Connections {
                allowed_uids,
                peers: BTreeMap::new(),
                next: 0,
            }),
            stopping: AtomicBool::new(false),
        }
    }

    pub(crate) fn connect(&self, peer: Peer) -> Result<u64> {
        let mut connections = self.connections.lock().unwrap();
        if !connections.allowed_uids.contains(&peer.uid) {
            return Err(Error("unauthorised peer uid".into()));
        }
        connections.next = connections
            .next
            .checked_add(1)
            .ok_or_else(|| Error("connection IDs exhausted".into()))?;
        let id = connections.next;
        connections.peers.insert(id, peer);
        Ok(id)
    }

    pub(crate) fn authorize(&self, id: u64) -> Result<Peer> {
        let connections = self.connections.lock().unwrap();
        let peer = *connections
            .peers
            .get(&id)
            .ok_or_else(|| Error("connection is not authorised".into()))?;
        self.check_running()?;
        Ok(peer)
    }

    pub(crate) fn disconnect(&self, id: u64) {
        self.connections.lock().unwrap().peers.remove(&id);
    }

    pub(crate) fn stop(&self) {
        self.stopping.store(true, Ordering::SeqCst);
    }

    pub(crate) fn check_running(&self) -> Result<()> {
        if self.stopping.load(Ordering::SeqCst) {
            Err(Error("helper is stopping".into()))
        } else {
            Ok(())
        }
    }
}
