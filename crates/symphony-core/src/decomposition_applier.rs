//! Tracker-side application of an approved [`DecompositionProposal`]
//! (SPEC v2 §5.7 "child issue creation").
//!
//! [`crate::decomposition_runner`] owns the *pure* eligibility/acceptance
//! seam — it produces a [`DecompositionProposal`] but never touches a
//! tracker. This module owns the *I/O* seam that takes an approved
//! proposal and drives `TrackerMutations::create_issue` per child.
//!
//! Gating by `decomposition.child_issue_policy` is enforced indirectly
//! via the proposal's lifecycle:
//!
//! * [`crate::FollowupPolicy::CreateDirectly`] — the proposal is born
//!   [`DecompositionStatus::Approved`]; the applier accepts it.
//! * [`crate::FollowupPolicy::ProposeForApproval`] — the proposal is
//!   born [`DecompositionStatus::Proposed`] and the applier refuses
//!   until [`DecompositionProposal::approve`] has run.
//!
//! The applier is intentionally narrow:
//!
//! * It does not allocate kernel [`crate::WorkItemId`]s — that belongs
//!   to `symphony-state` once the persistence row lands. The caller maps
//!   the returned tracker [`IssueId`]s onto fresh `WorkItemId`s and
//!   feeds them to [`DecompositionProposal::mark_applied`].
//! * It does not perform structural parent/child linking on adapters
//!   that lack [`TrackerCapabilities::link_parent_child`]. The
//!   `parent` field on [`CreateIssueRequest`] carries that information
//!   to whichever adapter is wired in; per-adapter advisory rendering
//!   (e.g. the GitHub `> Parent: #N` footer) is the adapter's
//!   responsibility, not the applier's.
//! * It surfaces partial failures explicitly — a `ChildCreationFailed`
//!   error includes the [`ChildKey`] that failed and the underlying
//!   [`crate::TrackerError`], leaving the surrounding workflow free to
//!   record the partial state, retry, or roll back per policy.

use async_trait::async_trait;

use crate::decomposition::{ChildKey, DecompositionId, DecompositionProposal, DecompositionStatus};
use crate::tracker::IssueId;
use crate::tracker_trait::{
    CreateIssueRequest, LinkParentChildRequest, TrackerCapabilities, TrackerError, TrackerMutations,
};

/// Tracker-side outcome of one child issue creation.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AppliedChild {
    /// Proposal-local identifier of the source [`crate::ChildProposal`].
    pub key: ChildKey,
    /// Tracker-internal identifier returned by the adapter.
    pub tracker_id: IssueId,
    /// Human-readable identifier (`#42`, `ABC-7`).
    pub identifier: String,
    /// Tracker URL when the adapter could synthesize one.
    pub url: Option<String>,
    /// `true` when the applier issued a follow-up
    /// [`TrackerMutations::link_parent_child`] call after creation
    /// (capability-gated). `false` when parent linking rode atomically
    /// on [`CreateIssueRequest::parent`] or was skipped because the
    /// adapter cannot express structural edges.
    pub linked_parent: bool,
}

/// Successful application of a [`DecompositionProposal`].
///
/// The orchestrator persists `work_items` rows for each entry and then
/// calls [`DecompositionProposal::mark_applied`] with the resulting
/// `ChildKey -> WorkItemId` map.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AppliedDecomposition {
    /// Proposal that was applied. Echoed for caller convenience so the
    /// resulting record can be persisted without re-borrowing the
    /// proposal.
    pub proposal_id: DecompositionId,
    /// Per-child creation results in proposal declaration order.
    pub children: Vec<AppliedChild>,
}

/// Errors raised by [`apply_decomposition`].
#[derive(Debug, thiserror::Error)]
pub enum ApplyError {
    /// Caller invoked the applier on a proposal whose status is not
    /// [`DecompositionStatus::Approved`]. Mirrors SPEC §5.7's two-step
    /// gating: `propose_for_approval` proposals must clear approval
    /// before any tracker mutation lands.
    #[error("decomposition {id} cannot be applied from status {current}")]
    NotApproved {
        /// Proposal id (echoed for log correlation).
        id: DecompositionId,
        /// Current status — typically
        /// [`DecompositionStatus::Proposed`] or a terminal status.
        current: DecompositionStatus,
    },
    /// The wired tracker reported [`TrackerCapabilities::create_issue`]
    /// as `false`. Workflows that decompose require a mutation-capable
    /// adapter; the applier refuses rather than silently turning the
    /// call into an advisory no-op.
    #[error("tracker adapter does not support create_issue")]
    CreateIssueUnsupported,
    /// `create_issue` failed mid-iteration. Carries the failing child's
    /// key and the underlying tracker error so the caller can decide
    /// between retry, rollback, or recording the partial state on the
    /// proposal.
    #[error("creating child {key} failed: {source}")]
    ChildCreationFailed {
        /// Proposal-local identifier of the failing child.
        key: ChildKey,
        /// Underlying tracker error.
        #[source]
        source: TrackerError,
    },
    /// Follow-up [`TrackerMutations::link_parent_child`] failed after
    /// the child issue had already been created. The child exists on
    /// the tracker; the link does not.
    #[error("linking child {key} ({tracker_id}) to parent failed: {source}")]
    ParentLinkFailed {
        /// Proposal-local identifier of the child that was created.
        key: ChildKey,
        /// Tracker id of the orphaned child.
        tracker_id: IssueId,
        /// Underlying tracker error.
        #[source]
        source: TrackerError,
    },
}

/// Apply an approved [`DecompositionProposal`] by creating each child
/// issue through the wired [`TrackerMutations`] adapter.
///
/// `parent_tracker_id` is the parent issue's tracker-side
/// [`IssueId`] (e.g. `42` for GitHub, `ABC-1` for Linear). The applier
/// passes it as [`CreateIssueRequest::parent`] so adapters that support
/// structural linking can record the edge atomically; for adapters that
/// expose [`TrackerCapabilities::link_parent_child`] separately, the
/// applier issues the follow-up link call and reports
/// [`AppliedChild::linked_parent`] accordingly.
///
/// The proposal is *not* mutated. Callers are expected to:
///
/// 1. Persist tracker results into `work_items` rows (allocating
///    [`crate::WorkItemId`]s as side effects of insertion).
/// 2. Build a `HashMap<ChildKey, WorkItemId>` from the
///    [`AppliedDecomposition::children`] entries.
/// 3. Invoke [`DecompositionProposal::mark_applied`] with that map
///    inside the same `symphony-state` transaction.
pub async fn apply_decomposition(
    proposal: &DecompositionProposal,
    parent_tracker_id: &IssueId,
    tracker: &dyn TrackerMutations,
    capabilities: TrackerCapabilities,
) -> Result<AppliedDecomposition, ApplyError> {
    if proposal.status != DecompositionStatus::Approved {
        return Err(ApplyError::NotApproved {
            id: proposal.id,
            current: proposal.status,
        });
    }
    if !capabilities.create_issue {
        return Err(ApplyError::CreateIssueUnsupported);
    }

    let mut applied = Vec::with_capacity(proposal.children.len());
    for child in &proposal.children {
        let body = render_child_body(child);
        let request = CreateIssueRequest {
            title: child.title.clone(),
            body,
            labels: Vec::new(),
            assignees: Vec::new(),
            parent: Some(parent_tracker_id.clone()),
            initial_state: None,
        };
        let response = tracker.create_issue(request).await.map_err(|source| {
            ApplyError::ChildCreationFailed {
                key: child.key.clone(),
                source,
            }
        })?;

        let mut linked_parent = false;
        if capabilities.link_parent_child {
            tracker
                .link_parent_child(LinkParentChildRequest {
                    parent: parent_tracker_id.clone(),
                    child: response.id.clone(),
                })
                .await
                .map_err(|source| ApplyError::ParentLinkFailed {
                    key: child.key.clone(),
                    tracker_id: response.id.clone(),
                    source,
                })?;
            linked_parent = true;
        }

        applied.push(AppliedChild {
            key: child.key.clone(),
            tracker_id: response.id,
            identifier: response.identifier,
            url: response.url,
            linked_parent,
        });
    }

    Ok(AppliedDecomposition {
        proposal_id: proposal.id,
        children: applied,
    })
}

/// Render a child's tracker body from its scope, description, and
/// acceptance criteria. Returns `None` when every source field is empty
/// so the adapter falls back to its native default body handling.
fn render_child_body(child: &crate::ChildProposal) -> Option<String> {
    let mut buf = String::new();
    if let Some(desc) = &child.description {
        let trimmed = desc.trim();
        if !trimmed.is_empty() {
            buf.push_str(trimmed);
            buf.push_str("\n\n");
        }
    }
    if !child.scope.trim().is_empty() {
        buf.push_str("**Scope:** ");
        buf.push_str(child.scope.trim());
        buf.push('\n');
    }
    if !child.acceptance_criteria.is_empty() {
        if !buf.is_empty() {
            buf.push('\n');
        }
        buf.push_str("**Acceptance Criteria:**\n");
        for criterion in &child.acceptance_criteria {
            buf.push_str("- ");
            buf.push_str(criterion);
            buf.push('\n');
        }
    }
    if buf.is_empty() { None } else { Some(buf) }
}

/// Trait alias used by tests and by code that prefers a typed handle to
/// the applier rather than re-exporting the free function. Implementations
/// other than the canonical [`apply_decomposition`] are not expected.
#[async_trait]
pub trait DecompositionApplier: Send + Sync {
    /// Apply the proposal. See [`apply_decomposition`].
    async fn apply(
        &self,
        proposal: &DecompositionProposal,
        parent_tracker_id: &IssueId,
        tracker: &dyn TrackerMutations,
        capabilities: TrackerCapabilities,
    ) -> Result<AppliedDecomposition, ApplyError>;
}

/// Canonical [`DecompositionApplier`] that delegates to
/// [`apply_decomposition`]. Held as a unit struct so it can be wired
/// behind a `Arc<dyn DecompositionApplier>` when the orchestrator wants
/// to swap in a fake for tests.
#[derive(Debug, Clone, Copy, Default)]
pub struct DefaultDecompositionApplier;

#[async_trait]
impl DecompositionApplier for DefaultDecompositionApplier {
    async fn apply(
        &self,
        proposal: &DecompositionProposal,
        parent_tracker_id: &IssueId,
        tracker: &dyn TrackerMutations,
        capabilities: TrackerCapabilities,
    ) -> Result<AppliedDecomposition, ApplyError> {
        apply_decomposition(proposal, parent_tracker_id, tracker, capabilities).await
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::decomposition::{ChildProposal, DecompositionProposal};
    use crate::followup::FollowupPolicy;
    use crate::role::RoleName;
    use crate::tracker_trait::{
        AddBlockerRequest, AddBlockerResponse, AddCommentRequest, AddCommentResponse,
        AttachArtifactRequest, AttachArtifactResponse, CreateIssueResponse,
        LinkParentChildResponse, TrackerError, TrackerResult, UpdateIssueRequest,
        UpdateIssueResponse,
    };
    use crate::work_item::WorkItemId;
    use std::collections::BTreeSet;
    use std::sync::Mutex;

    fn child(key: &str, deps: &[&str]) -> ChildProposal {
        ChildProposal::try_new(
            key,
            format!("title {key}"),
            format!("scope {key}"),
            RoleName::new("backend"),
            None,
            vec![format!("acceptance for {key}")],
            deps.iter()
                .map(|d| ChildKey::new(*d))
                .collect::<BTreeSet<_>>(),
            true,
            None,
            true,
        )
        .unwrap()
    }

    fn proposal(policy: FollowupPolicy) -> DecompositionProposal {
        DecompositionProposal::try_new(
            DecompositionId::new(1),
            WorkItemId::new(100),
            RoleName::new("platform_lead"),
            "split",
            vec![child("a", &[]), child("b", &["a"])],
            policy,
        )
        .unwrap()
    }

    /// Recording mutation tracker. Captures every request and returns
    /// canned successful responses unless the test explicitly rigs a
    /// failure for a specific child key.
    struct RecordingTracker {
        next_id: Mutex<u64>,
        calls: Mutex<Vec<TrackerCall>>,
        fail_create_for: Option<String>,
        fail_link_for: Option<String>,
    }

    #[derive(Debug, Clone, PartialEq, Eq)]
    enum TrackerCall {
        Create(CreateIssueRequest),
        Link(LinkParentChildRequest),
    }

    impl RecordingTracker {
        fn new() -> Self {
            Self {
                next_id: Mutex::new(1000),
                calls: Mutex::new(Vec::new()),
                fail_create_for: None,
                fail_link_for: None,
            }
        }

        fn fail_create(mut self, title_marker: impl Into<String>) -> Self {
            self.fail_create_for = Some(title_marker.into());
            self
        }

        fn fail_link(mut self, title_marker: impl Into<String>) -> Self {
            self.fail_link_for = Some(title_marker.into());
            self
        }

        fn calls(&self) -> Vec<TrackerCall> {
            self.calls.lock().unwrap().clone()
        }
    }

    #[async_trait]
    impl TrackerMutations for RecordingTracker {
        async fn create_issue(
            &self,
            request: CreateIssueRequest,
        ) -> TrackerResult<CreateIssueResponse> {
            if let Some(marker) = &self.fail_create_for
                && request.title.contains(marker)
            {
                return Err(TrackerError::Transport("create boom".into()));
            }
            let mut next = self.next_id.lock().unwrap();
            let id = *next;
            *next += 1;
            self.calls
                .lock()
                .unwrap()
                .push(TrackerCall::Create(request));
            Ok(CreateIssueResponse {
                id: IssueId::new(id.to_string()),
                identifier: format!("#{id}"),
                url: Some(format!("https://example/{id}")),
            })
        }

        async fn update_issue(
            &self,
            _request: UpdateIssueRequest,
        ) -> TrackerResult<UpdateIssueResponse> {
            unreachable!("applier never calls update_issue")
        }

        async fn add_comment(
            &self,
            _request: AddCommentRequest,
        ) -> TrackerResult<AddCommentResponse> {
            unreachable!("applier never calls add_comment")
        }

        async fn add_blocker(
            &self,
            _request: AddBlockerRequest,
        ) -> TrackerResult<AddBlockerResponse> {
            unreachable!("applier never calls add_blocker")
        }

        async fn link_parent_child(
            &self,
            request: LinkParentChildRequest,
        ) -> TrackerResult<LinkParentChildResponse> {
            if let Some(marker) = &self.fail_link_for
                && request.child.as_str().contains(marker)
            {
                return Err(TrackerError::Transport("link boom".into()));
            }
            self.calls.lock().unwrap().push(TrackerCall::Link(request));
            Ok(LinkParentChildResponse { edge_id: None })
        }

        async fn attach_artifact(
            &self,
            _request: AttachArtifactRequest,
        ) -> TrackerResult<AttachArtifactResponse> {
            unreachable!("applier never calls attach_artifact")
        }
    }

    #[tokio::test]
    async fn rejects_proposed_status() {
        let proposal = proposal(FollowupPolicy::ProposeForApproval);
        assert_eq!(proposal.status, DecompositionStatus::Proposed);
        let tracker = RecordingTracker::new();
        let err = apply_decomposition(
            &proposal,
            &IssueId::new("100"),
            &tracker,
            TrackerCapabilities::FULL,
        )
        .await
        .unwrap_err();
        assert!(matches!(
            err,
            ApplyError::NotApproved {
                current: DecompositionStatus::Proposed,
                ..
            }
        ));
        assert!(
            tracker.calls().is_empty(),
            "no tracker calls before approval"
        );
    }

    #[tokio::test]
    async fn rejects_when_create_issue_capability_missing() {
        let proposal = proposal(FollowupPolicy::CreateDirectly);
        let tracker = RecordingTracker::new();
        let err = apply_decomposition(
            &proposal,
            &IssueId::new("100"),
            &tracker,
            TrackerCapabilities::READ_ONLY,
        )
        .await
        .unwrap_err();
        assert!(matches!(err, ApplyError::CreateIssueUnsupported));
        assert!(tracker.calls().is_empty());
    }

    #[tokio::test]
    async fn create_directly_proposal_applies_each_child_in_order() {
        let proposal = proposal(FollowupPolicy::CreateDirectly);
        assert_eq!(proposal.status, DecompositionStatus::Approved);
        let tracker = RecordingTracker::new();
        let result = apply_decomposition(
            &proposal,
            &IssueId::new("100"),
            &tracker,
            TrackerCapabilities::FULL,
        )
        .await
        .unwrap();
        assert_eq!(result.proposal_id, DecompositionId::new(1));
        assert_eq!(result.children.len(), 2);
        assert_eq!(result.children[0].key.as_str(), "a");
        assert_eq!(result.children[1].key.as_str(), "b");
        assert!(result.children.iter().all(|c| c.linked_parent));
        // 2 creates + 2 links, interleaved create→link per child.
        let calls = tracker.calls();
        assert_eq!(calls.len(), 4);
        assert!(matches!(calls[0], TrackerCall::Create(_)));
        assert!(matches!(calls[1], TrackerCall::Link(_)));
        assert!(matches!(calls[2], TrackerCall::Create(_)));
        assert!(matches!(calls[3], TrackerCall::Link(_)));
    }

    #[tokio::test]
    async fn approved_after_propose_for_approval_applies() {
        let mut proposal = proposal(FollowupPolicy::ProposeForApproval);
        proposal
            .approve(RoleName::new("approver"), "ship it")
            .unwrap();
        let tracker = RecordingTracker::new();
        let result = apply_decomposition(
            &proposal,
            &IssueId::new("ABC-100"),
            &tracker,
            TrackerCapabilities::FULL,
        )
        .await
        .unwrap();
        assert_eq!(result.children.len(), 2);
    }

    #[tokio::test]
    async fn skips_link_step_when_capability_absent() {
        let proposal = proposal(FollowupPolicy::CreateDirectly);
        let tracker = RecordingTracker::new();
        let caps = TrackerCapabilities {
            create_issue: true,
            link_parent_child: false,
            ..TrackerCapabilities::READ_ONLY
        };
        let result = apply_decomposition(&proposal, &IssueId::new("100"), &tracker, caps)
            .await
            .unwrap();
        assert!(result.children.iter().all(|c| !c.linked_parent));
        assert!(
            tracker
                .calls()
                .iter()
                .all(|c| matches!(c, TrackerCall::Create(_))),
            "no link calls issued when capability is off"
        );
    }

    #[tokio::test]
    async fn forwards_parent_id_on_create_request() {
        let proposal = proposal(FollowupPolicy::CreateDirectly);
        let tracker = RecordingTracker::new();
        let _ = apply_decomposition(
            &proposal,
            &IssueId::new("PARENT-7"),
            &tracker,
            TrackerCapabilities::FULL,
        )
        .await
        .unwrap();
        for call in tracker.calls() {
            if let TrackerCall::Create(req) = call {
                assert_eq!(req.parent.as_ref().unwrap().as_str(), "PARENT-7");
            }
        }
    }

    #[tokio::test]
    async fn renders_body_with_scope_and_acceptance_criteria() {
        let proposal = proposal(FollowupPolicy::CreateDirectly);
        let tracker = RecordingTracker::new();
        let _ = apply_decomposition(
            &proposal,
            &IssueId::new("100"),
            &tracker,
            TrackerCapabilities::FULL,
        )
        .await
        .unwrap();
        let body = match &tracker.calls()[0] {
            TrackerCall::Create(req) => req.body.clone().expect("body rendered"),
            _ => panic!("first call should be create"),
        };
        assert!(body.contains("**Scope:** scope a"));
        assert!(body.contains("**Acceptance Criteria:**"));
        assert!(body.contains("- acceptance for a"));
    }

    #[tokio::test]
    async fn create_failure_aborts_remaining_children() {
        let proposal = proposal(FollowupPolicy::CreateDirectly);
        let tracker = RecordingTracker::new().fail_create("title b");
        let err = apply_decomposition(
            &proposal,
            &IssueId::new("100"),
            &tracker,
            TrackerCapabilities::FULL,
        )
        .await
        .unwrap_err();
        match err {
            ApplyError::ChildCreationFailed { key, .. } => assert_eq!(key.as_str(), "b"),
            other => panic!("expected ChildCreationFailed, got {other:?}"),
        }
        // 'a' was created + linked, then 'b' failed on create.
        let calls = tracker.calls();
        assert_eq!(calls.len(), 2);
        assert!(matches!(calls[0], TrackerCall::Create(_)));
        assert!(matches!(calls[1], TrackerCall::Link(_)));
    }

    #[tokio::test]
    async fn link_failure_surfaces_orphaned_child_id() {
        let proposal = proposal(FollowupPolicy::CreateDirectly);
        // RecordingTracker assigns sequential ids starting at 1000, so the
        // first child becomes "1000".
        let tracker = RecordingTracker::new().fail_link("1000");
        let err = apply_decomposition(
            &proposal,
            &IssueId::new("100"),
            &tracker,
            TrackerCapabilities::FULL,
        )
        .await
        .unwrap_err();
        match err {
            ApplyError::ParentLinkFailed {
                key, tracker_id, ..
            } => {
                assert_eq!(key.as_str(), "a");
                assert_eq!(tracker_id.as_str(), "1000");
            }
            other => panic!("expected ParentLinkFailed, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn default_applier_delegates_to_free_function() {
        let proposal = proposal(FollowupPolicy::CreateDirectly);
        let tracker = RecordingTracker::new();
        let applier = DefaultDecompositionApplier;
        let result = applier
            .apply(
                &proposal,
                &IssueId::new("100"),
                &tracker,
                TrackerCapabilities::FULL,
            )
            .await
            .unwrap();
        assert_eq!(result.children.len(), 2);
    }
}
