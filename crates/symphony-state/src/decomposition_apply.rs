//! Durable persistence for tracker-applied decompositions.
//!
//! The tracker applier in `symphony-core` creates child issues, but the
//! workflow gates depend on SQLite truth: child `work_items`,
//! `parent_child` edges, and decomposition-sourced `blocks` edges must
//! land before a proposal can be considered applied. This module owns
//! that state-layer composition.

use std::collections::HashMap;

use symphony_core::decomposition::{ChildKey, DecompositionProposal, DecompositionStatus};
use symphony_core::decomposition_applier::{AppliedChild, AppliedDecomposition};
use symphony_core::work_item::WorkItemStatusClass;

use crate::edges::{
    EdgeSource, EdgeType, NewDecompositionBlockerEdge, NewWorkItemEdge, WorkItemEdgeRecord,
};
use crate::repository::{NewWorkItem, WorkItemId, WorkItemRecord};
use crate::{StateDb, StateError, StateResult};

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
}

/// Persist tracker-created decomposition children and their local graph.
///
/// The operation is atomic: every child `work_items` row, every
/// `parent_child` edge, and every decomposition dependency `blocks` edge
/// lands in the same SQLite transaction. If any step fails, no partial
/// graph is visible to dispatch or integration gates.
pub fn persist_applied_decomposition(
    db: &mut StateDb,
    proposal: &DecompositionProposal,
    applied: &AppliedDecomposition,
    tracker_id: &str,
    now: &str,
) -> StateResult<PersistedAppliedDecomposition> {
    if proposal.status != DecompositionStatus::Approved {
        return Err(StateError::Invariant(format!(
            "decomposition {} must be approved before state persistence (status = {})",
            proposal.id, proposal.status
        )));
    }
    if applied.proposal_id != proposal.id {
        return Err(StateError::Invariant(format!(
            "applied decomposition {} does not match proposal {}",
            applied.proposal_id, proposal.id
        )));
    }

    db.transaction(|tx| {
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
                status_class: WorkItemStatusClass::Ready.as_str(),
                tracker_status: "open",
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
        let dependency_edges = tx.create_decomposition_blocker_edges(&blocker_edges)?;

        Ok(PersistedAppliedDecomposition {
            children: persisted,
            dependency_edges,
        })
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
        DecompositionProposal::try_new(
            DecompositionId::new(7),
            symphony_core::work_item::WorkItemId::new(parent.0),
            RoleName::new("platform_lead"),
            "split",
            vec![child("A", &[]), child("B", &["A"]), child("C", &["B"])],
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
        let proposal = proposal(parent);

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
    }

    #[test]
    fn duplicate_dependency_edge_rolls_back_children_and_parent_links() {
        let mut db = open();
        let parent = seed_parent(&mut db);
        let proposal = proposal(parent);

        let a = db
            .create_work_item(NewWorkItem {
                tracker_id: "github",
                identifier: "OWNER/REPO#100",
                parent_id: Some(parent),
                title: "preexisting duplicate",
                status_class: "ready",
                tracker_status: "open",
                assigned_role: None,
                assigned_agent: None,
                priority: None,
                workspace_policy: None,
                branch_policy: None,
                now: NOW,
            })
            .unwrap();
        let b = db
            .create_work_item(NewWorkItem {
                tracker_id: "github",
                identifier: "OWNER/REPO#101",
                parent_id: Some(parent),
                title: "preexisting duplicate",
                status_class: "ready",
                tracker_status: "open",
                assigned_role: None,
                assigned_agent: None,
                priority: None,
                workspace_policy: None,
                branch_policy: None,
                now: NOW,
            })
            .unwrap();
        db.create_decomposition_blocker_edges(&[NewDecompositionBlockerEdge {
            blocker_id: a.id,
            blocked_id: b.id,
            reason: "preexisting",
            now: NOW,
        }])
        .unwrap();

        let err = persist_applied_decomposition(&mut db, &proposal, &applied(), "github", NOW)
            .expect_err("duplicate natural key rolls back");
        assert!(matches!(err, StateError::Sqlite(_)));
        assert_eq!(
            db.list_outgoing(parent, EdgeType::ParentChild)
                .unwrap()
                .len(),
            0
        );
    }
}
