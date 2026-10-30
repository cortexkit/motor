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
            store.with_conn(load).map_err(other)?
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
        Ok((
            Self {
                store,
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
        let (mut file, _) = FileJournal::open(directory)?;
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
    } = new;
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
    diff_value(tx, "draining", &old.draining, draining)
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
