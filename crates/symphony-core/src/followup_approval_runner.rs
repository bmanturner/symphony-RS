//! Follow-up approval dispatch runner — drains a
//! [`FollowupApprovalDispatchQueue`] and invokes a per-proposal
//! [`FollowupApprovalDispatcher`] for each pending request, bounded by a
//! max-concurrency [`Semaphore`] (SPEC v2 §4.10, §5.13 / ARCHITECTURE v2
//! §6.4, §7.1).
//!
//! Operator-decision queue: unlike the specialist/integration/QA
//! runners, the dispatcher here is the workflow's approval handler. It
//! presents the proposal to the configured `followups.approval_role`
//! (operator UI, tracker comment, audit log, …) and reports the decision
//! as a [`FollowupApprovalDispatchReason`]. The runner does not interpret
//! the verdict; downstream wiring advances the durable
//! [`crate::FollowupIssueRequest`] via
//! [`crate::FollowupIssueRequest::approve`] /
//! [`crate::FollowupIssueRequest::reject`] and the next
//! [`crate::FollowupApprovalQueueTick`] pass prunes the claim.
//!
//! Mirrors the bounded-concurrency contract of
//! [`crate::qa_runner::QaDispatchRunner`] and
//! [`crate::integration_runner::IntegrationDispatchRunner`]:
//!
//! 1. Drains pending requests from [`FollowupApprovalDispatchQueue`] and
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

use crate::followup::FollowupId;
use crate::followup_approval_tick::{
    FollowupApprovalDispatchQueue, FollowupApprovalDispatchRequest,
};

/// Final outcome of one follow-up approval dispatch.
///
/// The runner does not interpret these — it only records what the
/// dispatcher returned. Downstream wiring advances the durable
/// [`crate::FollowupIssueRequest`] (via `approve`/`reject`); the next
/// [`crate::FollowupApprovalQueueTick`] then prunes the claim.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum FollowupApprovalDispatchReason {
    /// Approver accepted the proposal. Downstream calls
    /// [`crate::FollowupIssueRequest::approve`] (status →
    /// [`crate::FollowupStatus::Approved`]) and a follow-up tracker
    /// issue is created per workflow policy.
    Approved,
    /// Approver explicitly rejected the proposal. Downstream calls
    /// [`crate::FollowupIssueRequest::reject`] (status →
    /// [`crate::FollowupStatus::Rejected`]).
    Rejected,
    /// Approver could not reach a decision this pass (need more info,
    /// owner unavailable, …). The proposal stays in
    /// [`crate::FollowupStatus::Proposed`] so the next tick will surface
    /// it again.
    Deferred,
    /// Cooperative cancellation — parent token fired (e.g. SIGINT).
    Canceled,
    /// Dispatcher failed for a non-decision reason (transport, internal
    /// error, …). The scheduler will re-surface the proposal per retry
    /// policy.
    Failed,
}

/// Outcome reported when a spawned approval dispatcher task completes.
///
/// Returned in batches by [`FollowupApprovalRunner::reap_completed`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FollowupApprovalDispatchOutcome {
    /// Durable follow-up id the request carried.
    pub followup_id: FollowupId,
    /// Final [`FollowupApprovalDispatchReason`] from the dispatcher.
    pub reason: FollowupApprovalDispatchReason,
}

/// Pluggable handler invoked by the runner for each pending
/// [`FollowupApprovalDispatchRequest`].
///
/// The composition root will wire this to the workflow's approval
/// handler (operator UI, tracker comment, audit-log emission,
/// `approve`/`reject` lifecycle on the durable record). Tests substitute
/// a deterministic recording implementation.
#[async_trait]
pub trait FollowupApprovalDispatcher: Send + Sync + 'static {
    /// Run one approval session for `request`. Resolves when the
    /// approver decides — approve, reject, defer, in error, or because
    /// `cancel` fired.
    async fn dispatch(
        &self,
        request: FollowupApprovalDispatchRequest,
        cancel: CancellationToken,
    ) -> FollowupApprovalDispatchReason;
}

/// Per-pass observability summary for
/// [`FollowupApprovalRunner::run_pending`].
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct FollowupApprovalRunReport {
    /// Requests for which a dispatcher task was spawned this pass.
    pub spawned: usize,
    /// Requests that could not be spawned because the semaphore was
    /// already saturated. Parked on the runner's internal deferred queue
    /// for the next pass.
    pub deferred_capacity: usize,
}

impl FollowupApprovalRunReport {
    /// `true` iff the pass was a complete no-op.
    pub fn is_idle(&self) -> bool {
        self.spawned == 0 && self.deferred_capacity == 0
    }
}

/// Bounded-concurrency consumer for [`FollowupApprovalDispatchQueue`].
pub struct FollowupApprovalRunner {
    queue: Arc<FollowupApprovalDispatchQueue>,
    dispatcher: Arc<dyn FollowupApprovalDispatcher>,
    semaphore: Arc<Semaphore>,
    deferred: Mutex<Vec<FollowupApprovalDispatchRequest>>,
    inflight: Mutex<JoinSet<FollowupApprovalDispatchOutcome>>,
    max_concurrent: usize,
}

impl FollowupApprovalRunner {
    /// Build a runner with capacity `max_concurrent`. A capacity of `0`
    /// disables dispatch entirely — every drained request will be
    /// counted as `deferred_capacity`.
    pub fn new(
        queue: Arc<FollowupApprovalDispatchQueue>,
        dispatcher: Arc<dyn FollowupApprovalDispatcher>,
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
    /// SIGINT-style shutdown tears down in-flight approval sessions
    /// cooperatively. The pass itself does not await the spawned
    /// futures; call [`Self::reap_completed`] to observe verdicts.
    pub async fn run_pending(&self, cancel: CancellationToken) -> FollowupApprovalRunReport {
        let mut work: Vec<FollowupApprovalDispatchRequest> = {
            let mut deferred = self.deferred.lock().await;
            std::mem::take(&mut *deferred)
        };
        work.extend(self.queue.drain());

        if work.is_empty() {
            return FollowupApprovalRunReport::default();
        }

        let mut report = FollowupApprovalRunReport::default();
        let mut new_deferred: Vec<FollowupApprovalDispatchRequest> = Vec::new();

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
            let followup_id = req.followup_id;
            let req_for_dispatch = req.clone();

            let mut inflight = self.inflight.lock().await;
            inflight.spawn(async move {
                let reason = dispatcher.dispatch(req_for_dispatch, child_cancel).await;
                drop(permit);
                FollowupApprovalDispatchOutcome {
                    followup_id,
                    reason,
                }
            });
            drop(inflight);

            debug!(
                followup_id = followup_id.get(),
                source_work_item = req.source_work_item.get(),
                "followup approval runner: spawned dispatch",
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
    pub async fn reap_completed(&self) -> Vec<FollowupApprovalDispatchOutcome> {
        let mut inflight = self.inflight.lock().await;
        let mut out = Vec::new();
        while let Some(joined) = inflight.try_join_next() {
            match joined {
                Ok(outcome) => out.push(outcome),
                Err(err) if err.is_cancelled() => {
                    debug!("followup approval dispatch task cancelled before reap");
                }
                Err(err) => {
                    warn!(
                        error = %err,
                        "followup approval dispatch task failed to join cleanly",
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
    use crate::role::RoleName;
    use crate::work_item::WorkItemId;
    use std::sync::Mutex as StdMutex;
    use std::time::Duration;

    fn req(id: i64, source: i64, title: &str, blocking: bool) -> FollowupApprovalDispatchRequest {
        FollowupApprovalDispatchRequest {
            followup_id: FollowupId::new(id),
            source_work_item: WorkItemId::new(source),
            title: title.into(),
            blocking,
            approval_role: RoleName::new("platform_lead"),
        }
    }

    /// Records every dispatch invocation and resolves with a configured
    /// reason (per-id override). Optional gate lets tests hold a
    /// dispatch open to assert capacity behaviour.
    struct RecordingDispatcher {
        calls: Arc<StdMutex<Vec<FollowupId>>>,
        gate: Arc<tokio::sync::Notify>,
        gate_enabled: Arc<StdMutex<bool>>,
        default_reason: FollowupApprovalDispatchReason,
        per_id: Arc<StdMutex<Vec<(FollowupId, FollowupApprovalDispatchReason)>>>,
    }

    impl RecordingDispatcher {
        fn new(default_reason: FollowupApprovalDispatchReason) -> Self {
            Self {
                calls: Arc::new(StdMutex::new(Vec::new())),
                gate: Arc::new(tokio::sync::Notify::new()),
                gate_enabled: Arc::new(StdMutex::new(false)),
                default_reason,
                per_id: Arc::new(StdMutex::new(Vec::new())),
            }
        }

        fn calls(&self) -> Vec<FollowupId> {
            self.calls.lock().unwrap().clone()
        }

        fn enable_gate(&self) {
            *self.gate_enabled.lock().unwrap() = true;
        }

        fn release_gate(&self) {
            *self.gate_enabled.lock().unwrap() = false;
            self.gate.notify_waiters();
        }

        fn set_reason_for(&self, id: FollowupId, reason: FollowupApprovalDispatchReason) {
            self.per_id.lock().unwrap().push((id, reason));
        }
    }

    #[async_trait]
    impl FollowupApprovalDispatcher for RecordingDispatcher {
        async fn dispatch(
            &self,
            request: FollowupApprovalDispatchRequest,
            cancel: CancellationToken,
        ) -> FollowupApprovalDispatchReason {
            self.calls.lock().unwrap().push(request.followup_id);

            let gated = *self.gate_enabled.lock().unwrap();
            if gated {
                tokio::select! {
                    _ = self.gate.notified() => {}
                    _ = cancel.cancelled() => return FollowupApprovalDispatchReason::Canceled,
                }
            }

            let per = self.per_id.lock().unwrap();
            for (id, reason) in per.iter() {
                if *id == request.followup_id {
                    return *reason;
                }
            }
            self.default_reason
        }
    }

    async fn wait_for_outcomes(
        runner: &FollowupApprovalRunner,
        n: usize,
    ) -> Vec<FollowupApprovalDispatchOutcome> {
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
        let queue = Arc::new(FollowupApprovalDispatchQueue::new());
        let dispatcher = Arc::new(RecordingDispatcher::new(
            FollowupApprovalDispatchReason::Approved,
        ));
        let runner = FollowupApprovalRunner::new(queue, dispatcher.clone(), 4);

        let report = runner.run_pending(CancellationToken::new()).await;
        assert!(report.is_idle(), "expected idle, got {report:?}");
        assert_eq!(runner.inflight_count().await, 0);
        assert!(dispatcher.calls().is_empty());
    }

    #[tokio::test]
    async fn approved_request_dispatches_and_reaps() {
        let queue = Arc::new(FollowupApprovalDispatchQueue::new());
        let dispatcher = Arc::new(RecordingDispatcher::new(
            FollowupApprovalDispatchReason::Approved,
        ));
        let runner = FollowupApprovalRunner::new(queue.clone(), dispatcher.clone(), 4);

        queue.enqueue(req(1, 100, "rate limit", false));

        let report = runner.run_pending(CancellationToken::new()).await;
        assert_eq!(report.spawned, 1);
        assert_eq!(report.deferred_capacity, 0);

        let outcomes = wait_for_outcomes(&runner, 1).await;
        assert_eq!(outcomes.len(), 1);
        assert_eq!(outcomes[0].followup_id, FollowupId::new(1));
        assert_eq!(outcomes[0].reason, FollowupApprovalDispatchReason::Approved);
        assert_eq!(dispatcher.calls(), vec![FollowupId::new(1)]);
    }

    #[tokio::test]
    async fn rejected_request_surfaces_rejected_reason() {
        let queue = Arc::new(FollowupApprovalDispatchQueue::new());
        let dispatcher = Arc::new(RecordingDispatcher::new(
            FollowupApprovalDispatchReason::Approved,
        ));
        dispatcher.set_reason_for(FollowupId::new(2), FollowupApprovalDispatchReason::Rejected);
        let runner = FollowupApprovalRunner::new(queue.clone(), dispatcher.clone(), 4);

        queue.enqueue(req(2, 100, "out of scope", false));
        let report = runner.run_pending(CancellationToken::new()).await;
        assert_eq!(report.spawned, 1);

        let outcomes = wait_for_outcomes(&runner, 1).await;
        assert_eq!(outcomes.len(), 1);
        assert_eq!(outcomes[0].reason, FollowupApprovalDispatchReason::Rejected);
        assert_eq!(dispatcher.calls(), vec![FollowupId::new(2)]);
    }

    #[tokio::test]
    async fn deferred_decision_surfaces_deferred_reason() {
        // Approver couldn't decide this pass — durable record stays in
        // Proposed and the next FollowupApprovalQueueTick pass will
        // re-surface it. The runner only reports what was returned.
        let queue = Arc::new(FollowupApprovalDispatchQueue::new());
        let dispatcher = Arc::new(RecordingDispatcher::new(
            FollowupApprovalDispatchReason::Approved,
        ));
        dispatcher.set_reason_for(FollowupId::new(3), FollowupApprovalDispatchReason::Deferred);
        let runner = FollowupApprovalRunner::new(queue.clone(), dispatcher.clone(), 4);

        queue.enqueue(req(3, 100, "needs owner", false));
        let _ = runner.run_pending(CancellationToken::new()).await;

        let outcomes = wait_for_outcomes(&runner, 1).await;
        assert_eq!(outcomes.len(), 1);
        assert_eq!(outcomes[0].reason, FollowupApprovalDispatchReason::Deferred);
    }

    #[tokio::test]
    async fn mixed_decisions_dispatch_independently() {
        let queue = Arc::new(FollowupApprovalDispatchQueue::new());
        let dispatcher = Arc::new(RecordingDispatcher::new(
            FollowupApprovalDispatchReason::Approved,
        ));
        dispatcher.set_reason_for(FollowupId::new(2), FollowupApprovalDispatchReason::Rejected);
        dispatcher.set_reason_for(FollowupId::new(3), FollowupApprovalDispatchReason::Deferred);
        let runner = FollowupApprovalRunner::new(queue.clone(), dispatcher.clone(), 8);

        queue.enqueue(req(1, 100, "approve me", false));
        queue.enqueue(req(2, 100, "reject me", false));
        queue.enqueue(req(3, 100, "defer me", true));

        let report = runner.run_pending(CancellationToken::new()).await;
        assert_eq!(report.spawned, 3);

        let outcomes = wait_for_outcomes(&runner, 3).await;
        let mut by_id: Vec<(i64, FollowupApprovalDispatchReason)> = outcomes
            .into_iter()
            .map(|o| (o.followup_id.get(), o.reason))
            .collect();
        by_id.sort_by_key(|(id, _)| *id);
        assert_eq!(
            by_id,
            vec![
                (1, FollowupApprovalDispatchReason::Approved),
                (2, FollowupApprovalDispatchReason::Rejected),
                (3, FollowupApprovalDispatchReason::Deferred),
            ]
        );
    }

    #[tokio::test]
    async fn run_pending_defers_when_capacity_saturated() {
        let queue = Arc::new(FollowupApprovalDispatchQueue::new());
        let dispatcher = Arc::new(RecordingDispatcher::new(
            FollowupApprovalDispatchReason::Approved,
        ));
        dispatcher.enable_gate();

        let runner = FollowupApprovalRunner::new(queue.clone(), dispatcher.clone(), 1);

        queue.enqueue(req(1, 100, "a", false));
        queue.enqueue(req(2, 100, "b", false));
        queue.enqueue(req(3, 100, "c", false));

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
        let queue = Arc::new(FollowupApprovalDispatchQueue::new());
        let dispatcher = Arc::new(RecordingDispatcher::new(
            FollowupApprovalDispatchReason::Approved,
        ));
        let runner = FollowupApprovalRunner::new(queue.clone(), dispatcher.clone(), 0);

        queue.enqueue(req(1, 100, "a", false));
        queue.enqueue(req(2, 100, "b", false));
        let r1 = runner.run_pending(CancellationToken::new()).await;
        assert_eq!(r1.deferred_capacity, 2);

        queue.enqueue(req(3, 100, "c", false));
        let r2 = runner.run_pending(CancellationToken::new()).await;
        assert_eq!(r2.deferred_capacity, 3);
        assert_eq!(runner.deferred_count().await, 3);

        let parked = runner.deferred.lock().await;
        let parked_ids: Vec<i64> = parked.iter().map(|r| r.followup_id.get()).collect();
        assert_eq!(parked_ids, vec![1, 2, 3]);
    }

    #[tokio::test]
    async fn parent_cancellation_propagates_to_dispatch() {
        let queue = Arc::new(FollowupApprovalDispatchQueue::new());
        let dispatcher = Arc::new(RecordingDispatcher::new(
            FollowupApprovalDispatchReason::Approved,
        ));
        dispatcher.enable_gate();

        let runner = FollowupApprovalRunner::new(queue.clone(), dispatcher.clone(), 4);
        queue.enqueue(req(1, 100, "a", false));

        let cancel = CancellationToken::new();
        let _ = runner.run_pending(cancel.clone()).await;
        assert_eq!(runner.inflight_count().await, 1);

        cancel.cancel();

        let outcomes = wait_for_outcomes(&runner, 1).await;
        assert_eq!(outcomes.len(), 1);
        assert_eq!(outcomes[0].reason, FollowupApprovalDispatchReason::Canceled);
    }

    #[tokio::test]
    async fn capacity_zero_defers_everything() {
        let queue = Arc::new(FollowupApprovalDispatchQueue::new());
        let dispatcher = Arc::new(RecordingDispatcher::new(
            FollowupApprovalDispatchReason::Approved,
        ));
        let runner = FollowupApprovalRunner::new(queue.clone(), dispatcher.clone(), 0);
        queue.enqueue(req(1, 100, "a", false));

        let report = runner.run_pending(CancellationToken::new()).await;
        assert_eq!(report.spawned, 0);
        assert_eq!(report.deferred_capacity, 1);
        assert!(dispatcher.calls().is_empty());
    }

    #[tokio::test]
    async fn reap_returns_outcomes_for_every_completed_dispatch() {
        let queue = Arc::new(FollowupApprovalDispatchQueue::new());
        let dispatcher = Arc::new(RecordingDispatcher::new(
            FollowupApprovalDispatchReason::Approved,
        ));
        let runner = FollowupApprovalRunner::new(queue.clone(), dispatcher.clone(), 4);

        queue.enqueue(req(1, 100, "a", false));
        queue.enqueue(req(2, 100, "b", false));
        let report = runner.run_pending(CancellationToken::new()).await;
        assert_eq!(report.spawned, 2);

        let outcomes = wait_for_outcomes(&runner, 2).await;
        let mut ids: Vec<i64> = outcomes.iter().map(|o| o.followup_id.get()).collect();
        ids.sort();
        assert_eq!(ids, vec![1, 2]);
    }

    #[test]
    fn dispatch_reason_round_trips_through_json() {
        for r in [
            FollowupApprovalDispatchReason::Approved,
            FollowupApprovalDispatchReason::Rejected,
            FollowupApprovalDispatchReason::Deferred,
            FollowupApprovalDispatchReason::Canceled,
            FollowupApprovalDispatchReason::Failed,
        ] {
            let json = serde_json::to_string(&r).unwrap();
            let back: FollowupApprovalDispatchReason = serde_json::from_str(&json).unwrap();
            assert_eq!(back, r);
        }
    }
}
