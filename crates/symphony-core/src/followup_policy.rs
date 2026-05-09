//! Follow-up parent-close gate (SPEC v2 §4.10, §8.1).
//!
//! SPEC §4.10 gives agents two distinct ways to attach a follow-up to the
//! current work item: as a *non-blocking* adjacent finding, or as a
//! *blocking* finding that invalidates current acceptance. SPEC §8.1
//! folds the latter into the parent-close completion rules: a parent
//! issue MUST NOT close while a blocking follow-up still gates
//! acceptance.
//!
//! This module is the single pure-domain place where the kernel checks
//! that rule, mirroring the
//! [`crate::check_qa_blockers_at_parent_close`] split between QA-filed
//! blockers and parent close. Other call sites (integration owner close,
//! operator force-close, parent close gate composition in Phase 11)
//! consult [`check_blocking_followups_at_parent_close`] rather than
//! re-implementing the rule.
//!
//! Distinguishing blocking from non-blocking lives on
//! [`crate::FollowupIssueRequest::is_acceptance_gating`]: callers
//! materialise the snapshot list by filtering the durable follow-up
//! table for follow-ups whose `source_work_item` lies in the parent's
//! subtree and whose `is_acceptance_gating()` is `true`. Non-blocking
//! follow-ups never appear in the snapshot list and therefore never
//! gate parent close — that is the whole point of the
//! blocking/non-blocking split.

use serde::{Deserialize, Serialize};

use crate::followup::{FollowupId, FollowupIssueRequest, FollowupStatus};
use crate::role::RoleName;
use crate::work_item::WorkItemId;

/// Minimal view of an acceptance-gating blocking follow-up required by
/// [`check_blocking_followups_at_parent_close`].
///
/// The kernel materialises one of these per durable follow-up row whose
/// `source_work_item` lives in the parent's subtree and whose
/// [`FollowupIssueRequest::is_acceptance_gating`] returns `true`. The
/// snapshot is a point-in-time read of the follow-up table joined to the
/// authoring run + role; the gate has no opinion about how the caller
/// performed that join. Trusts the caller's filter for "blocking and not
/// yet dismissed" semantics, the same trust contract as
/// [`crate::QaBlockerSnapshot`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct BlockingFollowupSnapshot {
    /// Domain primary key of the follow-up row.
    pub followup_id: FollowupId,
    /// Work item that *spawned* the follow-up (the SPEC §5.13 source link).
    pub source_work_item: WorkItemId,
    /// Tracker title carried so operator output names the follow-up
    /// without a second lookup.
    pub title: String,
    /// Lifecycle status at snapshot time. Carried for diagnostic clarity:
    /// the gate already trusts the caller's `is_acceptance_gating` filter,
    /// but operators reading the error want to know whether the gate is
    /// holding on a `Proposed`/`Approved` queue item or a `Created`
    /// follow-up whose tracker work is still in flight.
    pub status: FollowupStatus,
    /// Role that authored the follow-up, when one was recorded. Mirrors
    /// [`crate::QaBlockerSnapshot::origin_role`] so the diagnostic output
    /// can name the agent or human responsible.
    pub origin_role: Option<RoleName>,
}

impl BlockingFollowupSnapshot {
    /// Build a snapshot from a durable [`FollowupIssueRequest`].
    ///
    /// Returns `None` when the follow-up is not currently
    /// acceptance-gating (either non-blocking or in a sticky-terminal
    /// `Rejected`/`Cancelled` state). Storage seams use this filter when
    /// materialising the per-parent gate input so non-gating rows never
    /// reach the gate.
    pub fn from_followup(followup: &FollowupIssueRequest) -> Option<Self> {
        if !followup.is_acceptance_gating() {
            return None;
        }
        Some(Self {
            followup_id: followup.id,
            source_work_item: followup.source_work_item,
            title: followup.title.clone(),
            status: followup.status,
            origin_role: followup.origin.role().cloned(),
        })
    }
}

/// Errors produced by [`check_blocking_followups_at_parent_close`].
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum BlockingFollowupGateError {
    /// At least one blocking follow-up on the parent's subtree is still
    /// acceptance-gating. The carried snapshots preserve caller-supplied
    /// order so error messages remain stable across runs.
    #[error(
        "parent {parent} cannot close: {} blocking follow-up(s) still gate acceptance",
        open.len()
    )]
    BlockingFollowupsOpen {
        /// Parent the gate was checked for.
        parent: WorkItemId,
        /// Acceptance-gating follow-ups in caller-supplied order.
        open: Vec<BlockingFollowupSnapshot>,
    },
}

/// Return `Ok(())` iff the parent may close given blocking follow-up state
/// (SPEC v2 §4.10, §8.1).
///
/// Rules:
///
/// * Non-blocking follow-ups never appear in `open_blocking_followups`
///   (the caller filters via
///   [`FollowupIssueRequest::is_acceptance_gating`]). They therefore
///   never gate parent close — that is the SPEC §4.10 distinction.
/// * Any non-empty input list rejects close: every entry represents a
///   blocking finding the agent flagged as invalidating current
///   acceptance, and the kernel honours that signal until it is
///   explicitly dismissed (`Rejected` / `Cancelled`).
///
/// The caller is responsible for supplying the per-parent snapshot list.
/// Mirrors [`crate::check_qa_blockers_at_parent_close`]'s caller
/// contract: storage performs the join, the gate consumes the result.
pub fn check_blocking_followups_at_parent_close(
    parent: WorkItemId,
    open_blocking_followups: &[BlockingFollowupSnapshot],
) -> Result<(), BlockingFollowupGateError> {
    if open_blocking_followups.is_empty() {
        return Ok(());
    }
    Err(BlockingFollowupGateError::BlockingFollowupsOpen {
        parent,
        open: open_blocking_followups.to_vec(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::blocker::{BlockerOrigin, RunRef};
    use crate::followup::FollowupPolicy;

    fn run_origin() -> BlockerOrigin {
        BlockerOrigin::Run {
            run_id: RunRef::new(7),
            role: Some(RoleName::new("backend")),
        }
    }

    fn followup(blocking: bool, policy: FollowupPolicy) -> FollowupIssueRequest {
        FollowupIssueRequest::try_new(
            FollowupId::new(1),
            WorkItemId::new(42),
            "title",
            "reason",
            "scope",
            vec!["ac".into()],
            blocking,
            policy,
            run_origin(),
            true,
        )
        .unwrap()
    }

    fn snap(id: i64, source: i64, title: &str, status: FollowupStatus) -> BlockingFollowupSnapshot {
        BlockingFollowupSnapshot {
            followup_id: FollowupId::new(id),
            source_work_item: WorkItemId::new(source),
            title: title.into(),
            status,
            origin_role: Some(RoleName::new("qa")),
        }
    }

    #[test]
    fn from_followup_skips_non_blocking_followups() {
        let f = followup(false, FollowupPolicy::CreateDirectly);
        assert!(BlockingFollowupSnapshot::from_followup(&f).is_none());
    }

    #[test]
    fn from_followup_skips_terminal_dismissals() {
        let mut rejected = followup(true, FollowupPolicy::ProposeForApproval);
        rejected
            .reject(RoleName::new("platform_lead"), "duplicate")
            .unwrap();
        assert!(BlockingFollowupSnapshot::from_followup(&rejected).is_none());

        let mut cancelled = followup(true, FollowupPolicy::ProposeForApproval);
        cancelled.cancel(Some("folded into source".into())).unwrap();
        assert!(BlockingFollowupSnapshot::from_followup(&cancelled).is_none());
    }

    #[test]
    fn from_followup_captures_blocking_proposed_followup_with_origin_role() {
        let f = followup(true, FollowupPolicy::ProposeForApproval);
        let snap = BlockingFollowupSnapshot::from_followup(&f).expect("blocking proposed gates");
        assert_eq!(snap.followup_id, FollowupId::new(1));
        assert_eq!(snap.source_work_item, WorkItemId::new(42));
        assert_eq!(snap.title, "title");
        assert_eq!(snap.status, FollowupStatus::Proposed);
        assert_eq!(snap.origin_role.as_ref().unwrap().as_str(), "backend");
    }

    #[test]
    fn from_followup_captures_blocking_created_followup_until_subtree_finishes() {
        let mut f = followup(true, FollowupPolicy::CreateDirectly);
        f.mark_created(WorkItemId::new(99)).unwrap();
        let snap = BlockingFollowupSnapshot::from_followup(&f)
            .expect("blocking + Created still gates parent close");
        assert_eq!(snap.status, FollowupStatus::Created);
    }

    #[test]
    fn empty_snapshot_list_passes_for_any_parent() {
        check_blocking_followups_at_parent_close(WorkItemId::new(1), &[]).unwrap();
    }

    #[test]
    fn any_blocking_followup_rejects_close() {
        let open = [snap(1, 42, "rate-limit cleanup", FollowupStatus::Proposed)];
        let err = check_blocking_followups_at_parent_close(WorkItemId::new(42), &open).unwrap_err();
        match err {
            BlockingFollowupGateError::BlockingFollowupsOpen { parent, open } => {
                assert_eq!(parent, WorkItemId::new(42));
                assert_eq!(open.len(), 1);
                assert_eq!(open[0].title, "rate-limit cleanup");
            }
        }
    }

    #[test]
    fn open_blocking_followups_reported_in_input_order() {
        let open = [
            snap(1, 42, "first", FollowupStatus::Proposed),
            snap(2, 42, "second", FollowupStatus::Approved),
            snap(3, 42, "third", FollowupStatus::Created),
        ];
        let err = check_blocking_followups_at_parent_close(WorkItemId::new(42), &open).unwrap_err();
        let BlockingFollowupGateError::BlockingFollowupsOpen { open, .. } = err;
        let titles: Vec<_> = open.iter().map(|f| f.title.as_str()).collect();
        assert_eq!(titles, vec!["first", "second", "third"]);
    }

    #[test]
    fn error_message_names_count_and_parent() {
        let open = [
            snap(1, 42, "a", FollowupStatus::Proposed),
            snap(2, 42, "b", FollowupStatus::Approved),
        ];
        let err = check_blocking_followups_at_parent_close(WorkItemId::new(7), &open).unwrap_err();
        let msg = err.to_string();
        assert!(msg.contains("2"), "msg should include count: {msg}");
        assert!(msg.contains("7"), "msg should name parent: {msg}");
    }

    #[test]
    fn snapshot_round_trips_through_json() {
        let s = snap(5, 2, "ENG-2", FollowupStatus::Approved);
        let json = serde_json::to_string(&s).unwrap();
        let back: BlockingFollowupSnapshot = serde_json::from_str(&json).unwrap();
        assert_eq!(back, s);
    }
}
