//! The broker contract's grammar for the bodies a host signs. CLOUD answers a
//! body that breaks it with 400 `bad_request`, and the same body is refused
//! again on every retry, so the host checks before signing and never sends
//! one.

use motor_lease::{Heartbeat, Usage};

/// The largest integer a JSON number carries exactly (2^53 - 1).
const SAFE_INTEGER: u64 = (1 << 53) - 1;
/// The contract bounds `warm` and `running` at 64 entries each.
pub(crate) const MAX_ENTRIES: usize = 64;

fn base64url_hash(value: &str) -> bool {
    value.len() == 43
        && value
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'_' || b == b'-')
}

fn name(value: &str) -> bool {
    (1..=64).contains(&value.len())
        && value
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'.' | b'_' | b'-'))
}

/// `acct:` followed by a ULID in Crockford base32 (no I, L, O or U).
fn account(value: &str) -> bool {
    let Some(ulid) = value.strip_prefix("acct:") else {
        return false;
    };
    ulid.len() == 26
        && matches!(ulid.as_bytes()[0], b'0'..=b'7')
        && ulid
            .bytes()
            .all(|b| (b.is_ascii_digit() || b.is_ascii_uppercase()) && !b"ILOU".contains(&b))
}

/// A lowercase version 7 UUID, as the contract writes job ids.
fn job_id(value: &str) -> bool {
    let bytes = value.as_bytes();
    bytes.len() == 36
        && bytes.iter().enumerate().all(|(i, b)| match i {
            8 | 13 | 18 | 23 => *b == b'-',
            _ => b.is_ascii_digit() || (b'a'..=b'f').contains(b),
        })
        && bytes[14] == b'7'
        && matches!(bytes[19], b'8' | b'9' | b'a' | b'b')
}

/// The first rule this heartbeat breaks, or None when the broker accepts it.
pub fn heartbeat_violation(heartbeat: &Heartbeat) -> Option<&'static str> {
    if [
        heartbeat.free_vcpus,
        heartbeat.free_mem_mib,
        heartbeat.queued,
    ]
    .into_iter()
    .any(|value| value > SAFE_INTEGER)
    {
        return Some("integer above the JSON safe range");
    }
    if heartbeat.warm.len() > MAX_ENTRIES || heartbeat.running.len() > MAX_ENTRIES {
        return Some("more than 64 warm or running entries");
    }
    for warm in &heartbeat.warm {
        if !account(&warm.tenant) {
            return Some("warm tenant is not acct:<ULID>");
        }
        if !base64url_hash(&warm.workspace_key_hash) {
            return Some("warm workspace_key_hash is not 43 base64url characters");
        }
        if !name(&warm.vm) {
            return Some("warm vm is not a 1-64 character name");
        }
    }
    for job in &heartbeat.running {
        if !job_id(&job.job_id) {
            return Some("running job_id is not a lowercase UUIDv7");
        }
        if job.elapsed_ms > SAFE_INTEGER {
            return Some("running elapsed_ms above the JSON safe range");
        }
    }
    None
}

/// The first rule this usage report breaks, or None when the broker accepts it.
pub fn usage_violation(usage: &Usage) -> Option<&'static str> {
    if !job_id(&usage.job_id) {
        return Some("job_id is not a lowercase UUIDv7");
    }
    if !(1..=64).contains(&usage.vcpus) {
        return Some("vcpus outside 1-64");
    }
    if [usage.started_at_ms, usage.ended_at_ms]
        .into_iter()
        .chain(usage.mem_peak_mib)
        .chain(usage.egress_bytes)
        .any(|value| value > SAFE_INTEGER)
    {
        return Some("integer above the JSON safe range");
    }
    if usage.ended_at_ms < usage.started_at_ms {
        return Some("ended_at_ms before started_at_ms");
    }
    None
}

/// Checks a queued outbox body against the grammar of the route it is sent
/// to. A body that does not decode as that route's type is a violation too.
pub fn request_violation(path: &str, body: &[u8]) -> Option<&'static str> {
    if path.ends_with("/heartbeat") {
        match serde_json::from_slice::<Heartbeat>(body) {
            Ok(heartbeat) => heartbeat_violation(&heartbeat),
            Err(_) => Some("heartbeat body does not decode"),
        }
    } else if path.ends_with("/usage") {
        match serde_json::from_slice::<Usage>(body) {
            Ok(usage) => usage_violation(&usage),
            Err(_) => Some("usage body does not decode"),
        }
    } else {
        Some("unknown host route")
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use motor_lease::{JobState, RunningJob, UsageOutcome, WarmVm};

    fn heartbeat() -> Heartbeat {
        Heartbeat {
            free_vcpus: 8,
            free_mem_mib: 87846,
            queued: 0,
            warm: vec![WarmVm {
                tenant: "acct:01KXJJHK4V6DB42W1YAN2XD9QN".into(),
                workspace_key_hash: "A".repeat(43),
                vm: "vm-1".into(),
            }],
            running: vec![RunningJob {
                job_id: "01a157b7-aab9-77d0-a79e-29ae3760e3e8".into(),
                state: JobState::Admitted,
                elapsed_ms: 0,
            }],
            healthy: true,
            draining: false,
        }
    }

    #[test]
    fn a_local_tenant_label_and_empty_hash_are_refused_and_a_contract_heartbeat_passes() {
        assert_eq!(heartbeat_violation(&heartbeat()), None);
        // The body CLOUD answered with 400: a host-local tenant label and an
        // empty workspace hash.
        let sent: Heartbeat = serde_json::from_str(r#"{"free_vcpus":8,"free_mem_mib":87846,"queued":0,"warm":[{"tenant":"tenant-1","workspace_key_hash":"","vm":"vm-1"}],"running":[],"healthy":true}"#).unwrap();
        assert_eq!(
            heartbeat_violation(&sent),
            Some("warm tenant is not acct:<ULID>")
        );
        let mut empty_hash = heartbeat();
        empty_hash.warm[0].workspace_key_hash.clear();
        assert_eq!(
            heartbeat_violation(&empty_hash),
            Some("warm workspace_key_hash is not 43 base64url characters")
        );
        for (tenant, valid) in [
            ("acct:01KXJJHK4V6DB42W1YAN2XD9QN", true),
            ("acct:81KXJJHK4V6DB42W1YAN2XD9QN", false),
            ("acct:01KXJJHK4V6DB42W1YAN2XD9QL", false),
            ("acct:01kxjjhk4v6db42w1yan2xd9qn", false),
            ("01KXJJHK4V6DB42W1YAN2XD9QN", false),
        ] {
            let mut body = heartbeat();
            body.warm[0].tenant = tenant.into();
            assert_eq!(heartbeat_violation(&body).is_none(), valid, "{tenant}");
        }
        let mut bad_vm = heartbeat();
        bad_vm.warm[0].vm = "vm/1".into();
        assert!(heartbeat_violation(&bad_vm).is_some());
        let mut bad_job = heartbeat();
        bad_job.running[0].job_id = "01A157B7-AAB9-77D0-A79E-29AE3760E3E8".into();
        assert!(heartbeat_violation(&bad_job).is_some());
        let mut unsafe_integer = heartbeat();
        unsafe_integer.free_mem_mib = SAFE_INTEGER + 1;
        assert!(heartbeat_violation(&unsafe_integer).is_some());
        let mut many = heartbeat();
        many.running = vec![many.running[0].clone(); 65];
        assert!(heartbeat_violation(&many).is_some());
        let mut empty = heartbeat();
        empty.warm.clear();
        empty.running.clear();
        assert_eq!(heartbeat_violation(&empty), None);
    }

    #[test]
    fn usage_grammar_matches_the_contract() {
        let usage = Usage {
            job_id: "01a157b7-aab9-77d0-a79e-29ae3760e3e8".into(),
            vcpus: 2,
            started_at_ms: 1792473378188,
            ended_at_ms: 1792473378188,
            mem_peak_mib: Some(481),
            egress_bytes: None,
            outcome: UsageOutcome::Cancelled,
        };
        assert_eq!(usage_violation(&usage), None);
        let body = serde_json::to_vec(&usage).unwrap();
        assert_eq!(request_violation("/v1/hosts/ovh-1/usage", &body), None);
        assert!(request_violation("/v1/hosts/ovh-1/heartbeat", &body).is_some());
        for broken in [
            Usage {
                vcpus: 0,
                ..usage.clone()
            },
            Usage {
                ended_at_ms: usage.started_at_ms - 1,
                ..usage.clone()
            },
            Usage {
                job_id: "not-a-job".into(),
                ..usage.clone()
            },
        ] {
            assert!(usage_violation(&broken).is_some(), "{broken:?}");
        }
    }
}
