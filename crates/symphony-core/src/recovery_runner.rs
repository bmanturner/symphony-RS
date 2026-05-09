//! Recovery dispatch runner — drains a [`RecoveryDispatchQueue`] and
//! invokes a per-candidate [`RecoveryDispatcher`] for each pending
//! request, bounded by a max-concurrency [`Semaphore`] (SPEC v2 §5.16 /
//! ARCHITECTURE v2 §7.2, §7.1).
//!
//! Reconciliation queue: unlike the specialist/integration/QA runners,
//! the dispatcher here is the workflow's recovery handler. It clears
//! the expired lease (or releases the orphaned workspace claim) and
//! reports the outcome as a [`RecoveryDispatchReason`]. The runner does
//! not interpret the verdict; downstream wiring is responsible for any
//! tracker-side rerouting, and the next [`crate::RecoveryQueueTick`]
//! pass prunes the claim set when the source no longer surfaces the
//! candidate.
//!
//! # Parity with the flat-loop reconciliation contract
//!
//! The legacy [`crate::PollLoop`] performed reconciliation inline and
//! emitted a single `reconciled` count per tick. The runner preserves
//! the same observable contract that mattered for production:
//!
//! * **Idempotency across passes.** Since the tick claims each surfaced
//!   candidate, a second pass over the same source view does not
//!   re-dispatch — the queue stays empty and the runner is a no-op
//!   (mirrors `reconciliation_is_idempotent_across_ticks` in
//!   `poll_loop.rs`).
//! * **Reap-once outcomes.** Each dispatched candidate produces exactly
//!   one [`RecoveryDispatchOutcome`] when its task completes (mirrors
//!   the single `Reconciled` summary the flat loop emitted).
//! * **Cooperative cancellation.** SIGINT propagates into in-flight
//!   recovery dispatchers via the parent [`CancellationToken`].
//!
//! Mirrors the bounded-concurrency contract of
//! [`crate::budget_pause_runner::BudgetPauseRunner`] and
//! [`crate::followup_approval_runner::FollowupApprovalRunner`]:
//!
//! 1. Drains pending requests from [`RecoveryDispatchQueue`] and folds
//!    them onto the runner's deferred-from-prior-pass queue (front-first
//!    so age-order is preserved across passes).
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

use crate::recovery_tick::{
    RecoveryDispatchQueue, RecoveryDispatchRequest, RecoveryRunId, RecoveryWorkspaceClaimId,
};
use crate::work_item::WorkItemId;

/// Final outcome of one recovery dispatch.
///
/// The runner does not interpret these — it only records what the
/// dispatcher returned. Downstream wiring clears the durable lease /
/// releases the workspace claim; the next [`crate::RecoveryQueueTick`]
/// pass prunes the claim set when the source stops surfacing it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RecoveryDispatchReason {
    /// Recovery handler successfully reconciled the candidate (lease
    /// cleared, workspace released, work item re-routed where
    /// applicable).
    Reconciled,
    /// Handler could not complete reconciliation this pass (e.g. busy
    /// resource). The candidate stays surfaced by the source and the
    /// next tick will re-emit it after the claim is pruned.
    Deferred,
    /// Cooperative cancellation — parent token fired (e.g. SIGINT).
    Canceled,
    /// Dispatcher failed for a non-decision reason (transport, internal
    /// error, …). The scheduler will re-surface the candidate per retry
    /// policy.
    Failed,
}

/// Stable target identifier for a recovery dispatch outcome — mirrors
/// the two upstream signals exposed by [`RecoveryDispatchRequest`] so
/// callers can correlate verdicts back to the surfaced candidate.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum RecoveryDispatchTarget {
    /// An expired-lease run.
    ExpiredLease {
        /// Durable run id.
        run_id: RecoveryRunId,
    },
    /// An orphaned workspace claim.
    OrphanedWorkspaceClaim {
        /// Durable claim id.
        claim_id: RecoveryWorkspaceClaimId,
    },
}

impl RecoveryDispatchTarget {
    fn from_request(req: &RecoveryDispatchRequest) -> Self {
        match req {
            RecoveryDispatchRequest::ExpiredLease { run_id, .. } => {
                Self::ExpiredLease { run_id: *run_id }
            }
            RecoveryDispatchRequest::OrphanedWorkspaceClaim { claim_id, .. } => {
                Self::OrphanedWorkspaceClaim {
                    claim_id: *claim_id,
                }
            }
        }
    }
}

fn work_item_of(req: &RecoveryDispatchRequest) -> WorkItemId {
    match req {
        RecoveryDispatchRequest::ExpiredLease { work_item_id, .. } => *work_item_id,
        RecoveryDispatchRequest::OrphanedWorkspaceClaim { work_item_id, .. } => *work_item_id,
    }
}

/// Outcome reported when a spawned recovery dispatcher task completes.
///
/// Returned in batches by [`RecoveryRunner::reap_completed`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RecoveryDispatchOutcome {
    /// Which upstream signal this outcome corresponds to.
    pub target: RecoveryDispatchTarget,
    /// Work item the candidate referenced.
    pub work_item_id: WorkItemId,
    /// Final [`RecoveryDispatchReason`] from the dispatcher.
    pub reason: RecoveryDispatchReason,
}

/// Pluggable handler invoked by the runner for each pending
/// [`RecoveryDispatchRequest`].
///
/// The composition root will wire this to the workflow's recovery
/// implementation (lease-clearing, workspace cleanup, intake re-routing,
/// audit-log emission). Tests substitute a deterministic recording
/// implementation.
#[async_trait]
pub trait RecoveryDispatcher: Send + Sync + 'static {
    /// Run one reconciliation for `request`. Resolves when the handler
    /// finishes — reconciled, deferred, in error, or because `cancel`
    /// fired.
    async fn dispatch(
        &self,
        request: RecoveryDispatchRequest,
        cancel: CancellationToken,
    ) -> RecoveryDispatchReason;
}

/// Per-pass observability summary for [`RecoveryRunner::run_pending`].
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct RecoveryRunReport {
    /// Requests for which a dispatcher task was spawned this pass.
    pub spawned: usize,
    /// Requests that could not be spawned because the semaphore was
    /// already saturated. Parked on the runner's internal deferred queue
    /// for the next pass.
    pub deferred_capacity: usize,
}

impl RecoveryRunReport {
    /// `true` iff the pass was a complete no-op.
    pub fn is_idle(&self) -> bool {
        self.spawned == 0 && self.deferred_capacity == 0
    }
}

/// Bounded-concurrency consumer for [`RecoveryDispatchQueue`].
pub struct RecoveryRunner {
    queue: Arc<RecoveryDispatchQueue>,
    dispatcher: Arc<dyn RecoveryDispatcher>,
    semaphore: Arc<Semaphore>,
    deferred: Mutex<Vec<RecoveryDispatchRequest>>,
    inflight: Mutex<JoinSet<RecoveryDispatchOutcome>>,
    max_concurrent: usize,
}

impl RecoveryRunner {
    /// Build a runner with capacity `max_concurrent`. A capacity of `0`
    /// disables dispatch entirely — every drained request will be
    /// counted as `deferred_capacity`.
    pub fn new(
        queue: Arc<RecoveryDispatchQueue>,
        dispatcher: Arc<dyn RecoveryDispatcher>,
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
    /// SIGINT-style shutdown tears down in-flight recovery dispatches
    /// cooperatively. The pass itself does not await the spawned
    /// futures; call [`Self::reap_completed`] to observe verdicts.
    pub async fn run_pending(&self, cancel: CancellationToken) -> RecoveryRunReport {
        let mut work: Vec<RecoveryDispatchRequest> = {
            let mut deferred = self.deferred.lock().await;
            std::mem::take(&mut *deferred)
        };
        work.extend(self.queue.drain());

        if work.is_empty() {
            return RecoveryRunReport::default();
        }

        let mut report = RecoveryRunReport::default();
        let mut new_deferred: Vec<RecoveryDispatchRequest> = Vec::new();

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
            let target = RecoveryDispatchTarget::from_request(&req);
            let work_item_id = work_item_of(&req);
            let req_for_dispatch = req.clone();

            let mut inflight = self.inflight.lock().await;
            inflight.spawn(async move {
                let reason = dispatcher.dispatch(req_for_dispatch, child_cancel).await;
                drop(permit);
                RecoveryDispatchOutcome {
                    target,
                    work_item_id,
                    reason,
                }
            });
            drop(inflight);

            debug!(
                ?target,
                work_item_id = work_item_id.get(),
                "recovery runner: spawned dispatch",
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
    pub async fn reap_completed(&self) -> Vec<RecoveryDispatchOutcome> {
        let mut inflight = self.inflight.lock().await;
        let mut out = Vec::new();
        while let Some(joined) = inflight.try_join_next() {
            match joined {
                Ok(outcome) => out.push(outcome),
                Err(err) if err.is_cancelled() => {
                    debug!("recovery dispatch task cancelled before reap");
                }
                Err(err) => {
                    warn!(
                        error = %err,
                        "recovery dispatch task failed to join cleanly",
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
    use crate::queue_tick::{QueueTick, QueueTickCadence};
    use crate::recovery_tick::{
        ExpiredLeaseCandidate, OrphanedWorkspaceClaimCandidate, RecoveryDispatchQueue,
        RecoveryQueueError, RecoveryQueueSource, RecoveryQueueTick,
    };
    use std::sync::Mutex as StdMutex;
    use std::time::Duration;

    fn lease_req(run: i64, work: i64) -> RecoveryDispatchRequest {
        RecoveryDispatchRequest::ExpiredLease {
            run_id: RecoveryRunId::new(run),
            work_item_id: WorkItemId::new(work),
            lease_owner: format!("worker-{run}"),
            lease_expires_at: "2026-05-08T00:00:00Z".into(),
            workspace_claim_id: None,
        }
    }

    fn orphan_req(claim: i64, work: i64) -> RecoveryDispatchRequest {
        RecoveryDispatchRequest::OrphanedWorkspaceClaim {
            claim_id: RecoveryWorkspaceClaimId::new(claim),
            work_item_id: WorkItemId::new(work),
            path: format!("/tmp/symphony/wt-{claim}"),
            claim_status: "active".into(),
        }
    }

    /// Records every dispatch invocation and resolves with a configured
    /// reason (per-target override). Optional gate lets tests hold a
    /// dispatch open to assert capacity behaviour.
    struct RecordingDispatcher {
        calls: Arc<StdMutex<Vec<RecoveryDispatchTarget>>>,
        gate: Arc<tokio::sync::Notify>,
        gate_enabled: Arc<StdMutex<bool>>,
        default_reason: RecoveryDispatchReason,
        per_target: Arc<StdMutex<Vec<(RecoveryDispatchTarget, RecoveryDispatchReason)>>>,
    }

    impl RecordingDispatcher {
        fn new(default_reason: RecoveryDispatchReason) -> Self {
            Self {
                calls: Arc::new(StdMutex::new(Vec::new())),
                gate: Arc::new(tokio::sync::Notify::new()),
                gate_enabled: Arc::new(StdMutex::new(false)),
                default_reason,
                per_target: Arc::new(StdMutex::new(Vec::new())),
            }
        }

        fn calls(&self) -> Vec<RecoveryDispatchTarget> {
            self.calls.lock().unwrap().clone()
        }

        fn enable_gate(&self) {
            *self.gate_enabled.lock().unwrap() = true;
        }

        fn release_gate(&self) {
            *self.gate_enabled.lock().unwrap() = false;
            self.gate.notify_waiters();
        }

        fn set_reason_for(&self, target: RecoveryDispatchTarget, reason: RecoveryDispatchReason) {
            self.per_target.lock().unwrap().push((target, reason));
        }
    }

    #[async_trait]
    impl RecoveryDispatcher for RecordingDispatcher {
        async fn dispatch(
            &self,
            request: RecoveryDispatchRequest,
            cancel: CancellationToken,
        ) -> RecoveryDispatchReason {
            let target = RecoveryDispatchTarget::from_request(&request);
            self.calls.lock().unwrap().push(target);

            let gated = *self.gate_enabled.lock().unwrap();
            if gated {
                tokio::select! {
                    _ = self.gate.notified() => {}
                    _ = cancel.cancelled() => return RecoveryDispatchReason::Canceled,
                }
            }

            let per = self.per_target.lock().unwrap();
            for (t, reason) in per.iter() {
                if *t == target {
                    return *reason;
                }
            }
            self.default_reason
        }
    }

    async fn wait_for_outcomes(runner: &RecoveryRunner, n: usize) -> Vec<RecoveryDispatchOutcome> {
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
        let queue = Arc::new(RecoveryDispatchQueue::new());
        let dispatcher = Arc::new(RecordingDispatcher::new(RecoveryDispatchReason::Reconciled));
        let runner = RecoveryRunner::new(queue, dispatcher.clone(), 4);

        let report = runner.run_pending(CancellationToken::new()).await;
        assert!(report.is_idle(), "expected idle, got {report:?}");
        assert_eq!(runner.inflight_count().await, 0);
        assert!(dispatcher.calls().is_empty());
    }

    #[tokio::test]
    async fn expired_lease_request_dispatches_and_reaps() {
        let queue = Arc::new(RecoveryDispatchQueue::new());
        let dispatcher = Arc::new(RecordingDispatcher::new(RecoveryDispatchReason::Reconciled));
        let runner = RecoveryRunner::new(queue.clone(), dispatcher.clone(), 4);

        queue.enqueue(lease_req(1, 100));

        let report = runner.run_pending(CancellationToken::new()).await;
        assert_eq!(report.spawned, 1);
        assert_eq!(report.deferred_capacity, 0);

        let outcomes = wait_for_outcomes(&runner, 1).await;
        assert_eq!(outcomes.len(), 1);
        assert_eq!(
            outcomes[0].target,
            RecoveryDispatchTarget::ExpiredLease {
                run_id: RecoveryRunId::new(1)
            }
        );
        assert_eq!(outcomes[0].work_item_id, WorkItemId::new(100));
        assert_eq!(outcomes[0].reason, RecoveryDispatchReason::Reconciled);
    }

    #[tokio::test]
    async fn orphan_request_dispatches_and_reaps() {
        let queue = Arc::new(RecoveryDispatchQueue::new());
        let dispatcher = Arc::new(RecordingDispatcher::new(RecoveryDispatchReason::Reconciled));
        let runner = RecoveryRunner::new(queue.clone(), dispatcher.clone(), 4);

        queue.enqueue(orphan_req(7, 100));
        let report = runner.run_pending(CancellationToken::new()).await;
        assert_eq!(report.spawned, 1);

        let outcomes = wait_for_outcomes(&runner, 1).await;
        assert_eq!(outcomes.len(), 1);
        assert_eq!(
            outcomes[0].target,
            RecoveryDispatchTarget::OrphanedWorkspaceClaim {
                claim_id: RecoveryWorkspaceClaimId::new(7)
            }
        );
        assert_eq!(outcomes[0].reason, RecoveryDispatchReason::Reconciled);
    }

    #[tokio::test]
    async fn deferred_decision_surfaces_deferred_reason() {
        let queue = Arc::new(RecoveryDispatchQueue::new());
        let dispatcher = Arc::new(RecordingDispatcher::new(RecoveryDispatchReason::Reconciled));
        let target = RecoveryDispatchTarget::ExpiredLease {
            run_id: RecoveryRunId::new(3),
        };
        dispatcher.set_reason_for(target, RecoveryDispatchReason::Deferred);
        let runner = RecoveryRunner::new(queue.clone(), dispatcher.clone(), 4);

        queue.enqueue(lease_req(3, 100));
        let _ = runner.run_pending(CancellationToken::new()).await;

        let outcomes = wait_for_outcomes(&runner, 1).await;
        assert_eq!(outcomes.len(), 1);
        assert_eq!(outcomes[0].reason, RecoveryDispatchReason::Deferred);
    }

    #[tokio::test]
    async fn mixed_targets_dispatch_independently() {
        let queue = Arc::new(RecoveryDispatchQueue::new());
        let dispatcher = Arc::new(RecordingDispatcher::new(RecoveryDispatchReason::Reconciled));
        dispatcher.set_reason_for(
            RecoveryDispatchTarget::OrphanedWorkspaceClaim {
                claim_id: RecoveryWorkspaceClaimId::new(7),
            },
            RecoveryDispatchReason::Failed,
        );
        let runner = RecoveryRunner::new(queue.clone(), dispatcher.clone(), 8);

        queue.enqueue(lease_req(1, 100));
        queue.enqueue(lease_req(2, 101));
        queue.enqueue(orphan_req(7, 102));

        let report = runner.run_pending(CancellationToken::new()).await;
        assert_eq!(report.spawned, 3);

        let outcomes = wait_for_outcomes(&runner, 3).await;
        let mut by_target: Vec<(RecoveryDispatchTarget, RecoveryDispatchReason)> =
            outcomes.into_iter().map(|o| (o.target, o.reason)).collect();
        by_target.sort_by_key(|(t, _)| match t {
            RecoveryDispatchTarget::ExpiredLease { run_id } => (0, run_id.get()),
            RecoveryDispatchTarget::OrphanedWorkspaceClaim { claim_id } => (1, claim_id.get()),
        });
        assert_eq!(
            by_target,
            vec![
                (
                    RecoveryDispatchTarget::ExpiredLease {
                        run_id: RecoveryRunId::new(1)
                    },
                    RecoveryDispatchReason::Reconciled
                ),
                (
                    RecoveryDispatchTarget::ExpiredLease {
                        run_id: RecoveryRunId::new(2)
                    },
                    RecoveryDispatchReason::Reconciled
                ),
                (
                    RecoveryDispatchTarget::OrphanedWorkspaceClaim {
                        claim_id: RecoveryWorkspaceClaimId::new(7)
                    },
                    RecoveryDispatchReason::Failed
                ),
            ]
        );
    }

    #[tokio::test]
    async fn run_pending_defers_when_capacity_saturated() {
        let queue = Arc::new(RecoveryDispatchQueue::new());
        let dispatcher = Arc::new(RecordingDispatcher::new(RecoveryDispatchReason::Reconciled));
        dispatcher.enable_gate();

        let runner = RecoveryRunner::new(queue.clone(), dispatcher.clone(), 1);

        queue.enqueue(lease_req(1, 100));
        queue.enqueue(lease_req(2, 100));
        queue.enqueue(orphan_req(3, 100));

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

        assert_eq!(dispatcher.calls().len(), 3);
    }

    #[tokio::test]
    async fn deferred_requests_keep_age_order_across_passes() {
        let queue = Arc::new(RecoveryDispatchQueue::new());
        let dispatcher = Arc::new(RecordingDispatcher::new(RecoveryDispatchReason::Reconciled));
        let runner = RecoveryRunner::new(queue.clone(), dispatcher.clone(), 0);

        queue.enqueue(lease_req(1, 100));
        queue.enqueue(lease_req(2, 100));
        let r1 = runner.run_pending(CancellationToken::new()).await;
        assert_eq!(r1.deferred_capacity, 2);

        queue.enqueue(orphan_req(3, 100));
        let r2 = runner.run_pending(CancellationToken::new()).await;
        assert_eq!(r2.deferred_capacity, 3);
        assert_eq!(runner.deferred_count().await, 3);

        let parked = runner.deferred.lock().await;
        let parked_targets: Vec<RecoveryDispatchTarget> = parked
            .iter()
            .map(RecoveryDispatchTarget::from_request)
            .collect();
        assert_eq!(
            parked_targets,
            vec![
                RecoveryDispatchTarget::ExpiredLease {
                    run_id: RecoveryRunId::new(1)
                },
                RecoveryDispatchTarget::ExpiredLease {
                    run_id: RecoveryRunId::new(2)
                },
                RecoveryDispatchTarget::OrphanedWorkspaceClaim {
                    claim_id: RecoveryWorkspaceClaimId::new(3)
                },
            ]
        );
    }

    #[tokio::test]
    async fn parent_cancellation_propagates_to_dispatch() {
        let queue = Arc::new(RecoveryDispatchQueue::new());
        let dispatcher = Arc::new(RecordingDispatcher::new(RecoveryDispatchReason::Reconciled));
        dispatcher.enable_gate();

        let runner = RecoveryRunner::new(queue.clone(), dispatcher.clone(), 4);
        queue.enqueue(lease_req(1, 100));

        let cancel = CancellationToken::new();
        let _ = runner.run_pending(cancel.clone()).await;
        assert_eq!(runner.inflight_count().await, 1);

        cancel.cancel();

        let outcomes = wait_for_outcomes(&runner, 1).await;
        assert_eq!(outcomes.len(), 1);
        assert_eq!(outcomes[0].reason, RecoveryDispatchReason::Canceled);
    }

    #[tokio::test]
    async fn capacity_zero_defers_everything() {
        let queue = Arc::new(RecoveryDispatchQueue::new());
        let dispatcher = Arc::new(RecordingDispatcher::new(RecoveryDispatchReason::Reconciled));
        let runner = RecoveryRunner::new(queue.clone(), dispatcher.clone(), 0);
        queue.enqueue(lease_req(1, 100));

        let report = runner.run_pending(CancellationToken::new()).await;
        assert_eq!(report.spawned, 0);
        assert_eq!(report.deferred_capacity, 1);
        assert!(dispatcher.calls().is_empty());
    }

    #[tokio::test]
    async fn reap_returns_outcomes_for_every_completed_dispatch() {
        let queue = Arc::new(RecoveryDispatchQueue::new());
        let dispatcher = Arc::new(RecordingDispatcher::new(RecoveryDispatchReason::Reconciled));
        let runner = RecoveryRunner::new(queue.clone(), dispatcher.clone(), 4);

        queue.enqueue(lease_req(1, 100));
        queue.enqueue(orphan_req(2, 101));
        let report = runner.run_pending(CancellationToken::new()).await;
        assert_eq!(report.spawned, 2);

        let outcomes = wait_for_outcomes(&runner, 2).await;
        assert_eq!(outcomes.len(), 2);
    }

    /// Parity with `reconciliation_is_idempotent_across_ticks` in
    /// `poll_loop.rs`: once the tick has claimed a candidate, a second
    /// pass over the same source view must not re-dispatch.
    #[tokio::test]
    async fn parity_reconciliation_is_idempotent_across_passes() {
        #[derive(Default)]
        struct StaticSource {
            leases: StdMutex<Vec<ExpiredLeaseCandidate>>,
        }
        impl RecoveryQueueSource for StaticSource {
            fn list_expired_leases(
                &self,
            ) -> Result<Vec<ExpiredLeaseCandidate>, RecoveryQueueError> {
                Ok(self.leases.lock().unwrap().clone())
            }
            fn list_orphaned_workspace_claims(
                &self,
            ) -> Result<Vec<OrphanedWorkspaceClaimCandidate>, RecoveryQueueError> {
                Ok(Vec::new())
            }
        }

        let src = Arc::new(StaticSource::default());
        src.leases.lock().unwrap().push(ExpiredLeaseCandidate {
            run_id: RecoveryRunId::new(11),
            work_item_id: WorkItemId::new(100),
            lease_owner: "worker-a".into(),
            lease_expires_at: "2026-05-08T00:00:00Z".into(),
            workspace_claim_id: None,
        });
        let q = Arc::new(RecoveryDispatchQueue::new());
        let mut tick = RecoveryQueueTick::new(
            src.clone() as Arc<dyn RecoveryQueueSource>,
            q.clone(),
            QueueTickCadence::fixed(Duration::from_millis(1)),
        );

        let dispatcher = Arc::new(RecordingDispatcher::new(RecoveryDispatchReason::Reconciled));
        let runner = RecoveryRunner::new(q.clone(), dispatcher.clone(), 4);

        // Tick 1: surfaces, runner dispatches once.
        let _ = tick.tick().await;
        let r1 = runner.run_pending(CancellationToken::new()).await;
        assert_eq!(r1.spawned, 1);
        let _ = wait_for_outcomes(&runner, 1).await;

        // Tick 2: source still surfaces it, but tick claim set defers.
        let outcome = tick.tick().await;
        assert_eq!(outcome.processed, 0);
        assert_eq!(outcome.deferred, 1);

        let r2 = runner.run_pending(CancellationToken::new()).await;
        assert!(r2.is_idle(), "second pass must not re-dispatch: {r2:?}");
        assert_eq!(dispatcher.calls().len(), 1);
    }

    /// Parity with `reconciliation_emits_a_single_reconciled_summary`:
    /// every dispatched candidate produces exactly one reaped outcome.
    #[tokio::test]
    async fn parity_each_dispatched_candidate_produces_exactly_one_outcome() {
        let queue = Arc::new(RecoveryDispatchQueue::new());
        let dispatcher = Arc::new(RecordingDispatcher::new(RecoveryDispatchReason::Reconciled));
        let runner = RecoveryRunner::new(queue.clone(), dispatcher.clone(), 4);

        queue.enqueue(lease_req(1, 100));
        queue.enqueue(orphan_req(2, 101));

        let report = runner.run_pending(CancellationToken::new()).await;
        assert_eq!(report.spawned, 2);

        let outcomes = wait_for_outcomes(&runner, 2).await;
        assert_eq!(outcomes.len(), 2);

        // Reap is a no-op a second time — outcomes are surfaced exactly
        // once.
        let again = runner.reap_completed().await;
        assert!(again.is_empty());
    }

    #[test]
    fn dispatch_reason_round_trips_through_json() {
        for r in [
            RecoveryDispatchReason::Reconciled,
            RecoveryDispatchReason::Deferred,
            RecoveryDispatchReason::Canceled,
            RecoveryDispatchReason::Failed,
        ] {
            let json = serde_json::to_string(&r).unwrap();
            let back: RecoveryDispatchReason = serde_json::from_str(&json).unwrap();
            assert_eq!(back, r);
        }
    }

    #[test]
    fn dispatch_target_round_trips_through_json_with_kind_tag() {
        let t1 = RecoveryDispatchTarget::ExpiredLease {
            run_id: RecoveryRunId::new(1),
        };
        let json = serde_json::to_string(&t1).unwrap();
        assert!(json.contains("\"kind\":\"expired_lease\""));
        let back: RecoveryDispatchTarget = serde_json::from_str(&json).unwrap();
        assert_eq!(back, t1);

        let t2 = RecoveryDispatchTarget::OrphanedWorkspaceClaim {
            claim_id: RecoveryWorkspaceClaimId::new(7),
        };
        let json = serde_json::to_string(&t2).unwrap();
        assert!(json.contains("\"kind\":\"orphaned_workspace_claim\""));
        let back: RecoveryDispatchTarget = serde_json::from_str(&json).unwrap();
        assert_eq!(back, t2);
    }
}
