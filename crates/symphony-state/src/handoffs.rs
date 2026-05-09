//! Persistence for the `handoffs` table (SPEC v2 §4.7).
//!
//! A handoff is the structured envelope an agent emits at the end of a
//! run: what changed, what tests ran, what evidence was captured, and —
//! crucially — what queue the work item should land on next
//! (`ready_for`). The kernel branches on those fields when it decides
//! the next consequence (`enqueue_integration`, `enqueue_qa`, …); the
//! durable row is what the operator surfaces (`symphony status`,
//! `symphony issue graph`) read so they can describe the orchestrator's
//! state without re-asking the agent.
//!
//! Two design choices worth flagging:
//!
//! * The storage layer keeps `ready_for` stringly-typed (no SQL `CHECK`,
//!   no Rust enum at this layer). The migration in [`crate::migrations`]
//!   already documents the variants; the typed enum lives in
//!   `symphony-core::handoff::ReadyFor` so policy code reads through that
//!   layer. Mirroring the enum here would force every storage caller to
//!   round-trip through `symphony-core`, which Phase 2 deliberately
//!   avoids — repository crates do not depend on domain types.
//! * `changed_files`, `tests_run`, `evidence`, and `known_risks` are
//!   already-encoded JSON arrays. We treat them as opaque strings so the
//!   storage crate does not take a hard `serde_json` round-trip on
//!   write, matching the convention used by `work_items.workspace_policy`
//!   and the `events.payload` column.

use rusqlite::{Connection, OptionalExtension, params};

use crate::repository::{RunId, WorkItemId};
use crate::{StateDb, StateResult};

/// Strongly-typed primary key for `handoffs`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct HandoffId(pub i64);

/// A `handoffs` row as stored.
///
/// JSON columns surface as `Option<String>` so consumers can decide
/// whether to deserialize them; the column is `NULL` when the agent did
/// not emit the field (rather than an empty `[]`), which keeps the
/// storage payload symmetric with the `Handoff` envelope shape in
/// `symphony-core`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HandoffRecord {
    /// Primary key.
    pub id: HandoffId,
    /// Run that produced the handoff.
    pub run_id: RunId,
    /// Owning work item.
    pub work_item_id: WorkItemId,
    /// `ReadyFor` discriminator (`integration`, `qa`, `human_review`,
    /// `blocked`, `done`).
    pub ready_for: String,
    /// Operator-facing summary. Always non-empty per `symphony-core`
    /// validation, but the storage layer does not re-check.
    pub summary: String,
    /// JSON array of changed file paths, if any.
    pub changed_files: Option<String>,
    /// JSON array of test commands the run executed, if any.
    pub tests_run: Option<String>,
    /// JSON array of verification-evidence pointers, if any.
    pub evidence: Option<String>,
    /// JSON array of known-risk strings, if any.
    pub known_risks: Option<String>,
    /// RFC3339 row creation timestamp.
    pub created_at: String,
}

/// Insertion payload for [`HandoffRepository::create_handoff`].
///
/// Mirrors the storage row exactly; the caller is responsible for
/// encoding any JSON fields before insert. This keeps the storage crate
/// free of `serde_json` and lets the kernel reuse the canonical
/// `Handoff` serializer in `symphony-core` without an extra hop.
#[derive(Debug, Clone)]
pub struct NewHandoff<'a> {
    /// Run that produced the handoff.
    pub run_id: RunId,
    /// Owning work item.
    pub work_item_id: WorkItemId,
    /// `ReadyFor` discriminator.
    pub ready_for: &'a str,
    /// Operator-facing summary.
    pub summary: &'a str,
    /// Already-encoded JSON array of changed file paths.
    pub changed_files: Option<&'a str>,
    /// Already-encoded JSON array of test commands.
    pub tests_run: Option<&'a str>,
    /// Already-encoded JSON array of verification-evidence pointers.
    pub evidence: Option<&'a str>,
    /// Already-encoded JSON array of known-risk strings.
    pub known_risks: Option<&'a str>,
    /// RFC3339 row creation timestamp.
    pub now: &'a str,
}

/// CRUD over `handoffs`.
///
/// Insertion is the hot path; reads exist for the operator surfaces
/// (`symphony status`, `symphony issue graph`) and for crash-recovery
/// audits that need to reconstruct what an agent reported before a
/// process restart.
pub trait HandoffRepository {
    /// Insert a new handoff envelope and return its persisted form.
    ///
    /// Foreign-key violations (unknown `run_id` or `work_item_id`)
    /// surface as [`crate::StateError::Sqlite`]; the storage layer
    /// trusts upstream validation for `ready_for` and `summary`.
    fn create_handoff(&mut self, new: NewHandoff<'_>) -> StateResult<HandoffRecord>;

    /// Fetch a single handoff by primary key.
    fn get_handoff(&self, id: HandoffId) -> StateResult<Option<HandoffRecord>>;

    /// List every handoff filed against `work_item_id`, oldest first.
    ///
    /// Order is `id ASC` — under SQLite's autoincrement that matches
    /// insertion order, which is also the order an operator wants to
    /// scan the audit trail.
    fn list_handoffs_for_work_item(
        &self,
        work_item_id: WorkItemId,
    ) -> StateResult<Vec<HandoffRecord>>;

    /// Return the most recent handoff filed against `work_item_id`, or
    /// `None` if none exist. Convenience wrapper over
    /// [`Self::list_handoffs_for_work_item`] for status surfaces that
    /// only need the latest envelope.
    fn latest_handoff_for_work_item(
        &self,
        work_item_id: WorkItemId,
    ) -> StateResult<Option<HandoffRecord>>;
}

const HANDOFF_COLUMNS: &str = "id, run_id, work_item_id, ready_for, summary, \
     changed_files, tests_run, evidence, known_risks, created_at";

fn map_handoff(row: &rusqlite::Row<'_>) -> rusqlite::Result<HandoffRecord> {
    Ok(HandoffRecord {
        id: HandoffId(row.get(0)?),
        run_id: RunId(row.get(1)?),
        work_item_id: WorkItemId(row.get(2)?),
        ready_for: row.get(3)?,
        summary: row.get(4)?,
        changed_files: row.get(5)?,
        tests_run: row.get(6)?,
        evidence: row.get(7)?,
        known_risks: row.get(8)?,
        created_at: row.get(9)?,
    })
}

pub(crate) fn create_handoff_in(
    conn: &Connection,
    new: NewHandoff<'_>,
) -> StateResult<HandoffRecord> {
    conn.execute(
        "INSERT INTO handoffs \
            (run_id, work_item_id, ready_for, summary, changed_files, \
             tests_run, evidence, known_risks, created_at) \
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9)",
        params![
            new.run_id.0,
            new.work_item_id.0,
            new.ready_for,
            new.summary,
            new.changed_files,
            new.tests_run,
            new.evidence,
            new.known_risks,
            new.now,
        ],
    )?;
    let id = HandoffId(conn.last_insert_rowid());
    Ok(get_handoff_in(conn, id)?.expect("freshly inserted handoff must be readable"))
}

pub(crate) fn get_handoff_in(
    conn: &Connection,
    id: HandoffId,
) -> StateResult<Option<HandoffRecord>> {
    let sql = format!("SELECT {HANDOFF_COLUMNS} FROM handoffs WHERE id = ?1");
    let mut stmt = conn.prepare(&sql)?;
    let row = stmt.query_row(params![id.0], map_handoff).optional()?;
    Ok(row)
}

pub(crate) fn list_handoffs_for_work_item_in(
    conn: &Connection,
    work_item_id: WorkItemId,
) -> StateResult<Vec<HandoffRecord>> {
    let sql = format!(
        "SELECT {HANDOFF_COLUMNS} FROM handoffs \
         WHERE work_item_id = ?1 ORDER BY id ASC"
    );
    let mut stmt = conn.prepare(&sql)?;
    let rows = stmt.query_map(params![work_item_id.0], map_handoff)?;
    let mut out = Vec::new();
    for row in rows {
        out.push(row?);
    }
    Ok(out)
}

pub(crate) fn latest_handoff_for_work_item_in(
    conn: &Connection,
    work_item_id: WorkItemId,
) -> StateResult<Option<HandoffRecord>> {
    let sql = format!(
        "SELECT {HANDOFF_COLUMNS} FROM handoffs \
         WHERE work_item_id = ?1 ORDER BY id DESC LIMIT 1"
    );
    let mut stmt = conn.prepare(&sql)?;
    let row = stmt
        .query_row(params![work_item_id.0], map_handoff)
        .optional()?;
    Ok(row)
}

impl HandoffRepository for StateDb {
    fn create_handoff(&mut self, new: NewHandoff<'_>) -> StateResult<HandoffRecord> {
        create_handoff_in(self.conn(), new)
    }

    fn get_handoff(&self, id: HandoffId) -> StateResult<Option<HandoffRecord>> {
        get_handoff_in(self.conn(), id)
    }

    fn list_handoffs_for_work_item(
        &self,
        work_item_id: WorkItemId,
    ) -> StateResult<Vec<HandoffRecord>> {
        list_handoffs_for_work_item_in(self.conn(), work_item_id)
    }

    fn latest_handoff_for_work_item(
        &self,
        work_item_id: WorkItemId,
    ) -> StateResult<Option<HandoffRecord>> {
        latest_handoff_for_work_item_in(self.conn(), work_item_id)
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

    fn seed_work_item_and_run(db: &mut StateDb, identifier: &str) -> (WorkItemId, RunId) {
        let now = "2026-05-08T00:00:00Z";
        let wi = db
            .create_work_item(NewWorkItem {
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

    fn sample_handoff<'a>(
        run_id: RunId,
        work_item_id: WorkItemId,
        ready_for: &'a str,
        summary: &'a str,
        now: &'a str,
    ) -> NewHandoff<'a> {
        NewHandoff {
            run_id,
            work_item_id,
            ready_for,
            summary,
            changed_files: None,
            tests_run: None,
            evidence: None,
            known_risks: None,
            now,
        }
    }

    #[test]
    fn create_then_get_round_trips_all_columns() {
        let mut db = open();
        let (wi, run) = seed_work_item_and_run(&mut db, "OWNER/REPO#1");

        let inserted = db
            .create_handoff(NewHandoff {
                changed_files: Some(r#"["src/lib.rs"]"#),
                tests_run: Some(r#"["cargo test"]"#),
                evidence: Some(r#"["logs/run-1.txt"]"#),
                known_risks: Some(r#"["flaky integration test"]"#),
                ..sample_handoff(run, wi, "qa", "specialist done", "2026-05-08T00:01:00Z")
            })
            .expect("create");

        let fetched = db.get_handoff(inserted.id).expect("get").expect("present");
        assert_eq!(fetched, inserted);
        assert_eq!(fetched.run_id, run);
        assert_eq!(fetched.work_item_id, wi);
        assert_eq!(fetched.ready_for, "qa");
        assert_eq!(fetched.summary, "specialist done");
        assert_eq!(fetched.changed_files.as_deref(), Some(r#"["src/lib.rs"]"#));
        assert_eq!(fetched.created_at, "2026-05-08T00:01:00Z");
    }

    #[test]
    fn json_columns_default_to_null_when_omitted() {
        let mut db = open();
        let (wi, run) = seed_work_item_and_run(&mut db, "OWNER/REPO#2");
        let inserted = db
            .create_handoff(sample_handoff(
                run,
                wi,
                "done",
                "no changes",
                "2026-05-08T00:00:01Z",
            ))
            .expect("create");
        assert!(inserted.changed_files.is_none());
        assert!(inserted.tests_run.is_none());
        assert!(inserted.evidence.is_none());
        assert!(inserted.known_risks.is_none());
    }

    #[test]
    fn create_handoff_rejects_orphan_run() {
        let mut db = open();
        let (wi, _run) = seed_work_item_and_run(&mut db, "OWNER/REPO#3");
        let err = db
            .create_handoff(sample_handoff(
                RunId(9_999),
                wi,
                "qa",
                "phantom",
                "2026-05-08T00:00:00Z",
            ))
            .expect_err("orphan run must fail");
        assert!(matches!(err, crate::StateError::Sqlite(_)));
    }

    #[test]
    fn create_handoff_rejects_orphan_work_item() {
        let mut db = open();
        let (_wi, run) = seed_work_item_and_run(&mut db, "OWNER/REPO#4");
        let err = db
            .create_handoff(sample_handoff(
                run,
                WorkItemId(9_999),
                "qa",
                "phantom",
                "2026-05-08T00:00:00Z",
            ))
            .expect_err("orphan work item must fail");
        assert!(matches!(err, crate::StateError::Sqlite(_)));
    }

    #[test]
    fn list_handoffs_for_work_item_returns_in_creation_order() {
        let mut db = open();
        let (wi, run) = seed_work_item_and_run(&mut db, "ENG-1");
        let (other_wi, other_run) = seed_work_item_and_run(&mut db, "ENG-2");

        let first = db
            .create_handoff(sample_handoff(
                run,
                wi,
                "integration",
                "round one",
                "2026-05-08T00:00:01Z",
            ))
            .unwrap();
        let _other = db
            .create_handoff(sample_handoff(
                other_run,
                other_wi,
                "qa",
                "different work item",
                "2026-05-08T00:00:02Z",
            ))
            .unwrap();
        let second = db
            .create_handoff(sample_handoff(
                run,
                wi,
                "qa",
                "round two",
                "2026-05-08T00:00:03Z",
            ))
            .unwrap();

        let listed = db.list_handoffs_for_work_item(wi).expect("list");
        let ids: Vec<_> = listed.iter().map(|h| h.id).collect();
        assert_eq!(ids, vec![first.id, second.id]);
    }

    #[test]
    fn list_handoffs_for_work_item_returns_empty_for_unknown_or_quiet_work_item() {
        let mut db = open();
        let (wi, _run) = seed_work_item_and_run(&mut db, "ENG-7");
        assert!(db.list_handoffs_for_work_item(wi).unwrap().is_empty());
        assert!(
            db.list_handoffs_for_work_item(WorkItemId(404))
                .unwrap()
                .is_empty()
        );
    }

    #[test]
    fn latest_handoff_for_work_item_returns_most_recent() {
        let mut db = open();
        let (wi, run) = seed_work_item_and_run(&mut db, "ENG-3");
        assert!(db.latest_handoff_for_work_item(wi).unwrap().is_none());

        let _first = db
            .create_handoff(sample_handoff(
                run,
                wi,
                "integration",
                "first",
                "2026-05-08T00:00:01Z",
            ))
            .unwrap();
        let last = db
            .create_handoff(sample_handoff(
                run,
                wi,
                "qa",
                "second",
                "2026-05-08T00:00:02Z",
            ))
            .unwrap();
        let latest = db
            .latest_handoff_for_work_item(wi)
            .expect("latest")
            .expect("present");
        assert_eq!(latest.id, last.id);
        assert_eq!(latest.summary, "second");
    }

    #[test]
    fn handoffs_cascade_when_run_is_deleted() {
        // FK is `ON DELETE CASCADE` — verify the storage layer relies on
        // it, so callers do not need to manually clear handoffs when
        // garbage-collecting runs.
        let mut db = open();
        let (wi, run) = seed_work_item_and_run(&mut db, "ENG-4");
        db.create_handoff(sample_handoff(
            run,
            wi,
            "qa",
            "to be deleted",
            "2026-05-08T00:00:01Z",
        ))
        .unwrap();

        db.conn()
            .execute("DELETE FROM runs WHERE id = ?1", params![run.0])
            .expect("delete run");
        assert!(db.list_handoffs_for_work_item(wi).unwrap().is_empty());
    }
}
