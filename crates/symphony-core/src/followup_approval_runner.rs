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

use std::collections::HashMap;
use std::sync::Arc;

use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use tokio::sync::{Mutex, Semaphore};
use tokio::task::JoinSet;
use tokio_util::sync::CancellationToken;
use tracing::{debug, warn};

use crate::blocker::RunRef;
use crate::followup::FollowupId;
use crate::followup_approval_tick::{
    FollowupApprovalDispatchQueue, FollowupApprovalDispatchRequest,
};
use crate::lease::{LeaseClock, LeaseConfig, LeaseOwner};
use crate::run_lease::{
    HeartbeatPulseObservation, LeaseAcquireOutcome, LeaseHeartbeat, RunLeaseGuard, RunLeaseStore,
    pulse_observed,
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

impl FollowupApprovalRunReport {
    /// `true` iff the pass was a complete no-op.
    pub fn is_idle(&self) -> bool {
        self.spawned == 0
            && self.deferred_capacity == 0
            && self.deferred_lease_contention == 0
            && self.missing_run == 0
            && self.lease_backend_error == 0
    }
}

/// Per-pulse heartbeat observation surfaced by
/// [`FollowupApprovalRunner::pulse_heartbeats`].
///
/// Pairs the typed [`HeartbeatPulseObservation`] from
/// [`crate::run_lease::pulse_observed`] with the run/follow-up identifier
/// the heartbeat targets so the scheduler can route the observation
/// (durable run-status update on contention, recovery dispatch on
/// not-found, transient-error log on backend error) without re-keying
/// against the in-flight set.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RunnerHeartbeatObservation {
    /// Durable run row the heartbeat was renewed against.
    pub run_id: RunRef,
    /// Follow-up id the dispatch covers.
    pub followup_id: FollowupId,
    /// Typed outcome of the renewal pulse.
    pub observation: HeartbeatPulseObservation,
}

/// Per-run heartbeat registration: the follow-up id (so observations
/// carry it back to the caller) plus the live heartbeat guarded by an
/// async mutex (`pulse` is `&mut self`).
struct HeartbeatRegistration {
    followup_id: FollowupId,
    heartbeat: Arc<Mutex<LeaseHeartbeat>>,
}

/// Bounded-concurrency consumer for [`FollowupApprovalDispatchQueue`].
pub struct FollowupApprovalRunner {
    queue: Arc<FollowupApprovalDispatchQueue>,
    dispatcher: Arc<dyn FollowupApprovalDispatcher>,
    semaphore: Arc<Semaphore>,
    deferred: Mutex<Vec<FollowupApprovalDispatchRequest>>,
    inflight: Mutex<JoinSet<FollowupApprovalDispatchOutcome>>,
    max_concurrent: usize,
    /// Optional durable-lease wiring. When `Some`, the runner acquires a
    /// lease on `request.run_id` before spawning the dispatcher and
    /// releases it on terminal completion. Requests whose `run_id` is
    /// `None` bypass lease acquisition.
    leasing: Option<LeaseWiring>,
    /// In-flight heartbeats, keyed by `run_id`. Populated when a lease
    /// is acquired alongside a spawned dispatch and removed by the
    /// spawned task at terminal release. [`Self::pulse_heartbeats`]
    /// snapshots the map and renews each due lease through the
    /// configured wiring's clock.
    heartbeats: Arc<Mutex<HashMap<RunRef, HeartbeatRegistration>>>,
}

/// Bundle holding the lease store, owner identity, TTL/renewal config,
/// and the clock used to derive `(now, expires_at)` per acquisition.
struct LeaseWiring {
    store: Arc<dyn RunLeaseStore>,
    owner: LeaseOwner,
    config: LeaseConfig,
    clock: Arc<dyn LeaseClock>,
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
            leasing: None,
            heartbeats: Arc::new(Mutex::new(HashMap::new())),
        }
    }

    /// Enable durable-lease wiring. Each subsequent dispatch whose
    /// request carries a `run_id` will acquire the lease on `run_id`,
    /// spawn the dispatcher only after acquisition succeeds, and release
    /// the lease at terminal completion regardless of the dispatcher's
    /// [`FollowupApprovalDispatchReason`]. Requests without a `run_id`
    /// are dispatched as before.
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

            // Durable-lease acquisition. Held through the spawned
            // dispatch and released on terminal completion. Permit is
            // only consumed once the lease is acquired so contention or
            // backend errors do not eat capacity.
            let leased = match self.try_acquire_lease(&req).await {
                LeaseAttempt::Acquired(leased) => leased,
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
            let followup_id = req.followup_id;
            let source = req.source_work_item;
            let req_for_dispatch = req.clone();
            let heartbeat_run_id = req.run_id;
            let heartbeats = self.heartbeats.clone();

            // Register the heartbeat so a parallel `pulse_heartbeats`
            // tick can renew the lease while dispatch is in flight.
            // Registration happens before spawn so a tight scheduler
            // tick sequence (spawn + immediate pulse) cannot miss it.
            if let (Some(run_id), Some(d)) = (heartbeat_run_id, leased.as_ref()) {
                heartbeats.lock().await.insert(
                    run_id,
                    HeartbeatRegistration {
                        followup_id,
                        heartbeat: d.heartbeat.clone(),
                    },
                );
            }

            let mut inflight = self.inflight.lock().await;
            inflight.spawn(async move {
                let reason = dispatcher.dispatch(req_for_dispatch, child_cancel).await;
                if let Some(LeasedDispatch { guard, .. }) = leased
                    && let Err(err) = guard.release().await
                {
                    warn!(
                        followup_id = followup_id.get(),
                        error = %err,
                        "followup approval runner: lease release failed; relying on TTL reaper",
                    );
                }
                // Deregister after release so a concurrent
                // `pulse_heartbeats` cannot re-acquire a lease the
                // terminal release just cleared.
                if let Some(run_id) = heartbeat_run_id {
                    heartbeats.lock().await.remove(&run_id);
                }
                drop(permit);
                FollowupApprovalDispatchOutcome {
                    followup_id,
                    reason,
                }
            });
            drop(inflight);

            debug!(
                followup_id = followup_id.get(),
                source_work_item = source.get(),
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

    async fn try_acquire_lease(&self, req: &FollowupApprovalDispatchRequest) -> LeaseAttempt {
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
            Ok(Ok(guard)) => {
                let heartbeat = LeaseHeartbeat::new(
                    wiring.store.clone(),
                    run_id,
                    wiring.owner.clone(),
                    ts.expires_at.clone(),
                    wiring.config.renewal_margin_ms,
                );
                LeaseAttempt::Acquired(Some(LeasedDispatch {
                    guard,
                    heartbeat: Arc::new(Mutex::new(heartbeat)),
                }))
            }
            Ok(Err(LeaseAcquireOutcome::Contended { holder, expires_at })) => {
                debug!(
                    followup_id = req.followup_id.get(),
                    holder = %holder,
                    expires_at = %expires_at,
                    "followup approval runner: lease contended; parking",
                );
                LeaseAttempt::Contended
            }
            Ok(Err(LeaseAcquireOutcome::NotFound)) => {
                warn!(
                    followup_id = req.followup_id.get(),
                    run_id = %run_id.get(),
                    "followup approval runner: durable run row missing; dropping request",
                );
                LeaseAttempt::NotFound
            }
            Ok(Err(LeaseAcquireOutcome::Acquired)) => {
                unreachable!("RunLeaseGuard::acquire returns Ok(Ok(_)) on Acquired")
            }
            Err(err) => {
                warn!(
                    followup_id = req.followup_id.get(),
                    error = %err,
                    "followup approval runner: lease store backend error; parking",
                );
                LeaseAttempt::BackendError
            }
        }
    }

    /// Number of heartbeats currently registered against in-flight
    /// dispatches. Diagnostic accessor for tests/scheduler logs.
    pub async fn heartbeat_count(&self) -> usize {
        self.heartbeats.lock().await.len()
    }

    /// Drive one renewal pass over every in-flight heartbeat.
    ///
    /// For each registered `(run_id, followup_id, heartbeat)` the runner
    /// checks [`LeaseHeartbeat::due`] against the configured wiring's
    /// clock and, when due, invokes [`pulse_observed`] to renew through
    /// the underlying [`RunLeaseStore::acquire`]. Heartbeats that are
    /// not yet due are skipped silently — the pass is idle for them.
    ///
    /// Each pulsed heartbeat produces a [`RunnerHeartbeatObservation`]
    /// in call order so the scheduler can route renewals, contention,
    /// missing-row, and backend-error outcomes without silently
    /// dropping the in-flight dispatch.
    ///
    /// Returns an empty vector when lease wiring is disabled.
    pub async fn pulse_heartbeats(&self) -> Vec<RunnerHeartbeatObservation> {
        let Some(wiring) = self.leasing.as_ref() else {
            return Vec::new();
        };
        let snapshot: Vec<(RunRef, FollowupId, Arc<Mutex<LeaseHeartbeat>>)> = {
            let map = self.heartbeats.lock().await;
            map.iter()
                .map(|(run_id, reg)| (*run_id, reg.followup_id, reg.heartbeat.clone()))
                .collect()
        };

        let clock = wiring.clock.as_ref();
        let ttl_ms = wiring.config.default_ttl_ms;
        let mut out = Vec::new();
        for (run_id, followup_id, hb_arc) in snapshot {
            let mut hb = hb_arc.lock().await;
            if !hb.due(clock) {
                continue;
            }
            let observation = pulse_observed(&mut hb, clock, ttl_ms).await;
            out.push(RunnerHeartbeatObservation {
                run_id,
                followup_id,
                observation,
            });
        }
        out
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

/// Outcome of [`FollowupApprovalRunner::try_acquire_lease`].
enum LeaseAttempt {
    /// Either lease acquired (`Some(LeasedDispatch)`) or lease wiring
    /// not configured (`None`). Both cases proceed to dispatch.
    Acquired(Option<LeasedDispatch>),
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

/// Bundle of artifacts produced by a successful lease acquisition: the
/// RAII guard the spawned task consumes at terminal release, and the
/// shared heartbeat the runner-driven pulse cadence renews.
struct LeasedDispatch {
    guard: RunLeaseGuard,
    heartbeat: Arc<Mutex<LeaseHeartbeat>>,
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
            run_id: None,
        }
    }

    fn req_with_run(
        id: i64,
        source: i64,
        title: &str,
        blocking: bool,
        run_id: i64,
    ) -> FollowupApprovalDispatchRequest {
        FollowupApprovalDispatchRequest {
            followup_id: FollowupId::new(id),
            source_work_item: WorkItemId::new(source),
            title: title.into(),
            blocking,
            approval_role: RoleName::new("platform_lead"),
            run_id: Some(crate::blocker::RunRef::new(run_id)),
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

    // --- Lease wiring (CHECKLIST_v2 Phase 11) -----------------------

    use crate::blocker::RunRef;
    use crate::lease::FixedLeaseClock;
    use crate::run_lease::InMemoryRunLeaseStore;

    fn lease_owner() -> LeaseOwner {
        LeaseOwner::new("test-host", "scheduler-test", 0).unwrap()
    }

    fn build_lease_runner(
        max_concurrent: usize,
        dispatcher: Arc<RecordingDispatcher>,
        store: Arc<InMemoryRunLeaseStore>,
        clock_now: &str,
    ) -> (Arc<FollowupApprovalDispatchQueue>, FollowupApprovalRunner) {
        let queue = Arc::new(FollowupApprovalDispatchQueue::new());
        let runner = FollowupApprovalRunner::new(queue.clone(), dispatcher, max_concurrent)
            .with_leasing(
                store,
                lease_owner(),
                LeaseConfig::default(),
                Arc::new(FixedLeaseClock::new(clock_now)),
            );
        (queue, runner)
    }

    #[tokio::test]
    async fn lease_acquired_during_dispatch_and_released_on_approved() {
        let dispatcher = Arc::new(RecordingDispatcher::new(
            FollowupApprovalDispatchReason::Approved,
        ));
        dispatcher.enable_gate();
        let store = Arc::new(InMemoryRunLeaseStore::new());
        store.register_run(RunRef::new(42)).await;

        let (queue, runner) = build_lease_runner(4, dispatcher.clone(), store.clone(), "T0");
        queue.enqueue(req_with_run(1, 100, "rate limit", false, 42));

        let report = runner.run_pending(CancellationToken::new()).await;
        assert_eq!(report.spawned, 1);
        assert_eq!(report.deferred_lease_contention, 0);
        assert_eq!(report.missing_run, 0);

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

        dispatcher.release_gate();
        let outcomes = wait_for_outcomes(&runner, 1).await;
        assert_eq!(outcomes[0].reason, FollowupApprovalDispatchReason::Approved);
        assert!(
            store.snapshot(RunRef::new(42)).await.is_none(),
            "lease must be cleared on Approved terminal release",
        );
        assert_eq!(store.release_calls().await, vec![RunRef::new(42)]);
    }

    #[tokio::test]
    async fn lease_cleared_on_rejected_terminal_release() {
        let dispatcher = Arc::new(RecordingDispatcher::new(
            FollowupApprovalDispatchReason::Approved,
        ));
        dispatcher.set_reason_for(FollowupId::new(2), FollowupApprovalDispatchReason::Rejected);
        let store = Arc::new(InMemoryRunLeaseStore::new());
        store.register_run(RunRef::new(7)).await;

        let (queue, runner) = build_lease_runner(4, dispatcher.clone(), store.clone(), "T1");
        queue.enqueue(req_with_run(2, 100, "out of scope", false, 7));

        let _ = runner.run_pending(CancellationToken::new()).await;
        let outcomes = wait_for_outcomes(&runner, 1).await;
        assert_eq!(outcomes[0].reason, FollowupApprovalDispatchReason::Rejected);
        assert!(store.snapshot(RunRef::new(7)).await.is_none());
        assert_eq!(store.release_calls().await, vec![RunRef::new(7)]);
    }

    #[tokio::test]
    async fn lease_cleared_on_deferred_terminal_release() {
        // Deferred is a terminal *dispatch* outcome (the proposal stays
        // in `Proposed` and the next tick re-surfaces it). The runner
        // must still release the lease when the dispatcher returns.
        let dispatcher = Arc::new(RecordingDispatcher::new(
            FollowupApprovalDispatchReason::Deferred,
        ));
        let store = Arc::new(InMemoryRunLeaseStore::new());
        store.register_run(RunRef::new(8)).await;

        let (queue, runner) = build_lease_runner(4, dispatcher.clone(), store.clone(), "T1");
        queue.enqueue(req_with_run(3, 100, "needs owner", false, 8));

        let _ = runner.run_pending(CancellationToken::new()).await;
        let outcomes = wait_for_outcomes(&runner, 1).await;
        assert_eq!(outcomes[0].reason, FollowupApprovalDispatchReason::Deferred);
        assert!(store.snapshot(RunRef::new(8)).await.is_none());
    }

    #[tokio::test]
    async fn lease_cleared_on_canceled_terminal_release() {
        let dispatcher = Arc::new(RecordingDispatcher::new(
            FollowupApprovalDispatchReason::Approved,
        ));
        dispatcher.enable_gate();
        let store = Arc::new(InMemoryRunLeaseStore::new());
        store.register_run(RunRef::new(11)).await;

        let (queue, runner) = build_lease_runner(4, dispatcher.clone(), store.clone(), "T2");
        queue.enqueue(req_with_run(6, 100, "cancel me", false, 11));

        let cancel = CancellationToken::new();
        let _ = runner.run_pending(cancel.clone()).await;
        assert!(store.snapshot(RunRef::new(11)).await.is_some());
        cancel.cancel();
        let outcomes = wait_for_outcomes(&runner, 1).await;
        assert_eq!(outcomes[0].reason, FollowupApprovalDispatchReason::Canceled);
        assert!(store.snapshot(RunRef::new(11)).await.is_none());
    }

    #[tokio::test]
    async fn lease_cleared_on_failed_terminal_release() {
        let dispatcher = Arc::new(RecordingDispatcher::new(
            FollowupApprovalDispatchReason::Failed,
        ));
        let store = Arc::new(InMemoryRunLeaseStore::new());
        store.register_run(RunRef::new(12)).await;

        let (queue, runner) = build_lease_runner(4, dispatcher.clone(), store.clone(), "T3");
        queue.enqueue(req_with_run(7, 100, "boom", false, 12));

        let _ = runner.run_pending(CancellationToken::new()).await;
        let outcomes = wait_for_outcomes(&runner, 1).await;
        assert_eq!(outcomes[0].reason, FollowupApprovalDispatchReason::Failed);
        assert!(store.snapshot(RunRef::new(12)).await.is_none());
    }

    #[tokio::test]
    async fn lease_visible_to_expired_scan_only_after_ttl() {
        let dispatcher = Arc::new(RecordingDispatcher::new(
            FollowupApprovalDispatchReason::Approved,
        ));
        dispatcher.enable_gate();
        let store = Arc::new(InMemoryRunLeaseStore::new());
        store.register_run(RunRef::new(99)).await;

        let (queue, runner) = build_lease_runner(4, dispatcher.clone(), store.clone(), "T4");
        queue.enqueue(req_with_run(8, 100, "ttl-check", false, 99));

        let _ = runner.run_pending(CancellationToken::new()).await;
        let snap = store.snapshot(RunRef::new(99)).await.unwrap();

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

        let after_ttl = format!("{}+999999999999ms", "T4");
        assert!(after_ttl > snap.1);
        let post = store
            .acquire(RunRef::new(99), &other_owner, "T9+later", &after_ttl)
            .await
            .unwrap();
        assert_eq!(post, crate::run_lease::LeaseAcquireOutcome::Acquired);

        dispatcher.release_gate();
        let _ = wait_for_outcomes(&runner, 1).await;
    }

    #[tokio::test]
    async fn contended_lease_parks_request_and_does_not_consume_capacity() {
        let dispatcher = Arc::new(RecordingDispatcher::new(
            FollowupApprovalDispatchReason::Approved,
        ));
        let store = Arc::new(InMemoryRunLeaseStore::new());
        store.register_run(RunRef::new(1)).await;
        store.register_run(RunRef::new(2)).await;

        let other = LeaseOwner::new("other-host", "scheduler-test", 9).unwrap();
        let _ = store
            .acquire(RunRef::new(1), &other, "T0+999ms", "T0")
            .await
            .unwrap();

        let (queue, runner) = build_lease_runner(2, dispatcher.clone(), store.clone(), "T0");
        queue.enqueue(req_with_run(1, 100, "blocked", false, 1));
        queue.enqueue(req_with_run(2, 100, "free", false, 2));

        let report = runner.run_pending(CancellationToken::new()).await;
        assert_eq!(report.spawned, 1);
        assert_eq!(report.deferred_lease_contention, 1);
        assert_eq!(runner.deferred_count().await, 1);
        let _ = wait_for_outcomes(&runner, 1).await;
    }

    #[tokio::test]
    async fn missing_run_drops_request_without_dispatch() {
        let dispatcher = Arc::new(RecordingDispatcher::new(
            FollowupApprovalDispatchReason::Approved,
        ));
        let store = Arc::new(InMemoryRunLeaseStore::new());

        let (queue, runner) = build_lease_runner(4, dispatcher.clone(), store, "T0");
        queue.enqueue(req_with_run(1, 100, "no-run", false, 404));

        let report = runner.run_pending(CancellationToken::new()).await;
        assert_eq!(report.spawned, 0);
        assert_eq!(report.missing_run, 1);
        assert!(dispatcher.calls().is_empty());
    }

    #[tokio::test]
    async fn request_without_run_id_dispatches_without_lease_acquisition() {
        let dispatcher = Arc::new(RecordingDispatcher::new(
            FollowupApprovalDispatchReason::Approved,
        ));
        let store = Arc::new(InMemoryRunLeaseStore::new());

        let (queue, runner) = build_lease_runner(4, dispatcher.clone(), store.clone(), "T0");
        queue.enqueue(req(1, 100, "no-lease", false));

        let report = runner.run_pending(CancellationToken::new()).await;
        assert_eq!(report.spawned, 1);
        let outcomes = wait_for_outcomes(&runner, 1).await;
        assert_eq!(outcomes[0].reason, FollowupApprovalDispatchReason::Approved);
        assert!(store.release_calls().await.is_empty());
    }

    #[tokio::test]
    async fn backend_error_parks_request_for_retry() {
        let dispatcher = Arc::new(RecordingDispatcher::new(
            FollowupApprovalDispatchReason::Approved,
        ));
        let store = Arc::new(InMemoryRunLeaseStore::new());
        store.register_run(RunRef::new(1)).await;
        store.fail_next("disk full").await;

        let (queue, runner) = build_lease_runner(4, dispatcher.clone(), store, "T0");
        queue.enqueue(req_with_run(1, 100, "io-blip", false, 1));

        let report = runner.run_pending(CancellationToken::new()).await;
        assert_eq!(report.spawned, 0);
        assert_eq!(report.lease_backend_error, 1);
        assert_eq!(runner.deferred_count().await, 1);
        assert!(dispatcher.calls().is_empty());
    }

    // ---- Heartbeat wiring (CHECKLIST_v2 Phase 11) ------------------

    /// Test-only [`LeaseClock`] whose `now` can be advanced between
    /// pulses. Mirrors the way a real wall-clock-backed clock evolves
    /// across scheduler ticks; deterministic for tests because the
    /// caller controls each advance.
    #[derive(Debug)]
    struct AdvanceableLeaseClock {
        now: StdMutex<String>,
    }

    impl AdvanceableLeaseClock {
        fn new(now: impl Into<String>) -> Self {
            Self {
                now: StdMutex::new(now.into()),
            }
        }
        fn set(&self, now: impl Into<String>) {
            *self.now.lock().unwrap() = now.into();
        }
    }

    impl LeaseClock for AdvanceableLeaseClock {
        fn timestamps(&self, ttl_ms: u64) -> crate::lease::LeaseTimestamps {
            let now = self.now.lock().unwrap().clone();
            // Same +<ttl>ms suffix as FixedLeaseClock so lex order is
            // monotonic with wall-clock time.
            let expires_at = format!("{}+{:012}ms", now, ttl_ms);
            crate::lease::LeaseTimestamps { now, expires_at }
        }
    }

    fn build_lease_runner_with_clock(
        max_concurrent: usize,
        dispatcher: Arc<RecordingDispatcher>,
        store: Arc<InMemoryRunLeaseStore>,
        clock: Arc<AdvanceableLeaseClock>,
    ) -> (Arc<FollowupApprovalDispatchQueue>, FollowupApprovalRunner) {
        let queue = Arc::new(FollowupApprovalDispatchQueue::new());
        let runner = FollowupApprovalRunner::new(queue.clone(), dispatcher, max_concurrent)
            .with_leasing(store, lease_owner(), LeaseConfig::default(), clock);
        (queue, runner)
    }

    #[tokio::test]
    async fn pulse_heartbeats_advances_lease_expiry_during_approved_dispatch() {
        let dispatcher = Arc::new(RecordingDispatcher::new(
            FollowupApprovalDispatchReason::Approved,
        ));
        dispatcher.enable_gate();
        let store = Arc::new(InMemoryRunLeaseStore::new());
        store.register_run(RunRef::new(42)).await;
        let clock = Arc::new(AdvanceableLeaseClock::new("T0"));

        let (queue, runner) =
            build_lease_runner_with_clock(4, dispatcher.clone(), store.clone(), clock.clone());
        queue.enqueue(req_with_run(1, 100, "rate limit", false, 42));

        let report = runner.run_pending(CancellationToken::new()).await;
        assert_eq!(report.spawned, 1);
        assert_eq!(runner.heartbeat_count().await, 1);

        let initial = store.snapshot(RunRef::new(42)).await.unwrap().1;

        // Not yet inside the renewal window — pulse should be idle.
        let observations = runner.pulse_heartbeats().await;
        assert!(
            observations.is_empty(),
            "heartbeat should be idle while full TTL remains; got {observations:?}",
        );
        let after_idle = store.snapshot(RunRef::new(42)).await.unwrap().1;
        assert_eq!(after_idle, initial);

        clock.set("T9");
        let observations = runner.pulse_heartbeats().await;
        assert_eq!(observations.len(), 1);
        let obs = &observations[0];
        assert_eq!(obs.run_id, RunRef::new(42));
        assert_eq!(obs.followup_id, FollowupId::new(1));
        assert_eq!(obs.observation, HeartbeatPulseObservation::Renewed);

        let after_renewal = store.snapshot(RunRef::new(42)).await.unwrap().1;
        assert!(
            after_renewal > initial,
            "lease_expires_at must advance after a successful pulse: {after_renewal} > {initial}",
        );

        dispatcher.release_gate();
        let outcomes = wait_for_outcomes(&runner, 1).await;
        assert_eq!(outcomes[0].reason, FollowupApprovalDispatchReason::Approved);
        assert!(store.snapshot(RunRef::new(42)).await.is_none());
        assert_eq!(store.release_calls().await, vec![RunRef::new(42)]);
        assert_eq!(runner.heartbeat_count().await, 0);
    }

    #[tokio::test]
    async fn pulse_heartbeats_advances_expiry_for_rejected_dispatch() {
        let dispatcher = Arc::new(RecordingDispatcher::new(
            FollowupApprovalDispatchReason::Approved,
        ));
        dispatcher.set_reason_for(FollowupId::new(2), FollowupApprovalDispatchReason::Rejected);
        dispatcher.enable_gate();
        let store = Arc::new(InMemoryRunLeaseStore::new());
        store.register_run(RunRef::new(7)).await;
        let clock = Arc::new(AdvanceableLeaseClock::new("T0"));

        let (queue, runner) =
            build_lease_runner_with_clock(4, dispatcher.clone(), store.clone(), clock.clone());
        queue.enqueue(req_with_run(2, 100, "out of scope", false, 7));

        let _ = runner.run_pending(CancellationToken::new()).await;
        assert_eq!(runner.heartbeat_count().await, 1);

        let initial = store.snapshot(RunRef::new(7)).await.unwrap().1;
        clock.set("T9");
        let obs = runner.pulse_heartbeats().await;
        assert_eq!(obs.len(), 1);
        assert_eq!(obs[0].observation, HeartbeatPulseObservation::Renewed);
        assert_eq!(obs[0].followup_id, FollowupId::new(2));
        let after = store.snapshot(RunRef::new(7)).await.unwrap().1;
        assert!(after > initial);

        dispatcher.release_gate();
        let outcomes = wait_for_outcomes(&runner, 1).await;
        assert_eq!(outcomes[0].reason, FollowupApprovalDispatchReason::Rejected);
        assert!(store.snapshot(RunRef::new(7)).await.is_none());
        assert_eq!(runner.heartbeat_count().await, 0);
    }

    #[tokio::test]
    async fn pulse_heartbeats_surfaces_contended_observation() {
        let dispatcher = Arc::new(RecordingDispatcher::new(
            FollowupApprovalDispatchReason::Approved,
        ));
        dispatcher.enable_gate();
        let store = Arc::new(InMemoryRunLeaseStore::new());
        store.register_run(RunRef::new(42)).await;
        let clock = Arc::new(AdvanceableLeaseClock::new("T0"));

        let (queue, runner) =
            build_lease_runner_with_clock(4, dispatcher.clone(), store.clone(), clock.clone());
        queue.enqueue(req_with_run(1, 100, "rate limit", false, 42));

        let _ = runner.run_pending(CancellationToken::new()).await;
        assert_eq!(runner.heartbeat_count().await, 1);

        let intruder = LeaseOwner::new("other-host", "scheduler-test", 9).unwrap();
        store.release(RunRef::new(42)).await.unwrap();
        let outcome = store
            .acquire(RunRef::new(42), &intruder, "Z9999+999999999999ms", "Z9999")
            .await
            .unwrap();
        assert_eq!(outcome, LeaseAcquireOutcome::Acquired);

        clock.set("T9");
        let observations = runner.pulse_heartbeats().await;
        assert_eq!(observations.len(), 1);
        match &observations[0].observation {
            HeartbeatPulseObservation::Contended { holder, .. } => {
                assert_eq!(holder, &intruder.to_string());
            }
            other => panic!("expected Contended, got {other:?}"),
        }
        assert_eq!(observations[0].run_id, RunRef::new(42));
        assert_eq!(observations[0].followup_id, FollowupId::new(1));

        dispatcher.release_gate();
        let _ = wait_for_outcomes(&runner, 1).await;
        assert_eq!(runner.heartbeat_count().await, 0);
    }

    #[tokio::test]
    async fn pulse_heartbeats_surfaces_not_found_observation() {
        let dispatcher = Arc::new(RecordingDispatcher::new(
            FollowupApprovalDispatchReason::Approved,
        ));
        dispatcher.enable_gate();
        let store = Arc::new(InMemoryRunLeaseStore::new());
        store.register_run(RunRef::new(42)).await;
        let clock = Arc::new(AdvanceableLeaseClock::new("T0"));

        let (queue, runner) =
            build_lease_runner_with_clock(4, dispatcher.clone(), store.clone(), clock.clone());
        queue.enqueue(req_with_run(1, 100, "rate limit", false, 42));
        let _ = runner.run_pending(CancellationToken::new()).await;

        store.forget_run(RunRef::new(42)).await;

        clock.set("T9");
        let observations = runner.pulse_heartbeats().await;
        assert_eq!(observations.len(), 1);
        assert_eq!(
            observations[0].observation,
            HeartbeatPulseObservation::NotFound,
        );
        assert_eq!(observations[0].run_id, RunRef::new(42));

        dispatcher.release_gate();
        let _ = wait_for_outcomes(&runner, 1).await;
    }

    #[tokio::test]
    async fn pulse_heartbeats_surfaces_backend_error_observation() {
        let dispatcher = Arc::new(RecordingDispatcher::new(
            FollowupApprovalDispatchReason::Approved,
        ));
        dispatcher.enable_gate();
        let store = Arc::new(InMemoryRunLeaseStore::new());
        store.register_run(RunRef::new(42)).await;
        let clock = Arc::new(AdvanceableLeaseClock::new("T0"));

        let (queue, runner) =
            build_lease_runner_with_clock(4, dispatcher.clone(), store.clone(), clock.clone());
        queue.enqueue(req_with_run(1, 100, "rate limit", false, 42));
        let _ = runner.run_pending(CancellationToken::new()).await;

        store.fail_next("disk full").await;
        clock.set("T9");
        let observations = runner.pulse_heartbeats().await;
        assert_eq!(observations.len(), 1);
        assert_eq!(
            observations[0].observation,
            HeartbeatPulseObservation::BackendError {
                message: "disk full".to_string(),
            },
        );

        dispatcher.release_gate();
        let _ = wait_for_outcomes(&runner, 1).await;
    }

    #[tokio::test]
    async fn pulse_heartbeats_is_noop_without_lease_wiring() {
        let queue = Arc::new(FollowupApprovalDispatchQueue::new());
        let dispatcher = Arc::new(RecordingDispatcher::new(
            FollowupApprovalDispatchReason::Approved,
        ));
        let runner = FollowupApprovalRunner::new(queue, dispatcher, 4);
        let observations = runner.pulse_heartbeats().await;
        assert!(observations.is_empty());
        assert_eq!(runner.heartbeat_count().await, 0);
    }

    #[tokio::test]
    async fn terminal_release_clears_lease_exactly_once_after_pulses() {
        let dispatcher = Arc::new(RecordingDispatcher::new(
            FollowupApprovalDispatchReason::Approved,
        ));
        dispatcher.enable_gate();
        let store = Arc::new(InMemoryRunLeaseStore::new());
        store.register_run(RunRef::new(42)).await;
        let clock = Arc::new(AdvanceableLeaseClock::new("T0"));

        let (queue, runner) =
            build_lease_runner_with_clock(4, dispatcher.clone(), store.clone(), clock.clone());
        queue.enqueue(req_with_run(1, 100, "rate limit", false, 42));
        let _ = runner.run_pending(CancellationToken::new()).await;

        clock.set("T1");
        let r1 = runner.pulse_heartbeats().await;
        assert_eq!(r1.len(), 1);
        assert_eq!(r1[0].observation, HeartbeatPulseObservation::Renewed);
        clock.set("T2");
        let r2 = runner.pulse_heartbeats().await;
        assert_eq!(r2.len(), 1);
        assert_eq!(r2[0].observation, HeartbeatPulseObservation::Renewed);

        dispatcher.release_gate();
        let outcomes = wait_for_outcomes(&runner, 1).await;
        assert_eq!(outcomes[0].reason, FollowupApprovalDispatchReason::Approved);

        assert_eq!(store.release_calls().await, vec![RunRef::new(42)]);
        assert!(store.snapshot(RunRef::new(42)).await.is_none());
        assert_eq!(runner.heartbeat_count().await, 0);
    }
}
