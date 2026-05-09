//! QA-gate run request (SPEC v2 §6.5, §5.12, ARCH §10).
//!
//! When the QA queue (see `symphony_state::QaQueueRepository`) surfaces a
//! work item that needs verification, the kernel builds a [`QaRunRequest`]
//! and hands it to the QA-gate agent runner. The request bundles every
//! input the QA role needs to author a verdict deterministically:
//!
//! * the work item identity (id, tracker key, title);
//! * the role under which the QA run executes;
//! * the canonical integration [`QaWorkspace`] claim — path, branch, base
//!   ref, strategy — so the runner can verify cwd/branch before launching
//!   the agent (SPEC v2 §8.3);
//! * an optional [`QaDraftPullRequest`] projection of the draft PR record
//!   (SPEC v2 §5.11) when the workflow opens PRs;
//! * the optional [`crate::IntegrationId`] of the consolidation the QA run
//!   gates;
//! * the operator-facing [`QaRunRequest::acceptance_criteria`] text the
//!   QA role must trace one-for-one in
//!   [`crate::QaOutcome::acceptance_trace`];
//! * the [`QaRunRequest::changed_files`] surface QA must review (SPEC §6.5
//!   step 3);
//! * an optional [`QaRunRequest::ci_status`] rollup captured by the
//!   integration owner (SPEC v2 §5.12 `evidence_required.tests`);
//! * the [`QaRunRequest::prior_handoffs`] from contributing runs (specialist +
//!   integration owner) so QA can read intent and risk without re-querying
//!   state (SPEC v2 §6.5 — "QA reviews tests and changed files");
//! * the [`QaRequestCause`] surfaced by the queue.
//!
//! Like [`crate::IntegrationRunRequest`], this is the *pure domain* shape:
//! no I/O, no agent invocation, no tracker mutation. Construct via
//! [`QaRunRequest::try_new`] so the cross-field invariants are enforced
//! once at creation time.
//!
//! ## Invariants enforced at construction
//!
//! 1. `qa_role.as_str()` is non-empty.
//! 2. `identifier` and `title` are non-empty.
//! 3. `workspace.path` is non-empty and `workspace.strategy` is non-empty.
//! 4. Every entry in `acceptance_criteria` is non-empty after trim.
//! 5. Every entry in `changed_files` is non-empty after trim.
//! 6. `ci_status`, when `Some`, is non-empty after trim.
//! 7. Every embedded [`Handoff`] in `prior_handoffs` passes
//!    [`Handoff::validate`].
//! 8. When `draft_pr` is `Some`, `head_branch` is non-empty (mirrors the
//!    SPEC §5.11 invariant on [`crate::PullRequestRecord`]).
//! 9. When `cause = IntegrationConsolidated`, `integration_id` MUST be
//!    `Some`. The integration record is the load-bearing signal for that
//!    cause; QA cannot brief without it.

use std::path::PathBuf;

use serde::{Deserialize, Serialize};

use crate::handoff::{Handoff, HandoffError};
use crate::integration::IntegrationId;
use crate::pull_request::{PullRequestProvider, PullRequestState};
use crate::role::RoleName;
use crate::work_item::WorkItemId;

/// Why the QA-gate role is being invoked for this work item.
///
/// Mirrors `symphony_state::QaQueueCause` on the wire (same `as_str()`
/// values) so events and tests can correlate the queue reason with the
/// request without depending on the state crate from core.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum QaRequestCause {
    /// The work item itself is in `status_class = qa` — typically because
    /// a specialist or integration-owner handoff signalled `ready_for: qa`
    /// and the kernel promoted it.
    DirectQaRequest,
    /// A successful integration record exists for the work item and no QA
    /// verdict has been filed since. QA verifies the consolidated branch.
    IntegrationConsolidated,
}

impl QaRequestCause {
    /// All variants in declaration order.
    pub const ALL: [Self; 2] = [Self::DirectQaRequest, Self::IntegrationConsolidated];

    /// Stable lowercase identifier used in event payloads.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::DirectQaRequest => "direct_qa_request",
            Self::IntegrationConsolidated => "integration_consolidated",
        }
    }
}

impl std::fmt::Display for QaRequestCause {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

/// Slim mirror of `symphony_workspace::WorkspaceClaim` for the QA run
/// request.
///
/// QA inspects the canonical integration workspace the owner produced, so
/// the projected fields match [`crate::IntegrationWorkspace`]. The two
/// types are kept independent to avoid coupling the QA flow to the
/// integration-request module.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct QaWorkspace {
    /// Absolute path to the workspace QA must operate against.
    pub path: PathBuf,
    /// Strategy in force (e.g. `git_worktree`, `existing_worktree`,
    /// `shared_branch`). Recorded so the verdict's evidence can capture
    /// *how* the workspace was set up.
    pub strategy: String,
    /// Branch the run must be on at agent-launch time, when applicable.
    /// `None` for strategies without a branch (e.g. plain directory).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub branch: Option<String>,
    /// Base ref the integration branch was cut from (e.g. `main`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub base_ref: Option<String>,
}

/// Slim projection of a [`crate::PullRequestRecord`] for the QA run
/// request.
///
/// Carries only the fields QA needs to fetch and verify the PR. The full
/// record stays in `symphony-state`; the projection is what crosses the
/// agent boundary.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct QaDraftPullRequest {
    /// Forge adapter that owns the PR.
    pub provider: PullRequestProvider,
    /// Lifecycle state at request-build time. Typically
    /// [`PullRequestState::Draft`] when QA is invoked; the kernel records
    /// the actual state so QA can reject a stale request.
    pub state: PullRequestState,
    /// Forge-assigned PR number, when the forge has accepted the open call.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub number: Option<i64>,
    /// Canonical PR URL on the forge, when known.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub url: Option<String>,
    /// Head ref the PR proposes to merge — the canonical integration
    /// branch. Required and non-empty so QA always has a branch to fetch.
    pub head_branch: String,
    /// Base ref the PR targets (e.g. `main`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub base_branch: Option<String>,
    /// Head SHA at record time, when known.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub head_sha: Option<String>,
}

/// Invariant violations for [`QaRunRequest::try_new`].
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum QaRequestError {
    /// The QA role's name was empty/whitespace.
    #[error("qa_role must not be empty")]
    EmptyQaRole,
    /// The work item's tracker identifier was empty/whitespace.
    #[error("identifier must not be empty")]
    EmptyIdentifier,
    /// The work item's title was empty/whitespace.
    #[error("title must not be empty")]
    EmptyTitle,
    /// The workspace path was empty.
    #[error("workspace.path must not be empty")]
    EmptyWorkspacePath,
    /// The workspace strategy label was empty/whitespace.
    #[error("workspace.strategy must not be empty")]
    EmptyWorkspaceStrategy,
    /// An entry in `acceptance_criteria` was empty/whitespace.
    #[error("acceptance criterion at index {0} is empty")]
    EmptyAcceptanceCriterion(usize),
    /// An entry in `changed_files` was empty/whitespace.
    #[error("changed_files entry at index {0} is empty")]
    EmptyChangedFile(usize),
    /// `ci_status` was supplied but blank.
    #[error("ci_status, when set, must not be blank")]
    EmptyCiStatus,
    /// The draft PR projection had an empty `head_branch`.
    #[error("draft_pr.head_branch must not be empty")]
    EmptyDraftPrHeadBranch,
    /// `cause: integration_consolidated` was supplied without an
    /// `integration_id`. The integration record is the load-bearing
    /// signal for that cause.
    #[error("cause `integration_consolidated` requires an integration_id")]
    MissingIntegrationIdForConsolidatedCause,
    /// An embedded prior handoff failed [`Handoff::validate`].
    #[error("prior handoff at index {index} is malformed: {source}")]
    InvalidPriorHandoff {
        /// Index into `prior_handoffs` of the offending row.
        index: usize,
        /// Underlying handoff validation error.
        #[source]
        source: HandoffError,
    },
}

/// QA-gate run request (SPEC v2 §6.5, ARCH §10).
///
/// Construct via [`Self::try_new`] so the cross-field invariants are
/// checked once. The struct is plain serializable data so the dispatcher
/// can persist it for replay and diagnostics.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct QaRunRequest {
    /// Work item the QA run gates.
    pub work_item_id: WorkItemId,
    /// Tracker-facing identifier for the work item (e.g. `ENG-42`).
    pub identifier: String,
    /// Work item title.
    pub title: String,
    /// Role the run executes under. Typically the workflow's `qa_gate`
    /// role.
    pub qa_role: RoleName,
    /// Canonical integration workspace claim summary QA must operate
    /// against.
    pub workspace: QaWorkspace,
    /// Integration record this QA run gates, when one exists. Required
    /// when `cause = IntegrationConsolidated`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub integration_id: Option<IntegrationId>,
    /// Draft PR projection, when the workflow opens PRs (SPEC v2 §5.11).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub draft_pr: Option<QaDraftPullRequest>,
    /// Operator-facing acceptance criteria QA must trace, in declaration
    /// order. Each entry feeds one [`crate::AcceptanceCriterionTrace`]
    /// row in the resulting verdict.
    #[serde(default)]
    pub acceptance_criteria: Vec<String>,
    /// Files in the integration surface QA must review (SPEC v2 §6.5
    /// step 3). Recorded so QA evidence can prove every changed file was
    /// examined.
    #[serde(default)]
    pub changed_files: Vec<String>,
    /// CI/check rollup string captured by the integration owner. Stored
    /// opaquely; QA consumes it as evidence per SPEC v2 §5.12
    /// `evidence_required.tests`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ci_status: Option<String>,
    /// Handoffs from contributing runs (typically the integration owner
    /// plus child specialists). Order is preserved so QA can read intent
    /// and risk without re-querying state.
    #[serde(default)]
    pub prior_handoffs: Vec<Handoff>,
    /// Why the queue surfaced this work item.
    pub cause: QaRequestCause,
}

impl QaRunRequest {
    /// Construct a request, enforcing every invariant listed in the
    /// module docs. Returns the offending [`QaRequestError`] on the first
    /// violation.
    #[allow(clippy::too_many_arguments)]
    pub fn try_new(
        work_item_id: WorkItemId,
        identifier: impl Into<String>,
        title: impl Into<String>,
        qa_role: RoleName,
        workspace: QaWorkspace,
        integration_id: Option<IntegrationId>,
        draft_pr: Option<QaDraftPullRequest>,
        acceptance_criteria: Vec<String>,
        changed_files: Vec<String>,
        ci_status: Option<String>,
        prior_handoffs: Vec<Handoff>,
        cause: QaRequestCause,
    ) -> Result<Self, QaRequestError> {
        if qa_role.as_str().trim().is_empty() {
            return Err(QaRequestError::EmptyQaRole);
        }
        let identifier = identifier.into();
        let title = title.into();
        if identifier.trim().is_empty() {
            return Err(QaRequestError::EmptyIdentifier);
        }
        if title.trim().is_empty() {
            return Err(QaRequestError::EmptyTitle);
        }
        if workspace.path.as_os_str().is_empty() {
            return Err(QaRequestError::EmptyWorkspacePath);
        }
        if workspace.strategy.trim().is_empty() {
            return Err(QaRequestError::EmptyWorkspaceStrategy);
        }
        for (idx, criterion) in acceptance_criteria.iter().enumerate() {
            if criterion.trim().is_empty() {
                return Err(QaRequestError::EmptyAcceptanceCriterion(idx));
            }
        }
        for (idx, file) in changed_files.iter().enumerate() {
            if file.trim().is_empty() {
                return Err(QaRequestError::EmptyChangedFile(idx));
            }
        }
        if let Some(status) = ci_status.as_deref()
            && status.trim().is_empty()
        {
            return Err(QaRequestError::EmptyCiStatus);
        }
        if let Some(pr) = &draft_pr
            && pr.head_branch.trim().is_empty()
        {
            return Err(QaRequestError::EmptyDraftPrHeadBranch);
        }
        if cause == QaRequestCause::IntegrationConsolidated && integration_id.is_none() {
            return Err(QaRequestError::MissingIntegrationIdForConsolidatedCause);
        }
        for (index, handoff) in prior_handoffs.iter().enumerate() {
            handoff
                .validate()
                .map_err(|source| QaRequestError::InvalidPriorHandoff { index, source })?;
        }

        Ok(Self {
            work_item_id,
            identifier,
            title,
            qa_role,
            workspace,
            integration_id,
            draft_pr,
            acceptance_criteria,
            changed_files,
            ci_status,
            prior_handoffs,
            cause,
        })
    }

    /// True when at least one prior handoff is attached.
    pub fn has_prior_handoffs(&self) -> bool {
        !self.prior_handoffs.is_empty()
    }

    /// True when a draft PR projection is attached.
    pub fn has_draft_pr(&self) -> bool {
        self.draft_pr.is_some()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::handoff::{BranchOrWorkspace, Handoff, ReadyFor};

    fn role() -> RoleName {
        RoleName::new("qa")
    }

    fn workspace() -> QaWorkspace {
        QaWorkspace {
            path: PathBuf::from("/tmp/proj-integration"),
            strategy: "git_worktree".into(),
            branch: Some("symphony/integration/PROJ-42".into()),
            base_ref: Some("main".into()),
        }
    }

    fn draft_pr() -> QaDraftPullRequest {
        QaDraftPullRequest {
            provider: PullRequestProvider::Github,
            state: PullRequestState::Draft,
            number: Some(123),
            url: Some("https://github.com/example/repo/pull/123".into()),
            head_branch: "symphony/integration/PROJ-42".into(),
            base_branch: Some("main".into()),
            head_sha: Some("deadbeef".into()),
        }
    }

    fn valid_handoff() -> Handoff {
        Handoff {
            summary: "consolidated children".into(),
            changed_files: vec!["src/widget.rs".into()],
            tests_run: vec!["cargo test --workspace".into()],
            verification_evidence: vec![],
            known_risks: vec![],
            blockers_created: vec![],
            followups_created_or_proposed: vec![],
            branch_or_workspace: BranchOrWorkspace {
                branch: Some("symphony/integration/PROJ-42".into()),
                workspace_path: Some("/tmp/proj-integration".into()),
                base_ref: Some("main".into()),
            },
            ready_for: ReadyFor::Qa,
            block_reason: None,
            reporting_role: None,
            verdict_request: None,
        }
    }

    fn ok_request() -> QaRunRequest {
        QaRunRequest::try_new(
            WorkItemId::new(1),
            "PROJ-42",
            "consolidate widget work",
            role(),
            workspace(),
            Some(IntegrationId::new(7)),
            Some(draft_pr()),
            vec!["tests pass".into(), "ui renders".into()],
            vec!["src/widget.rs".into(), "src/lib.rs".into()],
            Some("ci: success".into()),
            vec![valid_handoff()],
            QaRequestCause::IntegrationConsolidated,
        )
        .expect("valid request should construct")
    }

    #[test]
    fn cause_round_trips_via_as_str() {
        for cause in QaRequestCause::ALL {
            let s = cause.as_str();
            let json = serde_json::to_string(&cause).unwrap();
            assert_eq!(json, format!("\"{s}\""));
            let back: QaRequestCause = serde_json::from_str(&json).unwrap();
            assert_eq!(back, cause);
        }
    }

    #[test]
    fn valid_request_round_trips_through_json() {
        let req = ok_request();
        let json = serde_json::to_string(&req).unwrap();
        let back: QaRunRequest = serde_json::from_str(&json).unwrap();
        assert_eq!(back, req);
        assert!(req.has_prior_handoffs());
        assert!(req.has_draft_pr());
    }

    #[test]
    fn empty_qa_role_is_rejected() {
        let err = QaRunRequest::try_new(
            WorkItemId::new(1),
            "PROJ-42",
            "title",
            RoleName::new("   "),
            workspace(),
            None,
            None,
            vec![],
            vec![],
            None,
            vec![],
            QaRequestCause::DirectQaRequest,
        )
        .unwrap_err();
        assert_eq!(err, QaRequestError::EmptyQaRole);
    }

    #[test]
    fn empty_identifier_or_title_is_rejected() {
        let err = QaRunRequest::try_new(
            WorkItemId::new(1),
            "  ",
            "title",
            role(),
            workspace(),
            None,
            None,
            vec![],
            vec![],
            None,
            vec![],
            QaRequestCause::DirectQaRequest,
        )
        .unwrap_err();
        assert_eq!(err, QaRequestError::EmptyIdentifier);

        let err = QaRunRequest::try_new(
            WorkItemId::new(1),
            "PROJ-42",
            "",
            role(),
            workspace(),
            None,
            None,
            vec![],
            vec![],
            None,
            vec![],
            QaRequestCause::DirectQaRequest,
        )
        .unwrap_err();
        assert_eq!(err, QaRequestError::EmptyTitle);
    }

    #[test]
    fn empty_workspace_path_or_strategy_is_rejected() {
        let mut ws = workspace();
        ws.path = PathBuf::new();
        let err = QaRunRequest::try_new(
            WorkItemId::new(1),
            "PROJ-42",
            "title",
            role(),
            ws,
            None,
            None,
            vec![],
            vec![],
            None,
            vec![],
            QaRequestCause::DirectQaRequest,
        )
        .unwrap_err();
        assert_eq!(err, QaRequestError::EmptyWorkspacePath);

        let mut ws = workspace();
        ws.strategy = "  ".into();
        let err = QaRunRequest::try_new(
            WorkItemId::new(1),
            "PROJ-42",
            "title",
            role(),
            ws,
            None,
            None,
            vec![],
            vec![],
            None,
            vec![],
            QaRequestCause::DirectQaRequest,
        )
        .unwrap_err();
        assert_eq!(err, QaRequestError::EmptyWorkspaceStrategy);
    }

    #[test]
    fn empty_acceptance_criterion_is_rejected_with_index() {
        let err = QaRunRequest::try_new(
            WorkItemId::new(1),
            "PROJ-42",
            "title",
            role(),
            workspace(),
            None,
            None,
            vec!["tests pass".into(), "   ".into()],
            vec![],
            None,
            vec![],
            QaRequestCause::DirectQaRequest,
        )
        .unwrap_err();
        assert_eq!(err, QaRequestError::EmptyAcceptanceCriterion(1));
    }

    #[test]
    fn empty_changed_file_is_rejected_with_index() {
        let err = QaRunRequest::try_new(
            WorkItemId::new(1),
            "PROJ-42",
            "title",
            role(),
            workspace(),
            None,
            None,
            vec![],
            vec!["src/widget.rs".into(), "".into()],
            None,
            vec![],
            QaRequestCause::DirectQaRequest,
        )
        .unwrap_err();
        assert_eq!(err, QaRequestError::EmptyChangedFile(1));
    }

    #[test]
    fn blank_ci_status_is_rejected() {
        let err = QaRunRequest::try_new(
            WorkItemId::new(1),
            "PROJ-42",
            "title",
            role(),
            workspace(),
            None,
            None,
            vec![],
            vec![],
            Some("   ".into()),
            vec![],
            QaRequestCause::DirectQaRequest,
        )
        .unwrap_err();
        assert_eq!(err, QaRequestError::EmptyCiStatus);
    }

    #[test]
    fn empty_draft_pr_head_branch_is_rejected() {
        let mut pr = draft_pr();
        pr.head_branch = "  ".into();
        let err = QaRunRequest::try_new(
            WorkItemId::new(1),
            "PROJ-42",
            "title",
            role(),
            workspace(),
            Some(IntegrationId::new(7)),
            Some(pr),
            vec![],
            vec![],
            None,
            vec![],
            QaRequestCause::IntegrationConsolidated,
        )
        .unwrap_err();
        assert_eq!(err, QaRequestError::EmptyDraftPrHeadBranch);
    }

    #[test]
    fn integration_consolidated_cause_requires_integration_id() {
        let err = QaRunRequest::try_new(
            WorkItemId::new(1),
            "PROJ-42",
            "title",
            role(),
            workspace(),
            None,
            None,
            vec![],
            vec![],
            None,
            vec![],
            QaRequestCause::IntegrationConsolidated,
        )
        .unwrap_err();
        assert_eq!(
            err,
            QaRequestError::MissingIntegrationIdForConsolidatedCause
        );
    }

    #[test]
    fn direct_qa_request_allows_no_integration_or_pr() {
        let req = QaRunRequest::try_new(
            WorkItemId::new(1),
            "PROJ-42",
            "title",
            role(),
            workspace(),
            None,
            None,
            vec!["tests pass".into()],
            vec!["src/lib.rs".into()],
            None,
            vec![],
            QaRequestCause::DirectQaRequest,
        )
        .expect("direct qa request should not require integration or PR");
        assert_eq!(req.cause, QaRequestCause::DirectQaRequest);
        assert!(!req.has_draft_pr());
        assert!(!req.has_prior_handoffs());
    }

    #[test]
    fn malformed_prior_handoff_is_rejected_with_index() {
        let mut bad = valid_handoff();
        bad.summary.clear();
        let err = QaRunRequest::try_new(
            WorkItemId::new(1),
            "PROJ-42",
            "title",
            role(),
            workspace(),
            Some(IntegrationId::new(7)),
            Some(draft_pr()),
            vec![],
            vec![],
            None,
            vec![valid_handoff(), bad],
            QaRequestCause::IntegrationConsolidated,
        )
        .unwrap_err();
        match err {
            QaRequestError::InvalidPriorHandoff { index, .. } => assert_eq!(index, 1),
            other => panic!("expected InvalidPriorHandoff, got {other:?}"),
        }
    }

    #[test]
    fn prior_handoffs_preserve_order() {
        let mut h1 = valid_handoff();
        h1.summary = "first".into();
        let mut h2 = valid_handoff();
        h2.summary = "second".into();
        let req = QaRunRequest::try_new(
            WorkItemId::new(1),
            "PROJ-42",
            "title",
            role(),
            workspace(),
            Some(IntegrationId::new(7)),
            Some(draft_pr()),
            vec!["tests pass".into()],
            vec!["src/lib.rs".into()],
            Some("ci: success".into()),
            vec![h1, h2],
            QaRequestCause::IntegrationConsolidated,
        )
        .unwrap();
        let summaries: Vec<&str> = req
            .prior_handoffs
            .iter()
            .map(|h| h.summary.as_str())
            .collect();
        assert_eq!(summaries, vec!["first", "second"]);
    }
}
