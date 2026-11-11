//! Connection-independent job VMs. Callers name identities, never host paths.
use crate::{Error, Result};
use serde::{Deserialize, Serialize};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

// Starts expire before the helper's 48-hour tombstone retention ends, so a
// pruned identity can never become eligible to boot a second time.
const MAX_START_AGE: Duration = Duration::from_secs(24 * 60 * 60);
const MAX_START_FUTURE: Duration = Duration::from_secs(5 * 60);

pub const WORKSPACES: &str = "/var/lib/motor-vm/workspaces";
pub const IMAGES: &str = "/var/lib/motor-vm/job-images";
pub const RECORDS: &str = "/var/lib/motor-vm/jobs";

#[derive(Debug, Default, Clone, Copy, PartialEq, Eq, Deserialize, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum VmKind {
    #[default]
    Job,
    Fill,
    BaseBuild,
    Recovery,
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct Start {
    #[serde(default)]
    pub kind: VmKind,
    pub job_id: String,
    pub account: String,
    pub workspace_disk_id: String,
    pub image_generation_id: String,
    pub vcpus: u32,
    /// Guest RAM, already sized by the host for the job limit and boot overhead.
    pub memory_mib: u64,
    pub burst_mib: Option<u64>,
}

impl Start {
    pub fn validate(&self) -> Result<()> {
        validate_job_id(&self.job_id)?;
        let id = uuid::Uuid::parse_str(&self.job_id).unwrap();
        if id.get_version_num() != 7 || id.get_variant() != uuid::Variant::RFC4122 {
            return Err(Error("job start requires a UUIDv7".into()));
        }
        validate_account(&self.account)?;
        for id in [&self.workspace_disk_id, &self.image_generation_id] {
            validate_id(id)?;
        }
        if !(1..=32).contains(&self.vcpus) || self.memory_mib == 0 {
            return Err(Error("invalid job VM size".into()));
        }
        // Firecracker converts MiB to bytes. Reject overflow before launch.
        self.memory_mib
            .checked_mul(1048576)
            .ok_or_else(|| Error("job VM RAM overflow".into()))?;
        Ok(())
    }

    pub fn validate_fresh_at(&self, now: SystemTime) -> Result<()> {
        self.validate()?;
        validate_start_window(&self.job_id, now)
    }
}

pub(crate) fn validate_start_window(job_id: &str, now: SystemTime) -> Result<()> {
    validate_job_id(job_id)?;
    let id = uuid::Uuid::parse_str(job_id).unwrap();
    if id.get_version_num() != 7 || id.get_variant() != uuid::Variant::RFC4122 {
        return Err(Error("job start requires a UUIDv7".into()));
    }
    let millis = u64::from_be_bytes([
        0,
        0,
        id.as_bytes()[0],
        id.as_bytes()[1],
        id.as_bytes()[2],
        id.as_bytes()[3],
        id.as_bytes()[4],
        id.as_bytes()[5],
    ]);
    let created = UNIX_EPOCH + Duration::from_millis(millis);
    if now
        .duration_since(created)
        .is_ok_and(|age| age > MAX_START_AGE)
        || created
            .duration_since(now)
            .is_ok_and(|ahead| ahead > MAX_START_FUTURE)
    {
        return Err(Error("job UUIDv7 timestamp outside start window".into()));
    }
    Ok(())
}

pub fn validate_id(id: &str) -> Result<()> {
    if id.is_empty()
        || id.len() > 128
        || !id
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'_' | b'-'))
    {
        return Err(Error("invalid job resource identity".into()));
    }
    Ok(())
}

/// Account subjects remain byte-for-byte identical in RPCs, registries and the
/// credential store. Only the object-store prefix replaces ':' with '-'.
pub fn validate_account(account: &str) -> Result<()> {
    if !account.contains(':') {
        return validate_id(account);
    }
    let (kind, ulid) = account.split_once(':').unwrap();
    if !matches!(kind, "acct" | "org")
        || ulid.len() != 26
        || !ulid
            .bytes()
            .all(|b| b"0123456789ABCDEFGHJKMNPQRSTVWXYZ".contains(&b))
        || ulid.as_bytes()[0] > b'7'
    {
        return Err(Error("invalid job account subject".into()));
    }
    Ok(())
}

pub fn validate_job_id(id: &str) -> Result<()> {
    let parsed = uuid::Uuid::parse_str(id).map_err(|_| Error("invalid job id".into()))?;
    if parsed.to_string() != id {
        return Err(Error("job id must be a canonical UUID".into()));
    }
    Ok(())
}

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct Workspace {
    pub account: String,
    pub workspace_disk_id: String,
    #[serde(default)]
    pub role: WorkspaceRole,
}

/// Helper-owned provenance, stored separately from the guest-written image.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct Fill {
    pub account: String,
    pub repository_key: String,
    pub fill_id: String,
    pub size_bytes: u64,
}

impl Fill {
    pub fn validate(&self) -> Result<()> {
        validate_account(&self.account)?;
        validate_id(&self.repository_key)?;
        validate_job_id(&self.fill_id)
    }

    pub fn disk_id(&self) -> String {
        format!("fill-{}", self.fill_id)
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct Seed {
    pub account: String,
    pub repository_key: String,
    pub base_id: String,
    pub disk_id: String,
}

/// Internal sparse-disk request. The provisioning operation chooses the role;
/// ordinary workspace requests cannot select the Fill or Base role.
pub struct CreateWorkspace {
    pub account: String,
    pub workspace_disk_id: String,
    pub size_bytes: u64,
}

/// Fixed by root when provisioning the disk, never by a start request.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq, Deserialize, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum WorkspaceRole {
    #[default]
    Job,
    Fill,
    Base,
}

impl WorkspaceRole {
    pub fn permits(self, kind: VmKind) -> bool {
        matches!(
            (self, kind),
            (Self::Job, VmKind::Recovery)
                | (Self::Job, VmKind::Job)
                | (Self::Fill, VmKind::Fill)
                | (Self::Base, VmKind::BaseBuild)
        )
    }
}

/// Immutable evidence for one job VMM exit. The helper proves the cgroup empty
/// and syncs its final OOM and egress counters before removing their cgroup and
/// TAP evidence or releasing the disk. Polls
/// replay it until the host acknowledges durable receipt. Released, acknowledged
/// tombstones reject start_job replays for 48 hours after their last record
/// update. The helper accepts starts only for UUIDv7 timestamps in its 24-hour
/// start window, so removing a tombstone cannot make that identity reusable.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct ExitReport {
    pub job_id: String,
    pub oom_kill: u64,
    /// Final public-forward bytes for this VM only; absent when not measured.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub egress_bytes: Option<u64>,
}

pub enum Operation {
    PrepareFill(Fill),
    FinishFill {
        fill: Fill,
        published: bool,
    },
    SeedWorkspace(Seed),
    DeleteWorkspace {
        account: String,
        disk_id: String,
    },
    EvictBase {
        account: String,
        disk_id: String,
    },
    WorkspaceUsage {
        account: String,
        disk_id: String,
    },
    EnsureWorkspace {
        account: String,
        disk_id: String,
        size_bytes: u64,
    },
    Start(Start),
    Adopt(String),
    Stop(String),
    Relay(String, u32),
    Exits,
    AckExit(ExitReport),
    Memory(String),
    Balloon(String, u64),
}

impl Operation {
    pub fn validate(&self) -> Result<()> {
        match self {
            Self::PrepareFill(fill) | Self::FinishFill { fill, .. } => fill.validate(),
            Self::SeedWorkspace(seed) => {
                validate_account(&seed.account)?;
                validate_id(&seed.repository_key)?;
                validate_id(&seed.base_id)?;
                validate_id(&seed.disk_id)
            }
            Self::DeleteWorkspace { account, disk_id }
            | Self::EvictBase { account, disk_id }
            | Self::WorkspaceUsage { account, disk_id }
            | Self::EnsureWorkspace {
                account, disk_id, ..
            } => {
                validate_account(account)?;
                validate_id(disk_id)
            }
            Self::Start(start) => start.validate(),
            Self::Adopt(id) | Self::Stop(id) | Self::Memory(id) | Self::Balloon(id, _) => {
                validate_job_id(id)
            }
            Self::Exits => Ok(()),
            Self::AckExit(report) => validate_job_id(&report.job_id),
            Self::Relay(id, port) => {
                validate_job_id(id)?;
                crate::tenant::validate_port(*port)
            }
        }
    }
}

#[cfg(test)]
pub(crate) fn test_id(number: u64) -> String {
    static MILLIS: std::sync::OnceLock<u64> = std::sync::OnceLock::new();
    let millis = *MILLIS.get_or_init(|| {
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_millis() as u64
    });
    uuid::Uuid::from_u128(
        (u128::from(millis) << 80)
            | (0x7000u128 << 64)
            | (0x8000_0000_0000_0000u128 | u128::from(number)),
    )
    .to_string()
}

#[cfg(test)]
mod tests {
    #[test]
    fn workspace_storage_operations_accept_identities_never_paths_or_roles() {
        for op in ["workspace_usage", "evict_base", "ensure_workspace"] {
            let mut request = serde_json::json!({"op":op, "account":"acct:01ARZ3NDEKTSV4RRFFQ69G5FAV", "disk_id":"ws-1"});
            if op == "ensure_workspace" {
                request["size_bytes"] = serde_json::json!(4096);
            }
            assert!(serde_json::from_value::<crate::Request>(request.clone()).is_ok());
            for forbidden in ["path", "role", "root", "source"] {
                let mut bad = request.clone();
                bad[forbidden] = serde_json::json!("base");
                assert!(serde_json::from_value::<crate::Request>(bad).is_err());
            }
        }
        for account in ["../other", "a/b"] {
            for op in [
                super::Operation::WorkspaceUsage {
                    account: account.into(),
                    disk_id: "ws-1".into(),
                },
                super::Operation::EvictBase {
                    account: account.into(),
                    disk_id: "ws-1".into(),
                },
                super::Operation::EnsureWorkspace {
                    account: account.into(),
                    disk_id: "ws-1".into(),
                    size_bytes: 4096,
                },
            ] {
                assert!(op.validate().is_err());
            }
        }
    }
    #[test]
    fn memory_operations_accept_only_valid_job_id_and_never_host_paths_or_pids() {
        for op in ["job_memory", "balloon_job"] {
            let mut request =
                serde_json::json!({"op":op, "job_id":"00000000-0000-7000-8000-000000000001"});
            if op == "balloon_job" {
                request["amount_mib"] = serde_json::json!(1);
            }
            assert!(serde_json::from_value::<crate::Request>(request.clone()).is_ok());
            request["pid"] = serde_json::json!(1);
            assert!(serde_json::from_value::<crate::Request>(request.clone()).is_err());
            request.as_object_mut().unwrap().remove("pid");
            request["path"] = serde_json::json!("/proc/1/smaps");
            assert!(serde_json::from_value::<crate::Request>(request).is_err());
        }
        assert!(super::Operation::Memory("../escape".into())
            .validate()
            .is_err());
        assert!(super::Operation::Balloon("../escape".into(), 0)
            .validate()
            .is_err());
    }
    use super::*;

    #[test]
    fn start_uuidv7_freshness_edges_and_non_v7_refusal() {
        let now = UNIX_EPOCH + Duration::from_secs(2_000_000_000);
        let mut start = Start {
            kind: VmKind::Job,
            job_id: test_id(1),
            account: "account-1".into(),
            workspace_disk_id: "disk-1".into(),
            image_generation_id: "gen-1".into(),
            vcpus: 1,
            memory_mib: 1,
            burst_mib: None,
        };
        for (offset_ms, accepted) in [
            (-86_400_000i64, true),
            (-86_400_001, false),
            (300_000, true),
            (300_001, false),
        ] {
            let millis = (2_000_000_000_000i64 + offset_ms) as u128;
            start.job_id = uuid::Uuid::from_u128(
                (millis << 80) | (0x7000u128 << 64) | 0x8000_0000_0000_0001u128,
            )
            .to_string();
            assert_eq!(
                start.validate_fresh_at(now).is_ok(),
                accepted,
                "offset={offset_ms}"
            );
        }
        start.job_id = "00000000-0000-4000-8000-000000000001".into();
        assert!(start.validate().unwrap_err().0.contains("UUIDv7"));
    }

    #[test]
    fn workspace_role_authorizes_only_its_kind_and_legacy_is_job() {
        use WorkspaceRole::*;
        for (role, allowed) in [
            (Job, [true, false, false, true]),
            (Fill, [false, true, false, false]),
            (Base, [false, false, true, false]),
        ] {
            for (kind, expected) in [
                VmKind::Job,
                VmKind::Fill,
                VmKind::BaseBuild,
                VmKind::Recovery,
            ]
            .into_iter()
            .zip(allowed)
            {
                assert_eq!(role.permits(kind), expected, "{role:?} {kind:?}");
            }
        }
        let legacy: Workspace =
            serde_json::from_str(r#"{"account":"account-1","workspace_disk_id":"disk-1"}"#)
                .unwrap();
        assert_eq!(legacy.role, Job);
        assert!(!legacy.role.permits(VmKind::Fill));
    }

    #[test]
    fn exact_account_subjects_are_safe_path_components() {
        for account in [
            "acct:01ARZ3NDEKTSV4RRFFQ69G5FAV",
            "org:01ARZ3NDEKTSV4RRFFQ69G5FAV",
            "account-1",
        ] {
            validate_account(account).unwrap();
        }
        for account in [
            "acct:../secret",
            "acct:01ARZ3NDEKTSV4RRFFQ69G5FAV/other",
            "acct:01ARZ3NDEKTSV4RRFFQ69G5FAV:more",
            "other:01ARZ3NDEKTSV4RRFFQ69G5FAV",
            "acct:01arz3ndektsv4rrffq69g5fav",
            "acct:01ARZ3NDEKTSV4RRFFQ69G5FAI",
            "acct:81ARZ3NDEKTSV4RRFFQ69G5FAV",
            "acct:short",
            "..",
            "a/b",
        ] {
            assert!(validate_account(account).is_err(), "{account}");
        }
    }

    #[test]
    fn vm_kind_defaults_to_job_and_never_accepts_key_material_or_store_paths() {
        let value = serde_json::json!({"op":"start_job", "job_id":"00000000-0000-4000-8000-000000000001", "account":"account-1", "workspace_disk_id":"disk-1", "image_generation_id":"gen-1", "vcpus":1, "memory_mib":1024});
        let request: crate::Request = serde_json::from_value(value.clone()).unwrap();
        assert!(matches!(
            request,
            crate::Request::StartJob {
                kind: VmKind::Job,
                ..
            }
        ));
        for (kind, expected) in [
            ("job", VmKind::Job),
            ("fill", VmKind::Fill),
            ("base_build", VmKind::BaseBuild),
            ("recovery", VmKind::Recovery),
        ] {
            let mut request = value.clone();
            request["kind"] = serde_json::json!(kind);
            let request: crate::Request = serde_json::from_value(request).unwrap();
            assert!(matches!(request, crate::Request::StartJob { kind, .. } if kind == expected));
        }
        for field in [
            "read_key",
            "write_key",
            "config",
            "store_path",
            "drive_path",
        ] {
            let mut request = value.clone();
            request[field] = serde_json::json!("not-accepted");
            assert!(serde_json::from_value::<crate::Request>(request).is_err());
        }
    }
    #[test]
    fn resource_identities_cannot_select_paths() {
        for id in ["", ".", "..", "a/b", "/a", "a\\b", "a\n", "a.json"] {
            assert!(validate_id(id).is_err(), "{id:?}");
        }
        validate_id("account-1_workspace").unwrap();
        assert!(validate_job_id("00000000000000000000000000000000").is_err());
    }
}
