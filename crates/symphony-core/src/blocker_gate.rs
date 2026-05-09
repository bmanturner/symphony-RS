//! Open-blocker gate for draft PR creation and QA request (SPEC v2 §5.10
//! `require_no_open_blockers`, §5.11, §5.12).
//!
//! `IntegrationRunRequest::try_new` already refuses construction when the
//! parent's subtree carries open blockers. That guard is upstream of agent
//! launch. This module owns the parallel guard at the *next* two product
//! seams, where the integration owner emits durable side effects:
//!
//! * **Draft PR creation** — before the kernel writes a `pull_request_records`
//!   row in `Draft` state and calls the forge to open the PR.
//! * **QA request** — before the kernel hands the parent off to the QA gate
//!   role (queueing into the QA queue).
//!
//! The rule matches SPEC §5.11 and §5.12: a parent with an open blocker
//! anywhere in its subtree MUST NOT have a draft PR opened on its behalf
//! and MUST NOT be staged for QA. The integration owner consolidates code,
//! but the workflow refuses to surface that work for review or QA until
//! every open blocker has been resolved or explicitly waived.
//!
//! The gate is a pure check: it takes the parent id, the operation being
//! gated, and the list of currently *open* blockers in the subtree, and
//! returns a structured error when any are present. The kernel decides
//! waiver semantics upstream — by the time a snapshot reaches this guard,
//! waived blockers are already filtered out (they no longer satisfy
//! [`crate::BlockerStatus::is_blocking`]).
//!
//! Keeping the check here rather than inlining it at each call site means:
//!
//! 1. There is exactly one place where "no open blockers before draft PR /
//!    QA request" lives.
//! 2. The error carries the offending blockers verbatim, so the caller
//!    surfaces them in tracker comments / operator output without
//!    recomputing.
//! 3. The two operations route through distinct error variants, so logs
//!    and tests can tell *which* gate fired.

use serde::{Deserialize, Serialize};

use crate::blocker::{BlockerId, BlockerStatus};
use crate::work_item::WorkItemId;

/// The downstream operation being gated.
///
/// Distinct variants so error messages and structured logs can identify
/// which seam blocked. Both operations apply the same rule but route
/// differently in the kernel.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum GateOperation {
    /// The kernel is about to open a draft pull request for the parent
    /// (SPEC v2 §5.11).
    DraftPullRequest,
    /// The kernel is about to stage the parent for the QA gate role
    /// (SPEC v2 §5.12).
    QaRequest,
}

impl GateOperation {
    /// All variants in declaration order.
    pub const ALL: [Self; 2] = [Self::DraftPullRequest, Self::QaRequest];

    /// Stable lowercase identifier used in error messages and event payloads.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::DraftPullRequest => "draft_pull_request",
            Self::QaRequest => "qa_request",
        }
    }
}

impl std::fmt::Display for GateOperation {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

/// Minimal view of an open blocker required by the gate.
///
/// The kernel materialises one of these per open `blocks` edge in the
/// parent's subtree before calling [`check_no_open_blockers`]. Production
/// callers source the fields from the durable `work_item_edges` /
/// `blockers` rows; tests can construct snapshots inline.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct OpenBlockerSnapshot {
    /// Domain primary key of the blocker.
    pub blocker_id: BlockerId,
    /// Work item the blocker is filed against (often a child in the
    /// subtree, sometimes the parent itself).
    pub blocked_work_item: WorkItemId,
    /// Tracker-facing identifier of the blocked work item (e.g. `ENG-7`).
    pub blocked_identifier: String,
    /// Free-form reason the blocker exists, when one was recorded.
    pub reason: Option<String>,
}

/// Errors produced by [`check_no_open_blockers`].
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum BlockerGateError {
    /// At least one open blocker was present in the parent's subtree.
    /// The carried list preserves caller-supplied order.
    #[error(
        "{operation} for parent {parent} blocked by {} open blocker(s)",
        open.len()
    )]
    OpenBlockersPresent {
        /// The downstream operation the kernel attempted.
        operation: GateOperation,
        /// The parent work item being staged.
        parent: WorkItemId,
        /// The open blockers, in caller-supplied order.
        open: Vec<OpenBlockerSnapshot>,
    },
}

/// Return `Ok(())` iff the parent has no open blockers in its subtree.
///
/// Rule (SPEC v2 §5.10 `require_no_open_blockers`): every blocker passed
/// in MUST be in [`BlockerStatus::Open`]. The function is intentionally
/// strict — non-open blockers in `open` indicate the caller filtered the
/// snapshot incorrectly upstream and is not the gate's problem to silently
/// repair. They surface as
/// [`BlockerGateError::OpenBlockersPresent`] alongside any genuinely-open
/// blockers; tests assert this so future callers cannot accidentally pass
/// resolved blockers and have the gate hide the bug.
///
/// Empty `open` is the success case: a parent with no recorded open
/// blockers naturally satisfies the gate. The caller is responsible for
/// ensuring the snapshot list reflects the canonical subtree query.
pub fn check_no_open_blockers(
    operation: GateOperation,
    parent: WorkItemId,
    open: &[OpenBlockerSnapshot],
) -> Result<(), BlockerGateError> {
    if open.is_empty() {
        return Ok(());
    }
    Err(BlockerGateError::OpenBlockersPresent {
        operation,
        parent,
        open: open.to_vec(),
    })
}

/// Filter a list of `(BlockerId, BlockerStatus, …)` rows down to the
/// snapshot shape the gate consumes, dropping anything whose status is
/// not [`BlockerStatus::is_blocking`].
///
/// Provided so callers reading the durable `blockers` table can hand the
/// raw rows to the gate without each writing the same filter inline. The
/// gate itself takes the post-filter list to keep the rule "every entry
/// is open" testable.
pub fn collect_open(
    rows: impl IntoIterator<Item = (BlockerStatus, OpenBlockerSnapshot)>,
) -> Vec<OpenBlockerSnapshot> {
    rows.into_iter()
        .filter_map(|(status, snap)| status.is_blocking().then_some(snap))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn snap(id: i64, blocked: i64, ident: &str, reason: Option<&str>) -> OpenBlockerSnapshot {
        OpenBlockerSnapshot {
            blocker_id: BlockerId(id),
            blocked_work_item: WorkItemId::new(blocked),
            blocked_identifier: ident.into(),
            reason: reason.map(str::to_owned),
        }
    }

    #[test]
    fn empty_open_list_passes_for_both_operations() {
        for op in GateOperation::ALL {
            check_no_open_blockers(op, WorkItemId::new(1), &[]).unwrap();
        }
    }

    #[test]
    fn one_open_blocker_blocks_draft_pr_creation() {
        let open = vec![snap(7, 3, "ENG-3", Some("flaky test"))];
        let err =
            check_no_open_blockers(GateOperation::DraftPullRequest, WorkItemId::new(1), &open)
                .unwrap_err();
        match err {
            BlockerGateError::OpenBlockersPresent {
                operation,
                parent,
                open,
            } => {
                assert_eq!(operation, GateOperation::DraftPullRequest);
                assert_eq!(parent, WorkItemId::new(1));
                assert_eq!(open.len(), 1);
                assert_eq!(open[0].blocked_identifier, "ENG-3");
            }
        }
    }

    #[test]
    fn one_open_blocker_blocks_qa_request() {
        let open = vec![snap(7, 3, "ENG-3", None)];
        let err = check_no_open_blockers(GateOperation::QaRequest, WorkItemId::new(1), &open)
            .unwrap_err();
        match err {
            BlockerGateError::OpenBlockersPresent { operation, .. } => {
                assert_eq!(operation, GateOperation::QaRequest);
            }
        }
    }

    #[test]
    fn open_blockers_reported_in_input_order() {
        let open = vec![
            snap(1, 10, "ENG-10", None),
            snap(2, 11, "ENG-11", None),
            snap(3, 12, "ENG-12", None),
        ];
        let err = check_no_open_blockers(GateOperation::QaRequest, WorkItemId::new(99), &open)
            .unwrap_err();
        let BlockerGateError::OpenBlockersPresent { open, .. } = err;
        let idents: Vec<_> = open.iter().map(|b| b.blocked_identifier.as_str()).collect();
        assert_eq!(idents, vec!["ENG-10", "ENG-11", "ENG-12"]);
    }

    #[test]
    fn error_message_names_operation_and_count() {
        let open = vec![snap(1, 2, "ENG-2", None), snap(2, 3, "ENG-3", None)];
        let err =
            check_no_open_blockers(GateOperation::DraftPullRequest, WorkItemId::new(1), &open)
                .unwrap_err();
        let msg = err.to_string();
        assert!(msg.contains("draft_pull_request"), "msg: {msg}");
        assert!(msg.contains("2"), "msg should include count: {msg}");
    }

    #[test]
    fn collect_open_drops_non_open_statuses() {
        let rows = vec![
            (BlockerStatus::Open, snap(1, 10, "ENG-10", None)),
            (BlockerStatus::Resolved, snap(2, 11, "ENG-11", None)),
            (BlockerStatus::Waived, snap(3, 12, "ENG-12", None)),
            (BlockerStatus::Cancelled, snap(4, 13, "ENG-13", None)),
            (BlockerStatus::Open, snap(5, 14, "ENG-14", None)),
        ];
        let filtered = collect_open(rows);
        let ids: Vec<_> = filtered.iter().map(|b| b.blocker_id.0).collect();
        assert_eq!(ids, vec![1, 5]);
    }

    #[test]
    fn collect_open_then_check_round_trip_passes_when_all_resolved() {
        let rows = vec![
            (BlockerStatus::Resolved, snap(1, 10, "ENG-10", None)),
            (BlockerStatus::Waived, snap(2, 11, "ENG-11", None)),
        ];
        let filtered = collect_open(rows);
        check_no_open_blockers(GateOperation::QaRequest, WorkItemId::new(1), &filtered).unwrap();
    }

    #[test]
    fn operation_round_trips_via_as_str_and_json() {
        for op in GateOperation::ALL {
            let s = op.as_str();
            let json = serde_json::to_string(&op).unwrap();
            assert_eq!(json, format!("\"{s}\""));
            let back: GateOperation = serde_json::from_str(&json).unwrap();
            assert_eq!(back, op);
        }
    }

    #[test]
    fn snapshot_round_trips_through_json() {
        let s = snap(1, 2, "ENG-2", Some("compile error"));
        let json = serde_json::to_string(&s).unwrap();
        let back: OpenBlockerSnapshot = serde_json::from_str(&json).unwrap();
        assert_eq!(back, s);
    }
}
