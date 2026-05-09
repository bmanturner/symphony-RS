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
use crate::lease::{LeaseClock, LeaseConfig, LeaseOwner};
use crate::run_lease::{LeaseAcquireOutcome, RunLeaseGuard, RunLeaseStore};
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
    /// Requests whose lease acquisition was rejected with
    /// [`LeaseAcquireOutcome::Contended`]. Parked on the runner's
    /// deferred queue so a future pass (after lease expiry or release)
    /// can retry.
    pub deferred_lease_contention: usize,
    /// Requests whose lease acquisition reported
    /// [`LeaseAcquireOutcome::NotFound`] (durable run row missing).
    /// Dropped — the producer is responsible for ensuring run rows
    /// exist before flagging `run_id` on a request.
    pub missing_run: usize,
    /// Backend errors surfaced by the lease store during acquisition.
    /// The request is parked on the deferred queue so a transient I/O
    /// blip does not lose the dispatch.
    pub lease_backend_error: usize,
}

impl IntegrationRunReport {
    /// `true` iff the pass was a complete no-op.
    pub fn is_idle(&self) -> bool {
        self.spawned == 0
            && self.deferred_capacity == 0
            && self.deferred_lease_contention == 0
            && self.missing_run == 0
            && self.lease_backend_error == 0
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
    /// Optional durable-lease wiring. When `Some`, the runner acquires a
    /// lease on `request.run_id` before spawning the dispatcher and
    /// releases it on terminal completion. Requests whose `run_id` is
    /// `None` bypass lease acquisition.
    leasing: Option<LeaseWiring>,
}

/// Bundle holding the lease store, owner identity, TTL/renewal config,
/// and the clock used to derive `(now, expires_at)` per acquisition.
struct LeaseWiring {
    store: Arc<dyn RunLeaseStore>,
    owner: LeaseOwner,
    config: LeaseConfig,
    clock: Arc<dyn LeaseClock>,
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
            leasing: None,
        }
    }

    /// Enable durable-lease wiring. Each subsequent dispatch whose
    /// request carries a `run_id` will acquire the lease on `run_id`,
    /// spawn the dispatcher only after acquisition succeeds, and release
    /// the lease at terminal completion regardless of the dispatcher's
    /// [`IntegrationDispatchReason`]. Requests without a `run_id` are
    /// dispatched as before.
    ///
    /// Calling this method more than once replaces the prior wiring.
    pub fn with_leasing(
        mut self,
        store: Arc<dyn RunLeaseStore>,
        owner: LeaseOwner,
        config: LeaseConfig,
        clock: Arc<dyn LeaseClock>,
    ) -> Self {
        self.leasing = Some(LeaseWiring {
            store,
            owner,
            config,
            clock,
        });
        self
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

            // Durable-lease acquisition. Held through the spawned
            // dispatch and released on terminal completion. Permit is
            // only consumed once the lease is acquired so contention or
            // backend errors do not eat capacity.
            let guard = match self.try_acquire_lease(&req).await {
                LeaseAttempt::Acquired(guard) => guard,
                LeaseAttempt::Skipped => None,
                LeaseAttempt::Contended => {
                    drop(permit);
                    new_deferred.push(req);
                    report.deferred_lease_contention += 1;
                    continue;
                }
                LeaseAttempt::NotFound => {
                    drop(permit);
                    report.missing_run += 1;
                    continue;
                }
                LeaseAttempt::BackendError => {
                    drop(permit);
                    new_deferred.push(req);
                    report.lease_backend_error += 1;
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
                if let Some(guard) = guard
                    && let Err(err) = guard.release().await
                {
                    warn!(
                        parent_identifier = %parent_identifier,
                        error = %err,
                        "integration runner: lease release failed; relying on TTL reaper",
                    );
                }
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

    async fn try_acquire_lease(&self, req: &IntegrationDispatchRequest) -> LeaseAttempt {
        let Some(wiring) = &self.leasing else {
            return LeaseAttempt::Acquired(None);
        };
        let Some(run_id) = req.run_id else {
            return LeaseAttempt::Skipped;
        };

        let ts = wiring.clock.timestamps(wiring.config.default_ttl_ms);
        match RunLeaseGuard::acquire(
            wiring.store.clone(),
            run_id,
            &wiring.owner,
            &ts.expires_at,
            &ts.now,
        )
        .await
        {
            Ok(Ok(guard)) => LeaseAttempt::Acquired(Some(guard)),
            Ok(Err(LeaseAcquireOutcome::Contended { holder, expires_at })) => {
                debug!(
                    parent_identifier = %req.parent_identifier,
                    holder = %holder,
                    expires_at = %expires_at,
                    "integration runner: lease contended; parking",
                );
                LeaseAttempt::Contended
            }
            Ok(Err(LeaseAcquireOutcome::NotFound)) => {
                warn!(
                    parent_identifier = %req.parent_identifier,
                    run_id = %run_id.get(),
                    "integration runner: durable run row missing; dropping request",
                );
                LeaseAttempt::NotFound
            }
            Ok(Err(LeaseAcquireOutcome::Acquired)) => {
                unreachable!("RunLeaseGuard::acquire returns Ok(Ok(_)) on Acquired")
            }
            Err(err) => {
                warn!(
                    parent_identifier = %req.parent_identifier,
                    error = %err,
                    "integration runner: lease store backend error; parking",
                );
                LeaseAttempt::BackendError
            }
        }
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

/// Outcome of [`IntegrationDispatchRunner::try_acquire_lease`].
enum LeaseAttempt {
    /// Either lease acquired (`Some(guard)`) or no run_id on the
    /// request and lease wiring not exercised (`None`). Both cases
    /// proceed to dispatch.
    Acquired(Option<RunLeaseGuard>),
    /// Lease wiring is configured but the request had no `run_id`
    /// reservation. Dispatch proceeds without a lease guard.
    Skipped,
    /// Acquisition rejected — another owner holds an unexpired lease.
    Contended,
    /// The durable run row is missing.
    NotFound,
    /// The lease store reported a backend error (I/O, lock, encoding).
    BackendError,
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
            run_id: None,
        }
    }

    fn req_with_run(id: i64, identifier: &str, run_id: i64) -> IntegrationDispatchRequest {
        IntegrationDispatchRequest {
            parent_id: WorkItemId::new(id),
            parent_identifier: identifier.into(),
            parent_title: format!("title {identifier}"),
            cause: IntegrationRequestCause::DirectIntegrationRequest,
            run_id: Some(crate::blocker::RunRef::new(run_id)),
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

    // --- Lease wiring (CHECKLIST_v2 Phase 11) -----------------------

    use crate::blocker::RunRef;
    use crate::lease::{FixedLeaseClock, LeaseConfig, LeaseOwner};
    use crate::run_lease::InMemoryRunLeaseStore;

    fn lease_owner() -> LeaseOwner {
        LeaseOwner::new("test-host", "scheduler-test", 0).unwrap()
    }

    fn build_lease_runner(
        max_concurrent: usize,
        dispatcher: Arc<RecordingDispatcher>,
        store: Arc<InMemoryRunLeaseStore>,
        clock_now: &str,
    ) -> (Arc<IntegrationDispatchQueue>, IntegrationDispatchRunner) {
        let queue = Arc::new(IntegrationDispatchQueue::new());
        let runner = IntegrationDispatchRunner::new(queue.clone(), dispatcher, max_concurrent)
            .with_leasing(
                store,
                lease_owner(),
                LeaseConfig::default(),
                Arc::new(FixedLeaseClock::new(clock_now)),
            );
        (queue, runner)
    }

    #[tokio::test]
    async fn lease_acquired_during_dispatch_and_released_on_completed_ready_candidate() {
        let dispatcher = Arc::new(RecordingDispatcher::new(
            IntegrationDispatchReason::Completed,
        ));
        dispatcher.enable_gate(); // hold dispatch open so we can observe the in-flight lease
        let store = Arc::new(InMemoryRunLeaseStore::new());
        store.register_run(RunRef::new(42)).await;

        let (queue, runner) = build_lease_runner(4, dispatcher.clone(), store.clone(), "T0");
        queue.enqueue(req_with_run(1, "PROJ-1", 42));

        let report = runner.run_pending(CancellationToken::new()).await;
        assert_eq!(report.spawned, 1);
        assert_eq!(report.deferred_lease_contention, 0);
        assert_eq!(report.missing_run, 0);

        // Lease is held while the dispatcher is gated.
        let snap = store
            .snapshot(RunRef::new(42))
            .await
            .expect("lease should be held during dispatch");
        assert_eq!(snap.0, lease_owner().to_string());
        assert!(
            snap.1.starts_with("T0+"),
            "expires_at should derive from FixedLeaseClock 'now'; got {}",
            snap.1,
        );

        // Release the gate so the dispatcher returns; the spawned task
        // releases the lease as part of terminal completion.
        dispatcher.release_gate();
        let outcomes = wait_for_outcomes(&runner, 1).await;
        assert_eq!(outcomes[0].reason, IntegrationDispatchReason::Completed);
        assert!(
            store.snapshot(RunRef::new(42)).await.is_none(),
            "lease must be cleared on Completed terminal release",
        );
        assert_eq!(store.release_calls().await, vec![RunRef::new(42)]);
    }

    #[tokio::test]
    async fn lease_cleared_on_blocked_terminal_release() {
        // Blocked is the integration-runner equivalent of a "non-ready"
        // dispatch outcome — typically because the integration owner
        // filed a blocker mid-session. The lease must still clear.
        let dispatcher = Arc::new(RecordingDispatcher::new(
            IntegrationDispatchReason::Completed,
        ));
        dispatcher.set_reason_for("PROJ-2", IntegrationDispatchReason::Blocked);
        let store = Arc::new(InMemoryRunLeaseStore::new());
        store.register_run(RunRef::new(7)).await;

        let (queue, runner) = build_lease_runner(4, dispatcher.clone(), store.clone(), "T1");
        queue.enqueue(req_with_run(2, "PROJ-2", 7));

        let report = runner.run_pending(CancellationToken::new()).await;
        assert_eq!(report.spawned, 1);
        let outcomes = wait_for_outcomes(&runner, 1).await;
        assert_eq!(outcomes[0].reason, IntegrationDispatchReason::Blocked);
        assert!(store.snapshot(RunRef::new(7)).await.is_none());
        assert_eq!(store.release_calls().await, vec![RunRef::new(7)]);
    }

    #[tokio::test]
    async fn lease_cleared_on_canceled_terminal_release() {
        let dispatcher = Arc::new(RecordingDispatcher::new(
            IntegrationDispatchReason::Completed,
        ));
        dispatcher.enable_gate();
        let store = Arc::new(InMemoryRunLeaseStore::new());
        store.register_run(RunRef::new(11)).await;

        let (queue, runner) = build_lease_runner(4, dispatcher.clone(), store.clone(), "T2");
        queue.enqueue(req_with_run(3, "PROJ-3", 11));

        let cancel = CancellationToken::new();
        let _ = runner.run_pending(cancel.clone()).await;
        assert!(store.snapshot(RunRef::new(11)).await.is_some());
        cancel.cancel();
        let outcomes = wait_for_outcomes(&runner, 1).await;
        assert_eq!(outcomes[0].reason, IntegrationDispatchReason::Canceled);
        assert!(store.snapshot(RunRef::new(11)).await.is_none());
    }

    #[tokio::test]
    async fn lease_cleared_on_failed_terminal_release_for_waived_dispatch() {
        // "Waived" gating happens upstream in the integration tick — the
        // runner sees a request that the producer chose to surface
        // despite a blocker subtree. The dispatcher reports Failed for a
        // non-blocker error; the lease must still clear.
        let dispatcher = Arc::new(RecordingDispatcher::new(IntegrationDispatchReason::Failed));
        let store = Arc::new(InMemoryRunLeaseStore::new());
        store.register_run(RunRef::new(13)).await;

        let (queue, runner) = build_lease_runner(4, dispatcher.clone(), store.clone(), "T3");
        // AllChildrenTerminal cause models the consolidation path the
        // upstream tick uses for waived/all-children-done parents.
        queue.enqueue(IntegrationDispatchRequest {
            parent_id: WorkItemId::new(4),
            parent_identifier: "PROJ-4".into(),
            parent_title: "consolidate".into(),
            cause: IntegrationRequestCause::AllChildrenTerminal,
            run_id: Some(RunRef::new(13)),
        });

        let report = runner.run_pending(CancellationToken::new()).await;
        assert_eq!(report.spawned, 1);
        let outcomes = wait_for_outcomes(&runner, 1).await;
        assert_eq!(outcomes[0].reason, IntegrationDispatchReason::Failed);
        assert!(store.snapshot(RunRef::new(13)).await.is_none());
    }

    #[tokio::test]
    async fn lease_visible_to_expired_scan_only_after_ttl() {
        let dispatcher = Arc::new(RecordingDispatcher::new(
            IntegrationDispatchReason::Completed,
        ));
        dispatcher.enable_gate();
        let store = Arc::new(InMemoryRunLeaseStore::new());
        store.register_run(RunRef::new(99)).await;

        let (queue, runner) = build_lease_runner(4, dispatcher.clone(), store.clone(), "T4");
        queue.enqueue(req_with_run(5, "PROJ-5", 99));

        let _ = runner.run_pending(CancellationToken::new()).await;
        let snap = store.snapshot(RunRef::new(99)).await.unwrap();

        // Pre-TTL contention by another owner.
        let other_owner = LeaseOwner::new("other-host", "scheduler-test", 1).unwrap();
        let pre_ttl = store
            .acquire(RunRef::new(99), &other_owner, "T4+later", "T4")
            .await
            .unwrap();
        match pre_ttl {
            crate::run_lease::LeaseAcquireOutcome::Contended { holder, .. } => {
                assert_eq!(holder, lease_owner().to_string());
            }
            other => panic!("expected Contended pre-TTL, got {other:?}"),
        }

        // Post-TTL takeover succeeds.
        let after_ttl = format!("{}+999999999999ms", "T4");
        assert!(after_ttl > snap.1);
        let post = store
            .acquire(RunRef::new(99), &other_owner, "T9+later", &after_ttl)
            .await
            .unwrap();
        assert_eq!(post, crate::run_lease::LeaseAcquireOutcome::Acquired);

        // Cleanup: release the gate so the spawned task can finish.
        dispatcher.release_gate();
        let _ = wait_for_outcomes(&runner, 1).await;
    }

    #[tokio::test]
    async fn contended_lease_parks_request_and_does_not_consume_capacity() {
        let dispatcher = Arc::new(RecordingDispatcher::new(
            IntegrationDispatchReason::Completed,
        ));
        let store = Arc::new(InMemoryRunLeaseStore::new());
        store.register_run(RunRef::new(1)).await;
        store.register_run(RunRef::new(2)).await;

        // Pre-claim run 1 by another owner so the runner sees contention.
        let other = LeaseOwner::new("other-host", "scheduler-test", 9).unwrap();
        let _ = store
            .acquire(RunRef::new(1), &other, "T0+999ms", "T0")
            .await
            .unwrap();

        let (queue, runner) = build_lease_runner(2, dispatcher.clone(), store.clone(), "T0");
        queue.enqueue(req_with_run(1, "PROJ-1", 1));
        queue.enqueue(req_with_run(2, "PROJ-2", 2));

        let report = runner.run_pending(CancellationToken::new()).await;
        assert_eq!(report.spawned, 1);
        assert_eq!(report.deferred_lease_contention, 1);
        assert_eq!(runner.deferred_count().await, 1);
        let _ = wait_for_outcomes(&runner, 1).await;
    }

    #[tokio::test]
    async fn missing_run_drops_request_without_dispatch() {
        let dispatcher = Arc::new(RecordingDispatcher::new(
            IntegrationDispatchReason::Completed,
        ));
        let store = Arc::new(InMemoryRunLeaseStore::new()); // run not registered

        let (queue, runner) = build_lease_runner(4, dispatcher.clone(), store, "T0");
        queue.enqueue(req_with_run(1, "PROJ-1", 404));

        let report = runner.run_pending(CancellationToken::new()).await;
        assert_eq!(report.spawned, 0);
        assert_eq!(report.missing_run, 1);
        assert!(dispatcher.calls().is_empty());
    }

    #[tokio::test]
    async fn request_without_run_id_dispatches_without_lease_acquisition() {
        let dispatcher = Arc::new(RecordingDispatcher::new(
            IntegrationDispatchReason::Completed,
        ));
        let store = Arc::new(InMemoryRunLeaseStore::new());

        let (queue, runner) = build_lease_runner(4, dispatcher.clone(), store.clone(), "T0");
        // No run_id on the request; lease wiring should be skipped.
        queue.enqueue(req(1, "PROJ-1"));

        let report = runner.run_pending(CancellationToken::new()).await;
        assert_eq!(report.spawned, 1);
        let outcomes = wait_for_outcomes(&runner, 1).await;
        assert_eq!(outcomes[0].reason, IntegrationDispatchReason::Completed);
        assert!(store.release_calls().await.is_empty());
    }

    #[tokio::test]
    async fn backend_error_parks_request_for_retry() {
        let dispatcher = Arc::new(RecordingDispatcher::new(
            IntegrationDispatchReason::Completed,
        ));
        let store = Arc::new(InMemoryRunLeaseStore::new());
        store.register_run(RunRef::new(1)).await;
        store.fail_next("disk full").await;

        let (queue, runner) = build_lease_runner(4, dispatcher.clone(), store, "T0");
        queue.enqueue(req_with_run(1, "PROJ-1", 1));

        let report = runner.run_pending(CancellationToken::new()).await;
        assert_eq!(report.spawned, 0);
        assert_eq!(report.lease_backend_error, 1);
        assert_eq!(runner.deferred_count().await, 1);
        assert!(dispatcher.calls().is_empty());
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
