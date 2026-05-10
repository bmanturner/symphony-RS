//! Specialist dispatch runner — drains a [`SpecialistDispatchQueue`] and
//! invokes the per-issue [`Dispatcher`] for each pending request, bounded
//! by a max-concurrency [`Semaphore`] (SPEC v2 §5 / ARCHITECTURE v2 §7.1).
//!
//! Phase 11 of the v2 checklist replaces the flat
//! [`crate::poll_loop::PollLoop`] entry point with the [`crate::scheduler_v2::SchedulerV2`]
//! tick fan. The fan already enqueues dispatch *requests*; this runner is
//! the missing consumer that turns those requests into actual dispatcher
//! invocations under the same `agent.max_concurrent_agents` ceiling that
//! the flat poll loop enforces today. Each runner pass:
//!
//! 1. Drains pending requests from [`SpecialistDispatchQueue`] and folds
//!    them onto the runner's deferred-from-prior-pass queue (front-first
//!    so age-order is preserved across passes).
//! 2. Walks the combined queue. For each request, looks up the
//!    corresponding [`Issue`] in [`ActiveSetStore`]; if missing, records
//!    `missing_issue` and drops the request — the next intake pass will
//!    decide whether the issue is still active.
//! 3. Tries to acquire a semaphore permit non-blockingly. If unavailable
//!    the request is parked back in the runner's deferred queue and
//!    counted as `deferred_capacity`. Permit-holding requests spawn a
//!    dispatcher task into the runner's [`JoinSet`]; the spawned future
//!    holds the owned semaphore permit until the dispatcher returns,
//!    so capacity is automatically restored on completion.
//!
//! The runner intentionally does *not* own cadence (the scheduler does),
//! does *not* mutate [`crate::state_machine::StateMachine`] (the flat
//! loop still owns that ledger today), and does *not* reap finished
//! tasks on its own. Reaping is exposed as [`SpecialistRunner::reap_completed`]
//! so the caller (later: the v2 scheduler wiring in `symphony-cli`) can
//! drive both spawn and reap on its own schedule. Keeping the runner
//! narrow lets it slot into the existing composition root without
//! disturbing PollLoop.

use std::collections::HashMap;
use std::sync::Arc;

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
use crate::intake_tick::ActiveSetStore;
use crate::lease::{LeaseClock, LeaseConfig, LeaseOwner};
use crate::poll_loop::Dispatcher;
use crate::run_lease::{
    HeartbeatPulseObservation, LeaseAcquireOutcome, LeaseHeartbeat, RunLeaseGuard, RunLeaseStore,
    pulse_observed,
};
use crate::scope_contention_broadcaster::{
    ContentionSubject, ScopeContentionEventBroadcaster, ScopeFields,
};
use crate::specialist_tick::{SpecialistDispatchQueue, SpecialistDispatchRequest};
use crate::state_machine::ReleaseReason;
use crate::tracker::Issue;

/// Per-pass observability summary for [`SpecialistRunner::run_pending`].
///
/// Counts only — no payloads — so callers can log or assert cheaply
/// without snapshotting requests. The three buckets are mutually
/// exclusive: every drained request lands in exactly one of `spawned`,
/// `deferred_capacity`, or `missing_issue`.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct SpecialistRunReport {
    /// Requests for which a dispatcher task was spawned this pass.
    pub spawned: usize,
    /// Requests that could not be spawned because the semaphore was
    /// already saturated. Parked on the runner's internal deferred queue
    /// for the next pass.
    pub deferred_capacity: usize,
    /// Requests whose issue id was no longer in the active-set snapshot
    /// at lookup time. Dropped — the next intake/specialist pass will
    /// re-emit if the issue is still routable.
    pub missing_issue: usize,
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
    /// (SPEC v2 §4.5 / Phase 11.5). The runner released its capacity
    /// permit and any held scope permits, transitioned the run row to
    /// [`crate::RunStatus::Cancelled`] via the configured sink (when the
    /// request carried a `run_id`), and drained the run-keyed entry from
    /// the [`CancellationQueue`]. Length matches `cancellations.len()`.
    pub cancelled_before_lease: usize,
    /// Typed observations for every dispatch that was aborted by the
    /// pre-lease cancellation check this pass, in walk order. The
    /// associated [`CancelRequest`] is preserved so downstream observers
    /// can persist or log the operator-supplied reason without holding
    /// the queue lock across the wind-down path.
    pub cancellations: Vec<CancellationObserved>,
}

impl SpecialistRunReport {
    /// `true` iff the pass was a complete no-op (nothing drained,
    /// nothing parked).
    pub fn is_idle(&self) -> bool {
        self.spawned == 0
            && self.deferred_capacity == 0
            && self.missing_issue == 0
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
/// check (SPEC v2 §4.5 / Phase 11.5) aborted a dispatch.
///
/// Pairs the request's tracker-facing `identifier` and (optional)
/// reserved `run_id` with the [`CancelRequest`] that was observed in the
/// shared [`CancellationQueue`]. Recording the request here lets a
/// downstream observer (durable event subtask, status TUI, operator log)
/// answer "why did we never run this dispatch" without re-locking the
/// queue.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CancellationObserved {
    /// Tracker-facing identifier the parked request carries.
    pub identifier: String,
    /// Durable run row reserved for the dispatch, if the request had
    /// one. The runner only marks the run `Cancelled` and drains the
    /// run-keyed queue entry when this is `Some`; parent-only
    /// observations leave the work-item entry pending so the propagator
    /// keeps fanning it out.
    pub run_id: Option<RunRef>,
    /// Reason / requester / timestamp clone snapshotted from the queue
    /// at observation time. Owned, so the runner can record it without
    /// holding the queue lock across the lease-release path.
    pub request: CancelRequest,
}

/// Errors a [`RunCancellationStatusSink`] may return when the runner
/// asks it to mark a run `Cancelled`. The runner does not interpret
/// these — it logs and continues so a transient sink failure cannot
/// strand a cancel observation. The operator can re-issue the cancel
/// (which is idempotent at the queue layer) and the typed-status
/// reaper will re-converge the row on the next pass.
#[derive(Debug, thiserror::Error)]
pub enum RunCancellationStatusError {
    /// The sink rejected the transition (e.g. invalid status edge,
    /// missing row). Carries an opaque message for diagnostics.
    #[error("rejected: {0}")]
    Rejected(String),
    /// The sink encountered a backend / I/O failure.
    #[error("backend error: {0}")]
    Backend(String),
}

/// Kernel-side seam the runner uses to flow an observed cancel through
/// the typed `runs.status` gate (SPEC v2 §4.5 / Phase 11.5).
///
/// The composition root wires this to
/// `symphony_state::RunRepository::update_run_status_typed` with
/// [`crate::RunStatus::Cancelled`]; the runner stays free of any
/// `symphony-state` dependency. The trait is intentionally narrow: the
/// runner only ever needs to mark a single run cancelled, and the
/// implementation is expected to be idempotent (a re-marked run that is
/// already `Cancelled` returns `Ok(false)` rather than erroring).
pub trait RunCancellationStatusSink: Send + Sync {
    /// Mark the durable run row identified by `run_id` as `Cancelled`.
    /// Returns `Ok(true)` when the transition wrote a row, `Ok(false)`
    /// when the row was already terminal (idempotent re-cancel), and
    /// [`RunCancellationStatusError`] for everything else.
    fn mark_cancelled(
        &self,
        run_id: RunRef,
        request: &CancelRequest,
    ) -> Result<bool, RunCancellationStatusError>;
}

/// Per-pass typed observation surfaced when a dispatch could not acquire
/// its [`RunnerScopes`] permit set because at least one scope was at cap.
///
/// Pairs the request's tracker-facing `identifier` with the
/// [`ScopeContended`] reported by [`ConcurrencyGate::try_acquire`] so a
/// downstream observer (the durable-event subtask, the operator log, a
/// status TUI) can answer "why is this issue not running" without
/// re-deriving the scope keys from workflow config.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ScopeContendedObservation {
    /// Tracker-facing identifier the parked request carries.
    pub identifier: String,
    /// Durable run row reserved for this dispatch, if the request had
    /// one. Drives the [`ContentionSubject`] used by the durable
    /// [`ScopeContentionEventBroadcaster`]: `Run(run_id)` when present,
    /// `Identifier(identifier)` otherwise.
    pub run_id: Option<RunRef>,
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
    fn from_contention(
        identifier: String,
        run_id: Option<RunRef>,
        contended: &ScopeContended,
    ) -> Self {
        Self {
            identifier,
            run_id,
            scope_kind: contended.scope.kind(),
            scope_key: contended.scope.key().to_owned(),
            in_flight: contended.in_flight,
            cap: contended.cap,
        }
    }

    fn subject(&self) -> ContentionSubject {
        match self.run_id {
            Some(r) => ContentionSubject::Run(r.get()),
            None => ContentionSubject::Identifier(self.identifier.clone()),
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
/// [`SpecialistRunner::pulse_heartbeats`].
///
/// Pairs the typed [`HeartbeatPulseObservation`] from
/// [`crate::run_lease::pulse_observed`] with the run/identifier the
/// heartbeat targets so the scheduler can route the observation
/// (durable run-status update on contention, recovery dispatch on
/// not-found, transient-error log on backend error) without re-keying
/// against the in-flight set.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RunnerHeartbeatObservation {
    /// Durable run row the heartbeat was renewed against.
    pub run_id: RunRef,
    /// Tracker-facing identifier of the dispatch that owns the run row.
    pub identifier: String,
    /// Typed outcome of the renewal pulse.
    pub observation: HeartbeatPulseObservation,
}

/// Outcome reported when a spawned dispatcher task completes.
///
/// Returned in batches by [`SpecialistRunner::reap_completed`]. The
/// `release_reason` is the value the dispatcher returned; the runner
/// does not reinterpret it — that is the scheduler/state-machine layer's
/// job (Phase 11 follow-ups will add lease release here).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SpecialistDispatchOutcome {
    /// Tracker-facing identifier the request carried.
    pub identifier: String,
    /// Final [`ReleaseReason`] from [`Dispatcher::dispatch`].
    pub release_reason: ReleaseReason,
}

/// Bounded-concurrency consumer for [`SpecialistDispatchQueue`].
///
/// Construct with [`SpecialistRunner::new`] and drive with
/// [`SpecialistRunner::run_pending`]. The runner is `&self`-only so the
/// scheduler can hold it in an `Arc` and call it from any tick context.
pub struct SpecialistRunner {
    queue: Arc<SpecialistDispatchQueue>,
    active_set: Arc<ActiveSetStore>,
    dispatcher: Arc<dyn Dispatcher>,
    semaphore: Arc<Semaphore>,
    /// Requests parked across passes when capacity was unavailable.
    /// Front-first FIFO so age-order is preserved.
    deferred: Mutex<Vec<SpecialistDispatchRequest>>,
    inflight: Mutex<JoinSet<SpecialistDispatchOutcome>>,
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
    /// dispatch acquires `{Global, Role, AgentProfile?, Repository?}`
    /// permits before the agent runs and releases them on terminal
    /// completion. When `None`, only the local capacity semaphore caps
    /// concurrency.
    concurrency_gate: Option<ConcurrencyGate>,
    /// Optional durable scope-contention broadcaster. When `Some`, every
    /// `run_pending` pass — including idle passes — feeds the runner's
    /// per-pass [`ScopeContendedObservation`]s into the shared dedup
    /// log so exactly one
    /// [`crate::events::OrchestratorEvent::ScopeCapReached`] event is
    /// broadcast on the bus and persisted via the configured sink per
    /// contention episode. When `None`, observations remain on the run
    /// report only.
    scope_contention_broadcaster: Option<Arc<ScopeContentionEventBroadcaster>>,
    /// Optional cooperative cancellation wiring (SPEC v2 §4.5 / Phase
    /// 11.5). When `Some`, every dispatch consults the shared queue at
    /// the pre-lease safe point and aborts cleanly on a pending cancel.
    /// When `None`, dispatches proceed without observation — required
    /// during the rollout because composition roots opt in incrementally.
    cancellation: Option<CancellationWiring>,
}

/// Bundle holding the shared in-memory [`CancellationQueue`] (typically
/// wrapped in `Arc<tokio::sync::Mutex<_>>` so the propagator and CLI can
/// enqueue concurrently) and the typed-status sink the runner flows an
/// observed cancel through.
struct CancellationWiring {
    queue: Arc<Mutex<CancellationQueue>>,
    sink: Arc<dyn RunCancellationStatusSink>,
}

/// Per-run heartbeat registration: the human-readable identifier (so
/// observations carry it back to the caller) plus the live heartbeat
/// guarded by an async mutex (`pulse` is `&mut self`).
struct HeartbeatRegistration {
    identifier: String,
    heartbeat: Arc<Mutex<LeaseHeartbeat>>,
}

/// Bundle holding the lease store, owner identity, TTL/renewal config,
/// and the clock used to derive `(now, expires_at)` per acquisition.
struct LeaseWiring {
    store: Arc<dyn RunLeaseStore>,
    owner: LeaseOwner,
    config: LeaseConfig,
    clock: Arc<dyn LeaseClock>,
}

impl SpecialistRunner {
    /// Build a runner with capacity `max_concurrent`. A capacity of `0`
    /// disables dispatch entirely — every drained request will be
    /// counted as `deferred_capacity`. The flat poll loop accepts the
    /// same convention so this matches the existing escape hatch for
    /// "drain but never run".
    pub fn new(
        queue: Arc<SpecialistDispatchQueue>,
        active_set: Arc<ActiveSetStore>,
        dispatcher: Arc<dyn Dispatcher>,
        max_concurrent: usize,
    ) -> Self {
        Self {
            queue,
            active_set,
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
    /// After every successful scope acquisition and before lease
    /// acquisition, the runner calls
    /// [`observe_for_run_or_parent`] against the shared queue using the
    /// request's `run_id` and `parent_work_item_id`. On a
    /// [`CancellationDecision::Abort`] outcome the runner:
    ///
    /// 1. Drops the just-acquired scope permits and the local capacity
    ///    permit so a parallel routable dispatch can take that slot in
    ///    the same pass — the lease has not been acquired yet, so there
    ///    is nothing further to release.
    /// 2. Calls `sink.mark_cancelled(run_id, &request)` when the request
    ///    carries a `run_id`. The sink's job is to flow the transition
    ///    through the typed `runs.status` gate; failures are logged and
    ///    do not block the wind-down (the operator can re-issue the
    ///    cancel, which is idempotent).
    /// 3. Drains the run-keyed queue entry. Work-item-keyed entries are
    ///    left intact so the propagator keeps fanning the parent cancel
    ///    out to other in-flight children.
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
    pub fn with_scope_contention_broadcaster(
        mut self,
        broadcaster: Arc<ScopeContentionEventBroadcaster>,
    ) -> Self {
        self.scope_contention_broadcaster = Some(broadcaster);
        self
    }

    /// Wire a [`ConcurrencyGate`] into the runner.
    ///
    /// Each subsequent dispatch builds a [`DispatchTriple`] from
    /// `(role, agent_profile, repository)` on the request, calls
    /// [`RunnerScopes::try_acquire`], and only proceeds when every
    /// configured scope has headroom. A contended scope produces a
    /// [`ScopeContendedObservation`] on the pass report and the request
    /// is parked on the deferred queue for a future pass — capacity and
    /// lease are *not* consumed when the gate rejects.
    pub fn with_concurrency_gate(mut self, gate: ConcurrencyGate) -> Self {
        self.concurrency_gate = Some(gate);
        self
    }

    /// Enable durable-lease wiring. Each subsequent dispatch whose
    /// request carries a `run_id` will:
    ///
    /// 1. Acquire the lease on `run_id` via `store.acquire(...)` with
    ///    `(now, expires_at)` derived from `clock` and
    ///    `config.default_ttl_ms`.
    /// 2. Spawn the dispatcher only after acquisition succeeds.
    /// 3. Release the lease at terminal completion regardless of the
    ///    dispatcher's [`ReleaseReason`] (Completed / Failed / Canceled
    ///    all clear the lease columns).
    ///
    /// Calling this method more than once replaces the prior wiring.
    /// Requests without a `run_id` are dispatched as before.
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
    /// Includes tasks that have already completed but are still sitting
    /// in the [`JoinSet`] waiting for [`Self::reap_completed`].
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
    /// `cancel` is the *parent* cancellation token — it propagates into
    /// every spawned dispatcher task so a SIGINT-style shutdown tears
    /// down in-flight sessions cooperatively. The pass itself does not
    /// await the spawned futures; call [`Self::reap_completed`] to
    /// observe results.
    pub async fn run_pending(&self, cancel: CancellationToken) -> SpecialistRunReport {
        // Combine prior-pass deferreds with newly drained requests.
        // Deferreds first so they are tried again before fresh work,
        // preserving age-order across passes.
        let mut work: Vec<SpecialistDispatchRequest> = {
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
            return SpecialistRunReport::default();
        }

        let snapshot = self.active_set.snapshot();
        let mut report = SpecialistRunReport::default();
        let mut new_deferred: Vec<SpecialistDispatchRequest> = Vec::new();

        for req in work {
            let Some(issue) = find_issue(&snapshot, &req.issue_id) else {
                debug!(
                    issue_id = %req.issue_id,
                    identifier = %req.identifier,
                    "specialist runner: issue no longer in active set; dropping request",
                );
                report.missing_issue += 1;
                continue;
            };

            // Non-blocking permit acquisition — if capacity is exhausted
            // we want to *park*, not wait. Waiting would block the
            // scheduler's tick fan against a long-running agent session.
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
                            req.identifier.clone(),
                            req.run_id,
                            &contended,
                        ));
                    debug!(
                        identifier = %req.identifier,
                        scope_kind = ?contended.scope.kind(),
                        scope_key = %contended.scope.key(),
                        in_flight = contended.in_flight,
                        cap = contended.cap,
                        "specialist runner: scope contended; parking",
                    );
                    new_deferred.push(req);
                    report.deferred_scope_contention += 1;
                    continue;
                }
            };

            // Cooperative cancellation observation (SPEC v2 §4.5).
            // Runs after scope acquisition but before lease acquisition
            // so an aborted dispatch never holds a lease, and so any
            // already-acquired scope permits release on the same path
            // the wind-down policy requires. The queue is consulted
            // briefly under its own lock; the cloned `CancelRequest`
            // payload then flows out so the sink call and queue drain
            // happen without holding the queue lock.
            if let Some(wiring) = self.cancellation.as_ref()
                && (req.run_id.is_some() || req.parent_work_item_id.is_some())
            {
                // Match the keyspace precedence in
                // [`observe_for_run_or_parent`]: run-keyed entries win
                // over the parent work-item entry. Inlined here so we
                // can skip the run-keyspace probe entirely when the
                // request has no `run_id` reservation, without the
                // primitive needing an `Option<RunRef>` overload.
                let observed: Option<CancelRequest> = {
                    let q = wiring.queue.lock().await;
                    let from_run = req.run_id.and_then(|run_id| {
                        match observe_for_run_or_parent(&q, run_id, None) {
                            CancellationDecision::Abort(req) => Some(req),
                            CancellationDecision::Proceed => None,
                        }
                    });
                    from_run.or_else(|| {
                        req.parent_work_item_id
                            .and_then(|wi| q.pending_for_work_item(wi).cloned())
                    })
                };
                if let Some(cancel_req) = observed {
                    drop(scope_permits);
                    drop(permit);
                    if let Some(run_id) = req.run_id
                        && let Err(err) = wiring.sink.mark_cancelled(run_id, &cancel_req)
                    {
                        warn!(
                            identifier = %req.identifier,
                            run_id = %run_id,
                            error = %err,
                            "specialist runner: cancellation status sink failed; relying on operator re-issue",
                        );
                    }
                    if let Some(run_id) = req.run_id {
                        let mut q = wiring.queue.lock().await;
                        q.drain_for_run(run_id);
                    }
                    debug!(
                        identifier = %req.identifier,
                        reason = %cancel_req.reason,
                        "specialist runner: pre-lease cancel observed; aborted dispatch",
                    );
                    report.cancelled_before_lease += 1;
                    report.cancellations.push(CancellationObserved {
                        identifier: req.identifier.clone(),
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
            let identifier = req.identifier.clone();
            let issue_for_dispatch = issue.clone();
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
                        identifier: identifier.clone(),
                        heartbeat: d.heartbeat.clone(),
                    },
                );
            }

            let mut inflight = self.inflight.lock().await;
            inflight.spawn(async move {
                let release_reason = dispatcher.dispatch(issue_for_dispatch, child_cancel).await;
                if let Some(LeasedDispatch { guard, .. }) = leased
                    && let Err(err) = guard.release().await
                {
                    warn!(
                        identifier = %identifier,
                        error = %err,
                        "specialist runner: lease release failed; relying on TTL reaper",
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
                drop(permit); // explicit: release capacity at task exit
                SpecialistDispatchOutcome {
                    identifier,
                    release_reason,
                }
            });
            drop(inflight);

            report.spawned += 1;
        }

        // Park leftovers for the next pass.
        if !new_deferred.is_empty() {
            let mut deferred = self.deferred.lock().await;
            // Prepend so the older deferreds keep priority. We rebuild
            // because Vec lacks `prepend`.
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
        bx.observe_mixed_pass(observations.iter().map(|obs| (obs.subject(), obs.fields())));
    }

    /// Drain every dispatcher task that has finished since the last
    /// call. Tasks still running are left in place. Panics from the
    /// dispatcher future are logged and dropped — the runner's
    /// composition root cannot meaningfully recover from a panic, but it
    /// must not poison the rest of the in-flight set.
    pub async fn reap_completed(&self) -> Vec<SpecialistDispatchOutcome> {
        let mut inflight = self.inflight.lock().await;
        let mut out = Vec::new();
        while let Some(joined) = inflight.try_join_next() {
            match joined {
                Ok(outcome) => out.push(outcome),
                Err(err) if err.is_cancelled() => {
                    debug!("specialist dispatch task cancelled before reap");
                }
                Err(err) => {
                    warn!(error = %err, "specialist dispatch task failed to join cleanly");
                }
            }
        }
        out
    }
}

/// Outcome of [`SpecialistRunner::try_acquire_lease`].
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

impl SpecialistRunner {
    /// Number of heartbeats currently registered against in-flight
    /// dispatches. Diagnostic accessor for tests/scheduler logs.
    pub async fn heartbeat_count(&self) -> usize {
        self.heartbeats.lock().await.len()
    }

    /// Drive one renewal pass over every in-flight heartbeat.
    ///
    /// For each registered `(run_id, identifier, heartbeat)` the runner
    /// checks [`LeaseHeartbeat::due`] against the configured wiring's
    /// clock and, when due, invokes [`pulse_observed`] to renew through
    /// the underlying [`RunLeaseStore::acquire`]. Heartbeats that are
    /// not yet due are skipped silently — the pass is idle for them.
    ///
    /// Each pulsed heartbeat produces a [`RunnerHeartbeatObservation`]
    /// in call order so the scheduler can:
    ///
    /// * log/forward [`HeartbeatPulseObservation::Renewed`] for
    ///   diagnostics,
    /// * route [`HeartbeatPulseObservation::Contended`] /
    ///   [`HeartbeatPulseObservation::NotFound`] to durable
    ///   run-status updates, and
    /// * surface [`HeartbeatPulseObservation::BackendError`] without
    ///   silently dropping the in-flight dispatch.
    ///
    /// Returns an empty vector when lease wiring is disabled. Holds no
    /// runner locks across `acquire` calls — the heartbeats map is
    /// snapshotted up front so terminal-release deregistration is not
    /// blocked by a slow backend.
    pub async fn pulse_heartbeats(&self) -> Vec<RunnerHeartbeatObservation> {
        let Some(wiring) = self.leasing.as_ref() else {
            return Vec::new();
        };
        let snapshot: Vec<(RunRef, String, Arc<Mutex<LeaseHeartbeat>>)> = {
            let map = self.heartbeats.lock().await;
            map.iter()
                .map(|(run_id, reg)| (*run_id, reg.identifier.clone(), reg.heartbeat.clone()))
                .collect()
        };

        let clock = wiring.clock.as_ref();
        let ttl_ms = wiring.config.default_ttl_ms;
        let mut out = Vec::new();
        for (run_id, identifier, hb_arc) in snapshot {
            let mut hb = hb_arc.lock().await;
            if !hb.due(clock) {
                continue;
            }
            let observation = pulse_observed(&mut hb, clock, ttl_ms).await;
            out.push(RunnerHeartbeatObservation {
                run_id,
                identifier,
                observation,
            });
        }
        out
    }

    /// Attempt to acquire scope permits for `req` against the configured
    /// [`ConcurrencyGate`]. Returns `Ok(None)` when no gate is wired,
    /// `Ok(Some(set))` when every scope had headroom, and `Err(_)` when
    /// any scope was at cap (no permits held — the inner gate already
    /// rolled back partial acquisitions).
    fn try_acquire_scopes(
        &self,
        req: &SpecialistDispatchRequest,
    ) -> Result<Option<ScopePermitSet>, ScopeContended> {
        let Some(gate) = &self.concurrency_gate else {
            return Ok(None);
        };
        let triple = DispatchTriple::new(
            req.role.as_str().to_owned(),
            req.agent_profile.clone(),
            req.repository.clone(),
        );
        RunnerScopes::new(gate.clone(), triple)
            .try_acquire()
            .map(Some)
    }

    async fn try_acquire_lease(&self, req: &SpecialistDispatchRequest) -> LeaseAttempt {
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
                    identifier = %req.identifier,
                    holder = %holder,
                    expires_at = %expires_at,
                    "specialist runner: lease contended; parking",
                );
                LeaseAttempt::Contended
            }
            Ok(Err(LeaseAcquireOutcome::NotFound)) => {
                warn!(
                    identifier = %req.identifier,
                    run_id = %run_id,
                    "specialist runner: durable run row missing; dropping request",
                );
                LeaseAttempt::NotFound
            }
            Ok(Err(LeaseAcquireOutcome::Acquired)) => {
                unreachable!("RunLeaseGuard::acquire returns Ok(Ok(_)) on Acquired",)
            }
            Err(err) => {
                warn!(
                    identifier = %req.identifier,
                    error = %err,
                    "specialist runner: lease store backend error; parking",
                );
                LeaseAttempt::BackendError
            }
        }
    }
}

/// Linear scan over the most-recent active-set snapshot. Active sets
/// are bounded to the orchestrator's working horizon (10s, low
/// hundreds), so this is cheaper than maintaining a parallel index.
fn find_issue<'a>(snapshot: &'a [Issue], issue_id: &str) -> Option<&'a Issue> {
    snapshot.iter().find(|i| i.id.as_str() == issue_id)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::role::RoleName;
    use async_trait::async_trait;
    use std::sync::Mutex as StdMutex;
    use std::time::Duration;

    fn issue(id: &str, identifier: &str) -> Issue {
        Issue::minimal(id, identifier, "title", "Todo")
    }

    fn req(id: &str, identifier: &str) -> SpecialistDispatchRequest {
        SpecialistDispatchRequest {
            issue_id: id.into(),
            identifier: identifier.into(),
            role: RoleName::new("specialist"),
            rule_index: None,
            run_id: None,
            agent_profile: None,
            repository: None,
            parent_work_item_id: None,
        }
    }

    fn req_with_run(id: &str, identifier: &str, run_id: i64) -> SpecialistDispatchRequest {
        SpecialistDispatchRequest {
            issue_id: id.into(),
            identifier: identifier.into(),
            role: RoleName::new("specialist"),
            rule_index: None,
            run_id: Some(crate::blocker::RunRef::new(run_id)),
            agent_profile: None,
            repository: None,
            parent_work_item_id: None,
        }
    }

    fn req_full(
        id: &str,
        identifier: &str,
        role: &str,
        agent_profile: Option<&str>,
        repository: Option<&str>,
    ) -> SpecialistDispatchRequest {
        SpecialistDispatchRequest {
            issue_id: id.into(),
            identifier: identifier.into(),
            role: RoleName::new(role),
            rule_index: None,
            run_id: None,
            agent_profile: agent_profile.map(str::to_owned),
            repository: repository.map(str::to_owned),
            parent_work_item_id: None,
        }
    }

    /// Records every dispatch invocation and resolves with a configured
    /// [`ReleaseReason`]. Optional gate lets tests hold a dispatch open
    /// to assert capacity behaviour.
    struct RecordingDispatcher {
        calls: Arc<StdMutex<Vec<String>>>,
        gate: Arc<tokio::sync::Notify>,
        gate_enabled: Arc<StdMutex<bool>>,
        reason: ReleaseReason,
    }

    impl RecordingDispatcher {
        fn new(reason: ReleaseReason) -> Self {
            Self {
                calls: Arc::new(StdMutex::new(Vec::new())),
                gate: Arc::new(tokio::sync::Notify::new()),
                gate_enabled: Arc::new(StdMutex::new(false)),
                reason,
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
    }

    #[async_trait]
    impl Dispatcher for RecordingDispatcher {
        async fn dispatch(&self, issue: Issue, cancel: CancellationToken) -> ReleaseReason {
            self.calls.lock().unwrap().push(issue.identifier.clone());
            // Optional gate so capacity tests can keep dispatchers
            // pending until released.
            let gated = *self.gate_enabled.lock().unwrap();
            if gated {
                tokio::select! {
                    _ = self.gate.notified() => {}
                    _ = cancel.cancelled() => return ReleaseReason::Canceled,
                }
            }
            self.reason
        }
    }

    fn store_with(issues: Vec<Issue>) -> Arc<ActiveSetStore> {
        let store = Arc::new(ActiveSetStore::new());
        // ActiveSetStore::replace is pub(crate); we are inside the
        // crate so this is valid.
        store.replace(issues);
        store
    }

    #[tokio::test]
    async fn run_pending_is_noop_when_queue_empty() {
        let queue = Arc::new(SpecialistDispatchQueue::new());
        let active = Arc::new(ActiveSetStore::new());
        let dispatcher = Arc::new(RecordingDispatcher::new(ReleaseReason::Completed));
        let runner = SpecialistRunner::new(queue, active, dispatcher.clone(), 4);

        let report = runner.run_pending(CancellationToken::new()).await;
        assert!(report.is_idle(), "expected idle report, got {report:?}");
        assert_eq!(runner.inflight_count().await, 0);
        assert!(dispatcher.calls().is_empty());
    }

    #[tokio::test]
    async fn run_pending_dispatches_known_issue() {
        let queue = Arc::new(SpecialistDispatchQueue::new());
        let active = store_with(vec![issue("id-1", "ENG-1")]);
        let dispatcher = Arc::new(RecordingDispatcher::new(ReleaseReason::Completed));
        let runner = SpecialistRunner::new(queue.clone(), active, dispatcher.clone(), 4);

        // Inject a request directly through the public crate-internal
        // helper used by the specialist tick.
        queue.enqueue(req("id-1", "ENG-1"));

        let report = runner.run_pending(CancellationToken::new()).await;
        assert_eq!(report.spawned, 1);
        assert_eq!(report.deferred_capacity, 0);
        assert_eq!(report.missing_issue, 0);

        // Wait for the spawned task to complete, then reap.
        let outcomes = wait_for_outcomes(&runner, 1).await;
        assert_eq!(outcomes.len(), 1);
        assert_eq!(outcomes[0].identifier, "ENG-1");
        assert_eq!(outcomes[0].release_reason, ReleaseReason::Completed);
        assert_eq!(dispatcher.calls(), vec!["ENG-1"]);
    }

    #[tokio::test]
    async fn run_pending_drops_request_when_issue_left_active_set() {
        let queue = Arc::new(SpecialistDispatchQueue::new());
        let active = Arc::new(ActiveSetStore::new()); // empty
        let dispatcher = Arc::new(RecordingDispatcher::new(ReleaseReason::Completed));
        let runner = SpecialistRunner::new(queue.clone(), active, dispatcher.clone(), 4);

        queue.enqueue(req("id-missing", "ENG-MISSING"));

        let report = runner.run_pending(CancellationToken::new()).await;
        assert_eq!(report.spawned, 0);
        assert_eq!(report.missing_issue, 1);
        assert_eq!(report.deferred_capacity, 0);
        assert_eq!(runner.inflight_count().await, 0);
        assert!(dispatcher.calls().is_empty());
    }

    #[tokio::test]
    async fn run_pending_defers_when_capacity_saturated() {
        let queue = Arc::new(SpecialistDispatchQueue::new());
        let active = store_with(vec![
            issue("id-1", "ENG-1"),
            issue("id-2", "ENG-2"),
            issue("id-3", "ENG-3"),
        ]);
        let dispatcher = Arc::new(RecordingDispatcher::new(ReleaseReason::Completed));
        // Hold the dispatcher so the first permit is not released.
        dispatcher.enable_gate();

        // Capacity of 1 — second & third requests must defer.
        let runner = SpecialistRunner::new(queue.clone(), active, dispatcher.clone(), 1);

        queue.enqueue(req("id-1", "ENG-1"));
        queue.enqueue(req("id-2", "ENG-2"));
        queue.enqueue(req("id-3", "ENG-3"));

        let report = runner.run_pending(CancellationToken::new()).await;
        assert_eq!(report.spawned, 1, "report = {report:?}");
        assert_eq!(report.deferred_capacity, 2);
        assert_eq!(report.missing_issue, 0);
        assert_eq!(runner.deferred_count().await, 2);

        // Release the gate so the running dispatcher completes; the
        // permit is freed and the next pass can spawn another.
        dispatcher.release_gate();
        let _ = wait_for_outcomes(&runner, 1).await;

        // With cap=1 each pass can only spawn one of the deferreds; we
        // expect to drain the parked queue across two more passes.
        let report = runner.run_pending(CancellationToken::new()).await;
        assert_eq!(report.spawned, 1, "report = {report:?}");
        assert_eq!(report.deferred_capacity, 1);
        let _ = wait_for_outcomes(&runner, 1).await;

        let report = runner.run_pending(CancellationToken::new()).await;
        assert_eq!(report.spawned, 1, "report = {report:?}");
        assert_eq!(report.deferred_capacity, 0);
        assert_eq!(runner.deferred_count().await, 0);
        let _ = wait_for_outcomes(&runner, 1).await;

        let mut all = dispatcher.calls();
        all.sort();
        assert_eq!(all, vec!["ENG-1", "ENG-2", "ENG-3"]);
    }

    #[tokio::test]
    async fn run_pending_preserves_age_order_for_deferred_requests() {
        let queue = Arc::new(SpecialistDispatchQueue::new());
        let active = store_with(vec![
            issue("id-1", "ENG-1"),
            issue("id-2", "ENG-2"),
            issue("id-3", "ENG-3"),
        ]);
        let dispatcher = Arc::new(RecordingDispatcher::new(ReleaseReason::Completed));
        dispatcher.enable_gate();

        // Cap = 0 forces every request into the deferred queue.
        // After we raise capacity by destroying this runner and
        // building a new one with the deferreds, we can verify order
        // by spawning sequentially with cap = 1 + an unblocked dispatcher.
        let runner = SpecialistRunner::new(queue.clone(), active.clone(), dispatcher.clone(), 0);
        queue.enqueue(req("id-1", "ENG-1"));
        queue.enqueue(req("id-2", "ENG-2"));
        let r1 = runner.run_pending(CancellationToken::new()).await;
        assert_eq!(r1.deferred_capacity, 2);

        // Add a third fresh request: it must go *after* the two
        // deferreds when the next pass walks the combined queue.
        queue.enqueue(req("id-3", "ENG-3"));
        let r2 = runner.run_pending(CancellationToken::new()).await;
        assert_eq!(r2.deferred_capacity, 3);
        assert_eq!(runner.deferred_count().await, 3);

        // Drain order test: rebuild a permit-bearing runner sharing
        // state would be wrong — instead, capacity=0 means every pass
        // re-defers. We verify ordering by inspecting the deferred
        // queue contents directly.
        let parked = runner.deferred.lock().await;
        let parked_ids: Vec<&str> = parked.iter().map(|r| r.identifier.as_str()).collect();
        assert_eq!(
            parked_ids,
            vec!["ENG-1", "ENG-2", "ENG-3"],
            "deferreds must keep age-order: prior parks first, fresh request last",
        );
    }

    #[tokio::test]
    async fn run_pending_propagates_parent_cancellation_to_dispatch() {
        let queue = Arc::new(SpecialistDispatchQueue::new());
        let active = store_with(vec![issue("id-1", "ENG-1")]);
        let dispatcher = Arc::new(RecordingDispatcher::new(ReleaseReason::Completed));
        dispatcher.enable_gate(); // hold the dispatch open

        let runner = SpecialistRunner::new(queue.clone(), active, dispatcher.clone(), 4);
        queue.enqueue(req("id-1", "ENG-1"));

        let cancel = CancellationToken::new();
        let _ = runner.run_pending(cancel.clone()).await;
        assert_eq!(runner.inflight_count().await, 1);

        // Fire the parent cancel: the spawned dispatcher's child token
        // is derived from it and must observe cancellation.
        cancel.cancel();

        let outcomes = wait_for_outcomes(&runner, 1).await;
        assert_eq!(outcomes.len(), 1);
        assert_eq!(outcomes[0].release_reason, ReleaseReason::Canceled);
    }

    #[tokio::test]
    async fn capacity_zero_defers_everything() {
        let queue = Arc::new(SpecialistDispatchQueue::new());
        let active = store_with(vec![issue("id-1", "ENG-1")]);
        let dispatcher = Arc::new(RecordingDispatcher::new(ReleaseReason::Completed));
        let runner = SpecialistRunner::new(queue.clone(), active, dispatcher.clone(), 0);
        queue.enqueue(req("id-1", "ENG-1"));

        let report = runner.run_pending(CancellationToken::new()).await;
        assert_eq!(report.spawned, 0);
        assert_eq!(report.deferred_capacity, 1);
        assert!(dispatcher.calls().is_empty());
    }

    #[tokio::test]
    async fn reap_completed_returns_outcomes_in_completion_order() {
        let queue = Arc::new(SpecialistDispatchQueue::new());
        let active = store_with(vec![issue("id-1", "ENG-1"), issue("id-2", "ENG-2")]);
        let dispatcher = Arc::new(RecordingDispatcher::new(ReleaseReason::Completed));
        let runner = SpecialistRunner::new(queue.clone(), active, dispatcher.clone(), 4);

        queue.enqueue(req("id-1", "ENG-1"));
        queue.enqueue(req("id-2", "ENG-2"));
        let report = runner.run_pending(CancellationToken::new()).await;
        assert_eq!(report.spawned, 2);

        let outcomes = wait_for_outcomes(&runner, 2).await;
        let mut ids: Vec<String> = outcomes.iter().map(|o| o.identifier.clone()).collect();
        ids.sort();
        assert_eq!(ids, vec!["ENG-1".to_string(), "ENG-2".to_string()]);
    }

    /// Wait until at least `n` outcomes have been reaped or a generous
    /// per-test timeout fires. Tests don't sleep on cadence — they
    /// poll the JoinSet via `reap_completed`, which is lock-free apart
    /// from the small async mutex.
    async fn wait_for_outcomes(
        runner: &SpecialistRunner,
        n: usize,
    ) -> Vec<SpecialistDispatchOutcome> {
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

    // --- Lease wiring (CHECKLIST_v2 Phase 11) -----------------------

    use crate::blocker::RunRef;
    use crate::lease::{FixedLeaseClock, LeaseConfig, LeaseOwner};
    use crate::run_lease::{InMemoryRunLeaseStore, RunLeaseStore};

    fn lease_owner() -> LeaseOwner {
        LeaseOwner::new("test-host", "scheduler-test", 0).unwrap()
    }

    fn build_lease_runner(
        max_concurrent: usize,
        dispatcher: Arc<RecordingDispatcher>,
        active: Arc<ActiveSetStore>,
        store: Arc<InMemoryRunLeaseStore>,
        clock_now: &str,
    ) -> (Arc<SpecialistDispatchQueue>, SpecialistRunner) {
        let queue = Arc::new(SpecialistDispatchQueue::new());
        let runner = SpecialistRunner::new(queue.clone(), active, dispatcher, max_concurrent)
            .with_leasing(
                store,
                lease_owner(),
                LeaseConfig::default(),
                Arc::new(FixedLeaseClock::new(clock_now)),
            );
        (queue, runner)
    }

    #[tokio::test]
    async fn lease_acquired_during_dispatch_and_released_on_completed() {
        let active = store_with(vec![issue("id-1", "ENG-1")]);
        let dispatcher = Arc::new(RecordingDispatcher::new(ReleaseReason::Completed));
        dispatcher.enable_gate(); // hold dispatch open so we can observe the in-flight lease
        let store = Arc::new(InMemoryRunLeaseStore::new());
        store.register_run(RunRef::new(42)).await;

        let (queue, runner) =
            build_lease_runner(4, dispatcher.clone(), active, store.clone(), "T0");
        queue.enqueue(req_with_run("id-1", "ENG-1", 42));

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
        // expires_at honored ttl_ms with the kernel's default config.
        assert!(
            snap.1.starts_with("T0+"),
            "expires_at should derive from FixedLeaseClock 'now'; got {}",
            snap.1,
        );

        // Release the gate so the dispatcher returns; the spawned task
        // releases the lease as part of terminal completion.
        dispatcher.release_gate();
        let outcomes = wait_for_outcomes(&runner, 1).await;
        assert_eq!(outcomes[0].release_reason, ReleaseReason::Completed);
        assert!(
            store.snapshot(RunRef::new(42)).await.is_none(),
            "lease must be cleared on Completed terminal release",
        );
        assert_eq!(store.release_calls().await, vec![RunRef::new(42)]);
    }

    #[tokio::test]
    async fn lease_cleared_on_failed_terminal_release() {
        let active = store_with(vec![issue("id-1", "ENG-1")]);
        // `Terminal` covers the "failed/exhausted" branch in the
        // kernel's ReleaseReason taxonomy.
        let dispatcher = Arc::new(RecordingDispatcher::new(ReleaseReason::Terminal));
        let store = Arc::new(InMemoryRunLeaseStore::new());
        store.register_run(RunRef::new(7)).await;

        let (queue, runner) =
            build_lease_runner(4, dispatcher.clone(), active, store.clone(), "T1");
        queue.enqueue(req_with_run("id-1", "ENG-1", 7));

        let report = runner.run_pending(CancellationToken::new()).await;
        assert_eq!(report.spawned, 1);
        let outcomes = wait_for_outcomes(&runner, 1).await;
        assert_eq!(outcomes[0].release_reason, ReleaseReason::Terminal);
        assert!(store.snapshot(RunRef::new(7)).await.is_none());
    }

    #[tokio::test]
    async fn lease_cleared_on_canceled_terminal_release() {
        let active = store_with(vec![issue("id-1", "ENG-1")]);
        let dispatcher = Arc::new(RecordingDispatcher::new(ReleaseReason::Completed));
        dispatcher.enable_gate();
        let store = Arc::new(InMemoryRunLeaseStore::new());
        store.register_run(RunRef::new(11)).await;

        let (queue, runner) =
            build_lease_runner(4, dispatcher.clone(), active, store.clone(), "T2");
        queue.enqueue(req_with_run("id-1", "ENG-1", 11));

        let cancel = CancellationToken::new();
        let _ = runner.run_pending(cancel.clone()).await;
        assert!(store.snapshot(RunRef::new(11)).await.is_some());
        cancel.cancel();
        let outcomes = wait_for_outcomes(&runner, 1).await;
        assert_eq!(outcomes[0].release_reason, ReleaseReason::Canceled);
        assert!(store.snapshot(RunRef::new(11)).await.is_none());
    }

    #[tokio::test]
    async fn lease_visible_to_expired_scan_only_after_ttl() {
        let active = store_with(vec![issue("id-1", "ENG-1")]);
        let dispatcher = Arc::new(RecordingDispatcher::new(ReleaseReason::Completed));
        dispatcher.enable_gate();
        let store = Arc::new(InMemoryRunLeaseStore::new());
        store.register_run(RunRef::new(99)).await;

        let (queue, runner) =
            build_lease_runner(4, dispatcher.clone(), active, store.clone(), "T3");
        queue.enqueue(req_with_run("id-1", "ENG-1", 99));

        let _ = runner.run_pending(CancellationToken::new()).await;
        let snap = store.snapshot(RunRef::new(99)).await.unwrap();
        // A second owner attempting acquisition with a `now` *before*
        // expires_at must observe contention (lease has not expired).
        let other_owner = LeaseOwner::new("other-host", "scheduler-test", 1).unwrap();
        let outcome_before_ttl = store
            .acquire(RunRef::new(99), &other_owner, "T3+later", "T3")
            .await
            .unwrap();
        match outcome_before_ttl {
            crate::run_lease::LeaseAcquireOutcome::Contended { holder, .. } => {
                assert_eq!(holder, lease_owner().to_string());
            }
            other => panic!("expected Contended pre-TTL, got {other:?}"),
        }

        // After advancing `now` past the kernel-issued expires_at the
        // expired-takeover path succeeds.
        let after_ttl = format!("{}+999999999999ms", "T3"); // strictly greater than snap.1
        assert!(after_ttl > snap.1);
        let outcome_after_ttl = store
            .acquire(RunRef::new(99), &other_owner, "T9+later", &after_ttl)
            .await
            .unwrap();
        assert_eq!(
            outcome_after_ttl,
            crate::run_lease::LeaseAcquireOutcome::Acquired,
        );

        // Cleanup: release the gate so the spawned task can finish.
        dispatcher.release_gate();
        let _ = wait_for_outcomes(&runner, 1).await;
    }

    #[tokio::test]
    async fn contended_lease_parks_request_and_does_not_consume_capacity() {
        let active = store_with(vec![issue("id-1", "ENG-1"), issue("id-2", "ENG-2")]);
        let dispatcher = Arc::new(RecordingDispatcher::new(ReleaseReason::Completed));
        let store = Arc::new(InMemoryRunLeaseStore::new());
        store.register_run(RunRef::new(1)).await;
        store.register_run(RunRef::new(2)).await;

        // Pre-claim run 1 by another owner so the runner sees contention.
        let other = LeaseOwner::new("other-host", "scheduler-test", 9).unwrap();
        let _ = store
            .acquire(RunRef::new(1), &other, "T0+999ms", "T0")
            .await
            .unwrap();

        let (queue, runner) =
            build_lease_runner(2, dispatcher.clone(), active, store.clone(), "T0");
        queue.enqueue(req_with_run("id-1", "ENG-1", 1));
        queue.enqueue(req_with_run("id-2", "ENG-2", 2));

        let report = runner.run_pending(CancellationToken::new()).await;
        assert_eq!(report.spawned, 1);
        assert_eq!(report.deferred_lease_contention, 1);
        // Request 1 parked; request 2 dispatched.
        assert_eq!(runner.deferred_count().await, 1);
        let _ = wait_for_outcomes(&runner, 1).await;
    }

    #[tokio::test]
    async fn missing_run_drops_request_without_dispatch() {
        let active = store_with(vec![issue("id-1", "ENG-1")]);
        let dispatcher = Arc::new(RecordingDispatcher::new(ReleaseReason::Completed));
        let store = Arc::new(InMemoryRunLeaseStore::new()); // run not registered

        let (queue, runner) = build_lease_runner(4, dispatcher.clone(), active, store, "T0");
        queue.enqueue(req_with_run("id-1", "ENG-1", 404));

        let report = runner.run_pending(CancellationToken::new()).await;
        assert_eq!(report.spawned, 0);
        assert_eq!(report.missing_run, 1);
        assert!(dispatcher.calls().is_empty());
    }

    #[tokio::test]
    async fn request_without_run_id_dispatches_without_lease_acquisition() {
        let active = store_with(vec![issue("id-1", "ENG-1")]);
        let dispatcher = Arc::new(RecordingDispatcher::new(ReleaseReason::Completed));
        let store = Arc::new(InMemoryRunLeaseStore::new());

        let (queue, runner) =
            build_lease_runner(4, dispatcher.clone(), active, store.clone(), "T0");
        // No run_id on the request; lease wiring should be skipped.
        queue.enqueue(req("id-1", "ENG-1"));

        let report = runner.run_pending(CancellationToken::new()).await;
        assert_eq!(report.spawned, 1);
        let outcomes = wait_for_outcomes(&runner, 1).await;
        assert_eq!(outcomes[0].release_reason, ReleaseReason::Completed);
        // Store untouched.
        assert!(store.release_calls().await.is_empty());
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
        active: Arc<ActiveSetStore>,
        store: Arc<InMemoryRunLeaseStore>,
        clock: Arc<AdvanceableLeaseClock>,
    ) -> (Arc<SpecialistDispatchQueue>, SpecialistRunner) {
        let queue = Arc::new(SpecialistDispatchQueue::new());
        let runner = SpecialistRunner::new(queue.clone(), active, dispatcher, max_concurrent)
            .with_leasing(store, lease_owner(), LeaseConfig::default(), clock);
        (queue, runner)
    }

    #[tokio::test]
    async fn pulse_heartbeats_advances_lease_expiry_during_long_running_dispatch() {
        let active = store_with(vec![issue("id-1", "ENG-1")]);
        let dispatcher = Arc::new(RecordingDispatcher::new(ReleaseReason::Completed));
        dispatcher.enable_gate(); // hold dispatch open across pulses
        let store = Arc::new(InMemoryRunLeaseStore::new());
        store.register_run(RunRef::new(42)).await;
        let clock = Arc::new(AdvanceableLeaseClock::new("T0"));

        let (queue, runner) = build_lease_runner_with_clock(
            4,
            dispatcher.clone(),
            active,
            store.clone(),
            clock.clone(),
        );
        queue.enqueue(req_with_run("id-1", "ENG-1", 42));

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
        assert_eq!(
            after_idle, initial,
            "store expiry must not change while heartbeat is idle",
        );

        // Advance the clock so the heartbeat is due, then pulse.
        clock.set("T9");
        let observations = runner.pulse_heartbeats().await;
        assert_eq!(observations.len(), 1);
        let obs = &observations[0];
        assert_eq!(obs.run_id, RunRef::new(42));
        assert_eq!(obs.identifier, "ENG-1");
        assert_eq!(obs.observation, HeartbeatPulseObservation::Renewed);

        let after_renewal = store.snapshot(RunRef::new(42)).await.unwrap().1;
        assert!(
            after_renewal > initial,
            "lease_expires_at must advance after a successful pulse: {after_renewal} > {initial}",
        );

        // Release the gate so the dispatch completes; lease must be
        // cleared exactly once on terminal release, and the heartbeat
        // registry must drain.
        dispatcher.release_gate();
        let outcomes = wait_for_outcomes(&runner, 1).await;
        assert_eq!(outcomes[0].release_reason, ReleaseReason::Completed);
        assert!(store.snapshot(RunRef::new(42)).await.is_none());
        assert_eq!(store.release_calls().await, vec![RunRef::new(42)]);
        assert_eq!(runner.heartbeat_count().await, 0);
    }

    #[tokio::test]
    async fn pulse_heartbeats_surfaces_contended_observation() {
        let active = store_with(vec![issue("id-1", "ENG-1")]);
        let dispatcher = Arc::new(RecordingDispatcher::new(ReleaseReason::Completed));
        dispatcher.enable_gate();
        let store = Arc::new(InMemoryRunLeaseStore::new());
        store.register_run(RunRef::new(42)).await;
        let clock = Arc::new(AdvanceableLeaseClock::new("T0"));

        let (queue, runner) = build_lease_runner_with_clock(
            4,
            dispatcher.clone(),
            active,
            store.clone(),
            clock.clone(),
        );
        queue.enqueue(req_with_run("id-1", "ENG-1", 42));

        let _ = runner.run_pending(CancellationToken::new()).await;
        assert_eq!(runner.heartbeat_count().await, 1);

        // A different owner takes over the lease with a far-future
        // expiry — the heartbeat's pulse must surface Contended.
        let intruder = LeaseOwner::new("other-host", "scheduler-test", 9).unwrap();
        store.release(RunRef::new(42)).await.unwrap();
        let outcome = store
            .acquire(RunRef::new(42), &intruder, "Z9999+999999999999ms", "Z9999")
            .await
            .unwrap();
        assert_eq!(outcome, LeaseAcquireOutcome::Acquired);

        clock.set("T9"); // ensure renewal-due
        let observations = runner.pulse_heartbeats().await;
        assert_eq!(observations.len(), 1);
        match &observations[0].observation {
            HeartbeatPulseObservation::Contended { holder, .. } => {
                assert_eq!(holder, &intruder.to_string());
            }
            other => panic!("expected Contended, got {other:?}"),
        }
        assert_eq!(observations[0].run_id, RunRef::new(42));
        assert_eq!(observations[0].identifier, "ENG-1");

        // Cleanup so the spawned dispatch can finish.
        dispatcher.release_gate();
        let _ = wait_for_outcomes(&runner, 1).await;
        // Heartbeat registry drains regardless of pulse outcomes.
        assert_eq!(runner.heartbeat_count().await, 0);
    }

    #[tokio::test]
    async fn pulse_heartbeats_surfaces_not_found_observation() {
        let active = store_with(vec![issue("id-1", "ENG-1")]);
        let dispatcher = Arc::new(RecordingDispatcher::new(ReleaseReason::Completed));
        dispatcher.enable_gate();
        let store = Arc::new(InMemoryRunLeaseStore::new());
        store.register_run(RunRef::new(42)).await;
        let clock = Arc::new(AdvanceableLeaseClock::new("T0"));

        let (queue, runner) = build_lease_runner_with_clock(
            4,
            dispatcher.clone(),
            active,
            store.clone(),
            clock.clone(),
        );
        queue.enqueue(req_with_run("id-1", "ENG-1", 42));
        let _ = runner.run_pending(CancellationToken::new()).await;

        // Forcibly drop the run row from the store: the heartbeat's
        // re-acquire must observe NotFound (rather than silently
        // succeeding or panicking).
        store.forget_run(RunRef::new(42)).await;

        clock.set("T9");
        let observations = runner.pulse_heartbeats().await;
        assert_eq!(observations.len(), 1);
        assert_eq!(
            observations[0].observation,
            HeartbeatPulseObservation::NotFound
        );
        assert_eq!(observations[0].run_id, RunRef::new(42));

        dispatcher.release_gate();
        let _ = wait_for_outcomes(&runner, 1).await;
    }

    #[tokio::test]
    async fn pulse_heartbeats_surfaces_backend_error_observation() {
        let active = store_with(vec![issue("id-1", "ENG-1")]);
        let dispatcher = Arc::new(RecordingDispatcher::new(ReleaseReason::Completed));
        dispatcher.enable_gate();
        let store = Arc::new(InMemoryRunLeaseStore::new());
        store.register_run(RunRef::new(42)).await;
        let clock = Arc::new(AdvanceableLeaseClock::new("T0"));

        let (queue, runner) = build_lease_runner_with_clock(
            4,
            dispatcher.clone(),
            active,
            store.clone(),
            clock.clone(),
        );
        queue.enqueue(req_with_run("id-1", "ENG-1", 42));
        let _ = runner.run_pending(CancellationToken::new()).await;

        // Force the next acquire to error: the pulse must fold the
        // backend error into a typed observation rather than panicking
        // or silently dropping the in-flight dispatch.
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
        let queue = Arc::new(SpecialistDispatchQueue::new());
        let active = store_with(vec![issue("id-1", "ENG-1")]);
        let dispatcher = Arc::new(RecordingDispatcher::new(ReleaseReason::Completed));
        let runner = SpecialistRunner::new(queue, active, dispatcher, 4);
        let observations = runner.pulse_heartbeats().await;
        assert!(observations.is_empty());
        assert_eq!(runner.heartbeat_count().await, 0);
    }

    #[tokio::test]
    async fn terminal_release_clears_lease_exactly_once_after_pulses() {
        let active = store_with(vec![issue("id-1", "ENG-1")]);
        let dispatcher = Arc::new(RecordingDispatcher::new(ReleaseReason::Completed));
        dispatcher.enable_gate();
        let store = Arc::new(InMemoryRunLeaseStore::new());
        store.register_run(RunRef::new(42)).await;
        let clock = Arc::new(AdvanceableLeaseClock::new("T0"));

        let (queue, runner) = build_lease_runner_with_clock(
            4,
            dispatcher.clone(),
            active,
            store.clone(),
            clock.clone(),
        );
        queue.enqueue(req_with_run("id-1", "ENG-1", 42));
        let _ = runner.run_pending(CancellationToken::new()).await;

        // Pulse twice during dispatch — both should renew.
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
        assert_eq!(outcomes[0].release_reason, ReleaseReason::Completed);

        // Exactly one terminal release call recorded — pulses re-acquire
        // through `acquire`, never `release`.
        assert_eq!(store.release_calls().await, vec![RunRef::new(42)]);
        assert!(store.snapshot(RunRef::new(42)).await.is_none());
        assert_eq!(runner.heartbeat_count().await, 0);
    }

    #[tokio::test]
    async fn backend_error_parks_request_for_retry() {
        let active = store_with(vec![issue("id-1", "ENG-1")]);
        let dispatcher = Arc::new(RecordingDispatcher::new(ReleaseReason::Completed));
        let store = Arc::new(InMemoryRunLeaseStore::new());
        store.register_run(RunRef::new(1)).await;
        store.fail_next("disk full").await;

        let (queue, runner) = build_lease_runner(4, dispatcher.clone(), active, store, "T0");
        queue.enqueue(req_with_run("id-1", "ENG-1", 1));

        let report = runner.run_pending(CancellationToken::new()).await;
        assert_eq!(report.spawned, 0);
        assert_eq!(report.lease_backend_error, 1);
        assert_eq!(runner.deferred_count().await, 1);
        assert!(dispatcher.calls().is_empty());
    }

    // ---- Concurrency-gate wiring (CHECKLIST_v2 Phase 11) -----------

    use crate::concurrency_gate::{ConcurrencyGate, Scope};

    #[tokio::test]
    async fn scope_role_cap_serializes_dispatches_under_global_capacity() {
        // Role cap = 1 must hold exactly one dispatch in flight even
        // though local capacity allows more. The deferred request must
        // surface a typed contention observation against the Role scope.
        let queue = Arc::new(SpecialistDispatchQueue::new());
        let active = store_with(vec![
            issue("id-1", "ENG-1"),
            issue("id-2", "ENG-2"),
            issue("id-3", "ENG-3"),
        ]);
        let dispatcher = Arc::new(RecordingDispatcher::new(ReleaseReason::Completed));
        dispatcher.enable_gate();

        let gate = ConcurrencyGate::new();
        gate.set_cap(Scope::Role("specialist".into()), 1);

        let runner = SpecialistRunner::new(queue.clone(), active, dispatcher.clone(), 4)
            .with_concurrency_gate(gate.clone());

        queue.enqueue(req_full("id-1", "ENG-1", "specialist", None, None));
        queue.enqueue(req_full("id-2", "ENG-2", "specialist", None, None));
        queue.enqueue(req_full("id-3", "ENG-3", "specialist", None, None));

        let report = runner.run_pending(CancellationToken::new()).await;
        assert_eq!(report.spawned, 1, "report = {report:?}");
        assert_eq!(report.deferred_scope_contention, 2);
        assert_eq!(report.scope_contentions.len(), 2);

        for obs in &report.scope_contentions {
            assert_eq!(obs.scope_kind, ScopeKind::Role);
            assert_eq!(obs.scope_key, "specialist");
            assert_eq!(obs.cap, 1);
            assert_eq!(obs.in_flight, 1);
        }
        let parked_ids: Vec<&str> = report
            .scope_contentions
            .iter()
            .map(|o| o.identifier.as_str())
            .collect();
        assert_eq!(parked_ids, vec!["ENG-2", "ENG-3"]);

        // Local capacity is unaffected: the runner did not consume permits
        // for the parked dispatches.
        assert_eq!(runner.inflight_count().await, 1);
        assert_eq!(runner.deferred_count().await, 2);

        // Re-gate so the next dispatch holds the Role permit again.
        dispatcher.enable_gate();
        // Release the held dispatch; once the spawned task exits the
        // Role permit drops, freeing the scope for one more dispatch.
        dispatcher.release_gate();
        let _ = wait_for_outcomes(&runner, 1).await;
        dispatcher.enable_gate();

        // Next pass: one parked dispatch acquires the freed Role permit
        // and spawns; the second re-defers because cap=1.
        let report = runner.run_pending(CancellationToken::new()).await;
        assert_eq!(report.spawned, 1, "report = {report:?}");
        assert_eq!(report.deferred_scope_contention, 1);
        assert_eq!(report.scope_contentions[0].scope_kind, ScopeKind::Role);

        dispatcher.release_gate();
        let _ = wait_for_outcomes(&runner, 1).await;
    }

    #[tokio::test]
    async fn scope_global_cap_releases_permit_exactly_once_on_terminal_completion() {
        let queue = Arc::new(SpecialistDispatchQueue::new());
        let active = store_with(vec![issue("id-1", "ENG-1"), issue("id-2", "ENG-2")]);
        let dispatcher = Arc::new(RecordingDispatcher::new(ReleaseReason::Completed));

        let gate = ConcurrencyGate::new();
        gate.set_cap(Scope::Global, 1);

        let runner = SpecialistRunner::new(queue.clone(), active, dispatcher.clone(), 4)
            .with_concurrency_gate(gate.clone());

        // First dispatch holds the only Global permit until completion.
        queue.enqueue(req_full("id-1", "ENG-1", "specialist", None, None));
        let _ = runner.run_pending(CancellationToken::new()).await;
        let _ = wait_for_outcomes(&runner, 1).await;

        // Permit must be released after terminal completion — the next
        // dispatch on the same Global cap = 1 must succeed.
        queue.enqueue(req_full("id-2", "ENG-2", "specialist", None, None));
        let report = runner.run_pending(CancellationToken::new()).await;
        assert_eq!(report.spawned, 1);
        assert!(report.scope_contentions.is_empty());
        let _ = wait_for_outcomes(&runner, 1).await;

        match gate.available(&Scope::Global) {
            crate::concurrency_gate::ScopeAvailability::Bounded {
                cap,
                in_flight,
                available,
            } => {
                assert_eq!(cap, 1);
                assert_eq!(in_flight, 0, "Global permit must drop on completion");
                assert_eq!(available, 1);
            }
            other => panic!("expected Bounded(Global), got {other:?}"),
        }
    }

    #[tokio::test]
    async fn scope_repository_cap_defers_concurrent_dispatch_against_same_repo() {
        let queue = Arc::new(SpecialistDispatchQueue::new());
        let active = store_with(vec![issue("id-1", "ENG-1"), issue("id-2", "ENG-2")]);
        let dispatcher = Arc::new(RecordingDispatcher::new(ReleaseReason::Completed));
        dispatcher.enable_gate();

        let gate = ConcurrencyGate::new();
        gate.set_cap(Scope::Repository("acme/widgets".into()), 1);

        let runner = SpecialistRunner::new(queue.clone(), active, dispatcher.clone(), 4)
            .with_concurrency_gate(gate);

        queue.enqueue(req_full(
            "id-1",
            "ENG-1",
            "specialist",
            None,
            Some("acme/widgets"),
        ));
        queue.enqueue(req_full(
            "id-2",
            "ENG-2",
            "specialist",
            None,
            Some("acme/widgets"),
        ));

        let report = runner.run_pending(CancellationToken::new()).await;
        assert_eq!(report.spawned, 1);
        assert_eq!(report.deferred_scope_contention, 1);
        let obs = &report.scope_contentions[0];
        assert_eq!(obs.scope_kind, ScopeKind::Repository);
        assert_eq!(obs.scope_key, "acme/widgets");
        assert_eq!(obs.identifier, "ENG-2");

        dispatcher.release_gate();
        let _ = wait_for_outcomes(&runner, 1).await;
    }

    #[tokio::test]
    async fn scope_contention_does_not_consume_local_capacity() {
        // Capacity 1 + role cap 0 means scope rejects every dispatch,
        // but the local permit must NOT be consumed: the next pass over
        // a different (cap-free) role can still spawn.
        let queue = Arc::new(SpecialistDispatchQueue::new());
        let active = store_with(vec![issue("id-1", "ENG-1"), issue("id-2", "ENG-2")]);
        let dispatcher = Arc::new(RecordingDispatcher::new(ReleaseReason::Completed));

        let gate = ConcurrencyGate::new();
        gate.set_cap(Scope::Role("frozen".into()), 0);

        let runner = SpecialistRunner::new(queue.clone(), active, dispatcher.clone(), 1)
            .with_concurrency_gate(gate);

        queue.enqueue(req_full("id-1", "ENG-1", "frozen", None, None));
        queue.enqueue(req_full("id-2", "ENG-2", "specialist", None, None));

        let report = runner.run_pending(CancellationToken::new()).await;
        assert_eq!(report.spawned, 1, "second request should spawn: {report:?}");
        assert_eq!(report.deferred_scope_contention, 1);
        assert_eq!(report.scope_contentions.len(), 1);
        assert_eq!(report.scope_contentions[0].identifier, "ENG-1");
        assert_eq!(report.scope_contentions[0].scope_kind, ScopeKind::Role);
        assert_eq!(report.scope_contentions[0].scope_key, "frozen");
        assert_eq!(report.scope_contentions[0].cap, 0);

        let _ = wait_for_outcomes(&runner, 1).await;
        assert_eq!(dispatcher.calls(), vec!["ENG-2"]);
    }

    #[tokio::test]
    async fn no_concurrency_gate_means_no_scope_observations() {
        let queue = Arc::new(SpecialistDispatchQueue::new());
        let active = store_with(vec![issue("id-1", "ENG-1")]);
        let dispatcher = Arc::new(RecordingDispatcher::new(ReleaseReason::Completed));
        let runner = SpecialistRunner::new(queue.clone(), active, dispatcher.clone(), 4);
        queue.enqueue(req_full(
            "id-1",
            "ENG-1",
            "specialist",
            Some("claude"),
            Some("acme/widgets"),
        ));
        let report = runner.run_pending(CancellationToken::new()).await;
        assert_eq!(report.spawned, 1);
        assert!(report.scope_contentions.is_empty());
        assert_eq!(report.deferred_scope_contention, 0);
        let _ = wait_for_outcomes(&runner, 1).await;
    }

    // ---- Scope-contention broadcaster wiring (CHECKLIST_v2 Phase 11
    //      §195 specialist subtask) -------------------------------------

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

    fn req_full_with_run(
        id: &str,
        identifier: &str,
        role: &str,
        run_id: i64,
    ) -> SpecialistDispatchRequest {
        SpecialistDispatchRequest {
            issue_id: id.into(),
            identifier: identifier.into(),
            role: RoleName::new(role),
            rule_index: None,
            run_id: Some(crate::blocker::RunRef::new(run_id)),
            agent_profile: None,
            repository: None,
            parent_work_item_id: None,
        }
    }

    /// N consecutive contended passes for the same dispatch produce
    /// exactly one durable `ScopeCapReached` event.
    #[tokio::test]
    async fn broadcaster_dedups_repeated_contention_to_one_event_per_episode() {
        let queue = Arc::new(SpecialistDispatchQueue::new());
        let active = store_with(vec![issue("id-1", "ENG-1"), issue("id-2", "ENG-2")]);
        let dispatcher = Arc::new(RecordingDispatcher::new(ReleaseReason::Completed));
        dispatcher.enable_gate();

        let gate = ConcurrencyGate::new();
        gate.set_cap(Scope::Role("specialist".into()), 1);

        let bus = EventBus::default();
        let sink = RecordingSink::new();
        let broadcaster = Arc::new(ScopeContentionEventBroadcaster::new(
            bus.clone(),
            Some(sink.clone() as Arc<dyn ScopeContentionEventSink>),
        ));

        let runner = SpecialistRunner::new(queue.clone(), active, dispatcher.clone(), 4)
            .with_concurrency_gate(gate)
            .with_scope_contention_broadcaster(broadcaster.clone());

        // ENG-1 spawns; ENG-2 contends on the Role scope every pass.
        queue.enqueue(req_full("id-1", "ENG-1", "specialist", None, None));
        queue.enqueue(req_full("id-2", "ENG-2", "specialist", None, None));

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
                assert!(run_id.is_none(), "ENG-2 had no run_id reservation");
                assert_eq!(identifier.as_deref(), Some("ENG-2"));
                assert_eq!(*scope_kind, ScopeKind::Role);
                assert_eq!(scope_key, "specialist");
            }
            other => panic!("unexpected event: {other:?}"),
        }

        dispatcher.release_gate();
        // Drain the in-flight dispatch first, then run another pass so
        // the deferred request (which never spawned because the gate
        // contended) gets picked up before reaping.
        let _ = wait_for_outcomes(&runner, 1).await;
        let _ = runner.run_pending(CancellationToken::new()).await;
        let _ = wait_for_outcomes(&runner, 1).await;
    }

    /// `run_id` is populated on the durable event when the request had
    /// reserved a run row; absent otherwise.
    #[tokio::test]
    async fn broadcaster_reports_run_id_when_request_carries_one() {
        let queue = Arc::new(SpecialistDispatchQueue::new());
        let active = store_with(vec![issue("id-1", "ENG-1"), issue("id-2", "ENG-2")]);
        let dispatcher = Arc::new(RecordingDispatcher::new(ReleaseReason::Completed));
        dispatcher.enable_gate();

        let gate = ConcurrencyGate::new();
        gate.set_cap(Scope::Role("specialist".into()), 1);

        let bus = EventBus::default();
        let sink = RecordingSink::new();
        let broadcaster = Arc::new(ScopeContentionEventBroadcaster::new(
            bus.clone(),
            Some(sink.clone() as Arc<dyn ScopeContentionEventSink>),
        ));

        let runner = SpecialistRunner::new(queue.clone(), active, dispatcher.clone(), 4)
            .with_concurrency_gate(gate)
            .with_scope_contention_broadcaster(broadcaster.clone());

        // First request takes the only Role permit; the second request
        // contends and carries a reserved run row (id 99).
        queue.enqueue(req_full("id-1", "ENG-1", "specialist", None, None));
        queue.enqueue(req_full_with_run("id-2", "ENG-2", "specialist", 99));

        let _ = runner.run_pending(CancellationToken::new()).await;

        let persisted = sink.snapshot();
        assert_eq!(persisted.len(), 1);
        match &persisted[0] {
            OrchestratorEvent::ScopeCapReached {
                run_id, identifier, ..
            } => {
                assert_eq!(*run_id, Some(99));
                assert!(
                    identifier.is_none(),
                    "Run-subject events must not carry an identifier",
                );
            }
            other => panic!("unexpected event: {other:?}"),
        }

        dispatcher.release_gate();
        // Drain the in-flight dispatch first, then run another pass so
        // the deferred request (which never spawned because the gate
        // contended) gets picked up before reaping.
        let _ = wait_for_outcomes(&runner, 1).await;
        let _ = runner.run_pending(CancellationToken::new()).await;
        let _ = wait_for_outcomes(&runner, 1).await;
    }

    /// Bus subscribers see the same event stream the persistence sink
    /// receives.
    #[tokio::test]
    async fn broadcaster_emits_on_bus_in_lockstep_with_persistence() {
        use futures::StreamExt;

        let queue = Arc::new(SpecialistDispatchQueue::new());
        let active = store_with(vec![issue("id-1", "ENG-1"), issue("id-2", "ENG-2")]);
        let dispatcher = Arc::new(RecordingDispatcher::new(ReleaseReason::Completed));
        dispatcher.enable_gate();

        let gate = ConcurrencyGate::new();
        gate.set_cap(Scope::Role("specialist".into()), 1);

        let bus = EventBus::default();
        let mut rx = bus.subscribe();
        let sink = RecordingSink::new();
        let broadcaster = Arc::new(ScopeContentionEventBroadcaster::new(
            bus.clone(),
            Some(sink.clone() as Arc<dyn ScopeContentionEventSink>),
        ));

        let runner = SpecialistRunner::new(queue.clone(), active, dispatcher.clone(), 4)
            .with_concurrency_gate(gate)
            .with_scope_contention_broadcaster(broadcaster);

        queue.enqueue(req_full("id-1", "ENG-1", "specialist", None, None));
        queue.enqueue(req_full("id-2", "ENG-2", "specialist", None, None));

        let _ = runner.run_pending(CancellationToken::new()).await;

        let bus_event = rx.next().await.expect("event").expect("not lagged");
        let persisted = sink.snapshot();
        assert_eq!(persisted.len(), 1);
        assert_eq!(bus_event, persisted[0]);

        dispatcher.release_gate();
        // Drain the in-flight dispatch first, then run another pass so
        // the deferred request (which never spawned because the gate
        // contended) gets picked up before reaping.
        let _ = wait_for_outcomes(&runner, 1).await;
        let _ = runner.run_pending(CancellationToken::new()).await;
        let _ = wait_for_outcomes(&runner, 1).await;
    }

    /// An idle pass after a contention episode terminates the episode in
    /// the dedup log so a fresh contention re-emits.
    #[tokio::test]
    async fn idle_pass_terminates_episode_and_re_contention_re_emits() {
        let queue = Arc::new(SpecialistDispatchQueue::new());
        let active = store_with(vec![issue("id-1", "ENG-1"), issue("id-2", "ENG-2")]);
        let dispatcher = Arc::new(RecordingDispatcher::new(ReleaseReason::Completed));

        let gate = ConcurrencyGate::new();
        gate.set_cap(Scope::Role("specialist".into()), 1);

        let bus = EventBus::default();
        let sink = RecordingSink::new();
        let broadcaster = Arc::new(ScopeContentionEventBroadcaster::new(
            bus,
            Some(sink.clone() as Arc<dyn ScopeContentionEventSink>),
        ));

        let runner = SpecialistRunner::new(queue.clone(), active, dispatcher.clone(), 4)
            .with_concurrency_gate(gate)
            .with_scope_contention_broadcaster(broadcaster);

        // Episode 1: ENG-1 holds; ENG-2 contends. (RecordingDispatcher
        // not gated → spawn completes immediately.)
        dispatcher.enable_gate();
        queue.enqueue(req_full("id-1", "ENG-1", "specialist", None, None));
        queue.enqueue(req_full("id-2", "ENG-2", "specialist", None, None));
        let _ = runner.run_pending(CancellationToken::new()).await;
        assert_eq!(sink.snapshot().len(), 1);

        // Drain ENG-1 so the Role permit frees, then run a pass that
        // spawns ENG-2 (no contention → episode ends).
        dispatcher.release_gate();
        let _ = wait_for_outcomes(&runner, 1).await;
        dispatcher.enable_gate();
        let _ = runner.run_pending(CancellationToken::new()).await;
        // Episode for ENG-2 ended. No new event yet.
        assert_eq!(sink.snapshot().len(), 1);

        // A truly idle pass while ENG-2 is in flight: still no new event.
        let _ = runner.run_pending(CancellationToken::new()).await;
        assert_eq!(sink.snapshot().len(), 1);

        // Episode 2: enqueue ENG-3 against the still-held permit and
        // observe a fresh event with the new identifier.
        let active2 = store_with(vec![issue("id-3", "ENG-3")]);
        // Replace the runner's active set view via a fresh enqueue +
        // snapshot would be complex; instead enqueue another contender
        // for the same in-flight ENG-2 by simulating ENG-3 entering.
        let _ = active2; // silence unused warning; we use a separate pathway below

        // Simpler: release ENG-2, then re-run with two fresh requests so
        // we get a clean episode boundary.
        dispatcher.release_gate();
        let _ = wait_for_outcomes(&runner, 1).await;

        dispatcher.enable_gate();
        queue.enqueue(req_full("id-1", "ENG-1", "specialist", None, None));
        queue.enqueue(req_full("id-2", "ENG-2", "specialist", None, None));
        let _ = runner.run_pending(CancellationToken::new()).await;

        let persisted = sink.snapshot();
        assert_eq!(
            persisted.len(),
            2,
            "second episode must re-emit after the first ended; got {persisted:?}",
        );

        dispatcher.release_gate();
        // Drain the in-flight dispatch first, then run another pass so
        // the deferred request (which never spawned because the gate
        // contended) gets picked up before reaping.
        let _ = wait_for_outcomes(&runner, 1).await;
        let _ = runner.run_pending(CancellationToken::new()).await;
        let _ = wait_for_outcomes(&runner, 1).await;
    }

    // -- Pre-lease cancellation observation ------------------------------

    use crate::cancellation::{CancelRequest, CancellationQueue};
    use crate::work_item::WorkItemId;

    /// Sink that records every `mark_cancelled` invocation. Optionally
    /// fails on the first call so tests can assert the wind-down path
    /// still drains the queue and parks the report when the durable
    /// status update errors.
    struct RecordingCancellationSink {
        calls: StdMutex<Vec<(RunRef, CancelRequest)>>,
        fail_once: StdMutex<bool>,
    }

    impl RecordingCancellationSink {
        fn new() -> Self {
            Self {
                calls: StdMutex::new(Vec::new()),
                fail_once: StdMutex::new(false),
            }
        }
        fn fail_next(self: &Arc<Self>) {
            *self.fail_once.lock().unwrap() = true;
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
        ) -> Result<bool, RunCancellationStatusError> {
            let mut fail = self.fail_once.lock().unwrap();
            if *fail {
                *fail = false;
                return Err(RunCancellationStatusError::Backend("simulated".into()));
            }
            self.calls.lock().unwrap().push((run_id, request.clone()));
            Ok(true)
        }
    }

    fn req_with_parent(
        id: &str,
        identifier: &str,
        run_id: Option<i64>,
        parent: Option<i64>,
    ) -> SpecialistDispatchRequest {
        SpecialistDispatchRequest {
            issue_id: id.into(),
            identifier: identifier.into(),
            role: RoleName::new("specialist"),
            rule_index: None,
            run_id: run_id.map(RunRef::new),
            agent_profile: None,
            repository: None,
            parent_work_item_id: parent.map(WorkItemId::new),
        }
    }

    #[tokio::test]
    async fn cancellation_observed_for_run_aborts_dispatch_and_drains_run_entry() {
        let queue = Arc::new(SpecialistDispatchQueue::new());
        let active = store_with(vec![issue("id-1", "ENG-1")]);
        let dispatcher = Arc::new(RecordingDispatcher::new(ReleaseReason::Completed));
        let cancel_queue = Arc::new(Mutex::new(CancellationQueue::new()));
        let sink = Arc::new(RecordingCancellationSink::new());

        cancel_queue.lock().await.enqueue(CancelRequest::for_run(
            RunRef::new(42),
            "operator cancel",
            "tester",
            "2026-05-09T00:00:00Z",
        ));

        let runner = SpecialistRunner::new(queue.clone(), active, dispatcher.clone(), 4)
            .with_cancellation(cancel_queue.clone(), sink.clone());

        queue.enqueue(req_with_parent("id-1", "ENG-1", Some(42), None));

        let report = runner.run_pending(CancellationToken::new()).await;
        assert_eq!(report.spawned, 0);
        assert_eq!(report.cancelled_before_lease, 1);
        assert_eq!(report.cancellations.len(), 1);
        assert_eq!(report.cancellations[0].identifier, "ENG-1");
        assert_eq!(report.cancellations[0].run_id, Some(RunRef::new(42)));
        assert_eq!(report.cancellations[0].request.reason, "operator cancel");

        assert_eq!(runner.inflight_count().await, 0);
        assert!(dispatcher.calls().is_empty(), "dispatcher must not run");

        let calls = sink.snapshot();
        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0].0, RunRef::new(42));

        // Run-keyed entry drained; queue is empty.
        let q = cancel_queue.lock().await;
        assert!(q.pending_for_run(RunRef::new(42)).is_none());
        assert_eq!(q.len(), 0);
    }

    #[tokio::test]
    async fn cancellation_observed_via_parent_keyspace_aborts_but_leaves_parent_entry() {
        let queue = Arc::new(SpecialistDispatchQueue::new());
        let active = store_with(vec![issue("id-1", "ENG-1")]);
        let dispatcher = Arc::new(RecordingDispatcher::new(ReleaseReason::Completed));
        let cancel_queue = Arc::new(Mutex::new(CancellationQueue::new()));
        let sink = Arc::new(RecordingCancellationSink::new());

        cancel_queue
            .lock()
            .await
            .enqueue(CancelRequest::for_work_item(
                WorkItemId::new(7),
                "parent cancel",
                "tester",
                "2026-05-09T00:00:00Z",
            ));

        let runner = SpecialistRunner::new(queue.clone(), active, dispatcher.clone(), 4)
            .with_cancellation(cancel_queue.clone(), sink.clone());

        // Run reservation present so the runner can mark it cancelled,
        // but the matching cancel lives in the work-item keyspace.
        queue.enqueue(req_with_parent("id-1", "ENG-1", Some(99), Some(7)));

        let report = runner.run_pending(CancellationToken::new()).await;
        assert_eq!(report.spawned, 0);
        assert_eq!(report.cancelled_before_lease, 1);
        assert_eq!(report.cancellations[0].request.reason, "parent cancel");

        let calls = sink.snapshot();
        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0].0, RunRef::new(99));

        // Parent entry preserved — the propagator owns it.
        let q = cancel_queue.lock().await;
        assert!(q.pending_for_work_item(WorkItemId::new(7)).is_some());
        assert!(q.pending_for_run(RunRef::new(99)).is_none());
    }

    #[tokio::test]
    async fn cancellation_observed_without_run_reservation_skips_sink_and_run_drain() {
        let queue = Arc::new(SpecialistDispatchQueue::new());
        let active = store_with(vec![issue("id-1", "ENG-1")]);
        let dispatcher = Arc::new(RecordingDispatcher::new(ReleaseReason::Completed));
        let cancel_queue = Arc::new(Mutex::new(CancellationQueue::new()));
        let sink = Arc::new(RecordingCancellationSink::new());

        cancel_queue
            .lock()
            .await
            .enqueue(CancelRequest::for_work_item(
                WorkItemId::new(7),
                "parent cancel",
                "tester",
                "2026-05-09T00:00:00Z",
            ));

        let runner = SpecialistRunner::new(queue.clone(), active, dispatcher.clone(), 4)
            .with_cancellation(cancel_queue.clone(), sink.clone());

        queue.enqueue(req_with_parent("id-1", "ENG-1", None, Some(7)));

        let report = runner.run_pending(CancellationToken::new()).await;
        assert_eq!(report.cancelled_before_lease, 1);
        assert!(dispatcher.calls().is_empty());
        // No run id — no typed-status update was possible.
        assert!(sink.snapshot().is_empty());
        // Parent entry preserved for the propagator.
        let q = cancel_queue.lock().await;
        assert!(q.pending_for_work_item(WorkItemId::new(7)).is_some());
    }

    #[tokio::test]
    async fn cancellation_skipped_when_request_carries_no_subject() {
        let queue = Arc::new(SpecialistDispatchQueue::new());
        let active = store_with(vec![issue("id-1", "ENG-1")]);
        let dispatcher = Arc::new(RecordingDispatcher::new(ReleaseReason::Completed));
        let cancel_queue = Arc::new(Mutex::new(CancellationQueue::new()));
        let sink = Arc::new(RecordingCancellationSink::new());

        // Queue holds an unrelated cancel — must not affect a request
        // that carries neither run_id nor parent_work_item_id.
        cancel_queue.lock().await.enqueue(CancelRequest::for_run(
            RunRef::new(1234),
            "unrelated",
            "tester",
            "2026-05-09T00:00:00Z",
        ));

        let runner = SpecialistRunner::new(queue.clone(), active, dispatcher.clone(), 4)
            .with_cancellation(cancel_queue.clone(), sink.clone());

        queue.enqueue(req_with_parent("id-1", "ENG-1", None, None));

        let report = runner.run_pending(CancellationToken::new()).await;
        assert_eq!(report.spawned, 1, "report = {report:?}");
        assert_eq!(report.cancelled_before_lease, 0);

        let _ = wait_for_outcomes(&runner, 1).await;
        assert_eq!(dispatcher.calls(), vec!["ENG-1"]);
        assert!(sink.snapshot().is_empty());
    }

    #[tokio::test]
    async fn cancellation_sink_failure_still_aborts_and_drains_queue() {
        let queue = Arc::new(SpecialistDispatchQueue::new());
        let active = store_with(vec![issue("id-1", "ENG-1")]);
        let dispatcher = Arc::new(RecordingDispatcher::new(ReleaseReason::Completed));
        let cancel_queue = Arc::new(Mutex::new(CancellationQueue::new()));
        let sink = Arc::new(RecordingCancellationSink::new());
        sink.fail_next();

        cancel_queue.lock().await.enqueue(CancelRequest::for_run(
            RunRef::new(42),
            "operator cancel",
            "tester",
            "2026-05-09T00:00:00Z",
        ));

        let runner = SpecialistRunner::new(queue.clone(), active, dispatcher.clone(), 4)
            .with_cancellation(cancel_queue.clone(), sink.clone());

        queue.enqueue(req_with_parent("id-1", "ENG-1", Some(42), None));

        let report = runner.run_pending(CancellationToken::new()).await;
        assert_eq!(report.cancelled_before_lease, 1);
        assert!(dispatcher.calls().is_empty());

        // Sink failure does not block the queue drain — operator can
        // re-issue the cancel and converge later.
        let q = cancel_queue.lock().await;
        assert!(q.pending_for_run(RunRef::new(42)).is_none());
    }

    #[tokio::test]
    async fn cancellation_releases_scope_permits_so_other_dispatch_can_proceed() {
        use crate::concurrency_gate::{ConcurrencyGate, Scope};

        let queue = Arc::new(SpecialistDispatchQueue::new());
        let active = store_with(vec![issue("id-1", "ENG-1"), issue("id-2", "ENG-2")]);
        let dispatcher = Arc::new(RecordingDispatcher::new(ReleaseReason::Completed));
        let cancel_queue = Arc::new(Mutex::new(CancellationQueue::new()));
        let sink = Arc::new(RecordingCancellationSink::new());

        // Cap the specialist role at one in-flight dispatch.
        let gate = ConcurrencyGate::new();
        gate.set_cap(Scope::Role("specialist".into()), 1);

        cancel_queue.lock().await.enqueue(CancelRequest::for_run(
            RunRef::new(11),
            "stop",
            "tester",
            "2026-05-09T00:00:00Z",
        ));

        let runner = SpecialistRunner::new(queue.clone(), active, dispatcher.clone(), 4)
            .with_concurrency_gate(gate)
            .with_cancellation(cancel_queue.clone(), sink.clone());

        // The cancelled request comes first, so its scope permit must
        // be released before the second dispatch acquires the same
        // capped scope in the same pass.
        queue.enqueue(req_with_parent("id-1", "ENG-1", Some(11), None));
        queue.enqueue(req_with_parent("id-2", "ENG-2", Some(22), None));

        let report = runner.run_pending(CancellationToken::new()).await;
        assert_eq!(report.cancelled_before_lease, 1);
        assert_eq!(
            report.spawned, 1,
            "second request must claim the scope freed by the cancelled first; report = {report:?}",
        );
        let _ = wait_for_outcomes(&runner, 1).await;
        assert_eq!(dispatcher.calls(), vec!["ENG-2"]);
    }
}
