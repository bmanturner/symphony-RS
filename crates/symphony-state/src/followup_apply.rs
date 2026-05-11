//! Durable persistence for tracker-filed follow-up requests.
//!
//! Mirrors `decomposition_apply.rs` at a smaller scale: once
//! `symphony-core` has successfully filed an approved follow-up into the
//! tracker, this module creates the local `work_items` row, the
//! `followup_of` provenance edge, and advances the durable follow-up row
//! to `Created` in one SQLite transaction.

use symphony_core::followup::{FollowupIssueRequest, FollowupStatus};
use symphony_core::followup_applier::AppliedFollowup;
use symphony_core::work_item::WorkItemStatusClass;

use crate::edges::{EdgeSource, EdgeType, NewWorkItemEdge, WorkItemEdgeRecord};
use crate::repository::{NewWorkItem, WorkItemRecord};
use crate::{StateDb, StateError};

/// Persisted outcome of one tracker-filed follow-up.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PersistedAppliedFollowup {
    /// Newly created local work item row corresponding to the tracker
    /// issue created from the follow-up.
    pub work_item: WorkItemRecord,
    /// Provenance edge from source work item to the new follow-up work
    /// item.
    pub source_link: WorkItemEdgeRecord,
}

/// Persist a tracker-filed follow-up and advance the durable follow-up
/// request to `Created` atomically.
pub fn persist_applied_followup(
    db: &mut StateDb,
    followup: &FollowupIssueRequest,
    applied: &AppliedFollowup,
    tracker_id: &str,
    tracker_status: &str,
    now: &str,
) -> crate::StateResult<PersistedAppliedFollowup> {
    if followup.status != FollowupStatus::Approved {
        return Err(StateError::Invariant(format!(
            "followup {} must be approved before state persistence (status = {})",
            followup.id, followup.status
        )));
    }
    if followup.id != applied.followup_id {
        return Err(StateError::Invariant(format!(
            "applied followup {} does not match source followup {}",
            applied.followup_id, followup.id
        )));
    }

    db.transaction(|tx| {
        let work_item = tx.create_work_item(NewWorkItem {
            tracker_id,
            identifier: &applied.identifier,
            parent_id: None,
            title: &followup.title,
            status_class: WorkItemStatusClass::Ready.as_str(),
            tracker_status,
            assigned_role: None,
            assigned_agent: None,
            priority: None,
            workspace_policy: None,
            branch_policy: None,
            now,
        })?;
        let edge = tx.create_edge(NewWorkItemEdge {
            parent_id: crate::repository::WorkItemId(followup.source_work_item.get()),
            child_id: work_item.id,
            edge_type: EdgeType::FollowupOf,
            reason: Some(&followup.reason),
            status: if followup.blocking { "open" } else { "linked" },
            source: EdgeSource::Followup,
            now,
        })?;

        let mut updated = followup.clone();
        updated
            .mark_created(symphony_core::WorkItemId::new(work_item.id.0))
            .map_err(|err| StateError::Invariant(err.to_string()))?;
        let changed = tx.mark_followup_created(followup.id, work_item.id, now)?;
        if !changed {
            return Err(StateError::Invariant(format!(
                "followup {} disappeared before mark_created could persist",
                followup.id
            )));
        }

        Ok(PersistedAppliedFollowup {
            work_item,
            source_link: edge,
        })
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::followups::{FollowupRepository, NewFollowup};
    use crate::migrations::migrations;
    use crate::repository::{NewRun, NewWorkItem, RunRepository, WorkItemRepository};
    use symphony_core::{
        BlockerOrigin, FollowupId, FollowupPolicy, RoleName, RunRef, WorkItemId as CoreWorkItemId,
    };

    const NOW: &str = "2026-05-10T02:00:00Z";

    fn open() -> StateDb {
        let mut db = StateDb::open_in_memory().expect("open");
        db.migrate(migrations()).expect("migrate");
        db
    }

    fn approved_followup(db: &mut StateDb) -> FollowupIssueRequest {
        let source = db
            .create_work_item(NewWorkItem {
                tracker_id: "github",
                identifier: "OWNER/REPO#1",
                parent_id: None,
                title: "source",
                status_class: "running",
                tracker_status: "open",
                assigned_role: None,
                assigned_agent: None,
                priority: None,
                workspace_policy: None,
                branch_policy: None,
                now: NOW,
            })
            .unwrap()
            .id;
        let run = db
            .create_run(NewRun {
                work_item_id: source,
                role: "backend",
                agent: "claude",
                status: "running",
                workspace_claim_id: None,
                now: NOW,
            })
            .unwrap()
            .id;
        let row = db
            .create_followup(NewFollowup {
                source_work_item_id: source,
                title: "Add retry coverage",
                reason: "API retries need a regression test",
                scope: "tests only",
                acceptance_criteria: "[\"retry branch has coverage\"]",
                blocking: true,
                policy: "create_directly",
                origin_kind: "run",
                origin_run_id: Some(run),
                origin_role: Some("backend"),
                origin_agent_profile: None,
                origin_human_actor: None,
                status: "approved",
                created_issue_id: None,
                approval_role: None,
                approval_note: None,
                now: NOW,
            })
            .unwrap();
        row.to_domain().unwrap()
    }

    #[test]
    fn persist_applied_followup_creates_work_item_edge_and_updates_followup() {
        let mut db = open();
        let followup = approved_followup(&mut db);
        let persisted = persist_applied_followup(
            &mut db,
            &followup,
            &AppliedFollowup {
                followup_id: followup.id,
                tracker_id: symphony_core::IssueId::new("123"),
                identifier: "ENG-123".into(),
                url: Some("https://example.test/ENG-123".into()),
            },
            "github",
            "Backlog",
            NOW,
        )
        .unwrap();

        assert_eq!(persisted.work_item.identifier, "ENG-123");
        assert_eq!(persisted.work_item.tracker_status, "Backlog");
        assert_eq!(persisted.source_link.edge_type.as_str(), "followup_of");
        assert_eq!(persisted.source_link.status, "open");

        let stored = db.get_followup(followup.id).unwrap().unwrap();
        assert_eq!(stored.status, "created");
        assert_eq!(stored.created_issue_id, Some(persisted.work_item.id));
    }

    #[test]
    fn persist_applied_followup_rejects_non_approved_status() {
        let mut db = open();
        let source = db
            .create_work_item(NewWorkItem {
                tracker_id: "github",
                identifier: "OWNER/REPO#1",
                parent_id: None,
                title: "source",
                status_class: "running",
                tracker_status: "open",
                assigned_role: None,
                assigned_agent: None,
                priority: None,
                workspace_policy: None,
                branch_policy: None,
                now: NOW,
            })
            .unwrap()
            .id;
        let followup = FollowupIssueRequest::try_new(
            FollowupId::new(1),
            CoreWorkItemId::new(source.0),
            "title",
            "reason",
            "scope",
            vec!["ac".into()],
            false,
            FollowupPolicy::ProposeForApproval,
            BlockerOrigin::Run {
                run_id: RunRef::new(9),
                role: Some(RoleName::new("backend")),
            },
            true,
        )
        .unwrap();
        let err = persist_applied_followup(
            &mut db,
            &followup,
            &AppliedFollowup {
                followup_id: followup.id,
                tracker_id: symphony_core::IssueId::new("123"),
                identifier: "ENG-123".into(),
                url: None,
            },
            "github",
            "Backlog",
            NOW,
        )
        .unwrap_err();
        assert!(err.to_string().contains("must be approved"));
    }
}
