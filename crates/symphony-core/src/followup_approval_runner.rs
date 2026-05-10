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
use crate::cancellation::{CancelRequest, CancellationQueue};
use crate::cancellation_observer::{CancellationDecision, observe_for_run_or_parent};
use crate::concurrency_gate::{
    ConcurrencyGate, DispatchTriple, RunnerScopes, ScopeContended, ScopeKind, ScopePermitSet,
};
use crate::followup::FollowupId;
use crate::followup_approval_tick::{
    FollowupApprovalDispatchQueue, FollowupApprovalDispatchRequest,
};
use crate::lease::{LeaseClock, LeaseConfig, LeaseOwner};
use crate::run_lease::{
    HeartbeatPulseObservation, LeaseAcquireOutcome, LeaseHeartbeat, RunLeaseGuard, RunLeaseStore,
    pulse_observed,
};
use crate::scope_contention_broadcaster::{ScopeContentionEventBroadcaster, ScopeFields};
use crate::specialist_runner::RunCancellationStatusSink;
use crate::work_item::WorkItemId;

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
    pub cancellations: Vec<FollowupApprovalCancellationObserved>,
}

impl FollowupApprovalRunReport {
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
/// check (SPEC v2 §4.5 / Phase 11.5) aborted a follow-up approval
/// dispatch.
///
/// The runner observes the run keyspace first and falls back to the
/// `source_work_item` keyspace so a cancel against the work item that
/// spawned the proposal still short-circuits a fresh approval dispatch.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FollowupApprovalCancellationObserved {
    /// Durable follow-up id the parked request carries.
    pub followup_id: FollowupId,
    /// Source work item that spawned the proposal — used as the
    /// cancellation parent keyspace for this runner.
    pub source_work_item: WorkItemId,
    /// Durable run row reserved for the dispatch, if any. The runner
    /// only marks the run `Cancelled` and drains the run-keyed queue
    /// entry when this is `Some`; parent-only observations leave the
    /// work-item entry pending so the propagator keeps fanning it out.
    pub run_id: Option<RunRef>,
    /// Reason / requester / timestamp clone snapshotted from the queue
    /// at observation time.
    pub request: CancelRequest,
}

/// Per-pass typed observation surfaced when a follow-up approval
/// dispatch could not acquire its [`RunnerScopes`] permit set because at
/// least one scope was at cap.
///
/// Pairs the parked request's follow-up id with the [`ScopeContended`]
/// reported by [`ConcurrencyGate::try_acquire`] so a downstream observer
/// can answer "why is this approval not running" without re-deriving the
/// scope keys from workflow config.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ScopeContendedObservation {
    /// Follow-up id the parked request carries.
    pub followup_id: FollowupId,
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
    fn from_contention(followup_id: FollowupId, contended: &ScopeContended) -> Self {
        Self {
            followup_id,
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
    /// Optional multi-scope concurrency gate. When `Some`, every
    /// dispatch carrying a `role` acquires
    /// `{Global, Role, AgentProfile?, Repository?}` permits before the
    /// approval handler runs and releases them on terminal completion.
    /// When `None` (or when a request has no `role`), only the local
    /// capacity semaphore caps concurrency. ARCHITECTURE_v2 ADR records
    /// that approval is gated under the configured
    /// `followups.approval_role` by convention — producers populate
    /// `role` from that config.
    concurrency_gate: Option<ConcurrencyGate>,
    /// Optional durable scope-contention broadcaster. When `Some`, every
    /// `run_pending` pass — including idle passes — feeds the runner's
    /// per-pass [`ScopeContendedObservation`]s into the shared dedup
    /// log so exactly one
    /// [`crate::events::OrchestratorEvent::ScopeCapReached`] event is
    /// broadcast on the bus and persisted via the configured sink per
    /// contention episode. Follow-up approval does not reserve a
    /// `runs.id` row, so emitted events carry `identifier`
    /// (`followup_id` formatted as a decimal string) rather than
    /// `run_id`. When `None`, observations remain on the run report
    /// only.
    scope_contention_broadcaster: Option<Arc<ScopeContentionEventBroadcaster>>,
    /// Optional cooperative-cancellation wiring (SPEC v2 §4.5 / Phase
    /// 11.5).
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
            concurrency_gate: None,
            scope_contention_broadcaster: None,
            cancellation: None,
        }
    }

    /// Wire cooperative cancellation observation into the runner
    /// (SPEC v2 §4.5 / Phase 11.5).
    ///
    /// After scope acquisition and before lease acquisition, the runner
    /// calls [`observe_for_run_or_parent`] against the shared queue
    /// using the request's `run_id` and its always-present
    /// `source_work_item`. On an `Abort` outcome the runner releases
    /// the just-acquired scope/capacity permits, marks the run
    /// `Cancelled` via the typed-status sink (when a `run_id` is
    /// reserved), and drains the run-keyed queue entry. Parent-only
    /// observations leave the work-item entry pending for the
    /// propagator.
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
    /// observations to the broadcaster. The broadcaster's per-pass
    /// dedup log is what enforces the "one
    /// [`crate::events::OrchestratorEvent::ScopeCapReached`] per
    /// contention episode" invariant on the bus and the persistence
    /// sink. Idle passes with no current contentions are required to
    /// terminate stale episodes from the prior pass; the runner therefore
    /// always invokes the broadcaster when wiring is configured.
    ///
    /// Follow-up approval dispatches do not reserve a `runs.id` row, so
    /// emitted events use [`crate::scope_contention_broadcaster::ContentionSubject::Identifier`]
    /// keyed on the decimal-formatted `followup_id` and carry
    /// `identifier` (not `run_id`).
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
            // Even on an idle pass we must notify the broadcaster so any
            // episodes that were active in a prior pass get terminated
            // (the dedup log treats absence as "episode ended"). Without
            // this, a parked dispatch that drains terminally would leave
            // a stale episode in the log forever.
            self.broadcast_pass_observations(&[]);
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
                            req.followup_id,
                            &contended,
                        ));
                    debug!(
                        followup_id = req.followup_id.get(),
                        scope_kind = ?contended.scope.kind(),
                        scope_key = %contended.scope.key(),
                        in_flight = contended.in_flight,
                        cap = contended.cap,
                        "followup approval runner: scope contended; parking",
                    );
                    new_deferred.push(req);
                    report.deferred_scope_contention += 1;
                    continue;
                }
            };

            // Cooperative cancellation observation (SPEC v2 §4.5).
            if let Some(wiring) = self.cancellation.as_ref() {
                let observed: Option<CancelRequest> = {
                    let q = wiring.queue.lock().await;
                    let from_run = req.run_id.and_then(|run_id| {
                        match observe_for_run_or_parent(&q, run_id, None) {
                            CancellationDecision::Abort(req) => Some(req),
                            CancellationDecision::Proceed => None,
                        }
                    });
                    from_run.or_else(|| q.pending_for_work_item(req.source_work_item).cloned())
                };
                if let Some(cancel_req) = observed {
                    drop(scope_permits);
                    drop(permit);
                    if let Some(run_id) = req.run_id
                        && let Err(err) = wiring.sink.mark_cancelled(run_id, &cancel_req)
                    {
                        warn!(
                            followup_id = req.followup_id.get(),
                            run_id = %run_id,
                            error = %err,
                            "followup approval runner: cancellation status sink failed; relying on operator re-issue",
                        );
                    }
                    if let Some(run_id) = req.run_id {
                        let mut q = wiring.queue.lock().await;
                        q.drain_for_run(run_id);
                    }
                    debug!(
                        followup_id = req.followup_id.get(),
                        reason = %cancel_req.reason,
                        "followup approval runner: pre-lease cancel observed; aborted dispatch",
                    );
                    report.cancelled_before_lease += 1;
                    report
                        .cancellations
                        .push(FollowupApprovalCancellationObserved {
                            followup_id: req.followup_id,
                            source_work_item: req.source_work_item,
                            run_id: req.run_id,
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
                // Release scope permits exactly once on terminal exit;
                // ScopePermitSet::Drop is idempotent across the move.
                drop(scope_permits);
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
                .map(|obs| (obs.followup_id.to_string(), obs.fields())),
        );
    }

    /// Attempt to acquire scope permits for `req` against the configured
    /// [`ConcurrencyGate`]. Returns `Ok(None)` when no gate is wired or
    /// the request lacks a `role`, `Ok(Some(set))` when every scope had
    /// headroom, and `Err(_)` when any scope was at cap (no permits held
    /// — the inner gate already rolled back partial acquisitions).
    fn try_acquire_scopes(
        &self,
        req: &FollowupApprovalDispatchRequest,
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
            role: None,
            agent_profile: None,
            repository: None,
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
            role: None,
            agent_profile: None,
            repository: None,
        }
    }

    fn req_full(
        id: i64,
        source: i64,
        title: &str,
        role: &str,
        agent_profile: Option<&str>,
        repository: Option<&str>,
    ) -> FollowupApprovalDispatchRequest {
        FollowupApprovalDispatchRequest {
            followup_id: FollowupId::new(id),
            source_work_item: WorkItemId::new(source),
            title: title.into(),
            blocking: false,
            approval_role: RoleName::new("platform_lead"),
            run_id: None,
            role: Some(RoleName::new(role)),
            agent_profile: agent_profile.map(str::to_owned),
            repository: repository.map(str::to_owned),
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

    // ---- Concurrency-gate wiring (CHECKLIST_v2 Phase 11) -----------

    use crate::concurrency_gate::Scope;

    #[tokio::test]
    async fn approval_role_cap_serializes_dispatch_independent_of_qa_cap() {
        // The follow-up approval role's cap must hold even when other
        // role caps (QA, specialist) allow more. A QA cap of 99 must
        // not affect approval dispatch.
        let queue = Arc::new(FollowupApprovalDispatchQueue::new());
        let dispatcher = Arc::new(RecordingDispatcher::new(
            FollowupApprovalDispatchReason::Approved,
        ));
        dispatcher.enable_gate();

        let gate = ConcurrencyGate::new();
        gate.set_cap(Scope::Role("platform_lead".into()), 1);
        gate.set_cap(Scope::Role("qa".into()), 99);

        let runner = FollowupApprovalRunner::new(queue.clone(), dispatcher.clone(), 4)
            .with_concurrency_gate(gate);

        queue.enqueue(req_full(1, 100, "a", "platform_lead", None, None));
        queue.enqueue(req_full(2, 100, "b", "platform_lead", None, None));
        queue.enqueue(req_full(3, 100, "c", "platform_lead", None, None));

        let report = runner.run_pending(CancellationToken::new()).await;
        assert_eq!(report.spawned, 1, "report = {report:?}");
        assert_eq!(report.deferred_scope_contention, 2);
        assert_eq!(report.scope_contentions.len(), 2);
        for obs in &report.scope_contentions {
            assert_eq!(obs.scope_kind, crate::concurrency_gate::ScopeKind::Role);
            assert_eq!(obs.scope_key, "platform_lead");
            assert_eq!(obs.cap, 1);
            assert_eq!(obs.in_flight, 1);
        }
        let parked: Vec<i64> = report
            .scope_contentions
            .iter()
            .map(|o| o.followup_id.get())
            .collect();
        assert_eq!(parked, vec![2, 3]);

        // Local capacity unaffected — parked dispatches did not consume
        // permits.
        assert_eq!(runner.inflight_count().await, 1);
        assert_eq!(runner.deferred_count().await, 2);

        dispatcher.enable_gate();
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
    async fn approval_repository_cap_parks_without_consuming_capacity() {
        // A per-repository cap defers the second dispatch but the local
        // capacity permit must NOT be consumed: a third (different
        // repo) dispatch on the same pass still spawns.
        let queue = Arc::new(FollowupApprovalDispatchQueue::new());
        let dispatcher = Arc::new(RecordingDispatcher::new(
            FollowupApprovalDispatchReason::Approved,
        ));
        dispatcher.enable_gate();

        let gate = ConcurrencyGate::new();
        gate.set_cap(Scope::Repository("acme/widgets".into()), 1);

        let runner = FollowupApprovalRunner::new(queue.clone(), dispatcher.clone(), 2)
            .with_concurrency_gate(gate);

        queue.enqueue(req_full(
            1,
            100,
            "a",
            "platform_lead",
            None,
            Some("acme/widgets"),
        ));
        queue.enqueue(req_full(
            2,
            100,
            "b",
            "platform_lead",
            None,
            Some("acme/widgets"),
        ));
        queue.enqueue(req_full(
            3,
            100,
            "c",
            "platform_lead",
            None,
            Some("acme/other"),
        ));

        let report = runner.run_pending(CancellationToken::new()).await;
        assert_eq!(report.spawned, 2, "report = {report:?}");
        assert_eq!(report.deferred_scope_contention, 1);
        let obs = &report.scope_contentions[0];
        assert_eq!(
            obs.scope_kind,
            crate::concurrency_gate::ScopeKind::Repository
        );
        assert_eq!(obs.scope_key, "acme/widgets");
        assert_eq!(obs.followup_id, FollowupId::new(2));

        dispatcher.release_gate();
        let _ = wait_for_outcomes(&runner, 2).await;
    }

    #[tokio::test]
    async fn approval_no_concurrency_gate_means_no_scope_observations() {
        let queue = Arc::new(FollowupApprovalDispatchQueue::new());
        let dispatcher = Arc::new(RecordingDispatcher::new(
            FollowupApprovalDispatchReason::Approved,
        ));
        let runner = FollowupApprovalRunner::new(queue.clone(), dispatcher.clone(), 4);
        queue.enqueue(req_full(
            1,
            100,
            "a",
            "platform_lead",
            Some("claude"),
            Some("acme/widgets"),
        ));
        let report = runner.run_pending(CancellationToken::new()).await;
        assert_eq!(report.spawned, 1);
        assert!(report.scope_contentions.is_empty());
        assert_eq!(report.deferred_scope_contention, 0);
        let _ = wait_for_outcomes(&runner, 1).await;
    }

    #[tokio::test]
    async fn approval_dispatch_without_role_bypasses_concurrency_gate() {
        // Requests without a `role` must bypass gate acquisition even
        // when a gate is wired — the tick today emits role-less
        // requests, and they should not block on a Global cap.
        let queue = Arc::new(FollowupApprovalDispatchQueue::new());
        let dispatcher = Arc::new(RecordingDispatcher::new(
            FollowupApprovalDispatchReason::Approved,
        ));

        let gate = ConcurrencyGate::new();
        gate.set_cap(Scope::Global, 0);

        let runner = FollowupApprovalRunner::new(queue.clone(), dispatcher.clone(), 4)
            .with_concurrency_gate(gate);

        queue.enqueue(req(1, 100, "a", false));
        let report = runner.run_pending(CancellationToken::new()).await;
        assert_eq!(report.spawned, 1, "report = {report:?}");
        assert!(report.scope_contentions.is_empty());
        let _ = wait_for_outcomes(&runner, 1).await;
    }

    // ---- Scope-contention broadcaster wiring (CHECKLIST_v2 Phase 11
    //      §195 follow-up-approval subtask) -------------------------------

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
    /// `identifier` (the decimal-formatted `followup_id`) — not `run_id`,
    /// because follow-up approval does not reserve a `runs.id` row.
    #[tokio::test]
    async fn followup_broadcaster_dedups_repeated_contention_to_one_event_per_episode() {
        let queue = Arc::new(FollowupApprovalDispatchQueue::new());
        let dispatcher = Arc::new(RecordingDispatcher::new(
            FollowupApprovalDispatchReason::Approved,
        ));
        dispatcher.enable_gate();

        let gate = ConcurrencyGate::new();
        gate.set_cap(Scope::Role("platform_lead".into()), 1);

        let bus = EventBus::default();
        let sink = RecordingSink::new();
        let broadcaster = Arc::new(ScopeContentionEventBroadcaster::new(
            bus.clone(),
            Some(sink.clone() as Arc<dyn ScopeContentionEventSink>),
        ));

        let runner = FollowupApprovalRunner::new(queue.clone(), dispatcher.clone(), 4)
            .with_concurrency_gate(gate)
            .with_scope_contention_broadcaster(broadcaster);

        // FU-1 spawns; FU-2 contends on the Role scope every pass.
        queue.enqueue(req_full(1, 100, "a", "platform_lead", None, None));
        queue.enqueue(req_full(2, 100, "b", "platform_lead", None, None));

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
                    "follow-up approval has no runs.id; run_id must be None",
                );
                assert_eq!(
                    identifier.as_deref(),
                    Some("2"),
                    "identifier must be the decimal followup_id",
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
    async fn followup_broadcaster_emits_on_bus_in_lockstep_with_persistence() {
        use futures::StreamExt;

        let queue = Arc::new(FollowupApprovalDispatchQueue::new());
        let dispatcher = Arc::new(RecordingDispatcher::new(
            FollowupApprovalDispatchReason::Approved,
        ));
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

        let runner = FollowupApprovalRunner::new(queue.clone(), dispatcher.clone(), 4)
            .with_concurrency_gate(gate)
            .with_scope_contention_broadcaster(broadcaster);

        queue.enqueue(req_full(1, 100, "a", "platform_lead", None, None));
        queue.enqueue(req_full(2, 100, "b", "platform_lead", None, None));

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
    /// the dedup log so a fresh contention re-emits.
    #[tokio::test]
    async fn followup_idle_pass_terminates_episode_and_re_contention_re_emits() {
        let queue = Arc::new(FollowupApprovalDispatchQueue::new());
        let dispatcher = Arc::new(RecordingDispatcher::new(
            FollowupApprovalDispatchReason::Approved,
        ));

        let gate = ConcurrencyGate::new();
        gate.set_cap(Scope::Role("platform_lead".into()), 1);

        let bus = EventBus::default();
        let sink = RecordingSink::new();
        let broadcaster = Arc::new(ScopeContentionEventBroadcaster::new(
            bus,
            Some(sink.clone() as Arc<dyn ScopeContentionEventSink>),
        ));

        let runner = FollowupApprovalRunner::new(queue.clone(), dispatcher.clone(), 4)
            .with_concurrency_gate(gate)
            .with_scope_contention_broadcaster(broadcaster);

        // Episode 1: FU-1 holds the Role permit, FU-2 contends.
        dispatcher.enable_gate();
        queue.enqueue(req_full(1, 100, "a", "platform_lead", None, None));
        queue.enqueue(req_full(2, 100, "b", "platform_lead", None, None));
        let _ = runner.run_pending(CancellationToken::new()).await;
        assert_eq!(sink.snapshot().len(), 1);

        // Drain FU-1 so the Role permit frees; next pass spawns FU-2
        // (no contention → episode ends).
        dispatcher.release_gate();
        let _ = wait_for_outcomes(&runner, 1).await;
        dispatcher.enable_gate();
        let _ = runner.run_pending(CancellationToken::new()).await;
        assert_eq!(sink.snapshot().len(), 1);

        // A truly idle pass while FU-2 is in flight: still no new event.
        let _ = runner.run_pending(CancellationToken::new()).await;
        assert_eq!(sink.snapshot().len(), 1);

        // Episode 2: FU-2 still in flight; enqueue FU-3 against the
        // still-held Role permit and observe a fresh event for "3".
        queue.enqueue(req_full(3, 100, "c", "platform_lead", None, None));
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
    async fn cancellation_observed_for_run_aborts_followup_dispatch() {
        let queue = Arc::new(FollowupApprovalDispatchQueue::new());
        let dispatcher = Arc::new(RecordingDispatcher::new(
            FollowupApprovalDispatchReason::Approved,
        ));
        let cancel_queue = Arc::new(Mutex::new(CancellationQueue::new()));
        let sink = Arc::new(RecordingCancellationSink::new());

        cancel_queue.lock().await.enqueue(CancelRequest::for_run(
            RunRef::new(42),
            "operator cancel",
            "tester",
            "2026-05-09T00:00:00Z",
        ));

        let runner = FollowupApprovalRunner::new(queue.clone(), dispatcher.clone(), 4)
            .with_cancellation(cancel_queue.clone(), sink.clone());

        queue.enqueue(req_with_run(1, 7, "fix", false, 42));

        let report = runner.run_pending(CancellationToken::new()).await;
        assert_eq!(report.spawned, 0);
        assert_eq!(report.cancelled_before_lease, 1);
        assert_eq!(report.cancellations[0].followup_id, FollowupId::new(1));
        assert_eq!(report.cancellations[0].source_work_item, WorkItemId::new(7));
        assert_eq!(report.cancellations[0].run_id, Some(RunRef::new(42)));
        assert_eq!(report.cancellations[0].request.reason, "operator cancel");

        assert!(dispatcher.calls().is_empty());
        let calls = sink.snapshot();
        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0].0, RunRef::new(42));

        let q = cancel_queue.lock().await;
        assert!(q.pending_for_run(RunRef::new(42)).is_none());
    }

    #[tokio::test]
    async fn cancellation_observed_via_source_work_item_aborts_followup_but_leaves_entry() {
        let queue = Arc::new(FollowupApprovalDispatchQueue::new());
        let dispatcher = Arc::new(RecordingDispatcher::new(
            FollowupApprovalDispatchReason::Approved,
        ));
        let cancel_queue = Arc::new(Mutex::new(CancellationQueue::new()));
        let sink = Arc::new(RecordingCancellationSink::new());

        cancel_queue
            .lock()
            .await
            .enqueue(CancelRequest::for_work_item(
                WorkItemId::new(7),
                "source cancel",
                "tester",
                "2026-05-09T00:00:00Z",
            ));

        let runner = FollowupApprovalRunner::new(queue.clone(), dispatcher.clone(), 4)
            .with_cancellation(cancel_queue.clone(), sink.clone());

        queue.enqueue(req_with_run(1, 7, "fix", false, 99));

        let report = runner.run_pending(CancellationToken::new()).await;
        assert_eq!(report.cancelled_before_lease, 1);
        assert_eq!(report.cancellations[0].request.reason, "source cancel");

        let calls = sink.snapshot();
        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0].0, RunRef::new(99));

        let q = cancel_queue.lock().await;
        assert!(q.pending_for_work_item(WorkItemId::new(7)).is_some());
        assert!(q.pending_for_run(RunRef::new(99)).is_none());
    }

    #[tokio::test]
    async fn cancellation_without_run_reservation_skips_followup_sink_and_drain() {
        let queue = Arc::new(FollowupApprovalDispatchQueue::new());
        let dispatcher = Arc::new(RecordingDispatcher::new(
            FollowupApprovalDispatchReason::Approved,
        ));
        let cancel_queue = Arc::new(Mutex::new(CancellationQueue::new()));
        let sink = Arc::new(RecordingCancellationSink::new());

        cancel_queue
            .lock()
            .await
            .enqueue(CancelRequest::for_work_item(
                WorkItemId::new(7),
                "source cancel",
                "tester",
                "2026-05-09T00:00:00Z",
            ));

        let runner = FollowupApprovalRunner::new(queue.clone(), dispatcher.clone(), 4)
            .with_cancellation(cancel_queue.clone(), sink.clone());

        queue.enqueue(req(1, 7, "fix", false));

        let report = runner.run_pending(CancellationToken::new()).await;
        assert_eq!(report.cancelled_before_lease, 1);
        assert!(dispatcher.calls().is_empty());
        assert!(sink.snapshot().is_empty());

        let q = cancel_queue.lock().await;
        assert!(q.pending_for_work_item(WorkItemId::new(7)).is_some());
    }
}
