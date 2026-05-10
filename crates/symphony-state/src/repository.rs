//! Repository traits and SQLite implementations for v2 durable state.
//!
//! Phase 2 of the v2 checklist isolates persistence behind a small set of
//! synchronous repository traits so the kernel and scheduler can mock the
//! storage edge in unit tests. Concrete implementations live on
//! [`StateDb`] and use `rusqlite` directly; richer domain types
//! (`WorkItemStatusClass`, `RoleKind`, …) land in `symphony-core` during
//! Phase 3 and will wrap these stringly-typed records.
//!
//! Two design choices worth flagging:
//!
//! * Records carry timestamps as RFC3339 strings, matching what the
//!   migration in [`crate::migrations`] writes. Promoting these to typed
//!   `OffsetDateTime` values is a Phase 3 concern — doing it here would
//!   force `time`/`chrono` into the storage crate before we have a
//!   real consumer.
//! * Status fields (`status_class`, `tracker_status`, run `status`) are
//!   strings at this layer. The schema does not constrain them with a
//!   `CHECK`, deliberately — Phase 3 introduces `WorkItemStatusClass`
//!   and the kernel will validate before insert.

use rusqlite::{Connection, OptionalExtension, params};

use crate::{StateDb, StateResult};

/// Strongly-typed primary key for `work_items`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct WorkItemId(pub i64);

/// Strongly-typed primary key for `runs`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct RunId(pub i64);

/// A `work_items` row as stored.
///
/// JSON columns (`workspace_policy`, `branch_policy`) are returned as
/// raw strings so Phase 3 domain types can deserialize them without the
/// storage crate taking a hard dep on `serde_json` for round-tripping.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WorkItemRecord {
    /// Primary key.
    pub id: WorkItemId,
    /// Tracker adapter identifier (e.g. `github`, `linear`).
    pub tracker_id: String,
    /// Tracker-scoped human identifier (e.g. `OWNER/REPO#42`, `ENG-101`).
    pub identifier: String,
    /// Optional decomposition parent.
    pub parent_id: Option<WorkItemId>,
    /// Tracker-provided title.
    pub title: String,
    /// Normalized status class (Phase 3 will type this).
    pub status_class: String,
    /// Raw tracker status string at last sync.
    pub tracker_status: String,
    /// Role assignment, if routed.
    pub assigned_role: Option<String>,
    /// Concrete agent assignment, if dispatched.
    pub assigned_agent: Option<String>,
    /// Tracker-provided priority, if any.
    pub priority: Option<String>,
    /// Workspace policy snapshot (JSON string), if pinned at intake.
    pub workspace_policy: Option<String>,
    /// Branch policy snapshot (JSON string), if pinned at intake.
    pub branch_policy: Option<String>,
    /// RFC3339 created timestamp.
    pub created_at: String,
    /// RFC3339 updated timestamp.
    pub updated_at: String,
}

/// Insertion payload for [`WorkItemRepository::create_work_item`].
///
/// `created_at`/`updated_at` are caller-supplied so the kernel can keep
/// transactional state transitions consistent with event timestamps.
#[derive(Debug, Clone)]
pub struct NewWorkItem<'a> {
    /// Tracker adapter identifier.
    pub tracker_id: &'a str,
    /// Tracker-scoped human identifier.
    pub identifier: &'a str,
    /// Optional decomposition parent.
    pub parent_id: Option<WorkItemId>,
    /// Tracker-provided title.
    pub title: &'a str,
    /// Normalized status class.
    pub status_class: &'a str,
    /// Raw tracker status string.
    pub tracker_status: &'a str,
    /// Optional initial role assignment.
    pub assigned_role: Option<&'a str>,
    /// Optional initial agent assignment.
    pub assigned_agent: Option<&'a str>,
    /// Tracker priority, if any.
    pub priority: Option<&'a str>,
    /// Workspace policy snapshot (already-encoded JSON).
    pub workspace_policy: Option<&'a str>,
    /// Branch policy snapshot (already-encoded JSON).
    pub branch_policy: Option<&'a str>,
    /// RFC3339 timestamp written to both `created_at` and `updated_at`.
    pub now: &'a str,
}

/// A `runs` row as stored.
#[derive(Debug, Clone, PartialEq)]
pub struct RunRecord {
    /// Primary key.
    pub id: RunId,
    /// Owning work item.
    pub work_item_id: WorkItemId,
    /// Logical role (e.g. `platform_lead`, `qa`, `specialist:ui`).
    pub role: String,
    /// Concrete agent profile name.
    pub agent: String,
    /// Run lifecycle status (Phase 3 will type this).
    pub status: String,
    /// Foreign key into `workspace_claims`, if claimed.
    pub workspace_claim_id: Option<i64>,
    /// Lease holder identifier; populated while the run is in flight.
    pub lease_owner: Option<String>,
    /// RFC3339 expiration of the current lease, if held.
    pub lease_expires_at: Option<String>,
    /// RFC3339 actual run start.
    pub started_at: Option<String>,
    /// RFC3339 actual run end.
    pub ended_at: Option<String>,
    /// Cost in dollars, if reported.
    pub cost: Option<f64>,
    /// Token count, if reported.
    pub tokens: Option<i64>,
    /// Short human-readable result summary.
    pub result_summary: Option<String>,
    /// Terminal error message, if the run failed.
    pub error: Option<String>,
    /// RFC3339 row creation timestamp.
    pub created_at: String,
}

/// Insertion payload for [`RunRepository::create_run`].
#[derive(Debug, Clone)]
pub struct NewRun<'a> {
    /// Owning work item.
    pub work_item_id: WorkItemId,
    /// Logical role name.
    pub role: &'a str,
    /// Concrete agent profile name.
    pub agent: &'a str,
    /// Initial run status (typically `queued`).
    pub status: &'a str,
    /// Optional pre-claimed workspace.
    pub workspace_claim_id: Option<i64>,
    /// RFC3339 row creation timestamp.
    pub now: &'a str,
}

/// CRUD over `work_items`.
///
/// Lookups return [`Option`] so callers can distinguish "not found" from
/// transport errors without resorting to `find_or_create`-style helpers.
pub trait WorkItemRepository {
    /// Insert a new work item and return its persisted form.
    ///
    /// Honors the `(tracker_id, identifier)` UNIQUE constraint — duplicate
    /// inserts surface as [`crate::StateError::Sqlite`].
    fn create_work_item(&mut self, new: NewWorkItem<'_>) -> StateResult<WorkItemRecord>;

    /// Fetch a single work item by primary key.
    fn get_work_item(&self, id: WorkItemId) -> StateResult<Option<WorkItemRecord>>;

    /// Fetch by the natural `(tracker_id, identifier)` key.
    fn find_work_item_by_identifier(
        &self,
        tracker_id: &str,
        identifier: &str,
    ) -> StateResult<Option<WorkItemRecord>>;

    /// Update the cached status class and raw tracker status, bumping
    /// `updated_at`. Returns `Ok(false)` if the row does not exist; the
    /// caller decides whether that is a bug or an expected race.
    fn update_work_item_status(
        &mut self,
        id: WorkItemId,
        status_class: &str,
        tracker_status: &str,
        now: &str,
    ) -> StateResult<bool>;

    /// List children attached to a parent via the `parent_id` column.
    /// Edge-based decomposition (`work_item_edges`) lands in Phase 5.
    fn list_children(&self, parent: WorkItemId) -> StateResult<Vec<WorkItemRecord>>;
}

/// CRUD over `runs`.
pub trait RunRepository {
    /// Insert a new run and return its persisted form.
    fn create_run(&mut self, new: NewRun<'_>) -> StateResult<RunRecord>;

    /// Fetch a single run by primary key.
    fn get_run(&self, id: RunId) -> StateResult<Option<RunRecord>>;

    /// Update only the run's lifecycle status. Returns `Ok(false)` if the
    /// row does not exist.
    fn update_run_status(&mut self, id: RunId, status: &str) -> StateResult<bool>;

    /// List runs for a work item, ordered by `id` ascending (which is
    /// also creation order under SQLite's autoincrement).
    fn list_runs_for_work_item(&self, work_item_id: WorkItemId) -> StateResult<Vec<RunRecord>>;

    /// Return runs whose durable lease has expired as of `now`.
    ///
    /// A run is considered to hold a lease iff `lease_expires_at IS NOT
    /// NULL`; when the run reaches a terminal status the kernel is
    /// expected to clear the lease columns. This query therefore returns
    /// runs that were in flight when the previous orchestrator process
    /// died — exactly the rows the recovery scheduler must pick up
    /// (ARCHITECTURE_v2.md §7.2: "On restart, expired leases become
    /// recoverable work").
    ///
    /// `now` is an RFC3339 timestamp string compared lexicographically.
    /// Lex order matches chronological order only when callers use a
    /// consistent format (same fractional-second precision, `Z` suffix);
    /// the kernel produces timestamps from a single source so this holds
    /// in practice. Results are ordered by `lease_expires_at ASC` so the
    /// oldest stale lease is reclaimed first.
    fn find_expired_leases(&self, now: &str) -> StateResult<Vec<RunRecord>>;

    /// Atomically claim the durable lease on a run.
    ///
    /// The acquisition succeeds when, at the moment of the conditional
    /// UPDATE, one of the following holds:
    ///
    /// * The run has no lease (`lease_owner IS NULL`).
    /// * `owner` already holds the lease (idempotent refresh / extension).
    /// * The previous lease has strictly expired (`lease_expires_at <
    ///   now`). The boundary is the same `<` used by
    ///   [`Self::find_expired_leases`]: a lease that just touched its
    ///   deadline is not yet expired and cannot be taken over.
    ///
    /// On contention the existing holder + expiration are returned so the
    /// scheduler can park or reroute the dispatch request without losing
    /// it (see `CHECKLIST_v2.md` Phase 11 lease subtasks).
    ///
    /// `expires_at` and `now` are RFC3339 strings; `now` is provided
    /// explicitly rather than read from `CURRENT_TIMESTAMP` so kernel
    /// callers can keep clock semantics consistent with composed event
    /// timestamps inside [`crate::transaction::StateTransaction`].
    fn acquire_lease(
        &mut self,
        run_id: RunId,
        owner: &str,
        expires_at: &str,
        now: &str,
    ) -> StateResult<LeaseAcquisition>;

    /// Clear the lease columns on a run.
    ///
    /// Returns `Ok(true)` when the row exists (regardless of whether it
    /// previously held a lease — clearing an already-cleared lease is a
    /// no-op success), and `Ok(false)` when the run row is missing. The
    /// kernel is expected to call this exactly once per run, on terminal
    /// completion (success, failure, or cancellation), to keep
    /// `find_expired_leases` from surfacing finished work.
    fn release_lease(&mut self, run_id: RunId) -> StateResult<bool>;
}

/// Outcome of [`RunRepository::acquire_lease`].
///
/// Modeled as an enum rather than a `Result<bool, _>` so the scheduler
/// can branch on contention details (who holds it, until when) without a
/// follow-up query — and so future variants (e.g. `Stale` for explicit
/// takeover telemetry) can be added without breaking callers.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum LeaseAcquisition {
    /// Lease successfully acquired (or refreshed by the same owner).
    Acquired,
    /// Another owner holds an unexpired lease; the call had no effect.
    Contended {
        /// The current `lease_owner` value at the time the conditional
        /// UPDATE rejected the acquisition.
        holder: String,
        /// The current `lease_expires_at` value (RFC3339 string).
        expires_at: String,
    },
    /// The run row does not exist.
    NotFound,
}

pub(crate) const WORK_ITEM_COLUMNS: &str = "id, tracker_id, identifier, parent_id, title, \
     status_class, tracker_status, assigned_role, assigned_agent, priority, \
     workspace_policy, branch_policy, created_at, updated_at";

const RUN_COLUMNS: &str = "id, work_item_id, role, agent, status, workspace_claim_id, \
     lease_owner, lease_expires_at, started_at, ended_at, cost, tokens, \
     result_summary, error, created_at";

pub(crate) fn map_work_item(row: &rusqlite::Row<'_>) -> rusqlite::Result<WorkItemRecord> {
    Ok(WorkItemRecord {
        id: WorkItemId(row.get(0)?),
        tracker_id: row.get(1)?,
        identifier: row.get(2)?,
        parent_id: row.get::<_, Option<i64>>(3)?.map(WorkItemId),
        title: row.get(4)?,
        status_class: row.get(5)?,
        tracker_status: row.get(6)?,
        assigned_role: row.get(7)?,
        assigned_agent: row.get(8)?,
        priority: row.get(9)?,
        workspace_policy: row.get(10)?,
        branch_policy: row.get(11)?,
        created_at: row.get(12)?,
        updated_at: row.get(13)?,
    })
}

fn map_run(row: &rusqlite::Row<'_>) -> rusqlite::Result<RunRecord> {
    Ok(RunRecord {
        id: RunId(row.get(0)?),
        work_item_id: WorkItemId(row.get(1)?),
        role: row.get(2)?,
        agent: row.get(3)?,
        status: row.get(4)?,
        workspace_claim_id: row.get(5)?,
        lease_owner: row.get(6)?,
        lease_expires_at: row.get(7)?,
        started_at: row.get(8)?,
        ended_at: row.get(9)?,
        cost: row.get(10)?,
        tokens: row.get(11)?,
        result_summary: row.get(12)?,
        error: row.get(13)?,
        created_at: row.get(14)?,
    })
}

// Free helpers parameterized on `&Connection` so both the public
// `StateDb` repository impls and the transactional wrapper in
// `crate::transaction` execute the same SQL without duplication.
// `rusqlite::Transaction` derefs to `Connection`, which makes these
// callable from inside an open transaction transparently.

pub(crate) fn create_work_item_in(
    conn: &Connection,
    new: NewWorkItem<'_>,
) -> StateResult<WorkItemRecord> {
    conn.execute(
        "INSERT INTO work_items \
            (tracker_id, identifier, parent_id, title, status_class, \
             tracker_status, assigned_role, assigned_agent, priority, \
             workspace_policy, branch_policy, created_at, updated_at) \
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?12)",
        params![
            new.tracker_id,
            new.identifier,
            new.parent_id.map(|id| id.0),
            new.title,
            new.status_class,
            new.tracker_status,
            new.assigned_role,
            new.assigned_agent,
            new.priority,
            new.workspace_policy,
            new.branch_policy,
            new.now,
        ],
    )?;
    let id = WorkItemId(conn.last_insert_rowid());
    Ok(get_work_item_in(conn, id)?.expect("freshly inserted work item must be readable"))
}

pub(crate) fn get_work_item_in(
    conn: &Connection,
    id: WorkItemId,
) -> StateResult<Option<WorkItemRecord>> {
    let sql = format!("SELECT {WORK_ITEM_COLUMNS} FROM work_items WHERE id = ?1");
    let mut stmt = conn.prepare(&sql)?;
    let row = stmt.query_row(params![id.0], map_work_item).optional()?;
    Ok(row)
}

pub(crate) fn find_work_item_by_identifier_in(
    conn: &Connection,
    tracker_id: &str,
    identifier: &str,
) -> StateResult<Option<WorkItemRecord>> {
    let sql = format!(
        "SELECT {WORK_ITEM_COLUMNS} FROM work_items \
         WHERE tracker_id = ?1 AND identifier = ?2"
    );
    let mut stmt = conn.prepare(&sql)?;
    let row = stmt
        .query_row(params![tracker_id, identifier], map_work_item)
        .optional()?;
    Ok(row)
}

pub(crate) fn update_work_item_status_in(
    conn: &Connection,
    id: WorkItemId,
    status_class: &str,
    tracker_status: &str,
    now: &str,
) -> StateResult<bool> {
    let updated = conn.execute(
        "UPDATE work_items \
         SET status_class = ?2, tracker_status = ?3, updated_at = ?4 \
         WHERE id = ?1",
        params![id.0, status_class, tracker_status, now],
    )?;
    Ok(updated == 1)
}

pub(crate) fn list_children_in(
    conn: &Connection,
    parent: WorkItemId,
) -> StateResult<Vec<WorkItemRecord>> {
    let sql = format!(
        "SELECT {WORK_ITEM_COLUMNS} FROM work_items \
         WHERE parent_id = ?1 ORDER BY id ASC"
    );
    let mut stmt = conn.prepare(&sql)?;
    let rows = stmt.query_map(params![parent.0], map_work_item)?;
    let mut out = Vec::new();
    for row in rows {
        out.push(row?);
    }
    Ok(out)
}

pub(crate) fn create_run_in(conn: &Connection, new: NewRun<'_>) -> StateResult<RunRecord> {
    conn.execute(
        "INSERT INTO runs \
            (work_item_id, role, agent, status, workspace_claim_id, created_at) \
         VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
        params![
            new.work_item_id.0,
            new.role,
            new.agent,
            new.status,
            new.workspace_claim_id,
            new.now,
        ],
    )?;
    let id = RunId(conn.last_insert_rowid());
    Ok(get_run_in(conn, id)?.expect("freshly inserted run must be readable"))
}

pub(crate) fn get_run_in(conn: &Connection, id: RunId) -> StateResult<Option<RunRecord>> {
    let sql = format!("SELECT {RUN_COLUMNS} FROM runs WHERE id = ?1");
    let mut stmt = conn.prepare(&sql)?;
    let row = stmt.query_row(params![id.0], map_run).optional()?;
    Ok(row)
}

pub(crate) fn update_run_status_in(
    conn: &Connection,
    id: RunId,
    status: &str,
) -> StateResult<bool> {
    let updated = conn.execute(
        "UPDATE runs SET status = ?2 WHERE id = ?1",
        params![id.0, status],
    )?;
    Ok(updated == 1)
}

pub(crate) fn find_expired_leases_in(conn: &Connection, now: &str) -> StateResult<Vec<RunRecord>> {
    let sql = format!(
        "SELECT {RUN_COLUMNS} FROM runs \
         WHERE lease_expires_at IS NOT NULL AND lease_expires_at < ?1 \
         ORDER BY lease_expires_at ASC, id ASC"
    );
    let mut stmt = conn.prepare(&sql)?;
    let rows = stmt.query_map(params![now], map_run)?;
    let mut out = Vec::new();
    for row in rows {
        out.push(row?);
    }
    Ok(out)
}

pub(crate) fn acquire_lease_in(
    conn: &Connection,
    run_id: RunId,
    owner: &str,
    expires_at: &str,
    now: &str,
) -> StateResult<LeaseAcquisition> {
    // Conditional UPDATE: succeeds iff the lease is vacant, already
    // ours, or strictly expired. SQLite serializes writers, so this is
    // atomic with respect to other connections.
    let updated = conn.execute(
        "UPDATE runs \
         SET lease_owner = ?2, lease_expires_at = ?3 \
         WHERE id = ?1 \
           AND ( lease_owner IS NULL \
              OR lease_owner = ?2 \
              OR lease_expires_at < ?4 )",
        params![run_id.0, owner, expires_at, now],
    )?;
    if updated == 1 {
        return Ok(LeaseAcquisition::Acquired);
    }

    // Diagnose: missing row vs. live contention. The follow-up SELECT
    // can theoretically race with another writer that takes over after
    // our failed UPDATE, but reporting the most recently observed holder
    // is acceptable — the caller treats both Contended outcomes the
    // same (park the dispatch).
    let row: Option<(Option<String>, Option<String>)> = conn
        .query_row(
            "SELECT lease_owner, lease_expires_at FROM runs WHERE id = ?1",
            params![run_id.0],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )
        .optional()?;
    match row {
        None => Ok(LeaseAcquisition::NotFound),
        Some((Some(holder), Some(expires))) => Ok(LeaseAcquisition::Contended {
            holder,
            expires_at: expires,
        }),
        // The row exists but the lease columns are NULL or partially
        // NULL — that should have satisfied the UPDATE predicate. The
        // only realistic way to reach this branch is a concurrent
        // release between the UPDATE and the SELECT; reporting it as
        // contention with an empty holder would be wrong, so surface
        // the same "vacant" view the next acquire attempt would see.
        Some(_) => Ok(LeaseAcquisition::Contended {
            holder: String::new(),
            expires_at: String::new(),
        }),
    }
}

pub(crate) fn release_lease_in(conn: &Connection, run_id: RunId) -> StateResult<bool> {
    let updated = conn.execute(
        "UPDATE runs SET lease_owner = NULL, lease_expires_at = NULL WHERE id = ?1",
        params![run_id.0],
    )?;
    Ok(updated == 1)
}

pub(crate) fn list_runs_for_work_item_in(
    conn: &Connection,
    work_item_id: WorkItemId,
) -> StateResult<Vec<RunRecord>> {
    let sql = format!(
        "SELECT {RUN_COLUMNS} FROM runs \
         WHERE work_item_id = ?1 ORDER BY id ASC"
    );
    let mut stmt = conn.prepare(&sql)?;
    let rows = stmt.query_map(params![work_item_id.0], map_run)?;
    let mut out = Vec::new();
    for row in rows {
        out.push(row?);
    }
    Ok(out)
}

impl WorkItemRepository for StateDb {
    fn create_work_item(&mut self, new: NewWorkItem<'_>) -> StateResult<WorkItemRecord> {
        create_work_item_in(self.conn(), new)
    }

    fn get_work_item(&self, id: WorkItemId) -> StateResult<Option<WorkItemRecord>> {
        get_work_item_in(self.conn(), id)
    }

    fn find_work_item_by_identifier(
        &self,
        tracker_id: &str,
        identifier: &str,
    ) -> StateResult<Option<WorkItemRecord>> {
        find_work_item_by_identifier_in(self.conn(), tracker_id, identifier)
    }

    fn update_work_item_status(
        &mut self,
        id: WorkItemId,
        status_class: &str,
        tracker_status: &str,
        now: &str,
    ) -> StateResult<bool> {
        update_work_item_status_in(self.conn(), id, status_class, tracker_status, now)
    }

    fn list_children(&self, parent: WorkItemId) -> StateResult<Vec<WorkItemRecord>> {
        list_children_in(self.conn(), parent)
    }
}

impl RunRepository for StateDb {
    fn create_run(&mut self, new: NewRun<'_>) -> StateResult<RunRecord> {
        create_run_in(self.conn(), new)
    }

    fn get_run(&self, id: RunId) -> StateResult<Option<RunRecord>> {
        get_run_in(self.conn(), id)
    }

    fn update_run_status(&mut self, id: RunId, status: &str) -> StateResult<bool> {
        update_run_status_in(self.conn(), id, status)
    }

    fn list_runs_for_work_item(&self, work_item_id: WorkItemId) -> StateResult<Vec<RunRecord>> {
        list_runs_for_work_item_in(self.conn(), work_item_id)
    }

    fn find_expired_leases(&self, now: &str) -> StateResult<Vec<RunRecord>> {
        find_expired_leases_in(self.conn(), now)
    }

    fn acquire_lease(
        &mut self,
        run_id: RunId,
        owner: &str,
        expires_at: &str,
        now: &str,
    ) -> StateResult<LeaseAcquisition> {
        acquire_lease_in(self.conn(), run_id, owner, expires_at, now)
    }

    fn release_lease(&mut self, run_id: RunId) -> StateResult<bool> {
        release_lease_in(self.conn(), run_id)
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

    fn sample_work_item<'a>(identifier: &'a str, now: &'a str) -> NewWorkItem<'a> {
        NewWorkItem {
            tracker_id: "github",
            identifier,
            parent_id: None,
            title: "demo issue",
            status_class: "ready",
            tracker_status: "open",
            assigned_role: None,
            assigned_agent: None,
            priority: None,
            workspace_policy: None,
            branch_policy: None,
            now,
        }
    }

    #[test]
    fn create_then_get_round_trips_all_columns() {
        let mut db = open();
        let inserted = db
            .create_work_item(NewWorkItem {
                assigned_role: Some("platform_lead"),
                assigned_agent: Some("claude"),
                priority: Some("p1"),
                workspace_policy: Some(r#"{"strategy":"git_worktree"}"#),
                branch_policy: Some(r#"{"template":"sym/{id}"}"#),
                ..sample_work_item("OWNER/REPO#1", "2026-05-08T00:00:00Z")
            })
            .expect("create");

        let fetched = db
            .get_work_item(inserted.id)
            .expect("get")
            .expect("present");
        assert_eq!(fetched, inserted);
        assert_eq!(fetched.assigned_role.as_deref(), Some("platform_lead"));
        assert_eq!(
            fetched.workspace_policy.as_deref(),
            Some(r#"{"strategy":"git_worktree"}"#)
        );
        assert_eq!(fetched.created_at, fetched.updated_at);
    }

    #[test]
    fn find_by_identifier_returns_none_for_unknown() {
        let db = open();
        assert!(
            db.find_work_item_by_identifier("github", "OWNER/REPO#404")
                .expect("query")
                .is_none()
        );
    }

    #[test]
    fn duplicate_identifier_is_rejected() {
        let mut db = open();
        db.create_work_item(sample_work_item("OWNER/REPO#1", "2026-05-08T00:00:00Z"))
            .expect("first");
        let err = db
            .create_work_item(sample_work_item("OWNER/REPO#1", "2026-05-08T00:00:00Z"))
            .expect_err("duplicate must fail");
        assert!(matches!(err, crate::StateError::Sqlite(_)));
    }

    #[test]
    fn update_status_changes_only_intended_columns() {
        let mut db = open();
        let initial = db
            .create_work_item(sample_work_item("ENG-1", "2026-05-08T00:00:00Z"))
            .expect("create");
        let updated = db
            .update_work_item_status(initial.id, "in_progress", "started", "2026-05-08T01:00:00Z")
            .expect("update");
        assert!(updated);
        let after = db.get_work_item(initial.id).unwrap().unwrap();
        assert_eq!(after.status_class, "in_progress");
        assert_eq!(after.tracker_status, "started");
        assert_eq!(after.updated_at, "2026-05-08T01:00:00Z");
        assert_eq!(after.created_at, initial.created_at);
        assert_eq!(after.title, initial.title);
    }

    #[test]
    fn update_status_returns_false_for_missing_row() {
        let mut db = open();
        let updated = db
            .update_work_item_status(WorkItemId(999), "x", "y", "2026-05-08T00:00:00Z")
            .expect("update");
        assert!(!updated);
    }

    #[test]
    fn list_children_filters_by_parent_id() {
        let mut db = open();
        let parent = db
            .create_work_item(sample_work_item("ENG-1", "2026-05-08T00:00:00Z"))
            .expect("parent");
        let _unrelated = db
            .create_work_item(sample_work_item("ENG-2", "2026-05-08T00:00:00Z"))
            .expect("unrelated");
        let child_a = db
            .create_work_item(NewWorkItem {
                parent_id: Some(parent.id),
                ..sample_work_item("ENG-3", "2026-05-08T00:00:00Z")
            })
            .expect("child a");
        let child_b = db
            .create_work_item(NewWorkItem {
                parent_id: Some(parent.id),
                ..sample_work_item("ENG-4", "2026-05-08T00:00:00Z")
            })
            .expect("child b");

        let children = db.list_children(parent.id).expect("children");
        let ids: Vec<_> = children.iter().map(|c| c.id).collect();
        assert_eq!(ids, vec![child_a.id, child_b.id]);
    }

    #[test]
    fn create_run_round_trips() {
        let mut db = open();
        let work_item = db
            .create_work_item(sample_work_item("ENG-1", "2026-05-08T00:00:00Z"))
            .expect("work item");
        let run = db
            .create_run(NewRun {
                work_item_id: work_item.id,
                role: "platform_lead",
                agent: "claude",
                status: "queued",
                workspace_claim_id: None,
                now: "2026-05-08T00:00:00Z",
            })
            .expect("run");

        let fetched = db.get_run(run.id).unwrap().unwrap();
        assert_eq!(fetched, run);
        assert_eq!(fetched.work_item_id, work_item.id);
        assert_eq!(fetched.status, "queued");
        assert!(fetched.started_at.is_none());
    }

    #[test]
    fn create_run_round_trips_completed_status_label() {
        let mut db = open();
        let work_item = db
            .create_work_item(sample_work_item("ENG-1", "2026-05-08T00:00:00Z"))
            .expect("work item");
        let run = db
            .create_run(NewRun {
                work_item_id: work_item.id,
                role: "platform_lead",
                agent: "claude",
                status: "completed",
                workspace_claim_id: None,
                now: "2026-05-08T00:00:00Z",
            })
            .expect("run");

        let fetched = db.get_run(run.id).unwrap().unwrap();
        assert_eq!(fetched.status, "completed");
        assert_eq!(fetched, run);
    }

    #[test]
    fn create_run_rejects_orphan_work_item() {
        let mut db = open();
        let err = db
            .create_run(NewRun {
                work_item_id: WorkItemId(999),
                role: "qa",
                agent: "claude",
                status: "queued",
                workspace_claim_id: None,
                now: "2026-05-08T00:00:00Z",
            })
            .expect_err("orphan must fail");
        assert!(matches!(err, crate::StateError::Sqlite(_)));
    }

    #[test]
    fn update_run_status_transitions() {
        let mut db = open();
        let work_item = db
            .create_work_item(sample_work_item("ENG-1", "2026-05-08T00:00:00Z"))
            .expect("wi");
        let run = db
            .create_run(NewRun {
                work_item_id: work_item.id,
                role: "platform_lead",
                agent: "claude",
                status: "queued",
                workspace_claim_id: None,
                now: "2026-05-08T00:00:00Z",
            })
            .expect("run");
        assert!(db.update_run_status(run.id, "running").unwrap());
        assert_eq!(db.get_run(run.id).unwrap().unwrap().status, "running");
        assert!(!db.update_run_status(RunId(123_456), "done").unwrap());
    }

    fn set_lease(db: &StateDb, run_id: RunId, owner: Option<&str>, expires_at: Option<&str>) {
        db.conn()
            .execute(
                "UPDATE runs SET lease_owner = ?2, lease_expires_at = ?3 WHERE id = ?1",
                params![run_id.0, owner, expires_at],
            )
            .expect("set lease");
    }

    fn seed_run(db: &mut StateDb, identifier: &str) -> RunId {
        let wi = db
            .create_work_item(sample_work_item(identifier, "2026-05-08T00:00:00Z"))
            .expect("wi");
        db.create_run(NewRun {
            work_item_id: wi.id,
            role: "platform_lead",
            agent: "claude",
            status: "running",
            workspace_claim_id: None,
            now: "2026-05-08T00:00:00Z",
        })
        .expect("run")
        .id
    }

    #[test]
    fn find_expired_leases_returns_only_runs_past_now() {
        let mut db = open();
        let stale = seed_run(&mut db, "ENG-1");
        let fresh = seed_run(&mut db, "ENG-2");
        let cleared = seed_run(&mut db, "ENG-3");

        set_lease(&db, stale, Some("worker-a"), Some("2026-05-08T00:00:00Z"));
        set_lease(&db, fresh, Some("worker-b"), Some("2026-05-08T02:00:00Z"));
        // `cleared` keeps lease_expires_at = NULL — it must never appear.
        set_lease(&db, cleared, None, None);

        let expired = db
            .find_expired_leases("2026-05-08T01:00:00Z")
            .expect("query");
        let ids: Vec<_> = expired.iter().map(|r| r.id).collect();
        assert_eq!(ids, vec![stale]);
        assert_eq!(expired[0].lease_owner.as_deref(), Some("worker-a"));
    }

    #[test]
    fn find_expired_leases_orders_oldest_first() {
        let mut db = open();
        let mid = seed_run(&mut db, "ENG-1");
        let oldest = seed_run(&mut db, "ENG-2");
        let newest = seed_run(&mut db, "ENG-3");

        set_lease(&db, mid, Some("a"), Some("2026-05-08T01:00:00Z"));
        set_lease(&db, oldest, Some("b"), Some("2026-05-08T00:00:00Z"));
        set_lease(&db, newest, Some("c"), Some("2026-05-08T02:00:00Z"));

        let expired = db
            .find_expired_leases("2026-05-08T03:00:00Z")
            .expect("query");
        let ids: Vec<_> = expired.iter().map(|r| r.id).collect();
        assert_eq!(ids, vec![oldest, mid, newest]);
    }

    #[test]
    fn find_expired_leases_excludes_runs_with_no_lease() {
        let mut db = open();
        let _r = seed_run(&mut db, "ENG-1");
        let expired = db
            .find_expired_leases("2999-01-01T00:00:00Z")
            .expect("query");
        assert!(expired.is_empty());
    }

    #[test]
    fn find_expired_leases_boundary_is_strictly_less_than() {
        let mut db = open();
        let r = seed_run(&mut db, "ENG-1");
        set_lease(&db, r, Some("w"), Some("2026-05-08T00:00:00Z"));

        // Exact equality is not "expired" — the lease just reached its
        // deadline; the next tick will reclaim it.
        let at_boundary = db
            .find_expired_leases("2026-05-08T00:00:00Z")
            .expect("query");
        assert!(at_boundary.is_empty());

        let past = db
            .find_expired_leases("2026-05-08T00:00:01Z")
            .expect("query");
        assert_eq!(past.len(), 1);
    }

    fn read_lease(db: &StateDb, run_id: RunId) -> (Option<String>, Option<String>) {
        db.conn()
            .query_row(
                "SELECT lease_owner, lease_expires_at FROM runs WHERE id = ?1",
                params![run_id.0],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .expect("lease row")
    }

    #[test]
    fn acquire_lease_on_vacant_run_succeeds_and_writes_columns() {
        let mut db = open();
        let run = seed_run(&mut db, "ENG-1");
        let outcome = db
            .acquire_lease(
                run,
                "worker-a",
                "2026-05-08T01:00:00Z",
                "2026-05-08T00:00:00Z",
            )
            .expect("acquire");
        assert_eq!(outcome, LeaseAcquisition::Acquired);
        let (owner, expires) = read_lease(&db, run);
        assert_eq!(owner.as_deref(), Some("worker-a"));
        assert_eq!(expires.as_deref(), Some("2026-05-08T01:00:00Z"));
    }

    #[test]
    fn acquire_lease_is_idempotent_for_same_owner() {
        let mut db = open();
        let run = seed_run(&mut db, "ENG-1");
        db.acquire_lease(
            run,
            "worker-a",
            "2026-05-08T01:00:00Z",
            "2026-05-08T00:00:00Z",
        )
        .unwrap();
        // Same owner re-acquires while the lease is still live, with a
        // later expiration — this should refresh the deadline.
        let outcome = db
            .acquire_lease(
                run,
                "worker-a",
                "2026-05-08T02:00:00Z",
                "2026-05-08T00:30:00Z",
            )
            .expect("re-acquire");
        assert_eq!(outcome, LeaseAcquisition::Acquired);
        let (owner, expires) = read_lease(&db, run);
        assert_eq!(owner.as_deref(), Some("worker-a"));
        assert_eq!(expires.as_deref(), Some("2026-05-08T02:00:00Z"));
    }

    #[test]
    fn acquire_lease_rejects_other_owner_while_unexpired() {
        let mut db = open();
        let run = seed_run(&mut db, "ENG-1");
        db.acquire_lease(
            run,
            "worker-a",
            "2026-05-08T01:00:00Z",
            "2026-05-08T00:00:00Z",
        )
        .unwrap();
        let outcome = db
            .acquire_lease(
                run,
                "worker-b",
                "2026-05-08T01:30:00Z",
                "2026-05-08T00:30:00Z",
            )
            .expect("contend");
        assert_eq!(
            outcome,
            LeaseAcquisition::Contended {
                holder: "worker-a".into(),
                expires_at: "2026-05-08T01:00:00Z".into(),
            }
        );
        // Original lease is untouched.
        let (owner, expires) = read_lease(&db, run);
        assert_eq!(owner.as_deref(), Some("worker-a"));
        assert_eq!(expires.as_deref(), Some("2026-05-08T01:00:00Z"));
    }

    #[test]
    fn acquire_lease_takes_over_strictly_expired_lease() {
        let mut db = open();
        let run = seed_run(&mut db, "ENG-1");
        db.acquire_lease(
            run,
            "worker-a",
            "2026-05-08T01:00:00Z",
            "2026-05-08T00:00:00Z",
        )
        .unwrap();
        // `now` is strictly past the previous lease deadline.
        let outcome = db
            .acquire_lease(
                run,
                "worker-b",
                "2026-05-08T03:00:00Z",
                "2026-05-08T01:00:01Z",
            )
            .expect("takeover");
        assert_eq!(outcome, LeaseAcquisition::Acquired);
        let (owner, expires) = read_lease(&db, run);
        assert_eq!(owner.as_deref(), Some("worker-b"));
        assert_eq!(expires.as_deref(), Some("2026-05-08T03:00:00Z"));
    }

    #[test]
    fn acquire_lease_at_exact_expiration_is_contention() {
        let mut db = open();
        let run = seed_run(&mut db, "ENG-1");
        db.acquire_lease(
            run,
            "worker-a",
            "2026-05-08T01:00:00Z",
            "2026-05-08T00:00:00Z",
        )
        .unwrap();
        // Equality is "deadline reached" but not yet expired — matches
        // the boundary used by `find_expired_leases`.
        let outcome = db
            .acquire_lease(
                run,
                "worker-b",
                "2026-05-08T02:00:00Z",
                "2026-05-08T01:00:00Z",
            )
            .expect("boundary");
        assert!(matches!(outcome, LeaseAcquisition::Contended { .. }));
    }

    #[test]
    fn acquire_lease_on_missing_run_reports_not_found() {
        let mut db = open();
        let outcome = db
            .acquire_lease(
                RunId(999),
                "worker-a",
                "2026-05-08T01:00:00Z",
                "2026-05-08T00:00:00Z",
            )
            .expect("missing");
        assert_eq!(outcome, LeaseAcquisition::NotFound);
    }

    #[test]
    fn release_lease_clears_columns_and_returns_true() {
        let mut db = open();
        let run = seed_run(&mut db, "ENG-1");
        db.acquire_lease(
            run,
            "worker-a",
            "2026-05-08T01:00:00Z",
            "2026-05-08T00:00:00Z",
        )
        .unwrap();
        assert!(db.release_lease(run).expect("release"));
        let (owner, expires) = read_lease(&db, run);
        assert!(owner.is_none());
        assert!(expires.is_none());
        // After release, a different owner can acquire freely.
        let outcome = db
            .acquire_lease(
                run,
                "worker-b",
                "2026-05-08T02:00:00Z",
                "2026-05-08T00:30:00Z",
            )
            .unwrap();
        assert_eq!(outcome, LeaseAcquisition::Acquired);
    }

    #[test]
    fn release_lease_is_no_op_success_when_lease_already_clear() {
        let mut db = open();
        let run = seed_run(&mut db, "ENG-1");
        // No prior acquisition; release should still report the row
        // exists rather than failing.
        assert!(db.release_lease(run).expect("release vacant"));
    }

    #[test]
    fn release_lease_returns_false_for_missing_run() {
        let mut db = open();
        assert!(!db.release_lease(RunId(999)).expect("release missing"));
    }

    #[test]
    fn released_lease_is_no_longer_surfaced_by_find_expired_leases() {
        let mut db = open();
        let run = seed_run(&mut db, "ENG-1");
        db.acquire_lease(
            run,
            "worker-a",
            "2026-05-08T01:00:00Z",
            "2026-05-08T00:00:00Z",
        )
        .unwrap();
        // With a fresh `now` past expiration, the lease would otherwise
        // appear in `find_expired_leases` — releasing must remove it.
        db.release_lease(run).unwrap();
        let expired = db
            .find_expired_leases("2026-05-08T05:00:00Z")
            .expect("expired");
        assert!(expired.is_empty());
    }

    #[test]
    fn list_runs_for_work_item_returns_in_creation_order() {
        let mut db = open();
        let wi = db
            .create_work_item(sample_work_item("ENG-1", "2026-05-08T00:00:00Z"))
            .expect("wi");
        let other = db
            .create_work_item(sample_work_item("ENG-2", "2026-05-08T00:00:00Z"))
            .expect("other");

        let r1 = db
            .create_run(NewRun {
                work_item_id: wi.id,
                role: "platform_lead",
                agent: "claude",
                status: "queued",
                workspace_claim_id: None,
                now: "2026-05-08T00:00:00Z",
            })
            .unwrap();
        let _other_run = db
            .create_run(NewRun {
                work_item_id: other.id,
                role: "qa",
                agent: "claude",
                status: "queued",
                workspace_claim_id: None,
                now: "2026-05-08T00:00:00Z",
            })
            .unwrap();
        let r2 = db
            .create_run(NewRun {
                work_item_id: wi.id,
                role: "qa",
                agent: "claude",
                status: "queued",
                workspace_claim_id: None,
                now: "2026-05-08T00:00:00Z",
            })
            .unwrap();

        let runs = db.list_runs_for_work_item(wi.id).expect("runs");
        let ids: Vec<_> = runs.iter().map(|r| r.id).collect();
        assert_eq!(ids, vec![r1.id, r2.id]);
    }
}
