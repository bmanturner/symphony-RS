//! Adapter-agnostic conformance suite for the [`TrackerMutations`] trait.
//!
//! Read conformance ([`crate::conformance`]) pins the read-side invariants
//! the orchestrator depends on. The mutation side has its own cross-cutting
//! contract — driven by SPEC v2 §7.2 and the trait docs on
//! [`TrackerMutations`] / [`symphony_core::tracker_trait::TrackerCapabilities`] — that every capable
//! adapter MUST satisfy:
//!
//! 1. **Capability/behaviour coherence on off-flags.** The trait docs are
//!    explicit: an adapter that returns `add_blocker: false` from
//!    [`TrackerRead::capabilities`] MUST return [`TrackerError::Misconfigured`]
//!    from [`TrackerMutations::add_blocker`] rather than silently degrade
//!    to a label-only approximation. The same rule applies to every flag.
//!    Workflows trust the capability bag at the read seam, so an adapter
//!    that lies in either direction is exploitable.
//! 2. **Object-safety through `dyn TrackerMutations`.** The orchestrator
//!    wires mutations as `Arc<dyn TrackerMutations>`; an adapter that can't
//!    be erased to that vtable is unusable in production paths.
//!
//! Verifying that *supported* mutations succeed end-to-end requires
//! adapter-specific wire setup (wiremock fixtures, fake servers, …) and
//! belongs in adapter-local tests. This suite stays universal: it only
//! exercises the off-flag rejection contract, which every capable adapter
//! must satisfy without any backend setup.
//!
//! ## How to add a new adapter
//!
//! 1. Build the adapter with whatever minimal config it needs (a placeholder
//!    base URL is fine — the suite only invokes mutations whose capability
//!    flag is `false`, and adapters MUST reject those at the entry point
//!    rather than dialling out).
//! 2. Erase it to `&dyn TrackerRead` (for capabilities) and
//!    `&dyn TrackerMutations` (for invocation), then call
//!    [`assert_unsupported_mutations_reject_with_misconfigured`].
//! 3. Any contract violation panics with a descriptive message naming the
//!    failing flag.

use crate::{TrackerMutations, TrackerRead};
use symphony_core::tracker::{IssueId, IssueState};
use symphony_core::tracker_trait::{
    AddBlockerRequest, AddCommentRequest, ArtifactKind, AttachArtifactRequest, CreateIssueRequest,
    LinkParentChildRequest, TrackerError, UpdateIssueRequest,
};

/// Probe payload for [`TrackerMutations::create_issue`]. Field values are
/// intentionally minimal — the call is expected to reject before the
/// adapter touches the wire, so the payload only needs to satisfy the
/// type-system requirements.
fn probe_create() -> CreateIssueRequest {
    CreateIssueRequest {
        title: "conformance probe".into(),
        body: None,
        labels: Vec::new(),
        assignees: Vec::new(),
        parent: None,
        initial_state: None,
    }
}

fn probe_update() -> UpdateIssueRequest {
    UpdateIssueRequest {
        id: IssueId::new("probe-1"),
        state: Some(IssueState::new("Todo")),
        title: None,
        body: None,
        add_labels: Vec::new(),
        remove_labels: Vec::new(),
        add_assignees: Vec::new(),
        remove_assignees: Vec::new(),
    }
}

fn probe_add_comment() -> AddCommentRequest {
    AddCommentRequest {
        issue: IssueId::new("probe-1"),
        body: "conformance probe".into(),
        author_role: None,
    }
}

fn probe_add_blocker() -> AddBlockerRequest {
    AddBlockerRequest {
        blocked: IssueId::new("probe-blocked"),
        blocker: IssueId::new("probe-blocker"),
        reason: None,
    }
}

fn probe_link_parent_child() -> LinkParentChildRequest {
    LinkParentChildRequest {
        parent: IssueId::new("probe-parent"),
        child: IssueId::new("probe-child"),
    }
}

fn probe_attach_artifact() -> AttachArtifactRequest {
    AttachArtifactRequest {
        issue: IssueId::new("probe-1"),
        kind: ArtifactKind::PullRequest,
        uri: "https://example.test/pr/1".into(),
        label: None,
        note: None,
    }
}

/// Assert that every mutation whose capability flag is `false` rejects with
/// [`TrackerError::Misconfigured`].
///
/// Pins the SPEC v2 §7.2 contract that adapters MUST NOT silently degrade
/// unsupported mutations. The suite reads `read.capabilities()` and only
/// probes off-flags — supported mutations are out of scope here because
/// verifying their success requires backend wire setup.
///
/// On a violation, panics with a message naming the failing flag and the
/// observed result. Returning a `Result` would just push `.unwrap()` to the
/// caller without adding signal — the suite is meant to run inside
/// `#[tokio::test]` bodies where a panic is the idiomatic failure mode.
pub async fn assert_unsupported_mutations_reject_with_misconfigured(
    read: &dyn TrackerRead,
    mutations: &dyn TrackerMutations,
) {
    let caps = read.capabilities();

    if !caps.create_issue {
        let res = mutations.create_issue(probe_create()).await;
        assert_misconfigured("create_issue", &format!("{res:?}"), res.is_err(), &res);
    }
    if !caps.update_issue {
        let res = mutations.update_issue(probe_update()).await;
        assert_misconfigured("update_issue", &format!("{res:?}"), res.is_err(), &res);
    }
    if !caps.add_comment {
        let res = mutations.add_comment(probe_add_comment()).await;
        assert_misconfigured("add_comment", &format!("{res:?}"), res.is_err(), &res);
    }
    if !caps.add_blocker {
        let res = mutations.add_blocker(probe_add_blocker()).await;
        assert_misconfigured("add_blocker", &format!("{res:?}"), res.is_err(), &res);
    }
    if !caps.link_parent_child {
        let res = mutations.link_parent_child(probe_link_parent_child()).await;
        assert_misconfigured("link_parent_child", &format!("{res:?}"), res.is_err(), &res);
    }
    if !caps.attach_artifact {
        let res = mutations.attach_artifact(probe_attach_artifact()).await;
        assert_misconfigured("attach_artifact", &format!("{res:?}"), res.is_err(), &res);
    }
}

/// Shared assertion body. Kept private — every call site supplies the
/// method name, a `Debug` rendering of the result for the panic message,
/// the `is_err` flag, and the result itself for the variant check.
fn assert_misconfigured<T: std::fmt::Debug>(
    method: &str,
    rendered: &str,
    is_err: bool,
    result: &Result<T, TrackerError>,
) {
    assert!(
        is_err,
        "TrackerMutations::{method} returned Ok({rendered}) but capabilities reported the flag \
         as false — adapter is silently fulfilling an unsupported mutation. \
         SPEC v2 §7.2 forbids this; rework the adapter to reject with TrackerError::Misconfigured.",
    );
    let err = result
        .as_ref()
        .expect_err("is_err just verified the Result is Err");
    assert!(
        matches!(err, TrackerError::Misconfigured(_)),
        "TrackerMutations::{method} rejected with {err:?} but the trait contract requires \
         TrackerError::Misconfigured for off-flag mutations (SPEC v2 §7.2 / TrackerMutations \
         doc-comment). A different error variant would let workflows assume a transient \
         failure and retry, masking the missing capability.",
    );
}

/// Convenience suite runner. Mirrors [`crate::conformance::run_full_suite`]
/// for the read side. Today it dispatches a single assertion; the wrapper
/// exists so adapter tests can use the same call shape and so future
/// universal mutation contracts can be added without touching every
/// adapter test.
pub async fn run_full_suite(read: &dyn TrackerRead, mutations: &dyn TrackerMutations) {
    assert_unsupported_mutations_reject_with_misconfigured(read, mutations).await;
}

#[cfg(test)]
mod tests {
    use super::*;
    use async_trait::async_trait;
    use std::sync::Arc;
    use symphony_core::tracker::Issue;
    use symphony_core::tracker_trait::{
        AddBlockerResponse, AddCommentResponse, AttachArtifactResponse, CreateIssueResponse,
        LinkParentChildResponse, TrackerCapabilities, TrackerResult, UpdateIssueResponse,
    };

    /// A test stub that lets each test pick:
    /// - what capabilities to advertise via [`TrackerRead::capabilities`];
    /// - whether each mutation rejects (with `Misconfigured`) or "succeeds"
    ///   (returning a canned response). Together these let us exercise both
    ///   sides of the contract — including the negative tests below that
    ///   confirm the suite catches a lying adapter.
    struct ConfigurableAdapter {
        caps: TrackerCapabilities,
        reject_all: bool,
    }

    #[async_trait]
    impl TrackerRead for ConfigurableAdapter {
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
            self.caps
        }
    }

    fn misconfigured() -> TrackerError {
        TrackerError::Misconfigured("stub".into())
    }

    #[async_trait]
    impl TrackerMutations for ConfigurableAdapter {
        async fn create_issue(
            &self,
            _request: CreateIssueRequest,
        ) -> TrackerResult<CreateIssueResponse> {
            if self.reject_all || !self.caps.create_issue {
                Err(misconfigured())
            } else {
                Ok(CreateIssueResponse {
                    id: IssueId::new("probe-id"),
                    identifier: "PROBE-1".into(),
                    url: None,
                })
            }
        }
        async fn update_issue(
            &self,
            _request: UpdateIssueRequest,
        ) -> TrackerResult<UpdateIssueResponse> {
            if self.reject_all || !self.caps.update_issue {
                Err(misconfigured())
            } else {
                Ok(UpdateIssueResponse {
                    id: IssueId::new("probe-id"),
                    state: None,
                })
            }
        }
        async fn add_comment(
            &self,
            _request: AddCommentRequest,
        ) -> TrackerResult<AddCommentResponse> {
            if self.reject_all || !self.caps.add_comment {
                Err(misconfigured())
            } else {
                Ok(AddCommentResponse {
                    id: None,
                    url: None,
                })
            }
        }
        async fn add_blocker(
            &self,
            _request: AddBlockerRequest,
        ) -> TrackerResult<AddBlockerResponse> {
            if self.reject_all || !self.caps.add_blocker {
                Err(misconfigured())
            } else {
                Ok(AddBlockerResponse { edge_id: None })
            }
        }
        async fn link_parent_child(
            &self,
            _request: LinkParentChildRequest,
        ) -> TrackerResult<LinkParentChildResponse> {
            if self.reject_all || !self.caps.link_parent_child {
                Err(misconfigured())
            } else {
                Ok(LinkParentChildResponse { edge_id: None })
            }
        }
        async fn attach_artifact(
            &self,
            _request: AttachArtifactRequest,
        ) -> TrackerResult<AttachArtifactResponse> {
            if self.reject_all || !self.caps.attach_artifact {
                Err(misconfigured())
            } else {
                Ok(AttachArtifactResponse { id: None })
            }
        }
    }

    #[tokio::test]
    async fn read_only_adapter_rejects_every_mutation_with_misconfigured() {
        let adapter = Arc::new(ConfigurableAdapter {
            caps: TrackerCapabilities::READ_ONLY,
            reject_all: true,
        });
        let read: Arc<dyn TrackerRead> = adapter.clone();
        let mutations: Arc<dyn TrackerMutations> = adapter;
        run_full_suite(read.as_ref(), mutations.as_ref()).await;
    }

    #[tokio::test]
    async fn full_capability_adapter_skips_every_off_flag_probe_and_passes_trivially() {
        // FULL means no off-flags exist, so the suite makes zero calls and
        // returns. Exercise the path explicitly — Linear (capabilities ==
        // FULL today) relies on this skip semantics in the integration
        // tests below.
        let adapter = Arc::new(ConfigurableAdapter {
            caps: TrackerCapabilities::FULL,
            reject_all: false,
        });
        let read: Arc<dyn TrackerRead> = adapter.clone();
        let mutations: Arc<dyn TrackerMutations> = adapter;
        run_full_suite(read.as_ref(), mutations.as_ref()).await;
    }

    #[tokio::test]
    async fn partial_capability_adapter_passes_when_off_flags_reject_correctly() {
        // GitHub-shaped: create/update/comment/attach on, blocker/parent
        // off. Reject-policy honours the flags exactly.
        let caps = TrackerCapabilities {
            create_issue: true,
            update_issue: true,
            add_comment: true,
            add_blocker: false,
            link_parent_child: false,
            attach_artifact: true,
        };
        let adapter = Arc::new(ConfigurableAdapter {
            caps,
            reject_all: false,
        });
        let read: Arc<dyn TrackerRead> = adapter.clone();
        let mutations: Arc<dyn TrackerMutations> = adapter;
        assert_unsupported_mutations_reject_with_misconfigured(read.as_ref(), mutations.as_ref())
            .await;
    }

    #[tokio::test]
    async fn suite_panics_when_off_flag_mutation_silently_succeeds() {
        // Lying adapter: claims `add_blocker: false` but the method
        // returns Ok. The suite MUST catch this — without this negative
        // test, a future regression that drops the rejection check would
        // pass silently.
        struct LyingAdapter;

        #[async_trait]
        impl TrackerRead for LyingAdapter {
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
                TrackerCapabilities::READ_ONLY
            }
        }

        #[async_trait]
        impl TrackerMutations for LyingAdapter {
            async fn create_issue(
                &self,
                _request: CreateIssueRequest,
            ) -> TrackerResult<CreateIssueResponse> {
                Ok(CreateIssueResponse {
                    id: IssueId::new("liar"),
                    identifier: "LIAR-1".into(),
                    url: None,
                })
            }
            async fn update_issue(
                &self,
                _request: UpdateIssueRequest,
            ) -> TrackerResult<UpdateIssueResponse> {
                Err(misconfigured())
            }
            async fn add_comment(
                &self,
                _request: AddCommentRequest,
            ) -> TrackerResult<AddCommentResponse> {
                Err(misconfigured())
            }
            async fn add_blocker(
                &self,
                _request: AddBlockerRequest,
            ) -> TrackerResult<AddBlockerResponse> {
                Err(misconfigured())
            }
            async fn link_parent_child(
                &self,
                _request: LinkParentChildRequest,
            ) -> TrackerResult<LinkParentChildResponse> {
                Err(misconfigured())
            }
            async fn attach_artifact(
                &self,
                _request: AttachArtifactRequest,
            ) -> TrackerResult<AttachArtifactResponse> {
                Err(misconfigured())
            }
        }

        let adapter = Arc::new(LyingAdapter);
        let read: Arc<dyn TrackerRead> = adapter.clone();
        let mutations: Arc<dyn TrackerMutations> = adapter;
        let panicked = tokio::task::spawn(async move {
            assert_unsupported_mutations_reject_with_misconfigured(
                read.as_ref(),
                mutations.as_ref(),
            )
            .await;
        })
        .await
        .is_err();
        assert!(
            panicked,
            "suite must panic when an off-flag mutation returns Ok instead of Misconfigured"
        );
    }

    #[tokio::test]
    async fn suite_panics_when_off_flag_mutation_returns_wrong_error_variant() {
        // Adapter claims `add_blocker: false` and *does* fail, but with
        // `Transport` instead of `Misconfigured`. Workflows would mistake
        // this for a transient failure and retry, masking the missing
        // capability — the suite catches that.
        struct WrongVariantAdapter;

        #[async_trait]
        impl TrackerRead for WrongVariantAdapter {
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
                TrackerCapabilities::READ_ONLY
            }
        }

        #[async_trait]
        impl TrackerMutations for WrongVariantAdapter {
            async fn create_issue(
                &self,
                _request: CreateIssueRequest,
            ) -> TrackerResult<CreateIssueResponse> {
                Err(TrackerError::Transport("network blip".into()))
            }
            async fn update_issue(
                &self,
                _request: UpdateIssueRequest,
            ) -> TrackerResult<UpdateIssueResponse> {
                Err(misconfigured())
            }
            async fn add_comment(
                &self,
                _request: AddCommentRequest,
            ) -> TrackerResult<AddCommentResponse> {
                Err(misconfigured())
            }
            async fn add_blocker(
                &self,
                _request: AddBlockerRequest,
            ) -> TrackerResult<AddBlockerResponse> {
                Err(misconfigured())
            }
            async fn link_parent_child(
                &self,
                _request: LinkParentChildRequest,
            ) -> TrackerResult<LinkParentChildResponse> {
                Err(misconfigured())
            }
            async fn attach_artifact(
                &self,
                _request: AttachArtifactRequest,
            ) -> TrackerResult<AttachArtifactResponse> {
                Err(misconfigured())
            }
        }

        let adapter = Arc::new(WrongVariantAdapter);
        let read: Arc<dyn TrackerRead> = adapter.clone();
        let mutations: Arc<dyn TrackerMutations> = adapter;
        let panicked = tokio::task::spawn(async move {
            assert_unsupported_mutations_reject_with_misconfigured(
                read.as_ref(),
                mutations.as_ref(),
            )
            .await;
        })
        .await
        .is_err();
        assert!(
            panicked,
            "suite must panic when off-flag rejection uses a non-Misconfigured error variant"
        );
    }
}
