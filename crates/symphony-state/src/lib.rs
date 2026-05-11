//! Durable orchestrator state for Symphony-RS v2.
//!
//! This crate owns the persistence boundary called out in
//! `ARCHITECTURE_v2.md` § 4.3: SQLite via `rusqlite` is the local default,
//! and a migration to another database requires its own ADR.
//!
//! The first checklist item under Phase 2 only bootstraps the crate and
//! its migration runner. Subsequent checklist items add the concrete
//! `work_items`, `runs`, `events`, etc. migrations and repository traits
//! on top of [`StateDb`] and [`Migration`].

#![forbid(unsafe_code)]
#![warn(missing_docs)]

pub mod cancel_requests;
pub mod decomposition_apply;
pub mod dependency_reconciliation;
pub mod edges;
pub mod events;
pub mod followup_apply;
pub mod followups;
pub mod handoffs;
pub mod integration_queue;
pub mod integration_records;
pub mod migrations;
pub mod pending_tracker_syncs;
pub mod pull_request_records;
pub mod qa_queue;
pub mod qa_verdicts;
pub mod recovery_queue_source;
pub mod repository;
pub mod run_lease_store;
pub mod transaction;
pub mod workspace_claims;

use std::path::Path;

use rusqlite::Connection;
use thiserror::Error;

/// Errors emitted by the durable state layer.
#[derive(Debug, Error)]
pub enum StateError {
    /// Underlying SQLite error.
    #[error("sqlite error: {0}")]
    Sqlite(#[from] rusqlite::Error),

    /// A migration's declared `version` was not strictly greater than the
    /// previously applied one, or a duplicate version was supplied. The
    /// runner refuses to apply out-of-order migrations because doing so
    /// would corrupt the schema-history audit trail.
    #[error(
        "migration version {found} is out of order; \
         expected a strictly increasing sequence after {previous}"
    )]
    OutOfOrderMigration {
        /// The version that was rejected.
        found: i64,
        /// The highest version seen so far in the input list.
        previous: i64,
    },

    /// A migration was previously applied with a different `name` or SQL
    /// body than the version supplied this run. We refuse silent drift
    /// because schema migrations are append-only history.
    #[error(
        "migration version {version} was already applied as `{recorded_name}`, \
         but the current binary supplied `{supplied_name}`"
    )]
    MigrationDrift {
        /// Migration version under conflict.
        version: i64,
        /// The name recorded in `schema_migrations` from a prior run.
        recorded_name: String,
        /// The name supplied by the current binary.
        supplied_name: String,
    },

    /// A typed run-status update was requested with a `(from, to)` pair
    /// that is not allowed by `RunStatus::valid_transition`, or the
    /// stored label could not be parsed as a known `RunStatus`.
    ///
    /// The `from` and `to` fields are stored as `String` rather than
    /// `RunStatus` because this crate's error type is shared by call
    /// sites that may not have a typed value in hand (e.g. an
    /// unparseable legacy label coming back from SQLite). The kernel
    /// `RunStatus` lives in `symphony-core`; the typed update API
    /// converts before constructing the error.
    #[error("invalid run status transition: {from} -> {to}")]
    InvalidRunTransition {
        /// The status currently recorded on the row, as a string.
        from: String,
        /// The status the caller attempted to transition to, as a string.
        to: String,
    },

    /// A composed state transition violated a kernel invariant before
    /// SQLite could express the problem.
    #[error("state invariant violation: {0}")]
    Invariant(String),
}

/// Convenience result alias for the state crate.
pub type StateResult<T> = Result<T, StateError>;

/// A single forward-only schema migration.
///
/// Migrations are identified by a strictly increasing integer `version`.
/// `name` is human-readable and stored in the `schema_migrations` audit
/// table; `sql` is executed inside a transaction by [`StateDb::migrate`].
#[derive(Debug, Clone)]
pub struct Migration {
    /// Strictly increasing version number. Convention: `YYYYMMDDNN`.
    pub version: i64,
    /// Human-readable migration name; recorded for audit.
    pub name: &'static str,
    /// SQL body executed via `Connection::execute_batch`.
    pub sql: &'static str,
}

/// A durable state database handle.
///
/// Wraps a single `rusqlite::Connection`. The handle is intentionally not
/// `Clone` and not `Send`; downstream layers will introduce a connection
/// pool or worker model as concrete needs appear (see the ADR in
/// `ARCHITECTURE_v2.md`).
pub struct StateDb {
    conn: Connection,
}

impl StateDb {
    /// Open (or create) a SQLite database at `path`.
    ///
    /// Applies the runtime pragmas Symphony-RS v2 standardizes on:
    ///
    /// * `journal_mode = WAL` for crash-safe writers + concurrent readers.
    /// * `foreign_keys = ON` so parent/child and blocker edge invariants
    ///   are enforced at the storage layer.
    /// * `synchronous = NORMAL` (the WAL-recommended balance of safety
    ///   and throughput).
    pub fn open<P: AsRef<Path>>(path: P) -> StateResult<Self> {
        let conn = Connection::open(path)?;
        Self::configure(&conn)?;
        Ok(Self { conn })
    }

    /// Open an in-memory database. Intended for tests.
    pub fn open_in_memory() -> StateResult<Self> {
        let conn = Connection::open_in_memory()?;
        Self::configure(&conn)?;
        Ok(Self { conn })
    }

    fn configure(conn: &Connection) -> StateResult<()> {
        // `journal_mode = WAL` is a no-op on `:memory:` databases (they
        // stay in `memory` mode), but issuing the pragma is harmless and
        // keeps the file-backed path identical.
        conn.pragma_update(None, "journal_mode", "WAL")?;
        conn.pragma_update(None, "foreign_keys", "ON")?;
        conn.pragma_update(None, "synchronous", "NORMAL")?;
        Ok(())
    }

    /// Borrow the underlying connection. Used by repository modules
    /// inside this crate; not part of the cross-crate API contract.
    pub(crate) fn conn(&self) -> &Connection {
        &self.conn
    }

    /// Apply the supplied migrations in order, skipping any whose
    /// `version` was already recorded.
    ///
    /// Each migration runs inside its own transaction together with the
    /// `INSERT INTO schema_migrations` row, so a crash mid-migration
    /// leaves the database either fully migrated to that version or
    /// fully unmigrated past the previous one.
    pub fn migrate(&mut self, migrations: &[Migration]) -> StateResult<MigrationReport> {
        // Validate ordering up front so a malformed input list cannot
        // partially apply rows before the violation is detected.
        let mut previous_input_version: Option<i64> = None;
        for migration in migrations {
            if let Some(prev) = previous_input_version
                && migration.version <= prev
            {
                return Err(StateError::OutOfOrderMigration {
                    found: migration.version,
                    previous: prev,
                });
            }
            previous_input_version = Some(migration.version);
        }

        ensure_meta_table(&self.conn)?;

        let mut applied: Vec<i64> = Vec::new();
        let mut skipped: Vec<i64> = Vec::new();

        for migration in migrations {
            if let Some(recorded) = lookup_recorded_migration(&self.conn, migration.version)? {
                if recorded != migration.name {
                    return Err(StateError::MigrationDrift {
                        version: migration.version,
                        recorded_name: recorded,
                        supplied_name: migration.name.to_string(),
                    });
                }
                skipped.push(migration.version);
                continue;
            }

            let tx = self.conn.transaction()?;
            tx.execute_batch(migration.sql)?;
            tx.execute(
                "INSERT INTO schema_migrations (version, name, applied_at) \
                 VALUES (?1, ?2, strftime('%Y-%m-%dT%H:%M:%fZ', 'now'))",
                rusqlite::params![migration.version, migration.name],
            )?;
            tx.commit()?;
            applied.push(migration.version);
        }

        Ok(MigrationReport { applied, skipped })
    }

    /// Return the list of versions currently recorded in
    /// `schema_migrations`, in ascending order. Used by tests and by the
    /// future `symphony recover` CLI.
    pub fn applied_versions(&self) -> StateResult<Vec<i64>> {
        ensure_meta_table(&self.conn)?;
        let mut stmt = self
            .conn
            .prepare("SELECT version FROM schema_migrations ORDER BY version ASC")?;
        let rows = stmt.query_map([], |row| row.get::<_, i64>(0))?;
        let mut out = Vec::new();
        for row in rows {
            out.push(row?);
        }
        Ok(out)
    }
}

/// Summary of a single [`StateDb::migrate`] call.
#[derive(Debug, Default, PartialEq, Eq)]
pub struct MigrationReport {
    /// Versions newly applied during this call, in order.
    pub applied: Vec<i64>,
    /// Versions skipped because they were already recorded.
    pub skipped: Vec<i64>,
}

fn ensure_meta_table(conn: &Connection) -> StateResult<()> {
    conn.execute_batch(
        "CREATE TABLE IF NOT EXISTS schema_migrations (
            version    INTEGER PRIMARY KEY,
            name       TEXT NOT NULL,
            applied_at TEXT NOT NULL
         );",
    )?;
    Ok(())
}

fn lookup_recorded_migration(conn: &Connection, version: i64) -> StateResult<Option<String>> {
    let mut stmt = conn.prepare("SELECT name FROM schema_migrations WHERE version = ?1")?;
    let mut rows = stmt.query(rusqlite::params![version])?;
    match rows.next()? {
        Some(row) => Ok(Some(row.get::<_, String>(0)?)),
        None => Ok(None),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn meta_only() -> Migration {
        Migration {
            version: 1,
            name: "create_widgets",
            sql: "CREATE TABLE widgets (id INTEGER PRIMARY KEY, label TEXT NOT NULL);",
        }
    }

    fn second_migration() -> Migration {
        Migration {
            version: 2,
            name: "add_widget_size",
            sql: "ALTER TABLE widgets ADD COLUMN size INTEGER NOT NULL DEFAULT 0;",
        }
    }

    #[test]
    fn open_in_memory_applies_pragmas() {
        let db = StateDb::open_in_memory().expect("open");
        let fk: i64 = db
            .conn()
            .query_row("PRAGMA foreign_keys", [], |row| row.get(0))
            .expect("foreign_keys");
        assert_eq!(fk, 1, "foreign_keys pragma should be ON");
    }

    #[test]
    fn migrate_applies_new_migrations_in_order() {
        let mut db = StateDb::open_in_memory().expect("open");
        let report = db
            .migrate(&[meta_only(), second_migration()])
            .expect("migrate");
        assert_eq!(report.applied, vec![1, 2]);
        assert!(report.skipped.is_empty());
        assert_eq!(db.applied_versions().unwrap(), vec![1, 2]);
    }

    #[test]
    fn migrate_is_idempotent_for_recorded_versions() {
        let mut db = StateDb::open_in_memory().expect("open");
        db.migrate(&[meta_only()]).expect("first run");
        let report = db
            .migrate(&[meta_only(), second_migration()])
            .expect("second run");
        assert_eq!(report.applied, vec![2]);
        assert_eq!(report.skipped, vec![1]);
    }

    #[test]
    fn out_of_order_migrations_are_rejected() {
        let mut db = StateDb::open_in_memory().expect("open");
        let err = db
            .migrate(&[second_migration(), meta_only()])
            .expect_err("must reject");
        assert!(
            matches!(
                err,
                StateError::OutOfOrderMigration {
                    found: 1,
                    previous: 2
                }
            ),
            "got {err:?}"
        );
    }

    #[test]
    fn drift_in_migration_name_is_rejected() {
        let mut db = StateDb::open_in_memory().expect("open");
        db.migrate(&[meta_only()]).expect("apply v1");
        let renamed = Migration {
            version: 1,
            name: "create_widgets_v2",
            sql: meta_only().sql,
        };
        let err = db.migrate(&[renamed]).expect_err("must reject drift");
        assert!(
            matches!(
                err,
                StateError::MigrationDrift {
                    version: 1,
                    ref recorded_name,
                    ref supplied_name,
                } if recorded_name == "create_widgets" && supplied_name == "create_widgets_v2"
            ),
            "got {err:?}"
        );
    }

    #[test]
    fn failed_migration_rolls_back_atomically() {
        let mut db = StateDb::open_in_memory().expect("open");
        let bad = Migration {
            version: 1,
            name: "broken",
            sql: "CREATE TABLE good (id INTEGER); CREATE TABLE good (id INTEGER);",
        };
        let err = db.migrate(&[bad]).expect_err("must fail");
        assert!(matches!(err, StateError::Sqlite(_)), "got {err:?}");

        // Neither the table nor the schema_migrations row should exist.
        let table_count: i64 = db
            .conn()
            .query_row(
                "SELECT COUNT(*) FROM sqlite_master WHERE type='table' AND name='good'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(table_count, 0, "rolled-back table must not exist");
        assert!(db.applied_versions().unwrap().is_empty());
    }

    #[test]
    fn state_survives_reopen_on_disk() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("state.sqlite3");

        {
            let mut db = StateDb::open(&path).expect("open");
            db.migrate(&[meta_only(), second_migration()])
                .expect("migrate");
            db.conn()
                .execute("INSERT INTO widgets (label, size) VALUES ('hello', 7)", [])
                .unwrap();
        }

        let db = StateDb::open(&path).expect("reopen");
        assert_eq!(db.applied_versions().unwrap(), vec![1, 2]);
        let (label, size): (String, i64) = db
            .conn()
            .query_row("SELECT label, size FROM widgets", [], |row| {
                Ok((row.get(0)?, row.get(1)?))
            })
            .expect("row");
        assert_eq!(label, "hello");
        assert_eq!(size, 7);
    }

    #[test]
    fn invalid_run_transition_renders_expected_message() {
        let err = StateError::InvalidRunTransition {
            from: "completed".into(),
            to: "running".into(),
        };
        assert_eq!(
            err.to_string(),
            "invalid run status transition: completed -> running"
        );
    }

    #[test]
    fn invalid_run_transition_debug_carries_fields() {
        let err = StateError::InvalidRunTransition {
            from: "queued".into(),
            to: "completed".into(),
        };
        let debug = format!("{err:?}");
        assert!(
            debug.contains("InvalidRunTransition"),
            "debug must name the variant: {debug}"
        );
        assert!(debug.contains("queued"), "debug must include from: {debug}");
        assert!(
            debug.contains("completed"),
            "debug must include to: {debug}"
        );
    }
}
