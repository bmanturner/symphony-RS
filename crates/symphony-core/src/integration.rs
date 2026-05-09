//! Domain model for integration records (SPEC v2 §5.10, §6.4).
//!
//! An [`IntegrationRecord`] is the durable proof that an integration owner
//! consolidated child work into a canonical branch/worktree. It is the
//! evidence the kernel reads when checking the parent closeout gate
//! (ARCH §6.2: "integration record exists when required") and the
//! evidence QA reads to know what artifact it is verifying.
//!
//! This module supplies only the *domain* shape:
//!
//! * [`IntegrationId`], [`IntegrationRecord`] — the durable record;
//! * [`IntegrationStatus`] — the pending → succeeded/failed lifecycle;
//! * [`IntegrationMergeStrategy`] — mirrors the workflow-config enum so the
//!   record captures *how* consolidation was performed;
//! * [`IntegrationConflict`] — structured conflict evidence required when
//!   the record ends in [`IntegrationStatus::Failed`] or
//!   [`IntegrationStatus::Blocked`];
//! * [`IntegrationError`] — invariant violations surfaced by
//!   [`IntegrationRecord::try_new`].
//!
//! Persistence (the `integration_records` table — added in a later phase)
//! lives in `symphony-state`. This crate enforces the runtime invariants
//! the kernel branches on, mirroring the [`crate::QaOutcome`] /
//! [`crate::Blocker`] construction pattern.

use serde::{Deserialize, Serialize};

use crate::blocker::{BlockerId, RunRef};
use crate::role::RoleName;
use crate::work_item::WorkItemId;

/// Strongly-typed primary key for an [`IntegrationRecord`].
///
/// Matches the `i64` row id used by the durable storage table and is kept
/// as a separate Rust type from [`WorkItemId`] / [`crate::QaVerdictId`] so
/// an accidental swap is a type error.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(transparent)]
pub struct IntegrationId(pub i64);

impl IntegrationId {
    /// Construct an [`IntegrationId`] from a raw row id.
    pub fn new(id: i64) -> Self {
        Self(id)
    }

    /// Borrow the inner row id.
    pub fn get(self) -> i64 {
        self.0
    }
}

impl std::fmt::Display for IntegrationId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        std::fmt::Display::fmt(&self.0, f)
    }
}

/// Lifecycle of an integration attempt (SPEC v2 §5.10).
///
/// The integration owner produces one record per consolidation attempt.
/// [`Self::Pending`] is the initial state when work is queued but the
/// owner has not yet started; [`Self::InProgress`] is the active run;
/// the remaining variants are terminal. Terminal records are durable
/// evidence — operators file a *new* record rather than mutate a
/// terminal one (mirrors [`crate::BlockerStatus`]).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum IntegrationStatus {
    /// Work item is queued for the integration owner but consolidation
    /// has not yet started.
    Pending,
    /// The integration owner is actively consolidating.
    InProgress,
    /// Consolidation completed; the canonical branch/worktree carries the
    /// integrated artifact and the record satisfies the parent closeout
    /// gate (ARCH §6.2).
    Succeeded,
    /// Consolidation failed and the workflow's `conflict_policy` routes
    /// back for a repair turn or owner change. The record MUST carry at
    /// least one [`IntegrationConflict`] explaining what failed.
    Failed,
    /// Consolidation could not proceed because of an external blocker
    /// (e.g. an upstream child was reopened, CI red on base). Conflicts
    /// MAY be empty; [`IntegrationRecord::blockers_filed`] explains the
    /// gating reason.
    Blocked,
    /// The integration attempt was abandoned before reaching a terminal
    /// outcome (parent cancelled, scope changed). Preserved for audit.
    Cancelled,
}

impl IntegrationStatus {
    /// All variants in declaration order.
    pub const ALL: [Self; 6] = [
        Self::Pending,
        Self::InProgress,
        Self::Succeeded,
        Self::Failed,
        Self::Blocked,
        Self::Cancelled,
    ];

    /// Stable lowercase identifier used in YAML config and JSON payloads.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Pending => "pending",
            Self::InProgress => "in_progress",
            Self::Succeeded => "succeeded",
            Self::Failed => "failed",
            Self::Blocked => "blocked",
            Self::Cancelled => "cancelled",
        }
    }

    /// True when no further state changes are expected.
    pub fn is_terminal(self) -> bool {
        matches!(
            self,
            Self::Succeeded | Self::Failed | Self::Blocked | Self::Cancelled
        )
    }

    /// True when the record satisfies the parent closeout integration gate
    /// (ARCH §6.2). Only [`Self::Succeeded`] qualifies.
    pub fn is_pass(self) -> bool {
        matches!(self, Self::Succeeded)
    }
}

impl std::fmt::Display for IntegrationStatus {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

/// Merge strategy used to produce the canonical artifact (SPEC v2 §5.10).
///
/// Mirrors the workflow-config enum so the durable record captures *how*
/// the integration owner consolidated children, not just that it happened.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum IntegrationMergeStrategy {
    /// Replay each child branch in order via `git cherry-pick`.
    SequentialCherryPick,
    /// Merge each child branch with merge commits.
    MergeCommits,
    /// All children landed directly on the shared integration branch; the
    /// integration owner verifies the result rather than merging.
    SharedBranch,
}

impl IntegrationMergeStrategy {
    /// All variants in declaration order.
    pub const ALL: [Self; 3] = [
        Self::SequentialCherryPick,
        Self::MergeCommits,
        Self::SharedBranch,
    ];

    /// Stable lowercase identifier used in YAML config and JSON payloads.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::SequentialCherryPick => "sequential_cherry_pick",
            Self::MergeCommits => "merge_commits",
            Self::SharedBranch => "shared_branch",
        }
    }
}

impl std::fmt::Display for IntegrationMergeStrategy {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

/// Structured evidence for a consolidation failure (SPEC v2 §5.10).
///
/// Conflict evidence is recorded once per problematic child or merge step
/// so the integration owner's repair turn (or downstream blocker routing)
/// has the original failure surface to act on.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct IntegrationConflict {
    /// The child work item whose change could not be consolidated. May be
    /// `None` for conflicts that originate outside any single child
    /// (e.g. an integration test that failed after all children merged).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub child_id: Option<WorkItemId>,
    /// The branch/ref the integration owner attempted to consolidate.
    /// Recorded so a repair turn can re-target the same source ref.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub child_branch: Option<String>,
    /// Files that conflicted, when the failure was a git merge conflict.
    /// Empty for non-merge failures (e.g. integration test failure).
    #[serde(default)]
    pub files: Vec<String>,
    /// Operator-facing description of the conflict. Required and non-empty.
    pub message: String,
}

/// Invariant violations for [`IntegrationRecord::try_new`].
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum IntegrationError {
    /// The record was constructed with an empty operator-facing summary
    /// while the status requires one. SPEC §5.10 — succeeded records MUST
    /// carry a summary so QA and PR bodies can quote it.
    #[error("integration record with status {0} requires a non-empty summary")]
    MissingSummary(IntegrationStatus),
    /// A [`IntegrationStatus::Succeeded`] record was constructed without
    /// a canonical integration branch.
    #[error("succeeded integration record requires an integration_branch")]
    MissingIntegrationBranch,
    /// The branch ref recorded was empty/whitespace-only.
    #[error("integration_branch must not be empty")]
    EmptyBranch,
    /// A [`IntegrationStatus::Failed`] record was constructed without any
    /// [`IntegrationConflict`] evidence. Failed integration without
    /// conflicts would leave the kernel without anything to route on.
    #[error("failed integration record requires at least one conflict entry")]
    MissingConflictEvidence,
    /// An [`IntegrationConflict`] was constructed with an empty `message`.
    #[error("integration conflict message must not be empty")]
    EmptyConflictMessage,
    /// A child work item appeared more than once in
    /// [`IntegrationRecord::children_consolidated`]. The kernel uses this
    /// list to gate the parent closeout, so duplicates would distort
    /// "all required children integrated" checks.
    #[error("duplicate child {0} in children_consolidated")]
    DuplicateChild(WorkItemId),
    /// `parent_id` appeared in `children_consolidated`. Self-consolidation
    /// would create a cycle in the dependency graph.
    #[error("integration record parent {0} cannot consolidate itself")]
    SelfConsolidation(WorkItemId),
    /// A non-pending record was constructed without a run reference. SPEC
    /// §5.10 ties active/terminal integration to the integration-owner run
    /// that produced it; only [`IntegrationStatus::Pending`] is allowed to
    /// have no run yet.
    #[error("integration status {0} requires a run reference")]
    MissingRun(IntegrationStatus),
}

/// Durable integration record (SPEC v2 §5.10, §6.4; ARCH §3.3, §6.2).
///
/// The record links a *parent* work item (the broad/decomposed issue
/// being integrated) to:
///
/// * the integration-owner role and run that produced it,
/// * the canonical integration branch/worktree,
/// * the list of children consolidated,
/// * the merge strategy used,
/// * an operator-facing summary suitable for the PR body,
/// * any conflicts encountered,
/// * any blockers filed alongside the integration attempt.
///
/// Construct via [`Self::try_new`] so the status-vs-evidence invariants
/// are enforced once at creation time, the same pattern as
/// [`crate::QaOutcome`] / [`crate::Blocker`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct IntegrationRecord {
    /// Durable id (matches the storage row id).
    pub id: IntegrationId,
    /// The work item being integrated (typically a decomposed parent).
    pub parent_id: WorkItemId,
    /// Role that owned the integration. Typically the workflow's
    /// `integration_owner` role.
    pub owner_role: RoleName,
    /// The integration-owner run that produced or is producing this
    /// record. `None` is allowed only for [`IntegrationStatus::Pending`]
    /// records that have been queued but not yet started.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub run_id: Option<RunRef>,
    /// Lifecycle status.
    pub status: IntegrationStatus,
    /// Merge strategy the owner used (or intends to use).
    pub merge_strategy: IntegrationMergeStrategy,
    /// Canonical integration branch ref (e.g. `symphony/integration/PROJ-42`).
    /// Required for [`IntegrationStatus::Succeeded`]. May be unset for
    /// pending/cancelled records.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub integration_branch: Option<String>,
    /// Base ref the integration branch was cut from (e.g. `main`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub base_ref: Option<String>,
    /// SHA of the canonical branch tip at record time, when known.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub head_sha: Option<String>,
    /// Absolute or workspace-relative path to the integration workspace,
    /// when the workflow uses a worktree strategy.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub workspace_path: Option<String>,
    /// Children consolidated into the canonical branch. Order is
    /// significant for [`IntegrationMergeStrategy::SequentialCherryPick`];
    /// the kernel preserves it.
    #[serde(default)]
    pub children_consolidated: Vec<WorkItemId>,
    /// Operator-facing summary suitable for PR bodies and QA briefs.
    /// Required for [`IntegrationStatus::Succeeded`] (SPEC §5.10
    /// `require_integration_summary`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub summary: Option<String>,
    /// Structured conflict evidence. Required to be non-empty for
    /// [`IntegrationStatus::Failed`].
    #[serde(default)]
    pub conflicts: Vec<IntegrationConflict>,
    /// Blockers filed alongside the integration attempt (e.g. when status
    /// is [`IntegrationStatus::Blocked`]).
    #[serde(default)]
    pub blockers_filed: Vec<BlockerId>,
}

impl IntegrationRecord {
    /// Construct an integration record, enforcing status-vs-evidence
    /// invariants. See [`IntegrationError`] variants for the rule set.
    #[allow(clippy::too_many_arguments)]
    pub fn try_new(
        id: IntegrationId,
        parent_id: WorkItemId,
        owner_role: RoleName,
        run_id: Option<RunRef>,
        status: IntegrationStatus,
        merge_strategy: IntegrationMergeStrategy,
        integration_branch: Option<String>,
        base_ref: Option<String>,
        head_sha: Option<String>,
        workspace_path: Option<String>,
        children_consolidated: Vec<WorkItemId>,
        summary: Option<String>,
        conflicts: Vec<IntegrationConflict>,
        blockers_filed: Vec<BlockerId>,
    ) -> Result<Self, IntegrationError> {
        for conflict in &conflicts {
            if conflict.message.trim().is_empty() {
                return Err(IntegrationError::EmptyConflictMessage);
            }
        }

        let mut seen = Vec::with_capacity(children_consolidated.len());
        for child in &children_consolidated {
            if *child == parent_id {
                return Err(IntegrationError::SelfConsolidation(*child));
            }
            if seen.contains(child) {
                return Err(IntegrationError::DuplicateChild(*child));
            }
            seen.push(*child);
        }

        if status != IntegrationStatus::Pending && run_id.is_none() {
            return Err(IntegrationError::MissingRun(status));
        }

        let branch_present = integration_branch
            .as_deref()
            .is_some_and(|b| !b.trim().is_empty());
        if integration_branch.as_deref().is_some_and(str::is_empty) {
            return Err(IntegrationError::EmptyBranch);
        }

        let summary_present = summary.as_deref().is_some_and(|s| !s.trim().is_empty());

        match status {
            IntegrationStatus::Succeeded => {
                if !summary_present {
                    return Err(IntegrationError::MissingSummary(status));
                }
                if !branch_present {
                    return Err(IntegrationError::MissingIntegrationBranch);
                }
            }
            IntegrationStatus::Failed if conflicts.is_empty() => {
                return Err(IntegrationError::MissingConflictEvidence);
            }
            _ => {}
        }

        Ok(Self {
            id,
            parent_id,
            owner_role,
            run_id,
            status,
            merge_strategy,
            integration_branch,
            base_ref,
            head_sha,
            workspace_path,
            children_consolidated,
            summary,
            conflicts,
            blockers_filed,
        })
    }

    /// True when the record satisfies the parent closeout integration gate.
    pub fn is_pass(&self) -> bool {
        self.status.is_pass()
    }

    /// Iterate the children that were consolidated into the canonical
    /// branch, in insertion order.
    pub fn children(&self) -> impl Iterator<Item = &WorkItemId> {
        self.children_consolidated.iter()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn role() -> RoleName {
        RoleName::new("platform_lead")
    }

    fn succeeded(id: i64, parent: WorkItemId) -> IntegrationRecord {
        IntegrationRecord::try_new(
            IntegrationId::new(id),
            parent,
            role(),
            Some(RunRef::new(7)),
            IntegrationStatus::Succeeded,
            IntegrationMergeStrategy::SequentialCherryPick,
            Some("symphony/integration/PROJ-42".into()),
            Some("main".into()),
            Some("deadbeef".into()),
            Some("../proj-integration".into()),
            vec![WorkItemId::new(11), WorkItemId::new(12)],
            Some("Consolidated 2 children, all tests green.".into()),
            vec![],
            vec![],
        )
        .expect("succeeded record should construct")
    }

    #[test]
    fn integration_status_round_trips_and_partitions_terminal() {
        for status in IntegrationStatus::ALL {
            let json = serde_json::to_string(&status).unwrap();
            assert_eq!(json, format!("\"{}\"", status.as_str()));
            let back: IntegrationStatus = serde_json::from_str(&json).unwrap();
            assert_eq!(back, status);
        }
        assert!(IntegrationStatus::Succeeded.is_terminal());
        assert!(IntegrationStatus::Failed.is_terminal());
        assert!(IntegrationStatus::Blocked.is_terminal());
        assert!(IntegrationStatus::Cancelled.is_terminal());
        assert!(!IntegrationStatus::Pending.is_terminal());
        assert!(!IntegrationStatus::InProgress.is_terminal());
        assert!(IntegrationStatus::Succeeded.is_pass());
        assert!(!IntegrationStatus::Failed.is_pass());
        assert!(!IntegrationStatus::Blocked.is_pass());
    }

    #[test]
    fn merge_strategy_round_trips() {
        for strategy in IntegrationMergeStrategy::ALL {
            let json = serde_json::to_string(&strategy).unwrap();
            assert_eq!(json, format!("\"{}\"", strategy.as_str()));
            let back: IntegrationMergeStrategy = serde_json::from_str(&json).unwrap();
            assert_eq!(back, strategy);
        }
    }

    #[test]
    fn succeeded_record_round_trips_through_json() {
        let record = succeeded(1, WorkItemId::new(10));
        assert!(record.is_pass());
        let json = serde_json::to_string(&record).unwrap();
        let back: IntegrationRecord = serde_json::from_str(&json).unwrap();
        assert_eq!(back, record);
        let kids: Vec<_> = record.children().copied().collect();
        assert_eq!(kids, vec![WorkItemId::new(11), WorkItemId::new(12)]);
    }

    #[test]
    fn succeeded_requires_summary_and_branch() {
        let err = IntegrationRecord::try_new(
            IntegrationId::new(1),
            WorkItemId::new(10),
            role(),
            Some(RunRef::new(7)),
            IntegrationStatus::Succeeded,
            IntegrationMergeStrategy::SequentialCherryPick,
            Some("symphony/integration/x".into()),
            None,
            None,
            None,
            vec![],
            None,
            vec![],
            vec![],
        )
        .unwrap_err();
        assert_eq!(
            err,
            IntegrationError::MissingSummary(IntegrationStatus::Succeeded)
        );

        let err = IntegrationRecord::try_new(
            IntegrationId::new(1),
            WorkItemId::new(10),
            role(),
            Some(RunRef::new(7)),
            IntegrationStatus::Succeeded,
            IntegrationMergeStrategy::SequentialCherryPick,
            None,
            None,
            None,
            None,
            vec![],
            Some("ok".into()),
            vec![],
            vec![],
        )
        .unwrap_err();
        assert_eq!(err, IntegrationError::MissingIntegrationBranch);

        let err = IntegrationRecord::try_new(
            IntegrationId::new(1),
            WorkItemId::new(10),
            role(),
            Some(RunRef::new(7)),
            IntegrationStatus::Succeeded,
            IntegrationMergeStrategy::SequentialCherryPick,
            Some("   ".into()),
            None,
            None,
            None,
            vec![],
            Some("ok".into()),
            vec![],
            vec![],
        )
        .unwrap_err();
        assert_eq!(err, IntegrationError::MissingIntegrationBranch);
    }

    #[test]
    fn empty_branch_string_is_rejected() {
        let err = IntegrationRecord::try_new(
            IntegrationId::new(1),
            WorkItemId::new(10),
            role(),
            None,
            IntegrationStatus::Pending,
            IntegrationMergeStrategy::SharedBranch,
            Some(String::new()),
            None,
            None,
            None,
            vec![],
            None,
            vec![],
            vec![],
        )
        .unwrap_err();
        assert_eq!(err, IntegrationError::EmptyBranch);
    }

    #[test]
    fn failed_requires_conflict_evidence() {
        let err = IntegrationRecord::try_new(
            IntegrationId::new(1),
            WorkItemId::new(10),
            role(),
            Some(RunRef::new(7)),
            IntegrationStatus::Failed,
            IntegrationMergeStrategy::SequentialCherryPick,
            Some("symphony/integration/x".into()),
            None,
            None,
            None,
            vec![],
            Some("partial".into()),
            vec![],
            vec![],
        )
        .unwrap_err();
        assert_eq!(err, IntegrationError::MissingConflictEvidence);

        IntegrationRecord::try_new(
            IntegrationId::new(1),
            WorkItemId::new(10),
            role(),
            Some(RunRef::new(7)),
            IntegrationStatus::Failed,
            IntegrationMergeStrategy::SequentialCherryPick,
            Some("symphony/integration/x".into()),
            None,
            None,
            None,
            vec![],
            None,
            vec![IntegrationConflict {
                child_id: Some(WorkItemId::new(11)),
                child_branch: Some("feat/a".into()),
                files: vec!["src/lib.rs".into()],
                message: "merge conflict in src/lib.rs".into(),
            }],
            vec![],
        )
        .expect("failed with conflicts should construct");
    }

    #[test]
    fn empty_conflict_message_is_rejected() {
        let err = IntegrationRecord::try_new(
            IntegrationId::new(1),
            WorkItemId::new(10),
            role(),
            Some(RunRef::new(7)),
            IntegrationStatus::Failed,
            IntegrationMergeStrategy::SequentialCherryPick,
            Some("symphony/integration/x".into()),
            None,
            None,
            None,
            vec![],
            None,
            vec![IntegrationConflict {
                child_id: None,
                child_branch: None,
                files: vec![],
                message: "   ".into(),
            }],
            vec![],
        )
        .unwrap_err();
        assert_eq!(err, IntegrationError::EmptyConflictMessage);
    }

    #[test]
    fn duplicate_child_consolidation_is_rejected() {
        let err = IntegrationRecord::try_new(
            IntegrationId::new(1),
            WorkItemId::new(10),
            role(),
            Some(RunRef::new(7)),
            IntegrationStatus::InProgress,
            IntegrationMergeStrategy::MergeCommits,
            None,
            None,
            None,
            None,
            vec![WorkItemId::new(11), WorkItemId::new(11)],
            None,
            vec![],
            vec![],
        )
        .unwrap_err();
        assert_eq!(err, IntegrationError::DuplicateChild(WorkItemId::new(11)));
    }

    #[test]
    fn parent_in_consolidated_children_is_rejected() {
        let err = IntegrationRecord::try_new(
            IntegrationId::new(1),
            WorkItemId::new(10),
            role(),
            Some(RunRef::new(7)),
            IntegrationStatus::InProgress,
            IntegrationMergeStrategy::MergeCommits,
            None,
            None,
            None,
            None,
            vec![WorkItemId::new(10)],
            None,
            vec![],
            vec![],
        )
        .unwrap_err();
        assert_eq!(
            err,
            IntegrationError::SelfConsolidation(WorkItemId::new(10))
        );
    }

    #[test]
    fn non_pending_status_requires_run_reference() {
        for status in [
            IntegrationStatus::InProgress,
            IntegrationStatus::Succeeded,
            IntegrationStatus::Failed,
            IntegrationStatus::Blocked,
            IntegrationStatus::Cancelled,
        ] {
            let err = IntegrationRecord::try_new(
                IntegrationId::new(1),
                WorkItemId::new(10),
                role(),
                None,
                status,
                IntegrationMergeStrategy::SharedBranch,
                Some("symphony/integration/x".into()),
                None,
                None,
                None,
                vec![],
                Some("ok".into()),
                vec![IntegrationConflict {
                    child_id: None,
                    child_branch: None,
                    files: vec![],
                    message: "x".into(),
                }],
                vec![],
            )
            .unwrap_err();
            assert_eq!(err, IntegrationError::MissingRun(status));
        }
    }

    #[test]
    fn pending_record_does_not_require_run_or_evidence() {
        IntegrationRecord::try_new(
            IntegrationId::new(1),
            WorkItemId::new(10),
            role(),
            None,
            IntegrationStatus::Pending,
            IntegrationMergeStrategy::SequentialCherryPick,
            None,
            None,
            None,
            None,
            vec![],
            None,
            vec![],
            vec![],
        )
        .expect("pending records may be queued before any run");
    }

    #[test]
    fn integration_id_round_trips_as_bare_integer() {
        let id = IntegrationId::new(42);
        let json = serde_json::to_string(&id).unwrap();
        assert_eq!(json, "42");
        let back: IntegrationId = serde_json::from_str(&json).unwrap();
        assert_eq!(back, id);
    }

    #[test]
    fn blocked_status_allows_no_conflicts_with_blockers() {
        let record = IntegrationRecord::try_new(
            IntegrationId::new(1),
            WorkItemId::new(10),
            role(),
            Some(RunRef::new(7)),
            IntegrationStatus::Blocked,
            IntegrationMergeStrategy::SharedBranch,
            None,
            None,
            None,
            None,
            vec![],
            None,
            vec![],
            vec![BlockerId::new(99)],
        )
        .expect("blocked record may carry blockers without conflicts");
        assert!(!record.is_pass());
        assert_eq!(record.blockers_filed, vec![BlockerId::new(99)]);
    }
}
