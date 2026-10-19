//! Interim mapping of CLOUD's Boat lease fields. Keep provider-specific wire
//! names here until the broker contract is finalized; first-party leases retain
//! the closed motor-lease schema.
use super::Error;
use motor_lease::LeaseResponse;
use serde::{Deserialize, Deserializer, Serialize};
use std::{net::SocketAddrV4, ops::Deref};

#[derive(Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct Ssh {
    pub endpoint: String,
    pub host_key: String,
    pub user: String,
}

#[derive(Clone)]
pub(crate) struct Lease {
    pub response: LeaseResponse,
    pub ssh: Option<Ssh>,
    pub runner_port: Option<u16>,
}
impl Deref for Lease {
    type Target = LeaseResponse;
    fn deref(&self) -> &Self::Target {
        &self.response
    }
}
impl<'de> Deserialize<'de> for Lease {
    fn deserialize<D: Deserializer<'de>>(de: D) -> Result<Self, D::Error> {
        let mut value = serde_json::Value::deserialize(de)?;
        let object = value
            .as_object_mut()
            .ok_or_else(|| serde::de::Error::custom("lease object"))?;
        let provider = object.remove("provider");
        let ssh = object.remove("ssh");
        let runner_port = object.remove("runner_port");
        let boat = provider.as_ref().and_then(|p| p.as_str()) == Some("boat");
        if provider.is_some()
            && !boat
            && provider.as_ref().and_then(|p| p.as_str()) != Some("first_party")
        {
            return Err(serde::de::Error::custom("unsupported provider"));
        }
        if boat {
            object.entry("endpoint").or_insert(serde_json::json!(""));
            object
                .entry("endpoint_spki_sha256")
                .or_insert(serde_json::json!(""));
        } else if ssh.is_some() || runner_port.is_some() {
            return Err(serde::de::Error::custom(
                "SSH placement requires Boat provider",
            ));
        }
        let lease = Self {
            response: serde_json::from_value(value).map_err(serde::de::Error::custom)?,
            ssh: ssh
                .map(serde_json::from_value)
                .transpose()
                .map_err(serde::de::Error::custom)?,
            runner_port: runner_port
                .map(serde_json::from_value)
                .transpose()
                .map_err(serde::de::Error::custom)?,
        };
        if boat {
            lease
                .validate_ssh()
                .map_err(|_| serde::de::Error::custom("invalid Boat placement"))?;
        }
        Ok(lease)
    }
}
impl Lease {
    fn validate_ssh(&self) -> Result<(), Error> {
        let ssh = self.ssh.as_ref().ok_or(Error::Auth)?;
        let endpoint: SocketAddrV4 = ssh.endpoint.parse().map_err(|_| Error::Auth)?;
        if endpoint.port() == 0
            || self.runner_port.is_none_or(|p| p == 0)
            || ssh.user.is_empty()
            || ssh.user.starts_with('-')
            || !ssh
                .user
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || b"_-".contains(&b))
            || self
                .placement
                .vm
                .as_ref()
                .is_none_or(|vm| self.placement.host != format!("boat:{vm}"))
        {
            return Err(Error::Auth);
        }
        let key = ssh_key::PublicKey::from_openssh(&ssh.host_key).map_err(|_| Error::Auth)?;
        if !key.algorithm().is_ed25519() || ssh.host_key.contains(['\r', '\n']) {
            return Err(Error::Auth);
        }
        Ok(())
    }
}
pub(crate) fn terminal_placement(placement: &motor_lease::Placement) -> String {
    if placement.host.starts_with("boat:") {
        format!("boat/{}", placement.vm.as_deref().unwrap_or_default())
    } else {
        format!("first_party/{}", placement.host)
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct Finish {
    pub outcome: String,
    pub run_ms: u64,
}
impl Finish {
    pub(crate) fn from_terminal(terminal: &motor_protocol::TerminalRecord, max_s: u64) -> Self {
        use motor_protocol::{Killed, Outcome};
        let outcome = match (&terminal.killed, &terminal.outcome) {
            (Some(Killed::Deadline), _) => "timeout",
            (Some(Killed::Cancel), _) | (_, Outcome::Cancelled) => "cancelled",
            (_, Outcome::Exit { .. }) => "exit",
            (_, Outcome::Signal { .. }) => "signal",
            _ => "lost",
        };
        Self {
            outcome: outcome.into(),
            run_ms: terminal.wall_ms.min(max_s.saturating_mul(1000)),
        }
    }
}

#[derive(Deserialize)]
pub(super) struct FinishReply {
    pub job_id: String,
    pub settled_vcpu_ms: u64,
    pub released_vcpu_ms: u64,
}
