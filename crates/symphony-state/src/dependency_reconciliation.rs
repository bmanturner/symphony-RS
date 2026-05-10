//! Reconciliation helpers for v3 decomposition dependency blockers.
//!
//! Decomposition-created `blocks` edges are safe to auto-resolve when
//! their prerequisite child reaches a terminal status class. Other
//! blocker sources remain manual: QA, follow-up, and human blockers carry
//! independent intent and must not disappear because the blocking work
//! item is terminal.

use symphony_core::work_item::WorkItemStatusClass;

use crate::edges::WorkItemEdgeRepository;
use crate::repository::WorkItemRepository;
use crate::{StateDb, StateResult};

/// Summary returned by one reconciliation pass.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DependencyReconciliationReport {
    /// Decomposition blocker edges changed from `open` to `resolved`.
    pub resolved_edges: usize,
    /// Blocked children released to `ready` because no open blockers
    /// remained after edge resolution.
    pub released_children: usize,
}

/// Resolve decomposition blockers whose blocker child is terminal.
///
/// The query source is
/// [`WorkItemEdgeRepository::list_decomposition_blockers_with_terminal_blocker`],
/// which filters to open decomposition-sourced `blocks` edges and
/// terminal blocker rows. After each edge resolves, the blocked child is
/// released to `ready` only if it has no remaining incoming open
/// blockers. This preserves the `A, B -> C` join semantics.
pub fn reconcile_terminal_decomposition_blockers(
    db: &mut StateDb,
    now: &str,
) -> StateResult<DependencyReconciliationReport> {
    let eligible = db.list_decomposition_blockers_with_terminal_blocker()?;
    let mut resolved_edges = 0usize;
    let mut released_children = 0usize;

    for edge in eligible {
        if db.update_edge_status(edge.id, "resolved")? {
            resolved_edges += 1;
        }

        if db.list_incoming_open_blockers(edge.child_id)?.is_empty()
            && db.update_work_item_status(
                edge.child_id,
                WorkItemStatusClass::Ready.as_str(),
                "ready",
                now,
            )?
        {
            released_children += 1;
        }
    }

    Ok(DependencyReconciliationReport {
        resolved_edges,
        released_children,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::edges::{EdgeSource, EdgeType, NewDecompositionBlockerEdge, NewWorkItemEdge};
    use crate::migrations::migrations;
    use crate::repository::{NewWorkItem, WorkItemId};

    fn open() -> StateDb {
        let mut db = StateDb::open_in_memory().expect("open");
        db.migrate(migrations()).expect("migrate");
        db
    }

    fn seed_item(db: &mut StateDb, identifier: &str, status_class: &str) -> WorkItemId {
        db.create_work_item(NewWorkItem {
            tracker_id: "github",
            identifier,
            parent_id: None,
            title: "demo",
            status_class,
            tracker_status: status_class,
            assigned_role: None,
            assigned_agent: None,
            priority: None,
            workspace_policy: None,
            branch_policy: None,
            now: "2026-05-10T00:00:00Z",
        })
        .expect("seed work item")
        .id
    }

    #[test]
    fn terminal_decomposition_blocker_unblocks_dependent_child() {
        let mut db = open();
        let a = seed_item(&mut db, "A", "done");
        let b = seed_item(&mut db, "B", "blocked");
        let edge = db
            .create_decomposition_blocker_edges(&[NewDecompositionBlockerEdge {
                blocker_id: a,
                blocked_id: b,
                reason: "B depends on A",
                now: "2026-05-10T00:00:00Z",
            }])
            .unwrap()
            .remove(0);

        let report =
            reconcile_terminal_decomposition_blockers(&mut db, "2026-05-10T00:01:00Z").unwrap();

        assert_eq!(
            report,
            DependencyReconciliationReport {
                resolved_edges: 1,
                released_children: 1,
            }
        );
        assert_eq!(db.get_edge(edge.id).unwrap().unwrap().status, "resolved");
        let b_after = db.get_work_item(b).unwrap().unwrap();
        assert_eq!(b_after.status_class, "ready");
        assert_eq!(b_after.tracker_status, "ready");
    }

    #[test]
    fn join_child_waits_until_all_decomposition_blockers_resolve() {
        let mut db = open();
        let a = seed_item(&mut db, "A", "done");
        let b = seed_item(&mut db, "B", "ready");
        let c = seed_item(&mut db, "C", "blocked");
        db.create_decomposition_blocker_edges(&[
            NewDecompositionBlockerEdge {
                blocker_id: a,
                blocked_id: c,
                reason: "C depends on A",
                now: "2026-05-10T00:00:00Z",
            },
            NewDecompositionBlockerEdge {
                blocker_id: b,
                blocked_id: c,
                reason: "C depends on B",
                now: "2026-05-10T00:00:00Z",
            },
        ])
        .unwrap();

        let report =
            reconcile_terminal_decomposition_blockers(&mut db, "2026-05-10T00:01:00Z").unwrap();

        assert_eq!(report.resolved_edges, 1);
        assert_eq!(report.released_children, 0);
        assert_eq!(
            db.get_work_item(c).unwrap().unwrap().status_class,
            "blocked"
        );
        assert_eq!(db.list_incoming_open_blockers(c).unwrap().len(), 1);
    }

    #[test]
    fn qa_and_human_blockers_do_not_auto_resolve_with_terminal_blocker() {
        let mut db = open();
        let blocker = seed_item(&mut db, "QA", "done");
        let blocked = seed_item(&mut db, "B", "blocked");
        let qa_edge = db
            .create_edge(NewWorkItemEdge {
                parent_id: blocker,
                child_id: blocked,
                edge_type: EdgeType::Blocks,
                reason: Some("QA found a regression"),
                status: "open",
                source: EdgeSource::Qa,
                now: "2026-05-10T00:00:00Z",
            })
            .unwrap();

        let report =
            reconcile_terminal_decomposition_blockers(&mut db, "2026-05-10T00:01:00Z").unwrap();

        assert_eq!(report.resolved_edges, 0);
        assert_eq!(report.released_children, 0);
        assert_eq!(db.get_edge(qa_edge.id).unwrap().unwrap().status, "open");
        assert_eq!(
            db.get_work_item(blocked).unwrap().unwrap().status_class,
            "blocked"
        );
    }
}
