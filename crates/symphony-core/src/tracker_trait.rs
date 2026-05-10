//! Tracker traits — the abstract seam between Symphony and any concrete
//! issue-tracking backend (Linear, GitHub Issues, …).
//!
//! v2 splits the previous monolithic `TrackerRead` into two object-safe
//! halves per SPEC v2 §7.1 and §7.2:
//!
//! * [`TrackerRead`] — read-only candidate/state/recovery queries the
//!   poll loop relies on. All current adapters MUST implement this.
//! * [`TrackerMutations`] — write-side capabilities (create issue, update
//!   issue, comment, add blocker, link parent/child, attach artifact).
//!   Adapters opt in; workflows that require mutations check
//!   [`TrackerRead::capabilities`] before dispatching. Each method on the
//!   trait corresponds to a flag on [`TrackerCapabilities`] and a section
//!   in SPEC v2 §7.2.
//!
//! The split lets workflows detect read-only vs mutation-capable adapters
//! and lets adapters that genuinely cannot mutate (e.g. an audit-log
//! exporter) participate without faking writes.
//!
//! Method semantics on [`TrackerRead`] preserve the previous v1 contract
//! verbatim — `fetch_active`, `fetch_state`, and especially
//! `fetch_terminal_recent` (the startup/recovery sweep) keep their
//! contracts so adapters and callers don't change behavior with the
//! rename.
//!
//! ## Why the traits live in `symphony-core`
//!
//! Architectural tenet: the orchestrator never speaks Linear (or GitHub or
//! Jira) protocol. It only sees the [`Issue`] model and these traits.
//! Putting them alongside `Issue` keeps the abstract layer in one crate
//! and makes the dependency direction obvious — `symphony-tracker`
//! depends on `symphony-core`, never the reverse.
//!
//! ## Async, dynamic dispatch, and `Send + Sync`
//!
//! The orchestrator stores its tracker as `Arc<dyn TrackerRead>` and the
//! poll loop is multi-threaded under tokio. Methods therefore use
//! `async-trait` for object-safe async, and the traits inherit `Send +
//! Sync` so the box can cross task boundaries. Implementations whose
//! internal state is not naturally `Sync` (a `reqwest::Client` is, but a
//! `Cell` is not) need to wrap that state appropriately.
//!
//! ## Error model
//!
//! Trackers report failures through [`TrackerError`], a small enum that
//! distinguishes transient transport problems (the orchestrator should
//! keep running) from misconfiguration (the orchestrator should surface
//! and stop dispatching). The orchestrator's reconciliation logic in
//! SPEC §16.3 explicitly tolerates `fetch_issue_states_by_ids` failing —
//! "log_debug('keep workers running')" — so adapters should *not* panic
//! or wrap unrelated errors as misconfiguration.

use crate::tracker::{Issue, IssueComment, IssueId, IssueState, RelatedIssues};
use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use thiserror::Error;

/// Failures a tracker adapter can report to the orchestrator.
///
/// We deliberately keep this enum small. The orchestrator's reaction to an
/// error is coarse — keep running, or surface and stop dispatching — so
/// finer granularity would only add noise. Adapters convert their native
/// error types (a `reqwest::Error`, a GraphQL response error, an octocrab
/// failure) into the closest variant here.
#[derive(Debug, Error)]
pub enum TrackerError {
    /// Network-layer failure: connect timeout, read timeout, DNS, TLS,
    /// 5xx from the server, GraphQL response with `errors` populated.
    /// The orchestrator's reconcile path explicitly tolerates this and
    /// keeps running workers alive (SPEC §16.3).
    #[error("tracker transport failure: {0}")]
    Transport(String),

    /// The tracker rejected our credentials or scope. Distinct from
    /// [`TrackerError::Transport`] because operators usually want a
    /// loud, fatal-looking signal here rather than a quiet retry loop.
    #[error("tracker auth rejected: {0}")]
    Unauthorized(String),

    /// The tracker returned a payload we could not decode into the
    /// normalized [`Issue`] model. Almost always a schema drift bug in
    /// the adapter, not a runtime condition the orchestrator can fix.
    #[error("tracker returned malformed payload: {0}")]
    Malformed(String),

    /// Configuration the operator supplied is incompatible with this
    /// adapter (missing project slug, unknown state name, etc). The
    /// orchestrator surfaces this and skips dispatch for the tick.
    #[error("tracker misconfigured: {0}")]
    Misconfigured(String),

    /// Catch-all for adapter-internal failures that don't fit the
    /// variants above. Used sparingly; prefer the specific variants.
    #[error("tracker error: {0}")]
    Other(String),

    /// Optional read/mutation capability is not implemented by this
    /// adapter.
    #[error("tracker capability unsupported: {0}")]
    Unsupported(String),
}

/// Convenience alias used throughout the trait surface.
pub type TrackerResult<T> = Result<T, TrackerError>;

/// Per-adapter mutation capability report (SPEC v2 §7.2).
///
/// Workflows that decompose parents into children, file blockers, or attach
/// PR/artifact evidence MUST run against a mutation-capable adapter. The
/// capability bag is checked at the read seam — *before* any
/// [`TrackerMutations`] dispatch — so an advisory/proposal-mode workflow
/// can still run against a read-only tracker without ever asking for the
/// mutation trait object.
///
/// Adapters report their capabilities via [`TrackerRead::capabilities`].
/// The default impl returns [`TrackerCapabilities::READ_ONLY`], so adapters
/// that opt in to [`TrackerMutations`] need only override the method to
/// flip the relevant flags. Capability flags map 1:1 to the mutation
/// operations enumerated in SPEC v2 §7.2; flipping a flag is a public
/// promise that the adapter implements that mutation reliably enough for
/// production workflow use, not just that the trait method exists.
///
/// Granularity is intentional: GitHub Issues can `create_issue` and
/// `add_comment` but expresses "blockers" only as labels, while Linear has
/// first-class issue dependencies. A workflow that needs structural
/// blocker links can reject a GitHub-only configuration without paying for
/// a runtime probe.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TrackerCapabilities {
    /// Adapter can create new issues (SPEC v2 §7.2 "create issue").
    pub create_issue: bool,
    /// Adapter can update issue status / fields (SPEC v2 §7.2 "update status").
    pub update_issue: bool,
    /// Adapter can post comments / activity entries (SPEC v2 §7.2 "add comment").
    pub add_comment: bool,
    /// Adapter can express structural blocker/dependency edges
    /// (SPEC v2 §7.2 "create blocker/dependency"). Label-only proxies do
    /// NOT count — workflows that gate on this expect first-class edges.
    pub add_blocker: bool,
    /// Adapter can link a child issue to its parent (SPEC v2 §7.2
    /// "link parent/child"). Required for decomposition workflows.
    pub link_parent_child: bool,
    /// Adapter can attach PR refs, run logs, or QA evidence to an issue
    /// (SPEC v2 §7.2 "attach PR/artifact/evidence").
    pub attach_artifact: bool,
}

impl TrackerCapabilities {
    /// Capability set for an adapter that implements [`TrackerRead`] only.
    /// Used as the default return value of [`TrackerRead::capabilities`].
    pub const READ_ONLY: Self = Self {
        create_issue: false,
        update_issue: false,
        add_comment: false,
        add_blocker: false,
        link_parent_child: false,
        attach_artifact: false,
    };

    /// Capability set for an adapter that supports every mutation listed
    /// in SPEC v2 §7.2. Convenience constant for tests and for adapters
    /// that genuinely cover the full surface.
    pub const FULL: Self = Self {
        create_issue: true,
        update_issue: true,
        add_comment: true,
        add_blocker: true,
        link_parent_child: true,
        attach_artifact: true,
    };

    /// True when the adapter reports no mutation capability at all.
    /// Workflows in advisory/proposal mode are safe; autonomous workflows
    /// MUST refuse to dispatch (SPEC v2 §7.2 final paragraph).
    pub const fn is_read_only(&self) -> bool {
        !(self.create_issue
            || self.update_issue
            || self.add_comment
            || self.add_blocker
            || self.link_parent_child
            || self.attach_artifact)
    }

    /// True when the adapter reports any mutation capability. Inverse of
    /// [`Self::is_read_only`]; provided for readability at call sites that
    /// branch on "can we mutate at all".
    pub const fn supports_any_mutation(&self) -> bool {
        !self.is_read_only()
    }
}

impl Default for TrackerCapabilities {
    /// Defaults to [`TrackerCapabilities::READ_ONLY`] so a freshly
    /// constructed adapter or test stub reports the safest possible
    /// capability set until it explicitly opts in.
    fn default() -> Self {
        Self::READ_ONLY
    }
}

/// Read-only view onto an issue-tracking backend.
///
/// One implementation per backend (Linear, GitHub Issues, …) plus
/// `MockTracker` for tests. The trait is parameterised over no associated
/// types so it can be erased to `dyn TrackerRead` and stored as
/// `Arc<dyn TrackerRead>` inside the orchestrator's composition root.
///
/// ## Method contracts
///
/// Every method MUST return [`Issue`] values that satisfy the fabrication
/// policy documented on the [`Issue`] type — `branch_name`, `priority`,
/// and `blocked_by` left empty when the source backend does not provide
/// them. The conformance suite enforces this.
///
/// The trait is read-only by design; mutation capability is expressed
/// through the separate [`TrackerMutations`] trait so workflows can detect
/// read-only vs mutation-capable adapters without runtime faking.
#[async_trait]
pub trait TrackerRead: Send + Sync {
    /// Return all issues currently in one of the configured *active*
    /// states for the configured project (SPEC §11.1 op 1,
    /// `fetch_candidate_issues`).
    ///
    /// Active-state filtering happens at the adapter boundary — the
    /// orchestrator will *not* re-filter, so an adapter that returns a
    /// terminal issue from this method is buggy. The conformance suite
    /// enforces this. Ordering is unspecified at the trait level; the
    /// orchestrator applies its own sort (SPEC §8.2) before dispatch.
    async fn fetch_active(&self) -> TrackerResult<Vec<Issue>>;

    /// Refresh the *state* of a known set of issues by id (SPEC §11.1 op 3,
    /// `fetch_issue_states_by_ids`). Used by the reconcile pass on every
    /// poll tick to detect issues that have left the active states while
    /// a worker is mid-flight.
    ///
    /// Adapters MAY return a richer payload than just the state if it's
    /// cheap to do so — the orchestrator only consults the [`IssueState`]
    /// field in the reconcile path, but other fields are fair game for
    /// observability. Adapters MUST NOT silently drop ids they couldn't
    /// resolve; either they appear in the result with their last known
    /// state, or the whole call returns an error. (Partial silent
    /// dropping would let a deleted issue masquerade as still-running.)
    async fn fetch_state(&self, ids: &[IssueId]) -> TrackerResult<Vec<Issue>>;

    /// Return issues that recently entered one of the *terminal* states
    /// (SPEC §11.1 op 2, `fetch_issues_by_states`). Used at startup to
    /// clean up workspaces for issues that finished while Symphony was
    /// offline.
    ///
    /// "Recent" is adapter-defined: a sensible default is "updated within
    /// the last poll-interval window plus a generous buffer". Returning
    /// too many is harmless (the orchestrator deduplicates against its
    /// in-memory state); returning too few risks orphaned workspaces.
    /// The provided `terminal_states` is the operator-configured list
    /// from `WORKFLOW.md`; the adapter compares case-insensitively
    /// (SPEC §11.3).
    async fn fetch_terminal_recent(
        &self,
        terminal_states: &[IssueState],
    ) -> TrackerResult<Vec<Issue>>;

    /// Fetch a single issue by tracker id or human identifier.
    async fn get_issue(&self, _id_or_identifier: &str) -> TrackerResult<Issue> {
        Err(TrackerError::Unsupported(
            "get_issue_details is not supported by this tracker".into(),
        ))
    }

    /// Return tracker comments for an issue.
    async fn list_comments(&self, _id_or_identifier: &str) -> TrackerResult<Vec<IssueComment>> {
        Err(TrackerError::Unsupported(
            "list_comments is not supported by this tracker".into(),
        ))
    }

    /// Return full parent/children/blocker context for an issue.
    async fn get_related(&self, _id_or_identifier: &str) -> TrackerResult<RelatedIssues> {
        Err(TrackerError::Unsupported(
            "get_related_issues is not supported by this tracker".into(),
        ))
    }

    /// Report the adapter's mutation capability bag (SPEC v2 §7.2).
    ///
    /// Workflows query this at the read seam *before* attempting any
    /// mutation dispatch, so even an `Arc<dyn TrackerRead>` is enough to
    /// decide whether autonomous mode is safe or the workflow must fall
    /// back to advisory/proposal mode.
    ///
    /// The default impl returns [`TrackerCapabilities::READ_ONLY`] —
    /// adapters that also implement [`TrackerMutations`] override this
    /// method to flip the relevant flags. Returning capabilities the
    /// adapter cannot fulfil is a contract violation: the orchestrator
    /// will trust the report and dispatch mutations against it.
    fn capabilities(&self) -> TrackerCapabilities {
        TrackerCapabilities::READ_ONLY
    }
}

/// Request to create a new issue in the tracker (SPEC v2 §7.2 "create issue").
///
/// Used by the integration owner during decomposition and by any role
/// filing follow-up issues (`FollowupIssueRequest` lowers into this once
/// approved). Fields use `Option`/`Vec` rather than newtype wrappers so
/// adapters can map them directly onto whatever subset their backend
/// supports without a second translation layer.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CreateIssueRequest {
    /// Issue title. Required by every supported tracker.
    pub title: String,
    /// Markdown/plaintext body. `None` lets the adapter render its own
    /// default (e.g. an empty Linear description vs GitHub's omitted body).
    pub body: Option<String>,
    /// Labels to attach on creation. Adapters compare case-insensitively
    /// (SPEC §11.3) but preserve casing for display.
    pub labels: Vec<String>,
    /// Assignees expressed in adapter-native form (Linear user id, GitHub
    /// login). Empty vec means "do not assign".
    pub assignees: Vec<String>,
    /// Optional parent issue id; adapters that support structural parent
    /// links (Linear sub-issues) set the link atomically with creation,
    /// adapters that don't (GitHub) MUST issue a follow-up
    /// [`LinkParentChildRequest`] rather than silently dropping the link.
    pub parent: Option<IssueId>,
    /// Initial state to place the new issue in. `None` means "tracker
    /// default" (typically the configured first active state).
    pub initial_state: Option<IssueState>,
}

/// Result of [`TrackerMutations::create_issue`]. Carries identifiers the
/// orchestrator persists in `work_items` and uses to hydrate parent/child
/// edges; the adapter-supplied `url` is observability sugar, not load-bearing.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CreateIssueResponse {
    /// Stable tracker-internal id (matches [`Issue::id`]).
    pub id: IssueId,
    /// Human-readable identifier (`ABC-123` for Linear, `#42` for GitHub).
    pub identifier: String,
    /// Tracker-side URL, when the adapter can synthesize one cheaply.
    pub url: Option<String>,
}

/// Request to update an existing issue (SPEC v2 §7.2 "update status",
/// "assign role/agent or equivalent labels").
///
/// All mutation fields are `Option`/`Vec` so the same struct expresses
/// "advance state", "rename title", and "rotate assignees" without a
/// per-operation enum. Empty vecs are no-ops; the adapter MUST NOT
/// interpret an empty `add_labels` as "clear all labels" — `remove_labels`
/// is the only label-removal channel.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct UpdateIssueRequest {
    /// Issue to update.
    pub id: IssueId,
    /// New state, if the caller is advancing workflow status.
    pub state: Option<IssueState>,
    /// New title, if renaming.
    pub title: Option<String>,
    /// New body, if rewriting.
    pub body: Option<String>,
    /// Labels to add. Adapters dedupe against existing labels.
    pub add_labels: Vec<String>,
    /// Labels to remove. Removing a label that isn't attached is a no-op,
    /// not an error.
    pub remove_labels: Vec<String>,
    /// Assignees to add (adapter-native ids).
    pub add_assignees: Vec<String>,
    /// Assignees to remove (adapter-native ids).
    pub remove_assignees: Vec<String>,
}

/// Result of [`TrackerMutations::update_issue`]. The state echo lets the
/// orchestrator confirm the tracker-observed state, which can differ from
/// the requested state when the tracker rewrites unknown values to a
/// configured default.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct UpdateIssueResponse {
    /// Issue that was updated.
    pub id: IssueId,
    /// Authoritative state the tracker reports after the update; `None`
    /// when the request did not include a state change.
    pub state: Option<IssueState>,
}

/// Request to post a comment / activity entry (SPEC v2 §7.2 "add comment").
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AddCommentRequest {
    /// Issue to comment on.
    pub issue: IssueId,
    /// Comment body (markdown where supported).
    pub body: String,
    /// Optional role name for prefix/footer rendering — purely advisory
    /// for adapters that can't impersonate users (most can't).
    pub author_role: Option<String>,
}

/// Result of [`TrackerMutations::add_comment`]. Comment id is optional
/// because some adapters (or label-only proxies) cannot return one.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AddCommentResponse {
    /// Tracker-side comment id, when available.
    pub id: Option<String>,
    /// URL, when the adapter can synthesize one.
    pub url: Option<String>,
}

/// Request to express that one issue blocks another (SPEC v2 §7.2 "create
/// blocker/dependency"). Capability flag `add_blocker` is gated on
/// structural support — label-only proxies MUST report `add_blocker:
/// false` and reject this call rather than silently degrade.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AddBlockerRequest {
    /// Issue that is blocked.
    pub blocked: IssueId,
    /// Issue that is doing the blocking.
    pub blocker: IssueId,
    /// Optional human-readable reason; surfaces on the tracker UI when
    /// the backend supports a description on the blocker edge.
    pub reason: Option<String>,
}

/// Result of [`TrackerMutations::add_blocker`]. The edge id, when
/// available, is what the adapter would use to remove the relationship
/// later — adapters without addressable edges leave it `None` and remove
/// by `(blocked, blocker)` pair instead.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AddBlockerResponse {
    /// Tracker-internal id for the blocker/dependency edge, when
    /// available.
    pub edge_id: Option<String>,
}

/// Request to link a parent and child issue (SPEC v2 §7.2 "link
/// parent/child"). Required for decomposition workflows.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct LinkParentChildRequest {
    /// Parent (broad) issue.
    pub parent: IssueId,
    /// Child issue to attach under the parent.
    pub child: IssueId,
}

/// Result of [`TrackerMutations::link_parent_child`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct LinkParentChildResponse {
    /// Tracker-internal id for the parent/child edge, when available.
    pub edge_id: Option<String>,
}

/// Kind of artifact being attached to an issue (SPEC v2 §7.2 "attach
/// PR/artifact/evidence"). Open-ended `Other` exists because workflows
/// can configure custom evidence kinds (screenshot, design link, etc)
/// without a kernel change.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ArtifactKind {
    /// Pull request reference (URL or `owner/repo#N`).
    PullRequest,
    /// Run log, transcript, or trace produced by an agent run.
    RunLog,
    /// QA verdict evidence (screenshots, command transcripts, etc).
    QaEvidence,
    /// Workflow-specific evidence kind named by the workflow config.
    Other { name: String },
}

/// Request to attach an artifact to an issue (SPEC v2 §7.2 "attach
/// PR/artifact/evidence"). Typical callers are the integration owner
/// (PR ref) and QA (evidence URLs).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AttachArtifactRequest {
    /// Issue the artifact is being attached to.
    pub issue: IssueId,
    /// Discriminator the adapter uses to decide where to render it
    /// (description block, comment, custom field, …).
    pub kind: ArtifactKind,
    /// URI of the artifact. Adapters MUST NOT upload binaries through
    /// this surface; the artifact is hosted elsewhere and only its URI
    /// is recorded on the issue.
    pub uri: String,
    /// Display label / short caption, when the surface supports one.
    pub label: Option<String>,
    /// Free-form note appended near the artifact reference.
    pub note: Option<String>,
}

/// Result of [`TrackerMutations::attach_artifact`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AttachArtifactResponse {
    /// Tracker-side handle for the attachment, when available
    /// (comment id, attachment id, custom-field id…).
    pub id: Option<String>,
}

/// Mutation-side capabilities a tracker MAY support (SPEC v2 §7.2).
///
/// Adapters opt in by implementing this trait. Workflows that decompose
/// parents into children, file blockers, or attach evidence MUST run
/// against a mutation-capable adapter; advisory-only workflows can run
/// with a [`TrackerRead`]-only adapter.
///
/// Each method maps 1:1 to a [`TrackerCapabilities`] flag — an adapter
/// that returns `add_blocker: false` from [`TrackerRead::capabilities`]
/// MUST return a [`TrackerError::Misconfigured`] from
/// [`Self::add_blocker`] rather than silently degrading to a label-only
/// approximation. The orchestrator reads capabilities at the read seam
/// before dispatching, so this is a defense-in-depth check, not a hot
/// path.
///
/// `Send + Sync` for the same dyn-erasure reasons as [`TrackerRead`].
#[async_trait]
pub trait TrackerMutations: Send + Sync {
    /// Create a new issue (SPEC v2 §7.2 "create issue"). Caller MUST have
    /// confirmed [`TrackerCapabilities::create_issue`] beforehand.
    async fn create_issue(&self, request: CreateIssueRequest)
    -> TrackerResult<CreateIssueResponse>;

    /// Update an existing issue's state, title, body, labels, or
    /// assignees (SPEC v2 §7.2 "update status" + "assign role/agent or
    /// equivalent labels"). Caller MUST have confirmed
    /// [`TrackerCapabilities::update_issue`] beforehand.
    async fn update_issue(&self, request: UpdateIssueRequest)
    -> TrackerResult<UpdateIssueResponse>;

    /// Post a comment / activity entry (SPEC v2 §7.2 "add comment").
    /// Caller MUST have confirmed [`TrackerCapabilities::add_comment`]
    /// beforehand.
    async fn add_comment(&self, request: AddCommentRequest) -> TrackerResult<AddCommentResponse>;

    /// Express a structural blocker/dependency edge (SPEC v2 §7.2
    /// "create blocker/dependency"). Adapters that can only proxy this
    /// through labels MUST report `add_blocker: false` and reject this
    /// call.
    async fn add_blocker(&self, request: AddBlockerRequest) -> TrackerResult<AddBlockerResponse>;

    /// Link a child issue to its parent (SPEC v2 §7.2 "link
    /// parent/child"). Required by decomposition workflows.
    async fn link_parent_child(
        &self,
        request: LinkParentChildRequest,
    ) -> TrackerResult<LinkParentChildResponse>;

    /// Attach a PR ref, run log, or QA evidence URI to an issue (SPEC v2
    /// §7.2 "attach PR/artifact/evidence").
    async fn attach_artifact(
        &self,
        request: AttachArtifactRequest,
    ) -> TrackerResult<AttachArtifactResponse>;
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tracker::Issue;
    use std::sync::Arc;

    /// Minimal in-trait-test stub used to confirm the trait is
    /// object-safe and that `Arc<dyn TrackerRead>` round-trips through
    /// the methods. Not a real `MockTracker` — that lives in
    /// `symphony-tracker::mock` once the dedicated checklist item lands.
    struct StubTracker;

    #[async_trait]
    impl TrackerRead for StubTracker {
        async fn fetch_active(&self) -> TrackerResult<Vec<Issue>> {
            Ok(vec![Issue::minimal("id-1", "ABC-1", "stub", "Todo")])
        }

        async fn fetch_state(&self, ids: &[IssueId]) -> TrackerResult<Vec<Issue>> {
            Ok(ids
                .iter()
                .map(|id| Issue::minimal(id.as_str(), id.as_str(), "stub", "Todo"))
                .collect())
        }

        async fn fetch_terminal_recent(
            &self,
            _terminal_states: &[IssueState],
        ) -> TrackerResult<Vec<Issue>> {
            Ok(Vec::new())
        }
    }

    #[tokio::test]
    async fn trait_is_object_safe_and_callable_through_arc_dyn() {
        let tracker: Arc<dyn TrackerRead> = Arc::new(StubTracker);

        let active = tracker.fetch_active().await.unwrap();
        assert_eq!(active.len(), 1);
        assert_eq!(active[0].identifier, "ABC-1");

        let ids = vec![IssueId::new("a"), IssueId::new("b")];
        let refreshed = tracker.fetch_state(&ids).await.unwrap();
        assert_eq!(refreshed.len(), 2);

        let terminal = tracker
            .fetch_terminal_recent(&[IssueState::new("Done")])
            .await
            .unwrap();
        assert!(terminal.is_empty());
    }

    /// Pin the public-surface guarantee that [`TrackerMutations`] is
    /// object-safe, `Send + Sync`, and that every method the orchestrator
    /// wires into can be invoked through `Arc<dyn TrackerMutations>`.
    /// The stub adapter rejects every mutation with `Misconfigured` to
    /// model an adapter that exists in scaffolding but hasn't earned
    /// any capability flags yet.
    #[tokio::test]
    async fn tracker_mutations_is_object_safe_and_every_method_callable_through_dyn() {
        struct StubMutations;

        #[async_trait]
        impl TrackerMutations for StubMutations {
            async fn create_issue(
                &self,
                _request: CreateIssueRequest,
            ) -> TrackerResult<CreateIssueResponse> {
                Err(TrackerError::Misconfigured("stub".into()))
            }
            async fn update_issue(
                &self,
                _request: UpdateIssueRequest,
            ) -> TrackerResult<UpdateIssueResponse> {
                Err(TrackerError::Misconfigured("stub".into()))
            }
            async fn add_comment(
                &self,
                _request: AddCommentRequest,
            ) -> TrackerResult<AddCommentResponse> {
                Err(TrackerError::Misconfigured("stub".into()))
            }
            async fn add_blocker(
                &self,
                _request: AddBlockerRequest,
            ) -> TrackerResult<AddBlockerResponse> {
                Err(TrackerError::Misconfigured("stub".into()))
            }
            async fn link_parent_child(
                &self,
                _request: LinkParentChildRequest,
            ) -> TrackerResult<LinkParentChildResponse> {
                Err(TrackerError::Misconfigured("stub".into()))
            }
            async fn attach_artifact(
                &self,
                _request: AttachArtifactRequest,
            ) -> TrackerResult<AttachArtifactResponse> {
                Err(TrackerError::Misconfigured("stub".into()))
            }
        }

        let mutations: Arc<dyn TrackerMutations> = Arc::new(StubMutations);

        // Each call goes through the vtable; the rejection proves the
        // method actually dispatched rather than being optimized away.
        assert!(matches!(
            mutations
                .create_issue(CreateIssueRequest {
                    title: "t".into(),
                    body: None,
                    labels: vec![],
                    assignees: vec![],
                    parent: None,
                    initial_state: None,
                })
                .await,
            Err(TrackerError::Misconfigured(_))
        ));
        assert!(
            mutations
                .update_issue(UpdateIssueRequest {
                    id: IssueId::new("a"),
                    state: None,
                    title: None,
                    body: None,
                    add_labels: vec![],
                    remove_labels: vec![],
                    add_assignees: vec![],
                    remove_assignees: vec![],
                })
                .await
                .is_err()
        );
        assert!(
            mutations
                .add_comment(AddCommentRequest {
                    issue: IssueId::new("a"),
                    body: "hi".into(),
                    author_role: None,
                })
                .await
                .is_err()
        );
        assert!(
            mutations
                .add_blocker(AddBlockerRequest {
                    blocked: IssueId::new("a"),
                    blocker: IssueId::new("b"),
                    reason: None,
                })
                .await
                .is_err()
        );
        assert!(
            mutations
                .link_parent_child(LinkParentChildRequest {
                    parent: IssueId::new("a"),
                    child: IssueId::new("b"),
                })
                .await
                .is_err()
        );
        assert!(
            mutations
                .attach_artifact(AttachArtifactRequest {
                    issue: IssueId::new("a"),
                    kind: ArtifactKind::PullRequest,
                    uri: "https://example.test/pr/1".into(),
                    label: None,
                    note: None,
                })
                .await
                .is_err()
        );

        fn assert_send_sync<T: Send + Sync + ?Sized>() {}
        assert_send_sync::<dyn TrackerMutations>();
    }

    #[test]
    fn create_issue_request_round_trips_through_serde_with_optional_fields_omitted() {
        let req = CreateIssueRequest {
            title: "decompose payments refactor".into(),
            body: Some("see acceptance criteria".into()),
            labels: vec!["needs-triage".into()],
            assignees: vec!["alice".into()],
            parent: Some(IssueId::new("PARENT-1")),
            initial_state: Some(IssueState::new("Todo")),
        };
        let json = serde_json::to_string(&req).unwrap();
        let back: CreateIssueRequest = serde_json::from_str(&json).unwrap();
        assert_eq!(req, back);
    }

    #[test]
    fn update_issue_request_distinguishes_add_remove_label_channels() {
        let req = UpdateIssueRequest {
            id: IssueId::new("ABC-1"),
            state: Some(IssueState::new("In Review")),
            title: None,
            body: None,
            add_labels: vec!["qa-pass".into()],
            remove_labels: vec!["qa-pending".into()],
            add_assignees: vec![],
            remove_assignees: vec![],
        };
        let json = serde_json::to_string(&req).unwrap();
        let back: UpdateIssueRequest = serde_json::from_str(&json).unwrap();
        assert_eq!(req, back);
        assert_ne!(req.add_labels, req.remove_labels);
    }

    #[test]
    fn artifact_kind_serializes_with_internal_tag_and_supports_other_named_variant() {
        let pr = serde_json::to_value(ArtifactKind::PullRequest).unwrap();
        assert_eq!(pr["type"], "pull_request");

        let other = ArtifactKind::Other {
            name: "design-doc".into(),
        };
        let v = serde_json::to_value(&other).unwrap();
        assert_eq!(v["type"], "other");
        assert_eq!(v["name"], "design-doc");

        let back: ArtifactKind = serde_json::from_value(v).unwrap();
        assert_eq!(back, other);
    }

    #[test]
    fn attach_artifact_request_round_trips_with_optional_label_and_note() {
        let req = AttachArtifactRequest {
            issue: IssueId::new("PR-LINK"),
            kind: ArtifactKind::PullRequest,
            uri: "https://github.com/example/repo/pull/42".into(),
            label: Some("draft PR".into()),
            note: None,
        };
        let json = serde_json::to_string(&req).unwrap();
        let back: AttachArtifactRequest = serde_json::from_str(&json).unwrap();
        assert_eq!(req, back);
    }

    #[test]
    fn add_blocker_and_link_parent_child_distinguish_their_endpoints() {
        // The two requests are structurally similar but semantically
        // distinct — pin that the field names don't accidentally drift
        // into a single shape.
        let blocker = AddBlockerRequest {
            blocked: IssueId::new("downstream"),
            blocker: IssueId::new("upstream"),
            reason: Some("waiting on schema".into()),
        };
        let link = LinkParentChildRequest {
            parent: IssueId::new("EPIC-1"),
            child: IssueId::new("TASK-1"),
        };
        let blocker_json = serde_json::to_value(&blocker).unwrap();
        let link_json = serde_json::to_value(&link).unwrap();
        assert!(blocker_json.get("blocked").is_some());
        assert!(blocker_json.get("blocker").is_some());
        assert!(link_json.get("parent").is_some());
        assert!(link_json.get("child").is_some());
        assert!(link_json.get("blocked").is_none());
    }

    #[test]
    fn add_comment_response_allows_id_and_url_to_be_independently_optional() {
        // Adapter that returns neither (label-only proxy posting through
        // a custom field) is valid; the type permits the empty case.
        let resp = AddCommentResponse {
            id: None,
            url: None,
        };
        let json = serde_json::to_string(&resp).unwrap();
        let back: AddCommentResponse = serde_json::from_str(&json).unwrap();
        assert_eq!(resp, back);
    }

    #[test]
    fn tracker_error_display_includes_variant_context() {
        let e = TrackerError::Transport("connection refused".into());
        assert!(e.to_string().contains("connection refused"));
        assert!(e.to_string().contains("transport"));

        let e = TrackerError::Unauthorized("bad token".into());
        assert!(e.to_string().contains("auth rejected"));

        let e = TrackerError::Malformed("missing field `id`".into());
        assert!(e.to_string().contains("malformed"));

        let e = TrackerError::Misconfigured("project_slug required".into());
        assert!(e.to_string().contains("misconfigured"));
    }

    #[test]
    fn tracker_error_is_send_and_sync_so_it_can_cross_task_boundaries() {
        fn assert_send_sync<T: Send + Sync>() {}
        assert_send_sync::<TrackerError>();
    }

    #[test]
    fn tracker_capabilities_read_only_constant_has_every_flag_off() {
        let caps = TrackerCapabilities::READ_ONLY;
        assert!(caps.is_read_only());
        assert!(!caps.supports_any_mutation());
        assert!(!caps.create_issue);
        assert!(!caps.update_issue);
        assert!(!caps.add_comment);
        assert!(!caps.add_blocker);
        assert!(!caps.link_parent_child);
        assert!(!caps.attach_artifact);
    }

    #[test]
    fn tracker_capabilities_full_constant_has_every_flag_on() {
        let caps = TrackerCapabilities::FULL;
        assert!(!caps.is_read_only());
        assert!(caps.supports_any_mutation());
        assert!(caps.create_issue);
        assert!(caps.update_issue);
        assert!(caps.add_comment);
        assert!(caps.add_blocker);
        assert!(caps.link_parent_child);
        assert!(caps.attach_artifact);
    }

    #[test]
    fn tracker_capabilities_default_matches_read_only_so_new_adapters_are_safe() {
        assert_eq!(
            TrackerCapabilities::default(),
            TrackerCapabilities::READ_ONLY
        );
    }

    #[test]
    fn tracker_capabilities_partial_mutation_is_not_read_only() {
        let mut caps = TrackerCapabilities::READ_ONLY;
        caps.add_comment = true;
        assert!(!caps.is_read_only());
        assert!(caps.supports_any_mutation());
        // Other flags stay off — workflows that need structural blockers
        // can reject this adapter even though comments are available.
        assert!(!caps.add_blocker);
        assert!(!caps.create_issue);
    }

    #[tokio::test]
    async fn default_capabilities_method_reports_read_only_for_read_only_adapter() {
        let tracker: Arc<dyn TrackerRead> = Arc::new(StubTracker);
        let caps = tracker.capabilities();
        assert!(caps.is_read_only());
        assert_eq!(caps, TrackerCapabilities::READ_ONLY);
    }

    #[tokio::test]
    async fn adapter_can_override_capabilities_to_advertise_mutation_support() {
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
                TrackerCapabilities {
                    create_issue: true,
                    add_comment: true,
                    link_parent_child: true,
                    ..TrackerCapabilities::READ_ONLY
                }
            }
        }

        let tracker: Arc<dyn TrackerRead> = Arc::new(MutatingStub);
        let caps = tracker.capabilities();
        assert!(!caps.is_read_only());
        assert!(caps.create_issue);
        assert!(caps.add_comment);
        assert!(caps.link_parent_child);
        assert!(!caps.add_blocker);
        assert!(!caps.update_issue);
        assert!(!caps.attach_artifact);
    }

    #[test]
    fn tracker_capabilities_is_send_sync_and_copy_so_it_crosses_task_boundaries() {
        fn assert_send_sync_copy<T: Send + Sync + Copy>() {}
        assert_send_sync_copy::<TrackerCapabilities>();
    }
}
