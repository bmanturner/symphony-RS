//! Read access to the `qa_verdicts` table (SPEC v2 §4.9 / §5.12).
//!
//! Phase 9 widened the `verdict` column to the five SPEC verdicts and
//! added the authorship triple (`role`, `waiver_role`, `reason`). Phase 12
//! consumes that surface from the operator side: `symphony qa verdict
//! <id>` reads the rows for one work item so an operator can see what QA
//! decided and on what evidence — without reaching for SQL.
//!
//! The repository is intentionally read-only here. Verdict insertion is
//! the QA dispatch runner's job (Phase 9 / Phase 11) and the same crate
//! exposes inserts through the transaction helper. This module only
//! surfaces the typed read.

use rusqlite::{Connection, params};

use crate::repository::{RunId, WorkItemId};
use crate::{StateDb, StateResult};

/// One persisted `qa_verdicts` row, returned verbatim.
///
/// The JSON-shaped columns (`evidence`, `acceptance_trace`,
/// `blockers_created`) are surfaced as raw strings so the storage crate
/// stays free of `serde_json`. CLI / API callers that want the typed
/// payload deserialize against the [`symphony_core::qa`] vocabulary.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct QaVerdictRecord {
    /// `qa_verdicts.id` primary key.
    pub id: i64,
    /// Work item this verdict was filed against.
    pub work_item_id: WorkItemId,
    /// Run that produced the verdict.
    pub run_id: RunId,
    /// Authoring role (typically the `qa_gate` role name).
    pub role: String,
    /// One of the five SPEC verdicts: `passed`, `failed_with_blockers`,
    /// `failed_needs_rework`, `inconclusive`, `waived`.
    pub verdict: String,
    /// Role that authorised a waiver, if any (only set when verdict ==
    /// `waived`).
    pub waiver_role: Option<String>,
    /// Operator-facing reason recorded with the verdict.
    pub reason: Option<String>,
    /// Raw JSON evidence column, if any.
    pub evidence: Option<String>,
    /// Raw JSON acceptance-criteria trace, if any.
    pub acceptance_trace: Option<String>,
    /// Raw JSON list of blocker work-item ids QA filed, if any.
    pub blockers_created: Option<String>,
    /// RFC3339 created timestamp.
    pub created_at: String,
}

/// Insertion payload for [`create_qa_verdict_in`].
#[derive(Debug, Clone)]
pub struct NewQaVerdict<'a> {
    /// Work item this verdict gates.
    pub work_item_id: WorkItemId,
    /// Run that produced the verdict.
    pub run_id: RunId,
    /// Authoring role.
    pub role: &'a str,
    /// Stable verdict discriminator.
    pub verdict: &'a str,
    /// Waiver authoring role, if this is a waived verdict.
    pub waiver_role: Option<&'a str>,
    /// Operator-facing verdict reason.
    pub reason: Option<&'a str>,
    /// Raw JSON evidence column.
    pub evidence: Option<&'a str>,
    /// Raw JSON acceptance trace column.
    pub acceptance_trace: Option<&'a str>,
    /// Raw JSON blocker edge ids filed by QA.
    pub blockers_created: Option<&'a str>,
    /// RFC3339 creation timestamp.
    pub now: &'a str,
}

/// Read-only view over [`qa_verdicts`](self) rows.
pub trait QaVerdictRepository {
    /// Return every verdict filed against `work_item_id`, newest first by
    /// `created_at` then by `id` for determinism.
    fn list_qa_verdicts_for_work_item(
        &self,
        work_item_id: WorkItemId,
    ) -> StateResult<Vec<QaVerdictRecord>>;
}

/// Insert one QA verdict row.
pub fn create_qa_verdict_in(
    conn: &Connection,
    new: NewQaVerdict<'_>,
) -> StateResult<QaVerdictRecord> {
    conn.execute(
        "INSERT INTO qa_verdicts \
            (work_item_id, run_id, role, verdict, waiver_role, reason, \
             evidence, acceptance_trace, blockers_created, created_at) \
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10)",
        params![
            new.work_item_id.0,
            new.run_id.0,
            new.role,
            new.verdict,
            new.waiver_role,
            new.reason,
            new.evidence,
            new.acceptance_trace,
            new.blockers_created,
            new.now,
        ],
    )?;
    let id = conn.last_insert_rowid();
    Ok(QaVerdictRecord {
        id,
        work_item_id: new.work_item_id,
        run_id: new.run_id,
        role: new.role.to_string(),
        verdict: new.verdict.to_string(),
        waiver_role: new.waiver_role.map(str::to_string),
        reason: new.reason.map(str::to_string),
        evidence: new.evidence.map(str::to_string),
        acceptance_trace: new.acceptance_trace.map(str::to_string),
        blockers_created: new.blockers_created.map(str::to_string),
        created_at: new.now.to_string(),
    })
}

impl QaVerdictRepository for StateDb {
    fn list_qa_verdicts_for_work_item(
        &self,
        work_item_id: WorkItemId,
    ) -> StateResult<Vec<QaVerdictRecord>> {
        list_for_work_item_in(self.conn(), work_item_id)
    }
}

pub(crate) fn list_for_work_item_in(
    conn: &Connection,
    work_item_id: WorkItemId,
) -> StateResult<Vec<QaVerdictRecord>> {
    let mut stmt = conn.prepare(
        "SELECT id, work_item_id, run_id, role, verdict, \
                waiver_role, reason, evidence, acceptance_trace, \
                blockers_created, created_at \
           FROM qa_verdicts \
          WHERE work_item_id = ?1 \
          ORDER BY created_at DESC, id DESC",
    )?;
    let rows = stmt.query_map(rusqlite::params![work_item_id.0], |row| {
        Ok(QaVerdictRecord {
            id: row.get(0)?,
            work_item_id: WorkItemId(row.get(1)?),
            run_id: RunId(row.get(2)?),
            role: row.get(3)?,
            verdict: row.get(4)?,
            waiver_role: row.get(5)?,
            reason: row.get(6)?,
            evidence: row.get(7)?,
            acceptance_trace: row.get(8)?,
            blockers_created: row.get(9)?,
            created_at: row.get(10)?,
        })
    })?;
    let mut out = Vec::new();
    for row in rows {
        out.push(row?);
    }
    Ok(out)
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

    fn seed_item(db: &mut StateDb, identifier: &str) -> WorkItemId {
        db.create_work_item(NewWorkItem {
            tracker_id: "github",
            identifier,
            parent_id: None,
            title: "demo",
            status_class: "qa",
            tracker_status: "open",
            assigned_role: None,
            assigned_agent: None,
            priority: None,
            workspace_policy: None,
            branch_policy: None,
            now: "2026-05-08T00:00:00Z",
        })
        .expect("seed work item")
        .id
    }

    fn seed_run(db: &mut StateDb, work_item: WorkItemId, now: &str) -> RunId {
        db.create_run(NewRun {
            work_item_id: work_item,
            role: "qa",
            agent: "claude",
            status: "completed",
            workspace_claim_id: None,
            now,
        })
        .expect("seed run")
        .id
    }

    #[allow(clippy::too_many_arguments)]
    fn insert_verdict(
        db: &StateDb,
        work_item: WorkItemId,
        run: RunId,
        role: &str,
        verdict: &str,
        waiver_role: Option<&str>,
        reason: Option<&str>,
        evidence: Option<&str>,
        acceptance_trace: Option<&str>,
        blockers_created: Option<&str>,
        now: &str,
    ) {
        db.conn()
            .execute(
                "INSERT INTO qa_verdicts \
                    (work_item_id, run_id, role, verdict, waiver_role, reason, \
                     evidence, acceptance_trace, blockers_created, created_at) \
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10)",
                rusqlite::params![
                    work_item.0,
                    run.0,
                    role,
                    verdict,
                    waiver_role,
                    reason,
                    evidence,
                    acceptance_trace,
                    blockers_created,
                    now,
                ],
            )
            .expect("insert qa_verdict");
    }

    #[test]
    fn empty_listing_for_unknown_work_item() {
        let db = open();
        let rows = db
            .list_qa_verdicts_for_work_item(WorkItemId(99_999))
            .unwrap();
        assert!(rows.is_empty());
    }

    #[test]
    fn list_returns_rows_for_work_item_newest_first() {
        let mut db = open();
        let id = seed_item(&mut db, "ENG-1");
        let r1 = seed_run(&mut db, id, "2026-05-08T00:01:00Z");
        let r2 = seed_run(&mut db, id, "2026-05-08T00:02:00Z");

        // Older verdict first.
        insert_verdict(
            &db,
            id,
            r1,
            "qa",
            "failed_needs_rework",
            None,
            Some("flaky"),
            Some(r#"{"summary":"first"}"#),
            None,
            None,
            "2026-05-08T00:03:00Z",
        );
        // Newer verdict second.
        insert_verdict(
            &db,
            id,
            r2,
            "qa",
            "passed",
            None,
            None,
            Some(r#"{"summary":"green"}"#),
            Some(r#"[{"id":"AC1","status":"passed"}]"#),
            None,
            "2026-05-08T00:05:00Z",
        );

        let rows = db.list_qa_verdicts_for_work_item(id).unwrap();
        assert_eq!(rows.len(), 2);
        // Newest first.
        assert_eq!(rows[0].verdict, "passed");
        assert_eq!(rows[0].role, "qa");
        assert_eq!(rows[0].run_id, r2);
        assert_eq!(
            rows[0].acceptance_trace.as_deref(),
            Some(r#"[{"id":"AC1","status":"passed"}]"#)
        );
        assert_eq!(rows[1].verdict, "failed_needs_rework");
        assert_eq!(rows[1].run_id, r1);
        assert_eq!(rows[1].reason.as_deref(), Some("flaky"));
    }

    #[test]
    fn waiver_metadata_round_trips() {
        let mut db = open();
        let id = seed_item(&mut db, "ENG-1");
        let run = seed_run(&mut db, id, "2026-05-08T00:01:00Z");

        insert_verdict(
            &db,
            id,
            run,
            "qa",
            "waived",
            Some("platform_lead"),
            Some("ship: tracking in follow-up"),
            None,
            None,
            None,
            "2026-05-08T00:03:00Z",
        );

        let rows = db.list_qa_verdicts_for_work_item(id).unwrap();
        assert_eq!(rows.len(), 1);
        let row = &rows[0];
        assert_eq!(row.verdict, "waived");
        assert_eq!(row.waiver_role.as_deref(), Some("platform_lead"));
        assert_eq!(row.reason.as_deref(), Some("ship: tracking in follow-up"));
    }

    #[test]
    fn verdicts_for_other_work_items_are_isolated() {
        let mut db = open();
        let a = seed_item(&mut db, "ENG-A");
        let b = seed_item(&mut db, "ENG-B");
        let ra = seed_run(&mut db, a, "2026-05-08T00:01:00Z");
        let rb = seed_run(&mut db, b, "2026-05-08T00:01:00Z");

        insert_verdict(
            &db,
            a,
            ra,
            "qa",
            "passed",
            None,
            None,
            None,
            None,
            None,
            "2026-05-08T00:02:00Z",
        );
        insert_verdict(
            &db,
            b,
            rb,
            "qa",
            "failed_with_blockers",
            None,
            None,
            None,
            None,
            Some("[42]"),
            "2026-05-08T00:02:00Z",
        );

        let only_a = db.list_qa_verdicts_for_work_item(a).unwrap();
        assert_eq!(only_a.len(), 1);
        assert_eq!(only_a[0].verdict, "passed");

        let only_b = db.list_qa_verdicts_for_work_item(b).unwrap();
        assert_eq!(only_b.len(), 1);
        assert_eq!(only_b[0].verdict, "failed_with_blockers");
        assert_eq!(only_b[0].blockers_created.as_deref(), Some("[42]"));
    }
}
