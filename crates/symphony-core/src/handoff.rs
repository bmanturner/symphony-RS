//! Domain model for run handoffs (SPEC v2 §4.7).
//!
//! A handoff is the *structured output* an agent emits at the end of a run.
//! The kernel reads it to decide the next gate (integration, QA, human
//! review, blocked, done), to file durable [`crate::Blocker`] rows from
//! `blockers_created`, and to create or propose follow-up issues from
//! `followups_created_or_proposed`.
//!
//! Two design rules from SPEC §4.7 are enforced here:
//!
//! 1. `ready_for` is *advisory*. An agent saying `done` does not bypass
//!    integration/QA gates — that is a kernel decision in later phases.
//!    This module only validates the *shape* of the handoff so the kernel
//!    has the evidence it needs to make the gating call.
//! 2. `ready_for: blocked` is meaningless without evidence. Either the
//!    agent attached at least one [`HandoffBlockerRequest`] or it provided
//!    a structured `block_reason`. [`Handoff::validate`] rejects bare
//!    `blocked` handoffs.
//!
//! The full [`crate::Blocker`] / `FollowupIssueRequest` durable types live
//! in their own modules. Handoffs carry *requests* — light-weight intent —
//! that the kernel converts into durable rows after policy checks.

use serde::{Deserialize, Serialize};

use crate::blocker::{BlockerId, BlockerSeverity};
use crate::qa::{AcceptanceCriterionTrace, QaEvidence, QaVerdict};
use crate::role::RoleName;
use crate::work_item::WorkItemId;

/// Advisory next-gate hint from the agent (SPEC v2 §4.7).
///
/// The kernel may downgrade `done` to a stricter gate if integration or
/// QA is required by workflow policy; see SPEC §4.7. Use
/// [`Self::requires_block_evidence`] to check the only invariant this
/// module enforces purely from the variant.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ReadyFor {
    /// Enqueue for integration-owner consolidation.
    Integration,
    /// Enqueue for QA. The kernel still respects integration prerequisites.
    Qa,
    /// Pause with a durable approval item awaiting a human.
    HumanReview,
    /// Halt; require [`Handoff::blockers_created`] or
    /// [`Handoff::block_reason`] for evidence.
    Blocked,
    /// Mark complete. Valid only for simple non-decomposed work; the
    /// kernel downgrades to the next required gate otherwise.
    Done,
}

impl ReadyFor {
    /// All variants in declaration order.
    pub const ALL: [Self; 5] = [
        Self::Integration,
        Self::Qa,
        Self::HumanReview,
        Self::Blocked,
        Self::Done,
    ];

    /// Stable lowercase identifier used in YAML and JSON payloads.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Integration => "integration",
            Self::Qa => "qa",
            Self::HumanReview => "human_review",
            Self::Blocked => "blocked",
            Self::Done => "done",
        }
    }

    /// True when the variant is meaningless without at least one blocker
    /// or a structured `block_reason`.
    pub fn requires_block_evidence(self) -> bool {
        matches!(self, Self::Blocked)
    }

    /// Map this advisory hint to the typed kernel action it requests
    /// (SPEC v2 §4.7).
    ///
    /// The mapping is total and stable: every variant has exactly one
    /// consequence. The kernel is still free to *downgrade* the
    /// consequence (e.g. promote a [`ReadyForConsequence::AttemptDone`]
    /// into [`ReadyForConsequence::EnqueueIntegration`] when the work
    /// item has incomplete children); this method only records the
    /// agent's stated intent in typed form.
    pub fn consequence(self) -> ReadyForConsequence {
        match self {
            Self::Integration => ReadyForConsequence::EnqueueIntegration,
            Self::Qa => ReadyForConsequence::EnqueueQa,
            Self::HumanReview => ReadyForConsequence::PauseAwaitingHumanReview,
            Self::Blocked => ReadyForConsequence::HoldBlocked,
            Self::Done => ReadyForConsequence::AttemptDone,
        }
    }
}

/// Typed kernel-side queue consequence of a [`ReadyFor`] hint
/// (SPEC v2 §4.7).
///
/// `ReadyFor` is the *agent's* vocabulary; `ReadyForConsequence` is the
/// *kernel's*. The two are kept separate so the kernel can downgrade an
/// agent's hint (e.g. `done` → `qa`) without changing the durable
/// handoff payload, and so future kernel-only consequences can be added
/// without widening the agent-facing schema.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ReadyForConsequence {
    /// Enqueue the work item on the integration-owner queue.
    EnqueueIntegration,
    /// Enqueue the work item on the QA queue. The kernel is still
    /// responsible for refusing to dispatch QA before integration when
    /// integration is required.
    EnqueueQa,
    /// Pause the work item with a durable human-review approval row.
    PauseAwaitingHumanReview,
    /// Hold the work item in a blocked state. Requires durable blocker
    /// rows or a structured `block_reason` per [`Handoff::validate`].
    HoldBlocked,
    /// Attempt to mark the work item terminal. The kernel still gates
    /// on children, integration, blockers, and QA per workflow policy
    /// and will downgrade this consequence to the next required gate
    /// when those checks fail.
    AttemptDone,
}

impl ReadyForConsequence {
    /// All variants in declaration order.
    pub const ALL: [Self; 5] = [
        Self::EnqueueIntegration,
        Self::EnqueueQa,
        Self::PauseAwaitingHumanReview,
        Self::HoldBlocked,
        Self::AttemptDone,
    ];

    /// Stable lowercase identifier used in JSON payloads.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::EnqueueIntegration => "enqueue_integration",
            Self::EnqueueQa => "enqueue_qa",
            Self::PauseAwaitingHumanReview => "pause_awaiting_human_review",
            Self::HoldBlocked => "hold_blocked",
            Self::AttemptDone => "attempt_done",
        }
    }
}

impl std::fmt::Display for ReadyForConsequence {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

impl std::fmt::Display for ReadyFor {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

/// Branch/workspace pointer recorded with a handoff (SPEC v2 §4.7).
///
/// At least one of `branch` or `workspace_path` MUST be populated so the
/// kernel can verify integration prerequisites against a real ref or
/// directory. The `base_ref` is optional context for integration-owner
/// merge planning.
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct BranchOrWorkspace {
    /// Git branch the run produced output on (e.g. `feat/foo`). Optional
    /// because some workflows operate on workspaces without a dedicated
    /// branch (shared-branch policy).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub branch: Option<String>,
    /// Absolute or workspace-relative path to the workspace claim. Optional
    /// when the run only acted on a remote ref.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub workspace_path: Option<String>,
    /// Base ref the branch was created from (e.g. `main`). Optional.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub base_ref: Option<String>,
}

impl BranchOrWorkspace {
    /// True if neither a branch nor a workspace path is recorded.
    pub fn is_empty(&self) -> bool {
        self.branch.as_deref().is_none_or(str::is_empty)
            && self.workspace_path.as_deref().is_none_or(str::is_empty)
    }
}

/// Agent-emitted blocker request carried in a handoff.
///
/// The kernel converts each request into a durable [`crate::Blocker`] row
/// after policy checks. The *blocked* work item is implied by the run that
/// emitted the handoff; agents only declare *what* is blocking and *why*.
/// `blocking_id` is optional for the case where the blocking work has not
/// been filed yet — the kernel routes such requests through follow-up
/// issue creation per workflow policy.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct HandoffBlockerRequest {
    /// Work item that causes the block, when known.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub blocking_id: Option<WorkItemId>,
    /// Operator-facing reason. Required and non-empty.
    pub reason: String,
    /// Severity hint. Defaults to [`BlockerSeverity::Medium`].
    #[serde(default)]
    pub severity: BlockerSeverity,
}

/// Agent-emitted follow-up issue request carried in a handoff.
///
/// Light-weight intent the kernel resolves into either a tracker issue or
/// an entry in the follow-up approval queue. The richer
/// `FollowupIssueRequest` durable type (Phase 10) supersedes this when the
/// kernel persists the resolved row; this struct stays minimal so the
/// agent's output schema remains stable across policy changes.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct HandoffFollowupRequest {
    /// Short title for the follow-up.
    pub title: String,
    /// Free-form summary of the discovered work.
    pub summary: String,
    /// True if this follow-up gates the current item's acceptance.
    #[serde(default)]
    pub blocking: bool,
    /// True if the agent is *proposing* (kernel routes to approval queue)
    /// rather than asking for direct creation.
    #[serde(default)]
    pub propose_only: bool,
}

/// Agent-emitted QA verdict request carried in a handoff (SPEC v2 §4.9).
///
/// QA-gate runs emit this so the kernel can persist a durable
/// [`crate::QaOutcome`] without re-deriving evidence from free-form
/// text. The struct is the *request* shape: the kernel assigns the
/// durable [`crate::QaVerdictId`] and links the verdict to the run on
/// persistence. The work item is implied by the run that emitted the
/// handoff and is not re-stated here, mirroring
/// [`HandoffBlockerRequest`].
///
/// The verdict-vs-evidence invariants are enforced once at construction
/// via [`Self::try_new`], using the same rules as
/// [`crate::QaOutcome::try_new`] so request and durable record cannot
/// drift out of sync. Skip the constructor only in tests that
/// deliberately exercise validation — direct construction works through
/// public fields, but [`Handoff::validate`] re-runs the same checks.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct HandoffVerdictRequest {
    /// Typed verdict outcome.
    pub verdict: QaVerdict,
    /// Structured evidence the verdict relied on. Required categories
    /// are policed by `qa.evidence_required` at the kernel seam; this
    /// type only carries the slots.
    #[serde(default)]
    pub evidence: QaEvidence,
    /// One trace row per acceptance criterion the work item declared.
    /// May be empty when the work item has no acceptance criteria.
    #[serde(default)]
    pub acceptance_trace: Vec<AcceptanceCriterionTrace>,
    /// Blocker ids the verdict references. Required for
    /// [`QaVerdict::FailedWithBlockers`]; forbidden for other variants.
    /// The blockers themselves are typically filed via
    /// [`Handoff::blockers_created`] in the same handoff and resolved
    /// to ids by the kernel.
    #[serde(default)]
    pub blockers_created: Vec<BlockerId>,
    /// Role that authored a [`QaVerdict::Waived`] verdict. Required for
    /// `waived`, forbidden otherwise. Membership in `qa.waiver_roles`
    /// is checked at the kernel seam.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub waiver_role: Option<RoleName>,
    /// Operator-facing reason. Required for `waived` and for any
    /// non-passing verdict so the durable record explains itself.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
}

impl HandoffVerdictRequest {
    /// Construct a verdict request, enforcing the same verdict shape
    /// invariants [`crate::QaOutcome::try_new`] applies to the durable
    /// record.
    pub fn try_new(
        verdict: QaVerdict,
        evidence: QaEvidence,
        acceptance_trace: Vec<AcceptanceCriterionTrace>,
        blockers_created: Vec<BlockerId>,
        waiver_role: Option<RoleName>,
        reason: Option<String>,
    ) -> Result<Self, HandoffError> {
        let request = Self {
            verdict,
            evidence,
            acceptance_trace,
            blockers_created,
            waiver_role,
            reason,
        };
        request.validate_shape()?;
        Ok(request)
    }

    /// Validate the verdict-vs-evidence invariants. Called by
    /// [`Handoff::validate`] and [`Self::try_new`].
    pub fn validate_shape(&self) -> Result<(), HandoffError> {
        for trace in &self.acceptance_trace {
            if trace.criterion.trim().is_empty() {
                return Err(HandoffError::EmptyVerdictCriterion);
            }
        }
        match self.verdict {
            QaVerdict::FailedWithBlockers => {
                if self.blockers_created.is_empty() {
                    return Err(HandoffError::VerdictMissingBlockers);
                }
            }
            QaVerdict::Passed => {
                if !self.blockers_created.is_empty() {
                    return Err(HandoffError::VerdictUnexpectedBlockers {
                        verdict: self.verdict,
                        count: self.blockers_created.len(),
                    });
                }
                if self.acceptance_trace.iter().any(|t| !t.status.is_passing()) {
                    return Err(HandoffError::VerdictPassWithUnpassedCriterion);
                }
            }
            QaVerdict::FailedNeedsRework | QaVerdict::Inconclusive | QaVerdict::Waived => {
                if !self.blockers_created.is_empty() {
                    return Err(HandoffError::VerdictUnexpectedBlockers {
                        verdict: self.verdict,
                        count: self.blockers_created.len(),
                    });
                }
            }
        }
        let has_reason = self.reason.as_deref().is_some_and(|r| !r.trim().is_empty());
        if self.verdict.requires_waiver() {
            if self.waiver_role.is_none() || !has_reason {
                return Err(HandoffError::VerdictInvalidWaiver);
            }
        } else if self.waiver_role.is_some() {
            return Err(HandoffError::VerdictUnexpectedWaiver(self.verdict));
        }
        Ok(())
    }
}

/// Validation errors raised by [`Handoff::validate`] before persistence.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum HandoffError {
    /// `summary` is empty or whitespace-only. SPEC §4.7 lists summary as
    /// required.
    #[error("handoff summary must not be empty")]
    EmptySummary,
    /// `ready_for: blocked` without any [`HandoffBlockerRequest`] or
    /// `block_reason`. SPEC §4.7 — `blocked` requires evidence.
    #[error("ready_for=blocked requires at least one blocker or a block_reason")]
    BlockedWithoutEvidence,
    /// `block_reason` was provided but `ready_for` is not `blocked`.
    /// Prevents accidental block reasons silently dropped by the kernel.
    #[error("block_reason is only valid when ready_for=blocked (got {0})")]
    NonBlockedBlockReason(ReadyFor),
    /// A `changed_files` entry is empty. Empty paths break workspace
    /// verification and downstream diffing.
    #[error("handoff changed_files entry must not be empty")]
    EmptyChangedFile,
    /// A [`HandoffBlockerRequest::reason`] is empty. Mirrors the
    /// non-empty-reason invariant on durable [`crate::Blocker`] rows.
    #[error("handoff blocker request reason must not be empty")]
    EmptyBlockerReason,
    /// A [`HandoffFollowupRequest::title`] is empty.
    #[error("handoff followup request title must not be empty")]
    EmptyFollowupTitle,
    /// `branch_or_workspace` is missing both a branch and a workspace path.
    #[error("handoff branch_or_workspace must record at least one of branch or workspace_path")]
    EmptyBranchOrWorkspace,
    /// A verdict request's acceptance trace had an empty criterion.
    #[error("verdict_request acceptance criterion must not be empty")]
    EmptyVerdictCriterion,
    /// A `failed_with_blockers` verdict request had no blocker ids.
    #[error("verdict_request failed_with_blockers requires at least one blocker reference")]
    VerdictMissingBlockers,
    /// A non-`failed_with_blockers` verdict request carried blocker ids.
    #[error("verdict_request {verdict} must not carry blockers_created (got {count})")]
    VerdictUnexpectedBlockers {
        /// Verdict that was attached.
        verdict: QaVerdict,
        /// How many blocker ids were attached.
        count: usize,
    },
    /// A `waived` verdict request was missing waiver role or reason.
    #[error("verdict_request waived requires both a waiver role and a non-empty reason")]
    VerdictInvalidWaiver,
    /// A non-`waived` verdict request carried waiver metadata.
    #[error("verdict_request {0} must not carry waiver metadata")]
    VerdictUnexpectedWaiver(QaVerdict),
    /// A `passed` verdict request included a non-passing acceptance row.
    #[error(
        "verdict_request passed requires every acceptance criterion to be verified or not_applicable"
    )]
    VerdictPassWithUnpassedCriterion,
}

/// Structured run output emitted at end-of-run (SPEC v2 §4.7).
///
/// All required SPEC §4.7 fields are non-optional. `block_reason` and
/// `reporting_role` are kernel-useful extras: `block_reason` carries
/// `ready_for: blocked` evidence when no concrete blockers were filed,
/// and `reporting_role` lets the kernel route handoffs without re-fetching
/// the run row when the dispatcher already knows the role.
///
/// Construct directly (the struct is plain data) and call
/// [`Self::validate`] before persistence — the kernel rejects malformed
/// handoffs per SPEC §7 (failed run / repair turn).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Handoff {
    /// Operator-facing summary. Required and non-empty.
    pub summary: String,
    /// Files the run modified, relative to the workspace root.
    #[serde(default)]
    pub changed_files: Vec<String>,
    /// Test commands the run executed (or attempted).
    #[serde(default)]
    pub tests_run: Vec<String>,
    /// Verification evidence (logs, screenshots, terminal recordings, …).
    #[serde(default)]
    pub verification_evidence: Vec<String>,
    /// Known risks the agent surfaced.
    #[serde(default)]
    pub known_risks: Vec<String>,
    /// Blocker requests the kernel should durably file (subject to policy).
    #[serde(default)]
    pub blockers_created: Vec<HandoffBlockerRequest>,
    /// Follow-up issue requests the kernel should create or queue for
    /// approval (subject to policy).
    #[serde(default)]
    pub followups_created_or_proposed: Vec<HandoffFollowupRequest>,
    /// Where the run's output lives.
    pub branch_or_workspace: BranchOrWorkspace,
    /// Advisory next-gate hint.
    pub ready_for: ReadyFor,
    /// Structured block reason for `ready_for: blocked` runs that did not
    /// file concrete blockers (e.g. external dependency).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub block_reason: Option<String>,
    /// Reporting role. Optional; the dispatcher usually knows it already.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reporting_role: Option<RoleName>,
    /// QA verdict request, populated when a `qa_gate` run emits a
    /// verdict alongside its handoff. The kernel resolves this into a
    /// durable [`crate::QaOutcome`] after policy checks.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub verdict_request: Option<HandoffVerdictRequest>,
}

impl Handoff {
    /// Validate the handoff shape. Run before persisting or routing.
    ///
    /// This enforces SPEC §4.7 invariants the kernel branches on. It does
    /// *not* enforce gating (e.g. `done` requiring no blockers) — those
    /// are policy decisions handled by later phases against durable
    /// state.
    pub fn validate(&self) -> Result<(), HandoffError> {
        if self.summary.trim().is_empty() {
            return Err(HandoffError::EmptySummary);
        }
        if self.branch_or_workspace.is_empty() {
            return Err(HandoffError::EmptyBranchOrWorkspace);
        }
        for path in &self.changed_files {
            if path.trim().is_empty() {
                return Err(HandoffError::EmptyChangedFile);
            }
        }
        for blocker in &self.blockers_created {
            if blocker.reason.trim().is_empty() {
                return Err(HandoffError::EmptyBlockerReason);
            }
        }
        for followup in &self.followups_created_or_proposed {
            if followup.title.trim().is_empty() {
                return Err(HandoffError::EmptyFollowupTitle);
            }
        }
        let has_block_reason = self
            .block_reason
            .as_deref()
            .is_some_and(|r| !r.trim().is_empty());
        match self.ready_for {
            ReadyFor::Blocked => {
                if self.blockers_created.is_empty() && !has_block_reason {
                    return Err(HandoffError::BlockedWithoutEvidence);
                }
            }
            other => {
                if has_block_reason {
                    return Err(HandoffError::NonBlockedBlockReason(other));
                }
            }
        }
        if let Some(request) = &self.verdict_request {
            request.validate_shape()?;
        }
        Ok(())
    }

    /// Convenience accessor for the kernel-side queue consequence of
    /// [`Self::ready_for`]. See [`ReadyFor::consequence`].
    pub fn consequence(&self) -> ReadyForConsequence {
        self.ready_for.consequence()
    }

    /// True when [`Self::ready_for`] is one of the gate-deferring variants
    /// (`integration`, `qa`, `human_review`).
    pub fn defers_to_gate(&self) -> bool {
        matches!(
            self.ready_for,
            ReadyFor::Integration | ReadyFor::Qa | ReadyFor::HumanReview
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn minimal_branch() -> BranchOrWorkspace {
        BranchOrWorkspace {
            branch: Some("feat/foo".into()),
            workspace_path: None,
            base_ref: Some("main".into()),
        }
    }

    fn minimal_handoff() -> Handoff {
        Handoff {
            summary: "did the thing".into(),
            changed_files: vec!["src/foo.rs".into()],
            tests_run: vec!["cargo test --workspace".into()],
            verification_evidence: vec![],
            known_risks: vec![],
            blockers_created: vec![],
            followups_created_or_proposed: vec![],
            branch_or_workspace: minimal_branch(),
            ready_for: ReadyFor::Qa,
            block_reason: None,
            reporting_role: Some(RoleName::new("backend")),
            verdict_request: None,
        }
    }

    fn passing_trace() -> AcceptanceCriterionTrace {
        AcceptanceCriterionTrace {
            criterion: "tests pass".into(),
            status: crate::qa::AcceptanceCriterionStatus::Verified,
            evidence: vec!["cargo test --workspace".into()],
            notes: None,
        }
    }

    #[test]
    fn ready_for_round_trips_through_serde_for_every_variant() {
        for r in ReadyFor::ALL {
            let json = serde_json::to_string(&r).unwrap();
            assert_eq!(json, format!("\"{}\"", r.as_str()));
            let back: ReadyFor = serde_json::from_str(&json).unwrap();
            assert_eq!(back, r);
        }
    }

    #[test]
    fn ready_for_block_evidence_is_only_required_for_blocked() {
        for r in ReadyFor::ALL {
            assert_eq!(r.requires_block_evidence(), matches!(r, ReadyFor::Blocked));
        }
    }

    #[test]
    fn branch_or_workspace_is_empty_only_when_both_branch_and_path_are_missing() {
        assert!(BranchOrWorkspace::default().is_empty());
        assert!(
            !BranchOrWorkspace {
                branch: Some("x".into()),
                ..Default::default()
            }
            .is_empty()
        );
        assert!(
            !BranchOrWorkspace {
                workspace_path: Some("/tmp/wt".into()),
                ..Default::default()
            }
            .is_empty()
        );
        assert!(
            BranchOrWorkspace {
                branch: Some("".into()),
                workspace_path: Some("".into()),
                ..Default::default()
            }
            .is_empty()
        );
    }

    #[test]
    fn validate_accepts_minimal_handoff() {
        minimal_handoff().validate().unwrap();
    }

    #[test]
    fn validate_rejects_empty_summary() {
        let mut h = minimal_handoff();
        for s in ["", "   ", "\t\n"] {
            h.summary = s.into();
            assert_eq!(h.validate().unwrap_err(), HandoffError::EmptySummary);
        }
    }

    #[test]
    fn validate_rejects_empty_branch_or_workspace() {
        let mut h = minimal_handoff();
        h.branch_or_workspace = BranchOrWorkspace::default();
        assert_eq!(
            h.validate().unwrap_err(),
            HandoffError::EmptyBranchOrWorkspace
        );
    }

    #[test]
    fn validate_rejects_empty_changed_file_entry() {
        let mut h = minimal_handoff();
        h.changed_files.push("   ".into());
        assert_eq!(h.validate().unwrap_err(), HandoffError::EmptyChangedFile);
    }

    #[test]
    fn validate_rejects_blocker_request_with_empty_reason() {
        let mut h = minimal_handoff();
        h.blockers_created.push(HandoffBlockerRequest {
            blocking_id: Some(WorkItemId::new(7)),
            reason: "  ".into(),
            severity: BlockerSeverity::Medium,
        });
        assert_eq!(h.validate().unwrap_err(), HandoffError::EmptyBlockerReason);
    }

    #[test]
    fn validate_rejects_followup_with_empty_title() {
        let mut h = minimal_handoff();
        h.followups_created_or_proposed
            .push(HandoffFollowupRequest {
                title: "".into(),
                summary: "x".into(),
                blocking: false,
                propose_only: true,
            });
        assert_eq!(h.validate().unwrap_err(), HandoffError::EmptyFollowupTitle);
    }

    #[test]
    fn validate_rejects_blocked_without_evidence() {
        let mut h = minimal_handoff();
        h.ready_for = ReadyFor::Blocked;
        assert_eq!(
            h.validate().unwrap_err(),
            HandoffError::BlockedWithoutEvidence
        );
    }

    #[test]
    fn validate_accepts_blocked_with_block_reason() {
        let mut h = minimal_handoff();
        h.ready_for = ReadyFor::Blocked;
        h.block_reason = Some("waiting on vendor".into());
        h.validate().unwrap();
    }

    #[test]
    fn validate_accepts_blocked_with_blocker_request() {
        let mut h = minimal_handoff();
        h.ready_for = ReadyFor::Blocked;
        h.blockers_created.push(HandoffBlockerRequest {
            blocking_id: Some(WorkItemId::new(11)),
            reason: "needs schema sign-off".into(),
            severity: BlockerSeverity::High,
        });
        h.validate().unwrap();
    }

    #[test]
    fn validate_rejects_block_reason_when_ready_for_is_not_blocked() {
        let mut h = minimal_handoff();
        h.ready_for = ReadyFor::Qa;
        h.block_reason = Some("stuck".into());
        assert_eq!(
            h.validate().unwrap_err(),
            HandoffError::NonBlockedBlockReason(ReadyFor::Qa)
        );
    }

    #[test]
    fn validate_treats_whitespace_block_reason_as_missing() {
        let mut h = minimal_handoff();
        h.ready_for = ReadyFor::Blocked;
        h.block_reason = Some("   ".into());
        assert_eq!(
            h.validate().unwrap_err(),
            HandoffError::BlockedWithoutEvidence
        );
    }

    #[test]
    fn defers_to_gate_only_for_intermediate_ready_for_variants() {
        for r in ReadyFor::ALL {
            let mut h = minimal_handoff();
            h.ready_for = r;
            if r == ReadyFor::Blocked {
                h.block_reason = Some("waiting".into());
            }
            assert_eq!(
                h.defers_to_gate(),
                matches!(
                    r,
                    ReadyFor::Integration | ReadyFor::Qa | ReadyFor::HumanReview
                )
            );
        }
    }

    #[test]
    fn handoff_round_trips_through_json_with_full_payload() {
        let h = Handoff {
            summary: "integrated children A and B".into(),
            changed_files: vec!["src/a.rs".into(), "src/b.rs".into()],
            tests_run: vec!["cargo test -p foo".into()],
            verification_evidence: vec!["logs/run-7.txt".into()],
            known_risks: vec!["touches migration order".into()],
            blockers_created: vec![HandoffBlockerRequest {
                blocking_id: Some(WorkItemId::new(42)),
                reason: "missing migration".into(),
                severity: BlockerSeverity::High,
            }],
            followups_created_or_proposed: vec![HandoffFollowupRequest {
                title: "rate-limit cleanup".into(),
                summary: "tighten retries on 429".into(),
                blocking: false,
                propose_only: true,
            }],
            branch_or_workspace: BranchOrWorkspace {
                branch: Some("integration/parent-7".into()),
                workspace_path: Some("/tmp/wt/parent-7".into()),
                base_ref: Some("main".into()),
            },
            ready_for: ReadyFor::Qa,
            block_reason: None,
            reporting_role: Some(RoleName::new("platform_lead")),
            verdict_request: None,
        };
        h.validate().unwrap();
        let json = serde_json::to_string(&h).unwrap();
        let back: Handoff = serde_json::from_str(&json).unwrap();
        assert_eq!(h, back);
    }

    #[test]
    fn ready_for_consequence_is_total_and_distinct() {
        let consequences: Vec<_> = ReadyFor::ALL.iter().map(|r| r.consequence()).collect();
        assert_eq!(
            consequences,
            vec![
                ReadyForConsequence::EnqueueIntegration,
                ReadyForConsequence::EnqueueQa,
                ReadyForConsequence::PauseAwaitingHumanReview,
                ReadyForConsequence::HoldBlocked,
                ReadyForConsequence::AttemptDone,
            ]
        );
    }

    #[test]
    fn ready_for_consequence_round_trips_through_serde() {
        for c in ReadyForConsequence::ALL {
            let json = serde_json::to_string(&c).unwrap();
            assert_eq!(json, format!("\"{}\"", c.as_str()));
            let back: ReadyForConsequence = serde_json::from_str(&json).unwrap();
            assert_eq!(back, c);
        }
    }

    #[test]
    fn handoff_consequence_delegates_to_ready_for() {
        let mut h = minimal_handoff();
        for r in ReadyFor::ALL {
            h.ready_for = r;
            h.block_reason = if r == ReadyFor::Blocked {
                Some("waiting".into())
            } else {
                None
            };
            assert_eq!(h.consequence(), r.consequence());
        }
    }

    #[test]
    fn verdict_request_passed_constructs_with_passing_trace() {
        let req = HandoffVerdictRequest::try_new(
            QaVerdict::Passed,
            QaEvidence::default(),
            vec![passing_trace()],
            vec![],
            None,
            None,
        )
        .unwrap();
        assert_eq!(req.verdict, QaVerdict::Passed);
    }

    #[test]
    fn verdict_request_failed_with_blockers_requires_ids() {
        let err = HandoffVerdictRequest::try_new(
            QaVerdict::FailedWithBlockers,
            QaEvidence::default(),
            vec![],
            vec![],
            None,
            Some("tests fail".into()),
        )
        .unwrap_err();
        assert_eq!(err, HandoffError::VerdictMissingBlockers);

        HandoffVerdictRequest::try_new(
            QaVerdict::FailedWithBlockers,
            QaEvidence::default(),
            vec![],
            vec![BlockerId::new(7)],
            None,
            Some("tests fail".into()),
        )
        .unwrap();
    }

    #[test]
    fn verdict_request_non_failed_rejects_blocker_ids() {
        for verdict in [
            QaVerdict::Passed,
            QaVerdict::FailedNeedsRework,
            QaVerdict::Inconclusive,
            QaVerdict::Waived,
        ] {
            let waiver = (verdict == QaVerdict::Waived).then(|| RoleName::new("platform_lead"));
            let reason = (verdict == QaVerdict::Waived).then(|| "ok".to_string());
            let err = HandoffVerdictRequest::try_new(
                verdict,
                QaEvidence::default(),
                if verdict == QaVerdict::Passed {
                    vec![passing_trace()]
                } else {
                    vec![]
                },
                vec![BlockerId::new(1)],
                waiver,
                reason,
            )
            .unwrap_err();
            assert_eq!(
                err,
                HandoffError::VerdictUnexpectedBlockers { verdict, count: 1 }
            );
        }
    }

    #[test]
    fn verdict_request_waived_requires_role_and_reason() {
        let err = HandoffVerdictRequest::try_new(
            QaVerdict::Waived,
            QaEvidence::default(),
            vec![],
            vec![],
            None,
            Some("ok".into()),
        )
        .unwrap_err();
        assert_eq!(err, HandoffError::VerdictInvalidWaiver);

        HandoffVerdictRequest::try_new(
            QaVerdict::Waived,
            QaEvidence::default(),
            vec![],
            vec![],
            Some(RoleName::new("platform_lead")),
            Some("accepted risk".into()),
        )
        .unwrap();
    }

    #[test]
    fn verdict_request_passed_rejects_unpassed_criterion() {
        let trace = AcceptanceCriterionTrace {
            criterion: "ui renders".into(),
            status: crate::qa::AcceptanceCriterionStatus::Failed,
            evidence: vec![],
            notes: None,
        };
        let err = HandoffVerdictRequest::try_new(
            QaVerdict::Passed,
            QaEvidence::default(),
            vec![passing_trace(), trace],
            vec![],
            None,
            None,
        )
        .unwrap_err();
        assert_eq!(err, HandoffError::VerdictPassWithUnpassedCriterion);
    }

    #[test]
    fn verdict_request_rejects_empty_criterion_text() {
        let trace = AcceptanceCriterionTrace {
            criterion: "  ".into(),
            status: crate::qa::AcceptanceCriterionStatus::Verified,
            evidence: vec![],
            notes: None,
        };
        let err = HandoffVerdictRequest::try_new(
            QaVerdict::Passed,
            QaEvidence::default(),
            vec![trace],
            vec![],
            None,
            None,
        )
        .unwrap_err();
        assert_eq!(err, HandoffError::EmptyVerdictCriterion);
    }

    #[test]
    fn handoff_validate_runs_verdict_request_invariants() {
        let mut h = minimal_handoff();
        h.verdict_request = Some(HandoffVerdictRequest {
            verdict: QaVerdict::FailedWithBlockers,
            evidence: QaEvidence::default(),
            acceptance_trace: vec![],
            blockers_created: vec![],
            waiver_role: None,
            reason: Some("tests fail".into()),
        });
        assert_eq!(
            h.validate().unwrap_err(),
            HandoffError::VerdictMissingBlockers
        );
    }

    #[test]
    fn handoff_with_verdict_request_round_trips_through_json() {
        let mut h = minimal_handoff();
        h.verdict_request = Some(
            HandoffVerdictRequest::try_new(
                QaVerdict::Passed,
                QaEvidence {
                    tests_run: vec!["cargo test".into()],
                    ..QaEvidence::default()
                },
                vec![passing_trace()],
                vec![],
                None,
                None,
            )
            .unwrap(),
        );
        h.validate().unwrap();
        let json = serde_json::to_string(&h).unwrap();
        let back: Handoff = serde_json::from_str(&json).unwrap();
        assert_eq!(h, back);
    }

    #[test]
    fn handoff_deserializes_with_optional_fields_omitted() {
        let json = r#"{
            "summary": "small fix",
            "branch_or_workspace": { "branch": "fix/x" },
            "ready_for": "done"
        }"#;
        let h: Handoff = serde_json::from_str(json).unwrap();
        assert_eq!(h.summary, "small fix");
        assert_eq!(h.ready_for, ReadyFor::Done);
        assert!(h.changed_files.is_empty());
        assert!(h.blockers_created.is_empty());
        assert!(h.reporting_role.is_none());
        assert!(h.verdict_request.is_none());
        h.validate().unwrap();
    }
}
