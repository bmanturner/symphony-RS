//! Advisory / no-op mutation wrapper for read-only trackers.
//!
//! SPEC v2 §7.2: "If a tracker lacks mutation support, workflow MUST run
//! in advisory/proposal mode." The orchestrator still wants to dispatch
//! through a single [`TrackerMutations`] seam regardless of whether the
//! underlying adapter can mutate. [`AdvisoryMutations`] gives that seam a
//! safe implementation when the adapter is read-only:
//!
//! * each mutation request is recorded into an in-memory advisory log
//!   instead of being sent to the backend;
//! * the call returns `Ok(...)` with id-less synthetic responses so
//!   callers that already check capabilities don't blow up on a forced
//!   error path;
//! * the recorded log is drainable and inspectable so a proposal-mode
//!   workflow can later persist the requests as `propose_for_approval`
//!   follow-ups, surface them in `symphony status`, or attach them to
//!   the durable event tail.
//!
//! The wrapper deliberately does *not* claim any mutation capability — it
//! reports the wrapped tracker's capabilities verbatim, which by definition
//! is [`TrackerCapabilities::READ_ONLY`] for the read-only case. Workflows
//! that interrogate the read seam before dispatching will still see the
//! truth and route to advisory/proposal flows; the wrapper exists for the
//! code paths that already lowered into a `TrackerMutations` call.

use crate::tracker::IssueId;
use crate::tracker_trait::{
    AddBlockerRequest, AddBlockerResponse, AddCommentRequest, AddCommentResponse,
    AttachArtifactRequest, AttachArtifactResponse, CreateIssueRequest, CreateIssueResponse,
    LinkParentChildRequest, LinkParentChildResponse, TrackerCapabilities, TrackerMutations,
    TrackerRead, TrackerResult, UpdateIssueRequest, UpdateIssueResponse,
};
use async_trait::async_trait;
use std::sync::{Arc, Mutex};

/// One captured advisory mutation request. Variants mirror the methods of
/// [`TrackerMutations`] one-for-one so consumers can pattern-match without
/// downcasting.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AdvisoryRecord {
    CreateIssue(CreateIssueRequest),
    UpdateIssue(UpdateIssueRequest),
    AddComment(AddCommentRequest),
    AddBlocker(AddBlockerRequest),
    LinkParentChild(LinkParentChildRequest),
    AttachArtifact(AttachArtifactRequest),
}

/// Wraps a [`TrackerRead`] adapter so it can be presented as a
/// [`TrackerMutations`] without actually mutating the backend.
///
/// Construction takes an `Arc<dyn TrackerRead>` so the wrapper composes
/// with the orchestrator's existing dyn-erased adapter handles. The
/// reported capabilities are the inner adapter's capabilities — *not* a
/// fabricated full set — preserving the invariant from SPEC v2 §7.2 that
/// capability flags reflect real capability.
///
/// The advisory log is held under a `Mutex<Vec<...>>`. Mutation throughput
/// in advisory mode is bounded by agent decomposition cadence, not by
/// poll-loop hot paths, so a coarse mutex is plenty and keeps the API
/// borrow-free for `&self` callers across `await` points.
#[derive(Clone)]
pub struct AdvisoryMutations {
    inner: Arc<dyn TrackerRead>,
    log: Arc<Mutex<Vec<AdvisoryRecord>>>,
}

impl AdvisoryMutations {
    /// Wrap a read-only tracker. The wrapper does not assert that the
    /// inner tracker is *actually* read-only — composition with a
    /// mutation-capable adapter is occasionally useful for tests or for
    /// shadow / dry-run flows.
    pub fn new(inner: Arc<dyn TrackerRead>) -> Self {
        Self {
            inner,
            log: Arc::new(Mutex::new(Vec::new())),
        }
    }

    /// Capability bag reported by the wrapped adapter. Surfaced here so
    /// workflows holding only the wrapper can still consult the read-side
    /// truth without round-tripping through `Arc::downgrade`.
    pub fn capabilities(&self) -> TrackerCapabilities {
        self.inner.capabilities()
    }

    /// Borrow the wrapped read-only adapter, e.g. for capability checks
    /// from code that has only the wrapper handle.
    pub fn inner(&self) -> &Arc<dyn TrackerRead> {
        &self.inner
    }

    /// Snapshot the current advisory log without consuming it. Useful for
    /// `symphony status`-style read-only views.
    pub fn snapshot(&self) -> Vec<AdvisoryRecord> {
        self.log
            .lock()
            .expect("advisory log mutex poisoned")
            .clone()
    }

    /// Take and clear the advisory log. The proposal-mode workflow calls
    /// this on each tick to lower captured advisories into durable
    /// follow-up records (per SPEC v2 §5.13 `propose_for_approval`).
    pub fn drain(&self) -> Vec<AdvisoryRecord> {
        std::mem::take(&mut *self.log.lock().expect("advisory log mutex poisoned"))
    }

    /// Number of recorded advisories without cloning the log. Cheap status
    /// check for tests and metrics.
    pub fn len(&self) -> usize {
        self.log.lock().expect("advisory log mutex poisoned").len()
    }

    /// True when no mutation requests have been captured.
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    fn record(&self, entry: AdvisoryRecord) {
        self.log
            .lock()
            .expect("advisory log mutex poisoned")
            .push(entry);
    }
}

/// Synthetic identifier returned for advisory create-issue responses.
/// Distinguishable from any real tracker id by its `advisory:` prefix so
/// downstream code can detect and reject it if it leaks past the proposal
/// queue. Suffix is the 1-based index into the advisory log at insertion
/// time, which is stable enough for diagnostics.
fn advisory_issue_id(seq: usize) -> IssueId {
    IssueId::new(format!("advisory:proposal-{seq}"))
}

#[async_trait]
impl TrackerMutations for AdvisoryMutations {
    async fn create_issue(
        &self,
        request: CreateIssueRequest,
    ) -> TrackerResult<CreateIssueResponse> {
        self.record(AdvisoryRecord::CreateIssue(request));
        let seq = self.len();
        Ok(CreateIssueResponse {
            id: advisory_issue_id(seq),
            identifier: format!("advisory#{seq}"),
            url: None,
        })
    }

    async fn update_issue(
        &self,
        request: UpdateIssueRequest,
    ) -> TrackerResult<UpdateIssueResponse> {
        let echoed_state = request.state.clone();
        let id = request.id.clone();
        self.record(AdvisoryRecord::UpdateIssue(request));
        Ok(UpdateIssueResponse {
            id,
            state: echoed_state,
        })
    }

    async fn add_comment(&self, request: AddCommentRequest) -> TrackerResult<AddCommentResponse> {
        self.record(AdvisoryRecord::AddComment(request));
        Ok(AddCommentResponse {
            id: None,
            url: None,
        })
    }

    async fn add_blocker(&self, request: AddBlockerRequest) -> TrackerResult<AddBlockerResponse> {
        self.record(AdvisoryRecord::AddBlocker(request));
        Ok(AddBlockerResponse { edge_id: None })
    }

    async fn link_parent_child(
        &self,
        request: LinkParentChildRequest,
    ) -> TrackerResult<LinkParentChildResponse> {
        self.record(AdvisoryRecord::LinkParentChild(request));
        Ok(LinkParentChildResponse { edge_id: None })
    }

    async fn attach_artifact(
        &self,
        request: AttachArtifactRequest,
    ) -> TrackerResult<AttachArtifactResponse> {
        self.record(AdvisoryRecord::AttachArtifact(request));
        Ok(AttachArtifactResponse { id: None })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tracker::{Issue, IssueState};
    use crate::tracker_trait::ArtifactKind;

    /// Read-only tracker stub. Reports `READ_ONLY` capabilities so the
    /// wrapper composes with the canonical advisory-mode case.
    struct ReadOnlyStub;

    #[async_trait]
    impl TrackerRead for ReadOnlyStub {
        async fn fetch_active(&self) -> TrackerResult<Vec<Issue>> {
            Ok(Vec::new())
        }
        async fn fetch_state(&self, _ids: &[IssueId]) -> TrackerResult<Vec<Issue>> {
            Ok(Vec::new())
        }
        async fn fetch_terminal_recent(
            &self,
            _terminal_states: &[IssueState],
        ) -> TrackerResult<Vec<Issue>> {
            Ok(Vec::new())
        }
    }

    fn wrap_read_only() -> AdvisoryMutations {
        AdvisoryMutations::new(Arc::new(ReadOnlyStub))
    }

    #[tokio::test]
    async fn create_issue_records_request_and_returns_advisory_id() {
        let advisory = wrap_read_only();
        let resp = advisory
            .create_issue(CreateIssueRequest {
                title: "decompose payments".into(),
                body: None,
                labels: vec!["needs-triage".into()],
                assignees: vec![],
                parent: Some(IssueId::new("PARENT-1")),
                initial_state: None,
            })
            .await
            .unwrap();
        assert!(resp.id.as_str().starts_with("advisory:proposal-"));
        assert!(resp.identifier.starts_with("advisory#"));
        assert!(resp.url.is_none());
        assert_eq!(advisory.len(), 1);
        match &advisory.snapshot()[0] {
            AdvisoryRecord::CreateIssue(req) => {
                assert_eq!(req.title, "decompose payments");
                assert_eq!(req.parent.as_ref().unwrap().as_str(), "PARENT-1");
            }
            other => panic!("unexpected advisory record: {other:?}"),
        }
    }

    #[tokio::test]
    async fn update_issue_echoes_id_and_requested_state_in_response() {
        let advisory = wrap_read_only();
        let resp = advisory
            .update_issue(UpdateIssueRequest {
                id: IssueId::new("ABC-1"),
                state: Some(IssueState::new("In Review")),
                title: None,
                body: None,
                add_labels: vec!["qa-pending".into()],
                remove_labels: vec![],
                add_assignees: vec![],
                remove_assignees: vec![],
            })
            .await
            .unwrap();
        assert_eq!(resp.id.as_str(), "ABC-1");
        assert_eq!(resp.state.unwrap().as_str(), "In Review");
        assert_eq!(advisory.len(), 1);
    }

    #[tokio::test]
    async fn add_comment_blocker_link_attach_each_record_distinct_variant() {
        let advisory = wrap_read_only();
        advisory
            .add_comment(AddCommentRequest {
                issue: IssueId::new("X"),
                body: "note".into(),
                author_role: Some("qa".into()),
            })
            .await
            .unwrap();
        advisory
            .add_blocker(AddBlockerRequest {
                blocked: IssueId::new("X"),
                blocker: IssueId::new("Y"),
                reason: Some("waiting on schema".into()),
            })
            .await
            .unwrap();
        advisory
            .link_parent_child(LinkParentChildRequest {
                parent: IssueId::new("EPIC"),
                child: IssueId::new("X"),
            })
            .await
            .unwrap();
        advisory
            .attach_artifact(AttachArtifactRequest {
                issue: IssueId::new("X"),
                kind: ArtifactKind::PullRequest,
                uri: "https://example.test/pr/1".into(),
                label: None,
                note: None,
            })
            .await
            .unwrap();
        let snap = advisory.snapshot();
        assert_eq!(snap.len(), 4);
        assert!(matches!(snap[0], AdvisoryRecord::AddComment(_)));
        assert!(matches!(snap[1], AdvisoryRecord::AddBlocker(_)));
        assert!(matches!(snap[2], AdvisoryRecord::LinkParentChild(_)));
        assert!(matches!(snap[3], AdvisoryRecord::AttachArtifact(_)));
    }

    #[tokio::test]
    async fn drain_returns_and_clears_the_log_so_callers_can_lower_to_proposals() {
        let advisory = wrap_read_only();
        advisory
            .add_comment(AddCommentRequest {
                issue: IssueId::new("X"),
                body: "first".into(),
                author_role: None,
            })
            .await
            .unwrap();
        advisory
            .add_comment(AddCommentRequest {
                issue: IssueId::new("X"),
                body: "second".into(),
                author_role: None,
            })
            .await
            .unwrap();
        assert_eq!(advisory.len(), 2);

        let drained = advisory.drain();
        assert_eq!(drained.len(), 2);
        assert!(advisory.is_empty());

        // A subsequent call records into the now-empty log.
        advisory
            .add_comment(AddCommentRequest {
                issue: IssueId::new("X"),
                body: "third".into(),
                author_role: None,
            })
            .await
            .unwrap();
        assert_eq!(advisory.len(), 1);
    }

    #[tokio::test]
    async fn capabilities_mirror_inner_so_workflows_still_see_the_truth() {
        let advisory = wrap_read_only();
        let caps = advisory.capabilities();
        assert!(caps.is_read_only());
        assert_eq!(caps, TrackerCapabilities::READ_ONLY);
    }

    #[tokio::test]
    async fn capabilities_reflect_inner_even_when_inner_advertises_mutations() {
        // The wrapper does not lie about capabilities. Composing it over a
        // mutation-capable adapter is unusual, but the reported caps must
        // come from the inner adapter so workflows that route based on
        // capability still get the truth.
        struct MutatingStub;
        #[async_trait]
        impl TrackerRead for MutatingStub {
            async fn fetch_active(&self) -> TrackerResult<Vec<Issue>> {
                Ok(Vec::new())
            }
            async fn fetch_state(&self, _ids: &[IssueId]) -> TrackerResult<Vec<Issue>> {
                Ok(Vec::new())
            }
            async fn fetch_terminal_recent(
                &self,
                _terminal_states: &[IssueState],
            ) -> TrackerResult<Vec<Issue>> {
                Ok(Vec::new())
            }
            fn capabilities(&self) -> TrackerCapabilities {
                TrackerCapabilities::FULL
            }
        }
        let advisory = AdvisoryMutations::new(Arc::new(MutatingStub));
        assert_eq!(advisory.capabilities(), TrackerCapabilities::FULL);
    }

    #[tokio::test]
    async fn wrapper_is_object_safe_as_arc_dyn_tracker_mutations() {
        let advisory: Arc<dyn TrackerMutations> = Arc::new(wrap_read_only());
        let resp = advisory
            .add_comment(AddCommentRequest {
                issue: IssueId::new("X"),
                body: "via dyn".into(),
                author_role: None,
            })
            .await
            .unwrap();
        assert!(resp.id.is_none());
        assert!(resp.url.is_none());
    }

    #[tokio::test]
    async fn wrapper_handle_is_clone_and_shares_log_across_clones() {
        // Cloning yields a new handle pointing at the same advisory log;
        // this lets the kernel hold one handle while the proposal drainer
        // holds another without coordinating ownership.
        let a = wrap_read_only();
        let b = a.clone();
        a.add_comment(AddCommentRequest {
            issue: IssueId::new("X"),
            body: "from a".into(),
            author_role: None,
        })
        .await
        .unwrap();
        b.add_comment(AddCommentRequest {
            issue: IssueId::new("X"),
            body: "from b".into(),
            author_role: None,
        })
        .await
        .unwrap();
        assert_eq!(a.len(), 2);
        assert_eq!(b.len(), 2);
        assert_eq!(b.drain().len(), 2);
        assert!(a.is_empty());
    }

    #[test]
    fn advisory_mutations_is_send_sync_so_it_crosses_task_boundaries() {
        fn assert_send_sync<T: Send + Sync>() {}
        assert_send_sync::<AdvisoryMutations>();
        assert_send_sync::<AdvisoryRecord>();
    }
}
