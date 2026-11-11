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
    // Display metadata only; transport selection still uses the validated lease.
    provider: Option<String>,
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
            provider: provider
                .as_ref()
                .and_then(|p| p.as_str())
                .map(str::to_owned),
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
    pub(crate) fn caller_placement(&self) -> Option<cortexkit_exec_remote_types::Placement> {
        caller_placement(self.provider.as_deref(), Some(&self.placement.host))
    }

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

fn caller_placement(
    provider: Option<&str>,
    host: Option<&str>,
) -> Option<cortexkit_exec_remote_types::Placement> {
    use cortexkit_exec_remote_types::Placement;
    let provider = provider?;
    if matches!(provider, "boat" | "overflow") {
        return Some(Placement::new("overflow"));
    }
    let mut placement = Placement::new(provider);
    if let Some(host) = host {
        placement = placement.with_host(host);
    }
    Some(placement)
}

pub(crate) fn terminal_placement(placement: &motor_lease::Placement) -> String {
    if placement.host.starts_with("boat:") {
        format!("boat/{}", placement.vm.as_deref().unwrap_or_default())
    } else {
        format!("first_party/{}", placement.host)
    }
}

/// Whether the runner a lease places the job on fills its own remote compile
/// cache, and so should be offered default-branch tips. A registered host's
/// tenant runner does, with its own capacity; a Boat sandbox only reads.
pub(crate) fn fills_cache(placement: &motor_lease::Placement) -> bool {
    !placement.host.starts_with("boat:")
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

#[cfg(test)]
mod tests {
    use super::*;
    use motor_protocol::Accepted;
    use serde_json::{json, Value};
    use uuid::Uuid;

    fn first_party_grant(provider: Option<&str>) -> Lease {
        let mut grant = json!({
            "lease_id": "lease-1", "job_id": Uuid::nil().to_string(),
            "placement": { "host": "host-a", "vm": null },
            "endpoint": "127.0.0.1:9443", "endpoint_spki_sha256": "key",
            "job_token": "token", "token_expires_at_ms": 1000,
            "max_s": 60, "reserved_vcpu_ms": 120000
        });
        if let Some(provider) = provider {
            grant["provider"] = json!(provider);
        }
        serde_json::from_value(grant).unwrap()
    }

    #[test]
    fn first_party_grant_has_caller_host() {
        let lease = first_party_grant(Some("first_party"));
        let accepted =
            Accepted::new(Uuid::nil(), 0).with_placement(lease.caller_placement().unwrap());
        assert_eq!(
            serde_json::to_value(accepted).unwrap()["placement"],
            json!({ "provider": "first_party", "host": "host-a" })
        );
    }

    #[test]
    fn boat_grant_has_overflow_without_caller_host() {
        let vector: Value = serde_json::from_str(include_str!(
            "../../../motor-lease/contracts/cloud/boat-lease.json"
        ))
        .unwrap();
        let lease: Lease =
            serde_json::from_value(vector["booked"]["reply"]["body"].clone()).unwrap();
        let accepted =
            Accepted::new(Uuid::nil(), 0).with_placement(lease.caller_placement().unwrap());
        assert_eq!(
            serde_json::to_value(accepted).unwrap()["placement"],
            json!({ "provider": "overflow" })
        );
    }

    #[test]
    fn providerless_grant_does_not_invent_placement() {
        assert!(first_party_grant(None).caller_placement().is_none());
    }

    #[test]
    fn display_mapping_preserves_other_provider_names_and_optional_hosts() {
        for host in [Some("host-b"), None] {
            let placement = caller_placement(Some("future_provider"), host).unwrap();
            let mut expected = json!({ "provider": "future_provider" });
            if let Some(host) = host {
                expected["host"] = json!(host);
            }
            assert_eq!(serde_json::to_value(placement).unwrap(), expected);
        }
    }

    #[test]
    fn accepted_without_placement_omits_field_and_round_trips_bytes() {
        let bytes = br#"{"job_id":"00000000-0000-0000-0000-000000000000","queue_position":0}"#;
        let accepted: Accepted = serde_json::from_slice(bytes).unwrap();
        assert!(accepted.placement().is_none());
        let encoded = serde_json::to_vec(&accepted).unwrap();
        let value: Value = serde_json::from_slice(&encoded).unwrap();
        assert!(value.get("placement").is_none());
        assert_eq!(encoded, bytes);
        assert_eq!(
            serde_json::to_vec(&Accepted::new(Uuid::nil(), 0)).unwrap(),
            bytes
        );
    }
}
