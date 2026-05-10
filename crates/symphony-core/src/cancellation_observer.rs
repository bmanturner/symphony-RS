//! Pure observation helpers that turn a [`CancellationQueue`] lookup
//! into a runner-side decision (SPEC v2 §4.5 / Phase 11.5).
//!
//! Every dispatch runner observes pending cancels at the same two safe
//! points: before lease acquisition, and between agent steps. The shape
//! of that observation is identical across runners — look up the run
//! (and optionally its parent work item), and if a pending request is
//! found, abort cleanly. Centralising the lookup here keeps each runner
//! call site to a single line and gives the kernel one pure, exhaustively
//! tested seam to verify.
//!
//! Callers continue to own locking (typically `Arc<Mutex<CancellationQueue>>`)
//! and the lease/scope/workspace release sequence that follows an
//! `Abort` decision. The primitive does not consume queue entries;
//! `drain_for_run` / `drain_for_work_item` happen after the runner has
//! finished winding the dispatch down so that a panic mid-release does
//! not lose the cancel intent.

use crate::blocker::RunRef;
use crate::cancellation::{CancelRequest, CancellationQueue};
use crate::work_item::WorkItemId;

/// Outcome of a pre-flight (or between-step) cancellation observation.
///
/// The `Abort` variant carries an owned [`CancelRequest`] clone so the
/// caller can persist or log the reason without holding the queue lock
/// across the lease-release path.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CancellationDecision {
    /// No pending cancel for this subject. The runner may proceed.
    Proceed,
    /// A pending cancel was observed. The runner must release any held
    /// lease/scope permits/workspace claim, transition the run row to
    /// `Cancelled` via the typed status gate, and then `drain_for_run`
    /// (or `drain_for_work_item`, depending on the matched subject).
    Abort(CancelRequest),
}

impl CancellationDecision {
    /// True when the decision is `Abort`. Lets callers gate without
    /// pattern matching when they only need the boolean.
    pub fn should_abort(&self) -> bool {
        matches!(self, Self::Abort(_))
    }

    /// Borrow the underlying request when the decision is `Abort`.
    pub fn request(&self) -> Option<&CancelRequest> {
        match self {
            Self::Abort(req) => Some(req),
            Self::Proceed => None,
        }
    }
}

/// Observe whether a pending cancel exists for `run_id`.
///
/// This is the canonical pre-lease and between-step check for runners
/// that have already reserved a run row. The lookup is non-consuming;
/// the runner is expected to call [`CancellationQueue::drain_for_run`]
/// only after the lease and scope permits have been released and the
/// status transition has been recorded.
pub fn observe_for_run(queue: &CancellationQueue, run_id: RunRef) -> CancellationDecision {
    match queue.pending_for_run(run_id) {
        Some(req) => CancellationDecision::Abort(req.clone()),
        None => CancellationDecision::Proceed,
    }
}

/// Observe whether a pending cancel exists for `run_id` *or* for the
/// supplied parent `work_item_id` (when present). The run-keyed
/// keyspace is checked first; a parent cancel is only consulted when
/// the run itself has no direct pending request.
///
/// Runners that have a stable parent work-item id at observation time
/// (specialist, integration, QA, follow-up approval) use this entry
/// point so a parent cancel that has not yet been fanned out by the
/// propagator still short-circuits a fresh dispatch. Runners without
/// a parent context (budget pause, recovery) call [`observe_for_run`]
/// instead.
pub fn observe_for_run_or_parent(
    queue: &CancellationQueue,
    run_id: RunRef,
    work_item_id: Option<WorkItemId>,
) -> CancellationDecision {
    if let Some(req) = queue.pending_for_run(run_id) {
        return CancellationDecision::Abort(req.clone());
    }
    if let Some(work_item_id) = work_item_id
        && let Some(req) = queue.pending_for_work_item(work_item_id)
    {
        return CancellationDecision::Abort(req.clone());
    }
    CancellationDecision::Proceed
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::cancellation::CancelRequest;

    fn run_req(id: i64, reason: &str) -> CancelRequest {
        CancelRequest::for_run(RunRef::new(id), reason, "tester", "2026-05-09T00:00:00Z")
    }

    fn wi_req(id: i64, reason: &str) -> CancelRequest {
        CancelRequest::for_work_item(
            WorkItemId::new(id),
            reason,
            "tester",
            "2026-05-09T00:00:00Z",
        )
    }

    #[test]
    fn observe_for_run_returns_proceed_when_queue_is_empty() {
        let queue = CancellationQueue::new();
        let decision = observe_for_run(&queue, RunRef::new(1));
        assert_eq!(decision, CancellationDecision::Proceed);
        assert!(!decision.should_abort());
        assert!(decision.request().is_none());
    }

    #[test]
    fn observe_for_run_returns_abort_with_cloned_request() {
        let mut queue = CancellationQueue::new();
        queue.enqueue(run_req(1, "operator cancel"));

        let decision = observe_for_run(&queue, RunRef::new(1));
        match &decision {
            CancellationDecision::Abort(req) => assert_eq!(req.reason, "operator cancel"),
            other => panic!("expected Abort, got {other:?}"),
        }
        assert!(decision.should_abort());

        // Observation is non-consuming — repeated calls keep returning Abort.
        let again = observe_for_run(&queue, RunRef::new(1));
        assert!(again.should_abort());
    }

    #[test]
    fn observe_for_run_ignores_unrelated_run_entries() {
        let mut queue = CancellationQueue::new();
        queue.enqueue(run_req(2, "other"));
        let decision = observe_for_run(&queue, RunRef::new(1));
        assert_eq!(decision, CancellationDecision::Proceed);
    }

    #[test]
    fn observe_for_run_ignores_work_item_entries() {
        let mut queue = CancellationQueue::new();
        queue.enqueue(wi_req(1, "parent"));
        // Same numeric id, but observe_for_run only consults the run keyspace.
        let decision = observe_for_run(&queue, RunRef::new(1));
        assert_eq!(decision, CancellationDecision::Proceed);
    }

    #[test]
    fn observe_for_run_or_parent_proceeds_when_neither_keyspace_matches() {
        let queue = CancellationQueue::new();
        let decision = observe_for_run_or_parent(&queue, RunRef::new(1), Some(WorkItemId::new(7)));
        assert_eq!(decision, CancellationDecision::Proceed);
    }

    #[test]
    fn observe_for_run_or_parent_prefers_run_keyspace_over_parent() {
        let mut queue = CancellationQueue::new();
        queue.enqueue(run_req(1, "run cancel"));
        queue.enqueue(wi_req(7, "parent cancel"));

        let decision = observe_for_run_or_parent(&queue, RunRef::new(1), Some(WorkItemId::new(7)));
        match decision {
            CancellationDecision::Abort(req) => {
                assert_eq!(req.reason, "run cancel", "run keyspace must win");
            }
            other => panic!("expected Abort, got {other:?}"),
        }
    }

    #[test]
    fn observe_for_run_or_parent_falls_back_to_work_item_when_run_is_clean() {
        let mut queue = CancellationQueue::new();
        queue.enqueue(wi_req(7, "parent cancel"));

        let decision = observe_for_run_or_parent(&queue, RunRef::new(1), Some(WorkItemId::new(7)));
        match decision {
            CancellationDecision::Abort(req) => assert_eq!(req.reason, "parent cancel"),
            other => panic!("expected Abort, got {other:?}"),
        }
    }

    #[test]
    fn observe_for_run_or_parent_with_no_parent_context_only_checks_run_keyspace() {
        let mut queue = CancellationQueue::new();
        queue.enqueue(wi_req(1, "parent cancel"));

        let decision = observe_for_run_or_parent(&queue, RunRef::new(1), None);
        assert_eq!(decision, CancellationDecision::Proceed);
    }

    #[test]
    fn observe_returns_owned_clone_independent_of_queue_state() {
        let mut queue = CancellationQueue::new();
        queue.enqueue(run_req(1, "first"));

        let CancellationDecision::Abort(snapshot) = observe_for_run(&queue, RunRef::new(1)) else {
            panic!("expected Abort");
        };
        // Replace the in-queue payload — the previously observed clone
        // must not change.
        queue.enqueue(run_req(1, "second"));
        assert_eq!(snapshot.reason, "first");

        // And a fresh observation now sees the new payload.
        let CancellationDecision::Abort(fresh) = observe_for_run(&queue, RunRef::new(1)) else {
            panic!("expected Abort");
        };
        assert_eq!(fresh.reason, "second");
    }

    #[test]
    fn decision_helpers_round_trip() {
        let proceed = CancellationDecision::Proceed;
        assert!(!proceed.should_abort());
        assert!(proceed.request().is_none());

        let abort = CancellationDecision::Abort(run_req(1, "x"));
        assert!(abort.should_abort());
        assert_eq!(abort.request().map(|r| r.reason.as_str()), Some("x"));
    }
}
