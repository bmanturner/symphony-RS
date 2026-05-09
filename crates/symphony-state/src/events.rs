//! Append-only event log with monotonically increasing sequence numbers.
//!
//! The `events` table is the durable log behind every operator surface
//! Symphony-RS v2 ships: SSE replay buffers (SPEC v2 §5.14), `symphony
//! status`, the TUI, and crash recovery audit trails. Two invariants make
//! the log useful:
//!
//! 1. **Append-only.** Rows are never updated or deleted by product code.
//!    The schema lacks an `UPDATE` path on purpose — see
//!    `crate::migrations`.
//! 2. **Monotonically increasing `sequence`.** Every row gets a `sequence`
//!    strictly greater than every prior row. SSE consumers can resume by
//!    passing `after_sequence` and trust they will not miss entries.
//!
//! `sequence` is assigned in the same SQL statement as the insert
//! (`SELECT COALESCE(MAX(sequence), 0) + 1 ...`) so a single transaction
//! cannot hand out duplicate values. The `UNIQUE` constraint on
//! `events.sequence` catches the cross-connection edge case a future
//! pooled writer would introduce, surfacing as [`crate::StateError::Sqlite`].
//!
//! The kernel will compose [`EventRepository::append_event`] with state
//! transitions inside one transaction in Phase 2's transaction-helper
//! checklist item; this module deliberately keeps the API narrow so that
//! composition is the only public way the kernel writes events.

use rusqlite::{OptionalExtension, params};

use crate::repository::{RunId, WorkItemId};
use crate::{StateDb, StateResult};

/// Strongly-typed primary key for `events`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct EventId(pub i64);

/// Strongly-typed sequence number from the `events.sequence` column.
///
/// Sequence is distinct from `id` because consumers need an ordering
/// guarantee even if SQLite ever recycles rowids (it doesn't today, but
/// the contract should not depend on that).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct EventSequence(pub i64);

/// An `events` row as stored.
///
/// `payload` is opaque JSON at this layer: typed event variants land in
/// `symphony-core` during Phase 12 alongside `OrchestratorEvent`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EventRecord {
    /// Primary key.
    pub id: EventId,
    /// Monotonically increasing sequence number.
    pub sequence: EventSequence,
    /// Event type discriminator (e.g. `work_item.created`, `qa.verdict`).
    pub event_type: String,
    /// Optional related work item.
    pub work_item_id: Option<WorkItemId>,
    /// Optional related run.
    pub run_id: Option<RunId>,
    /// Opaque JSON payload as stored.
    pub payload: String,
    /// RFC3339 created timestamp.
    pub created_at: String,
}

/// Insertion payload for [`EventRepository::append_event`].
#[derive(Debug, Clone)]
pub struct NewEvent<'a> {
    /// Event type discriminator.
    pub event_type: &'a str,
    /// Optional related work item.
    pub work_item_id: Option<WorkItemId>,
    /// Optional related run.
    pub run_id: Option<RunId>,
    /// Already-encoded JSON payload. The repository does not validate it.
    pub payload: &'a str,
    /// RFC3339 timestamp written to `created_at`.
    pub now: &'a str,
}

/// Append-only access to the event log.
pub trait EventRepository {
    /// Append one event, returning the persisted record (including the
    /// kernel-assigned [`EventSequence`]).
    ///
    /// The implementation guarantees the assigned sequence is strictly
    /// greater than every previously appended event on this database.
    fn append_event(&mut self, new: NewEvent<'_>) -> StateResult<EventRecord>;

    /// The highest sequence seen so far, or `None` if the log is empty.
    /// SSE consumers seed their cursor with this value at startup.
    fn last_event_sequence(&self) -> StateResult<Option<EventSequence>>;

    /// Events strictly after `after`, in ascending sequence order, capped
    /// at `limit`. Pass `None` to start from the beginning.
    fn list_events_after(
        &self,
        after: Option<EventSequence>,
        limit: usize,
    ) -> StateResult<Vec<EventRecord>>;

    /// Events for one work item, in ascending sequence order. Useful for
    /// `symphony issue graph <id>` and audit views.
    fn list_events_for_work_item(&self, work_item_id: WorkItemId) -> StateResult<Vec<EventRecord>>;
}

const EVENT_COLUMNS: &str = "id, sequence, event_type, work_item_id, run_id, payload, created_at";

fn map_event(row: &rusqlite::Row<'_>) -> rusqlite::Result<EventRecord> {
    Ok(EventRecord {
        id: EventId(row.get(0)?),
        sequence: EventSequence(row.get(1)?),
        event_type: row.get(2)?,
        work_item_id: row.get::<_, Option<i64>>(3)?.map(WorkItemId),
        run_id: row.get::<_, Option<i64>>(4)?.map(RunId),
        payload: row.get(5)?,
        created_at: row.get(6)?,
    })
}

impl EventRepository for StateDb {
    fn append_event(&mut self, new: NewEvent<'_>) -> StateResult<EventRecord> {
        let conn = self.conn();
        // The `SELECT COALESCE(MAX(sequence), 0) + 1 FROM events` subquery
        // executes inside the same statement as the INSERT, so the
        // sequence read and write are atomic on a single connection. The
        // UNIQUE index on `events.sequence` is the safety net for any
        // future multi-connection writer.
        conn.execute(
            "INSERT INTO events \
                (sequence, event_type, work_item_id, run_id, payload, created_at) \
             VALUES \
                ((SELECT COALESCE(MAX(sequence), 0) + 1 FROM events), \
                 ?1, ?2, ?3, ?4, ?5)",
            params![
                new.event_type,
                new.work_item_id.map(|id| id.0),
                new.run_id.map(|id| id.0),
                new.payload,
                new.now,
            ],
        )?;
        let id = EventId(conn.last_insert_rowid());
        let sql = format!("SELECT {EVENT_COLUMNS} FROM events WHERE id = ?1");
        let mut stmt = conn.prepare(&sql)?;
        let record = stmt.query_row(params![id.0], map_event)?;
        Ok(record)
    }

    fn last_event_sequence(&self) -> StateResult<Option<EventSequence>> {
        let mut stmt = self.conn().prepare("SELECT MAX(sequence) FROM events")?;
        let value: Option<i64> = stmt
            .query_row([], |row| row.get::<_, Option<i64>>(0))
            .optional()?
            .flatten();
        Ok(value.map(EventSequence))
    }

    fn list_events_after(
        &self,
        after: Option<EventSequence>,
        limit: usize,
    ) -> StateResult<Vec<EventRecord>> {
        let cutoff = after.map(|s| s.0).unwrap_or(0);
        // `i64::try_from(usize)` is the right cast for SQLite's LIMIT. A
        // pathological `usize::MAX` on a 64-bit host saturates to i64::MAX,
        // which is well past any realistic page size.
        let limit_sql = i64::try_from(limit).unwrap_or(i64::MAX);
        let sql = format!(
            "SELECT {EVENT_COLUMNS} FROM events \
             WHERE sequence > ?1 \
             ORDER BY sequence ASC \
             LIMIT ?2"
        );
        let mut stmt = self.conn().prepare(&sql)?;
        let rows = stmt.query_map(params![cutoff, limit_sql], map_event)?;
        let mut out = Vec::new();
        for row in rows {
            out.push(row?);
        }
        Ok(out)
    }

    fn list_events_for_work_item(&self, work_item_id: WorkItemId) -> StateResult<Vec<EventRecord>> {
        let sql = format!(
            "SELECT {EVENT_COLUMNS} FROM events \
             WHERE work_item_id = ?1 \
             ORDER BY sequence ASC"
        );
        let mut stmt = self.conn().prepare(&sql)?;
        let rows = stmt.query_map(params![work_item_id.0], map_event)?;
        let mut out = Vec::new();
        for row in rows {
            out.push(row?);
        }
        Ok(out)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::migrations::migrations;
    use crate::repository::{NewRun, NewWorkItem, RunRepository, WorkItemRepository};

    fn open() -> StateDb {
        let mut db = StateDb::open_in_memory().expect("open");
        db.migrate(migrations()).expect("migrate");
        db
    }

    fn seed_work_item(db: &mut StateDb, identifier: &str) -> WorkItemId {
        db.create_work_item(NewWorkItem {
            tracker_id: "github",
            identifier,
            parent_id: None,
            title: "demo",
            status_class: "ready",
            tracker_status: "open",
            assigned_role: None,
            assigned_agent: None,
            priority: None,
            workspace_policy: None,
            branch_policy: None,
            now: "2026-05-08T00:00:00Z",
        })
        .expect("work item")
        .id
    }

    fn seed_run(db: &mut StateDb, work_item_id: WorkItemId) -> RunId {
        db.create_run(NewRun {
            work_item_id,
            role: "platform_lead",
            agent: "claude",
            status: "queued",
            workspace_claim_id: None,
            now: "2026-05-08T00:00:00Z",
        })
        .expect("run")
        .id
    }

    fn append(db: &mut StateDb, event_type: &str, wi: Option<WorkItemId>) -> EventRecord {
        db.append_event(NewEvent {
            event_type,
            work_item_id: wi,
            run_id: None,
            payload: "{}",
            now: "2026-05-08T00:00:00Z",
        })
        .expect("append")
    }

    #[test]
    fn last_event_sequence_is_none_when_log_is_empty() {
        let db = open();
        assert!(db.last_event_sequence().expect("query").is_none());
    }

    #[test]
    fn append_assigns_sequence_one_when_log_is_empty() {
        let mut db = open();
        let event = append(&mut db, "work_item.created", None);
        assert_eq!(event.sequence, EventSequence(1));
        assert_eq!(event.event_type, "work_item.created");
        assert_eq!(event.payload, "{}");
        assert!(event.work_item_id.is_none());
    }

    #[test]
    fn sequences_are_strictly_increasing() {
        let mut db = open();
        let mut last = 0;
        for i in 0..5 {
            let event = append(&mut db, "tick", None);
            assert!(
                event.sequence.0 > last,
                "iteration {i}: sequence {:?} not greater than {last}",
                event.sequence
            );
            last = event.sequence.0;
        }
        assert_eq!(db.last_event_sequence().unwrap(), Some(EventSequence(5)));
    }

    #[test]
    fn append_persists_optional_foreign_keys() {
        let mut db = open();
        let wi = seed_work_item(&mut db, "ENG-1");
        let run = seed_run(&mut db, wi);
        let event = db
            .append_event(NewEvent {
                event_type: "run.started",
                work_item_id: Some(wi),
                run_id: Some(run),
                payload: r#"{"agent":"claude"}"#,
                now: "2026-05-08T01:00:00Z",
            })
            .expect("append");
        assert_eq!(event.work_item_id, Some(wi));
        assert_eq!(event.run_id, Some(run));
        assert_eq!(event.payload, r#"{"agent":"claude"}"#);
    }

    #[test]
    fn list_events_after_skips_already_seen_sequences() {
        let mut db = open();
        for n in 0..3 {
            append(&mut db, &format!("type-{n}"), None);
        }
        let after_first = db
            .list_events_after(Some(EventSequence(1)), 100)
            .expect("after");
        let types: Vec<_> = after_first.iter().map(|e| e.event_type.clone()).collect();
        assert_eq!(types, vec!["type-1", "type-2"]);
    }

    #[test]
    fn list_events_after_respects_limit() {
        let mut db = open();
        for n in 0..5 {
            append(&mut db, &format!("type-{n}"), None);
        }
        let page = db.list_events_after(None, 2).expect("page");
        assert_eq!(page.len(), 2);
        assert_eq!(page[0].sequence, EventSequence(1));
        assert_eq!(page[1].sequence, EventSequence(2));
    }

    #[test]
    fn list_events_for_work_item_filters_correctly() {
        let mut db = open();
        let wi_a = seed_work_item(&mut db, "ENG-A");
        let wi_b = seed_work_item(&mut db, "ENG-B");
        append(&mut db, "noise", None);
        append(&mut db, "for-a-1", Some(wi_a));
        append(&mut db, "for-b", Some(wi_b));
        append(&mut db, "for-a-2", Some(wi_a));

        let a_events = db.list_events_for_work_item(wi_a).expect("a events");
        let types: Vec<_> = a_events.iter().map(|e| e.event_type.clone()).collect();
        assert_eq!(types, vec!["for-a-1", "for-a-2"]);
        // Sequence ordering is preserved.
        assert!(a_events[0].sequence < a_events[1].sequence);
    }

    #[test]
    fn duplicate_sequence_is_rejected_by_unique_index() {
        let mut db = open();
        append(&mut db, "first", None);
        // Force a collision by writing directly with an existing sequence.
        let err = db.conn().execute(
            "INSERT INTO events (sequence, event_type, payload, created_at) \
             VALUES (1, 'collision', '{}', '2026-05-08T00:00:00Z')",
            [],
        );
        assert!(err.is_err(), "UNIQUE(sequence) must reject duplicate");
    }

    #[test]
    fn sequence_continues_monotonic_after_reopen() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("state.sqlite3");

        {
            let mut db = StateDb::open(&path).expect("open");
            db.migrate(migrations()).expect("migrate");
            for _ in 0..3 {
                append(&mut db, "tick", None);
            }
        }

        let mut db = StateDb::open(&path).expect("reopen");
        let next = append(&mut db, "after-restart", None);
        assert_eq!(next.sequence, EventSequence(4));
        assert_eq!(db.last_event_sequence().unwrap(), Some(EventSequence(4)));
    }
}
