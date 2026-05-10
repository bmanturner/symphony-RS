//! Durable persistence for tracker-applied decompositions.
//!
//! The tracker applier in `symphony-core` creates child issues, but the
//! workflow gates depend on SQLite truth: child `work_items`,
//! `parent_child` edges, and decomposition-sourced `blocks` edges must
//! land before a proposal can be considered applied. This module owns
//! that state-layer composition.

use std::collections::HashMap;

use symphony_core::decomposition::{
    ChildKey, DecompositionProposal, DecompositionStatus, DependencyApplicationEvidence,
};
use symphony_core::decomposition_applier::{AppliedChild, AppliedDecomposition};
use symphony_core::work_item::WorkItemStatusClass;

use crate::edges::{
    EdgeSource, EdgeType, NewDecompositionBlockerEdge, NewWorkItemEdge, TrackerEdgeSyncFailure,
    TrackerEdgeSyncLocalOnly, WorkItemEdgeRecord, WorkItemEdgeRepository,
};
use crate::repository::{NewWorkItem, WorkItemId, WorkItemRecord};
use crate::{StateDb, StateError};

/// Child row persisted from one tracker-created decomposition child.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PersistedAppliedChild {
    /// Proposal-local key used by `ChildProposal.depends_on`.
    pub key: ChildKey,
    /// Newly persisted durable work item row.
    pub work_item: WorkItemRecord,
}

/// State-layer outcome of persisting an applied decomposition.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PersistedAppliedDecomposition {
    /// Persisted children in proposal declaration order.
    pub children: Vec<PersistedAppliedChild>,
    /// Local open `blocks` edges materialized from
    /// [`symphony_core::decomposition::ChildProposal::depends_on`] in
    /// dependency declaration order.
    pub dependency_edges: Vec<WorkItemEdgeRecord>,
}

impl PersistedAppliedDecomposition {
    /// Return the `ChildKey -> WorkItemId` map expected by
    /// [`DecompositionProposal::mark_applied`].
    pub fn child_work_item_ids(&self) -> HashMap<ChildKey, symphony_core::work_item::WorkItemId> {
        self.children
            .iter()
            .map(|child| {
                (
                    child.key.clone(),
                    symphony_core::work_item::WorkItemId::new(child.work_item.id.0),
                )
            })
            .collect()
    }

    /// Evidence required by [`DecompositionProposal::mark_applied`] once
    /// local dependency materialization has succeeded. Tracker sync is
    /// not required by this Phase 3 persistence helper; Phase 4's sync
    /// path can build stricter evidence after recording mirror state.
    pub fn local_dependency_evidence(&self) -> DependencyApplicationEvidence {
        DependencyApplicationEvidence {
            persisted_edge_count: self.dependency_edges.len(),
            tracker_sync_required: false,
            tracker_sync_record_count: 0,
        }
    }
}

/// Error from persisting tracker-created decomposition children.
#[derive(Debug, thiserror::Error)]
pub enum PersistAppliedDecompositionError {
    /// The proposal/applied payloads were invalid before any row mutation
    /// could safely land.
    #[error(transparent)]
    State(#[from] StateError),
    /// Child work items and parent-child edges were committed, but the
    /// dependency blocker graph failed to materialize.
    ///
    /// The children are intentionally left in `blocked` status so
    /// specialist dispatch cannot race ahead while the dependency graph is
    /// incomplete. The caller can persist proposal state as partial and
    /// retry dependency materialization using the returned child ids.
    #[error("dependency materialization failed after child persistence: {source}")]
    DependencyMaterializationFailed {
        /// Children that were durably created before the dependency
        /// failure. Parent-child edges for these rows were also committed.
        children: Vec<PersistedAppliedChild>,
        /// Underlying state-layer failure from creating blocker edges.
        #[source]
        source: StateError,
    },
    /// Dependency edges materialized, but the final status transition
    /// that releases children to the specialist queue failed.
    #[error("releasing persisted decomposition children failed: {source}")]
    ChildReleaseFailed {
        /// Fully materialized dependency edges.
        dependency_edges: Vec<WorkItemEdgeRecord>,
        /// Children that were persisted and then attempted to release.
        children: Vec<PersistedAppliedChild>,
        /// Underlying state-layer failure from the status update.
        #[source]
        source: StateError,
    },
}

/// Result alias for [`persist_applied_decomposition`].
pub type PersistAppliedDecompositionResult =
    Result<PersistedAppliedDecomposition, PersistAppliedDecompositionError>;

/// Record a required tracker blocker-sync failure and park the blocked
/// child.
///
/// `tracker_sync: required` means the local edge may exist, but the
/// downstream child must not be treated as dispatchable until the
/// structural tracker mutation succeeds or policy explicitly changes.
/// This helper keeps those two facts atomic: the edge receives durable
/// failure metadata, and the edge's `child_id` work item returns to the
/// `blocked` status class for specialist gating and operator status.
pub fn record_required_dependency_sync_failure(
    db: &mut StateDb,
    edge: &WorkItemEdgeRecord,
    error: &str,
    now: &str,
) -> crate::StateResult<()> {
    db.transaction(|tx| {
        tx.record_tracker_edge_sync_failure(TrackerEdgeSyncFailure {
            edge_id: edge.id,
            error,
            attempted_at: now,
        })?;
        tx.update_work_item_status(
            edge.child_id,
            WorkItemStatusClass::Blocked.as_str(),
            "blocked",
            now,
        )?;
        Ok(())
    })
}

/// Record a best-effort tracker blocker-sync failure without changing
/// local dispatch truth.
///
/// Under `tracker_sync: best_effort`, Symphony's local open `blocks`
/// edge remains authoritative. The failure is still durable and
/// retryable through edge sync metadata, but the blocked child's status
/// is not demoted merely because the tracker mirror failed.
pub fn record_best_effort_dependency_sync_failure(
    db: &mut StateDb,
    edge: &WorkItemEdgeRecord,
    error: &str,
    now: &str,
) -> crate::StateResult<()> {
    db.record_tracker_edge_sync_failure(TrackerEdgeSyncFailure {
        edge_id: edge.id,
        error,
        attempted_at: now,
    })?;
    Ok(())
}

/// Record that workflow policy intentionally kept a dependency blocker
/// local-only.
///
/// Under `tracker_sync: local_only`, Symphony must not call structural
/// tracker blocker mutation APIs. The local open edge remains the
/// dispatch gate, and this metadata tells operator/retry surfaces that
/// the missing tracker edge is policy, not an unattempted or failed sync.
pub fn record_local_only_dependency_sync(
    db: &mut StateDb,
    edge: &WorkItemEdgeRecord,
    now: &str,
) -> crate::StateResult<()> {
    db.record_tracker_edge_sync_local_only(TrackerEdgeSyncLocalOnly {
        edge_id: edge.id,
        recorded_at: now,
    })?;
    Ok(())
}

/// Persist tracker-created decomposition children and their local graph.
///
/// The operation is recoverable rather than all-or-nothing: child
/// `work_items` rows and `parent_child` edges land first with children
/// parked in `blocked` status. Dependency `blocks` edges then
/// materialize in their own transaction. On success, children are
/// released to `ready`; on dependency failure, the partial child state
/// remains durable and blocked so recovery can resume without recreating
/// tracker issues or allowing unsafe dispatch.
pub fn persist_applied_decomposition(
    db: &mut StateDb,
    proposal: &DecompositionProposal,
    applied: &AppliedDecomposition,
    tracker_id: &str,
    now: &str,
) -> PersistAppliedDecompositionResult {
    if proposal.status != DecompositionStatus::Approved {
        return Err(StateError::Invariant(format!(
            "decomposition {} must be approved before state persistence (status = {})",
            proposal.id, proposal.status
        ))
        .into());
    }
    if applied.proposal_id != proposal.id {
        return Err(StateError::Invariant(format!(
            "applied decomposition {} does not match proposal {}",
            applied.proposal_id, proposal.id
        ))
        .into());
    }

    let persisted_children = db.transaction(|tx| {
        let applied_by_key: HashMap<&ChildKey, &AppliedChild> = applied
            .children
            .iter()
            .map(|child| (&child.key, child))
            .collect();
        let mut persisted = Vec::with_capacity(proposal.children.len());
        let mut ids_by_key: HashMap<ChildKey, WorkItemId> =
            HashMap::with_capacity(proposal.children.len());

        for proposal_child in &proposal.children {
            let Some(applied_child) = applied_by_key.get(&proposal_child.key).copied() else {
                return Err(StateError::Invariant(format!(
                    "missing tracker-created child for decomposition key {}",
                    proposal_child.key
                )));
            };
            let row = tx.create_work_item(NewWorkItem {
                tracker_id,
                identifier: &applied_child.identifier,
                parent_id: Some(WorkItemId(proposal.parent.get())),
                title: &proposal_child.title,
                status_class: WorkItemStatusClass::Blocked.as_str(),
                tracker_status: "blocked",
                assigned_role: Some(proposal_child.assigned_role.as_str()),
                assigned_agent: None,
                priority: None,
                workspace_policy: None,
                branch_policy: None,
                now,
            })?;
            tx.create_edge(NewWorkItemEdge {
                parent_id: WorkItemId(proposal.parent.get()),
                child_id: row.id,
                edge_type: EdgeType::ParentChild,
                reason: Some("decomposition child"),
                status: "linked",
                source: EdgeSource::Decomposition,
                now,
            })?;
            ids_by_key.insert(proposal_child.key.clone(), row.id);
            persisted.push(PersistedAppliedChild {
                key: proposal_child.key.clone(),
                work_item: row,
            });
        }

        Ok(persisted)
    })?;

    let ids_by_key: HashMap<ChildKey, WorkItemId> = persisted_children
        .iter()
        .map(|child| (child.key.clone(), child.work_item.id))
        .collect();
    let mut blockers = Vec::new();
    for proposal_child in &proposal.children {
        let blocked_id = *ids_by_key.get(&proposal_child.key).ok_or_else(|| {
            StateError::Invariant(format!("missing persisted child {}", proposal_child.key))
        })?;
        for dependency in &proposal_child.depends_on {
            let blocker_id = *ids_by_key.get(dependency).ok_or_else(|| {
                StateError::Invariant(format!(
                    "missing persisted dependency {dependency} for child {}",
                    proposal_child.key
                ))
            })?;
            let reason = format!("{} depends on {}", proposal_child.key, dependency);
            blockers.push((blocker_id, blocked_id, reason));
        }
    }

    let blocker_edges: Vec<NewDecompositionBlockerEdge<'_>> = blockers
        .iter()
        .map(
            |(blocker_id, blocked_id, reason)| NewDecompositionBlockerEdge {
                blocker_id: *blocker_id,
                blocked_id: *blocked_id,
                reason: reason.as_str(),
                now,
            },
        )
        .collect();
    let dependency_edges = db
        .create_decomposition_blocker_edges(&blocker_edges)
        .map_err(
            |source| PersistAppliedDecompositionError::DependencyMaterializationFailed {
                children: persisted_children.clone(),
                source,
            },
        )?;

    db.transaction(|tx| {
        for child in &persisted_children {
            tx.update_work_item_status(
                child.work_item.id,
                WorkItemStatusClass::Ready.as_str(),
                "open",
                now,
            )?;
        }
        Ok(())
    })
    .map_err(
        |source| PersistAppliedDecompositionError::ChildReleaseFailed {
            dependency_edges: dependency_edges.clone(),
            children: persisted_children.clone(),
            source,
        },
    )?;

    let children = persisted_children
        .into_iter()
        .map(|mut child| {
            child.work_item.status_class = WorkItemStatusClass::Ready.as_str().to_string();
            child.work_item.tracker_status = "open".to_string();
            child.work_item.updated_at = now.to_string();
            child
        })
        .collect();
    Ok(PersistedAppliedDecomposition {
        children,
        dependency_edges,
    })
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeSet;

    use super::*;
    use crate::edges::WorkItemEdgeRepository;
    use crate::migrations::migrations;
    use crate::repository::{NewWorkItem, WorkItemRepository};
    use symphony_core::decomposition::{ChildProposal, DecompositionId};
    use symphony_core::followup::FollowupPolicy;
    use symphony_core::role::RoleName;
    use symphony_core::tracker::IssueId;

    const NOW: &str = "2026-05-10T00:00:00Z";

    fn open() -> StateDb {
        let mut db = StateDb::open_in_memory().expect("open");
        db.migrate(migrations()).expect("migrate");
        db
    }

    fn seed_parent(db: &mut StateDb) -> WorkItemId {
        db.create_work_item(NewWorkItem {
            tracker_id: "github",
            identifier: "OWNER/REPO#1",
            parent_id: None,
            title: "parent",
            status_class: "intake",
            tracker_status: "open",
            assigned_role: Some("platform_lead"),
            assigned_agent: None,
            priority: None,
            workspace_policy: None,
            branch_policy: None,
            now: NOW,
        })
        .expect("parent")
        .id
    }

    fn child(key: &str, deps: &[&str]) -> ChildProposal {
        ChildProposal::try_new(
            key,
            format!("{key} title"),
            format!("{key} scope"),
            RoleName::new("backend_engineer"),
            None,
            vec![format!("{key} accepted")],
            deps.iter()
                .map(|dep| ChildKey::new(*dep))
                .collect::<BTreeSet<_>>(),
            true,
            None,
            true,
        )
        .expect("child")
    }

    fn proposal(parent: WorkItemId) -> DecompositionProposal {
        proposal_with_children(
            parent,
            vec![child("A", &[]), child("B", &["A"]), child("C", &["B"])],
        )
    }

    fn proposal_with_children(
        parent: WorkItemId,
        children: Vec<ChildProposal>,
    ) -> DecompositionProposal {
        DecompositionProposal::try_new(
            DecompositionId::new(7),
            symphony_core::work_item::WorkItemId::new(parent.0),
            RoleName::new("platform_lead"),
            "split",
            children,
            FollowupPolicy::CreateDirectly,
        )
        .expect("proposal")
    }

    fn applied() -> AppliedDecomposition {
        AppliedDecomposition {
            proposal_id: DecompositionId::new(7),
            children: vec![
                AppliedChild {
                    key: ChildKey::new("A"),
                    tracker_id: IssueId::new("100"),
                    identifier: "OWNER/REPO#100".to_string(),
                    url: None,
                    linked_parent: false,
                },
                AppliedChild {
                    key: ChildKey::new("B"),
                    tracker_id: IssueId::new("101"),
                    identifier: "OWNER/REPO#101".to_string(),
                    url: None,
                    linked_parent: false,
                },
                AppliedChild {
                    key: ChildKey::new("C"),
                    tracker_id: IssueId::new("102"),
                    identifier: "OWNER/REPO#102".to_string(),
                    url: None,
                    linked_parent: false,
                },
            ],
        }
    }

    #[test]
    fn persists_children_and_parent_child_edges_before_dependency_edges() {
        let mut db = open();
        let parent = seed_parent(&mut db);
        let proposal = proposal(parent);

        let persisted =
            persist_applied_decomposition(&mut db, &proposal, &applied(), "github", NOW)
                .expect("persist");

        assert_eq!(persisted.children.len(), 3);
        for child in &persisted.children {
            assert_eq!(child.work_item.parent_id, Some(parent));
            assert_eq!(child.work_item.status_class, "ready");
            assert_eq!(
                child.work_item.assigned_role.as_deref(),
                Some("backend_engineer")
            );
        }
        let parent_children = db.list_outgoing(parent, EdgeType::ParentChild).unwrap();
        assert_eq!(parent_children.len(), 3);
        assert!(
            parent_children
                .iter()
                .all(|edge| edge.source == EdgeSource::Decomposition)
        );

        let ids = persisted.child_work_item_ids();
        let b = WorkItemId(ids[&ChildKey::new("B")].get());
        let c = WorkItemId(ids[&ChildKey::new("C")].get());
        assert_eq!(db.list_incoming_open_blockers(b).unwrap().len(), 1);
        assert_eq!(db.list_incoming_open_blockers(c).unwrap().len(), 1);
    }

    #[test]
    fn materializes_each_depends_on_as_a_local_open_blocks_edge() {
        let mut db = open();
        let parent = seed_parent(&mut db);
        let mut proposal = proposal(parent);

        let persisted =
            persist_applied_decomposition(&mut db, &proposal, &applied(), "github", NOW)
                .expect("persist");
        let ids = persisted.child_work_item_ids();
        let a = WorkItemId(ids[&ChildKey::new("A")].get());
        let b = WorkItemId(ids[&ChildKey::new("B")].get());
        let c = WorkItemId(ids[&ChildKey::new("C")].get());

        assert_eq!(persisted.dependency_edges.len(), 2);
        assert_eq!(persisted.dependency_edges[0].parent_id, a);
        assert_eq!(persisted.dependency_edges[0].child_id, b);
        assert_eq!(persisted.dependency_edges[0].edge_type, EdgeType::Blocks);
        assert_eq!(persisted.dependency_edges[0].status, "open");
        assert_eq!(
            persisted.dependency_edges[0].source,
            EdgeSource::Decomposition
        );
        assert_eq!(
            persisted.dependency_edges[0].reason.as_deref(),
            Some("B depends on A")
        );

        assert_eq!(persisted.dependency_edges[1].parent_id, b);
        assert_eq!(persisted.dependency_edges[1].child_id, c);
        assert_eq!(
            persisted.dependency_edges[1].reason.as_deref(),
            Some("C depends on B")
        );

        proposal
            .mark_applied(
                persisted.child_work_item_ids(),
                persisted.local_dependency_evidence(),
            )
            .expect("A -> B -> C materialization is enough to apply");
        assert_eq!(proposal.status, DecompositionStatus::Applied);
    }

    #[test]
    fn materializes_mixed_parallel_sequential_graph_with_two_root_children() {
        let mut db = open();
        let parent = seed_parent(&mut db);
        let mut proposal = proposal_with_children(
            parent,
            vec![child("A", &[]), child("B", &[]), child("C", &["A", "B"])],
        );

        let persisted =
            persist_applied_decomposition(&mut db, &proposal, &applied(), "github", NOW)
                .expect("persist");
        let ids = persisted.child_work_item_ids();
        let a = WorkItemId(ids[&ChildKey::new("A")].get());
        let b = WorkItemId(ids[&ChildKey::new("B")].get());
        let c = WorkItemId(ids[&ChildKey::new("C")].get());

        assert!(db.list_incoming_open_blockers(a).unwrap().is_empty());
        assert!(db.list_incoming_open_blockers(b).unwrap().is_empty());

        let c_blockers = db.list_incoming_open_blockers(c).unwrap();
        assert_eq!(c_blockers.len(), 2);
        assert_eq!(
            c_blockers
                .iter()
                .map(|edge| (edge.parent_id, edge.child_id, edge.reason.as_deref()))
                .collect::<Vec<_>>(),
            vec![
                (a, c, Some("C depends on A")),
                (b, c, Some("C depends on B"))
            ]
        );

        assert_eq!(persisted.dependency_edges.len(), 2);
        assert!(persisted.dependency_edges.iter().all(|edge| {
            edge.edge_type == EdgeType::Blocks
                && edge.status == "open"
                && edge.source == EdgeSource::Decomposition
        }));

        proposal
            .mark_applied(
                persisted.child_work_item_ids(),
                persisted.local_dependency_evidence(),
            )
            .expect("mixed parallel/sequential materialization is enough to apply");
        assert_eq!(proposal.status, DecompositionStatus::Applied);
    }

    #[test]
    fn dependency_materialization_failure_preserves_blocked_children_and_parent_links() {
        let mut db = open();
        let parent = seed_parent(&mut db);
        let mut proposal = proposal(parent);

        db.conn()
            .execute_batch(
                "CREATE TRIGGER fail_decomposition_blocks
                 BEFORE INSERT ON work_item_edges
                 WHEN NEW.edge_type = 'blocks'
                 BEGIN
                   SELECT RAISE(ABORT, 'dependency edge write failed');
                 END;",
            )
            .unwrap();

        let err = persist_applied_decomposition(&mut db, &proposal, &applied(), "github", NOW)
            .expect_err("dependency write fails after children");

        let PersistAppliedDecompositionError::DependencyMaterializationFailed { children, source } =
            err
        else {
            panic!("expected partial dependency failure");
        };
        assert!(matches!(source, StateError::Sqlite(_)));
        assert_eq!(children.len(), 3);
        assert!(children.iter().all(|child| {
            child.work_item.status_class == WorkItemStatusClass::Blocked.as_str()
                && child.work_item.tracker_status == "blocked"
        }));

        let parent_children = db.list_outgoing(parent, EdgeType::ParentChild).unwrap();
        assert_eq!(parent_children.len(), 3);
        let child_ids: Vec<WorkItemId> = children.iter().map(|child| child.work_item.id).collect();
        let created_ids = children
            .iter()
            .map(|child| {
                (
                    child.key.clone(),
                    symphony_core::work_item::WorkItemId::new(child.work_item.id.0),
                )
            })
            .collect();
        let apply_err = proposal
            .mark_applied(
                created_ids,
                DependencyApplicationEvidence {
                    persisted_edge_count: 0,
                    tracker_sync_required: false,
                    tracker_sync_record_count: 0,
                },
            )
            .expect_err("missing dependency evidence keeps proposal non-terminal");
        assert_eq!(
            apply_err,
            symphony_core::decomposition::DecompositionError::MissingDependencyEdges {
                expected: 2,
                actual: 0
            }
        );
        assert_eq!(proposal.status, DecompositionStatus::Approved);

        for child in children {
            let stored = db.get_work_item(child.work_item.id).unwrap().unwrap();
            assert_eq!(stored.status_class, WorkItemStatusClass::Blocked.as_str());
            assert_eq!(stored.tracker_status, "blocked");
        }
        for child_id in child_ids {
            assert!(db.list_incoming_open_blockers(child_id).unwrap().is_empty());
        }
    }

    #[test]
    fn required_tracker_sync_failure_keeps_blocked_child_parked() {
        let mut db = open();
        let parent = seed_parent(&mut db);
        let proposal = proposal(parent);
        let persisted =
            persist_applied_decomposition(&mut db, &proposal, &applied(), "github", NOW)
                .expect("dependencies materialize");
        let edge = persisted
            .dependency_edges
            .iter()
            .find(|edge| edge.reason.as_deref() == Some("B depends on A"))
            .expect("B is blocked by A")
            .clone();

        let blocked_child_before = db.get_work_item(edge.child_id).unwrap().unwrap();
        assert_eq!(
            blocked_child_before.status_class,
            WorkItemStatusClass::Ready.as_str()
        );

        record_required_dependency_sync_failure(
            &mut db,
            &edge,
            "Linear issueRelationCreate failed",
            "2026-05-10T00:01:00Z",
        )
        .expect("record required sync failure");

        let failed_edge = db.get_edge(edge.id).unwrap().unwrap();
        assert_eq!(failed_edge.status, "open");
        assert_eq!(failed_edge.tracker_sync_status.as_deref(), Some("failed"));
        assert_eq!(
            failed_edge.tracker_sync_last_error.as_deref(),
            Some("Linear issueRelationCreate failed")
        );
        assert_eq!(failed_edge.tracker_sync_attempts, 1);

        let blocked_child_after = db.get_work_item(edge.child_id).unwrap().unwrap();
        assert_eq!(
            blocked_child_after.status_class,
            WorkItemStatusClass::Blocked.as_str()
        );
        assert_eq!(blocked_child_after.tracker_status, "blocked");
        assert_eq!(blocked_child_after.updated_at, "2026-05-10T00:01:00Z");
    }

    #[test]
    fn best_effort_tracker_sync_failure_records_retry_metadata_without_parking_child() {
        let mut db = open();
        let parent = seed_parent(&mut db);
        let proposal = proposal(parent);
        let persisted =
            persist_applied_decomposition(&mut db, &proposal, &applied(), "github", NOW)
                .expect("dependencies materialize");
        let edge = persisted
            .dependency_edges
            .iter()
            .find(|edge| edge.reason.as_deref() == Some("B depends on A"))
            .expect("B is blocked by A")
            .clone();

        let child_before = db.get_work_item(edge.child_id).unwrap().unwrap();
        assert_eq!(
            child_before.status_class,
            WorkItemStatusClass::Ready.as_str()
        );
        assert_eq!(child_before.tracker_status, "open");

        record_best_effort_dependency_sync_failure(
            &mut db,
            &edge,
            "Linear issueRelationCreate timed out",
            "2026-05-10T00:02:00Z",
        )
        .expect("record best-effort sync failure");

        let failed_edge = db.get_edge(edge.id).unwrap().unwrap();
        assert_eq!(failed_edge.status, "open");
        assert_eq!(failed_edge.tracker_sync_status.as_deref(), Some("failed"));
        assert_eq!(
            failed_edge.tracker_sync_last_error.as_deref(),
            Some("Linear issueRelationCreate timed out")
        );
        assert_eq!(failed_edge.tracker_sync_attempts, 1);
        assert_eq!(
            failed_edge.tracker_sync_last_attempt_at.as_deref(),
            Some("2026-05-10T00:02:00Z")
        );

        let child_after = db.get_work_item(edge.child_id).unwrap().unwrap();
        assert_eq!(
            child_after.status_class,
            WorkItemStatusClass::Ready.as_str()
        );
        assert_eq!(child_after.tracker_status, "open");
        assert_eq!(child_after.updated_at, NOW);
        assert_eq!(
            db.list_incoming_open_blockers(edge.child_id).unwrap().len(),
            1,
            "the local open blocker remains the authoritative dispatch gate"
        );
    }

    #[test]
    fn local_only_tracker_sync_records_policy_without_tracker_attempt_or_child_demotion() {
        let mut db = open();
        let parent = seed_parent(&mut db);
        let proposal = proposal(parent);
        let persisted =
            persist_applied_decomposition(&mut db, &proposal, &applied(), "github", NOW)
                .expect("dependencies materialize");
        let edge = persisted
            .dependency_edges
            .iter()
            .find(|edge| edge.reason.as_deref() == Some("B depends on A"))
            .expect("B is blocked by A")
            .clone();

        record_local_only_dependency_sync(&mut db, &edge, "2026-05-10T00:03:00Z")
            .expect("record local-only sync policy");

        let local_only_edge = db.get_edge(edge.id).unwrap().unwrap();
        assert_eq!(local_only_edge.status, "open");
        assert_eq!(local_only_edge.tracker_edge_id, None);
        assert_eq!(
            local_only_edge.tracker_sync_status.as_deref(),
            Some("local_only")
        );
        assert_eq!(local_only_edge.tracker_sync_last_error, None);
        assert_eq!(local_only_edge.tracker_sync_attempts, 0);
        assert_eq!(
            local_only_edge.tracker_sync_last_attempt_at.as_deref(),
            Some("2026-05-10T00:03:00Z")
        );

        let child_after = db.get_work_item(edge.child_id).unwrap().unwrap();
        assert_eq!(
            child_after.status_class,
            WorkItemStatusClass::Ready.as_str()
        );
        assert_eq!(child_after.tracker_status, "open");
        assert_eq!(
            db.list_incoming_open_blockers(edge.child_id).unwrap().len(),
            1,
            "local Symphony edge remains the dispatch gate under local_only"
        );
    }
}
