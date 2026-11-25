//! A bounded admission hold that does not change operator drain state.
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::{
    collections::BTreeSet,
    fs::{File, OpenOptions},
    io::{self, Read, Write},
    os::unix::fs::{OpenOptionsExt, PermissionsExt},
    path::PathBuf,
};
use time::{format_description::well_known::Rfc3339, OffsetDateTime};

#[cfg(test)]
#[path = "hold_tests.rs"]
mod tests;

// Limit how long one hold can block fleet macOS jobs. A longer run writes a new hold.
const ALWAYS_HOLD_MAX: u64 = 3 * 60 * 60 * 1_000;

// The ck-motor client retries refused fleet jobs about every 9 seconds, so a
// waiting job can be admitted well before holds resume.
const FLEET_TURN: u64 = 60 * 1_000;

#[derive(Clone, Copy, Debug, Deserialize)]
#[serde(try_from = "String")]
pub(crate) enum Window {
    Always,
    Daily { start: u64, end: u64 },
}

impl TryFrom<String> for Window {
    type Error = String;

    fn try_from(value: String) -> Result<Self, Self::Error> {
        fn minute(value: &str) -> Option<u64> {
            let bytes = value.as_bytes();
            if bytes.len() != 5
                || bytes[2] != b':'
                || ![bytes[0], bytes[1], bytes[3], bytes[4]]
                    .iter()
                    .all(u8::is_ascii_digit)
            {
                return None;
            }
            let hour: u64 = value[..2].parse().ok()?;
            let minute: u64 = value[3..].parse().ok()?;
            (hour < 24 && minute < 60).then_some((hour * 60 + minute) * 60_000)
        }
        if value == "always" {
            return Ok(Self::Always);
        }
        value
            .split_once('-')
            .and_then(|(start, end)| {
                let start = minute(start)?;
                let end = minute(end)?;
                (start != end).then_some(Self::Daily { start, end })
            })
            .ok_or_else(|| {
                "hold_window must be always or UTC HH:MM-HH:MM with distinct endpoints".into()
            })
    }
}

impl Window {
    fn expiry_limit(self, now: u64) -> Option<u64> {
        let (start, end) = match self {
            Self::Always => return Some(now.saturating_add(ALWAYS_HOLD_MAX)),
            Self::Daily { start, end } => (start, end),
        };
        const DAY: u64 = 86_400_000;
        let day = now / DAY * DAY;
        let clock = now % DAY;
        if start < end {
            (clock >= start && clock < end).then_some(day + end)
        } else if clock >= start {
            Some(day + DAY + end)
        } else if clock < end {
            Some(day + end)
        } else {
            None
        }
    }
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
struct Hold {
    owner: String,
    expires_at: String,
}

impl Hold {
    fn expiry(&self) -> io::Result<OffsetDateTime> {
        if self.owner.is_empty()
            || self.owner.len() > 64
            || !self
                .owner
                .bytes()
                .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-')
        {
            return Err(super::invalid("invalid hold owner"));
        }
        if !(self.expires_at.ends_with('Z') || self.expires_at.ends_with("+00:00")) {
            return Err(super::invalid(
                "hold expiry must be RFC3339 UTC (Z or +00:00)",
            ));
        }
        OffsetDateTime::parse(&self.expires_at, &Rfc3339).map_err(super::invalid)
    }
}

pub(crate) struct State {
    window: Option<Window>,
    hold_dir: PathBuf,
    grant_dir: PathBuf,
    granted: Option<Hold>,
    bad: BTreeSet<[u8; 32]>,
    latest: Snapshot,
    refused_owner: Option<String>,
    fleet_turn_until: u64,
}

#[derive(Clone, Default)]
pub(crate) struct Snapshot {
    revision: u64,
    hold: Option<Hold>,
}

impl State {
    pub(crate) fn new(window: Option<Window>, paths: Option<(PathBuf, PathBuf)>) -> Self {
        let (hold_dir, grant_dir) = paths.unwrap_or_else(|| {
            (
                "/Users/Shared/motor-macos-hold".into(),
                "/Users/Shared/motor-macos-grant".into(),
            )
        });
        Self {
            window,
            hold_dir,
            grant_dir,
            granted: None,
            bad: BTreeSet::new(),
            latest: Snapshot::default(),
            refused_owner: None,
            fleet_turn_until: 0,
        }
    }

    fn warn_once(&mut self, key: &[u8], error: impl std::fmt::Display) {
        if self.bad.insert(Sha256::digest(key).into()) {
            tracing::warn!(%error, "ignoring invalid macOS hold file");
        }
    }

    fn read(&mut self) -> Option<Hold> {
        let file = match OpenOptions::new()
            .read(true)
            .custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK | libc::O_CLOEXEC)
            .open(self.hold_dir.join("hold.json"))
        {
            Ok(file) => file,
            Err(error) if error.kind() == io::ErrorKind::NotFound => return None,
            Err(error) => {
                self.warn_once(format!("open: {:?}", error.kind()).as_bytes(), &error);
                return None;
            }
        };
        let mut bytes = Vec::new();
        let result = (|| -> io::Result<Hold> {
            let metadata = file.metadata()?;
            if !metadata.is_file() {
                bytes.extend_from_slice(
                    format!("nonregular: {:?}", metadata.file_type()).as_bytes(),
                );
                return Err(super::invalid(
                    "hold must be a regular file of at most 4 KiB",
                ));
            }
            file.take(4097).read_to_end(&mut bytes)?;
            if metadata.len() > 4096 || bytes.len() > 4096 {
                bytes.extend_from_slice(&metadata.len().to_le_bytes());
                return Err(super::invalid("hold exceeds 4 KiB"));
            }
            let hold: Hold = serde_json::from_slice(&bytes).map_err(super::invalid)?;
            hold.expiry()?;
            Ok(hold)
        })();
        match result {
            Ok(hold) => Some(hold),
            Err(error) => {
                self.warn_once(&bytes, error);
                None
            }
        }
    }

    pub(crate) fn observe(&mut self, now: u64) -> Snapshot {
        let hold = if self
            .window
            .and_then(|window| window.expiry_limit(now))
            .is_some()
        {
            self.read()
        } else {
            None
        };
        self.latest = Snapshot {
            revision: self.latest.revision + 1,
            hold,
        };
        // Remember the owner that blocked admission, not the expiry: the
        // writer can refresh the same hold or replace it without an observed gap.
        if self.refused_owner.as_ref().is_some_and(|owner| {
            self.honoured(now, &self.latest)
                .is_none_or(|hold| &hold.owner != owner)
        }) {
            self.refused_owner = None;
            self.fleet_turn_until = now.saturating_add(FLEET_TURN);
            self.remove_grant();
        }
        self.latest.clone()
    }

    fn honoured(&self, now: u64, snapshot: &Snapshot) -> Option<Hold> {
        if now < self.fleet_turn_until {
            return None;
        }
        let limit = self.window?.expiry_limit(now)?;
        let hold = snapshot.hold.as_ref()?;
        let expiry = hold.expiry().ok()?.unix_timestamp_nanos();
        (expiry > i128::from(now) * 1_000_000 && expiry <= i128::from(limit) * 1_000_000)
            .then_some(hold.clone())
    }

    fn remove_grant(&mut self) {
        self.granted = None;
        if let Err(error) = std::fs::remove_file(self.grant_dir.join("granted.json")) {
            if error.kind() != io::ErrorKind::NotFound {
                tracing::warn!(%error, "cannot remove macOS hold grant");
            }
        }
    }

    fn write_grant(&self, hold: &Hold) -> io::Result<()> {
        let temporary = self
            .grant_dir
            .join(format!(".granted-{}.tmp", uuid::Uuid::now_v7()));
        let result = (|| {
            let mut file = OpenOptions::new()
                .write(true)
                .create_new(true)
                .mode(0o644)
                .custom_flags(libc::O_NOFOLLOW | libc::O_CLOEXEC)
                .open(&temporary)?;
            file.set_permissions(std::fs::Permissions::from_mode(0o644))?;
            file.write_all(&serde_json::to_vec(hold).map_err(super::invalid)?)?;
            file.sync_all()?;
            std::fs::rename(&temporary, self.grant_dir.join("granted.json"))?;
            File::open(&self.grant_dir)?.sync_all()
        })();
        let _ = std::fs::remove_file(temporary);
        result
    }

    /// The engine holds its state lock across the idle check and grant rename.
    pub(crate) fn apply(&mut self, now: u64, idle: bool, snapshot: Snapshot) -> bool {
        // A newer observation may have arrived while this caller waited for the
        // engine lock. Never overwrite it with an older admission snapshot.
        let snapshot = if snapshot.revision < self.latest.revision {
            &self.latest
        } else {
            &snapshot
        };
        let hold = self.honoured(now, snapshot);
        match hold.as_ref() {
            Some(hold) if idle => {
                if self.granted.as_ref() != Some(hold) {
                    self.remove_grant();
                    match self.write_grant(hold) {
                        Ok(()) => self.granted = Some(hold.clone()),
                        Err(error) => tracing::warn!(%error, "cannot publish macOS hold grant"),
                    }
                }
            }
            _ => self.remove_grant(),
        }
        hold.is_some()
    }

    pub(crate) fn status(&mut self, now: u64) -> Option<String> {
        self.admission_refusal(now, false, || false)
    }

    pub(crate) fn admission_refusal(
        &mut self,
        now: u64,
        record_refusal: bool,
        allow_pending: impl FnOnce() -> bool,
    ) -> Option<String> {
        let hold = self.honoured(now, &self.latest);
        if hold.as_ref() != self.granted.as_ref() {
            self.remove_grant();
        }
        let hold = hold?;
        let granted = self.granted.as_ref() == Some(&hold);
        if !granted && allow_pending() {
            return None;
        }
        if record_refusal {
            self.refused_owner = Some(hold.owner.clone());
        }
        Some(format!(
            "held for {} until {} (granted: {granted})",
            hold.owner, hold.expires_at,
        ))
    }

    #[cfg(test)]
    fn refresh(&mut self, now: u64, idle: bool) -> bool {
        let snapshot = self.observe(now);
        self.apply(now, idle, snapshot)
    }
}
