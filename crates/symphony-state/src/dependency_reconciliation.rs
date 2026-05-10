//! Reconciliation helpers for v3 decomposition dependency blockers.
//!
//! Decomposition-created `blocks` edges are safe to auto-resolve when
//! their prerequisite child reaches a terminal status class. Other
//! blocker sources remain manual: QA, follow-up, and human blockers carry
//! independent intent and must not disappear because the blocking work
//! item is terminal.

use symphony_core::work_item::WorkItemStatusClass;

use crate::edges::{
    EdgeId, TrackerEdgeSyncFailure, TrackerEdgeSyncSuccess, WorkItemEdgeRecord,
    WorkItemEdgeRepository,
};
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
    /// Safer-blocked disagreements observed while comparing terminal
    /// local children with tracker-side structural blocker state.
    pub warnings: Vec<DependencyReconciliationWarning>,
}

/// Summary returned by a tracker sync retry pass.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DependencySyncRetryReport {
    /// Failed dependency-edge syncs selected for retry.
    pub attempted: usize,
    /// Retries that succeeded and updated tracker mirror metadata.
    pub succeeded: usize,
    /// Retries that failed again and incremented retry metadata.
    pub failed: usize,
}

/// Warning emitted by a reconciliation pass.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DependencyReconciliationWarning {
    /// Local dependency edge that stayed open.
    pub edge_id: EdgeId,
    /// Operator-facing explanation.
    pub message: String,
}

/// Tracker-side blocker state observed during reconciliation.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TrackerDependencyState {
    /// Tracker still reports the structural blocker relation open.
    Open,
    /// Tracker agrees the relation no longer blocks.
    Resolved,
    /// Tracker cannot currently answer. The local terminal state is used.
    Unknown,
}

/// Probe used by reconciliation to compare local and tracker truth.
pub trait TrackerDependencyStateProvider {
    /// Return the current tracker-side state for `edge`.
    fn dependency_state(&self, edge: &WorkItemEdgeRecord) -> StateResult<TrackerDependencyState>;
}

/// Result of retrying one tracker dependency mirror.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DependencySyncRetryOutcome {
    /// Tracker mutation succeeded. Carries a tracker-native relation id
    /// when the adapter exposes one.
    Synced {
        /// Tracker-native dependency/relation id, if the adapter returns one.
        tracker_edge_id: Option<String>,
    },
    /// Tracker mutation failed again. The string is persisted as the
    /// latest retry error.
    Failed {
        /// Operator-facing error from the failed retry attempt.
        error: String,
    },
}

/// Hook used to retry failed structural tracker blocker syncs.
pub trait DependencySyncRetryer {
    /// Retry syncing `edge` to the tracker.
    fn retry_edge(&self, edge: &WorkItemEdgeRecord) -> StateResult<DependencySyncRetryOutcome>;
}

#[derive(Debug, Default)]
struct LocalOnlyTrackerDependencyState;

impl TrackerDependencyStateProvider for LocalOnlyTrackerDependencyState {
    fn dependency_state(&self, _edge: &WorkItemEdgeRecord) -> StateResult<TrackerDependencyState> {
        Ok(TrackerDependencyState::Unknown)
    }
}

/// Resolve decomposition blockers whose blocker child is terminal using
/// local state when no tracker probe is available.
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
    reconcile_terminal_decomposition_blockers_with_tracker(
        db,
        now,
        &LocalOnlyTrackerDependencyState,
    )
}

/// Resolve terminal decomposition blockers after probing tracker state.
///
/// If tracker state says the structural blocker relation is still open,
/// local reconciliation chooses the safer state: the edge remains open,
/// the child stays blocked, and a warning is returned for operator
/// surfaces to persist or display.
pub fn reconcile_terminal_decomposition_blockers_with_tracker(
    db: &mut StateDb,
    now: &str,
    tracker: &dyn TrackerDependencyStateProvider,
) -> StateResult<DependencyReconciliationReport> {
    let eligible = db.list_decomposition_blockers_with_terminal_blocker()?;
    let mut resolved_edges = 0usize;
    let mut released_children = 0usize;
    let mut warnings = Vec::new();

    for edge in eligible {
        if tracker.dependency_state(&edge)? == TrackerDependencyState::Open {
            warnings.push(DependencyReconciliationWarning {
                edge_id: edge.id,
                message:
                    "local blocker child is terminal, but tracker blocker relation is still open"
                        .into(),
            });
            continue;
        }

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
        warnings,
    })
}

/// Retry failed tracker-edge syncs for open decomposition blockers.
///
/// The retryer owns tracker-specific mutation details. This helper owns
/// the durable retry bookkeeping: success records `synced`, failure
/// records `failed`, and both paths increment the edge attempt counter.
pub fn retry_failed_dependency_edge_syncs(
    db: &mut StateDb,
    now: &str,
    retryer: &dyn DependencySyncRetryer,
) -> StateResult<DependencySyncRetryReport> {
    let failed_edges = failed_dependency_sync_edges(db)?;
    let mut report = DependencySyncRetryReport {
        attempted: failed_edges.len(),
        succeeded: 0,
        failed: 0,
    };

    for edge in failed_edges {
        match retryer.retry_edge(&edge)? {
            DependencySyncRetryOutcome::Synced { tracker_edge_id } => {
                db.record_tracker_edge_sync_success(TrackerEdgeSyncSuccess {
                    edge_id: edge.id,
                    tracker_edge_id: tracker_edge_id.as_deref(),
                    attempted_at: now,
                })?;
                report.succeeded += 1;
            }
            DependencySyncRetryOutcome::Failed { error } => {
                db.record_tracker_edge_sync_failure(TrackerEdgeSyncFailure {
                    edge_id: edge.id,
                    error: &error,
                    attempted_at: now,
                })?;
                report.failed += 1;
            }
        }
    }

    Ok(report)
}

fn failed_dependency_sync_edges(db: &StateDb) -> StateResult<Vec<WorkItemEdgeRecord>> {
    let ids = {
        let mut stmt = db.conn().prepare(
            "SELECT id FROM work_item_edges
              WHERE edge_type = 'blocks'
                AND status = 'open'
                AND source = 'decomposition'
                AND tracker_sync_status = 'failed'
              ORDER BY id ASC",
        )?;
        let rows = stmt.query_map([], |row| row.get::<_, i64>(0))?;
        let mut ids = Vec::new();
        for row in rows {
            ids.push(EdgeId(row?));
        }
        ids
    };

    let mut edges = Vec::with_capacity(ids.len());
    for id in ids {
        if let Some(edge) = db.get_edge(id)? {
            edges.push(edge);
        }
    }
    Ok(edges)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::edges::{EdgeSource, EdgeType, NewDecompositionBlockerEdge, NewWorkItemEdge};
    use crate::integration_queue::IntegrationQueueRepository;
    use crate::integration_records::{IntegrationRecordRepository, NewIntegrationRecord};
    use crate::migrations::migrations;
    use crate::qa_queue::QaQueueRepository;
    use crate::repository::{NewRun, NewWorkItem, RunRepository, WorkItemId};

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

    fn seed_run(db: &mut StateDb, work_item: WorkItemId, role: &str, now: &str) {
        db.create_run(NewRun {
            work_item_id: work_item,
            role,
            agent: "codex",
            status: "completed",
            workspace_claim_id: None,
            now,
        })
        .expect("seed run");
    }

    fn link_parent_child(db: &mut StateDb, parent: WorkItemId, child: WorkItemId) {
        db.create_edge(NewWorkItemEdge {
            parent_id: parent,
            child_id: child,
            edge_type: EdgeType::ParentChild,
            reason: Some("decomposition child"),
            status: "linked",
            source: EdgeSource::Decomposition,
            now: "2026-05-10T00:00:00Z",
        })
        .expect("parent child edge");
    }

    fn succeeded_integration(db: &mut StateDb, parent: WorkItemId, now: &str) {
        db.create_integration_record(NewIntegrationRecord {
            parent_work_item_id: parent,
            owner_role: "platform_lead",
            run_id: None,
            status: "succeeded",
            merge_strategy: "merge_commits",
            integration_branch: Some("integration/parent"),
            base_ref: Some("main"),
            head_sha: Some("deadbeef"),
            workspace_path: None,
            children_consolidated: Some("[]"),
            summary: Some("integrated children"),
            conflicts: None,
            blockers_filed: None,
            now,
        })
        .expect("integration record");
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
                warnings: Vec::new(),
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

    #[derive(Debug)]
    struct FixedTrackerState(TrackerDependencyState);

    impl TrackerDependencyStateProvider for FixedTrackerState {
        fn dependency_state(
            &self,
            _edge: &WorkItemEdgeRecord,
        ) -> StateResult<TrackerDependencyState> {
            Ok(self.0)
        }
    }

    #[test]
    fn tracker_local_disagreement_keeps_dependent_child_blocked() {
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

        let report = reconcile_terminal_decomposition_blockers_with_tracker(
            &mut db,
            "2026-05-10T00:01:00Z",
            &FixedTrackerState(TrackerDependencyState::Open),
        )
        .unwrap();

        assert_eq!(report.resolved_edges, 0);
        assert_eq!(report.released_children, 0);
        assert_eq!(report.warnings.len(), 1);
        assert_eq!(report.warnings[0].edge_id, edge.id);
        assert_eq!(db.get_edge(edge.id).unwrap().unwrap().status, "open");
        assert_eq!(
            db.get_work_item(b).unwrap().unwrap().status_class,
            "blocked"
        );
    }

    #[derive(Debug)]
    struct FixedRetryer(DependencySyncRetryOutcome);

    impl DependencySyncRetryer for FixedRetryer {
        fn retry_edge(
            &self,
            _edge: &WorkItemEdgeRecord,
        ) -> StateResult<DependencySyncRetryOutcome> {
            Ok(self.0.clone())
        }
    }

    #[test]
    fn failed_dependency_sync_retry_success_records_eventual_success() {
        let mut db = open();
        let a = seed_item(&mut db, "A", "ready");
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
        db.record_tracker_edge_sync_failure(TrackerEdgeSyncFailure {
            edge_id: edge.id,
            error: "Linear timeout",
            attempted_at: "2026-05-10T00:01:00Z",
        })
        .unwrap();

        let report = retry_failed_dependency_edge_syncs(
            &mut db,
            "2026-05-10T00:02:00Z",
            &FixedRetryer(DependencySyncRetryOutcome::Synced {
                tracker_edge_id: Some("rel-2".into()),
            }),
        )
        .unwrap();

        assert_eq!(
            report,
            DependencySyncRetryReport {
                attempted: 1,
                succeeded: 1,
                failed: 0,
            }
        );
        let fetched = db.get_edge(edge.id).unwrap().unwrap();
        assert_eq!(fetched.tracker_edge_id.as_deref(), Some("rel-2"));
        assert_eq!(fetched.tracker_sync_status.as_deref(), Some("synced"));
        assert_eq!(fetched.tracker_sync_last_error, None);
        assert_eq!(fetched.tracker_sync_attempts, 2);
    }

    #[test]
    fn failed_dependency_sync_retry_failure_increments_counter() {
        let mut db = open();
        let a = seed_item(&mut db, "A", "ready");
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
        db.record_tracker_edge_sync_failure(TrackerEdgeSyncFailure {
            edge_id: edge.id,
            error: "first failure",
            attempted_at: "2026-05-10T00:01:00Z",
        })
        .unwrap();

        let report = retry_failed_dependency_edge_syncs(
            &mut db,
            "2026-05-10T00:02:00Z",
            &FixedRetryer(DependencySyncRetryOutcome::Failed {
                error: "still down".into(),
            }),
        )
        .unwrap();

        assert_eq!(report.attempted, 1);
        assert_eq!(report.succeeded, 0);
        assert_eq!(report.failed, 1);
        let fetched = db.get_edge(edge.id).unwrap().unwrap();
        assert_eq!(fetched.tracker_sync_status.as_deref(), Some("failed"));
        assert_eq!(
            fetched.tracker_sync_last_error.as_deref(),
            Some("still down")
        );
        assert_eq!(fetched.tracker_sync_attempts, 2);
    }

    #[test]
    fn fake_e2e_sequential_chain_reaches_integration_and_qa() {
        let mut db = open();
        let parent = seed_item(&mut db, "P", "running");
        let a = seed_item(&mut db, "A", "ready");
        let b = seed_item(&mut db, "B", "blocked");
        let c = seed_item(&mut db, "C", "blocked");
        for child in [a, b, c] {
            link_parent_child(&mut db, parent, child);
        }
        db.create_decomposition_blocker_edges(&[
            NewDecompositionBlockerEdge {
                blocker_id: a,
                blocked_id: b,
                reason: "B depends on A",
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

        assert_eq!(db.list_incoming_open_blockers(a).unwrap().len(), 0);
        assert_eq!(db.list_incoming_open_blockers(b).unwrap().len(), 1);
        assert_eq!(db.list_incoming_open_blockers(c).unwrap().len(), 1);

        seed_run(&mut db, a, "backend", "2026-05-10T00:01:00Z");
        db.update_work_item_status(a, "done", "done", "2026-05-10T00:02:00Z")
            .unwrap();
        reconcile_terminal_decomposition_blockers(&mut db, "2026-05-10T00:03:00Z").unwrap();
        assert_eq!(db.get_work_item(b).unwrap().unwrap().status_class, "ready");

        seed_run(&mut db, b, "backend", "2026-05-10T00:04:00Z");
        db.update_work_item_status(b, "done", "done", "2026-05-10T00:05:00Z")
            .unwrap();
        reconcile_terminal_decomposition_blockers(&mut db, "2026-05-10T00:06:00Z").unwrap();
        assert_eq!(db.get_work_item(c).unwrap().unwrap().status_class, "ready");

        seed_run(&mut db, c, "frontend", "2026-05-10T00:07:00Z");
        db.update_work_item_status(c, "done", "done", "2026-05-10T00:08:00Z")
            .unwrap();
        assert_eq!(db.list_ready_for_integration().unwrap()[0].id(), parent);

        succeeded_integration(&mut db, parent, "2026-05-10T00:09:00Z");
        assert_eq!(db.list_ready_for_qa().unwrap()[0].id(), parent);

        db.update_work_item_status(parent, "done", "done", "2026-05-10T00:10:00Z")
            .unwrap();
        assert!(db.list_ready_for_integration().unwrap().is_empty());
    }

    #[test]
    fn fake_e2e_parallel_roots_join_child_waits_for_reworked_root() {
        let mut db = open();
        let parent = seed_item(&mut db, "P", "running");
        let a = seed_item(&mut db, "A", "ready");
        let b = seed_item(&mut db, "B", "ready");
        let c = seed_item(&mut db, "C", "blocked");
        for child in [a, b, c] {
            link_parent_child(&mut db, parent, child);
        }
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

        seed_run(&mut db, a, "backend", "2026-05-10T00:01:00Z");
        seed_run(&mut db, b, "frontend", "2026-05-10T00:01:00Z");
        db.update_work_item_status(a, "done", "done", "2026-05-10T00:02:00Z")
            .unwrap();
        db.update_work_item_status(b, "blocked", "qa_rework", "2026-05-10T00:02:30Z")
            .unwrap();
        reconcile_terminal_decomposition_blockers(&mut db, "2026-05-10T00:03:00Z").unwrap();

        assert_eq!(
            db.get_work_item(c).unwrap().unwrap().status_class,
            "blocked"
        );
        assert_eq!(db.list_incoming_open_blockers(c).unwrap().len(), 1);

        db.update_work_item_status(b, "done", "done", "2026-05-10T00:04:00Z")
            .unwrap();
        reconcile_terminal_decomposition_blockers(&mut db, "2026-05-10T00:05:00Z").unwrap();

        assert_eq!(db.get_work_item(c).unwrap().unwrap().status_class, "ready");
        assert!(db.list_incoming_open_blockers(c).unwrap().is_empty());
    }
}
