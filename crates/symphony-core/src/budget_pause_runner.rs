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
//!
//! When durable-lease wiring is configured via
//! [`BudgetPauseRunner::with_leasing`], each request whose `run_id` is
//! `Some` acquires a lease on the durable run row before dispatching
//! and releases it on terminal completion (Resumed, Waived, Deferred,
//! Canceled, or Failed). Contention parks the request for retry;
//! missing-run rows drop the request; backend errors park for retry.

use std::collections::HashMap;
use std::sync::Arc;

use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use tokio::sync::{Mutex, Semaphore};
use tokio::task::JoinSet;
use tokio_util::sync::CancellationToken;
use tracing::{debug, warn};

use crate::blocker::RunRef;
use crate::budget_pause_tick::{
    BudgetPauseDispatchQueue, BudgetPauseDispatchRequest, BudgetPauseId,
};
use crate::cancellation::{CancelRequest, CancellationQueue};
use crate::cancellation_observer::{CancellationDecision, observe_for_run};
use crate::concurrency_gate::{
    ConcurrencyGate, DispatchTriple, RunnerScopes, ScopeContended, ScopeKind, ScopePermitSet,
};
use crate::lease::{LeaseClock, LeaseConfig, LeaseOwner};
use crate::run_lease::{
    HeartbeatPulseObservation, LeaseAcquireOutcome, LeaseHeartbeat, RunLeaseGuard, RunLeaseStore,
    pulse_observed,
};
use crate::scope_contention_broadcaster::{ScopeContentionEventBroadcaster, ScopeFields};
use crate::specialist_runner::RunCancellationStatusSink;
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
    /// Requests parked because at least one
    /// [`crate::concurrency_gate::ConcurrencyGate`] scope was at cap.
    /// Length matches `scope_contentions.len()` — the typed observations
    /// are surfaced separately so callers can route them.
    pub deferred_scope_contention: usize,
    /// Typed in-flight observations for every dispatch that was deferred
    /// because a scope was at cap on this pass. Emitted in walk order so
    /// downstream observers can correlate against the parked dispatches.
    pub scope_contentions: Vec<ScopeContendedObservation>,
    /// Requests aborted by the pre-lease cancellation observation
    /// (SPEC v2 §4.5 / Phase 11.5). Length matches `cancellations.len()`.
    pub cancelled_before_lease: usize,
    /// Typed observations for every dispatch that was aborted by the
    /// pre-lease cancellation check this pass, in walk order.
    pub cancellations: Vec<BudgetPauseCancellationObserved>,
}

impl BudgetPauseRunReport {
    /// `true` iff the pass was a complete no-op.
    pub fn is_idle(&self) -> bool {
        self.spawned == 0
            && self.deferred_capacity == 0
            && self.deferred_lease_contention == 0
            && self.missing_run == 0
            && self.lease_backend_error == 0
            && self.deferred_scope_contention == 0
            && self.scope_contentions.is_empty()
            && self.cancelled_before_lease == 0
            && self.cancellations.is_empty()
    }
}

/// Per-request observation surfaced when the pre-lease cancellation
/// check (SPEC v2 §4.5 / Phase 11.5) aborted a budget-pause dispatch.
///
/// Budget pause has no parent context, so the runner observes only the
/// run keyspace via [`observe_for_run`] (per
/// [`crate::cancellation_observer`] policy). Requests without a
/// `run_id` therefore bypass the observation entirely.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BudgetPauseCancellationObserved {
    /// Pause id the parked request carries.
    pub pause_id: BudgetPauseId,
    /// Work item the pause blocks.
    pub work_item_id: WorkItemId,
    /// Durable run row that was reserved for the dispatch and now
    /// matched a pending cancel.
    pub run_id: RunRef,
    /// Reason / requester / timestamp clone snapshotted from the queue
    /// at observation time.
    pub request: CancelRequest,
}

/// Per-pass typed observation surfaced when a budget-pause dispatch
/// could not acquire its [`RunnerScopes`] permit set because at least
/// one scope was at cap.
///
/// Pairs the parked request's pause id with the [`ScopeContended`]
/// reported by [`ConcurrencyGate::try_acquire`] so a downstream observer
/// can answer "why is this resume not running" without re-deriving the
/// scope keys from workflow config.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ScopeContendedObservation {
    /// Pause id the parked request carries.
    pub pause_id: BudgetPauseId,
    /// The kind of scope that contended.
    pub scope_kind: ScopeKind,
    /// The scope key (role/profile/repo label, or `""` for `Global`).
    pub scope_key: String,
    /// In-flight permits on the contended scope at rejection.
    pub in_flight: u32,
    /// Configured cap on the contended scope.
    pub cap: u32,
}

impl ScopeContendedObservation {
    fn from_contention(pause_id: BudgetPauseId, contended: &ScopeContended) -> Self {
        Self {
            pause_id,
            scope_kind: contended.scope.kind(),
            scope_key: contended.scope.key().to_owned(),
            in_flight: contended.in_flight,
            cap: contended.cap,
        }
    }

    fn fields(&self) -> ScopeFields {
        ScopeFields {
            scope_kind: self.scope_kind,
            scope_key: self.scope_key.clone(),
            in_flight: self.in_flight,
            cap: self.cap,
        }
    }
}

/// Per-pulse heartbeat observation surfaced by
/// [`BudgetPauseRunner::pulse_heartbeats`].
///
/// Pairs the typed [`HeartbeatPulseObservation`] from
/// [`crate::run_lease::pulse_observed`] with the run/pause identifier the
/// heartbeat targets so the scheduler can route the observation
/// (durable run-status update on contention, recovery dispatch on
/// not-found, transient-error log on backend error) without re-keying
/// against the in-flight set.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RunnerHeartbeatObservation {
    /// Durable run row the heartbeat was renewed against.
    pub run_id: RunRef,
    /// Pause id the dispatch covers.
    pub pause_id: BudgetPauseId,
    /// Typed outcome of the renewal pulse.
    pub observation: HeartbeatPulseObservation,
}

/// Per-run heartbeat registration: the pause id (so observations carry
/// it back to the caller) plus the live heartbeat guarded by an async
/// mutex (`pulse` is `&mut self`).
struct HeartbeatRegistration {
    pause_id: BudgetPauseId,
    heartbeat: Arc<Mutex<LeaseHeartbeat>>,
}

/// Bounded-concurrency consumer for [`BudgetPauseDispatchQueue`].
pub struct BudgetPauseRunner {
    queue: Arc<BudgetPauseDispatchQueue>,
    dispatcher: Arc<dyn BudgetPauseDispatcher>,
    semaphore: Arc<Semaphore>,
    deferred: Mutex<Vec<BudgetPauseDispatchRequest>>,
    inflight: Mutex<JoinSet<BudgetPauseDispatchOutcome>>,
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
    /// Optional multi-scope concurrency gate. When `Some`, every
    /// dispatch carrying a `role` acquires
    /// `{Global, Role, AgentProfile?, Repository?}` permits before the
    /// resume handler runs and releases them on terminal completion.
    /// When `None` (or when a request has no `role`), only the local
    /// capacity semaphore caps concurrency. ARCHITECTURE_v2 ADR records
    /// that budget-pause resume is gated under the configured workflow
    /// resume role by convention — producers populate `role` from that
    /// config (typically the same role that owns budget waivers).
    concurrency_gate: Option<ConcurrencyGate>,
    /// Optional durable scope-contention broadcaster. When `Some`, every
    /// `run_pending` pass — including idle passes — feeds the runner's
    /// per-pass [`ScopeContendedObservation`]s into the shared dedup
    /// log so exactly one
    /// [`crate::events::OrchestratorEvent::ScopeCapReached`] event is
    /// broadcast on the bus and persisted via the configured sink per
    /// contention episode. Budget-pause dispatch keys on the durable
    /// `BudgetPauseId` rather than a `runs.id` row, so emitted events
    /// carry `identifier` (`pause_id` formatted as a decimal string)
    /// rather than `run_id`. When `None`, observations remain on the
    /// run report only.
    scope_contention_broadcaster: Option<Arc<ScopeContentionEventBroadcaster>>,
    /// Optional cooperative-cancellation wiring (SPEC v2 §4.5 / Phase
    /// 11.5). Budget pause has no parent context, so the runner only
    /// observes the run keyspace via [`observe_for_run`].
    cancellation: Option<CancellationWiring>,
}

/// Bundle holding the shared in-memory [`CancellationQueue`] and the
/// typed-status sink the runner flows an observed cancel through.
struct CancellationWiring {
    queue: Arc<Mutex<CancellationQueue>>,
    sink: Arc<dyn RunCancellationStatusSink>,
}

/// Bundle holding the lease store, owner identity, TTL/renewal config,
/// and the clock used to derive `(now, expires_at)` per acquisition.
struct LeaseWiring {
    store: Arc<dyn RunLeaseStore>,
    owner: LeaseOwner,
    config: LeaseConfig,
    clock: Arc<dyn LeaseClock>,
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
            leasing: None,
            heartbeats: Arc::new(Mutex::new(HashMap::new())),
            concurrency_gate: None,
            scope_contention_broadcaster: None,
            cancellation: None,
        }
    }

    /// Wire cooperative cancellation observation into the runner
    /// (SPEC v2 §4.5 / Phase 11.5).
    ///
    /// Budget pause has no parent context, so the runner observes only
    /// the run keyspace via [`observe_for_run`]. Requests without a
    /// `run_id` reservation skip observation entirely. On an `Abort`
    /// outcome the runner releases the just-acquired scope/capacity
    /// permits, marks the run `Cancelled` via the typed-status sink,
    /// and drains the run-keyed queue entry.
    pub fn with_cancellation(
        mut self,
        queue: Arc<Mutex<CancellationQueue>>,
        sink: Arc<dyn RunCancellationStatusSink>,
    ) -> Self {
        self.cancellation = Some(CancellationWiring { queue, sink });
        self
    }

    /// Wire a [`ScopeContentionEventBroadcaster`] into the runner.
    ///
    /// After every [`Self::run_pending`] pass — including idle passes
    /// with zero contentions — the runner forwards its per-request
    /// observations to the broadcaster. The broadcaster's per-pass dedup
    /// log enforces the "one
    /// [`crate::events::OrchestratorEvent::ScopeCapReached`] per
    /// contention episode" invariant on the bus and persistence sink.
    /// Idle passes with no current contentions are required to terminate
    /// stale episodes from a prior pass; the runner therefore always
    /// invokes the broadcaster when wiring is configured.
    ///
    /// Budget-pause dispatches do not reserve a `runs.id` row, so emitted
    /// events use [`crate::scope_contention_broadcaster::ContentionSubject::Identifier`]
    /// keyed on the decimal-formatted `pause_id` and carry `identifier`
    /// (not `run_id`).
    pub fn with_scope_contention_broadcaster(
        mut self,
        broadcaster: Arc<ScopeContentionEventBroadcaster>,
    ) -> Self {
        self.scope_contention_broadcaster = Some(broadcaster);
        self
    }

    /// Wire a [`ConcurrencyGate`] into the runner.
    ///
    /// Each subsequent dispatch whose request carries a `role` builds a
    /// [`DispatchTriple`] from `(role, agent_profile, repository)`, calls
    /// [`RunnerScopes::try_acquire`], and only proceeds when every
    /// configured scope has headroom. A contended scope produces a
    /// [`ScopeContendedObservation`] on the pass report and the request
    /// is parked on the deferred queue for a future pass — capacity and
    /// lease are *not* consumed when the gate rejects. Requests without
    /// a `role` bypass the gate entirely.
    pub fn with_concurrency_gate(mut self, gate: ConcurrencyGate) -> Self {
        self.concurrency_gate = Some(gate);
        self
    }

    /// Enable durable-lease wiring. Each subsequent dispatch whose
    /// request carries a `run_id` will acquire the lease on `run_id`,
    /// spawn the dispatcher only after acquisition succeeds, and release
    /// the lease at terminal completion regardless of the dispatcher's
    /// [`BudgetPauseDispatchReason`]. Requests without a `run_id` are
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
            // Even on an idle pass we must notify the broadcaster so any
            // episodes that were active in a prior pass get terminated
            // (the dedup log treats absence as "episode ended"). Without
            // this, a parked dispatch that drains terminally would leave
            // a stale episode in the log forever.
            self.broadcast_pass_observations(&[]);
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

            // Multi-scope concurrency gate. A contended scope must not
            // consume the local capacity permit — drop it before parking
            // so a different routable dispatch can take that slot in the
            // same pass. Lease acquisition runs after this so a parked
            // dispatch never holds a lease while waiting on a scope cap.
            let scope_permits = match self.try_acquire_scopes(&req) {
                Ok(set) => set,
                Err(contended) => {
                    drop(permit);
                    report
                        .scope_contentions
                        .push(ScopeContendedObservation::from_contention(
                            req.pause_id,
                            &contended,
                        ));
                    debug!(
                        pause_id = req.pause_id.get(),
                        scope_kind = ?contended.scope.kind(),
                        scope_key = %contended.scope.key(),
                        in_flight = contended.in_flight,
                        cap = contended.cap,
                        "budget pause runner: scope contended; parking",
                    );
                    new_deferred.push(req);
                    report.deferred_scope_contention += 1;
                    continue;
                }
            };

            // Cooperative cancellation observation (SPEC v2 §4.5).
            // Budget pause has no parent context — only the run keyspace
            // is consulted, and requests without a `run_id` reservation
            // skip observation entirely.
            if let (Some(wiring), Some(run_id)) = (self.cancellation.as_ref(), req.run_id) {
                let observed: Option<CancelRequest> = {
                    let q = wiring.queue.lock().await;
                    match observe_for_run(&q, run_id) {
                        CancellationDecision::Abort(req) => Some(req),
                        CancellationDecision::Proceed => None,
                    }
                };
                if let Some(cancel_req) = observed {
                    drop(scope_permits);
                    drop(permit);
                    if let Err(err) = wiring.sink.mark_cancelled(run_id, &cancel_req) {
                        warn!(
                            pause_id = req.pause_id.get(),
                            run_id = %run_id,
                            error = %err,
                            "budget pause runner: cancellation status sink failed; relying on operator re-issue",
                        );
                    }
                    {
                        let mut q = wiring.queue.lock().await;
                        q.drain_for_run(run_id);
                    }
                    debug!(
                        pause_id = req.pause_id.get(),
                        reason = %cancel_req.reason,
                        "budget pause runner: pre-lease cancel observed; aborted dispatch",
                    );
                    report.cancelled_before_lease += 1;
                    report.cancellations.push(BudgetPauseCancellationObserved {
                        pause_id: req.pause_id,
                        work_item_id: req.work_item_id,
                        run_id,
                        request: cancel_req,
                    });
                    continue;
                }
            }

            // Durable-lease acquisition. Held through the spawned
            // dispatch and released on terminal completion. Permit is
            // only consumed once the lease is acquired so contention or
            // backend errors do not eat capacity.
            let leased = match self.try_acquire_lease(&req).await {
                LeaseAttempt::Acquired(leased) => leased,
                LeaseAttempt::Skipped => None,
                LeaseAttempt::Contended => {
                    drop(scope_permits);
                    drop(permit);
                    new_deferred.push(req);
                    report.deferred_lease_contention += 1;
                    continue;
                }
                LeaseAttempt::NotFound => {
                    drop(scope_permits);
                    drop(permit);
                    report.missing_run += 1;
                    continue;
                }
                LeaseAttempt::BackendError => {
                    drop(scope_permits);
                    drop(permit);
                    new_deferred.push(req);
                    report.lease_backend_error += 1;
                    continue;
                }
            };

            let dispatcher = self.dispatcher.clone();
            let child_cancel = cancel.child_token();
            let pause_id = req.pause_id;
            let work_item_id = req.work_item_id;
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
                        pause_id,
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
                        pause_id = pause_id.get(),
                        error = %err,
                        "budget pause runner: lease release failed; relying on TTL reaper",
                    );
                }
                // Deregister after release so a concurrent
                // `pulse_heartbeats` cannot re-acquire a lease the
                // terminal release just cleared.
                if let Some(run_id) = heartbeat_run_id {
                    heartbeats.lock().await.remove(&run_id);
                }
                // Release scope permits exactly once on terminal exit;
                // ScopePermitSet::Drop is idempotent across the move.
                drop(scope_permits);
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

        // Feed this pass's contentions into the durable broadcaster
        // (when wired). Always called — a pass with zero contentions
        // ends any episodes that were active last pass.
        self.broadcast_pass_observations(&report.scope_contentions);

        report
    }

    fn broadcast_pass_observations(&self, observations: &[ScopeContendedObservation]) {
        let Some(bx) = self.scope_contention_broadcaster.as_ref() else {
            return;
        };
        bx.observe_identifier_pass(
            observations
                .iter()
                .map(|obs| (obs.pause_id.get().to_string(), obs.fields())),
        );
    }

    /// Attempt to acquire scope permits for `req` against the configured
    /// [`ConcurrencyGate`]. Returns `Ok(None)` when no gate is wired or
    /// the request lacks a `role`, `Ok(Some(set))` when every scope had
    /// headroom, and `Err(_)` when any scope was at cap (no permits held
    /// — the inner gate already rolled back partial acquisitions).
    fn try_acquire_scopes(
        &self,
        req: &BudgetPauseDispatchRequest,
    ) -> Result<Option<ScopePermitSet>, ScopeContended> {
        let Some(gate) = &self.concurrency_gate else {
            return Ok(None);
        };
        let Some(role) = req.role.as_ref() else {
            return Ok(None);
        };
        let triple = DispatchTriple::new(
            role.as_str().to_owned(),
            req.agent_profile.clone(),
            req.repository.clone(),
        );
        RunnerScopes::new(gate.clone(), triple)
            .try_acquire()
            .map(Some)
    }

    async fn try_acquire_lease(&self, req: &BudgetPauseDispatchRequest) -> LeaseAttempt {
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
                    pause_id = req.pause_id.get(),
                    holder = %holder,
                    expires_at = %expires_at,
                    "budget pause runner: lease contended; parking",
                );
                LeaseAttempt::Contended
            }
            Ok(Err(LeaseAcquireOutcome::NotFound)) => {
                warn!(
                    pause_id = req.pause_id.get(),
                    run_id = %run_id.get(),
                    "budget pause runner: durable run row missing; dropping request",
                );
                LeaseAttempt::NotFound
            }
            Ok(Err(LeaseAcquireOutcome::Acquired)) => {
                unreachable!("RunLeaseGuard::acquire returns Ok(Ok(_)) on Acquired")
            }
            Err(err) => {
                warn!(
                    pause_id = req.pause_id.get(),
                    error = %err,
                    "budget pause runner: lease store backend error; parking",
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
    /// For each registered `(run_id, pause_id, heartbeat)` the runner
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
        let snapshot: Vec<(RunRef, BudgetPauseId, Arc<Mutex<LeaseHeartbeat>>)> = {
            let map = self.heartbeats.lock().await;
            map.iter()
                .map(|(run_id, reg)| (*run_id, reg.pause_id, reg.heartbeat.clone()))
                .collect()
        };

        let clock = wiring.clock.as_ref();
        let ttl_ms = wiring.config.default_ttl_ms;
        let mut out = Vec::new();
        for (run_id, pause_id, hb_arc) in snapshot {
            let mut hb = hb_arc.lock().await;
            if !hb.due(clock) {
                continue;
            }
            let observation = pulse_observed(&mut hb, clock, ttl_ms).await;
            out.push(RunnerHeartbeatObservation {
                run_id,
                pause_id,
                observation,
            });
        }
        out
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

/// Outcome of [`BudgetPauseRunner::try_acquire_lease`].
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
    use std::sync::Mutex as StdMutex;
    use std::time::Duration;

    fn req(id: i64, work: i64, kind: &str) -> BudgetPauseDispatchRequest {
        BudgetPauseDispatchRequest {
            pause_id: BudgetPauseId::new(id),
            work_item_id: WorkItemId::new(work),
            budget_kind: kind.into(),
            limit_value: 5.0,
            observed: 6.0,
            run_id: None,
            role: None,
            agent_profile: None,
            repository: None,
        }
    }

    fn req_with_run(id: i64, work: i64, kind: &str, run_id: i64) -> BudgetPauseDispatchRequest {
        BudgetPauseDispatchRequest {
            pause_id: BudgetPauseId::new(id),
            work_item_id: WorkItemId::new(work),
            budget_kind: kind.into(),
            limit_value: 5.0,
            observed: 6.0,
            run_id: Some(crate::blocker::RunRef::new(run_id)),
            role: None,
            agent_profile: None,
            repository: None,
        }
    }

    fn req_full(
        id: i64,
        work: i64,
        kind: &str,
        role: &str,
        agent_profile: Option<&str>,
        repository: Option<&str>,
    ) -> BudgetPauseDispatchRequest {
        BudgetPauseDispatchRequest {
            pause_id: BudgetPauseId::new(id),
            work_item_id: WorkItemId::new(work),
            budget_kind: kind.into(),
            limit_value: 5.0,
            observed: 6.0,
            run_id: None,
            role: Some(crate::role::RoleName::new(role)),
            agent_profile: agent_profile.map(str::to_owned),
            repository: repository.map(str::to_owned),
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
    ) -> (Arc<BudgetPauseDispatchQueue>, BudgetPauseRunner) {
        let queue = Arc::new(BudgetPauseDispatchQueue::new());
        let runner = BudgetPauseRunner::new(queue.clone(), dispatcher, max_concurrent)
            .with_leasing(
                store,
                lease_owner(),
                LeaseConfig::default(),
                Arc::new(FixedLeaseClock::new(clock_now)),
            );
        (queue, runner)
    }

    #[tokio::test]
    async fn lease_acquired_during_dispatch_and_released_on_resumed() {
        let dispatcher = Arc::new(RecordingDispatcher::new(BudgetPauseDispatchReason::Resumed));
        dispatcher.enable_gate();
        let store = Arc::new(InMemoryRunLeaseStore::new());
        store.register_run(RunRef::new(42)).await;

        let (queue, runner) = build_lease_runner(4, dispatcher.clone(), store.clone(), "T0");
        queue.enqueue(req_with_run(1, 100, "max_cost_per_issue_usd", 42));

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
        assert_eq!(outcomes[0].reason, BudgetPauseDispatchReason::Resumed);
        assert!(
            store.snapshot(RunRef::new(42)).await.is_none(),
            "lease must be cleared on Resumed terminal release",
        );
        assert_eq!(store.release_calls().await, vec![RunRef::new(42)]);
    }

    #[tokio::test]
    async fn lease_cleared_on_waived_terminal_release() {
        let dispatcher = Arc::new(RecordingDispatcher::new(BudgetPauseDispatchReason::Resumed));
        dispatcher.set_reason_for(BudgetPauseId::new(2), BudgetPauseDispatchReason::Waived);
        let store = Arc::new(InMemoryRunLeaseStore::new());
        store.register_run(RunRef::new(7)).await;

        let (queue, runner) = build_lease_runner(4, dispatcher.clone(), store.clone(), "T1");
        queue.enqueue(req_with_run(2, 100, "max_retries", 7));

        let _ = runner.run_pending(CancellationToken::new()).await;
        let outcomes = wait_for_outcomes(&runner, 1).await;
        assert_eq!(outcomes[0].reason, BudgetPauseDispatchReason::Waived);
        assert!(store.snapshot(RunRef::new(7)).await.is_none());
        assert_eq!(store.release_calls().await, vec![RunRef::new(7)]);
    }

    #[tokio::test]
    async fn lease_cleared_on_deferred_terminal_release() {
        // Deferred is a terminal *dispatch* outcome (the pause stays
        // Active and the next BudgetPauseQueueTick re-surfaces it). The
        // runner must still release the lease when the dispatcher
        // returns — i.e. the "hold" path in resume/hold semantics.
        let dispatcher = Arc::new(RecordingDispatcher::new(
            BudgetPauseDispatchReason::Deferred,
        ));
        let store = Arc::new(InMemoryRunLeaseStore::new());
        store.register_run(RunRef::new(8)).await;

        let (queue, runner) = build_lease_runner(4, dispatcher.clone(), store.clone(), "T1");
        queue.enqueue(req_with_run(3, 100, "max_retries", 8));

        let _ = runner.run_pending(CancellationToken::new()).await;
        let outcomes = wait_for_outcomes(&runner, 1).await;
        assert_eq!(outcomes[0].reason, BudgetPauseDispatchReason::Deferred);
        assert!(store.snapshot(RunRef::new(8)).await.is_none());
    }

    #[tokio::test]
    async fn lease_cleared_on_canceled_terminal_release() {
        let dispatcher = Arc::new(RecordingDispatcher::new(BudgetPauseDispatchReason::Resumed));
        dispatcher.enable_gate();
        let store = Arc::new(InMemoryRunLeaseStore::new());
        store.register_run(RunRef::new(11)).await;

        let (queue, runner) = build_lease_runner(4, dispatcher.clone(), store.clone(), "T2");
        queue.enqueue(req_with_run(6, 100, "k", 11));

        let cancel = CancellationToken::new();
        let _ = runner.run_pending(cancel.clone()).await;
        assert!(store.snapshot(RunRef::new(11)).await.is_some());
        cancel.cancel();
        let outcomes = wait_for_outcomes(&runner, 1).await;
        assert_eq!(outcomes[0].reason, BudgetPauseDispatchReason::Canceled);
        assert!(store.snapshot(RunRef::new(11)).await.is_none());
    }

    #[tokio::test]
    async fn lease_cleared_on_failed_terminal_release() {
        let dispatcher = Arc::new(RecordingDispatcher::new(BudgetPauseDispatchReason::Failed));
        let store = Arc::new(InMemoryRunLeaseStore::new());
        store.register_run(RunRef::new(12)).await;

        let (queue, runner) = build_lease_runner(4, dispatcher.clone(), store.clone(), "T3");
        queue.enqueue(req_with_run(7, 100, "boom", 12));

        let _ = runner.run_pending(CancellationToken::new()).await;
        let outcomes = wait_for_outcomes(&runner, 1).await;
        assert_eq!(outcomes[0].reason, BudgetPauseDispatchReason::Failed);
        assert!(store.snapshot(RunRef::new(12)).await.is_none());
    }

    #[tokio::test]
    async fn lease_visible_to_expired_scan_only_after_ttl() {
        let dispatcher = Arc::new(RecordingDispatcher::new(BudgetPauseDispatchReason::Resumed));
        dispatcher.enable_gate();
        let store = Arc::new(InMemoryRunLeaseStore::new());
        store.register_run(RunRef::new(99)).await;

        let (queue, runner) = build_lease_runner(4, dispatcher.clone(), store.clone(), "T4");
        queue.enqueue(req_with_run(8, 100, "ttl-check", 99));

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
        let dispatcher = Arc::new(RecordingDispatcher::new(BudgetPauseDispatchReason::Resumed));
        let store = Arc::new(InMemoryRunLeaseStore::new());
        store.register_run(RunRef::new(1)).await;
        store.register_run(RunRef::new(2)).await;

        let other = LeaseOwner::new("other-host", "scheduler-test", 9).unwrap();
        let _ = store
            .acquire(RunRef::new(1), &other, "T0+999ms", "T0")
            .await
            .unwrap();

        let (queue, runner) = build_lease_runner(2, dispatcher.clone(), store.clone(), "T0");
        queue.enqueue(req_with_run(1, 100, "blocked", 1));
        queue.enqueue(req_with_run(2, 100, "free", 2));

        let report = runner.run_pending(CancellationToken::new()).await;
        assert_eq!(report.spawned, 1);
        assert_eq!(report.deferred_lease_contention, 1);
        assert_eq!(runner.deferred_count().await, 1);
        let _ = wait_for_outcomes(&runner, 1).await;
    }

    #[tokio::test]
    async fn missing_run_drops_request_without_dispatch() {
        let dispatcher = Arc::new(RecordingDispatcher::new(BudgetPauseDispatchReason::Resumed));
        let store = Arc::new(InMemoryRunLeaseStore::new());

        let (queue, runner) = build_lease_runner(4, dispatcher.clone(), store, "T0");
        queue.enqueue(req_with_run(1, 100, "no-run", 404));

        let report = runner.run_pending(CancellationToken::new()).await;
        assert_eq!(report.spawned, 0);
        assert_eq!(report.missing_run, 1);
        assert!(dispatcher.calls().is_empty());
    }

    #[tokio::test]
    async fn request_without_run_id_dispatches_without_lease_acquisition() {
        let dispatcher = Arc::new(RecordingDispatcher::new(BudgetPauseDispatchReason::Resumed));
        let store = Arc::new(InMemoryRunLeaseStore::new());

        let (queue, runner) = build_lease_runner(4, dispatcher.clone(), store.clone(), "T0");
        queue.enqueue(req(1, 100, "no-lease"));

        let report = runner.run_pending(CancellationToken::new()).await;
        assert_eq!(report.spawned, 1);
        let outcomes = wait_for_outcomes(&runner, 1).await;
        assert_eq!(outcomes[0].reason, BudgetPauseDispatchReason::Resumed);
        assert!(store.release_calls().await.is_empty());
    }

    #[tokio::test]
    async fn backend_error_parks_request_for_retry() {
        let dispatcher = Arc::new(RecordingDispatcher::new(BudgetPauseDispatchReason::Resumed));
        let store = Arc::new(InMemoryRunLeaseStore::new());
        store.register_run(RunRef::new(1)).await;
        store.fail_next("disk full").await;

        let (queue, runner) = build_lease_runner(4, dispatcher.clone(), store, "T0");
        queue.enqueue(req_with_run(1, 100, "io-blip", 1));

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
    ) -> (Arc<BudgetPauseDispatchQueue>, BudgetPauseRunner) {
        let queue = Arc::new(BudgetPauseDispatchQueue::new());
        let runner = BudgetPauseRunner::new(queue.clone(), dispatcher, max_concurrent)
            .with_leasing(store, lease_owner(), LeaseConfig::default(), clock);
        (queue, runner)
    }

    #[tokio::test]
    async fn pulse_heartbeats_advances_lease_expiry_during_resumed_dispatch() {
        let dispatcher = Arc::new(RecordingDispatcher::new(BudgetPauseDispatchReason::Resumed));
        dispatcher.enable_gate();
        let store = Arc::new(InMemoryRunLeaseStore::new());
        store.register_run(RunRef::new(42)).await;
        let clock = Arc::new(AdvanceableLeaseClock::new("T0"));

        let (queue, runner) =
            build_lease_runner_with_clock(4, dispatcher.clone(), store.clone(), clock.clone());
        queue.enqueue(req_with_run(1, 100, "max_cost_per_issue_usd", 42));

        let report = runner.run_pending(CancellationToken::new()).await;
        assert_eq!(report.spawned, 1);
        assert_eq!(runner.heartbeat_count().await, 1);

        let initial = store.snapshot(RunRef::new(42)).await.unwrap().1;

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
        assert_eq!(obs.pause_id, BudgetPauseId::new(1));
        assert_eq!(obs.observation, HeartbeatPulseObservation::Renewed);

        let after_renewal = store.snapshot(RunRef::new(42)).await.unwrap().1;
        assert!(
            after_renewal > initial,
            "lease_expires_at must advance after a successful pulse: {after_renewal} > {initial}",
        );

        dispatcher.release_gate();
        let outcomes = wait_for_outcomes(&runner, 1).await;
        assert_eq!(outcomes[0].reason, BudgetPauseDispatchReason::Resumed);
        assert!(store.snapshot(RunRef::new(42)).await.is_none());
        assert_eq!(store.release_calls().await, vec![RunRef::new(42)]);
        assert_eq!(runner.heartbeat_count().await, 0);
    }

    #[tokio::test]
    async fn pulse_heartbeats_advances_expiry_for_held_dispatch() {
        // Verifies the "hold" leg: dispatcher returns Deferred (the pause
        // stays Active and is re-surfaced) — heartbeat must still pulse
        // mid-flight.
        let dispatcher = Arc::new(RecordingDispatcher::new(BudgetPauseDispatchReason::Resumed));
        dispatcher.set_reason_for(BudgetPauseId::new(2), BudgetPauseDispatchReason::Deferred);
        dispatcher.enable_gate();
        let store = Arc::new(InMemoryRunLeaseStore::new());
        store.register_run(RunRef::new(7)).await;
        let clock = Arc::new(AdvanceableLeaseClock::new("T0"));

        let (queue, runner) =
            build_lease_runner_with_clock(4, dispatcher.clone(), store.clone(), clock.clone());
        queue.enqueue(req_with_run(2, 100, "max_retries", 7));

        let _ = runner.run_pending(CancellationToken::new()).await;
        assert_eq!(runner.heartbeat_count().await, 1);

        let initial = store.snapshot(RunRef::new(7)).await.unwrap().1;
        clock.set("T9");
        let obs = runner.pulse_heartbeats().await;
        assert_eq!(obs.len(), 1);
        assert_eq!(obs[0].observation, HeartbeatPulseObservation::Renewed);
        assert_eq!(obs[0].pause_id, BudgetPauseId::new(2));
        let after = store.snapshot(RunRef::new(7)).await.unwrap().1;
        assert!(after > initial);

        dispatcher.release_gate();
        let outcomes = wait_for_outcomes(&runner, 1).await;
        assert_eq!(outcomes[0].reason, BudgetPauseDispatchReason::Deferred);
        assert!(store.snapshot(RunRef::new(7)).await.is_none());
        assert_eq!(runner.heartbeat_count().await, 0);
    }

    #[tokio::test]
    async fn pulse_heartbeats_surfaces_contended_observation() {
        let dispatcher = Arc::new(RecordingDispatcher::new(BudgetPauseDispatchReason::Resumed));
        dispatcher.enable_gate();
        let store = Arc::new(InMemoryRunLeaseStore::new());
        store.register_run(RunRef::new(42)).await;
        let clock = Arc::new(AdvanceableLeaseClock::new("T0"));

        let (queue, runner) =
            build_lease_runner_with_clock(4, dispatcher.clone(), store.clone(), clock.clone());
        queue.enqueue(req_with_run(1, 100, "max_cost", 42));

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
        assert_eq!(observations[0].pause_id, BudgetPauseId::new(1));

        dispatcher.release_gate();
        let _ = wait_for_outcomes(&runner, 1).await;
        assert_eq!(runner.heartbeat_count().await, 0);
    }

    #[tokio::test]
    async fn pulse_heartbeats_surfaces_not_found_observation() {
        let dispatcher = Arc::new(RecordingDispatcher::new(BudgetPauseDispatchReason::Resumed));
        dispatcher.enable_gate();
        let store = Arc::new(InMemoryRunLeaseStore::new());
        store.register_run(RunRef::new(42)).await;
        let clock = Arc::new(AdvanceableLeaseClock::new("T0"));

        let (queue, runner) =
            build_lease_runner_with_clock(4, dispatcher.clone(), store.clone(), clock.clone());
        queue.enqueue(req_with_run(1, 100, "max_cost", 42));
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
        let dispatcher = Arc::new(RecordingDispatcher::new(BudgetPauseDispatchReason::Resumed));
        dispatcher.enable_gate();
        let store = Arc::new(InMemoryRunLeaseStore::new());
        store.register_run(RunRef::new(42)).await;
        let clock = Arc::new(AdvanceableLeaseClock::new("T0"));

        let (queue, runner) =
            build_lease_runner_with_clock(4, dispatcher.clone(), store.clone(), clock.clone());
        queue.enqueue(req_with_run(1, 100, "max_cost", 42));
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
        let queue = Arc::new(BudgetPauseDispatchQueue::new());
        let dispatcher = Arc::new(RecordingDispatcher::new(BudgetPauseDispatchReason::Resumed));
        let runner = BudgetPauseRunner::new(queue, dispatcher, 4);
        let observations = runner.pulse_heartbeats().await;
        assert!(observations.is_empty());
        assert_eq!(runner.heartbeat_count().await, 0);
    }

    #[tokio::test]
    async fn terminal_release_clears_lease_exactly_once_after_pulses() {
        let dispatcher = Arc::new(RecordingDispatcher::new(BudgetPauseDispatchReason::Resumed));
        dispatcher.enable_gate();
        let store = Arc::new(InMemoryRunLeaseStore::new());
        store.register_run(RunRef::new(42)).await;
        let clock = Arc::new(AdvanceableLeaseClock::new("T0"));

        let (queue, runner) =
            build_lease_runner_with_clock(4, dispatcher.clone(), store.clone(), clock.clone());
        queue.enqueue(req_with_run(1, 100, "max_cost", 42));
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
        assert_eq!(outcomes[0].reason, BudgetPauseDispatchReason::Resumed);

        assert_eq!(store.release_calls().await, vec![RunRef::new(42)]);
        assert!(store.snapshot(RunRef::new(42)).await.is_none());
        assert_eq!(runner.heartbeat_count().await, 0);
    }

    // ---- Concurrency-gate wiring (CHECKLIST_v2 Phase 11) -----------

    use crate::concurrency_gate::Scope;

    #[tokio::test]
    async fn resume_role_cap_serializes_dispatch_independent_of_other_caps() {
        // Resume role cap=1 must serialize budget-pause dispatch even
        // when other role caps allow more.
        let queue = Arc::new(BudgetPauseDispatchQueue::new());
        let dispatcher = Arc::new(RecordingDispatcher::new(BudgetPauseDispatchReason::Resumed));
        dispatcher.enable_gate();

        let gate = ConcurrencyGate::new();
        gate.set_cap(Scope::Role("platform_lead".into()), 1);
        gate.set_cap(Scope::Role("qa".into()), 99);

        let runner = BudgetPauseRunner::new(queue.clone(), dispatcher.clone(), 4)
            .with_concurrency_gate(gate);

        queue.enqueue(req_full(1, 100, "max_cost", "platform_lead", None, None));
        queue.enqueue(req_full(2, 100, "max_cost", "platform_lead", None, None));
        queue.enqueue(req_full(3, 100, "max_cost", "platform_lead", None, None));

        let report = runner.run_pending(CancellationToken::new()).await;
        assert_eq!(report.spawned, 1, "report = {report:?}");
        assert_eq!(report.deferred_scope_contention, 2);
        assert_eq!(report.scope_contentions.len(), 2);
        for obs in &report.scope_contentions {
            assert_eq!(obs.scope_kind, ScopeKind::Role);
            assert_eq!(obs.scope_key, "platform_lead");
            assert_eq!(obs.cap, 1);
            assert_eq!(obs.in_flight, 1);
        }
        let parked: Vec<i64> = report
            .scope_contentions
            .iter()
            .map(|o| o.pause_id.get())
            .collect();
        assert_eq!(parked, vec![2, 3]);

        // Local capacity unaffected — parked dispatches did not consume
        // permits.
        assert_eq!(runner.inflight_count().await, 1);
        assert_eq!(runner.deferred_count().await, 2);

        dispatcher.release_gate();
        let _ = wait_for_outcomes(&runner, 1).await;
        dispatcher.enable_gate();

        // Next pass: one parked dispatch acquires the freed permit and
        // spawns; the second re-defers because cap=1.
        let report = runner.run_pending(CancellationToken::new()).await;
        assert_eq!(report.spawned, 1, "report = {report:?}");
        assert_eq!(report.deferred_scope_contention, 1);

        dispatcher.release_gate();
        let _ = wait_for_outcomes(&runner, 1).await;
    }

    #[tokio::test]
    async fn repository_cap_parks_without_consuming_capacity() {
        // A per-repository cap defers the second dispatch but the local
        // capacity permit must NOT be consumed: a third (different
        // repo) dispatch on the same pass still spawns.
        let queue = Arc::new(BudgetPauseDispatchQueue::new());
        let dispatcher = Arc::new(RecordingDispatcher::new(BudgetPauseDispatchReason::Resumed));
        dispatcher.enable_gate();

        let gate = ConcurrencyGate::new();
        gate.set_cap(Scope::Repository("acme/widgets".into()), 1);

        let runner = BudgetPauseRunner::new(queue.clone(), dispatcher.clone(), 2)
            .with_concurrency_gate(gate);

        queue.enqueue(req_full(
            1,
            100,
            "k",
            "platform_lead",
            None,
            Some("acme/widgets"),
        ));
        queue.enqueue(req_full(
            2,
            100,
            "k",
            "platform_lead",
            None,
            Some("acme/widgets"),
        ));
        queue.enqueue(req_full(
            3,
            100,
            "k",
            "platform_lead",
            None,
            Some("other/repo"),
        ));

        let report = runner.run_pending(CancellationToken::new()).await;
        assert_eq!(report.spawned, 2, "report = {report:?}");
        assert_eq!(report.deferred_scope_contention, 1);
        assert_eq!(report.scope_contentions.len(), 1);
        let obs = &report.scope_contentions[0];
        assert_eq!(obs.pause_id, BudgetPauseId::new(2));
        assert_eq!(obs.scope_kind, ScopeKind::Repository);
        assert_eq!(obs.scope_key, "acme/widgets");

        dispatcher.release_gate();
        let _ = wait_for_outcomes(&runner, 2).await;
    }

    #[tokio::test]
    async fn no_gate_passthrough_dispatches_normally() {
        // No gate wired — every request dispatches up to local capacity.
        let queue = Arc::new(BudgetPauseDispatchQueue::new());
        let dispatcher = Arc::new(RecordingDispatcher::new(BudgetPauseDispatchReason::Resumed));

        let runner = BudgetPauseRunner::new(queue.clone(), dispatcher.clone(), 4);

        queue.enqueue(req_full(1, 100, "k", "platform_lead", None, None));
        queue.enqueue(req_full(2, 100, "k", "platform_lead", None, None));

        let report = runner.run_pending(CancellationToken::new()).await;
        assert_eq!(report.spawned, 2);
        assert_eq!(report.deferred_scope_contention, 0);
        assert!(report.scope_contentions.is_empty());

        let _ = wait_for_outcomes(&runner, 2).await;
    }

    #[tokio::test]
    async fn role_less_request_bypasses_gate() {
        // A request without a `role` is dispatched even when scope caps
        // are configured: gate keys cannot be derived without a role.
        let queue = Arc::new(BudgetPauseDispatchQueue::new());
        let dispatcher = Arc::new(RecordingDispatcher::new(BudgetPauseDispatchReason::Resumed));

        let gate = ConcurrencyGate::new();
        gate.set_cap(Scope::Global, 0);
        gate.set_cap(Scope::Role("platform_lead".into()), 0);

        let runner = BudgetPauseRunner::new(queue.clone(), dispatcher.clone(), 4)
            .with_concurrency_gate(gate);

        // role: None — should bypass gate entirely.
        queue.enqueue(req(1, 100, "max_cost"));

        let report = runner.run_pending(CancellationToken::new()).await;
        assert_eq!(report.spawned, 1, "report = {report:?}");
        assert_eq!(report.deferred_scope_contention, 0);

        let _ = wait_for_outcomes(&runner, 1).await;
    }

    // ---- Scope-contention broadcaster wiring (CHECKLIST_v2 Phase 11
    //      §195 budget-pause subtask) ----------------------------------

    use crate::event_bus::EventBus;
    use crate::events::OrchestratorEvent;
    use crate::scope_contention_broadcaster::{
        ScopeContentionEventBroadcaster, ScopeContentionEventSink, ScopeContentionSinkError,
    };

    #[derive(Default)]
    struct RecordingSink {
        events: StdMutex<Vec<OrchestratorEvent>>,
    }

    impl RecordingSink {
        fn new() -> Arc<Self> {
            Arc::new(Self::default())
        }

        fn snapshot(&self) -> Vec<OrchestratorEvent> {
            self.events.lock().unwrap().clone()
        }
    }

    impl ScopeContentionEventSink for RecordingSink {
        fn append_scope_cap_reached(
            &self,
            event: &OrchestratorEvent,
        ) -> Result<i64, ScopeContentionSinkError> {
            assert!(matches!(event, OrchestratorEvent::ScopeCapReached { .. }));
            self.events.lock().unwrap().push(event.clone());
            Ok(self.events.lock().unwrap().len() as i64)
        }
    }

    /// N consecutive contended passes for the same dispatch produce
    /// exactly one durable `ScopeCapReached` event, and the event carries
    /// `identifier` (the decimal-formatted `pause_id`) — not `run_id`,
    /// because budget-pause dispatch does not reserve a `runs.id` row.
    #[tokio::test]
    async fn budget_pause_broadcaster_dedups_repeated_contention_to_one_event_per_episode() {
        let queue = Arc::new(BudgetPauseDispatchQueue::new());
        let dispatcher = Arc::new(RecordingDispatcher::new(BudgetPauseDispatchReason::Resumed));
        dispatcher.enable_gate();

        let gate = ConcurrencyGate::new();
        gate.set_cap(Scope::Role("platform_lead".into()), 1);

        let bus = EventBus::default();
        let sink = RecordingSink::new();
        let broadcaster = Arc::new(ScopeContentionEventBroadcaster::new(
            bus.clone(),
            Some(sink.clone() as Arc<dyn ScopeContentionEventSink>),
        ));

        let runner = BudgetPauseRunner::new(queue.clone(), dispatcher.clone(), 4)
            .with_concurrency_gate(gate)
            .with_scope_contention_broadcaster(broadcaster);

        // Pause-1 spawns; Pause-2 contends on the Role scope every pass.
        queue.enqueue(req_full(1, 100, "k", "platform_lead", None, None));
        queue.enqueue(req_full(2, 100, "k", "platform_lead", None, None));

        for _ in 0..5 {
            let _ = runner.run_pending(CancellationToken::new()).await;
        }

        let persisted = sink.snapshot();
        assert_eq!(
            persisted.len(),
            1,
            "5 contended passes must produce 1 durable event, got {persisted:?}",
        );
        match &persisted[0] {
            OrchestratorEvent::ScopeCapReached {
                run_id,
                identifier,
                scope_kind,
                scope_key,
                ..
            } => {
                assert!(
                    run_id.is_none(),
                    "budget pause has no runs.id; run_id must be None",
                );
                assert_eq!(
                    identifier.as_deref(),
                    Some("2"),
                    "identifier must be the decimal pause_id",
                );
                assert_eq!(*scope_kind, ScopeKind::Role);
                assert_eq!(scope_key, "platform_lead");
            }
            other => panic!("unexpected event: {other:?}"),
        }

        dispatcher.release_gate();
        let _ = wait_for_outcomes(&runner, 1).await;
        dispatcher.enable_gate();
        let _ = runner.run_pending(CancellationToken::new()).await;
        dispatcher.release_gate();
        let _ = wait_for_outcomes(&runner, 1).await;
    }

    /// Bus subscribers see the same event stream the persistence sink
    /// receives.
    #[tokio::test]
    async fn budget_pause_broadcaster_emits_on_bus_in_lockstep_with_persistence() {
        use futures::StreamExt;

        let queue = Arc::new(BudgetPauseDispatchQueue::new());
        let dispatcher = Arc::new(RecordingDispatcher::new(BudgetPauseDispatchReason::Resumed));
        dispatcher.enable_gate();

        let gate = ConcurrencyGate::new();
        gate.set_cap(Scope::Role("platform_lead".into()), 1);

        let bus = EventBus::default();
        let mut rx = bus.subscribe();
        let sink = RecordingSink::new();
        let broadcaster = Arc::new(ScopeContentionEventBroadcaster::new(
            bus.clone(),
            Some(sink.clone() as Arc<dyn ScopeContentionEventSink>),
        ));

        let runner = BudgetPauseRunner::new(queue.clone(), dispatcher.clone(), 4)
            .with_concurrency_gate(gate)
            .with_scope_contention_broadcaster(broadcaster);

        queue.enqueue(req_full(1, 100, "k", "platform_lead", None, None));
        queue.enqueue(req_full(2, 100, "k", "platform_lead", None, None));

        let _ = runner.run_pending(CancellationToken::new()).await;

        let bus_event = rx.next().await.expect("event").expect("not lagged");
        let persisted = sink.snapshot();
        assert_eq!(persisted.len(), 1);
        assert_eq!(bus_event, persisted[0]);

        dispatcher.release_gate();
        let _ = wait_for_outcomes(&runner, 1).await;
        dispatcher.enable_gate();
        let _ = runner.run_pending(CancellationToken::new()).await;
        dispatcher.release_gate();
        let _ = wait_for_outcomes(&runner, 1).await;
    }

    /// An idle pass after a contention episode terminates the episode in
    /// the dedup log so a fresh contention re-emits with the new
    /// `pause_id`.
    #[tokio::test]
    async fn budget_pause_idle_pass_terminates_episode_and_re_contention_re_emits() {
        let queue = Arc::new(BudgetPauseDispatchQueue::new());
        let dispatcher = Arc::new(RecordingDispatcher::new(BudgetPauseDispatchReason::Resumed));

        let gate = ConcurrencyGate::new();
        gate.set_cap(Scope::Role("platform_lead".into()), 1);

        let bus = EventBus::default();
        let sink = RecordingSink::new();
        let broadcaster = Arc::new(ScopeContentionEventBroadcaster::new(
            bus,
            Some(sink.clone() as Arc<dyn ScopeContentionEventSink>),
        ));

        let runner = BudgetPauseRunner::new(queue.clone(), dispatcher.clone(), 4)
            .with_concurrency_gate(gate)
            .with_scope_contention_broadcaster(broadcaster);

        // Episode 1: Pause-1 holds the Role permit, Pause-2 contends.
        dispatcher.enable_gate();
        queue.enqueue(req_full(1, 100, "k", "platform_lead", None, None));
        queue.enqueue(req_full(2, 100, "k", "platform_lead", None, None));
        let _ = runner.run_pending(CancellationToken::new()).await;
        assert_eq!(sink.snapshot().len(), 1);

        // Drain Pause-1 so the Role permit frees; next pass spawns
        // Pause-2 (no contention → episode ends).
        dispatcher.release_gate();
        let _ = wait_for_outcomes(&runner, 1).await;
        dispatcher.enable_gate();
        let _ = runner.run_pending(CancellationToken::new()).await;
        assert_eq!(sink.snapshot().len(), 1);

        // A truly idle pass while Pause-2 is in flight: still no new
        // event.
        let _ = runner.run_pending(CancellationToken::new()).await;
        assert_eq!(sink.snapshot().len(), 1);

        // Episode 2: Pause-2 still in flight; enqueue Pause-3 against the
        // still-held Role permit and observe a fresh event for "3".
        queue.enqueue(req_full(3, 100, "k", "platform_lead", None, None));
        let _ = runner.run_pending(CancellationToken::new()).await;

        let persisted = sink.snapshot();
        assert_eq!(
            persisted.len(),
            2,
            "second episode must re-emit after the first ended; got {persisted:?}",
        );
        match &persisted[1] {
            OrchestratorEvent::ScopeCapReached {
                run_id, identifier, ..
            } => {
                assert!(run_id.is_none());
                assert_eq!(identifier.as_deref(), Some("3"));
            }
            other => panic!("unexpected event: {other:?}"),
        }

        dispatcher.release_gate();
        let _ = wait_for_outcomes(&runner, 1).await;
        dispatcher.enable_gate();
        let _ = runner.run_pending(CancellationToken::new()).await;
        dispatcher.release_gate();
        let _ = wait_for_outcomes(&runner, 1).await;
    }

    // -- Pre-lease cancellation observation ------------------------------

    struct RecordingCancellationSink {
        calls: StdMutex<Vec<(RunRef, CancelRequest)>>,
    }

    impl RecordingCancellationSink {
        fn new() -> Self {
            Self {
                calls: StdMutex::new(Vec::new()),
            }
        }
        fn snapshot(&self) -> Vec<(RunRef, CancelRequest)> {
            self.calls.lock().unwrap().clone()
        }
    }

    impl RunCancellationStatusSink for RecordingCancellationSink {
        fn mark_cancelled(
            &self,
            run_id: RunRef,
            request: &CancelRequest,
        ) -> Result<bool, crate::specialist_runner::RunCancellationStatusError> {
            self.calls.lock().unwrap().push((run_id, request.clone()));
            Ok(true)
        }
    }

    #[tokio::test]
    async fn cancellation_observed_for_run_aborts_budget_pause_dispatch() {
        let queue = Arc::new(BudgetPauseDispatchQueue::new());
        let dispatcher = Arc::new(RecordingDispatcher::new(BudgetPauseDispatchReason::Resumed));
        let cancel_queue = Arc::new(Mutex::new(CancellationQueue::new()));
        let sink = Arc::new(RecordingCancellationSink::new());

        cancel_queue.lock().await.enqueue(CancelRequest::for_run(
            RunRef::new(42),
            "operator cancel",
            "tester",
            "2026-05-09T00:00:00Z",
        ));

        let runner = BudgetPauseRunner::new(queue.clone(), dispatcher.clone(), 4)
            .with_cancellation(cancel_queue.clone(), sink.clone());

        queue.enqueue(req_with_run(1, 7, "tokens", 42));

        let report = runner.run_pending(CancellationToken::new()).await;
        assert_eq!(report.spawned, 0);
        assert_eq!(report.cancelled_before_lease, 1);
        assert_eq!(report.cancellations[0].pause_id, BudgetPauseId::new(1));
        assert_eq!(report.cancellations[0].work_item_id, WorkItemId::new(7));
        assert_eq!(report.cancellations[0].run_id, RunRef::new(42));
        assert_eq!(report.cancellations[0].request.reason, "operator cancel");

        assert!(dispatcher.calls().is_empty());
        let calls = sink.snapshot();
        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0].0, RunRef::new(42));

        let q = cancel_queue.lock().await;
        assert!(q.pending_for_run(RunRef::new(42)).is_none());
    }

    #[tokio::test]
    async fn budget_pause_ignores_work_item_keyspace_per_observe_for_run_contract() {
        // Budget pause uses observe_for_run only — a work-item keyed
        // cancel must not abort the dispatch.
        let queue = Arc::new(BudgetPauseDispatchQueue::new());
        let dispatcher = Arc::new(RecordingDispatcher::new(BudgetPauseDispatchReason::Resumed));
        let cancel_queue = Arc::new(Mutex::new(CancellationQueue::new()));
        let sink = Arc::new(RecordingCancellationSink::new());

        cancel_queue
            .lock()
            .await
            .enqueue(CancelRequest::for_work_item(
                WorkItemId::new(7),
                "work item cancel",
                "tester",
                "2026-05-09T00:00:00Z",
            ));

        let runner = BudgetPauseRunner::new(queue.clone(), dispatcher.clone(), 4)
            .with_cancellation(cancel_queue.clone(), sink.clone());

        queue.enqueue(req_with_run(1, 7, "tokens", 99));

        let report = runner.run_pending(CancellationToken::new()).await;
        assert_eq!(report.spawned, 1, "report = {report:?}");
        assert_eq!(report.cancelled_before_lease, 0);

        let _ = wait_for_outcomes(&runner, 1).await;
        assert_eq!(dispatcher.calls(), vec![BudgetPauseId::new(1)]);
        assert!(sink.snapshot().is_empty());
    }

    #[tokio::test]
    async fn budget_pause_request_without_run_id_skips_cancellation_observation() {
        let queue = Arc::new(BudgetPauseDispatchQueue::new());
        let dispatcher = Arc::new(RecordingDispatcher::new(BudgetPauseDispatchReason::Resumed));
        let cancel_queue = Arc::new(Mutex::new(CancellationQueue::new()));
        let sink = Arc::new(RecordingCancellationSink::new());

        // Even an exact-numeric run keyspace match (id=1) must not abort
        // a request that has no run_id reservation.
        cancel_queue.lock().await.enqueue(CancelRequest::for_run(
            RunRef::new(1),
            "irrelevant",
            "tester",
            "2026-05-09T00:00:00Z",
        ));

        let runner = BudgetPauseRunner::new(queue.clone(), dispatcher.clone(), 4)
            .with_cancellation(cancel_queue.clone(), sink.clone());

        queue.enqueue(req(1, 7, "tokens"));

        let report = runner.run_pending(CancellationToken::new()).await;
        assert_eq!(report.spawned, 1);
        assert_eq!(report.cancelled_before_lease, 0);
        let _ = wait_for_outcomes(&runner, 1).await;
        assert!(sink.snapshot().is_empty());
    }
}
