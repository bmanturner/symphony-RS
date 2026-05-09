//! Tracker traits — the abstract seam between Symphony and any concrete
//! issue-tracking backend (Linear, GitHub Issues, …).
//!
//! v2 splits the previous monolithic `TrackerRead` into two object-safe
//! halves per SPEC v2 §7.1 and §7.2:
//!
//! * [`TrackerRead`] — read-only candidate/state/recovery queries the
//!   poll loop relies on. All current adapters MUST implement this.
//! * [`TrackerMutations`] — write-side capabilities (create issue, comment,
//!   add blocker, link parent/child, attach artifact). Adapters opt in;
//!   workflows that require mutations check capability before dispatching.
//!   The trait surface is intentionally a marker here — request/response
//!   types and methods land in a follow-up checklist item.
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

use crate::tracker::{Issue, IssueId, IssueState};
use async_trait::async_trait;
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

/// Mutation-side capabilities a tracker MAY support (SPEC v2 §7.2).
///
/// Adapters opt in by implementing this trait. Workflows that decompose
/// parents into children, file blockers, or attach evidence MUST run
/// against a mutation-capable adapter; advisory-only workflows can run
/// with a [`TrackerRead`]-only adapter.
///
/// This trait is intentionally a marker right now: request/response types
/// and the actual `create_issue` / `add_comment` / `add_blocker` /
/// `link_parent_child` / `attach_artifact` methods land in a dedicated
/// follow-up checklist item ("Add mutation request/response types"). The
/// type is exposed in the public surface today so adapter scaffolding,
/// capability reporting, and downstream code can already refer to it
/// without a churn-cycle later.
///
/// `Send + Sync` for the same dyn-erasure reasons as [`TrackerRead`].
#[async_trait]
pub trait TrackerMutations: Send + Sync {}

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

    /// `TrackerMutations` is intentionally a marker today; this test pins
    /// the public-surface guarantee that the trait exists, is object-safe,
    /// and inherits `Send + Sync` so adapters can scaffold against it
    /// before request/response types land.
    #[test]
    fn tracker_mutations_is_object_safe_and_send_sync() {
        struct ReadOnlyAdapter;
        #[async_trait]
        impl TrackerMutations for ReadOnlyAdapter {}

        let _erased: Arc<dyn TrackerMutations> = Arc::new(ReadOnlyAdapter);
        fn assert_send_sync<T: Send + Sync + ?Sized>() {}
        assert_send_sync::<dyn TrackerMutations>();
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
