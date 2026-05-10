//! Read/reconcile surface over `workspace_claims` for the `symphony
//! recover` CLI (CHECKLIST_v2 Phase 12).
//!
//! Phase 6 landed the `workspace_claims` schema but no production code
//! yet writes rows to it (workspace strategies persist their claim only
//! in-memory). The recover command still needs a deterministic surface so
//! the orphan-claim path is structurally exercisable today and ready for
//! the day a writer lands. We therefore expose a tiny pair of methods on
//! [`StateDb`]:
//!
//! * [`StateDb::list_orphan_workspace_claims`] — claims whose `claim_status`
//!   is not already a terminal label and whose owning run set is empty
//!   (or entirely terminal). Ordered by id ascending so retries observe a
//!   stable order.
//! * [`StateDb::mark_workspace_claim_orphaned`] — flips `claim_status` to
//!   `'orphaned'` for one id; idempotent (the orphan query excludes rows
//!   already in a terminal label).
//!
//! Terminal claim labels are intentionally a small fixed set: `'orphaned'`,
//! `'released'`, `'reaped'`. The set lives here rather than on a typed
//! enum because no kernel code yet writes claim rows — the labels become
//! a typed enum when Phase 6 production wiring lands and the writer
//! exists to round-trip them.
//!
//! Live-run inclusion mirrors [`symphony_core::run_status::RunStatus::is_terminal`]:
//! a claim with at least one referencing run whose `runs.status` is *not*
//! in `('completed','failed','cancelled')` is considered live and is not
//! surfaced as an orphan.

use rusqlite::params;

use crate::{StateDb, StateResult};

/// One orphaned `workspace_claims` row, projected to the columns the
/// recover CLI needs.
///
/// Plain data — the recover renderer formats it; the kernel
/// [`symphony_core::recovery_tick::OrphanedWorkspaceClaimCandidate`] is a
/// separate but compatible projection used by the running orchestrator's
/// recovery tick.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OrphanWorkspaceClaim {
    /// Primary key.
    pub id: i64,
    /// Owning work item.
    pub work_item_id: i64,
    /// On-disk path the claim allocated.
    pub path: String,
    /// `claim_status` column verbatim at observe time.
    pub claim_status: String,
}

const ORPHAN_QUERY: &str = "\
SELECT wc.id, wc.work_item_id, wc.path, wc.claim_status \
FROM workspace_claims wc \
WHERE wc.claim_status NOT IN ('orphaned','released','reaped') \
  AND NOT EXISTS ( \
    SELECT 1 FROM runs r \
    WHERE r.workspace_claim_id = wc.id \
      AND r.status NOT IN ('completed','failed','cancelled') \
  ) \
ORDER BY wc.id ASC\
";

impl StateDb {
    /// Return every `workspace_claims` row whose `claim_status` is not
    /// already terminal and that no live (non-terminal) run references.
    ///
    /// Lex-stable ordering: `id ASC`. Empty result is the common steady
    /// state.
    pub fn list_orphan_workspace_claims(&self) -> StateResult<Vec<OrphanWorkspaceClaim>> {
        let conn = self.conn();
        let mut stmt = conn.prepare(ORPHAN_QUERY)?;
        let rows = stmt.query_map([], |row| {
            Ok(OrphanWorkspaceClaim {
                id: row.get(0)?,
                work_item_id: row.get(1)?,
                path: row.get(2)?,
                claim_status: row.get(3)?,
            })
        })?;
        let mut out = Vec::new();
        for r in rows {
            out.push(r?);
        }
        Ok(out)
    }

    /// Flip `claim_status` to `'orphaned'` for the given claim id.
    ///
    /// Returns `Ok(true)` when one row was updated, `Ok(false)` when no
    /// row matches the id. Calling on a row whose `claim_status` is
    /// already `'orphaned'` still returns `Ok(true)`; the caller is
    /// expected to drive this from [`StateDb::list_orphan_workspace_claims`]
    /// which excludes terminal labels in the first place.
    pub fn mark_workspace_claim_orphaned(&self, id: i64) -> StateResult<bool> {
        let updated = self.conn().execute(
            "UPDATE workspace_claims SET claim_status = 'orphaned' WHERE id = ?1",
            params![id],
        )?;
        Ok(updated == 1)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::migrations::migrations;
    use crate::repository::{
        LeaseAcquisition, NewRun, NewWorkItem, RunRepository, WorkItemRepository,
    };

    fn open_db() -> (tempfile::TempDir, StateDb) {
        let dir = tempfile::tempdir().expect("tempdir");
        let mut db = StateDb::open(dir.path().join("state.sqlite3")).expect("open");
        db.migrate(migrations()).expect("migrate");
        (dir, db)
    }

    fn seed_work_item(db: &mut StateDb, identifier: &str) -> i64 {
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
        .unwrap()
        .id
        .0
    }

    fn insert_claim(db: &StateDb, work_item_id: i64, status: &str) -> i64 {
        db.conn()
            .execute(
                "INSERT INTO workspace_claims \
                    (work_item_id, path, strategy, claim_status, created_at) \
                 VALUES (?1, ?2, 'git_worktree', ?3, '2026-05-08T00:00:00Z')",
                params![work_item_id, format!("/tmp/wt-{work_item_id}"), status],
            )
            .unwrap();
        db.conn().last_insert_rowid()
    }

    fn insert_run(db: &mut StateDb, work_item_id: i64, claim_id: i64, status: &str) -> i64 {
        let rec = db
            .create_run(NewRun {
                work_item_id: crate::repository::WorkItemId(work_item_id),
                role: "specialist:ui",
                agent: "codex",
                status,
                workspace_claim_id: Some(claim_id),
                now: "2026-05-08T00:00:00Z",
            })
            .unwrap();
        rec.id.0
    }

    #[test]
    fn empty_db_returns_empty_orphan_list() {
        let (_dir, db) = open_db();
        assert!(db.list_orphan_workspace_claims().unwrap().is_empty());
    }

    #[test]
    fn claim_with_no_referencing_run_is_orphan() {
        let (_dir, mut db) = open_db();
        let wi = seed_work_item(&mut db, "OWNER/REPO#1");
        let claim_id = insert_claim(&db, wi, "active");
        let orphans = db.list_orphan_workspace_claims().unwrap();
        assert_eq!(orphans.len(), 1);
        assert_eq!(orphans[0].id, claim_id);
        assert_eq!(orphans[0].claim_status, "active");
    }

    #[test]
    fn claim_with_live_run_is_not_orphan() {
        let (_dir, mut db) = open_db();
        let wi = seed_work_item(&mut db, "OWNER/REPO#1");
        let claim_id = insert_claim(&db, wi, "active");
        let _ = insert_run(&mut db, wi, claim_id, "running");
        assert!(db.list_orphan_workspace_claims().unwrap().is_empty());
    }

    #[test]
    fn claim_with_only_terminal_runs_is_orphan() {
        let (_dir, mut db) = open_db();
        let wi = seed_work_item(&mut db, "OWNER/REPO#1");
        let claim_id = insert_claim(&db, wi, "active");
        let _ = insert_run(&mut db, wi, claim_id, "completed");
        let _ = insert_run(&mut db, wi, claim_id, "cancelled");
        let orphans = db.list_orphan_workspace_claims().unwrap();
        assert_eq!(orphans.len(), 1);
        assert_eq!(orphans[0].id, claim_id);
    }

    #[test]
    fn claim_already_orphaned_is_not_re_surfaced() {
        let (_dir, mut db) = open_db();
        let wi = seed_work_item(&mut db, "OWNER/REPO#1");
        let _ = insert_claim(&db, wi, "orphaned");
        let _ = insert_claim(&db, wi, "released");
        let _ = insert_claim(&db, wi, "reaped");
        assert!(db.list_orphan_workspace_claims().unwrap().is_empty());
    }

    #[test]
    fn mark_orphaned_updates_status() {
        let (_dir, mut db) = open_db();
        let wi = seed_work_item(&mut db, "OWNER/REPO#1");
        let claim_id = insert_claim(&db, wi, "active");
        assert!(db.mark_workspace_claim_orphaned(claim_id).unwrap());
        let orphans = db.list_orphan_workspace_claims().unwrap();
        assert!(
            orphans.is_empty(),
            "post-mark, orphan list excludes the row"
        );
    }

    #[test]
    fn mark_orphaned_missing_id_returns_false() {
        let (_dir, db) = open_db();
        assert!(!db.mark_workspace_claim_orphaned(9_999).unwrap());
    }

    #[test]
    fn ordering_is_stable_by_id_ascending() {
        let (_dir, mut db) = open_db();
        let wi = seed_work_item(&mut db, "OWNER/REPO#1");
        let a = insert_claim(&db, wi, "active");
        let b = insert_claim(&db, wi, "active");
        let c = insert_claim(&db, wi, "active");
        let orphans = db.list_orphan_workspace_claims().unwrap();
        let ids: Vec<i64> = orphans.iter().map(|o| o.id).collect();
        assert_eq!(ids, vec![a, b, c]);
    }

    /// Smoke check: lease APIs interact with the orphan query as expected
    /// — a run with a held lease but `status = 'completed'` does not
    /// keep the claim live.
    #[test]
    fn terminal_run_with_held_lease_does_not_block_orphan_detection() {
        let (_dir, mut db) = open_db();
        let wi = seed_work_item(&mut db, "OWNER/REPO#1");
        let claim_id = insert_claim(&db, wi, "active");
        let run_id = insert_run(&mut db, wi, claim_id, "completed");
        let outcome = db
            .acquire_lease(
                crate::repository::RunId(run_id),
                "host/sched/0",
                "2099-01-01T00:00:00Z",
                "2026-05-08T00:00:00Z",
            )
            .unwrap();
        assert_eq!(outcome, LeaseAcquisition::Acquired);
        let orphans = db.list_orphan_workspace_claims().unwrap();
        assert_eq!(orphans.len(), 1);
    }
}
