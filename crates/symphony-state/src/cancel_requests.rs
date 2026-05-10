//! Persistence for the `cancel_requests` table (SPEC v2 §4.5 / Phase 11.5).
//!
//! The in-memory [`symphony_core::cancellation::CancellationQueue`] is the
//! dispatch surface runners poll at safe points; this module is its
//! durable mirror. Each pending [`CancelRequest`] is written through to
//! SQLite at enqueue time so an operator-issued cancel survives a
//! scheduler crash, and a freshly-started process can rebuild the
//! queue's pending set by replaying
//! [`CancelRequestRepository::list_pending_cancel_requests`] through
//! [`symphony_core::cancellation::CancellationQueue::from_pending`].
//!
//! Lifecycle:
//!
//! 1. Operator (or cascade) calls [`CancelRequestRepository::enqueue_cancel_request`].
//!    The composition root mirrors the same `CancelRequest` into the
//!    in-memory queue. Re-enqueuing the same subject replaces the stored
//!    reason/identity/timestamp in-place — same shape as the in-memory
//!    primitive, enforced by the partial unique index on
//!    `(subject_kind, subject_id) WHERE state = 'pending'`.
//! 2. A runner observes the pending entry pre-lease, transitions the
//!    target run to its terminal state, and calls
//!    [`CancelRequestRepository::drain_cancel_request_for_run`] (or
//!    [`CancelRequestRepository::drain_cancel_request_for_work_item`]).
//!    The row stays in the table with `state = 'drained'` and
//!    `drained_at` set, so the pending unique index releases the slot
//!    but the audit trail survives.
//! 3. On startup, the composition root calls
//!    [`CancelRequestRepository::list_pending_cancel_requests`] and
//!    feeds the result to `CancellationQueue::from_pending`. Drained
//!    rows are not replayed.

use rusqlite::{Connection, OptionalExtension, params};

use symphony_core::blocker::RunRef;
use symphony_core::cancellation::{CancelRequest, CancelSubject};
use symphony_core::work_item::WorkItemId as CoreWorkItemId;

use crate::{StateDb, StateResult};

/// Strongly-typed primary key for `cancel_requests`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct CancelRequestId(pub i64);

/// Durable lifecycle state of a `cancel_requests` row.
///
/// Mirrors the SQL `CHECK` enumeration. Stored as a discriminator string
/// in the database; the typed enum lets call sites pattern-match without
/// stringly-typed comparisons.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum CancelRequestState {
    /// Still observable to runners. The partial unique index guarantees
    /// at most one pending row per `(subject_kind, subject_id)`.
    Pending,
    /// A runner observed the cancel and transitioned its target. The
    /// row is preserved as audit; the pending slot for the subject is
    /// free to be re-used by a fresh cancel.
    Drained,
}

impl CancelRequestState {
    /// Stable lowercase identifier matching the SQL `CHECK` set.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Pending => "pending",
            Self::Drained => "drained",
        }
    }

    fn parse(raw: &str) -> Option<Self> {
        match raw {
            "pending" => Some(Self::Pending),
            "drained" => Some(Self::Drained),
            _ => None,
        }
    }
}

/// One row from the `cancel_requests` table.
///
/// The kernel-side [`CancelRequest`] is recoverable via [`Self::request`];
/// this struct preserves the durable-only metadata (id, lifecycle state,
/// drained-at timestamp) for audit and observability surfaces.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CancelRequestRecord {
    /// Primary key.
    pub id: CancelRequestId,
    /// Reconstructed kernel-side cancel request.
    pub request: CancelRequest,
    /// Lifecycle state.
    pub state: CancelRequestState,
    /// RFC3339 row creation timestamp.
    pub created_at: String,
    /// RFC3339 timestamp of the drain transition, if any.
    pub drained_at: Option<String>,
}

/// CRUD over `cancel_requests`.
///
/// The trait's contract intentionally mirrors
/// [`symphony_core::cancellation::CancellationQueue`] so the composition
/// root can write through to durable storage on every queue mutation
/// without translating shapes.
pub trait CancelRequestRepository {
    /// Enqueue a pending cancel request for the supplied subject.
    ///
    /// If a pending row already exists for the same subject, its
    /// `reason`, `requested_by`, and `requested_at` are overwritten in
    /// place — matching the in-memory queue's idempotent replace
    /// semantics and the partial unique index.
    ///
    /// Returns `true` if a new pending row was inserted, `false` if an
    /// existing pending row was updated. Mirrors the boolean returned by
    /// `CancellationQueue::enqueue` so callers can branch on
    /// "first cancel observed" without re-reading the row.
    fn enqueue_cancel_request(&mut self, request: &CancelRequest, now: &str) -> StateResult<bool>;

    /// Mark the pending row for `run_id` as drained. Returns the row as
    /// it stood before the transition (so callers can log the consumed
    /// reason/identity); returns `None` if no pending row exists.
    fn drain_cancel_request_for_run(
        &mut self,
        run_id: RunRef,
        now: &str,
    ) -> StateResult<Option<CancelRequestRecord>>;

    /// Mark the pending row for `work_item_id` as drained. Returns the
    /// row as it stood before the transition; returns `None` if no
    /// pending row exists.
    fn drain_cancel_request_for_work_item(
        &mut self,
        work_item_id: CoreWorkItemId,
        now: &str,
    ) -> StateResult<Option<CancelRequestRecord>>;

    /// All pending rows, ordered by insertion id (the order operators
    /// enqueued them). Used by startup hydration to seed the in-memory
    /// queue.
    fn list_pending_cancel_requests(&self) -> StateResult<Vec<CancelRequestRecord>>;

    /// Single-row read by primary key. Used by tests and the
    /// `symphony cancel` CLI surface for echoing what was persisted.
    fn get_cancel_request(&self, id: CancelRequestId) -> StateResult<Option<CancelRequestRecord>>;
}

const COLUMNS: &str = "id, subject_kind, subject_id, reason, requested_by, \
     requested_at, state, created_at, drained_at";

fn map_record(row: &rusqlite::Row<'_>) -> rusqlite::Result<CancelRequestRecord> {
    let id: i64 = row.get(0)?;
    let subject_kind: String = row.get(1)?;
    let subject_id: i64 = row.get(2)?;
    let reason: String = row.get(3)?;
    let requested_by: String = row.get(4)?;
    let requested_at: String = row.get(5)?;
    let state_raw: String = row.get(6)?;
    let created_at: String = row.get(7)?;
    let drained_at: Option<String> = row.get(8)?;

    let subject = match subject_kind.as_str() {
        "run" => CancelSubject::run(RunRef::new(subject_id)),
        "work_item" => CancelSubject::work_item(CoreWorkItemId::new(subject_id)),
        other => {
            return Err(rusqlite::Error::FromSqlConversionFailure(
                1,
                rusqlite::types::Type::Text,
                Box::new(std::io::Error::new(
                    std::io::ErrorKind::InvalidData,
                    format!("unknown cancel subject_kind: {other}"),
                )),
            ));
        }
    };
    let state = CancelRequestState::parse(&state_raw).ok_or_else(|| {
        rusqlite::Error::FromSqlConversionFailure(
            6,
            rusqlite::types::Type::Text,
            Box::new(std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                format!("unknown cancel_requests.state: {state_raw}"),
            )),
        )
    })?;

    Ok(CancelRequestRecord {
        id: CancelRequestId(id),
        request: CancelRequest {
            subject,
            reason,
            requested_by,
            requested_at,
        },
        state,
        created_at,
        drained_at,
    })
}

fn subject_columns(subject: &CancelSubject) -> (&'static str, i64) {
    match subject {
        CancelSubject::Run { run_id } => ("run", run_id.get()),
        CancelSubject::WorkItem { work_item_id } => ("work_item", work_item_id.get()),
    }
}

fn enqueue_in(conn: &Connection, request: &CancelRequest, now: &str) -> StateResult<bool> {
    let (kind, sid) = subject_columns(&request.subject);

    // Check for an existing pending row first so we can preserve
    // `created_at` on replace and report the boolean the in-memory
    // primitive returns.
    let existing: Option<i64> = conn
        .query_row(
            "SELECT id FROM cancel_requests \
              WHERE subject_kind = ?1 AND subject_id = ?2 AND state = 'pending'",
            params![kind, sid],
            |row| row.get(0),
        )
        .optional()?;

    match existing {
        Some(id) => {
            conn.execute(
                "UPDATE cancel_requests \
                    SET reason = ?1, requested_by = ?2, requested_at = ?3 \
                  WHERE id = ?4",
                params![
                    request.reason,
                    request.requested_by,
                    request.requested_at,
                    id
                ],
            )?;
            Ok(false)
        }
        None => {
            conn.execute(
                "INSERT INTO cancel_requests \
                    (subject_kind, subject_id, reason, requested_by, \
                     requested_at, state, created_at) \
                 VALUES (?1, ?2, ?3, ?4, ?5, 'pending', ?6)",
                params![
                    kind,
                    sid,
                    request.reason,
                    request.requested_by,
                    request.requested_at,
                    now,
                ],
            )?;
            Ok(true)
        }
    }
}

fn drain_in(
    conn: &Connection,
    subject_kind: &str,
    subject_id: i64,
    now: &str,
) -> StateResult<Option<CancelRequestRecord>> {
    let sql = format!(
        "SELECT {COLUMNS} FROM cancel_requests \
         WHERE subject_kind = ?1 AND subject_id = ?2 AND state = 'pending'"
    );
    let mut stmt = conn.prepare(&sql)?;
    let row = stmt
        .query_row(params![subject_kind, subject_id], map_record)
        .optional()?;
    let Some(record) = row else {
        return Ok(None);
    };
    conn.execute(
        "UPDATE cancel_requests \
            SET state = 'drained', drained_at = ?1 \
          WHERE id = ?2",
        params![now, record.id.0],
    )?;
    Ok(Some(record))
}

fn list_pending_in(conn: &Connection) -> StateResult<Vec<CancelRequestRecord>> {
    let sql = format!(
        "SELECT {COLUMNS} FROM cancel_requests \
         WHERE state = 'pending' ORDER BY id ASC"
    );
    let mut stmt = conn.prepare(&sql)?;
    let rows = stmt.query_map([], map_record)?;
    let mut out = Vec::new();
    for row in rows {
        out.push(row?);
    }
    Ok(out)
}

fn get_in(conn: &Connection, id: CancelRequestId) -> StateResult<Option<CancelRequestRecord>> {
    let sql = format!("SELECT {COLUMNS} FROM cancel_requests WHERE id = ?1");
    let mut stmt = conn.prepare(&sql)?;
    let row = stmt.query_row(params![id.0], map_record).optional()?;
    Ok(row)
}

impl CancelRequestRepository for StateDb {
    fn enqueue_cancel_request(&mut self, request: &CancelRequest, now: &str) -> StateResult<bool> {
        enqueue_in(self.conn(), request, now)
    }

    fn drain_cancel_request_for_run(
        &mut self,
        run_id: RunRef,
        now: &str,
    ) -> StateResult<Option<CancelRequestRecord>> {
        drain_in(self.conn(), "run", run_id.get(), now)
    }

    fn drain_cancel_request_for_work_item(
        &mut self,
        work_item_id: CoreWorkItemId,
        now: &str,
    ) -> StateResult<Option<CancelRequestRecord>> {
        drain_in(self.conn(), "work_item", work_item_id.get(), now)
    }

    fn list_pending_cancel_requests(&self) -> StateResult<Vec<CancelRequestRecord>> {
        list_pending_in(self.conn())
    }

    fn get_cancel_request(&self, id: CancelRequestId) -> StateResult<Option<CancelRequestRecord>> {
        get_in(self.conn(), id)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::migrations::migrations;

    fn open() -> StateDb {
        let mut db = StateDb::open_in_memory().expect("open");
        db.migrate(migrations()).expect("migrate");
        db
    }

    fn req_for_run(id: i64, reason: &str, who: &str) -> CancelRequest {
        CancelRequest::for_run(RunRef::new(id), reason, who, "2026-05-09T00:00:00Z")
    }

    fn req_for_work_item(id: i64, reason: &str, who: &str) -> CancelRequest {
        CancelRequest::for_work_item(CoreWorkItemId::new(id), reason, who, "2026-05-09T00:00:00Z")
    }

    #[test]
    fn enqueue_inserts_a_pending_row_and_round_trips() {
        let mut db = open();
        let req = req_for_run(7, "operator cancel", "tester");
        let inserted = db
            .enqueue_cancel_request(&req, "2026-05-09T00:00:01Z")
            .expect("enqueue");
        assert!(inserted, "first enqueue must report inserted = true");

        let pending = db.list_pending_cancel_requests().expect("list");
        assert_eq!(pending.len(), 1);
        assert_eq!(pending[0].request, req);
        assert_eq!(pending[0].state, CancelRequestState::Pending);
        assert_eq!(pending[0].created_at, "2026-05-09T00:00:01Z");
        assert!(pending[0].drained_at.is_none());
    }

    #[test]
    fn re_enqueue_same_subject_replaces_payload_in_place() {
        let mut db = open();
        let first = req_for_run(7, "first", "alice");
        let second = req_for_run(7, "second", "bob");
        assert!(
            db.enqueue_cancel_request(&first, "2026-05-09T00:00:01Z")
                .expect("first"),
        );
        assert!(
            !db.enqueue_cancel_request(&second, "2026-05-09T00:00:02Z")
                .expect("replace"),
            "second enqueue against same subject must report inserted = false"
        );

        let pending = db.list_pending_cancel_requests().expect("list");
        assert_eq!(
            pending.len(),
            1,
            "partial unique index must collapse to one row"
        );
        assert_eq!(pending[0].request.reason, "second");
        assert_eq!(pending[0].request.requested_by, "bob");
        // created_at must be preserved on replace; only the payload is updated.
        assert_eq!(pending[0].created_at, "2026-05-09T00:00:01Z");
    }

    #[test]
    fn run_and_work_item_subjects_share_numeric_id_without_collision() {
        let mut db = open();
        assert!(
            db.enqueue_cancel_request(&req_for_run(42, "run", "a"), "2026-05-09T00:00:01Z")
                .expect("run")
        );
        assert!(
            db.enqueue_cancel_request(&req_for_work_item(42, "wi", "a"), "2026-05-09T00:00:02Z",)
                .expect("wi"),
            "work-item subject must not collide with run subject"
        );
        let pending = db.list_pending_cancel_requests().expect("list");
        assert_eq!(pending.len(), 2);
    }

    #[test]
    fn drain_for_run_marks_state_drained_and_releases_pending_slot() {
        let mut db = open();
        db.enqueue_cancel_request(&req_for_run(7, "first", "a"), "2026-05-09T00:00:01Z")
            .expect("enqueue");

        let drained = db
            .drain_cancel_request_for_run(RunRef::new(7), "2026-05-09T00:00:05Z")
            .expect("drain")
            .expect("row exists");
        assert_eq!(drained.request.reason, "first");
        assert_eq!(
            drained.state,
            CancelRequestState::Pending,
            "drain returns the pre-transition snapshot"
        );

        let pending = db.list_pending_cancel_requests().expect("list");
        assert!(
            pending.is_empty(),
            "drained row must not be listed as pending"
        );

        // The audit row survives with state = 'drained' and a drained_at.
        let audit = db
            .get_cancel_request(drained.id)
            .expect("get")
            .expect("row");
        assert_eq!(audit.state, CancelRequestState::Drained);
        assert_eq!(audit.drained_at.as_deref(), Some("2026-05-09T00:00:05Z"));

        // The partial unique index frees the slot — a fresh cancel for
        // the same subject inserts a new pending row, not an upsert.
        let inserted = db
            .enqueue_cancel_request(&req_for_run(7, "second", "b"), "2026-05-09T00:00:06Z")
            .expect("re-enqueue");
        assert!(inserted, "post-drain enqueue must insert a fresh row");
        let pending = db.list_pending_cancel_requests().expect("list");
        assert_eq!(pending.len(), 1);
        assert_eq!(pending[0].request.reason, "second");
    }

    #[test]
    fn drain_for_run_does_not_affect_work_item_pending_slot() {
        let mut db = open();
        db.enqueue_cancel_request(&req_for_run(1, "run", "a"), "2026-05-09T00:00:01Z")
            .expect("run");
        db.enqueue_cancel_request(&req_for_work_item(1, "wi", "a"), "2026-05-09T00:00:02Z")
            .expect("wi");

        db.drain_cancel_request_for_run(RunRef::new(1), "2026-05-09T00:00:05Z")
            .expect("drain run")
            .expect("row exists");

        let pending = db.list_pending_cancel_requests().expect("list");
        assert_eq!(pending.len(), 1);
        assert!(matches!(
            pending[0].request.subject,
            CancelSubject::WorkItem { .. }
        ));
    }

    #[test]
    fn drain_for_missing_subject_returns_none() {
        let mut db = open();
        let drained = db
            .drain_cancel_request_for_run(RunRef::new(99), "2026-05-09T00:00:01Z")
            .expect("drain");
        assert!(drained.is_none());
        let drained = db
            .drain_cancel_request_for_work_item(CoreWorkItemId::new(99), "2026-05-09T00:00:01Z")
            .expect("drain");
        assert!(drained.is_none());
    }

    #[test]
    fn list_pending_orders_by_insertion_id() {
        let mut db = open();
        db.enqueue_cancel_request(&req_for_run(1, "a", "x"), "2026-05-09T00:00:01Z")
            .expect("a");
        db.enqueue_cancel_request(&req_for_work_item(2, "b", "x"), "2026-05-09T00:00:02Z")
            .expect("b");
        db.enqueue_cancel_request(&req_for_run(3, "c", "x"), "2026-05-09T00:00:03Z")
            .expect("c");

        let reasons: Vec<String> = db
            .list_pending_cancel_requests()
            .expect("list")
            .into_iter()
            .map(|r| r.request.reason)
            .collect();
        assert_eq!(reasons, vec!["a", "b", "c"]);
    }

    #[test]
    fn list_pending_skips_drained_rows() {
        let mut db = open();
        db.enqueue_cancel_request(&req_for_run(1, "a", "x"), "2026-05-09T00:00:01Z")
            .expect("a");
        db.enqueue_cancel_request(&req_for_run(2, "b", "x"), "2026-05-09T00:00:02Z")
            .expect("b");
        db.drain_cancel_request_for_run(RunRef::new(1), "2026-05-09T00:00:05Z")
            .expect("drain")
            .expect("row");

        let pending = db.list_pending_cancel_requests().expect("list");
        assert_eq!(pending.len(), 1);
        assert!(
            matches!(pending[0].request.subject, CancelSubject::Run { run_id } if run_id == RunRef::new(2))
        );
    }

    #[test]
    fn pending_rows_survive_reopen_on_disk() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("state.sqlite3");

        {
            let mut db = StateDb::open(&path).expect("open");
            db.migrate(migrations()).expect("migrate");
            db.enqueue_cancel_request(&req_for_run(7, "operator", "alice"), "2026-05-09T00:00:01Z")
                .expect("enqueue");
            db.enqueue_cancel_request(
                &req_for_work_item(11, "cascade", "alice"),
                "2026-05-09T00:00:02Z",
            )
            .expect("enqueue wi");
        }

        let db = StateDb::open(&path).expect("reopen");
        let pending = db.list_pending_cancel_requests().expect("list");
        assert_eq!(pending.len(), 2);

        // Reconstruct the in-memory queue from durable rows and verify
        // both subjects round-trip through the hydration constructor.
        let queue = symphony_core::cancellation::CancellationQueue::from_pending(
            pending.iter().map(|r| r.request.clone()),
        );
        assert!(queue.pending_for_run(RunRef::new(7)).is_some());
        assert!(
            queue
                .pending_for_work_item(CoreWorkItemId::new(11))
                .is_some()
        );
        assert_eq!(queue.len(), 2);
    }
}
