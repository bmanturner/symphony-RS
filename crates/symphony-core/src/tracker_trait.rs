//! The [`IssueTracker`] trait — the abstract seam between Symphony and any
//! concrete issue-tracking backend (Linear, GitHub Issues, Jira, …).
//!
//! Per SPEC §11.1, every tracker adapter MUST support three operations:
//! `fetch_candidate_issues`, `fetch_issues_by_states`, and
//! `fetch_issue_states_by_ids`. We expose those as the trait methods
//! [`IssueTracker::fetch_active`], [`IssueTracker::fetch_terminal_recent`],
//! and [`IssueTracker::fetch_state`] respectively. The renames are
//! intentional: the trait is the orchestrator-facing surface, and the
//! shorter names read better at call sites in the poll loop. The doc
//! comments below name the SPEC operation each method implements so a
//! reader cross-checking against SPEC.md can find the mapping immediately.
//!
//! ## Why the trait lives in `symphony-core`
//!
//! Architectural tenet: the orchestrator never speaks Linear (or GitHub or
//! Jira) protocol. It only sees the [`Issue`] model and this trait. Putting
//! the trait alongside `Issue` keeps the abstract layer in one crate and
//! makes the dependency direction obvious — `symphony-tracker` depends on
//! `symphony-core`, never the reverse.
//!
//! ## Async, dynamic dispatch, and `Send + Sync`
//!
//! The orchestrator stores its tracker as `Arc<dyn IssueTracker>` and the
//! poll loop is multi-threaded under tokio. Methods therefore use
//! `async-trait` for object-safe async, and the trait inherits `Send +
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

/// Abstract issue-tracking backend.
///
/// One implementation per backend (Linear, GitHub Issues, Jira, …) plus
/// `MockTracker` for tests. The trait is parameterised over no associated
/// types so it can be erased to `dyn IssueTracker` and stored as
/// `Arc<dyn IssueTracker>` inside the orchestrator's composition root.
///
/// ## Method contracts
///
/// Every method MUST return [`Issue`] values that satisfy the fabrication
/// policy documented on the [`Issue`] type — `branch_name`, `priority`,
/// and `blocked_by` left empty when the source backend does not provide
/// them. The Phase 2 conformance suite enforces this.
///
/// Methods are read-only from the orchestrator's perspective. Symphony
/// does not write back to the tracker (no comments, no state transitions);
/// any future write surface would be a separate trait.
#[async_trait]
pub trait IssueTracker: Send + Sync {
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
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tracker::Issue;
    use std::sync::Arc;

    /// Minimal in-trait-test stub used to confirm the trait is
    /// object-safe and that `Arc<dyn IssueTracker>` round-trips through
    /// the methods. Not a real `MockTracker` — that lives in
    /// `symphony-tracker::mock` once the dedicated checklist item lands.
    struct StubTracker;

    #[async_trait]
    impl IssueTracker for StubTracker {
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
        let tracker: Arc<dyn IssueTracker> = Arc::new(StubTracker);

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
}
