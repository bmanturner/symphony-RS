//! QA dispatch runner — drains a [`QaDispatchQueue`] and invokes a
//! per-work-item [`QaDispatcher`] for each pending request, bounded by a
//! max-concurrency [`Semaphore`] (SPEC v2 §6.5 / ARCHITECTURE v2 §7.1, §10).
//!
//! Mirrors the bounded-concurrency contract of
//! [`crate::integration_runner::IntegrationDispatchRunner`] and
//! [`crate::specialist_runner::SpecialistRunner`]:
//!
//! 1. Drains pending requests from [`QaDispatchQueue`] and folds them
//!    onto the runner's deferred-from-prior-pass queue (front-first so
//!    age-order is preserved across passes).
//! 2. For each request, tries to acquire a semaphore permit
//!    non-blockingly. If unavailable the request is parked back in the
//!    runner's deferred queue and counted as `deferred_capacity`.
//!    Permit-holding requests spawn a dispatcher task into a
//!    [`JoinSet`]; the spawned future holds the owned permit until the
//!    dispatcher returns, so capacity is automatically restored on
//!    completion.
//!
//! Like the integration runner, QA requests are pre-gated upstream by
//! [`crate::QaQueueTick`] (it consults the durable `QaQueueRepository`
//! view under the workflow's [`crate::QaGates`]). The runner therefore
//! does not perform its own gate check or active-set lookup — every
//! drained request is authoritative. The QA dispatcher's verdict is
//! signalled by the returned [`QaDispatchReason`], which mirrors the
//! [`crate::QaVerdict`] variants plus the transport-level outcomes
//! (`Canceled`, `Failed`).

use std::sync::Arc;

use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use tokio::sync::{Mutex, Semaphore};
use tokio::task::JoinSet;
use tokio_util::sync::CancellationToken;
use tracing::{debug, warn};

use crate::qa_tick::{QaDispatchQueue, QaDispatchRequest};
use crate::work_item::WorkItemId;

/// Final outcome of one QA-gate dispatch.
///
/// The five "verdict" variants mirror [`crate::QaVerdict`]; the runner
/// does not interpret them — it only records what the dispatcher
/// returned. Downstream wiring (Phase 11 scheduler hookup, Phase 12
/// events) will translate into work-item state transitions, blocker
/// creation, rework routing, and observability events.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum QaDispatchReason {
    /// QA verified all acceptance criteria. Downstream may promote the
    /// work item past `qa` per workflow policy.
    Passed,
    /// QA rejected the work and filed at least one blocker. Blockers
    /// gate parent completion per SPEC §8.2 and route the parent back
    /// into the integration/rework cycle.
    FailedWithBlockers,
    /// QA rejected the work without a structural blocker. Routes back
    /// to the prior role for rework.
    FailedNeedsRework,
    /// QA could not reach a decision (missing evidence, environment
    /// failure, …). The work item stays in a QA-pending state.
    Inconclusive,
    /// A configured waiver role intentionally accepted a non-passing
    /// outcome. SPEC §5.12 — requires a waiver role and a reason; the
    /// runner only records the verdict, the validation lives in
    /// [`crate::QaOutcome::try_new`] and [`crate::validate_qa_waiver`].
    Waived,
    /// Cooperative cancellation — parent token fired (e.g. SIGINT).
    Canceled,
    /// Dispatcher failed for a non-verdict reason (transport, internal
    /// error, …). The scheduler will requeue per retry policy.
    Failed,
}

/// Outcome reported when a spawned QA dispatcher task completes.
///
/// Returned in batches by [`QaDispatchRunner::reap_completed`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct QaDispatchOutcome {
    /// Durable work item id the request carried.
    pub work_item_id: WorkItemId,
    /// Tracker-facing identifier (e.g. `PROJ-42`).
    pub identifier: String,
    /// Final [`QaDispatchReason`] from the dispatcher.
    pub reason: QaDispatchReason,
}

/// Pluggable handler invoked by the runner for each pending
/// [`QaDispatchRequest`].
///
/// The composition root will wire this to the QA-gate agent runner
/// (workspace claim → cwd/branch verification → agent session →
/// verdict persistence + blocker filing). Tests substitute a
/// deterministic recording implementation.
#[async_trait]
pub trait QaDispatcher: Send + Sync + 'static {
    /// Run one full QA-gate session for `request`. Resolves when the
    /// session ends — with a verdict, in error, or because `cancel`
    /// fired.
    async fn dispatch(
        &self,
        request: QaDispatchRequest,
        cancel: CancellationToken,
    ) -> QaDispatchReason;
}

/// Per-pass observability summary for [`QaDispatchRunner::run_pending`].
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct QaRunReport {
    /// Requests for which a dispatcher task was spawned this pass.
    pub spawned: usize,
    /// Requests that could not be spawned because the semaphore was
    /// already saturated. Parked on the runner's internal deferred
    /// queue for the next pass.
    pub deferred_capacity: usize,
}

impl QaRunReport {
    /// `true` iff the pass was a complete no-op.
    pub fn is_idle(&self) -> bool {
        self.spawned == 0 && self.deferred_capacity == 0
    }
}

/// Bounded-concurrency consumer for [`QaDispatchQueue`].
pub struct QaDispatchRunner {
    queue: Arc<QaDispatchQueue>,
    dispatcher: Arc<dyn QaDispatcher>,
    semaphore: Arc<Semaphore>,
    deferred: Mutex<Vec<QaDispatchRequest>>,
    inflight: Mutex<JoinSet<QaDispatchOutcome>>,
    max_concurrent: usize,
}

impl QaDispatchRunner {
    /// Build a runner with capacity `max_concurrent`. A capacity of `0`
    /// disables dispatch entirely — every drained request will be
    /// counted as `deferred_capacity`.
    pub fn new(
        queue: Arc<QaDispatchQueue>,
        dispatcher: Arc<dyn QaDispatcher>,
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
    /// SIGINT-style shutdown tears down in-flight QA sessions
    /// cooperatively. The pass itself does not await the spawned
    /// futures; call [`Self::reap_completed`] to observe verdicts.
    pub async fn run_pending(&self, cancel: CancellationToken) -> QaRunReport {
        let mut work: Vec<QaDispatchRequest> = {
            let mut deferred = self.deferred.lock().await;
            std::mem::take(&mut *deferred)
        };
        work.extend(self.queue.drain());

        if work.is_empty() {
            return QaRunReport::default();
        }

        let mut report = QaRunReport::default();
        let mut new_deferred: Vec<QaDispatchRequest> = Vec::new();

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
            let work_item_id = req.work_item_id;
            let identifier = req.identifier.clone();
            let req_for_dispatch = req.clone();

            let mut inflight = self.inflight.lock().await;
            inflight.spawn(async move {
                let reason = dispatcher.dispatch(req_for_dispatch, child_cancel).await;
                drop(permit);
                QaDispatchOutcome {
                    work_item_id,
                    identifier,
                    reason,
                }
            });
            drop(inflight);

            debug!(
                work_item_id = work_item_id.get(),
                identifier = %req.identifier,
                "qa runner: spawned dispatch",
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
    pub async fn reap_completed(&self) -> Vec<QaDispatchOutcome> {
        let mut inflight = self.inflight.lock().await;
        let mut out = Vec::new();
        while let Some(joined) = inflight.try_join_next() {
            match joined {
                Ok(outcome) => out.push(outcome),
                Err(err) if err.is_cancelled() => {
                    debug!("qa dispatch task cancelled before reap");
                }
                Err(err) => {
                    warn!(error = %err, "qa dispatch task failed to join cleanly");
                }
            }
        }
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::qa_request::QaRequestCause;
    use std::sync::Mutex as StdMutex;
    use std::time::Duration;

    fn req(id: i64, identifier: &str) -> QaDispatchRequest {
        QaDispatchRequest {
            work_item_id: WorkItemId::new(id),
            identifier: identifier.into(),
            title: format!("title {identifier}"),
            cause: QaRequestCause::DirectQaRequest,
        }
    }

    /// Records every dispatch invocation and resolves with a configured
    /// reason (per-identifier override). Optional gate lets tests hold
    /// a dispatch open to assert capacity behaviour.
    struct RecordingDispatcher {
        calls: Arc<StdMutex<Vec<String>>>,
        gate: Arc<tokio::sync::Notify>,
        gate_enabled: Arc<StdMutex<bool>>,
        default_reason: QaDispatchReason,
        per_identifier: Arc<StdMutex<Vec<(String, QaDispatchReason)>>>,
    }

    impl RecordingDispatcher {
        fn new(default_reason: QaDispatchReason) -> Self {
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

        fn set_reason_for(&self, identifier: &str, reason: QaDispatchReason) {
            self.per_identifier
                .lock()
                .unwrap()
                .push((identifier.into(), reason));
        }
    }

    #[async_trait]
    impl QaDispatcher for RecordingDispatcher {
        async fn dispatch(
            &self,
            request: QaDispatchRequest,
            cancel: CancellationToken,
        ) -> QaDispatchReason {
            self.calls.lock().unwrap().push(request.identifier.clone());

            let gated = *self.gate_enabled.lock().unwrap();
            if gated {
                tokio::select! {
                    _ = self.gate.notified() => {}
                    _ = cancel.cancelled() => return QaDispatchReason::Canceled,
                }
            }

            let per = self.per_identifier.lock().unwrap();
            for (id, reason) in per.iter() {
                if id == &request.identifier {
                    return *reason;
                }
            }
            self.default_reason
        }
    }

    async fn wait_for_outcomes(runner: &QaDispatchRunner, n: usize) -> Vec<QaDispatchOutcome> {
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
        let queue = Arc::new(QaDispatchQueue::new());
        let dispatcher = Arc::new(RecordingDispatcher::new(QaDispatchReason::Passed));
        let runner = QaDispatchRunner::new(queue, dispatcher.clone(), 4);

        let report = runner.run_pending(CancellationToken::new()).await;
        assert!(report.is_idle(), "expected idle, got {report:?}");
        assert_eq!(runner.inflight_count().await, 0);
        assert!(dispatcher.calls().is_empty());
    }

    #[tokio::test]
    async fn ready_candidate_is_dispatched_to_passed_verdict() {
        let queue = Arc::new(QaDispatchQueue::new());
        let dispatcher = Arc::new(RecordingDispatcher::new(QaDispatchReason::Passed));
        let runner = QaDispatchRunner::new(queue.clone(), dispatcher.clone(), 4);

        // The upstream tick has already gated this candidate: the
        // runner simply spawns a dispatcher task for it.
        queue.enqueue(req(1, "PROJ-1"));

        let report = runner.run_pending(CancellationToken::new()).await;
        assert_eq!(report.spawned, 1);
        assert_eq!(report.deferred_capacity, 0);

        let outcomes = wait_for_outcomes(&runner, 1).await;
        assert_eq!(outcomes.len(), 1);
        assert_eq!(outcomes[0].work_item_id, WorkItemId::new(1));
        assert_eq!(outcomes[0].identifier, "PROJ-1");
        assert_eq!(outcomes[0].reason, QaDispatchReason::Passed);
        assert_eq!(dispatcher.calls(), vec!["PROJ-1"]);
    }

    #[tokio::test]
    async fn blocked_candidate_surfaces_failed_with_blockers_reason() {
        // The upstream tick may waive the blocker gate (per workflow
        // policy) and surface a candidate whose QA session ultimately
        // resolves as `failed_with_blockers` because the QA-gate role
        // filed new blockers. The runner only reports the dispatcher's
        // verdict; verdict persistence and rework routing are
        // downstream.
        let queue = Arc::new(QaDispatchQueue::new());
        let dispatcher = Arc::new(RecordingDispatcher::new(QaDispatchReason::Passed));
        dispatcher.set_reason_for("PROJ-2", QaDispatchReason::FailedWithBlockers);
        let runner = QaDispatchRunner::new(queue.clone(), dispatcher.clone(), 4);

        queue.enqueue(req(2, "PROJ-2"));
        let report = runner.run_pending(CancellationToken::new()).await;
        assert_eq!(report.spawned, 1);

        let outcomes = wait_for_outcomes(&runner, 1).await;
        assert_eq!(outcomes.len(), 1);
        assert_eq!(outcomes[0].reason, QaDispatchReason::FailedWithBlockers);
        assert_eq!(dispatcher.calls(), vec!["PROJ-2"]);
    }

    #[tokio::test]
    async fn waiver_routed_verdict_surfaces_waived_reason() {
        // SPEC §5.12 — a configured waiver role may intentionally
        // accept a non-passing outcome. The runner must propagate the
        // dispatcher's `Waived` verdict verbatim; downstream policy
        // (`validate_qa_waiver`) enforces that the persisted verdict
        // carries a waiver role and reason.
        let queue = Arc::new(QaDispatchQueue::new());
        let dispatcher = Arc::new(RecordingDispatcher::new(QaDispatchReason::Passed));
        dispatcher.set_reason_for("PROJ-7", QaDispatchReason::Waived);
        let runner = QaDispatchRunner::new(queue.clone(), dispatcher.clone(), 4);

        queue.enqueue(req(7, "PROJ-7"));
        let report = runner.run_pending(CancellationToken::new()).await;
        assert_eq!(report.spawned, 1);

        let outcomes = wait_for_outcomes(&runner, 1).await;
        assert_eq!(outcomes.len(), 1);
        assert_eq!(outcomes[0].reason, QaDispatchReason::Waived);
        assert_eq!(outcomes[0].work_item_id, WorkItemId::new(7));
        assert_eq!(dispatcher.calls(), vec!["PROJ-7"]);
    }

    #[tokio::test]
    async fn mixed_verdicts_are_dispatched_independently() {
        let queue = Arc::new(QaDispatchQueue::new());
        let dispatcher = Arc::new(RecordingDispatcher::new(QaDispatchReason::Passed));
        dispatcher.set_reason_for("PROJ-2", QaDispatchReason::FailedWithBlockers);
        dispatcher.set_reason_for("PROJ-3", QaDispatchReason::FailedNeedsRework);
        dispatcher.set_reason_for("PROJ-4", QaDispatchReason::Inconclusive);
        dispatcher.set_reason_for("PROJ-5", QaDispatchReason::Waived);
        let runner = QaDispatchRunner::new(queue.clone(), dispatcher.clone(), 8);

        queue.enqueue(req(1, "PROJ-1"));
        queue.enqueue(req(2, "PROJ-2"));
        queue.enqueue(req(3, "PROJ-3"));
        queue.enqueue(req(4, "PROJ-4"));
        queue.enqueue(req(5, "PROJ-5"));

        let report = runner.run_pending(CancellationToken::new()).await;
        assert_eq!(report.spawned, 5);

        let outcomes = wait_for_outcomes(&runner, 5).await;
        let mut by_id: Vec<(String, QaDispatchReason)> = outcomes
            .into_iter()
            .map(|o| (o.identifier, o.reason))
            .collect();
        by_id.sort_by(|a, b| a.0.cmp(&b.0));
        assert_eq!(
            by_id,
            vec![
                ("PROJ-1".to_string(), QaDispatchReason::Passed),
                ("PROJ-2".to_string(), QaDispatchReason::FailedWithBlockers),
                ("PROJ-3".to_string(), QaDispatchReason::FailedNeedsRework),
                ("PROJ-4".to_string(), QaDispatchReason::Inconclusive),
                ("PROJ-5".to_string(), QaDispatchReason::Waived),
            ]
        );
    }

    #[tokio::test]
    async fn run_pending_defers_when_capacity_saturated() {
        let queue = Arc::new(QaDispatchQueue::new());
        let dispatcher = Arc::new(RecordingDispatcher::new(QaDispatchReason::Passed));
        dispatcher.enable_gate();

        let runner = QaDispatchRunner::new(queue.clone(), dispatcher.clone(), 1);

        queue.enqueue(req(1, "PROJ-1"));
        queue.enqueue(req(2, "PROJ-2"));
        queue.enqueue(req(3, "PROJ-3"));

        let report = runner.run_pending(CancellationToken::new()).await;
        assert_eq!(report.spawned, 1, "report = {report:?}");
        assert_eq!(report.deferred_capacity, 2);
        assert_eq!(runner.deferred_count().await, 2);

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
        let queue = Arc::new(QaDispatchQueue::new());
        let dispatcher = Arc::new(RecordingDispatcher::new(QaDispatchReason::Passed));
        // capacity=0 forces every request into the deferred queue
        let runner = QaDispatchRunner::new(queue.clone(), dispatcher.clone(), 0);

        queue.enqueue(req(1, "PROJ-1"));
        queue.enqueue(req(2, "PROJ-2"));
        let r1 = runner.run_pending(CancellationToken::new()).await;
        assert_eq!(r1.deferred_capacity, 2);

        queue.enqueue(req(3, "PROJ-3"));
        let r2 = runner.run_pending(CancellationToken::new()).await;
        assert_eq!(r2.deferred_capacity, 3);
        assert_eq!(runner.deferred_count().await, 3);

        let parked = runner.deferred.lock().await;
        let parked_ids: Vec<&str> = parked.iter().map(|r| r.identifier.as_str()).collect();
        assert_eq!(
            parked_ids,
            vec!["PROJ-1", "PROJ-2", "PROJ-3"],
            "deferreds must keep age-order: prior parks first, fresh request last",
        );
    }

    #[tokio::test]
    async fn parent_cancellation_propagates_to_dispatch() {
        let queue = Arc::new(QaDispatchQueue::new());
        let dispatcher = Arc::new(RecordingDispatcher::new(QaDispatchReason::Passed));
        dispatcher.enable_gate();

        let runner = QaDispatchRunner::new(queue.clone(), dispatcher.clone(), 4);
        queue.enqueue(req(1, "PROJ-1"));

        let cancel = CancellationToken::new();
        let _ = runner.run_pending(cancel.clone()).await;
        assert_eq!(runner.inflight_count().await, 1);

        cancel.cancel();

        let outcomes = wait_for_outcomes(&runner, 1).await;
        assert_eq!(outcomes.len(), 1);
        assert_eq!(outcomes[0].reason, QaDispatchReason::Canceled);
    }

    #[tokio::test]
    async fn capacity_zero_defers_everything() {
        let queue = Arc::new(QaDispatchQueue::new());
        let dispatcher = Arc::new(RecordingDispatcher::new(QaDispatchReason::Passed));
        let runner = QaDispatchRunner::new(queue.clone(), dispatcher.clone(), 0);
        queue.enqueue(req(1, "PROJ-1"));

        let report = runner.run_pending(CancellationToken::new()).await;
        assert_eq!(report.spawned, 0);
        assert_eq!(report.deferred_capacity, 1);
        assert!(dispatcher.calls().is_empty());
    }

    #[tokio::test]
    async fn reap_returns_outcomes_for_every_completed_dispatch() {
        let queue = Arc::new(QaDispatchQueue::new());
        let dispatcher = Arc::new(RecordingDispatcher::new(QaDispatchReason::Passed));
        let runner = QaDispatchRunner::new(queue.clone(), dispatcher.clone(), 4);

        queue.enqueue(req(1, "PROJ-1"));
        queue.enqueue(req(2, "PROJ-2"));
        let report = runner.run_pending(CancellationToken::new()).await;
        assert_eq!(report.spawned, 2);

        let outcomes = wait_for_outcomes(&runner, 2).await;
        let mut ids: Vec<i64> = outcomes.iter().map(|o| o.work_item_id.get()).collect();
        ids.sort();
        assert_eq!(ids, vec![1, 2]);
    }

    #[test]
    fn dispatch_reason_round_trips_through_json() {
        for r in [
            QaDispatchReason::Passed,
            QaDispatchReason::FailedWithBlockers,
            QaDispatchReason::FailedNeedsRework,
            QaDispatchReason::Inconclusive,
            QaDispatchReason::Waived,
            QaDispatchReason::Canceled,
            QaDispatchReason::Failed,
        ] {
            let json = serde_json::to_string(&r).unwrap();
            let back: QaDispatchReason = serde_json::from_str(&json).unwrap();
            assert_eq!(back, r);
        }
    }
}
