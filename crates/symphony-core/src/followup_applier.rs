//! Tracker-side application of an approved [`crate::FollowupIssueRequest`]
//! (SPEC v2 §5.13 "create follow-up issue").
//!
//! Like [`crate::decomposition_applier`], this module owns the narrow
//! I/O seam from a validated durable kernel record to
//! [`crate::TrackerMutations::create_issue`]. Persistence of the
//! resulting tracker identifiers remains a separate concern in
//! `symphony-state`; this module intentionally focuses on the tracker
//! mutation itself so scheduler/state wiring can target one canonical
//! helper.

use async_trait::async_trait;

use crate::followup::{FollowupId, FollowupIssueRequest, FollowupStatus};
use crate::tracker::IssueState;
use crate::tracker_trait::{
    CreateIssueRequest, TrackerCapabilities, TrackerError, TrackerMutations,
};

/// Tracker-side outcome of filing one approved follow-up issue.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AppliedFollowup {
    /// Durable follow-up row that was filed.
    pub followup_id: FollowupId,
    /// Tracker-internal identifier returned by the adapter.
    pub tracker_id: crate::tracker::IssueId,
    /// Human-readable identifier (`#42`, `ENG-7`, ...).
    pub identifier: String,
    /// Tracker URL when available.
    pub url: Option<String>,
}

/// Errors raised by [`apply_followup`].
#[derive(Debug, thiserror::Error)]
pub enum ApplyFollowupError {
    /// Only approved follow-ups may be filed into the tracker.
    #[error("followup {id} cannot be applied from status {current}")]
    NotApproved {
        /// Follow-up id.
        id: FollowupId,
        /// Current lifecycle status.
        current: FollowupStatus,
    },
    /// The wired tracker cannot create issues.
    #[error("tracker adapter does not support create_issue")]
    CreateIssueUnsupported,
    /// The tracker write failed.
    #[error("creating followup {id} failed: {source}")]
    CreateIssueFailed {
        /// Follow-up id that failed to file.
        id: FollowupId,
        /// Underlying tracker error.
        #[source]
        source: TrackerError,
    },
}

/// File an approved follow-up into the tracker.
///
/// `initial_state` lets callers keep newly created follow-ups dormant by
/// default when the workflow config provides a tracker state outside the
/// intake-active set.
pub async fn apply_followup(
    followup: &FollowupIssueRequest,
    tracker: &dyn TrackerMutations,
    capabilities: TrackerCapabilities,
    initial_state: Option<IssueState>,
    labels: Vec<String>,
) -> Result<AppliedFollowup, ApplyFollowupError> {
    if followup.status != FollowupStatus::Approved {
        return Err(ApplyFollowupError::NotApproved {
            id: followup.id,
            current: followup.status,
        });
    }
    if !capabilities.create_issue {
        return Err(ApplyFollowupError::CreateIssueUnsupported);
    }

    let request = CreateIssueRequest {
        title: followup.title.clone(),
        body: render_followup_body(followup),
        labels,
        assignees: Vec::new(),
        parent: None,
        initial_state,
    };

    let response = tracker
        .create_issue(request)
        .await
        .map_err(|source| ApplyFollowupError::CreateIssueFailed {
            id: followup.id,
            source,
        })?;

    Ok(AppliedFollowup {
        followup_id: followup.id,
        tracker_id: response.id,
        identifier: response.identifier,
        url: response.url,
    })
}

fn render_followup_body(followup: &FollowupIssueRequest) -> Option<String> {
    let mut buf = String::new();

    let reason = followup.reason.trim();
    if !reason.is_empty() {
        buf.push_str(reason);
        buf.push_str("\n\n");
    }

    let scope = followup.scope.trim();
    if !scope.is_empty() {
        buf.push_str("**Scope:** ");
        buf.push_str(scope);
        buf.push('\n');
    }

    if !followup.acceptance_criteria.is_empty() {
        if !buf.is_empty() {
            buf.push('\n');
        }
        buf.push_str("**Acceptance Criteria:**\n");
        for criterion in &followup.acceptance_criteria {
            buf.push_str("- ");
            buf.push_str(criterion);
            buf.push('\n');
        }
    }

    if buf.is_empty() { None } else { Some(buf) }
}

/// Typed handle mirroring [`crate::DecompositionApplier`].
#[async_trait]
pub trait FollowupApplier: Send + Sync {
    /// Apply the follow-up. See [`apply_followup`].
    async fn apply(
        &self,
        followup: &FollowupIssueRequest,
        tracker: &dyn TrackerMutations,
        capabilities: TrackerCapabilities,
        initial_state: Option<IssueState>,
        labels: Vec<String>,
    ) -> Result<AppliedFollowup, ApplyFollowupError>;
}

/// Canonical [`FollowupApplier`] implementation delegating to
/// [`apply_followup`].
#[derive(Debug, Clone, Copy, Default)]
pub struct DefaultFollowupApplier;

#[async_trait]
impl FollowupApplier for DefaultFollowupApplier {
    async fn apply(
        &self,
        followup: &FollowupIssueRequest,
        tracker: &dyn TrackerMutations,
        capabilities: TrackerCapabilities,
        initial_state: Option<IssueState>,
        labels: Vec<String>,
    ) -> Result<AppliedFollowup, ApplyFollowupError> {
        apply_followup(followup, tracker, capabilities, initial_state, labels).await
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::blocker::{BlockerOrigin, RunRef};
    use crate::followup::FollowupPolicy;
    use crate::role::RoleName;
    use crate::tracker::IssueId;
    use crate::tracker_trait::{
        AddBlockerRequest, AddBlockerResponse, AddCommentRequest, AddCommentResponse,
        AttachArtifactRequest, AttachArtifactResponse, CreateIssueResponse,
        LinkParentChildRequest, LinkParentChildResponse, UpdateIssueRequest, UpdateIssueResponse,
    };
    use crate::work_item::WorkItemId;
    use std::sync::Mutex;

    fn followup(policy: FollowupPolicy, blocking: bool) -> FollowupIssueRequest {
        FollowupIssueRequest::try_new(
            FollowupId::new(41),
            WorkItemId::new(7),
            "Retry jitter histogram",
            "Useful observability follow-up outside the current acceptance criteria.",
            "Telemetry-only extension",
            vec!["status output shows histogram buckets".into()],
            blocking,
            policy,
            BlockerOrigin::Run {
                run_id: RunRef::new(8),
                role: Some(RoleName::new("backend")),
            },
            true,
        )
        .unwrap()
    }

    #[derive(Debug, Clone, PartialEq, Eq)]
    enum TrackerCall {
        Create(CreateIssueRequest),
    }

    struct RecordingTracker {
        calls: Mutex<Vec<TrackerCall>>,
        fail_create: bool,
    }

    impl RecordingTracker {
        fn new() -> Self {
            Self {
                calls: Mutex::new(Vec::new()),
                fail_create: false,
            }
        }

        fn failing() -> Self {
            Self {
                calls: Mutex::new(Vec::new()),
                fail_create: true,
            }
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
        ) -> crate::tracker_trait::TrackerResult<CreateIssueResponse> {
            if self.fail_create {
                return Err(TrackerError::Transport("create boom".into()));
            }
            self.calls.lock().unwrap().push(TrackerCall::Create(request));
            Ok(CreateIssueResponse {
                id: IssueId::new("123"),
                identifier: "ENG-123".into(),
                url: Some("https://example.test/ENG-123".into()),
            })
        }

        async fn update_issue(
            &self,
            _request: UpdateIssueRequest,
        ) -> crate::tracker_trait::TrackerResult<UpdateIssueResponse> {
            unreachable!("followup applier never calls update_issue")
        }

        async fn add_comment(
            &self,
            _request: AddCommentRequest,
        ) -> crate::tracker_trait::TrackerResult<AddCommentResponse> {
            unreachable!("followup applier never calls add_comment")
        }

        async fn add_blocker(
            &self,
            _request: AddBlockerRequest,
        ) -> crate::tracker_trait::TrackerResult<AddBlockerResponse> {
            unreachable!("followup applier never calls add_blocker")
        }

        async fn link_parent_child(
            &self,
            _request: LinkParentChildRequest,
        ) -> crate::tracker_trait::TrackerResult<LinkParentChildResponse> {
            unreachable!("followup applier never calls link_parent_child")
        }

        async fn attach_artifact(
            &self,
            _request: AttachArtifactRequest,
        ) -> crate::tracker_trait::TrackerResult<AttachArtifactResponse> {
            unreachable!("followup applier never calls attach_artifact")
        }
    }

    #[tokio::test]
    async fn rejects_proposed_followup() {
        let followup = followup(FollowupPolicy::ProposeForApproval, false);
        let tracker = RecordingTracker::new();

        let err = apply_followup(
            &followup,
            &tracker,
            TrackerCapabilities::FULL,
            None,
            Vec::new(),
        )
        .await
        .unwrap_err();

        assert!(matches!(
            err,
            ApplyFollowupError::NotApproved {
                current: FollowupStatus::Proposed,
                ..
            }
        ));
        assert!(tracker.calls().is_empty());
    }

    #[tokio::test]
    async fn rejects_when_create_issue_capability_missing() {
        let followup = followup(FollowupPolicy::CreateDirectly, false);
        let tracker = RecordingTracker::new();

        let err = apply_followup(
            &followup,
            &tracker,
            TrackerCapabilities::READ_ONLY,
            None,
            Vec::new(),
        )
        .await
        .unwrap_err();

        assert!(matches!(err, ApplyFollowupError::CreateIssueUnsupported));
        assert!(tracker.calls().is_empty());
    }

    #[tokio::test]
    async fn create_directly_followup_files_with_labels_and_initial_state() {
        let followup = followup(FollowupPolicy::CreateDirectly, true);
        let tracker = RecordingTracker::new();

        let applied = apply_followup(
            &followup,
            &tracker,
            TrackerCapabilities::FULL,
            Some(IssueState::new("Backlog")),
            vec!["blocker".into(), "follow-up".into()],
        )
        .await
        .unwrap();

        assert_eq!(applied.followup_id, FollowupId::new(41));
        assert_eq!(applied.tracker_id.as_str(), "123");
        assert_eq!(applied.identifier, "ENG-123");

        let calls = tracker.calls();
        assert_eq!(calls.len(), 1);
        let TrackerCall::Create(request) = &calls[0];
        assert_eq!(request.title, "Retry jitter histogram");
        assert_eq!(
            request.initial_state.as_ref().map(IssueState::as_str),
            Some("Backlog")
        );
        assert_eq!(request.labels, vec!["blocker", "follow-up"]);
        assert!(request.parent.is_none());
    }

    #[tokio::test]
    async fn approved_followup_after_manual_approval_files_successfully() {
        let mut followup = followup(FollowupPolicy::ProposeForApproval, false);
        followup
            .approve(RoleName::new("platform_lead"), "triage accepted")
            .unwrap();
        let tracker = RecordingTracker::new();

        let applied = apply_followup(
            &followup,
            &tracker,
            TrackerCapabilities::FULL,
            None,
            Vec::new(),
        )
        .await
        .unwrap();

        assert_eq!(applied.identifier, "ENG-123");
        assert_eq!(tracker.calls().len(), 1);
    }

    #[tokio::test]
    async fn rendered_body_contains_reason_scope_and_acceptance_criteria() {
        let followup = followup(FollowupPolicy::CreateDirectly, false);
        let tracker = RecordingTracker::new();

        let _ = apply_followup(
            &followup,
            &tracker,
            TrackerCapabilities::FULL,
            None,
            Vec::new(),
        )
        .await
        .unwrap();

        let TrackerCall::Create(request) = &tracker.calls()[0];
        let body = request.body.as_ref().expect("body rendered");
        assert!(body.contains("Useful observability follow-up"));
        assert!(body.contains("**Scope:** Telemetry-only extension"));
        assert!(body.contains("**Acceptance Criteria:**"));
        assert!(body.contains("- status output shows histogram buckets"));
    }

    #[tokio::test]
    async fn tracker_error_surfaces_with_followup_id() {
        let followup = followup(FollowupPolicy::CreateDirectly, false);
        let tracker = RecordingTracker::failing();

        let err = apply_followup(
            &followup,
            &tracker,
            TrackerCapabilities::FULL,
            None,
            Vec::new(),
        )
        .await
        .unwrap_err();

        match err {
            ApplyFollowupError::CreateIssueFailed { id, source } => {
                assert_eq!(id, FollowupId::new(41));
                assert!(source.to_string().contains("create boom"));
            }
            other => panic!("unexpected error: {other:?}"),
        }
    }

    #[tokio::test]
    async fn default_applier_delegates_to_free_function() {
        let followup = followup(FollowupPolicy::CreateDirectly, false);
        let tracker = RecordingTracker::new();
        let applier = DefaultFollowupApplier;

        let applied = applier
            .apply(
                &followup,
                &tracker,
                TrackerCapabilities::FULL,
                Some(IssueState::new("Backlog")),
                vec!["follow-up".into()],
            )
            .await
            .unwrap();

        assert_eq!(applied.identifier, "ENG-123");
        assert_eq!(tracker.calls().len(), 1);
    }
}
