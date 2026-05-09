//! Persistence for the `integration_records` table (SPEC v2 §5.10, §6.4).
//!
//! An integration record is the durable proof that an integration owner
//! consolidated child work into a canonical branch/worktree. The kernel
//! reads it when it gates parent closeout (ARCH §6.2: "integration record
//! exists when required"), and QA reads it to know what artifact it is
//! verifying. Operators read it through `symphony status` /
//! `symphony issue graph` to describe the orchestrator's progress on a
//! decomposed parent.
//!
//! Two design choices worth flagging — both mirror the handoff repository
//! in [`crate::handoffs`]:
//!
//! * The storage layer keeps `status` and `merge_strategy` stringly-typed
//!   even though the migration adds SQL `CHECK` constraints. Round-tripping
//!   through the typed enums in `symphony-core::integration` would force a
//!   cross-crate dependency the storage crate does not need; the kernel
//!   already validates before insert.
//! * `children_consolidated`, `conflicts`, and `blockers_filed` are stored
//!   as already-encoded JSON strings. The canonical encoder lives in
//!   `symphony-core` so the storage crate stays free of `serde_json`.

use rusqlite::{Connection, OptionalExtension, params};

use crate::repository::{RunId, WorkItemId};
use crate::{StateDb, StateResult};

/// Strongly-typed primary key for `integration_records`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct IntegrationRecordId(pub i64);

/// An `integration_records` row as stored.
///
/// JSON columns surface as `Option<String>` so callers can decide whether
/// to deserialize them; the column is `NULL` when the integration owner
/// did not record the field, which keeps the storage payload symmetric
/// with the `IntegrationRecord` envelope shape in `symphony-core`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct IntegrationRecordRow {
    /// Primary key.
    pub id: IntegrationRecordId,
    /// Parent work item being integrated (typically a decomposed parent).
    pub parent_work_item_id: WorkItemId,
    /// Integration-owner role name (e.g. `platform_lead`).
    pub owner_role: String,
    /// Integration-owner run that produced or is producing the record.
    /// `None` is valid only for `pending` rows that have been queued but
    /// not yet started; the migration leaves the column nullable rather
    /// than encoding that rule with a `CHECK` so the constraint stays in
    /// the kernel where the rest of the lifecycle invariants live.
    pub run_id: Option<RunId>,
    /// Lifecycle status (`pending`, `in_progress`, `succeeded`, `failed`,
    /// `blocked`, `cancelled`). The migration enforces the variant set
    /// with a SQL `CHECK`.
    pub status: String,
    /// Merge strategy used (or planned). The migration enforces the
    /// variant set with a SQL `CHECK`.
    pub merge_strategy: String,
    /// Canonical integration branch ref, when known.
    pub integration_branch: Option<String>,
    /// Base ref the integration branch was cut from (e.g. `main`).
    pub base_ref: Option<String>,
    /// SHA of the canonical branch tip at record time, when known.
    pub head_sha: Option<String>,
    /// Absolute or workspace-relative path to the integration workspace,
    /// when the workflow uses a worktree strategy.
    pub workspace_path: Option<String>,
    /// JSON array of `WorkItemId` ids consolidated into the canonical
    /// branch. `NULL` when no children were recorded.
    pub children_consolidated: Option<String>,
    /// Operator-facing summary suitable for PR bodies and QA briefs.
    pub summary: Option<String>,
    /// JSON array of structured conflict evidence, if any.
    pub conflicts: Option<String>,
    /// JSON array of `BlockerId` ids filed alongside the integration
    /// attempt, if any.
    pub blockers_filed: Option<String>,
    /// RFC3339 row creation timestamp.
    pub created_at: String,
}

/// Insertion payload for [`IntegrationRecordRepository::create_integration_record`].
///
/// Mirrors the storage row exactly; the caller is responsible for encoding
/// any JSON fields before insert. This keeps the storage crate free of
/// `serde_json` and lets the kernel reuse the canonical
/// `IntegrationRecord` serializer in `symphony-core` without an extra hop.
#[derive(Debug, Clone)]
pub struct NewIntegrationRecord<'a> {
    /// Parent work item being integrated.
    pub parent_work_item_id: WorkItemId,
    /// Integration-owner role name.
    pub owner_role: &'a str,
    /// Integration-owner run, if started.
    pub run_id: Option<RunId>,
    /// Lifecycle status.
    pub status: &'a str,
    /// Merge strategy used.
    pub merge_strategy: &'a str,
    /// Canonical integration branch ref, if known.
    pub integration_branch: Option<&'a str>,
    /// Base ref the integration branch was cut from.
    pub base_ref: Option<&'a str>,
    /// SHA of the canonical branch tip at record time.
    pub head_sha: Option<&'a str>,
    /// Path to the integration workspace, if any.
    pub workspace_path: Option<&'a str>,
    /// Already-encoded JSON array of consolidated child work item ids.
    pub children_consolidated: Option<&'a str>,
    /// Operator-facing summary.
    pub summary: Option<&'a str>,
    /// Already-encoded JSON array of conflict evidence.
    pub conflicts: Option<&'a str>,
    /// Already-encoded JSON array of blocker ids filed.
    pub blockers_filed: Option<&'a str>,
    /// RFC3339 row creation timestamp.
    pub now: &'a str,
}

/// CRUD over `integration_records`.
///
/// Insertion is the hot path; reads exist for the parent-closeout gate
/// (ARCH §6.2), the QA brief (SPEC §5.12), and operator surfaces.
pub trait IntegrationRecordRepository {
    /// Insert a new integration record and return its persisted form.
    ///
    /// Foreign-key violations (unknown `parent_work_item_id` or `run_id`)
    /// surface as [`crate::StateError::Sqlite`]. SQL `CHECK` violations
    /// (unknown `status`/`merge_strategy`) likewise surface as
    /// [`crate::StateError::Sqlite`]; the storage layer trusts upstream
    /// validation for the JSON columns.
    fn create_integration_record(
        &mut self,
        new: NewIntegrationRecord<'_>,
    ) -> StateResult<IntegrationRecordRow>;

    /// Fetch a single integration record by primary key.
    fn get_integration_record(
        &self,
        id: IntegrationRecordId,
    ) -> StateResult<Option<IntegrationRecordRow>>;

    /// List every integration record filed against `parent_work_item_id`,
    /// oldest first.
    ///
    /// Order is `id ASC` — under SQLite's autoincrement that matches
    /// insertion order, which is also the order an operator wants to
    /// scan the audit trail (each repair turn appends a fresh row;
    /// terminal rows are durable evidence and are not mutated in place).
    fn list_integration_records_for_parent(
        &self,
        parent_work_item_id: WorkItemId,
    ) -> StateResult<Vec<IntegrationRecordRow>>;

    /// Return the most recent integration record filed against
    /// `parent_work_item_id`, or `None` if none exist.
    ///
    /// This is what the parent-closeout gate reads: the latest record
    /// must be `succeeded` (and carry an integration branch + summary)
    /// before the parent can close.
    fn latest_integration_record_for_parent(
        &self,
        parent_work_item_id: WorkItemId,
    ) -> StateResult<Option<IntegrationRecordRow>>;
}

const INTEGRATION_RECORD_COLUMNS: &str = "id, parent_work_item_id, owner_role, run_id, \
     status, merge_strategy, integration_branch, base_ref, head_sha, \
     workspace_path, children_consolidated, summary, conflicts, \
     blockers_filed, created_at";

fn map_integration_record(row: &rusqlite::Row<'_>) -> rusqlite::Result<IntegrationRecordRow> {
    Ok(IntegrationRecordRow {
        id: IntegrationRecordId(row.get(0)?),
        parent_work_item_id: WorkItemId(row.get(1)?),
        owner_role: row.get(2)?,
        run_id: row.get::<_, Option<i64>>(3)?.map(RunId),
        status: row.get(4)?,
        merge_strategy: row.get(5)?,
        integration_branch: row.get(6)?,
        base_ref: row.get(7)?,
        head_sha: row.get(8)?,
        workspace_path: row.get(9)?,
        children_consolidated: row.get(10)?,
        summary: row.get(11)?,
        conflicts: row.get(12)?,
        blockers_filed: row.get(13)?,
        created_at: row.get(14)?,
    })
}

pub(crate) fn create_integration_record_in(
    conn: &Connection,
    new: NewIntegrationRecord<'_>,
) -> StateResult<IntegrationRecordRow> {
    conn.execute(
        "INSERT INTO integration_records \
            (parent_work_item_id, owner_role, run_id, status, \
             merge_strategy, integration_branch, base_ref, head_sha, \
             workspace_path, children_consolidated, summary, conflicts, \
             blockers_filed, created_at) \
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14)",
        params![
            new.parent_work_item_id.0,
            new.owner_role,
            new.run_id.map(|r| r.0),
            new.status,
            new.merge_strategy,
            new.integration_branch,
            new.base_ref,
            new.head_sha,
            new.workspace_path,
            new.children_consolidated,
            new.summary,
            new.conflicts,
            new.blockers_filed,
            new.now,
        ],
    )?;
    let id = IntegrationRecordId(conn.last_insert_rowid());
    Ok(get_integration_record_in(conn, id)?
        .expect("freshly inserted integration record must be readable"))
}

pub(crate) fn get_integration_record_in(
    conn: &Connection,
    id: IntegrationRecordId,
) -> StateResult<Option<IntegrationRecordRow>> {
    let sql = format!("SELECT {INTEGRATION_RECORD_COLUMNS} FROM integration_records WHERE id = ?1");
    let mut stmt = conn.prepare(&sql)?;
    let row = stmt
        .query_row(params![id.0], map_integration_record)
        .optional()?;
    Ok(row)
}

pub(crate) fn list_integration_records_for_parent_in(
    conn: &Connection,
    parent_work_item_id: WorkItemId,
) -> StateResult<Vec<IntegrationRecordRow>> {
    let sql = format!(
        "SELECT {INTEGRATION_RECORD_COLUMNS} FROM integration_records \
         WHERE parent_work_item_id = ?1 ORDER BY id ASC"
    );
    let mut stmt = conn.prepare(&sql)?;
    let rows = stmt.query_map(params![parent_work_item_id.0], map_integration_record)?;
    let mut out = Vec::new();
    for row in rows {
        out.push(row?);
    }
    Ok(out)
}

pub(crate) fn latest_integration_record_for_parent_in(
    conn: &Connection,
    parent_work_item_id: WorkItemId,
) -> StateResult<Option<IntegrationRecordRow>> {
    let sql = format!(
        "SELECT {INTEGRATION_RECORD_COLUMNS} FROM integration_records \
         WHERE parent_work_item_id = ?1 ORDER BY id DESC LIMIT 1"
    );
    let mut stmt = conn.prepare(&sql)?;
    let row = stmt
        .query_row(params![parent_work_item_id.0], map_integration_record)
        .optional()?;
    Ok(row)
}

impl IntegrationRecordRepository for StateDb {
    fn create_integration_record(
        &mut self,
        new: NewIntegrationRecord<'_>,
    ) -> StateResult<IntegrationRecordRow> {
        create_integration_record_in(self.conn(), new)
    }

    fn get_integration_record(
        &self,
        id: IntegrationRecordId,
    ) -> StateResult<Option<IntegrationRecordRow>> {
        get_integration_record_in(self.conn(), id)
    }

    fn list_integration_records_for_parent(
        &self,
        parent_work_item_id: WorkItemId,
    ) -> StateResult<Vec<IntegrationRecordRow>> {
        list_integration_records_for_parent_in(self.conn(), parent_work_item_id)
    }

    fn latest_integration_record_for_parent(
        &self,
        parent_work_item_id: WorkItemId,
    ) -> StateResult<Option<IntegrationRecordRow>> {
        latest_integration_record_for_parent_in(self.conn(), parent_work_item_id)
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

    fn seed_parent_and_run(db: &mut StateDb, identifier: &str) -> (WorkItemId, RunId) {
        let now = "2026-05-08T00:00:00Z";
        let wi = db
            .create_work_item(NewWorkItem {
                tracker_id: "github",
                identifier,
                parent_id: None,
                title: "broad parent",
                status_class: "integration",
                tracker_status: "in_progress",
                assigned_role: Some("platform_lead"),
                assigned_agent: None,
                priority: None,
                workspace_policy: None,
                branch_policy: None,
                now,
            })
            .expect("wi");
        let run = db
            .create_run(NewRun {
                work_item_id: wi.id,
                role: "platform_lead",
                agent: "claude",
                status: "running",
                workspace_claim_id: None,
                now,
            })
            .expect("run");
        (wi.id, run.id)
    }

    fn pending_payload<'a>(parent: WorkItemId, now: &'a str) -> NewIntegrationRecord<'a> {
        NewIntegrationRecord {
            parent_work_item_id: parent,
            owner_role: "platform_lead",
            run_id: None,
            status: "pending",
            merge_strategy: "sequential_cherry_pick",
            integration_branch: None,
            base_ref: None,
            head_sha: None,
            workspace_path: None,
            children_consolidated: None,
            summary: None,
            conflicts: None,
            blockers_filed: None,
            now,
        }
    }

    #[test]
    fn create_then_get_round_trips_all_columns() {
        let mut db = open();
        let (parent, run) = seed_parent_and_run(&mut db, "OWNER/REPO#42");

        let inserted = db
            .create_integration_record(NewIntegrationRecord {
                run_id: Some(run),
                status: "succeeded",
                merge_strategy: "merge_commits",
                integration_branch: Some("symphony/integration/PROJ-42"),
                base_ref: Some("main"),
                head_sha: Some("deadbeef"),
                workspace_path: Some("../proj-integration"),
                children_consolidated: Some("[11,12]"),
                summary: Some("consolidated 2 children"),
                conflicts: Some("[]"),
                blockers_filed: Some("[]"),
                ..pending_payload(parent, "2026-05-08T00:01:00Z")
            })
            .expect("create");

        let fetched = db
            .get_integration_record(inserted.id)
            .expect("get")
            .expect("present");
        assert_eq!(fetched, inserted);
        assert_eq!(fetched.parent_work_item_id, parent);
        assert_eq!(fetched.run_id, Some(run));
        assert_eq!(fetched.status, "succeeded");
        assert_eq!(fetched.merge_strategy, "merge_commits");
        assert_eq!(
            fetched.integration_branch.as_deref(),
            Some("symphony/integration/PROJ-42")
        );
        assert_eq!(fetched.children_consolidated.as_deref(), Some("[11,12]"));
        assert_eq!(fetched.created_at, "2026-05-08T00:01:00Z");
    }

    #[test]
    fn pending_record_allows_null_optional_columns() {
        let mut db = open();
        let (parent, _run) = seed_parent_and_run(&mut db, "OWNER/REPO#1");
        let inserted = db
            .create_integration_record(pending_payload(parent, "2026-05-08T00:00:01Z"))
            .expect("create pending");
        assert!(inserted.run_id.is_none());
        assert!(inserted.integration_branch.is_none());
        assert!(inserted.base_ref.is_none());
        assert!(inserted.head_sha.is_none());
        assert!(inserted.workspace_path.is_none());
        assert!(inserted.children_consolidated.is_none());
        assert!(inserted.summary.is_none());
        assert!(inserted.conflicts.is_none());
        assert!(inserted.blockers_filed.is_none());
    }

    #[test]
    fn create_rejects_orphan_parent_work_item() {
        let mut db = open();
        let err = db
            .create_integration_record(pending_payload(WorkItemId(9_999), "2026-05-08T00:00:00Z"))
            .expect_err("orphan parent must fail");
        assert!(matches!(err, crate::StateError::Sqlite(_)));
    }

    #[test]
    fn create_rejects_orphan_run_reference() {
        let mut db = open();
        let (parent, _run) = seed_parent_and_run(&mut db, "OWNER/REPO#2");
        let err = db
            .create_integration_record(NewIntegrationRecord {
                run_id: Some(RunId(9_999)),
                ..pending_payload(parent, "2026-05-08T00:00:00Z")
            })
            .expect_err("orphan run must fail");
        assert!(matches!(err, crate::StateError::Sqlite(_)));
    }

    #[test]
    fn create_rejects_unknown_status() {
        let mut db = open();
        let (parent, _run) = seed_parent_and_run(&mut db, "OWNER/REPO#3");
        let err = db
            .create_integration_record(NewIntegrationRecord {
                status: "maybe",
                ..pending_payload(parent, "2026-05-08T00:00:00Z")
            })
            .expect_err("unknown status must fail");
        assert!(matches!(err, crate::StateError::Sqlite(_)));
    }

    #[test]
    fn create_rejects_unknown_merge_strategy() {
        let mut db = open();
        let (parent, _run) = seed_parent_and_run(&mut db, "OWNER/REPO#4");
        let err = db
            .create_integration_record(NewIntegrationRecord {
                merge_strategy: "rebase_n_pray",
                ..pending_payload(parent, "2026-05-08T00:00:00Z")
            })
            .expect_err("unknown merge_strategy must fail");
        assert!(matches!(err, crate::StateError::Sqlite(_)));
    }

    #[test]
    fn list_returns_records_for_parent_in_creation_order() {
        let mut db = open();
        let (parent, run) = seed_parent_and_run(&mut db, "ENG-1");
        let (other_parent, _) = seed_parent_and_run(&mut db, "ENG-2");

        let first = db
            .create_integration_record(pending_payload(parent, "2026-05-08T00:00:01Z"))
            .unwrap();
        let _other = db
            .create_integration_record(pending_payload(other_parent, "2026-05-08T00:00:02Z"))
            .unwrap();
        let second = db
            .create_integration_record(NewIntegrationRecord {
                run_id: Some(run),
                status: "in_progress",
                merge_strategy: "shared_branch",
                ..pending_payload(parent, "2026-05-08T00:00:03Z")
            })
            .unwrap();

        let listed = db
            .list_integration_records_for_parent(parent)
            .expect("list");
        let ids: Vec<_> = listed.iter().map(|r| r.id).collect();
        assert_eq!(ids, vec![first.id, second.id]);
    }

    #[test]
    fn list_returns_empty_for_unknown_parent() {
        let db = open();
        assert!(
            db.list_integration_records_for_parent(WorkItemId(404))
                .unwrap()
                .is_empty()
        );
    }

    #[test]
    fn latest_returns_most_recent_record() {
        let mut db = open();
        let (parent, run) = seed_parent_and_run(&mut db, "ENG-3");
        assert!(
            db.latest_integration_record_for_parent(parent)
                .unwrap()
                .is_none()
        );

        let _first = db
            .create_integration_record(pending_payload(parent, "2026-05-08T00:00:01Z"))
            .unwrap();
        let last = db
            .create_integration_record(NewIntegrationRecord {
                run_id: Some(run),
                status: "succeeded",
                merge_strategy: "merge_commits",
                integration_branch: Some("symphony/integration/ENG-3"),
                summary: Some("done"),
                ..pending_payload(parent, "2026-05-08T00:00:02Z")
            })
            .unwrap();

        let latest = db
            .latest_integration_record_for_parent(parent)
            .expect("latest")
            .expect("present");
        assert_eq!(latest.id, last.id);
        assert_eq!(latest.status, "succeeded");
    }

    #[test]
    fn record_run_reference_clears_when_run_deleted() {
        // FK is `ON DELETE SET NULL`. The integration record itself must
        // survive run garbage-collection because it is durable evidence,
        // even after the run row is reaped.
        let mut db = open();
        let (parent, run) = seed_parent_and_run(&mut db, "ENG-4");
        let inserted = db
            .create_integration_record(NewIntegrationRecord {
                run_id: Some(run),
                status: "in_progress",
                ..pending_payload(parent, "2026-05-08T00:00:01Z")
            })
            .unwrap();

        db.conn()
            .execute("DELETE FROM runs WHERE id = ?1", params![run.0])
            .expect("delete run");

        let after = db
            .get_integration_record(inserted.id)
            .unwrap()
            .expect("record survives run deletion");
        assert!(after.run_id.is_none());
    }
}
