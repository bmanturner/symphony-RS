//! Integration dispatch runner — drains an [`IntegrationDispatchQueue`]
//! and invokes a per-parent [`IntegrationDispatcher`] for each pending
//! request, bounded by a max-concurrency [`Semaphore`]
//! (SPEC v2 §6.4 / ARCHITECTURE v2 §7.1, §9).
//!
//! Mirrors the bounded-concurrency contract of
//! [`crate::specialist_runner::SpecialistRunner`]:
//!
//! 1. Drains pending requests from [`IntegrationDispatchQueue`] and folds
//!    them onto the runner's deferred-from-prior-pass queue (front-first
//!    so age-order is preserved across passes).
//! 2. For each request, tries to acquire a semaphore permit
//!    non-blockingly. If unavailable the request is parked back in the
//!    runner's deferred queue and counted as `deferred_capacity`. Permit-
//!    holding requests spawn a dispatcher task into a [`JoinSet`]; the
//!    spawned future holds the owned permit until the dispatcher returns,
//!    so capacity is automatically restored on completion.
//!
//! Unlike the specialist runner, integration requests are pre-gated
//! upstream by [`crate::IntegrationQueueTick`] (it consults the durable
//! `IntegrationQueueRepository` view under the workflow's
//! [`crate::IntegrationGates`]). The runner therefore does not perform
//! its own gate check or active-set lookup — every drained request is
//! authoritative. Whether the underlying integration succeeded or hit a
//! blocker is signalled by the dispatcher's returned
//! [`IntegrationDispatchReason`].

use std::sync::Arc;

use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use tokio::sync::{Mutex, Semaphore};
use tokio::task::JoinSet;
use tokio_util::sync::CancellationToken;
use tracing::{debug, warn};

use crate::integration_tick::{IntegrationDispatchQueue, IntegrationDispatchRequest};
use crate::work_item::WorkItemId;

/// Final state of one integration-owner dispatch.
///
/// The runner does not interpret these — it only records what the
/// dispatcher returned. Downstream wiring (Phase 11 scheduler hookup,
/// Phase 12 events) will translate into work-item state transitions and
/// observability events.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum IntegrationDispatchReason {
    /// Integration finished cleanly. Downstream may promote the parent
    /// to `qa` (or `done` for trivial flows) per workflow policy.
    Completed,
    /// Integration was blocked — typically because the dispatcher filed a
    /// blocker (merge conflict, missing artifact, etc.). The parent
    /// remains in the integration class until a rework cycle resolves the
    /// blocker.
    Blocked,
    /// Cooperative cancellation — parent token fired (e.g. SIGINT).
    Canceled,
    /// Dispatcher failed for a non-blocker reason (transport, internal
    /// error, …). The scheduler will requeue per retry policy.
    Failed,
}

/// Outcome reported when a spawned integration dispatcher task completes.
///
/// Returned in batches by [`IntegrationDispatchRunner::reap_completed`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct IntegrationDispatchOutcome {
    /// Durable parent work item id the request carried.
    pub parent_id: WorkItemId,
    /// Tracker-facing identifier (e.g. `PROJ-42`).
    pub parent_identifier: String,
    /// Final [`IntegrationDispatchReason`] from the dispatcher.
    pub reason: IntegrationDispatchReason,
}

/// Pluggable handler invoked by the runner for each pending
/// [`IntegrationDispatchRequest`].
///
/// The composition root will wire this to the integration-owner agent
/// runner (workspace claim → cwd/branch verification → agent session →
/// integration record persistence). Tests substitute a deterministic
/// recording implementation.
#[async_trait]
pub trait IntegrationDispatcher: Send + Sync + 'static {
    /// Run one full integration-owner session for `request`. Resolves
    /// when the session ends — successfully, blocked, in error, or
    /// because `cancel` fired.
    async fn dispatch(
        &self,
        request: IntegrationDispatchRequest,
        cancel: CancellationToken,
    ) -> IntegrationDispatchReason;
}

/// Per-pass observability summary for [`IntegrationDispatchRunner::run_pending`].
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct IntegrationRunReport {
    /// Requests for which a dispatcher task was spawned this pass.
    pub spawned: usize,
    /// Requests that could not be spawned because the semaphore was
    /// already saturated. Parked on the runner's internal deferred queue
    /// for the next pass.
    pub deferred_capacity: usize,
}

impl IntegrationRunReport {
    /// `true` iff the pass was a complete no-op.
    pub fn is_idle(&self) -> bool {
        self.spawned == 0 && self.deferred_capacity == 0
    }
}

/// Bounded-concurrency consumer for [`IntegrationDispatchQueue`].
pub struct IntegrationDispatchRunner {
    queue: Arc<IntegrationDispatchQueue>,
    dispatcher: Arc<dyn IntegrationDispatcher>,
    semaphore: Arc<Semaphore>,
    deferred: Mutex<Vec<IntegrationDispatchRequest>>,
    inflight: Mutex<JoinSet<IntegrationDispatchOutcome>>,
    max_concurrent: usize,
}

impl IntegrationDispatchRunner {
    /// Build a runner with capacity `max_concurrent`. A capacity of `0`
    /// disables dispatch entirely — every drained request will be
    /// counted as `deferred_capacity`.
    pub fn new(
        queue: Arc<IntegrationDispatchQueue>,
        dispatcher: Arc<dyn IntegrationDispatcher>,
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
    /// SIGINT-style shutdown tears down in-flight integrations
    /// cooperatively. The pass itself does not await the spawned
    /// futures; call [`Self::reap_completed`] to observe results.
    pub async fn run_pending(&self, cancel: CancellationToken) -> IntegrationRunReport {
        // Combine prior-pass deferreds with newly drained requests,
        // deferreds first to preserve age-order.
        let mut work: Vec<IntegrationDispatchRequest> = {
            let mut deferred = self.deferred.lock().await;
            std::mem::take(&mut *deferred)
        };
        work.extend(self.queue.drain());

        if work.is_empty() {
            return IntegrationRunReport::default();
        }

        let mut report = IntegrationRunReport::default();
        let mut new_deferred: Vec<IntegrationDispatchRequest> = Vec::new();

        for req in work {
            // Non-blocking permit acquisition — if capacity is exhausted
            // we want to *park*, not wait. Waiting would block the
            // scheduler's tick fan against a long-running integration.
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
            let parent_id = req.parent_id;
            let parent_identifier = req.parent_identifier.clone();
            let req_for_dispatch = req.clone();

            let mut inflight = self.inflight.lock().await;
            inflight.spawn(async move {
                let reason = dispatcher.dispatch(req_for_dispatch, child_cancel).await;
                drop(permit); // release capacity at task exit
                IntegrationDispatchOutcome {
                    parent_id,
                    parent_identifier,
                    reason,
                }
            });
            drop(inflight);

            debug!(
                parent_id = parent_id.get(),
                identifier = %req.parent_identifier,
                "integration runner: spawned dispatch",
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

    /// Drain every dispatcher task that has finished since the last call.
    pub async fn reap_completed(&self) -> Vec<IntegrationDispatchOutcome> {
        let mut inflight = self.inflight.lock().await;
        let mut out = Vec::new();
        while let Some(joined) = inflight.try_join_next() {
            match joined {
                Ok(outcome) => out.push(outcome),
                Err(err) if err.is_cancelled() => {
                    debug!("integration dispatch task cancelled before reap");
                }
                Err(err) => {
                    warn!(error = %err, "integration dispatch task failed to join cleanly");
                }
            }
        }
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::integration_request::IntegrationRequestCause;
    use std::sync::Mutex as StdMutex;
    use std::time::Duration;

    fn req(id: i64, identifier: &str) -> IntegrationDispatchRequest {
        IntegrationDispatchRequest {
            parent_id: WorkItemId::new(id),
            parent_identifier: identifier.into(),
            parent_title: format!("title {identifier}"),
            cause: IntegrationRequestCause::DirectIntegrationRequest,
        }
    }

    /// Records every dispatch invocation and resolves with a configured
    /// reason (per-identifier override). Optional gate lets tests hold a
    /// dispatch open to assert capacity behaviour.
    struct RecordingDispatcher {
        calls: Arc<StdMutex<Vec<String>>>,
        gate: Arc<tokio::sync::Notify>,
        gate_enabled: Arc<StdMutex<bool>>,
        default_reason: IntegrationDispatchReason,
        per_identifier: Arc<StdMutex<Vec<(String, IntegrationDispatchReason)>>>,
    }

    impl RecordingDispatcher {
        fn new(default_reason: IntegrationDispatchReason) -> Self {
            Self {
                calls: Arc::new(StdMutex::new(Vec::new())),
                gate: Arc::new(tokio::sync::Notify::new()),
                gate_enabled: Arc::new(StdMutex::new(false)),
                default_reason,
                per_identifier: Arc::new(StdMutex::new(Vec::new())),
            }
        }

        fn calls(&self) -> Vec<String> {
            self.calls.lock().unwrap().clone()
        }

        fn enable_gate(&self) {
            *self.gate_enabled.lock().unwrap() = true;
        }

        fn release_gate(&self) {
            *self.gate_enabled.lock().unwrap() = false;
            self.gate.notify_waiters();
        }

        fn set_reason_for(&self, identifier: &str, reason: IntegrationDispatchReason) {
            self.per_identifier
                .lock()
                .unwrap()
                .push((identifier.into(), reason));
        }
    }

    #[async_trait]
    impl IntegrationDispatcher for RecordingDispatcher {
        async fn dispatch(
            &self,
            request: IntegrationDispatchRequest,
            cancel: CancellationToken,
        ) -> IntegrationDispatchReason {
            self.calls
                .lock()
                .unwrap()
                .push(request.parent_identifier.clone());

            let gated = *self.gate_enabled.lock().unwrap();
            if gated {
                tokio::select! {
                    _ = self.gate.notified() => {}
                    _ = cancel.cancelled() => return IntegrationDispatchReason::Canceled,
                }
            }

            let per = self.per_identifier.lock().unwrap();
            for (id, reason) in per.iter() {
                if id == &request.parent_identifier {
                    return *reason;
                }
            }
            self.default_reason
        }
    }

    async fn wait_for_outcomes(
        runner: &IntegrationDispatchRunner,
        n: usize,
    ) -> Vec<IntegrationDispatchOutcome> {
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
        let queue = Arc::new(IntegrationDispatchQueue::new());
        let dispatcher = Arc::new(RecordingDispatcher::new(
            IntegrationDispatchReason::Completed,
        ));
        let runner = IntegrationDispatchRunner::new(queue, dispatcher.clone(), 4);

        let report = runner.run_pending(CancellationToken::new()).await;
        assert!(report.is_idle(), "expected idle, got {report:?}");
        assert_eq!(runner.inflight_count().await, 0);
        assert!(dispatcher.calls().is_empty());
    }

    #[tokio::test]
    async fn ready_candidate_is_dispatched_to_completion() {
        let queue = Arc::new(IntegrationDispatchQueue::new());
        let dispatcher = Arc::new(RecordingDispatcher::new(
            IntegrationDispatchReason::Completed,
        ));
        let runner = IntegrationDispatchRunner::new(queue.clone(), dispatcher.clone(), 4);

        // The upstream tick has already gated this candidate: the runner
        // simply spawns a dispatcher task for it.
        queue.enqueue(req(1, "PROJ-1"));

        let report = runner.run_pending(CancellationToken::new()).await;
        assert_eq!(report.spawned, 1);
        assert_eq!(report.deferred_capacity, 0);

        let outcomes = wait_for_outcomes(&runner, 1).await;
        assert_eq!(outcomes.len(), 1);
        assert_eq!(outcomes[0].parent_id, WorkItemId::new(1));
        assert_eq!(outcomes[0].parent_identifier, "PROJ-1");
        assert_eq!(outcomes[0].reason, IntegrationDispatchReason::Completed);
        assert_eq!(dispatcher.calls(), vec!["PROJ-1"]);
    }

    #[tokio::test]
    async fn blocked_candidate_surfaces_blocked_reason() {
        // "Blocked" at this layer means the dispatcher resolved with
        // `Blocked` — typically because the integration owner filed a
        // blocker (merge conflict, missing artifact). The upstream tick
        // would only re-emit the parent after a rework cycle, so the
        // runner's job is just to honour the dispatcher's verdict.
        let queue = Arc::new(IntegrationDispatchQueue::new());
        let dispatcher = Arc::new(RecordingDispatcher::new(
            IntegrationDispatchReason::Completed,
        ));
        dispatcher.set_reason_for("PROJ-2", IntegrationDispatchReason::Blocked);
        let runner = IntegrationDispatchRunner::new(queue.clone(), dispatcher.clone(), 4);

        queue.enqueue(req(2, "PROJ-2"));
        let report = runner.run_pending(CancellationToken::new()).await;
        assert_eq!(report.spawned, 1);

        let outcomes = wait_for_outcomes(&runner, 1).await;
        assert_eq!(outcomes.len(), 1);
        assert_eq!(outcomes[0].reason, IntegrationDispatchReason::Blocked);
        assert_eq!(dispatcher.calls(), vec!["PROJ-2"]);
    }

    #[tokio::test]
    async fn mixed_ready_and_blocked_candidates_dispatch_independently() {
        let queue = Arc::new(IntegrationDispatchQueue::new());
        let dispatcher = Arc::new(RecordingDispatcher::new(
            IntegrationDispatchReason::Completed,
        ));
        dispatcher.set_reason_for("PROJ-2", IntegrationDispatchReason::Blocked);
        dispatcher.set_reason_for("PROJ-3", IntegrationDispatchReason::Failed);
        let runner = IntegrationDispatchRunner::new(queue.clone(), dispatcher.clone(), 4);

        queue.enqueue(req(1, "PROJ-1"));
        queue.enqueue(req(2, "PROJ-2"));
        queue.enqueue(req(3, "PROJ-3"));

        let report = runner.run_pending(CancellationToken::new()).await;
        assert_eq!(report.spawned, 3);

        let outcomes = wait_for_outcomes(&runner, 3).await;
        let mut by_id: Vec<(String, IntegrationDispatchReason)> = outcomes
            .into_iter()
            .map(|o| (o.parent_identifier, o.reason))
            .collect();
        by_id.sort_by(|a, b| a.0.cmp(&b.0));
        assert_eq!(
            by_id,
            vec![
                ("PROJ-1".to_string(), IntegrationDispatchReason::Completed),
                ("PROJ-2".to_string(), IntegrationDispatchReason::Blocked),
                ("PROJ-3".to_string(), IntegrationDispatchReason::Failed),
            ]
        );
    }

    #[tokio::test]
    async fn run_pending_defers_when_capacity_saturated() {
        let queue = Arc::new(IntegrationDispatchQueue::new());
        let dispatcher = Arc::new(RecordingDispatcher::new(
            IntegrationDispatchReason::Completed,
        ));
        dispatcher.enable_gate();

        let runner = IntegrationDispatchRunner::new(queue.clone(), dispatcher.clone(), 1);

        queue.enqueue(req(1, "PROJ-1"));
        queue.enqueue(req(2, "PROJ-2"));
        queue.enqueue(req(3, "PROJ-3"));

        let report = runner.run_pending(CancellationToken::new()).await;
        assert_eq!(report.spawned, 1, "report = {report:?}");
        assert_eq!(report.deferred_capacity, 2);
        assert_eq!(runner.deferred_count().await, 2);

        // Release the gate; the running dispatcher completes and the
        // permit is freed for the next pass.
        dispatcher.release_gate();
        let _ = wait_for_outcomes(&runner, 1).await;

        let report = runner.run_pending(CancellationToken::new()).await;
        assert_eq!(report.spawned, 1, "report = {report:?}");
        assert_eq!(report.deferred_capacity, 1);
        let _ = wait_for_outcomes(&runner, 1).await;

        let report = runner.run_pending(CancellationToken::new()).await;
        assert_eq!(report.spawned, 1);
        assert_eq!(report.deferred_capacity, 0);
        let _ = wait_for_outcomes(&runner, 1).await;

        let mut all = dispatcher.calls();
        all.sort();
        assert_eq!(all, vec!["PROJ-1", "PROJ-2", "PROJ-3"]);
    }

    #[tokio::test]
    async fn deferred_requests_keep_age_order_across_passes() {
        let queue = Arc::new(IntegrationDispatchQueue::new());
        let dispatcher = Arc::new(RecordingDispatcher::new(
            IntegrationDispatchReason::Completed,
        ));
        // capacity=0 forces every request into the deferred queue
        let runner = IntegrationDispatchRunner::new(queue.clone(), dispatcher.clone(), 0);

        queue.enqueue(req(1, "PROJ-1"));
        queue.enqueue(req(2, "PROJ-2"));
        let r1 = runner.run_pending(CancellationToken::new()).await;
        assert_eq!(r1.deferred_capacity, 2);

        queue.enqueue(req(3, "PROJ-3"));
        let r2 = runner.run_pending(CancellationToken::new()).await;
        assert_eq!(r2.deferred_capacity, 3);
        assert_eq!(runner.deferred_count().await, 3);

        let parked = runner.deferred.lock().await;
        let parked_ids: Vec<&str> = parked
            .iter()
            .map(|r| r.parent_identifier.as_str())
            .collect();
        assert_eq!(
            parked_ids,
            vec!["PROJ-1", "PROJ-2", "PROJ-3"],
            "deferreds must keep age-order: prior parks first, fresh request last",
        );
    }

    #[tokio::test]
    async fn parent_cancellation_propagates_to_dispatch() {
        let queue = Arc::new(IntegrationDispatchQueue::new());
        let dispatcher = Arc::new(RecordingDispatcher::new(
            IntegrationDispatchReason::Completed,
        ));
        dispatcher.enable_gate();

        let runner = IntegrationDispatchRunner::new(queue.clone(), dispatcher.clone(), 4);
        queue.enqueue(req(1, "PROJ-1"));

        let cancel = CancellationToken::new();
        let _ = runner.run_pending(cancel.clone()).await;
        assert_eq!(runner.inflight_count().await, 1);

        cancel.cancel();

        let outcomes = wait_for_outcomes(&runner, 1).await;
        assert_eq!(outcomes.len(), 1);
        assert_eq!(outcomes[0].reason, IntegrationDispatchReason::Canceled);
    }

    #[tokio::test]
    async fn capacity_zero_defers_everything() {
        let queue = Arc::new(IntegrationDispatchQueue::new());
        let dispatcher = Arc::new(RecordingDispatcher::new(
            IntegrationDispatchReason::Completed,
        ));
        let runner = IntegrationDispatchRunner::new(queue.clone(), dispatcher.clone(), 0);
        queue.enqueue(req(1, "PROJ-1"));

        let report = runner.run_pending(CancellationToken::new()).await;
        assert_eq!(report.spawned, 0);
        assert_eq!(report.deferred_capacity, 1);
        assert!(dispatcher.calls().is_empty());
    }

    #[tokio::test]
    async fn reap_returns_outcomes_for_every_completed_dispatch() {
        let queue = Arc::new(IntegrationDispatchQueue::new());
        let dispatcher = Arc::new(RecordingDispatcher::new(
            IntegrationDispatchReason::Completed,
        ));
        let runner = IntegrationDispatchRunner::new(queue.clone(), dispatcher.clone(), 4);

        queue.enqueue(req(1, "PROJ-1"));
        queue.enqueue(req(2, "PROJ-2"));
        let report = runner.run_pending(CancellationToken::new()).await;
        assert_eq!(report.spawned, 2);

        let outcomes = wait_for_outcomes(&runner, 2).await;
        let mut ids: Vec<i64> = outcomes.iter().map(|o| o.parent_id.get()).collect();
        ids.sort();
        assert_eq!(ids, vec![1, 2]);
    }

    #[test]
    fn dispatch_reason_round_trips_through_json() {
        for r in [
            IntegrationDispatchReason::Completed,
            IntegrationDispatchReason::Blocked,
            IntegrationDispatchReason::Canceled,
            IntegrationDispatchReason::Failed,
        ] {
            let json = serde_json::to_string(&r).unwrap();
            let back: IntegrationDispatchReason = serde_json::from_str(&json).unwrap();
            assert_eq!(back, r);
        }
    }
}
