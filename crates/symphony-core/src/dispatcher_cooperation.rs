//! Dispatcher-level cooperative cancellation cooperation point
//! (SPEC v2 §4.5 / Phase 11.5).
//!
//! Pre-lease cancellation observation lives in
//! [`crate::cancellation_observer`]: every dispatch runner consults the
//! shared [`CancellationQueue`] before reserving a lease so an aborted
//! dispatch never holds a lease at all. That covers the *gap between
//! intake and dispatch* — but not the *gap between agent steps* inside a
//! long-running dispatch.
//!
//! Long-running dispatches are dominated by an outer loop that drains
//! the [`crate::AgentEvent`] stream until a terminal event arrives. A
//! cancel that lands while the agent is mid-turn must be observed at
//! the next safe seam between steps, otherwise the dispatcher would
//! happily run a multi-minute turn after the operator already cancelled.
//!
//! This module defines a tiny, lock-aware primitive — [`DispatcherCancellationCheck`]
//! — that captures the dispatch context (the durable run id, plus the
//! optional parent work-item id when the runner has one) and exposes a
//! single asynchronous `check` method that:
//!
//! 1. Locks the shared [`CancellationQueue`] briefly.
//! 2. Delegates to [`crate::cancellation_observer::observe_for_run_or_parent`]
//!    so the keyspace precedence (`run_id` wins over `work_item_id`)
//!    matches the pre-lease path bit-for-bit.
//! 3. Drops the lock before returning the [`CancellationDecision`] so
//!    the caller never holds the queue lock while winding the dispatch
//!    down.
//!
//! The primitive is intentionally *non-consuming*. Drain semantics — and
//! the lease/scope/workspace release order — stay the responsibility of
//! the caller, exactly the same way the pre-lease path handles them.
//! Centralising the lock-and-look pattern here means a future
//! `Dispatcher` impl in `symphony-cli` (or in any other dispatch surface
//! that drives an [`crate::AgentRunner`] event stream directly) can call
//! `check().await` between events without re-implementing the keyspace
//! precedence rules.
//!
//! ## Where this is called
//!
//! Once a dispatch acquires a lease and starts streaming agent events,
//! the loop should call [`DispatcherCancellationCheck::check`] before
//! processing each non-terminal event. Terminal events
//! ([`crate::AgentEvent::is_terminal`]) end the stream regardless of
//! cancel state, so the check can be skipped for them — though calling
//! it once more on terminal is harmless.
//!
//! Calling between *every* event keeps the latency from "operator hits
//! cancel" to "dispatcher releases lease" bounded by one model turn,
//! which is the same upper bound the SPEC commits to for cooperative
//! cancellation. Higher-frequency checks (e.g. on a wall-clock timer
//! independent of the event stream) are not needed at this layer; the
//! pre-lease path already protects the lease itself.

use std::sync::Arc;

use tokio::sync::Mutex;

use crate::blocker::RunRef;
use crate::cancellation::CancellationQueue;
use crate::cancellation_observer::{CancellationDecision, observe_for_run_or_parent};
use crate::work_item::WorkItemId;

/// Dispatcher-side cancellation cooperation seam.
///
/// Constructed once per dispatch (from the run id and optional parent
/// work-item id reserved by the runner that hands work to the
/// dispatcher), and called between agent steps to decide whether the
/// dispatch should proceed or wind down. Cheap to clone — the inner
/// [`Arc<Mutex<CancellationQueue>>`] is the same handle the runner
/// already holds.
#[derive(Debug, Clone)]
pub struct DispatcherCancellationCheck {
    queue: Arc<Mutex<CancellationQueue>>,
    run_id: RunRef,
    parent_work_item_id: Option<WorkItemId>,
}

impl DispatcherCancellationCheck {
    /// Construct a cooperation check bound to a specific dispatch.
    ///
    /// `parent_work_item_id` is `None` when the dispatcher has no
    /// stable parent context (recovery, budget pause). When `Some`, a
    /// pending work-item-keyed cancel that has not yet been fanned out
    /// by the propagator still short-circuits this dispatch on the
    /// next check.
    pub fn new(
        queue: Arc<Mutex<CancellationQueue>>,
        run_id: RunRef,
        parent_work_item_id: Option<WorkItemId>,
    ) -> Self {
        Self {
            queue,
            run_id,
            parent_work_item_id,
        }
    }

    /// The run id this cooperation check is bound to.
    pub fn run_id(&self) -> RunRef {
        self.run_id
    }

    /// The optional parent work-item id this cooperation check
    /// considers in addition to the run-keyed entry.
    pub fn parent_work_item_id(&self) -> Option<WorkItemId> {
        self.parent_work_item_id
    }

    /// Observe the queue and return a [`CancellationDecision`].
    ///
    /// Locks the queue briefly. The decision is non-consuming — drain
    /// is still the caller's responsibility, performed after the
    /// lease, scope permits, and workspace claim have been released
    /// (so a panic during release does not lose the cancel intent).
    pub async fn check(&self) -> CancellationDecision {
        let queue = self.queue.lock().await;
        observe_for_run_or_parent(&queue, self.run_id, self.parent_work_item_id)
    }

    /// Convenience wrapper: returns `true` when the next dispatcher
    /// step should be skipped because a cancel is pending. Useful for
    /// loops that only need the boolean and otherwise discard the
    /// payload (e.g. when the runner that owns the dispatch will
    /// re-observe the queue itself to decide the wind-down path).
    pub async fn should_abort(&self) -> bool {
        self.check().await.should_abort()
    }
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

    fn check_for(
        queue: Arc<Mutex<CancellationQueue>>,
        run: i64,
        parent: Option<i64>,
    ) -> DispatcherCancellationCheck {
        DispatcherCancellationCheck::new(queue, RunRef::new(run), parent.map(WorkItemId::new))
    }

    #[tokio::test]
    async fn proceeds_when_queue_is_empty() {
        let queue = Arc::new(Mutex::new(CancellationQueue::new()));
        let check = check_for(queue, 1, Some(7));
        assert_eq!(check.check().await, CancellationDecision::Proceed);
        assert!(!check.should_abort().await);
    }

    #[tokio::test]
    async fn aborts_when_run_keyed_cancel_arrives_between_steps() {
        let queue = Arc::new(Mutex::new(CancellationQueue::new()));
        let check = check_for(queue.clone(), 1, None);

        // First step: clean queue → proceed.
        assert_eq!(check.check().await, CancellationDecision::Proceed);

        // Operator enqueues a cancel between agent events.
        queue.lock().await.enqueue(run_req(1, "operator cancel"));

        // Next inter-step check observes it.
        match check.check().await {
            CancellationDecision::Abort(req) => assert_eq!(req.reason, "operator cancel"),
            other => panic!("expected Abort, got {other:?}"),
        }
        assert!(check.should_abort().await);
    }

    #[tokio::test]
    async fn aborts_when_parent_work_item_cancel_arrives_between_steps() {
        let queue = Arc::new(Mutex::new(CancellationQueue::new()));
        let check = check_for(queue.clone(), 1, Some(7));

        assert_eq!(check.check().await, CancellationDecision::Proceed);

        queue.lock().await.enqueue(wi_req(7, "parent cancel"));

        match check.check().await {
            CancellationDecision::Abort(req) => assert_eq!(req.reason, "parent cancel"),
            other => panic!("expected Abort, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn run_keyspace_wins_over_parent_keyspace() {
        let queue = Arc::new(Mutex::new(CancellationQueue::new()));
        {
            let mut q = queue.lock().await;
            q.enqueue(run_req(1, "run cancel"));
            q.enqueue(wi_req(7, "parent cancel"));
        }
        let check = check_for(queue, 1, Some(7));
        match check.check().await {
            CancellationDecision::Abort(req) => assert_eq!(req.reason, "run cancel"),
            other => panic!("expected Abort, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn ignores_parent_cancel_when_no_parent_context_supplied() {
        let queue = Arc::new(Mutex::new(CancellationQueue::new()));
        queue.lock().await.enqueue(wi_req(7, "parent cancel"));

        let check = check_for(queue, 1, None);
        assert_eq!(check.check().await, CancellationDecision::Proceed);
    }

    #[tokio::test]
    async fn check_is_non_consuming_so_repeated_calls_keep_observing() {
        let queue = Arc::new(Mutex::new(CancellationQueue::new()));
        queue.lock().await.enqueue(run_req(1, "cancel"));
        let check = check_for(queue.clone(), 1, None);
        for _ in 0..3 {
            assert!(check.should_abort().await, "queue must remain pending");
        }
        // And the queue still holds the entry.
        assert!(queue.lock().await.pending_for_run(RunRef::new(1)).is_some());
    }

    #[tokio::test]
    async fn ignores_unrelated_run_entries() {
        let queue = Arc::new(Mutex::new(CancellationQueue::new()));
        queue.lock().await.enqueue(run_req(2, "other"));
        let check = check_for(queue, 1, None);
        assert_eq!(check.check().await, CancellationDecision::Proceed);
    }

    #[tokio::test]
    async fn does_not_hold_queue_lock_across_decision_return() {
        // If `check` accidentally held the queue lock while the caller
        // processed the decision, a second checker (or the runner
        // wind-down path) would deadlock the moment it tried to lock
        // the queue again. Drive that scenario explicitly.
        let queue = Arc::new(Mutex::new(CancellationQueue::new()));
        queue.lock().await.enqueue(run_req(1, "cancel"));
        let check = check_for(queue.clone(), 1, None);

        let decision = check.check().await;
        assert!(decision.should_abort());

        // After the decision returns, an unrelated lock acquisition
        // must succeed immediately without timing out.
        let acquired =
            tokio::time::timeout(std::time::Duration::from_millis(50), queue.lock()).await;
        assert!(
            acquired.is_ok(),
            "queue lock must be released before check returns"
        );
    }

    #[tokio::test]
    async fn accessors_return_constructor_values() {
        let queue = Arc::new(Mutex::new(CancellationQueue::new()));
        let check = check_for(queue, 5, Some(11));
        assert_eq!(check.run_id(), RunRef::new(5));
        assert_eq!(check.parent_work_item_id(), Some(WorkItemId::new(11)));
    }

    #[tokio::test]
    async fn clone_shares_underlying_queue_handle() {
        let queue = Arc::new(Mutex::new(CancellationQueue::new()));
        let a = check_for(queue.clone(), 1, None);
        let b = a.clone();

        assert_eq!(a.check().await, CancellationDecision::Proceed);
        assert_eq!(b.check().await, CancellationDecision::Proceed);

        queue.lock().await.enqueue(run_req(1, "shared"));

        assert!(a.should_abort().await);
        assert!(b.should_abort().await, "clone must observe the same queue");
    }
}
