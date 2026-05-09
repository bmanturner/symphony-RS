//! QA queue: durable read view of work items ready for the QA-gate role
//! (SPEC v2 §6.5, ARCH §6.3).
//!
//! Like the integration queue in [`crate::integration_queue`], the QA
//! queue is a *query* over existing tables rather than a materialized
//! list. Membership is derived from current state at read time, so there
//! is no cache-coherence problem when integration succeeds, a verdict
//! lands, or a new run is queued.
//!
//! A work item is *ready for QA* when **all** of the following hold:
//!
//! 1. The work item itself is not in a terminal status class
//!    (`done` / `cancelled`).
//! 2. **Either**
//!    * its `status_class = 'qa'` — i.e. a specialist or integration
//!      owner signalled `ready_for: qa` and the kernel has already
//!      promoted the item (the *direct* cause), *or*
//!    * an integration record with `status = 'succeeded'` exists for the
//!      work item, and no QA verdict has been recorded against the work
//!      item since the latest such integration record (the
//!      *integration-consolidated* cause).
//! 3. There are no open `blocks` edges anywhere in the work item's
//!    subtree (mirrors the integration-queue blocker subtree exclusion
//!    from SPEC v2 §5.10 / §5.12).
//!
//! The two readiness causes are surfaced separately as [`QaQueueCause`]
//! so the scheduler can record *why* an item entered the queue when
//! emitting events.
//!
//! ## Cause precedence
//!
//! The direct cause beats the integration-consolidated cause when both
//! apply. If the kernel has already promoted the item to
//! `status_class = 'qa'` after a successful integration, the queue
//! reports `DirectQaRequest` rather than `IntegrationConsolidated`,
//! mirroring the integration queue's preference for the kernel-promoted
//! signal.
//!
//! ## Re-QA after rework
//!
//! When QA fails, the workflow files blockers and routes to rework.
//! After rework lands, the integration owner produces a *new*
//! integration record. The "no QA verdict newer than the latest
//! integration record" check then re-admits the parent — the new
//! integration record is the load-bearing signal that prior verdicts no
//! longer apply.
//!
//! ## Gate waivers
//!
//! The blocker-subtree gate corresponds to the workflow-level switch
//! `QaConfig.require_no_open_blockers` from SPEC v2 §5.12. The default
//! matches the SPEC: gate *on*. A workflow may waive it by passing a
//! [`QaQueueGates`] value to
//! [`QaQueueRepository::list_ready_for_qa_with_gates`]. The legacy
//! [`QaQueueRepository::list_ready_for_qa`] keeps the gate-on behavior
//! so existing callers do not change.

use rusqlite::Connection;

use crate::repository::{WORK_ITEM_COLUMNS, WorkItemId, WorkItemRecord, map_work_item};
use crate::{StateDb, StateResult};

/// Why a work item is in the QA queue.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum QaQueueCause {
    /// The work item itself is in `status_class = 'qa'` — typically
    /// because a specialist or integration-owner handoff signalled
    /// `ready_for: qa` and the kernel promoted the item.
    DirectQaRequest,
    /// A successful integration record exists for the work item and no
    /// QA verdict has been filed since. The QA-gate role is expected to
    /// verify the consolidated branch/draft PR.
    IntegrationConsolidated,
}

impl QaQueueCause {
    /// Stable lowercase identifier used in event payloads and tests.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::DirectQaRequest => "direct_qa_request",
            Self::IntegrationConsolidated => "integration_consolidated",
        }
    }
}

/// One queue entry: the work item and why it qualified.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct QaQueueEntry {
    /// The work item the QA-gate role should pick up.
    pub work_item: WorkItemRecord,
    /// Why this item qualifies right now.
    pub cause: QaQueueCause,
}

impl QaQueueEntry {
    /// Convenience accessor for the work item id.
    pub fn id(&self) -> WorkItemId {
        self.work_item.id
    }
}

/// Workflow-level gate switches that decide which items the QA queue
/// surfaces (SPEC v2 §5.12).
///
/// The default matches the SPEC: blocker gate *on*. A workflow may waive
/// it by setting [`Self::require_no_open_blockers`] to `false`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct QaQueueGates {
    /// When `true` (default), any open `blocks` edge anywhere in the
    /// work item's subtree excludes it from the queue. When `false`,
    /// the gate is waived and the blocker-subtree check is skipped.
    pub require_no_open_blockers: bool,
}

impl Default for QaQueueGates {
    fn default() -> Self {
        Self {
            require_no_open_blockers: true,
        }
    }
}

/// Read-only view over [`QA queue`](self) candidates.
pub trait QaQueueRepository {
    /// Return every work item currently ready for the QA-gate role under
    /// the SPEC defaults (blocker gate on), ordered FIFO by
    /// `created_at` then `id` for determinism.
    fn list_ready_for_qa(&self) -> StateResult<Vec<QaQueueEntry>> {
        self.list_ready_for_qa_with_gates(QaQueueGates::default())
    }

    /// Return every work item ready for the QA-gate role under the
    /// supplied [`QaQueueGates`]. Workflows pass a non-default value to
    /// explicitly waive the open-blocker gate.
    fn list_ready_for_qa_with_gates(&self, gates: QaQueueGates) -> StateResult<Vec<QaQueueEntry>>;
}

const TERMINAL_CLASSES: &str = "('done','cancelled')";

pub(crate) fn list_ready_for_qa_in(
    conn: &Connection,
    gates: QaQueueGates,
) -> StateResult<Vec<QaQueueEntry>> {
    // Two cause sources:
    //   `direct`     — work_items.status_class = 'qa'
    //   `integrated` — latest succeeded integration_record exists AND
    //                  no qa_verdict has been filed since
    //
    // The `integrated` source is computed via correlated subqueries
    // against `integration_records` and `qa_verdicts` so the union stays
    // a single statement. Cause precedence (direct beats integrated) is
    // enforced by excluding direct ids from the integrated branch, the
    // same shape as the integration queue.
    let blocker_filter = if gates.require_no_open_blockers {
        "AND NOT EXISTS ( \
             WITH RECURSIVE descendants(id) AS ( \
                 SELECT w.id \
                 UNION \
                 SELECT e.child_id \
                   FROM work_item_edges e \
                   JOIN descendants d ON e.parent_id = d.id \
                  WHERE e.edge_type = 'parent_child' \
             ) \
             SELECT 1 \
               FROM work_item_edges b \
              WHERE b.edge_type = 'blocks' \
                AND b.status = 'open' \
                AND b.child_id IN (SELECT id FROM descendants) \
         )"
    } else {
        ""
    };
    let sql = format!(
        "WITH \
         direct AS ( \
             SELECT id AS work_item_id, 'direct_qa_request' AS cause \
               FROM work_items \
              WHERE status_class = 'qa' \
         ), \
         integrated AS ( \
             SELECT w.id AS work_item_id, 'integration_consolidated' AS cause \
               FROM work_items w \
              WHERE w.status_class NOT IN {terminal} \
                AND EXISTS ( \
                    SELECT 1 FROM integration_records ir \
                     WHERE ir.parent_work_item_id = w.id \
                       AND ir.status = 'succeeded' \
                ) \
                AND NOT EXISTS ( \
                    SELECT 1 FROM qa_verdicts q \
                     WHERE q.work_item_id = w.id \
                       AND q.created_at >= ( \
                           SELECT MAX(ir2.created_at) \
                             FROM integration_records ir2 \
                            WHERE ir2.parent_work_item_id = w.id \
                              AND ir2.status = 'succeeded' \
                       ) \
                ) \
         ), \
         candidates AS ( \
             SELECT work_item_id, cause FROM direct \
             UNION \
             SELECT work_item_id, cause FROM integrated \
              WHERE work_item_id NOT IN (SELECT work_item_id FROM direct) \
         ) \
         SELECT {cols}, c.cause \
           FROM candidates c \
           JOIN work_items w ON w.id = c.work_item_id \
          WHERE w.status_class NOT IN {terminal} \
            {blocker_filter} \
          ORDER BY w.created_at ASC, w.id ASC",
        cols = prefixed_columns("w"),
        terminal = TERMINAL_CLASSES,
    );
    let mut stmt = conn.prepare(&sql)?;
    let rows = stmt.query_map([], |row| {
        let work_item = map_work_item(row)?;
        let raw_cause: String = row.get(14)?;
        let cause = match raw_cause.as_str() {
            "direct_qa_request" => QaQueueCause::DirectQaRequest,
            "integration_consolidated" => QaQueueCause::IntegrationConsolidated,
            other => {
                return Err(rusqlite::Error::FromSqlConversionFailure(
                    14,
                    rusqlite::types::Type::Text,
                    format!("unknown qa queue cause `{other}`").into(),
                ));
            }
        };
        Ok(QaQueueEntry { work_item, cause })
    })?;
    let mut out = Vec::new();
    for row in rows {
        out.push(row?);
    }
    Ok(out)
}

fn prefixed_columns(prefix: &str) -> String {
    WORK_ITEM_COLUMNS
        .split(", ")
        .map(|c| format!("{prefix}.{c}"))
        .collect::<Vec<_>>()
        .join(", ")
}

impl QaQueueRepository for StateDb {
    fn list_ready_for_qa_with_gates(&self, gates: QaQueueGates) -> StateResult<Vec<QaQueueEntry>> {
        list_ready_for_qa_in(self.conn(), gates)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::edges::{EdgeType, NewWorkItemEdge, WorkItemEdgeRepository};
    use crate::integration_records::{IntegrationRecordRepository, NewIntegrationRecord};
    use crate::migrations::migrations;
    use crate::repository::{NewRun, NewWorkItem, RunId, RunRepository, WorkItemRepository};

    fn open() -> StateDb {
        let mut db = StateDb::open_in_memory().expect("open");
        db.migrate(migrations()).expect("migrate");
        db
    }

    fn seed_item(
        db: &mut StateDb,
        identifier: &str,
        status_class: &str,
        created_at: &str,
    ) -> WorkItemId {
        db.create_work_item(NewWorkItem {
            tracker_id: "github",
            identifier,
            parent_id: None,
            title: "demo",
            status_class,
            tracker_status: "open",
            assigned_role: None,
            assigned_agent: None,
            priority: None,
            workspace_policy: None,
            branch_policy: None,
            now: created_at,
        })
        .expect("seed work item")
        .id
    }

    fn link_parent_child(db: &mut StateDb, parent: WorkItemId, child: WorkItemId) {
        db.create_edge(NewWorkItemEdge {
            parent_id: parent,
            child_id: child,
            edge_type: EdgeType::ParentChild,
            reason: None,
            status: "linked",
            now: "2026-05-08T00:00:00Z",
        })
        .expect("link parent_child");
    }

    fn open_blocker(db: &mut StateDb, blocker: WorkItemId, blocked: WorkItemId) {
        db.create_edge(NewWorkItemEdge {
            parent_id: blocker,
            child_id: blocked,
            edge_type: EdgeType::Blocks,
            reason: Some("test"),
            status: "open",
            now: "2026-05-08T00:00:00Z",
        })
        .expect("open blocker");
    }

    fn seed_run(db: &mut StateDb, work_item: WorkItemId, now: &str) -> RunId {
        db.create_run(NewRun {
            work_item_id: work_item,
            role: "platform_lead",
            agent: "claude",
            status: "succeeded",
            workspace_claim_id: None,
            now,
        })
        .expect("seed run")
        .id
    }

    fn seed_succeeded_integration(db: &mut StateDb, parent: WorkItemId, run: RunId, now: &str) {
        db.create_integration_record(NewIntegrationRecord {
            parent_work_item_id: parent,
            owner_role: "platform_lead",
            run_id: Some(run),
            status: "succeeded",
            merge_strategy: "merge_commits",
            integration_branch: Some("integration/parent-1"),
            base_ref: Some("main"),
            head_sha: Some("deadbeef"),
            workspace_path: None,
            children_consolidated: None,
            summary: Some("ok"),
            conflicts: None,
            blockers_filed: None,
            now,
        })
        .expect("seed integration record");
    }

    fn seed_qa_verdict(db: &mut StateDb, work_item: WorkItemId, run: RunId, now: &str) {
        db.conn()
            .execute(
                "INSERT INTO qa_verdicts \
                    (work_item_id, run_id, verdict, evidence, acceptance_trace, \
                     blockers_created, created_at) \
                 VALUES (?1, ?2, 'pass', NULL, NULL, NULL, ?3)",
                rusqlite::params![work_item.0, run.0, now],
            )
            .expect("insert qa_verdict");
    }

    #[test]
    fn empty_db_returns_empty_queue() {
        let db = open();
        assert!(db.list_ready_for_qa().unwrap().is_empty());
    }

    #[test]
    fn direct_qa_status_appears_in_queue() {
        let mut db = open();
        let id = seed_item(&mut db, "ENG-1", "qa", "2026-05-08T00:00:00Z");
        let queue = db.list_ready_for_qa().unwrap();
        assert_eq!(queue.len(), 1);
        assert_eq!(queue[0].id(), id);
        assert_eq!(queue[0].cause, QaQueueCause::DirectQaRequest);
    }

    #[test]
    fn succeeded_integration_record_surfaces_parent() {
        let mut db = open();
        let parent = seed_item(&mut db, "ENG-1", "integration", "2026-05-08T00:00:00Z");
        let run = seed_run(&mut db, parent, "2026-05-08T00:01:00Z");
        seed_succeeded_integration(&mut db, parent, run, "2026-05-08T00:02:00Z");

        let queue = db.list_ready_for_qa().unwrap();
        assert_eq!(queue.len(), 1);
        assert_eq!(queue[0].id(), parent);
        assert_eq!(queue[0].cause, QaQueueCause::IntegrationConsolidated);
    }

    #[test]
    fn direct_cause_takes_precedence_over_integration_consolidated() {
        let mut db = open();
        // Parent already promoted to `qa` AND has a successful integration.
        let parent = seed_item(&mut db, "ENG-1", "qa", "2026-05-08T00:00:00Z");
        let run = seed_run(&mut db, parent, "2026-05-08T00:01:00Z");
        seed_succeeded_integration(&mut db, parent, run, "2026-05-08T00:02:00Z");

        let queue = db.list_ready_for_qa().unwrap();
        assert_eq!(queue.len(), 1);
        assert_eq!(queue[0].id(), parent);
        assert_eq!(queue[0].cause, QaQueueCause::DirectQaRequest);
    }

    #[test]
    fn unsucceeded_integration_record_does_not_surface_parent() {
        let mut db = open();
        let parent = seed_item(&mut db, "ENG-1", "integration", "2026-05-08T00:00:00Z");
        let run = seed_run(&mut db, parent, "2026-05-08T00:01:00Z");
        // Pending integration — not yet succeeded.
        db.create_integration_record(NewIntegrationRecord {
            parent_work_item_id: parent,
            owner_role: "platform_lead",
            run_id: Some(run),
            status: "in_progress",
            merge_strategy: "merge_commits",
            integration_branch: None,
            base_ref: Some("main"),
            head_sha: None,
            workspace_path: None,
            children_consolidated: None,
            summary: None,
            conflicts: None,
            blockers_filed: None,
            now: "2026-05-08T00:02:00Z",
        })
        .unwrap();

        assert!(db.list_ready_for_qa().unwrap().is_empty());
    }

    #[test]
    fn qa_verdict_after_integration_excludes_parent() {
        let mut db = open();
        let parent = seed_item(&mut db, "ENG-1", "integration", "2026-05-08T00:00:00Z");
        let run = seed_run(&mut db, parent, "2026-05-08T00:01:00Z");
        seed_succeeded_integration(&mut db, parent, run, "2026-05-08T00:02:00Z");
        seed_qa_verdict(&mut db, parent, run, "2026-05-08T00:03:00Z");

        // QA already verdicted; queue does not re-surface the integration cause.
        assert!(db.list_ready_for_qa().unwrap().is_empty());
    }

    #[test]
    fn re_integration_after_qa_verdict_resurfaces_parent() {
        let mut db = open();
        let parent = seed_item(&mut db, "ENG-1", "integration", "2026-05-08T00:00:00Z");
        let r1 = seed_run(&mut db, parent, "2026-05-08T00:01:00Z");
        seed_succeeded_integration(&mut db, parent, r1, "2026-05-08T00:02:00Z");
        seed_qa_verdict(&mut db, parent, r1, "2026-05-08T00:03:00Z");
        // Rework lands and integration owner re-runs successfully.
        let r2 = seed_run(&mut db, parent, "2026-05-08T00:04:00Z");
        seed_succeeded_integration(&mut db, parent, r2, "2026-05-08T00:05:00Z");

        let queue = db.list_ready_for_qa().unwrap();
        assert_eq!(queue.len(), 1);
        assert_eq!(queue[0].id(), parent);
        assert_eq!(queue[0].cause, QaQueueCause::IntegrationConsolidated);
    }

    #[test]
    fn terminal_work_item_is_not_queued() {
        let mut db = open();
        let parent = seed_item(&mut db, "ENG-1", "done", "2026-05-08T00:00:00Z");
        let run = seed_run(&mut db, parent, "2026-05-08T00:01:00Z");
        seed_succeeded_integration(&mut db, parent, run, "2026-05-08T00:02:00Z");
        assert!(db.list_ready_for_qa().unwrap().is_empty());
    }

    #[test]
    fn terminal_qa_status_class_is_not_queued() {
        // `cancelled` parent in `qa` should never be picked up.
        let mut db = open();
        seed_item(&mut db, "ENG-1", "cancelled", "2026-05-08T00:00:00Z");
        assert!(db.list_ready_for_qa().unwrap().is_empty());
    }

    #[test]
    fn open_blocker_in_subtree_excludes_parent() {
        let mut db = open();
        let parent = seed_item(&mut db, "ENG-1", "qa", "2026-05-08T00:00:00Z");
        let blocker = seed_item(&mut db, "ENG-9", "blocked", "2026-05-08T00:00:05Z");
        open_blocker(&mut db, blocker, parent);

        assert!(db.list_ready_for_qa().unwrap().is_empty());
    }

    #[test]
    fn open_blocker_on_descendant_excludes_parent() {
        let mut db = open();
        let parent = seed_item(&mut db, "ENG-1", "qa", "2026-05-08T00:00:00Z");
        let child = seed_item(&mut db, "ENG-2", "done", "2026-05-08T00:00:01Z");
        link_parent_child(&mut db, parent, child);
        let blocker = seed_item(&mut db, "ENG-9", "blocked", "2026-05-08T00:00:02Z");
        open_blocker(&mut db, blocker, child);

        assert!(db.list_ready_for_qa().unwrap().is_empty());
    }

    #[test]
    fn resolved_blocker_does_not_exclude_parent() {
        let mut db = open();
        let parent = seed_item(&mut db, "ENG-1", "qa", "2026-05-08T00:00:00Z");
        let blocker = seed_item(&mut db, "ENG-9", "done", "2026-05-08T00:00:05Z");
        let edge = db
            .create_edge(NewWorkItemEdge {
                parent_id: blocker,
                child_id: parent,
                edge_type: EdgeType::Blocks,
                reason: Some("test"),
                status: "open",
                now: "2026-05-08T00:00:00Z",
            })
            .unwrap();
        assert!(db.update_edge_status(edge.id, "resolved").unwrap());

        let queue = db.list_ready_for_qa().unwrap();
        assert_eq!(queue.len(), 1);
        assert_eq!(queue[0].id(), parent);
    }

    #[test]
    fn waived_blocker_gate_surfaces_parent_with_open_blocker() {
        let mut db = open();
        let parent = seed_item(&mut db, "ENG-1", "qa", "2026-05-08T00:00:00Z");
        let blocker = seed_item(&mut db, "ENG-9", "blocked", "2026-05-08T00:00:05Z");
        open_blocker(&mut db, blocker, parent);

        // Default gate: hidden.
        assert!(db.list_ready_for_qa().unwrap().is_empty());

        // Waiver: blocker gate off.
        let waived = QaQueueGates {
            require_no_open_blockers: false,
        };
        let queue = db.list_ready_for_qa_with_gates(waived).unwrap();
        assert_eq!(queue.len(), 1);
        assert_eq!(queue[0].id(), parent);
        assert_eq!(queue[0].cause, QaQueueCause::DirectQaRequest);
    }

    #[test]
    fn fifo_ordering_by_created_at() {
        let mut db = open();
        let older = seed_item(&mut db, "ENG-1", "qa", "2026-05-08T00:00:00Z");
        let newer = seed_item(&mut db, "ENG-2", "qa", "2026-05-08T01:00:00Z");
        let queue = db.list_ready_for_qa().unwrap();
        assert_eq!(queue.len(), 2);
        assert_eq!(queue[0].id(), older);
        assert_eq!(queue[1].id(), newer);
    }

    #[test]
    fn default_gates_match_spec() {
        let gates = QaQueueGates::default();
        assert!(gates.require_no_open_blockers);
    }

    #[test]
    fn cause_round_trips_through_as_str() {
        for cause in [
            QaQueueCause::DirectQaRequest,
            QaQueueCause::IntegrationConsolidated,
        ] {
            let s = cause.as_str();
            let back = match s {
                "direct_qa_request" => QaQueueCause::DirectQaRequest,
                "integration_consolidated" => QaQueueCause::IntegrationConsolidated,
                _ => unreachable!(),
            };
            assert_eq!(back, cause);
        }
    }
}
