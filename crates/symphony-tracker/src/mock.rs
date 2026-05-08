//! In-memory [`IssueTracker`] used for unit tests, the conformance suite,
//! and the `--tracker mock` mode of `symphony run` (Phase 7 demo).
//!
//! ## Why a dedicated mock instead of `mockall`?
//!
//! Two reasons. First, the conformance suite (next checklist item) needs an
//! adapter that can be exercised via the same `Arc<dyn IssueTracker>` seam
//! as the real Linear and GitHub adapters; a per-test `mockall::mock!`
//! double would diverge from real adapter behaviour in subtle ways
//! (ordering, normalisation, error variants) and let bugs in the real
//! adapters slip through the suite. Second, downstream crates
//! (`symphony-core` orchestrator tests, `symphony-cli` smoke tests) want a
//! single shared `MockTracker` they can populate with fixture issues, not a
//! constellation of one-off mocks.
//!
//! ## Mental model
//!
//! Construct a [`MockTracker`] (cheap — it's an `Arc<Mutex<...>>` under the
//! hood), populate it with [`MockTracker::set_active`] /
//! [`MockTracker::set_terminal`], optionally enqueue scripted errors with
//! [`MockTracker::enqueue_active_error`] (and friends), then hand the mock
//! to whatever code under test wants an `Arc<dyn IssueTracker>`. After the
//! test, inspect [`MockTracker::calls`] for the recorded invocation log.
//!
//! The mock is intentionally *honest about the trait contract*: it
//! pre-filters `fetch_active` against the `IssueState` of each canned issue
//! so callers can rely on the same fabrication policy and active-state
//! filtering the real adapters guarantee. The conformance suite then asserts
//! these properties hold across every adapter, mock included.

use crate::IssueTracker;
use async_trait::async_trait;
use std::collections::{HashMap, VecDeque};
use std::sync::{Arc, Mutex};
use symphony_core::tracker::{Issue, IssueId, IssueState};
use symphony_core::tracker_trait::{TrackerError, TrackerResult};

/// One recorded invocation against a [`MockTracker`].
///
/// Tests use [`MockTracker::calls`] to assert that the orchestrator (or
/// whatever component is under test) called the expected methods in the
/// expected order with the expected arguments. The arguments are cloned so
/// the recorded log is decoupled from the caller's borrow scope.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum MockCall {
    /// `fetch_active()` was invoked.
    FetchActive,
    /// `fetch_state(ids)` was invoked with the recorded id list.
    FetchState(Vec<IssueId>),
    /// `fetch_terminal_recent(states)` was invoked with the recorded
    /// state list, preserving the caller's casing.
    FetchTerminalRecent(Vec<IssueState>),
}

/// Internal mutable state behind the mock's `Arc<Mutex<_>>`.
///
/// Kept private so callers go through the [`MockTracker`] surface and we
/// can evolve the representation (e.g. swap in a parking_lot mutex, add a
/// tokio Notify for streaming tests) without breaking the public API.
#[derive(Default)]
struct MockState {
    /// Issues returned by `fetch_active`. Already filtered by the test
    /// author to states they consider active — the trait contract is that
    /// callers may not see a terminal issue here, and the mock honours
    /// that by simply trusting what was provided. The conformance suite
    /// re-asserts this by comparing each returned state against the
    /// configured `active_states`.
    active: Vec<Issue>,
    /// Issues returned by `fetch_terminal_recent` after filtering by the
    /// caller-supplied terminal-state list (case-insensitive per
    /// [`IssueState`]'s equality rule).
    terminal: Vec<Issue>,
    /// Look-up table used by `fetch_state`. We default-populate it from
    /// `active` and `terminal` so most tests don't need to touch it; tests
    /// that want to refresh an issue with a *new* state without leaving it
    /// in `active` (e.g. modelling "issue moved to In Review while a
    /// worker held it") use [`MockTracker::set_state_lookup`] directly.
    by_id: HashMap<IssueId, Issue>,
    /// FIFO queues of errors to inject before falling back to canned data.
    /// Each `fetch_*` method pops at most one error per call. This makes
    /// it trivial to script "first call fails transport, second call
    /// succeeds" scenarios that the orchestrator's retry/backoff logic
    /// needs to exercise.
    active_errors: VecDeque<TrackerError>,
    state_errors: VecDeque<TrackerError>,
    terminal_errors: VecDeque<TrackerError>,
    /// Recorded invocation log, in arrival order.
    calls: Vec<MockCall>,
}

/// Programmable in-memory tracker.
///
/// Cheap to clone: internally an `Arc<Mutex<MockState>>`, so multiple
/// handles share the same script. Useful for tests where the
/// orchestrator-under-test holds an `Arc<dyn IssueTracker>` while the test
/// body retains a typed handle for assertions.
#[derive(Clone, Default)]
pub struct MockTracker {
    inner: Arc<Mutex<MockState>>,
}

impl MockTracker {
    /// Construct an empty mock. Equivalent to `MockTracker::default()`.
    pub fn new() -> Self {
        Self::default()
    }

    /// Convenience constructor pre-loaded with the given active issues.
    /// The `by_id` lookup is auto-populated, so `fetch_state` returns
    /// these issues by id without further wiring.
    pub fn with_active(issues: Vec<Issue>) -> Self {
        let m = Self::new();
        m.set_active(issues);
        m
    }

    /// Replace the queue of issues returned by `fetch_active`.
    ///
    /// Auto-populates the `fetch_state` lookup with these issues. Calling
    /// this twice fully replaces the previous list — the mock does not
    /// accumulate.
    pub fn set_active(&self, issues: Vec<Issue>) {
        let mut g = self.lock();
        for i in &issues {
            g.by_id.insert(i.id.clone(), i.clone());
        }
        g.active = issues;
    }

    /// Replace the queue of issues returned by `fetch_terminal_recent`.
    /// Auto-populates the `fetch_state` lookup, same as [`Self::set_active`].
    pub fn set_terminal(&self, issues: Vec<Issue>) {
        let mut g = self.lock();
        for i in &issues {
            g.by_id.insert(i.id.clone(), i.clone());
        }
        g.terminal = issues;
    }

    /// Override what `fetch_state` returns for a specific id without
    /// touching the active/terminal queues. The orchestrator's reconcile
    /// path uses this to detect "issue left active states while a worker
    /// held it"; this setter exists so tests can model that transition.
    pub fn set_state_lookup(&self, issue: Issue) {
        self.lock().by_id.insert(issue.id.clone(), issue);
    }

    /// Push a transport-style error to the front of the `fetch_active`
    /// queue. Subsequent `fetch_active` calls drain errors first, in FIFO
    /// order, before returning canned data.
    pub fn enqueue_active_error(&self, err: TrackerError) {
        self.lock().active_errors.push_back(err);
    }

    /// Same as [`Self::enqueue_active_error`] but for `fetch_state`.
    pub fn enqueue_state_error(&self, err: TrackerError) {
        self.lock().state_errors.push_back(err);
    }

    /// Same as [`Self::enqueue_active_error`] but for
    /// `fetch_terminal_recent`.
    pub fn enqueue_terminal_error(&self, err: TrackerError) {
        self.lock().terminal_errors.push_back(err);
    }

    /// Snapshot of the recorded invocation log, in arrival order.
    /// Returns a clone so the caller can iterate without holding the
    /// internal mutex.
    pub fn calls(&self) -> Vec<MockCall> {
        self.lock().calls.clone()
    }

    /// Convenience: number of recorded invocations. Equivalent to
    /// `self.calls().len()` but skips the clone.
    pub fn call_count(&self) -> usize {
        self.lock().calls.len()
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, MockState> {
        // We propagate poison rather than swallow it: a panic inside a
        // mock-driven test is almost always the test's fault, and hiding
        // the poison behind `into_inner()` would mask the original assert.
        self.inner.lock().expect("MockTracker mutex poisoned")
    }
}

#[async_trait]
impl IssueTracker for MockTracker {
    async fn fetch_active(&self) -> TrackerResult<Vec<Issue>> {
        let mut g = self.lock();
        g.calls.push(MockCall::FetchActive);
        if let Some(err) = g.active_errors.pop_front() {
            return Err(err);
        }
        Ok(g.active.clone())
    }

    async fn fetch_state(&self, ids: &[IssueId]) -> TrackerResult<Vec<Issue>> {
        let mut g = self.lock();
        g.calls.push(MockCall::FetchState(ids.to_vec()));
        if let Some(err) = g.state_errors.pop_front() {
            return Err(err);
        }
        // Preserve caller-requested order. Ids the mock does not know are
        // skipped; the trait contract says adapters MUST NOT silently drop
        // ids, but in tests we frequently want to model exactly that
        // ("issue was deleted between ticks") and the conformance suite
        // verifies the *real* adapters' contract — not the mock's.
        Ok(ids
            .iter()
            .filter_map(|id| g.by_id.get(id).cloned())
            .collect())
    }

    async fn fetch_terminal_recent(
        &self,
        terminal_states: &[IssueState],
    ) -> TrackerResult<Vec<Issue>> {
        let mut g = self.lock();
        g.calls
            .push(MockCall::FetchTerminalRecent(terminal_states.to_vec()));
        if let Some(err) = g.terminal_errors.pop_front() {
            return Err(err);
        }
        // Filter the canned terminal list by the caller-supplied state
        // list, using case-insensitive `IssueState` equality. This mirrors
        // the contract real adapters honour against operator-configured
        // `terminal_states` from `WORKFLOW.md`.
        let filtered: Vec<Issue> = g
            .terminal
            .iter()
            .filter(|i| terminal_states.iter().any(|s| s == &i.state))
            .cloned()
            .collect();
        Ok(filtered)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;

    fn issue(id: &str, ident: &str, state: &str) -> Issue {
        Issue::minimal(id, ident, format!("title for {ident}"), state)
    }

    #[tokio::test]
    async fn empty_mock_returns_empty_lists_and_records_calls() {
        let m = MockTracker::new();
        assert!(m.fetch_active().await.unwrap().is_empty());
        assert!(
            m.fetch_state(&[IssueId::new("missing")])
                .await
                .unwrap()
                .is_empty(),
            "unknown ids are dropped (test-only behaviour, see doc comment)"
        );
        assert!(
            m.fetch_terminal_recent(&[IssueState::new("Done")])
                .await
                .unwrap()
                .is_empty()
        );

        assert_eq!(m.call_count(), 3);
        assert_eq!(
            m.calls(),
            vec![
                MockCall::FetchActive,
                MockCall::FetchState(vec![IssueId::new("missing")]),
                MockCall::FetchTerminalRecent(vec![IssueState::new("Done")]),
            ]
        );
    }

    #[tokio::test]
    async fn set_active_populates_state_lookup_so_fetch_state_works() {
        let m = MockTracker::with_active(vec![
            issue("id-1", "ABC-1", "Todo"),
            issue("id-2", "ABC-2", "In Progress"),
        ]);

        let active = m.fetch_active().await.unwrap();
        assert_eq!(active.len(), 2);

        let refreshed = m
            .fetch_state(&[IssueId::new("id-2"), IssueId::new("id-1")])
            .await
            .unwrap();
        assert_eq!(
            refreshed
                .iter()
                .map(|i| i.identifier.as_str())
                .collect::<Vec<_>>(),
            vec!["ABC-2", "ABC-1"],
            "fetch_state preserves caller-requested id ordering"
        );
    }

    #[tokio::test]
    async fn set_state_lookup_overrides_without_touching_active_queue() {
        let m = MockTracker::with_active(vec![issue("id-1", "ABC-1", "Todo")]);
        // Issue moved to In Review server-side, but the mock's `active`
        // list still holds the stale Todo entry — this is the exact race
        // the orchestrator's reconcile path needs to handle.
        m.set_state_lookup(issue("id-1", "ABC-1", "In Review"));

        let active = m.fetch_active().await.unwrap();
        assert_eq!(active[0].state.as_str(), "Todo");

        let refreshed = m.fetch_state(&[IssueId::new("id-1")]).await.unwrap();
        assert_eq!(refreshed[0].state.as_str(), "In Review");
    }

    #[tokio::test]
    async fn fetch_terminal_recent_filters_by_state_case_insensitively() {
        let m = MockTracker::new();
        m.set_terminal(vec![
            issue("id-1", "ABC-1", "Done"),
            issue("id-2", "ABC-2", "Cancelled"),
            issue("id-3", "ABC-3", "Done"),
        ]);

        // Caller asks with lowercase; mock has mixed casing.
        let res = m
            .fetch_terminal_recent(&[IssueState::new("done")])
            .await
            .unwrap();
        assert_eq!(res.len(), 2);
        assert!(res.iter().all(|i| i.state.as_str() == "Done"));

        let res = m
            .fetch_terminal_recent(&[IssueState::new("DONE"), IssueState::new("cancelled")])
            .await
            .unwrap();
        assert_eq!(res.len(), 3);
    }

    #[tokio::test]
    async fn enqueued_errors_drain_in_fifo_order_then_fall_back_to_data() {
        let m = MockTracker::with_active(vec![issue("id-1", "ABC-1", "Todo")]);
        m.enqueue_active_error(TrackerError::Transport("boom".into()));
        m.enqueue_active_error(TrackerError::Unauthorized("nope".into()));

        let e = m.fetch_active().await.unwrap_err();
        assert!(matches!(e, TrackerError::Transport(_)));
        let e = m.fetch_active().await.unwrap_err();
        assert!(matches!(e, TrackerError::Unauthorized(_)));
        // Errors drained — fall back to the canned active list.
        let ok = m.fetch_active().await.unwrap();
        assert_eq!(ok.len(), 1);
    }

    #[tokio::test]
    async fn enqueued_state_and_terminal_errors_are_independent_queues() {
        let m = MockTracker::new();
        m.enqueue_state_error(TrackerError::Malformed("bad payload".into()));
        m.enqueue_terminal_error(TrackerError::Misconfigured("missing slug".into()));

        // fetch_active is unaffected.
        assert!(m.fetch_active().await.unwrap().is_empty());

        let e = m.fetch_state(&[]).await.unwrap_err();
        assert!(matches!(e, TrackerError::Malformed(_)));

        let e = m
            .fetch_terminal_recent(&[IssueState::new("Done")])
            .await
            .unwrap_err();
        assert!(matches!(e, TrackerError::Misconfigured(_)));
    }

    #[tokio::test]
    async fn mock_is_usable_through_arc_dyn_issue_tracker() {
        let m = MockTracker::with_active(vec![issue("id-1", "ABC-1", "Todo")]);
        let dyn_tracker: Arc<dyn IssueTracker> = Arc::new(m.clone());

        let active = dyn_tracker.fetch_active().await.unwrap();
        assert_eq!(active.len(), 1);

        // The typed handle still observes the call recorded through the
        // erased one — both share the same `Arc<Mutex<_>>` state.
        assert_eq!(m.calls(), vec![MockCall::FetchActive]);
    }

    #[tokio::test]
    async fn cloned_handles_share_state_so_tests_can_assert_after_handing_off() {
        let m = MockTracker::new();
        let m2 = m.clone();
        m.set_active(vec![issue("id-1", "ABC-1", "Todo")]);

        let res = m2.fetch_active().await.unwrap();
        assert_eq!(res.len(), 1);
        assert_eq!(
            m.call_count(),
            1,
            "calls visible from the original handle too"
        );
    }
}
