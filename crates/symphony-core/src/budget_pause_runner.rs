//! Budget-pause dispatch runner — drains a
//! [`BudgetPauseDispatchQueue`] and invokes a per-pause
//! [`BudgetPauseDispatcher`] for each pending request, bounded by a
//! max-concurrency [`Semaphore`] (SPEC v2 §5.15 / ARCHITECTURE v2 §5.8,
//! §7.1).
//!
//! Operator-decision queue: unlike the specialist/integration/QA
//! runners, the dispatcher here is the workflow's resume handler. It
//! presents the active pause to the operator (or a configured policy
//! waiver role) and reports the decision as a
//! [`BudgetPauseDispatchReason`]. The runner does not interpret the
//! decision; downstream wiring transitions the durable
//! [`crate::BudgetPauseStatus::Active`] row to
//! [`crate::BudgetPauseStatus::Resolved`] (operator resume) or
//! [`crate::BudgetPauseStatus::Waived`] (policy waiver), and the next
//! [`crate::BudgetPauseQueueTick`] pass prunes the claim.
//!
//! Mirrors the bounded-concurrency contract of
//! [`crate::qa_runner::QaDispatchRunner`] and
//! [`crate::integration_runner::IntegrationDispatchRunner`]:
//!
//! 1. Drains pending requests from [`BudgetPauseDispatchQueue`] and
//!    folds them onto the runner's deferred-from-prior-pass queue
//!    (front-first so age-order is preserved across passes).
//! 2. For each request, tries to acquire a semaphore permit
//!    non-blockingly. If unavailable the request is parked back in the
//!    runner's deferred queue and counted as `deferred_capacity`.
//!    Permit-holding requests spawn a dispatcher task into a
//!    [`JoinSet`]; the spawned future holds the owned permit until the
//!    dispatcher returns, so capacity is automatically restored on
//!    completion.

use std::sync::Arc;

use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use tokio::sync::{Mutex, Semaphore};
use tokio::task::JoinSet;
use tokio_util::sync::CancellationToken;
use tracing::{debug, warn};

use crate::budget_pause_tick::{
    BudgetPauseDispatchQueue, BudgetPauseDispatchRequest, BudgetPauseId,
};
use crate::work_item::WorkItemId;

/// Final outcome of one budget-pause dispatch.
///
/// The runner does not interpret these — it only records what the
/// dispatcher returned. Downstream wiring transitions the durable row
/// out of [`crate::BudgetPauseStatus::Active`]; the next
/// [`crate::BudgetPauseQueueTick`] pass prunes the claim.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum BudgetPauseDispatchReason {
    /// Operator explicitly resumed the work item. Downstream marks the
    /// pause [`crate::BudgetPauseStatus::Resolved`].
    Resumed,
    /// Configured policy waiver role accepted continuing past the cap.
    /// Downstream marks the pause [`crate::BudgetPauseStatus::Waived`].
    Waived,
    /// Operator/policy could not reach a decision this pass. The pause
    /// stays in [`crate::BudgetPauseStatus::Active`] so the next tick
    /// will surface it again.
    Deferred,
    /// Cooperative cancellation — parent token fired (e.g. SIGINT).
    Canceled,
    /// Dispatcher failed for a non-decision reason (transport, internal
    /// error, …). The scheduler will re-surface the pause per retry
    /// policy.
    Failed,
}

/// Outcome reported when a spawned budget-pause dispatcher task
/// completes.
///
/// Returned in batches by [`BudgetPauseRunner::reap_completed`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BudgetPauseDispatchOutcome {
    /// Durable pause id the request carried.
    pub pause_id: BudgetPauseId,
    /// Work item the pause blocks.
    pub work_item_id: WorkItemId,
    /// Final [`BudgetPauseDispatchReason`] from the dispatcher.
    pub reason: BudgetPauseDispatchReason,
}

/// Pluggable handler invoked by the runner for each pending
/// [`BudgetPauseDispatchRequest`].
///
/// The composition root will wire this to the workflow's resume handler
/// (operator UI, policy waiver evaluation, durable status transition).
/// Tests substitute a deterministic recording implementation.
#[async_trait]
pub trait BudgetPauseDispatcher: Send + Sync + 'static {
    /// Run one resume session for `request`. Resolves when the operator
    /// or policy decides — resume, waive, defer, in error, or because
    /// `cancel` fired.
    async fn dispatch(
        &self,
        request: BudgetPauseDispatchRequest,
        cancel: CancellationToken,
    ) -> BudgetPauseDispatchReason;
}

/// Per-pass observability summary for [`BudgetPauseRunner::run_pending`].
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct BudgetPauseRunReport {
    /// Requests for which a dispatcher task was spawned this pass.
    pub spawned: usize,
    /// Requests that could not be spawned because the semaphore was
    /// already saturated. Parked on the runner's internal deferred queue
    /// for the next pass.
    pub deferred_capacity: usize,
}

impl BudgetPauseRunReport {
    /// `true` iff the pass was a complete no-op.
    pub fn is_idle(&self) -> bool {
        self.spawned == 0 && self.deferred_capacity == 0
    }
}

/// Bounded-concurrency consumer for [`BudgetPauseDispatchQueue`].
pub struct BudgetPauseRunner {
    queue: Arc<BudgetPauseDispatchQueue>,
    dispatcher: Arc<dyn BudgetPauseDispatcher>,
    semaphore: Arc<Semaphore>,
    deferred: Mutex<Vec<BudgetPauseDispatchRequest>>,
    inflight: Mutex<JoinSet<BudgetPauseDispatchOutcome>>,
    max_concurrent: usize,
}

impl BudgetPauseRunner {
    /// Build a runner with capacity `max_concurrent`. A capacity of `0`
    /// disables dispatch entirely — every drained request will be
    /// counted as `deferred_capacity`.
    pub fn new(
        queue: Arc<BudgetPauseDispatchQueue>,
        dispatcher: Arc<dyn BudgetPauseDispatcher>,
        max_concurrent: usize,
    ) -> Self {
        Self {
            queue,
            dispatcher,
            semaphore: Arc::new(Semaphore::new(max_concurrent)),
            deferred: Mutex::new(Vec::new()),
            inflight: Mutex::new(JoinSet::new()),
            max_concurrent,
        }
    }

    /// Configured max concurrency.
    pub fn max_concurrent(&self) -> usize {
        self.max_concurrent
    }

    /// Number of dispatcher tasks currently spawned but not yet reaped.
    pub async fn inflight_count(&self) -> usize {
        self.inflight.lock().await.len()
    }

    /// Number of requests parked for a future pass because the
    /// semaphore was saturated when they were considered.
    pub async fn deferred_count(&self) -> usize {
        self.deferred.lock().await.len()
    }

    /// Drive one drain-and-spawn pass.
    ///
    /// `cancel` propagates into every spawned dispatcher task so a
    /// SIGINT-style shutdown tears down in-flight resume sessions
    /// cooperatively. The pass itself does not await the spawned
    /// futures; call [`Self::reap_completed`] to observe verdicts.
    pub async fn run_pending(&self, cancel: CancellationToken) -> BudgetPauseRunReport {
        let mut work: Vec<BudgetPauseDispatchRequest> = {
            let mut deferred = self.deferred.lock().await;
            std::mem::take(&mut *deferred)
        };
        work.extend(self.queue.drain());

        if work.is_empty() {
            return BudgetPauseRunReport::default();
        }

        let mut report = BudgetPauseRunReport::default();
        let mut new_deferred: Vec<BudgetPauseDispatchRequest> = Vec::new();

        for req in work {
            let permit = match self.semaphore.clone().try_acquire_owned() {
                Ok(p) => p,
                Err(_) => {
                    new_deferred.push(req);
                    report.deferred_capacity += 1;
                    continue;
                }
            };

            let dispatcher = self.dispatcher.clone();
            let child_cancel = cancel.child_token();
            let pause_id = req.pause_id;
            let work_item_id = req.work_item_id;
            let req_for_dispatch = req.clone();

            let mut inflight = self.inflight.lock().await;
            inflight.spawn(async move {
                let reason = dispatcher.dispatch(req_for_dispatch, child_cancel).await;
                drop(permit);
                BudgetPauseDispatchOutcome {
                    pause_id,
                    work_item_id,
                    reason,
                }
            });
            drop(inflight);

            debug!(
                pause_id = pause_id.get(),
                work_item_id = work_item_id.get(),
                "budget pause runner: spawned dispatch",
            );
            report.spawned += 1;
        }

        if !new_deferred.is_empty() {
            let mut deferred = self.deferred.lock().await;
            new_deferred.append(&mut *deferred);
            *deferred = new_deferred;
        }

        report
    }

    /// Drain every dispatcher task that has finished since the last
    /// call.
    pub async fn reap_completed(&self) -> Vec<BudgetPauseDispatchOutcome> {
        let mut inflight = self.inflight.lock().await;
        let mut out = Vec::new();
        while let Some(joined) = inflight.try_join_next() {
            match joined {
                Ok(outcome) => out.push(outcome),
                Err(err) if err.is_cancelled() => {
                    debug!("budget pause dispatch task cancelled before reap");
                }
                Err(err) => {
                    warn!(
                        error = %err,
                        "budget pause dispatch task failed to join cleanly",
                    );
                }
            }
        }
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex as StdMutex;
    use std::time::Duration;

    fn req(id: i64, work: i64, kind: &str) -> BudgetPauseDispatchRequest {
        BudgetPauseDispatchRequest {
            pause_id: BudgetPauseId::new(id),
            work_item_id: WorkItemId::new(work),
            budget_kind: kind.into(),
            limit_value: 5.0,
            observed: 6.0,
        }
    }

    /// Records every dispatch invocation and resolves with a configured
    /// reason (per-pause-id override). Optional gate lets tests hold a
    /// dispatch open to assert capacity behaviour.
    struct RecordingDispatcher {
        calls: Arc<StdMutex<Vec<BudgetPauseId>>>,
        gate: Arc<tokio::sync::Notify>,
        gate_enabled: Arc<StdMutex<bool>>,
        default_reason: BudgetPauseDispatchReason,
        per_id: Arc<StdMutex<Vec<(BudgetPauseId, BudgetPauseDispatchReason)>>>,
    }

    impl RecordingDispatcher {
        fn new(default_reason: BudgetPauseDispatchReason) -> Self {
            Self {
                calls: Arc::new(StdMutex::new(Vec::new())),
                gate: Arc::new(tokio::sync::Notify::new()),
                gate_enabled: Arc::new(StdMutex::new(false)),
                default_reason,
                per_id: Arc::new(StdMutex::new(Vec::new())),
            }
        }

        fn calls(&self) -> Vec<BudgetPauseId> {
            self.calls.lock().unwrap().clone()
        }

        fn enable_gate(&self) {
            *self.gate_enabled.lock().unwrap() = true;
        }

        fn release_gate(&self) {
            *self.gate_enabled.lock().unwrap() = false;
            self.gate.notify_waiters();
        }

        fn set_reason_for(&self, id: BudgetPauseId, reason: BudgetPauseDispatchReason) {
            self.per_id.lock().unwrap().push((id, reason));
        }
    }

    #[async_trait]
    impl BudgetPauseDispatcher for RecordingDispatcher {
        async fn dispatch(
            &self,
            request: BudgetPauseDispatchRequest,
            cancel: CancellationToken,
        ) -> BudgetPauseDispatchReason {
            self.calls.lock().unwrap().push(request.pause_id);

            let gated = *self.gate_enabled.lock().unwrap();
            if gated {
                tokio::select! {
                    _ = self.gate.notified() => {}
                    _ = cancel.cancelled() => return BudgetPauseDispatchReason::Canceled,
                }
            }

            let per = self.per_id.lock().unwrap();
            for (id, reason) in per.iter() {
                if *id == request.pause_id {
                    return *reason;
                }
            }
            self.default_reason
        }
    }

    async fn wait_for_outcomes(
        runner: &BudgetPauseRunner,
        n: usize,
    ) -> Vec<BudgetPauseDispatchOutcome> {
        let deadline = std::time::Instant::now() + Duration::from_secs(2);
        let mut acc = Vec::new();
        while acc.len() < n {
            acc.extend(runner.reap_completed().await);
            if acc.len() >= n {
                break;
            }
            if std::time::Instant::now() >= deadline {
                panic!(
                    "timed out waiting for {n} outcomes; got {} so far",
                    acc.len()
                );
            }
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
        acc
    }

    #[tokio::test]
    async fn run_pending_is_noop_when_queue_empty() {
        let queue = Arc::new(BudgetPauseDispatchQueue::new());
        let dispatcher = Arc::new(RecordingDispatcher::new(BudgetPauseDispatchReason::Resumed));
        let runner = BudgetPauseRunner::new(queue, dispatcher.clone(), 4);

        let report = runner.run_pending(CancellationToken::new()).await;
        assert!(report.is_idle(), "expected idle, got {report:?}");
        assert_eq!(runner.inflight_count().await, 0);
        assert!(dispatcher.calls().is_empty());
    }

    #[tokio::test]
    async fn resumed_request_dispatches_and_reaps() {
        let queue = Arc::new(BudgetPauseDispatchQueue::new());
        let dispatcher = Arc::new(RecordingDispatcher::new(BudgetPauseDispatchReason::Resumed));
        let runner = BudgetPauseRunner::new(queue.clone(), dispatcher.clone(), 4);

        queue.enqueue(req(1, 100, "max_cost_per_issue_usd"));

        let report = runner.run_pending(CancellationToken::new()).await;
        assert_eq!(report.spawned, 1);
        assert_eq!(report.deferred_capacity, 0);

        let outcomes = wait_for_outcomes(&runner, 1).await;
        assert_eq!(outcomes.len(), 1);
        assert_eq!(outcomes[0].pause_id, BudgetPauseId::new(1));
        assert_eq!(outcomes[0].work_item_id, WorkItemId::new(100));
        assert_eq!(outcomes[0].reason, BudgetPauseDispatchReason::Resumed);
        assert_eq!(dispatcher.calls(), vec![BudgetPauseId::new(1)]);
    }

    #[tokio::test]
    async fn waived_request_surfaces_waived_reason() {
        let queue = Arc::new(BudgetPauseDispatchQueue::new());
        let dispatcher = Arc::new(RecordingDispatcher::new(BudgetPauseDispatchReason::Resumed));
        dispatcher.set_reason_for(BudgetPauseId::new(2), BudgetPauseDispatchReason::Waived);
        let runner = BudgetPauseRunner::new(queue.clone(), dispatcher.clone(), 4);

        queue.enqueue(req(2, 100, "max_retries"));
        let report = runner.run_pending(CancellationToken::new()).await;
        assert_eq!(report.spawned, 1);

        let outcomes = wait_for_outcomes(&runner, 1).await;
        assert_eq!(outcomes.len(), 1);
        assert_eq!(outcomes[0].reason, BudgetPauseDispatchReason::Waived);
        assert_eq!(dispatcher.calls(), vec![BudgetPauseId::new(2)]);
    }

    #[tokio::test]
    async fn deferred_decision_surfaces_deferred_reason() {
        let queue = Arc::new(BudgetPauseDispatchQueue::new());
        let dispatcher = Arc::new(RecordingDispatcher::new(BudgetPauseDispatchReason::Resumed));
        dispatcher.set_reason_for(BudgetPauseId::new(3), BudgetPauseDispatchReason::Deferred);
        let runner = BudgetPauseRunner::new(queue.clone(), dispatcher.clone(), 4);

        queue.enqueue(req(3, 100, "max_cost_per_issue_usd"));
        let _ = runner.run_pending(CancellationToken::new()).await;

        let outcomes = wait_for_outcomes(&runner, 1).await;
        assert_eq!(outcomes.len(), 1);
        assert_eq!(outcomes[0].reason, BudgetPauseDispatchReason::Deferred);
    }

    #[tokio::test]
    async fn mixed_decisions_dispatch_independently() {
        let queue = Arc::new(BudgetPauseDispatchQueue::new());
        let dispatcher = Arc::new(RecordingDispatcher::new(BudgetPauseDispatchReason::Resumed));
        dispatcher.set_reason_for(BudgetPauseId::new(2), BudgetPauseDispatchReason::Waived);
        dispatcher.set_reason_for(BudgetPauseId::new(3), BudgetPauseDispatchReason::Deferred);
        let runner = BudgetPauseRunner::new(queue.clone(), dispatcher.clone(), 8);

        queue.enqueue(req(1, 100, "k"));
        queue.enqueue(req(2, 100, "k"));
        queue.enqueue(req(3, 100, "k"));

        let report = runner.run_pending(CancellationToken::new()).await;
        assert_eq!(report.spawned, 3);

        let outcomes = wait_for_outcomes(&runner, 3).await;
        let mut by_id: Vec<(i64, BudgetPauseDispatchReason)> = outcomes
            .into_iter()
            .map(|o| (o.pause_id.get(), o.reason))
            .collect();
        by_id.sort_by_key(|(id, _)| *id);
        assert_eq!(
            by_id,
            vec![
                (1, BudgetPauseDispatchReason::Resumed),
                (2, BudgetPauseDispatchReason::Waived),
                (3, BudgetPauseDispatchReason::Deferred),
            ]
        );
    }

    #[tokio::test]
    async fn run_pending_defers_when_capacity_saturated() {
        let queue = Arc::new(BudgetPauseDispatchQueue::new());
        let dispatcher = Arc::new(RecordingDispatcher::new(BudgetPauseDispatchReason::Resumed));
        dispatcher.enable_gate();

        let runner = BudgetPauseRunner::new(queue.clone(), dispatcher.clone(), 1);

        queue.enqueue(req(1, 100, "k"));
        queue.enqueue(req(2, 100, "k"));
        queue.enqueue(req(3, 100, "k"));

        let report = runner.run_pending(CancellationToken::new()).await;
        assert_eq!(report.spawned, 1);
        assert_eq!(report.deferred_capacity, 2);
        assert_eq!(runner.deferred_count().await, 2);

        dispatcher.release_gate();
        let _ = wait_for_outcomes(&runner, 1).await;

        let report = runner.run_pending(CancellationToken::new()).await;
        assert_eq!(report.spawned, 1);
        assert_eq!(report.deferred_capacity, 1);
        let _ = wait_for_outcomes(&runner, 1).await;

        let report = runner.run_pending(CancellationToken::new()).await;
        assert_eq!(report.spawned, 1);
        assert_eq!(report.deferred_capacity, 0);
        let _ = wait_for_outcomes(&runner, 1).await;

        let mut all: Vec<i64> = dispatcher.calls().into_iter().map(|i| i.get()).collect();
        all.sort();
        assert_eq!(all, vec![1, 2, 3]);
    }

    #[tokio::test]
    async fn deferred_requests_keep_age_order_across_passes() {
        let queue = Arc::new(BudgetPauseDispatchQueue::new());
        let dispatcher = Arc::new(RecordingDispatcher::new(BudgetPauseDispatchReason::Resumed));
        let runner = BudgetPauseRunner::new(queue.clone(), dispatcher.clone(), 0);

        queue.enqueue(req(1, 100, "k"));
        queue.enqueue(req(2, 100, "k"));
        let r1 = runner.run_pending(CancellationToken::new()).await;
        assert_eq!(r1.deferred_capacity, 2);

        queue.enqueue(req(3, 100, "k"));
        let r2 = runner.run_pending(CancellationToken::new()).await;
        assert_eq!(r2.deferred_capacity, 3);
        assert_eq!(runner.deferred_count().await, 3);

        let parked = runner.deferred.lock().await;
        let parked_ids: Vec<i64> = parked.iter().map(|r| r.pause_id.get()).collect();
        assert_eq!(parked_ids, vec![1, 2, 3]);
    }

    #[tokio::test]
    async fn parent_cancellation_propagates_to_dispatch() {
        let queue = Arc::new(BudgetPauseDispatchQueue::new());
        let dispatcher = Arc::new(RecordingDispatcher::new(BudgetPauseDispatchReason::Resumed));
        dispatcher.enable_gate();

        let runner = BudgetPauseRunner::new(queue.clone(), dispatcher.clone(), 4);
        queue.enqueue(req(1, 100, "k"));

        let cancel = CancellationToken::new();
        let _ = runner.run_pending(cancel.clone()).await;
        assert_eq!(runner.inflight_count().await, 1);

        cancel.cancel();

        let outcomes = wait_for_outcomes(&runner, 1).await;
        assert_eq!(outcomes.len(), 1);
        assert_eq!(outcomes[0].reason, BudgetPauseDispatchReason::Canceled);
    }

    #[tokio::test]
    async fn capacity_zero_defers_everything() {
        let queue = Arc::new(BudgetPauseDispatchQueue::new());
        let dispatcher = Arc::new(RecordingDispatcher::new(BudgetPauseDispatchReason::Resumed));
        let runner = BudgetPauseRunner::new(queue.clone(), dispatcher.clone(), 0);
        queue.enqueue(req(1, 100, "k"));

        let report = runner.run_pending(CancellationToken::new()).await;
        assert_eq!(report.spawned, 0);
        assert_eq!(report.deferred_capacity, 1);
        assert!(dispatcher.calls().is_empty());
    }

    #[tokio::test]
    async fn reap_returns_outcomes_for_every_completed_dispatch() {
        let queue = Arc::new(BudgetPauseDispatchQueue::new());
        let dispatcher = Arc::new(RecordingDispatcher::new(BudgetPauseDispatchReason::Resumed));
        let runner = BudgetPauseRunner::new(queue.clone(), dispatcher.clone(), 4);

        queue.enqueue(req(1, 100, "k"));
        queue.enqueue(req(2, 100, "k"));
        let report = runner.run_pending(CancellationToken::new()).await;
        assert_eq!(report.spawned, 2);

        let outcomes = wait_for_outcomes(&runner, 2).await;
        let mut ids: Vec<i64> = outcomes.iter().map(|o| o.pause_id.get()).collect();
        ids.sort();
        assert_eq!(ids, vec![1, 2]);
    }

    #[test]
    fn dispatch_reason_round_trips_through_json() {
        for r in [
            BudgetPauseDispatchReason::Resumed,
            BudgetPauseDispatchReason::Waived,
            BudgetPauseDispatchReason::Deferred,
            BudgetPauseDispatchReason::Canceled,
            BudgetPauseDispatchReason::Failed,
        ] {
            let json = serde_json::to_string(&r).unwrap();
            let back: BudgetPauseDispatchReason = serde_json::from_str(&json).unwrap();
            assert_eq!(back, r);
        }
    }
}
