//! The host's durable state in SQLite, through the fleet's shared
//! `cortexkit-store` layer (single-writer lease, versioned migrations and the
//! removals journal).
//!
//! The host keeps its whole state in memory as a [`Snapshot`] and hands every
//! change to [`Journal::store`]. This journal writes only the entries that
//! differ from the last stored snapshot, in one transaction, so a change costs
//! a few rows however many jobs the host has run. SQLite commits each
//! transaction durably before returning (write-ahead log, full sync), which
//! keeps the old journal's promise: a change is on disk before the host acts
//! on it.
//!
//! Finished jobs leave through the store's removals journal. Their removal is
//! recorded beside the database before the rows are deleted, and every open
//! replays it, so an older copy of the database can't bring a deleted job's
//! lease (and with it a replayable admission) back.
//!
//! The first open on a host that still has the whole-file `journal.json`
//! imports it and renames it aside. `export-journal` writes that file back from
//! the database, so an older host binary can be restored.
use crate::journal::{FileJournal, Journal};
use crate::state::Snapshot;
use cortexkit_store::{
    open_sqlite_with_removals, Isolation, Migration, RemovalRecord, RemovalsHandler,
    SqliteOpenOptions, SqliteStore, StorageBackend, StorageDescriptor,
};
use rusqlite::{params, Connection, OptionalExtension, Transaction};
use serde::{de::DeserializeOwned, Serialize};
use std::collections::{BTreeMap, BTreeSet};
use std::fs::{self, File};
use std::io;
use std::path::{Path, PathBuf};

/// Every map in [`Snapshot`] is stored as rows of one `kind`, keyed as in the
/// map. The job kinds are the ones a finished job's removal deletes.
const LEASE: &str = "lease";
const BILLING: &str = "billing";
const ZERO_USAGE: &str = "zero_usage";
const PENDING_CANCEL: &str = "pending_cancel";
const NONCE: &str = "nonce";
const VM: &str = "vm";
const CLOSED: &str = "closed";
const REVOCATION: &str = "revocation";
const JOB_KINDS: [&str; 4] = [LEASE, BILLING, ZERO_USAGE, PENDING_CANCEL];

/// Set once `journal.json` has been imported (or there was none), so a later
/// open never imports again. `export-journal` clears it.
const IMPORTED: &str = "imported_journal";

const MIGRATIONS: &[Migration] = &[Migration {
    version: 1,
    statements: "
        CREATE TABLE host_entries (
            kind TEXT NOT NULL,
            key TEXT NOT NULL,
            value TEXT NOT NULL,
            PRIMARY KEY (kind, key)
        ) WITHOUT ROWID;
        CREATE TABLE host_values (
            name TEXT PRIMARY KEY NOT NULL,
            value TEXT NOT NULL
        ) WITHOUT ROWID;
    ",
}];

pub struct SqliteJournal {
    store: SqliteStore,
    directory: PathBuf,
    /// What the database holds: the last snapshot whose transaction committed.
    stored: Snapshot,
}

/// Replays a finished job's removal onto a database that still holds it.
struct JobRemovals;

impl RemovalsHandler for JobRemovals {
    fn replay(&self, tx: &Transaction<'_>, record: &RemovalRecord) -> rusqlite::Result<()> {
        erase_jobs(tx, &record.ids)
    }
}

fn erase_jobs(tx: &Transaction<'_>, jobs: &[String]) -> rusqlite::Result<()> {
    for job in jobs {
        for kind in JOB_KINDS {
            tx.execute(
                "DELETE FROM host_entries WHERE kind = ?1 AND key = ?2",
                params![kind, job],
            )?;
        }
    }
    Ok(())
}

fn other(error: impl std::fmt::Display) -> io::Error {
    io::Error::other(error.to_string())
}

impl SqliteJournal {
    /// Opens `<directory>/store.db`, importing `<directory>/journal.json` the
    /// first time, and returns the journal with the state it holds.
    pub fn open(directory: &Path) -> io::Result<(Self, Snapshot)> {
        fs::create_dir_all(directory)?;
        let path = directory.join("store.db");
        let descriptor = StorageDescriptor {
            module_id: "motor-host".into(),
            storage_namespace: "state".into(),
            isolation: Isolation::Module,
            backend: StorageBackend::Sqlite {
                path: path
                    .to_str()
                    .ok_or_else(|| other("the journal directory is not valid UTF-8"))?
                    .into(),
            },
        };
        let store =
            open_sqlite_with_removals(&descriptor, SqliteOpenOptions::default(), &JobRemovals)
                .map_err(other)?;
        let outcome = store.migrate("motor_host", MIGRATIONS).map_err(other)?;
        if outcome.store_ahead() {
            return Err(other(format!(
                "{} holds schema version {}, newer than this motor-host's {}; run the newer binary or its export-journal",
                path.display(),
                outcome.recorded,
                outcome.chain_max
            )));
        }
        let legacy = directory.join("journal.json");
        let imported = store
            .with_conn(|c| {
                c.query_row(
                    "SELECT 1 FROM host_values WHERE name = ?1",
                    [IMPORTED],
                    |_| Ok(()),
                )
                .optional()
            })
            .map_err(other)?
            .is_some();
        let snapshot = if imported {
            store.with_conn(load_snapshot).map_err(other)?
        } else {
            let snapshot = match fs::read(&legacy) {
                Ok(bytes) => serde_json::from_slice(&bytes).map_err(other)?,
                Err(e) if e.kind() == io::ErrorKind::NotFound => Snapshot::default(),
                Err(e) => return Err(e),
            };
            // The file is the authority until this commits, so whatever an
            // earlier binary left in the tables is replaced, not merged.
            store
                .with_conn_fenced(|tx| {
                    tx.execute("DELETE FROM host_entries", [])?;
                    tx.execute("DELETE FROM host_values", [])?;
                    tx.execute_batch("DROP TABLE IF EXISTS fill_source_entries;")?;
                    write_diff(tx, &Snapshot::default(), &snapshot)?;
                    put_value(tx, IMPORTED, &true)
                })
                .map_err(other)?;
            snapshot
        };
        // Retired only after the import committed. Kept, renamed, as the
        // record of what was imported; a newer import would overwrite it.
        if legacy.exists() {
            fs::rename(&legacy, directory.join("journal.json.imported"))?;
            File::open(directory)?.sync_all()?;
        }
        crate::jobs::fill_source::remove_unreferenced(directory, &snapshot.per_job.fill_sources)?;
        Ok((
            Self {
                store,
                directory: directory.to_path_buf(),
                stored: snapshot.clone(),
            },
            snapshot,
        ))
    }

    /// Writes the stored state to `<directory>/journal.json` in the format the
    /// whole-file journal used, so an older motor-host can be started on it,
    /// and clears the import marker, so this binary imports that file again
    /// (with whatever the older one changed) on its next start. Run only while
    /// motor-host is stopped: the store's lease refuses a second writer.
    pub fn export(directory: &Path) -> io::Result<PathBuf> {
        let (journal, snapshot) = Self::open(directory)?;
        let mut file = FileJournal::for_export(directory)?;
        file.store(&snapshot)?;
        journal
            .store
            .with_conn_fenced(|tx| {
                tx.execute("DELETE FROM host_values WHERE name = ?1", [IMPORTED])
                    .map(drop)
            })
            .map_err(other)?;
        Ok(directory.join("journal.json"))
    }
}

impl Journal for SqliteJournal {
    fn directory(&self) -> Option<PathBuf> {
        Some(self.directory.clone())
    }
    fn store(&mut self, next: &Snapshot) -> io::Result<()> {
        let old = &self.stored;
        // Only the retention pass deletes a job's lease, billing or no-start
        // record, so a deleted one is a finished job leaving the host: it goes
        // through the removals journal, in the same transaction as the rest of
        // this change.
        let removed: BTreeSet<&String> = old
            .leases
            .keys()
            .chain(old.billing.keys())
            .chain(old.zero_usage.keys())
            .filter(|job| {
                !next.leases.contains_key(*job)
                    && !next.billing.contains_key(*job)
                    && !next.zero_usage.contains_key(*job)
            })
            .collect();
        if removed.is_empty() {
            self.store
                .with_conn_fenced(|tx| write_diff(tx, old, next))
                .map_err(other)?;
        } else {
            self.store
                .remove("job", removed.into_iter().cloned(), |tx, _| {
                    write_diff(tx, old, next)
                })
                .map_err(other)?;
        }
        self.stored = next.clone();
        Ok(())
    }
}

fn to_json<T: Serialize>(value: &T) -> rusqlite::Result<String> {
    serde_json::to_string(value).map_err(|e| rusqlite::Error::ToSqlConversionFailure(Box::new(e)))
}

fn from_json<T: DeserializeOwned>(value: &str) -> rusqlite::Result<T> {
    serde_json::from_str(value).map_err(|e| {
        rusqlite::Error::FromSqlConversionFailure(0, rusqlite::types::Type::Text, Box::new(e))
    })
}

fn put_value<T: Serialize>(tx: &Transaction<'_>, name: &str, value: &T) -> rusqlite::Result<()> {
    tx.execute(
        "INSERT INTO host_values (name, value) VALUES (?1, ?2)
         ON CONFLICT (name) DO UPDATE SET value = excluded.value",
        params![name, to_json(value)?],
    )
    .map(drop)
}

fn diff_entries<V: Serialize + PartialEq>(
    tx: &Transaction<'_>,
    kind: &str,
    old: &BTreeMap<String, V>,
    new: &BTreeMap<String, V>,
) -> rusqlite::Result<()> {
    for (key, value) in new {
        if old.get(key) != Some(value) {
            tx.execute(
                "INSERT INTO host_entries (kind, key, value) VALUES (?1, ?2, ?3)
                 ON CONFLICT (kind, key) DO UPDATE SET value = excluded.value",
                params![kind, key, to_json(value)?],
            )?;
        }
    }
    for key in old.keys().filter(|key| !new.contains_key(*key)) {
        tx.execute(
            "DELETE FROM host_entries WHERE kind = ?1 AND key = ?2",
            params![kind, key],
        )?;
    }
    Ok(())
}

fn diff_value<T: Serialize + PartialEq>(
    tx: &Transaction<'_>,
    name: &str,
    old: &T,
    new: &T,
) -> rusqlite::Result<()> {
    if old != new {
        put_value(tx, name, new)?;
    }
    Ok(())
}

/// Writes every entry of `new` that differs from `old`, and deletes every
/// entry `old` has and `new` doesn't. Adding a field to [`Snapshot`] means
/// adding it here and in [`load`]; the round-trip test fails otherwise.
fn write_diff(tx: &Transaction<'_>, old: &Snapshot, new: &Snapshot) -> rusqlite::Result<()> {
    let Snapshot {
        remote_cache_write_blocked,
        revocations,
        leases,
        nonces,
        vms,
        zero_usage,
        pending_cancels,
        billing,
        outbox,
        expired_outbox,
        rejected_outbox,
        counter,
        last_alive,
        closed,
        draining,
        accept_until_ms,
        per_job,
    } = new;
    diff_entries(tx, REVOCATION, &old.revocations, revocations)?;
    diff_entries(tx, LEASE, &old.leases, leases)?;
    diff_entries(tx, NONCE, &old.nonces, nonces)?;
    diff_entries(tx, VM, &old.vms, vms)?;
    diff_entries(tx, ZERO_USAGE, &old.zero_usage, zero_usage)?;
    diff_entries(tx, PENDING_CANCEL, &old.pending_cancels, pending_cancels)?;
    diff_entries(tx, BILLING, &old.billing, billing)?;
    diff_entries(tx, CLOSED, &old.closed, closed)?;
    diff_value(tx, "outbox", &old.outbox, outbox)?;
    diff_value(tx, "expired_outbox", &old.expired_outbox, expired_outbox)?;
    diff_value(tx, "rejected_outbox", &old.rejected_outbox, rejected_outbox)?;
    diff_value(tx, "counter", &old.counter, counter)?;
    diff_value(tx, "last_alive", &old.last_alive, last_alive)?;
    diff_value(tx, "draining", &old.draining, draining)?;
    if old.remote_cache_write_blocked != *remote_cache_write_blocked {
        if *remote_cache_write_blocked {
            diff_value(tx, "remote_cache_write_blocked", &false, &true)?;
        } else {
            tx.execute(
                "DELETE FROM host_values WHERE name = 'remote_cache_write_blocked'",
                [],
            )?;
        }
    }
    diff_value(tx, "accept_until_ms", &old.accept_until_ms, accept_until_ms)?;
    write_job_ledger(tx, &old.per_job, per_job)
}

// Keep job-VM state out of host_entries: older binaries reject unknown kinds.
// This additive table deliberately does not advance the schema version, so a
// tenant-mode rollback can still open and update its own unchanged records.
fn write_job_ledger(
    tx: &Transaction<'_>,
    old: &crate::state::jobs::Ledger,
    new: &crate::state::jobs::Ledger,
) -> rusqlite::Result<()> {
    if old == new {
        return Ok(());
    }
    tx.execute_batch(
        "CREATE TABLE IF NOT EXISTS job_vm_entries (
        kind TEXT NOT NULL, key TEXT NOT NULL, value TEXT NOT NULL,
        PRIMARY KEY (kind, key)
    ) WITHOUT ROWID;",
    )?;
    if old.tenant_rollback_pending != new.tenant_rollback_pending {
        if new.tenant_rollback_pending {
            tx.execute(
                "INSERT INTO job_vm_entries (kind, key, value) VALUES ('tenant_rollback_pending', '', 'true')
                 ON CONFLICT (kind, key) DO UPDATE SET value = excluded.value",
                [],
            )?;
        } else {
            tx.execute(
                "DELETE FROM job_vm_entries WHERE kind = 'tenant_rollback_pending' AND key = ''",
                [],
            )?;
        }
    }
    diff_job_entries(tx, "disk", &old.disks, &new.disks)?;
    diff_job_entries(tx, "base", &old.bases, &new.bases)?;
    diff_job_entries(tx, "image", &old.images, &new.images)?;
    diff_job_entries(tx, "fill", &old.fills, &new.fills)?;
    diff_job_entries(tx, "execution", &old.executions, &new.executions)
        .and_then(|()| write_fill_sources(tx, old, new))
}

fn write_fill_sources(
    tx: &Transaction<'_>,
    old: &crate::state::jobs::Ledger,
    new: &crate::state::jobs::Ledger,
) -> rusqlite::Result<()> {
    if old.fill_sources == new.fill_sources && old.export_tips == new.export_tips {
        return Ok(());
    }
    // Older hosts query only their known tables. Keep optional source metadata
    // separate so a rollback can still load and update its job/lease records.
    tx.execute_batch("CREATE TABLE IF NOT EXISTS fill_source_entries (kind TEXT NOT NULL, key TEXT NOT NULL, value TEXT NOT NULL, PRIMARY KEY(kind,key)) WITHOUT ROWID;")?;
    diff_source_entries(tx, "slot", &old.fill_sources, &new.fill_sources)?;
    diff_source_entries(tx, "export", &old.export_tips, &new.export_tips)
}

fn diff_source_entries<V: Serialize + PartialEq>(
    tx: &Transaction<'_>,
    kind: &str,
    old: &BTreeMap<String, V>,
    new: &BTreeMap<String, V>,
) -> rusqlite::Result<()> {
    for (key, value) in new {
        if old.get(key) != Some(value) {
            tx.execute("INSERT INTO fill_source_entries(kind,key,value) VALUES (?1,?2,?3) ON CONFLICT(kind,key) DO UPDATE SET value=excluded.value", params![kind,key,to_json(value)?])?;
        }
    }
    for key in old.keys().filter(|key| !new.contains_key(*key)) {
        tx.execute(
            "DELETE FROM fill_source_entries WHERE kind=?1 AND key=?2",
            params![kind, key],
        )?;
    }
    Ok(())
}

fn diff_job_entries<V: Serialize + PartialEq>(
    tx: &Transaction<'_>,
    kind: &str,
    old: &BTreeMap<String, V>,
    new: &BTreeMap<String, V>,
) -> rusqlite::Result<()> {
    for (key, value) in new {
        if old.get(key) != Some(value) {
            tx.execute(
                "INSERT INTO job_vm_entries (kind, key, value) VALUES (?1, ?2, ?3)
                 ON CONFLICT (kind, key) DO UPDATE SET value = excluded.value",
                params![kind, key, to_json(value)?],
            )?;
        }
    }
    for key in old.keys().filter(|key| !new.contains_key(*key)) {
        tx.execute(
            "DELETE FROM job_vm_entries WHERE kind = ?1 AND key = ?2",
            params![kind, key],
        )?;
    }
    Ok(())
}

fn load_snapshot(conn: &Connection) -> rusqlite::Result<Snapshot> {
    let mut snapshot = load_job_snapshot(conn)?;
    let exists: bool = conn.query_row("SELECT EXISTS (SELECT 1 FROM sqlite_master WHERE type='table' AND name='fill_source_entries')", [], |row| row.get(0))?;
    if exists {
        let mut entries = conn.prepare("SELECT kind,key,value FROM fill_source_entries")?;
        let mut rows = entries.query([])?;
        while let Some(row) = rows.next()? {
            let (kind, key, value): (String, String, String) =
                (row.get(0)?, row.get(1)?, row.get(2)?);
            match kind.as_str() {
                "slot" => drop(
                    snapshot
                        .per_job
                        .fill_sources
                        .insert(key, from_json(&value)?),
                ),
                "export" => drop(snapshot.per_job.export_tips.insert(key, from_json(&value)?)),
                _ => return Err(unknown("fill source entry kind", &kind)),
            }
        }
    }
    Ok(snapshot)
}

fn load_job_snapshot(conn: &Connection) -> rusqlite::Result<Snapshot> {
    let mut snapshot = load(conn)?;
    let exists: bool = conn.query_row(
        "SELECT EXISTS (SELECT 1 FROM sqlite_master WHERE type = 'table' AND name = 'job_vm_entries')",
        [], |row| row.get(0),
    )?;
    if exists {
        let mut entries = conn.prepare("SELECT kind, key, value FROM job_vm_entries")?;
        let mut rows = entries.query([])?;
        while let Some(row) = rows.next()? {
            let (kind, key, value): (String, String, String) =
                (row.get(0)?, row.get(1)?, row.get(2)?);
            match kind.as_str() {
                "tenant_rollback_pending" if key.is_empty() => {
                    snapshot.per_job.tenant_rollback_pending = from_json(&value)?;
                }
                "disk" => drop(snapshot.per_job.disks.insert(key, from_json(&value)?)),
                "base" => drop(snapshot.per_job.bases.insert(key, from_json(&value)?)),
                "image" => drop(snapshot.per_job.images.insert(key, from_json(&value)?)),
                "fill" => drop(snapshot.per_job.fills.insert(key, from_json(&value)?)),
                "execution" => drop(snapshot.per_job.executions.insert(key, from_json(&value)?)),
                _ => return Err(unknown("job VM entry kind", &kind)),
            }
        }
    }
    Ok(snapshot)
}

/// Reads the whole state. An entry kind or value name this binary doesn't
/// know means a newer binary wrote the database; the open fails rather than
/// serve a state it can only partly read.
fn load(conn: &Connection) -> rusqlite::Result<Snapshot> {
    let mut snapshot = Snapshot::default();
    let mut entries = conn.prepare("SELECT kind, key, value FROM host_entries")?;
    let mut rows = entries.query([])?;
    while let Some(row) = rows.next()? {
        let (kind, key, value): (String, String, String) = (row.get(0)?, row.get(1)?, row.get(2)?);
        match kind.as_str() {
            REVOCATION => drop(snapshot.revocations.insert(key, from_json(&value)?)),
            LEASE => drop(snapshot.leases.insert(key, from_json(&value)?)),
            NONCE => drop(snapshot.nonces.insert(key, from_json(&value)?)),
            VM => drop(snapshot.vms.insert(key, from_json(&value)?)),
            ZERO_USAGE => drop(snapshot.zero_usage.insert(key, from_json(&value)?)),
            PENDING_CANCEL => drop(snapshot.pending_cancels.insert(key, from_json(&value)?)),
            BILLING => drop(snapshot.billing.insert(key, from_json(&value)?)),
            CLOSED => drop(snapshot.closed.insert(key, from_json(&value)?)),
            _ => return Err(unknown("entry kind", &kind)),
        }
    }
    let mut values = conn.prepare("SELECT name, value FROM host_values")?;
    let mut rows = values.query([])?;
    while let Some(row) = rows.next()? {
        let (name, value): (String, String) = (row.get(0)?, row.get(1)?);
        match name.as_str() {
            "outbox" => snapshot.outbox = from_json(&value)?,
            "expired_outbox" => snapshot.expired_outbox = from_json(&value)?,
            "rejected_outbox" => snapshot.rejected_outbox = from_json(&value)?,
            "counter" => snapshot.counter = from_json(&value)?,
            "last_alive" => snapshot.last_alive = from_json(&value)?,
            "draining" => snapshot.draining = from_json(&value)?,
            "remote_cache_write_blocked" => {
                snapshot.remote_cache_write_blocked = from_json(&value)?
            }
            "accept_until_ms" => snapshot.accept_until_ms = from_json(&value)?,
            IMPORTED => {}
            _ => return Err(unknown("value", &name)),
        }
    }
    Ok(snapshot)
}

fn unknown(what: &str, name: &str) -> rusqlite::Error {
    rusqlite::Error::FromSqlConversionFailure(
        0,
        rusqlite::types::Type::Text,
        format!("unknown {what} {name:?}: the database was written by a newer motor-host").into(),
    )
}

#[cfg(test)]
mod rollback_tests {
    use super::*;

    #[test]
    fn unchanged_legacy_snapshot_loader_ignores_additive_job_vm_table() {
        let dir = tempfile::tempdir_in(cortexkit_test_support::scratch_root()).unwrap();
        let (mut journal, mut snapshot) = SqliteJournal::open(dir.path()).unwrap();
        snapshot.counter = 17;
        journal.store(&snapshot).unwrap();
        let legacy = snapshot.clone();
        snapshot.per_job.images.insert("gen-rollback".into(), serde_json::from_value(serde_json::json!({
            "manifest": {
                "kernel": {"bytes":4096,"sha256":"a".repeat(64)},
                "rootfs": {"bytes":4096,"sha256":"b".repeat(64)},
                "toolchain": {"bytes":8192,"sha256":"c".repeat(64)},
                "hello":{}, "guest_init":{}
            },
            "current":true, "accounts":{"account":false}, "recovery_holders":{"recovery":"account"}
        })).unwrap());
        snapshot.per_job.disks.insert(
            "ws-rollback".into(),
            crate::state::jobs::WorkspaceDisk {
                storage: Some(crate::state::jobs::storage::Disk::new(1024, 17)),
                sub: "account".into(),
                workspace_key_hash: "hash".into(),
                queue: vec![],
                attached_job: None,
                quarantined: false,
                recovery: None,
                recovery_result: None,
                deleted: false,
                have: Vec::new(),
            },
        );
        journal.store(&snapshot).unwrap();
        drop(journal);
        let conn = Connection::open(dir.path().join("store.db")).unwrap();
        // load is the unchanged tenant loader from before job-VM state existed.
        // It still rejects unknown legacy kinds, but never visits the new table.
        assert_eq!(load(&conn).unwrap(), legacy);
        assert_eq!(load_snapshot(&conn).unwrap(), snapshot);
    }

    #[test]
    fn tenant_rollback_pending_round_trips_without_changing_legacy_rows() {
        let dir = tempfile::tempdir_in(cortexkit_test_support::scratch_root()).unwrap();
        let (mut journal, mut snapshot) = SqliteJournal::open(dir.path()).unwrap();
        let legacy = snapshot.clone();
        snapshot.per_job.tenant_rollback_pending = true;
        journal.store(&snapshot).unwrap();
        drop(journal);
        let conn = Connection::open(dir.path().join("store.db")).unwrap();
        assert_eq!(load(&conn).unwrap(), legacy);
        drop(conn);
        let (mut journal, mut reopened) = SqliteJournal::open(dir.path()).unwrap();
        assert_eq!(reopened, snapshot);
        reopened.per_job.tenant_rollback_pending = false;
        journal.store(&reopened).unwrap();
        drop(journal);
        assert_eq!(SqliteJournal::open(dir.path()).unwrap().1, legacy);
    }

    #[test]
    fn emergency_write_block_survives_restart_and_off_state_is_unchanged() {
        let dir = tempfile::tempdir_in(cortexkit_test_support::scratch_root()).unwrap();
        let (mut journal, mut snapshot) = SqliteJournal::open(dir.path()).unwrap();
        let legacy_bytes = serde_json::to_vec(&snapshot).unwrap();
        assert!(!serde_json::to_value(&snapshot)
            .unwrap()
            .as_object()
            .unwrap()
            .contains_key("remote_cache_write_blocked"));
        let old: Snapshot = serde_json::from_slice(&legacy_bytes).unwrap();
        assert!(!old.remote_cache_write_blocked);
        snapshot.remote_cache_write_blocked = true;
        journal.store(&snapshot).unwrap();
        drop(journal);
        let (mut journal, mut reopened) = SqliteJournal::open(dir.path()).unwrap();
        assert_eq!(reopened, snapshot);
        reopened.remote_cache_write_blocked = false;
        journal.store(&reopened).unwrap();
        drop(journal);
        assert_eq!(
            serde_json::to_vec(&SqliteJournal::open(dir.path()).unwrap().1).unwrap(),
            legacy_bytes
        );
        let conn = Connection::open(dir.path().join("store.db")).unwrap();
        let count: u64 = conn
            .query_row(
                "SELECT COUNT(*) FROM host_values WHERE name = 'remote_cache_write_blocked'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(count, 0);
    }

    #[test]
    fn tenant_journal_updates_never_create_or_write_job_vm_rows() {
        let dir = tempfile::tempdir_in(cortexkit_test_support::scratch_root()).unwrap();
        let (mut journal, mut snapshot) = SqliteJournal::open(dir.path()).unwrap();
        snapshot.counter = 18;
        journal.store(&snapshot).unwrap();
        drop(journal);
        let conn = Connection::open(dir.path().join("store.db")).unwrap();
        let tables: u64 = conn
            .query_row(
                "SELECT COUNT(*) FROM sqlite_master WHERE name = 'job_vm_entries'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(tables, 0);
        drop(conn);
        // A tenant-mode rollback preserves the ledger without updating it.
        let (mut journal, mut snapshot) = SqliteJournal::open(dir.path()).unwrap();
        snapshot.per_job.disks.insert(
            "ws-rollback".into(),
            crate::state::jobs::WorkspaceDisk {
                storage: None,
                sub: "account".into(),
                workspace_key_hash: "hash".into(),
                queue: vec![],
                attached_job: None,
                quarantined: false,
                recovery: None,
                recovery_result: None,
                deleted: false,
                have: Vec::new(),
            },
        );
        journal.store(&snapshot).unwrap();
        drop(journal);
        let conn = Connection::open(dir.path().join("store.db")).unwrap();
        conn.execute_batch(
            "CREATE TRIGGER forbid_job_vm_update BEFORE UPDATE ON job_vm_entries
            BEGIN SELECT RAISE(ABORT, 'tenant wrote job VM state'); END;",
        )
        .unwrap();
        drop(conn);
        let (mut journal, mut snapshot) = SqliteJournal::open(dir.path()).unwrap();
        snapshot.counter += 1;
        journal.store(&snapshot).unwrap();
        drop(journal);
        assert_eq!(SqliteJournal::open(dir.path()).unwrap().1, snapshot);
    }
}
#[test]
fn legacy_loader_opens_job_claim_lease_and_ignores_boot_timestamp() {
    let root = cortexkit_test_support::scratch_root();
    std::fs::create_dir_all(&root).unwrap();
    let dir = tempfile::tempdir_in(root).unwrap();
    let (mut journal, mut snapshot) = SqliteJournal::open(dir.path()).unwrap();
    let job = "00000000-0000-7000-8000-000000000001";
    let claims: motor_lease::Claims = serde_json::from_value(serde_json::json!({
        "iss":motor_lease::ISSUER,"aud":"host","jti":job,"sub":"acct:01ARZ3NDEKTSV4RRFFQ69G5FAV",
        "purpose":"run","iat":1800000000,"exp":1800000300,"vcpus":1,"mem_mib":16384,
        "platform":"linux-x64","net":false,"placement":{"host":"host","vm":null},"max_s":30,
        "cnf":{"jkt":"key"},"workspace_key_hash":"hash"
    }))
    .unwrap();
    snapshot.leases.insert(
        job.into(),
        crate::state::Lease {
            claims: claims.clone(),
            tenant: job.into(),
            state: crate::state::LeaseState::Reserved,
            delivered: false,
            replacing: false,
            started_at_ms: None,
            ended_at_ms: None,
            highest_seq: 0,
            refusal: None,
        },
    );
    let legacy = snapshot.clone();
    snapshot.per_job.disks.insert(
        "ws-rollback".into(),
        crate::state::jobs::WorkspaceDisk {
            storage: Some(crate::state::jobs::storage::Disk {
                cap_bytes: 1024,
                last_used_ms: 123,
                growth_reservation_bytes: 512,
                evicting: true,
            }),
            sub: claims.sub.clone(),
            workspace_key_hash: "hash".into(),
            queue: vec![],
            attached_job: None,
            quarantined: false,
            recovery: None,
            recovery_result: None,
            deleted: false,
            have: vec![motor_protocol::MirrorRef {
                repository: Some("/repo".into()),
                dependency: None,
                reference: "refs/snapshots/held".into(),
                commit: "held".into(),
            }],
        },
    );
    snapshot.per_job.executions.insert(
        job.into(),
        crate::state::jobs::Execution {
            relayed_return_paths: None,
            usage_accepted_at_ms: None,
            image_generation: Some("gen-rollback".into()),
            replay_reservation_bytes: Some(crate::state::jobs::storage::REPLAY_RESERVATION_BYTES),
            final_replay_reservation_bytes: Some(crate::state::jobs::storage::REPLAY_RECORD_BYTES),
            replay_completion: None,
            evidence: Some(serde_json::from_value(serde_json::json!({
                "winner":"host_stop", "stop":{"cause":"host_oom","at_ms":1800000000001u64,"future":true},
                "exit":{"job_id":job,"oom_kill":3,"egress_bytes":12,"future":true},
                "acknowledged":true,"unclean_stop":true,"gone_at_ms":1800000000000u64,
                "usage_inputs":{"started_at_ms":1,"ended_at_ms":2,"outcome":"signal","mem_peak_mib":3,"mem_working_peak_mib":4,"mem_over_mib_ms":5,"oom_kill":"host_oom","future":true},
                "evidence_expired":true,
                "memory":{"jti":job,"mem_working_peak_mib":22,"mem_over_mib_ms":33},
                "future":true
            })).unwrap()),
            claims,
            disk_id: "ws-rollback".into(),
            memory_debit_mib: 16404,
            guest_ram_mib: 21846,
            boot_started: true,
            boot_started_at_ms: Some(1800000000000),
            runner_hello: false,
            ended: false,
            have_invalidated: true,
        },
    );
    snapshot.per_job.bases.insert(
        "base-test".into(),
        crate::state::jobs::storage::Base {
            sub: "acct:01ARZ3NDEKTSV4RRFFQ69G5FAV".into(),
            repository_key: None,
            have: Vec::new(),
            last_used_ms: 456,
            evicting: true,
        },
    );
    snapshot.per_job.fills.insert(
        "00000000-0000-7000-8000-000000000002".into(),
        crate::state::jobs::fills::Reservation {
            fill: motor_vm_helper::job::Fill {
                account: "acct:01ARZ3NDEKTSV4RRFFQ69G5FAV".into(),
                repository_key: "repo".into(),
                fill_id: "00000000-0000-7000-8000-000000000002".into(),
                size_bytes: 1024,
            },
            vcpus: 4,
            memory_debit_mib: 120,
            guest_ram_mib: 134,
            runner_hello: true,
            image_generation: Some("gen-rollback".into()),
            publication: Some(snapshot.per_job.disks["ws-rollback"].have.clone()),
        },
    );
    journal.store(&snapshot).unwrap();
    drop(journal);
    let conn = Connection::open(dir.path().join("store.db")).unwrap();
    assert_eq!(load(&conn).unwrap(), legacy);
    assert_eq!(load_snapshot(&conn).unwrap(), snapshot);
    drop(conn);
    for phase in [
        crate::state::jobs::recovery::Phase::Reserved,
        crate::state::jobs::recovery::Phase::Running,
        crate::state::jobs::recovery::Phase::Stopping,
        crate::state::jobs::recovery::Phase::Deleting,
    ] {
        let (mut journal, mut current) = SqliteJournal::open(dir.path()).unwrap();
        let disk = current.per_job.disks.get_mut("ws-rollback").unwrap();
        disk.quarantined = true;
        disk.recovery_result = Some("reformatted".into());
        disk.recovery = Some(crate::state::jobs::recovery::Recovery {
            id: job.into(),
            memory_debit_mib: 10,
            guest_ram_mib: 20,
            started_at_ms: Some(123),
            phase,
        });
        journal.store(&current).unwrap();
        drop(journal);
        let conn = Connection::open(dir.path().join("store.db")).unwrap();
        assert_eq!(load(&conn).unwrap(), legacy);
        assert_eq!(load_snapshot(&conn).unwrap(), current);
    }
}
#[cfg(test)]
fn source_slot_fixture() -> crate::jobs::fill_source::Slot {
    crate::jobs::fill_source::Slot {
        request: crate::jobs::fill_source::Request {
            account: "acct:01ARZ3NDEKTSV4RRFFQ69G5FAV".into(),
            repository_root: "/home/source".into(),
            integration_tip: "a".repeat(40),
            mem_mib: 100,
            observed_ms: 1,
        },
        generation: "referenced".into(),
        source: motor_protocol::FillSource {
            repository_root: "/home/source".into(),
            integration_tip: "a".repeat(40),
            bundles: vec![],
            git_dependencies: vec![],
        },
        bytes: 1,
        last_fill_started_ms: None,
    }
}

#[cfg(test)]
#[test]
fn fill_source_table_is_lazy_and_the_legacy_job_loader_can_update_old_state() {
    let dir = tempfile::tempdir_in(cortexkit_test_support::scratch_root()).unwrap();
    let (mut journal, mut snapshot) = SqliteJournal::open(dir.path()).unwrap();
    let exists = || {
        Connection::open(dir.path().join("store.db")).unwrap().query_row("SELECT EXISTS(SELECT 1 FROM sqlite_master WHERE type='table' AND name='fill_source_entries')", [], |row| row.get::<_, bool>(0)).unwrap()
    };
    assert!(!exists());
    journal.store(&snapshot).unwrap();
    assert!(!exists());
    let slot = source_slot_fixture();
    snapshot
        .per_job
        .export_tips
        .insert("job".into(), slot.request.clone());
    snapshot.per_job.fill_sources.insert("slot".into(), slot);
    journal.store(&snapshot).unwrap();
    assert!(exists());
    drop(journal);
    let mut conn = Connection::open(dir.path().join("store.db")).unwrap();
    // This is the unchanged loader used before fill sources existed: it reads
    // host and job tables, never enumerating unrelated additive tables.
    let legacy = load_job_snapshot(&conn).unwrap();
    assert!(legacy.per_job.fill_sources.is_empty());
    assert!(legacy.per_job.export_tips.is_empty());
    let mut updated = legacy.clone();
    updated.draining = true;
    let tx = conn.transaction().unwrap();
    write_diff(&tx, &legacy, &updated).unwrap();
    tx.commit().unwrap();
    assert!(load_job_snapshot(&conn).unwrap().draining);
    let restored = load_snapshot(&conn).unwrap();
    assert_eq!(restored.per_job.fill_sources, snapshot.per_job.fill_sources);
    assert_eq!(restored.per_job.export_tips, snapshot.per_job.export_tips);
}

#[cfg(test)]
#[test]
fn open_removes_unreferenced_source_generations_before_admission_and_export_keeps_referenced_files()
{
    let dir = tempfile::tempdir_in(cortexkit_test_support::scratch_root()).unwrap();
    let (mut journal, mut snapshot) = SqliteJournal::open(dir.path()).unwrap();
    snapshot
        .per_job
        .fill_sources
        .insert("slot".into(), source_slot_fixture());
    journal.store(&snapshot).unwrap();
    for name in [
        "referenced",
        "renamed-before-commit",
        "incoming-interrupted",
    ] {
        let path = crate::jobs::fill_source::directory(dir.path(), name);
        fs::create_dir_all(&path).unwrap();
        fs::write(path.join("0"), b"x").unwrap();
    }
    drop(journal);
    let (journal, restored) = SqliteJournal::open(dir.path()).unwrap();
    assert_eq!(restored.per_job.fill_sources, snapshot.per_job.fill_sources);
    let referenced = crate::jobs::fill_source::directory(dir.path(), "referenced");
    assert!(referenced.join("0").exists());
    assert!(!crate::jobs::fill_source::directory(dir.path(), "renamed-before-commit").exists());
    assert!(!crate::jobs::fill_source::directory(dir.path(), "incoming-interrupted").exists());
    drop(journal);
    SqliteJournal::export(dir.path()).unwrap();
    let (_, exported) = FileJournal::open(dir.path()).unwrap();
    assert_eq!(exported.per_job.fill_sources, snapshot.per_job.fill_sources);
    assert!(referenced.join("0").exists());
}
