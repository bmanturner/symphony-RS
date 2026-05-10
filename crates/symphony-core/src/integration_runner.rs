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

use std::collections::HashMap;
use std::sync::Arc;

use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use tokio::sync::{Mutex, Semaphore};
use tokio::task::JoinSet;
use tokio_util::sync::CancellationToken;
use tracing::{debug, warn};

use crate::blocker::RunRef;
use crate::concurrency_gate::{
    ConcurrencyGate, DispatchTriple, RunnerScopes, ScopeContended, ScopeKind, ScopePermitSet,
};
use crate::integration_tick::{IntegrationDispatchQueue, IntegrationDispatchRequest};
use crate::lease::{LeaseClock, LeaseConfig, LeaseOwner};
use crate::run_lease::{
    HeartbeatPulseObservation, LeaseAcquireOutcome, LeaseHeartbeat, RunLeaseGuard, RunLeaseStore,
    pulse_observed,
};
use crate::scope_contention_broadcaster::{
    ContentionSubject, ScopeContentionEventBroadcaster, ScopeFields,
};
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
    /// Requests parked because at least one
    /// [`crate::concurrency_gate::ConcurrencyGate`] scope was at cap.
    /// Length matches `scope_contentions.len()` — the typed observations
    /// are surfaced separately so callers can route them.
    pub deferred_scope_contention: usize,
    /// Typed in-flight observations for every dispatch that was deferred
    /// because a scope was at cap on this pass. Emitted in walk order so
    /// downstream observers can correlate against the parked dispatches.
    pub scope_contentions: Vec<ScopeContendedObservation>,
}

impl IntegrationRunReport {
    /// `true` iff the pass was a complete no-op.
    pub fn is_idle(&self) -> bool {
        self.spawned == 0
            && self.deferred_capacity == 0
            && self.deferred_lease_contention == 0
            && self.missing_run == 0
            && self.lease_backend_error == 0
            && self.deferred_scope_contention == 0
            && self.scope_contentions.is_empty()
    }
}

/// Per-pass typed observation surfaced when a dispatch could not acquire
/// its [`RunnerScopes`] permit set because at least one scope was at cap.
///
/// Pairs the request's tracker-facing parent identifier with the
/// [`ScopeContended`] reported by [`ConcurrencyGate::try_acquire`] so a
/// downstream observer (the durable-event subtask, the operator log, a
/// status TUI) can answer "why is this parent not integrating" without
/// re-deriving the scope keys from workflow config.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ScopeContendedObservation {
    /// Tracker-facing parent identifier the parked request carries.
    pub identifier: String,
    /// Durable run row reserved for this dispatch, if the request had
    /// one. Drives the [`ContentionSubject`] used by the durable
    /// [`ScopeContentionEventBroadcaster`]: `Run(run_id)` when present,
    /// `Identifier(identifier)` otherwise. Integration dispatches today
    /// always reserve a run row, but the field stays optional so the
    /// runner does not silently break if a future caller emits a
    /// run-less request.
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
/// [`IntegrationDispatchRunner::pulse_heartbeats`].
///
/// Pairs the typed [`HeartbeatPulseObservation`] from
/// [`crate::run_lease::pulse_observed`] with the run/parent identifier
/// the heartbeat targets so the scheduler can route the observation
/// (durable run-status update on contention, recovery dispatch on
/// not-found, transient-error log on backend error) without re-keying
/// against the in-flight set.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RunnerHeartbeatObservation {
    /// Durable run row the heartbeat was renewed against.
    pub run_id: RunRef,
    /// Tracker-facing identifier of the parent the dispatch owns.
    pub identifier: String,
    /// Typed outcome of the renewal pulse.
    pub observation: HeartbeatPulseObservation,
}

/// Per-run heartbeat registration: the human-readable identifier (so
/// observations carry it back to the caller) plus the live heartbeat
/// guarded by an async mutex (`pulse` is `&mut self`).
struct HeartbeatRegistration {
    identifier: String,
    heartbeat: Arc<Mutex<LeaseHeartbeat>>,
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
    /// In-flight heartbeats, keyed by `run_id`. Populated when a lease
    /// is acquired alongside a spawned dispatch and removed by the
    /// spawned task at terminal release. [`Self::pulse_heartbeats`]
    /// snapshots the map and renews each due lease through the
    /// configured wiring's clock.
    heartbeats: Arc<Mutex<HashMap<RunRef, HeartbeatRegistration>>>,
    /// Optional multi-scope concurrency gate. When `Some`, every
    /// dispatch carrying a `role` acquires
    /// `{Global, Role, AgentProfile?, Repository?}` permits before the
    /// agent runs and releases them on terminal completion. When `None`
    /// (or when a request has no `role`), only the local capacity
    /// semaphore caps concurrency.
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
            heartbeats: Arc::new(Mutex::new(HashMap::new())),
            concurrency_gate: None,
            scope_contention_broadcaster: None,
        }
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
            // Even on an idle pass we must notify the broadcaster so any
            // episodes that were active in a prior pass get terminated
            // (the dedup log treats absence as "episode ended"). Without
            // this, a parked dispatch that drains terminally would leave
            // a stale episode in the log forever.
            self.broadcast_pass_observations(&[]);
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
                            req.parent_identifier.clone(),
                            req.run_id,
                            &contended,
                        ));
                    debug!(
                        parent_identifier = %req.parent_identifier,
                        scope_kind = ?contended.scope.kind(),
                        scope_key = %contended.scope.key(),
                        in_flight = contended.in_flight,
                        cap = contended.cap,
                        "integration runner: scope contended; parking",
                    );
                    new_deferred.push(req);
                    report.deferred_scope_contention += 1;
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
            let parent_id = req.parent_id;
            let parent_identifier = req.parent_identifier.clone();
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
                        identifier: parent_identifier.clone(),
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
                        parent_identifier = %parent_identifier,
                        error = %err,
                        "integration runner: lease release failed; relying on TTL reaper",
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

    /// Attempt to acquire scope permits for `req` against the configured
    /// [`ConcurrencyGate`]. Returns `Ok(None)` when no gate is wired or
    /// the request lacks a `role`, `Ok(Some(set))` when every scope had
    /// headroom, and `Err(_)` when any scope was at cap (no permits held
    /// — the inner gate already rolled back partial acquisitions).
    fn try_acquire_scopes(
        &self,
        req: &IntegrationDispatchRequest,
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
            role: None,
            agent_profile: None,
            repository: None,
        }
    }

    fn req_with_run(id: i64, identifier: &str, run_id: i64) -> IntegrationDispatchRequest {
        IntegrationDispatchRequest {
            parent_id: WorkItemId::new(id),
            parent_identifier: identifier.into(),
            parent_title: format!("title {identifier}"),
            cause: IntegrationRequestCause::DirectIntegrationRequest,
            run_id: Some(crate::blocker::RunRef::new(run_id)),
            role: None,
            agent_profile: None,
            repository: None,
        }
    }

    fn req_full(
        id: i64,
        identifier: &str,
        role: &str,
        agent_profile: Option<&str>,
        repository: Option<&str>,
    ) -> IntegrationDispatchRequest {
        IntegrationDispatchRequest {
            parent_id: WorkItemId::new(id),
            parent_identifier: identifier.into(),
            parent_title: format!("title {identifier}"),
            cause: IntegrationRequestCause::DirectIntegrationRequest,
            run_id: None,
            role: Some(crate::role::RoleName::new(role)),
            agent_profile: agent_profile.map(str::to_owned),
            repository: repository.map(str::to_owned),
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
            role: None,
            agent_profile: None,
            repository: None,
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
    ) -> (Arc<IntegrationDispatchQueue>, IntegrationDispatchRunner) {
        let queue = Arc::new(IntegrationDispatchQueue::new());
        let runner = IntegrationDispatchRunner::new(queue.clone(), dispatcher, max_concurrent)
            .with_leasing(store, lease_owner(), LeaseConfig::default(), clock);
        (queue, runner)
    }

    #[tokio::test]
    async fn pulse_heartbeats_advances_lease_expiry_during_long_running_dispatch() {
        let dispatcher = Arc::new(RecordingDispatcher::new(
            IntegrationDispatchReason::Completed,
        ));
        dispatcher.enable_gate(); // hold dispatch open across pulses
        let store = Arc::new(InMemoryRunLeaseStore::new());
        store.register_run(RunRef::new(42)).await;
        let clock = Arc::new(AdvanceableLeaseClock::new("T0"));

        let (queue, runner) =
            build_lease_runner_with_clock(4, dispatcher.clone(), store.clone(), clock.clone());
        queue.enqueue(req_with_run(1, "PROJ-1", 42));

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
        assert_eq!(obs.identifier, "PROJ-1");
        assert_eq!(obs.observation, HeartbeatPulseObservation::Renewed);

        let after_renewal = store.snapshot(RunRef::new(42)).await.unwrap().1;
        assert!(
            after_renewal > initial,
            "lease_expires_at must advance after a successful pulse: {after_renewal} > {initial}",
        );

        // Release the gate so the dispatch completes; lease must clear
        // exactly once on terminal release and the heartbeat registry
        // must drain.
        dispatcher.release_gate();
        let outcomes = wait_for_outcomes(&runner, 1).await;
        assert_eq!(outcomes[0].reason, IntegrationDispatchReason::Completed);
        assert!(store.snapshot(RunRef::new(42)).await.is_none());
        assert_eq!(store.release_calls().await, vec![RunRef::new(42)]);
        assert_eq!(runner.heartbeat_count().await, 0);
    }

    #[tokio::test]
    async fn pulse_heartbeats_advances_expiry_for_blocked_dispatch() {
        let dispatcher = Arc::new(RecordingDispatcher::new(
            IntegrationDispatchReason::Completed,
        ));
        dispatcher.set_reason_for("PROJ-2", IntegrationDispatchReason::Blocked);
        dispatcher.enable_gate();
        let store = Arc::new(InMemoryRunLeaseStore::new());
        store.register_run(RunRef::new(7)).await;
        let clock = Arc::new(AdvanceableLeaseClock::new("T0"));

        let (queue, runner) =
            build_lease_runner_with_clock(4, dispatcher.clone(), store.clone(), clock.clone());
        queue.enqueue(req_with_run(2, "PROJ-2", 7));

        let _ = runner.run_pending(CancellationToken::new()).await;
        assert_eq!(runner.heartbeat_count().await, 1);

        let initial = store.snapshot(RunRef::new(7)).await.unwrap().1;
        clock.set("T9");
        let obs = runner.pulse_heartbeats().await;
        assert_eq!(obs.len(), 1);
        assert_eq!(obs[0].observation, HeartbeatPulseObservation::Renewed);
        let after = store.snapshot(RunRef::new(7)).await.unwrap().1;
        assert!(after > initial);

        // Terminal release on Blocked still clears the lease.
        dispatcher.release_gate();
        let outcomes = wait_for_outcomes(&runner, 1).await;
        assert_eq!(outcomes[0].reason, IntegrationDispatchReason::Blocked);
        assert!(store.snapshot(RunRef::new(7)).await.is_none());
        assert_eq!(runner.heartbeat_count().await, 0);
    }

    #[tokio::test]
    async fn pulse_heartbeats_advances_expiry_for_waived_dispatch() {
        // "Waived" gating happens upstream: the dispatcher reports
        // Failed for a non-blocker error. The heartbeat must still pulse
        // mid-flight and the lease must still clear on terminal release.
        let dispatcher = Arc::new(RecordingDispatcher::new(IntegrationDispatchReason::Failed));
        dispatcher.enable_gate();
        let store = Arc::new(InMemoryRunLeaseStore::new());
        store.register_run(RunRef::new(13)).await;
        let clock = Arc::new(AdvanceableLeaseClock::new("T0"));

        let (queue, runner) =
            build_lease_runner_with_clock(4, dispatcher.clone(), store.clone(), clock.clone());
        queue.enqueue(IntegrationDispatchRequest {
            parent_id: WorkItemId::new(4),
            parent_identifier: "PROJ-4".into(),
            parent_title: "consolidate".into(),
            cause: IntegrationRequestCause::AllChildrenTerminal,
            run_id: Some(RunRef::new(13)),
            role: None,
            agent_profile: None,
            repository: None,
        });

        let _ = runner.run_pending(CancellationToken::new()).await;

        let initial = store.snapshot(RunRef::new(13)).await.unwrap().1;
        clock.set("T9");
        let obs = runner.pulse_heartbeats().await;
        assert_eq!(obs.len(), 1);
        assert_eq!(obs[0].observation, HeartbeatPulseObservation::Renewed);
        assert_eq!(obs[0].identifier, "PROJ-4");
        let after = store.snapshot(RunRef::new(13)).await.unwrap().1;
        assert!(after > initial);

        dispatcher.release_gate();
        let outcomes = wait_for_outcomes(&runner, 1).await;
        assert_eq!(outcomes[0].reason, IntegrationDispatchReason::Failed);
        assert!(store.snapshot(RunRef::new(13)).await.is_none());
        assert_eq!(runner.heartbeat_count().await, 0);
    }

    #[tokio::test]
    async fn pulse_heartbeats_surfaces_contended_observation() {
        let dispatcher = Arc::new(RecordingDispatcher::new(
            IntegrationDispatchReason::Completed,
        ));
        dispatcher.enable_gate();
        let store = Arc::new(InMemoryRunLeaseStore::new());
        store.register_run(RunRef::new(42)).await;
        let clock = Arc::new(AdvanceableLeaseClock::new("T0"));

        let (queue, runner) =
            build_lease_runner_with_clock(4, dispatcher.clone(), store.clone(), clock.clone());
        queue.enqueue(req_with_run(1, "PROJ-1", 42));

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
        assert_eq!(observations[0].identifier, "PROJ-1");

        dispatcher.release_gate();
        let _ = wait_for_outcomes(&runner, 1).await;
        assert_eq!(runner.heartbeat_count().await, 0);
    }

    #[tokio::test]
    async fn pulse_heartbeats_surfaces_not_found_observation() {
        let dispatcher = Arc::new(RecordingDispatcher::new(
            IntegrationDispatchReason::Completed,
        ));
        dispatcher.enable_gate();
        let store = Arc::new(InMemoryRunLeaseStore::new());
        store.register_run(RunRef::new(42)).await;
        let clock = Arc::new(AdvanceableLeaseClock::new("T0"));

        let (queue, runner) =
            build_lease_runner_with_clock(4, dispatcher.clone(), store.clone(), clock.clone());
        queue.enqueue(req_with_run(1, "PROJ-1", 42));
        let _ = runner.run_pending(CancellationToken::new()).await;

        // Drop the run row out from under the heartbeat: the pulse must
        // surface NotFound rather than silently succeeding.
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
            IntegrationDispatchReason::Completed,
        ));
        dispatcher.enable_gate();
        let store = Arc::new(InMemoryRunLeaseStore::new());
        store.register_run(RunRef::new(42)).await;
        let clock = Arc::new(AdvanceableLeaseClock::new("T0"));

        let (queue, runner) =
            build_lease_runner_with_clock(4, dispatcher.clone(), store.clone(), clock.clone());
        queue.enqueue(req_with_run(1, "PROJ-1", 42));
        let _ = runner.run_pending(CancellationToken::new()).await;

        // Force the next acquire to fail: the pulse must fold the
        // backend error into a typed observation.
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
        let queue = Arc::new(IntegrationDispatchQueue::new());
        let dispatcher = Arc::new(RecordingDispatcher::new(
            IntegrationDispatchReason::Completed,
        ));
        let runner = IntegrationDispatchRunner::new(queue, dispatcher, 4);
        let observations = runner.pulse_heartbeats().await;
        assert!(observations.is_empty());
        assert_eq!(runner.heartbeat_count().await, 0);
    }

    #[tokio::test]
    async fn terminal_release_clears_lease_exactly_once_after_pulses() {
        let dispatcher = Arc::new(RecordingDispatcher::new(
            IntegrationDispatchReason::Completed,
        ));
        dispatcher.enable_gate();
        let store = Arc::new(InMemoryRunLeaseStore::new());
        store.register_run(RunRef::new(42)).await;
        let clock = Arc::new(AdvanceableLeaseClock::new("T0"));

        let (queue, runner) =
            build_lease_runner_with_clock(4, dispatcher.clone(), store.clone(), clock.clone());
        queue.enqueue(req_with_run(1, "PROJ-1", 42));
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
        assert_eq!(outcomes[0].reason, IntegrationDispatchReason::Completed);

        // Pulses re-acquire through `acquire`; only terminal release
        // calls `release` — so exactly one release call recorded.
        assert_eq!(store.release_calls().await, vec![RunRef::new(42)]);
        assert!(store.snapshot(RunRef::new(42)).await.is_none());
        assert_eq!(runner.heartbeat_count().await, 0);
    }

    // ---- Concurrency-gate wiring (CHECKLIST_v2 Phase 11) -----------

    use crate::concurrency_gate::{ConcurrencyGate, Scope};

    #[tokio::test]
    async fn scope_role_cap_serializes_integration_under_global_capacity() {
        // Role cap = 1 (the default for integration owners) must hold
        // exactly one dispatch in flight even though local capacity
        // allows more. Parked dispatches surface a typed contention
        // observation against the Role scope.
        let queue = Arc::new(IntegrationDispatchQueue::new());
        let dispatcher = Arc::new(RecordingDispatcher::new(
            IntegrationDispatchReason::Completed,
        ));
        dispatcher.enable_gate();

        let gate = ConcurrencyGate::new();
        gate.set_cap(Scope::Role("platform_lead".into()), 1);

        let runner = IntegrationDispatchRunner::new(queue.clone(), dispatcher.clone(), 4)
            .with_concurrency_gate(gate.clone());

        queue.enqueue(req_full(1, "PROJ-1", "platform_lead", None, None));
        queue.enqueue(req_full(2, "PROJ-2", "platform_lead", None, None));
        queue.enqueue(req_full(3, "PROJ-3", "platform_lead", None, None));

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
        let parked: Vec<&str> = report
            .scope_contentions
            .iter()
            .map(|o| o.identifier.as_str())
            .collect();
        assert_eq!(parked, vec!["PROJ-2", "PROJ-3"]);

        // Local capacity is unaffected: parked dispatches did not
        // consume permits.
        assert_eq!(runner.inflight_count().await, 1);
        assert_eq!(runner.deferred_count().await, 2);

        dispatcher.enable_gate();
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
    async fn scope_repository_cap_defers_concurrent_dispatch_against_same_repo() {
        let queue = Arc::new(IntegrationDispatchQueue::new());
        let dispatcher = Arc::new(RecordingDispatcher::new(
            IntegrationDispatchReason::Completed,
        ));
        dispatcher.enable_gate();

        let gate = ConcurrencyGate::new();
        gate.set_cap(Scope::Repository("acme/widgets".into()), 1);

        let runner = IntegrationDispatchRunner::new(queue.clone(), dispatcher.clone(), 4)
            .with_concurrency_gate(gate);

        queue.enqueue(req_full(
            1,
            "PROJ-1",
            "platform_lead",
            None,
            Some("acme/widgets"),
        ));
        queue.enqueue(req_full(
            2,
            "PROJ-2",
            "platform_lead",
            None,
            Some("acme/widgets"),
        ));

        let report = runner.run_pending(CancellationToken::new()).await;
        assert_eq!(report.spawned, 1);
        assert_eq!(report.deferred_scope_contention, 1);
        let obs = &report.scope_contentions[0];
        assert_eq!(obs.scope_kind, ScopeKind::Repository);
        assert_eq!(obs.scope_key, "acme/widgets");
        assert_eq!(obs.identifier, "PROJ-2");

        dispatcher.release_gate();
        let _ = wait_for_outcomes(&runner, 1).await;
    }

    #[tokio::test]
    async fn scope_contention_does_not_consume_local_capacity() {
        // Capacity 1 + role cap 0 means the gate rejects the first
        // dispatch, but the local permit must NOT be consumed: the
        // second (cap-free) dispatch on the same pass still spawns.
        let queue = Arc::new(IntegrationDispatchQueue::new());
        let dispatcher = Arc::new(RecordingDispatcher::new(
            IntegrationDispatchReason::Completed,
        ));

        let gate = ConcurrencyGate::new();
        gate.set_cap(Scope::Role("frozen".into()), 0);

        let runner = IntegrationDispatchRunner::new(queue.clone(), dispatcher.clone(), 1)
            .with_concurrency_gate(gate);

        queue.enqueue(req_full(1, "PROJ-1", "frozen", None, None));
        queue.enqueue(req_full(2, "PROJ-2", "platform_lead", None, None));

        let report = runner.run_pending(CancellationToken::new()).await;
        assert_eq!(report.spawned, 1, "report = {report:?}");
        assert_eq!(report.deferred_scope_contention, 1);
        assert_eq!(report.scope_contentions[0].identifier, "PROJ-1");
        assert_eq!(report.scope_contentions[0].scope_kind, ScopeKind::Role);
        assert_eq!(report.scope_contentions[0].scope_key, "frozen");
        assert_eq!(report.scope_contentions[0].cap, 0);

        let _ = wait_for_outcomes(&runner, 1).await;
        assert_eq!(dispatcher.calls(), vec!["PROJ-2"]);
    }

    #[tokio::test]
    async fn no_concurrency_gate_means_no_scope_observations() {
        let queue = Arc::new(IntegrationDispatchQueue::new());
        let dispatcher = Arc::new(RecordingDispatcher::new(
            IntegrationDispatchReason::Completed,
        ));
        let runner = IntegrationDispatchRunner::new(queue.clone(), dispatcher.clone(), 4);
        queue.enqueue(req_full(
            1,
            "PROJ-1",
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
    async fn dispatch_without_role_bypasses_concurrency_gate() {
        // Requests without a `role` must bypass gate acquisition even
        // when a gate is wired — the integration tick emits role-less
        // requests today, and they should not block on a Global cap.
        let queue = Arc::new(IntegrationDispatchQueue::new());
        let dispatcher = Arc::new(RecordingDispatcher::new(
            IntegrationDispatchReason::Completed,
        ));

        let gate = ConcurrencyGate::new();
        gate.set_cap(Scope::Global, 0);

        let runner = IntegrationDispatchRunner::new(queue.clone(), dispatcher.clone(), 4)
            .with_concurrency_gate(gate);

        queue.enqueue(req(1, "PROJ-1"));
        let report = runner.run_pending(CancellationToken::new()).await;
        assert_eq!(report.spawned, 1, "report = {report:?}");
        assert!(report.scope_contentions.is_empty());
        let _ = wait_for_outcomes(&runner, 1).await;
    }

    // ---- Scope-contention broadcaster wiring (CHECKLIST_v2 Phase 11
    //      §195 integration subtask) ------------------------------------

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
        id: i64,
        identifier: &str,
        role: &str,
        run_id: i64,
    ) -> IntegrationDispatchRequest {
        IntegrationDispatchRequest {
            parent_id: WorkItemId::new(id),
            parent_identifier: identifier.into(),
            parent_title: format!("title {identifier}"),
            cause: IntegrationRequestCause::DirectIntegrationRequest,
            run_id: Some(crate::blocker::RunRef::new(run_id)),
            role: Some(crate::role::RoleName::new(role)),
            agent_profile: None,
            repository: None,
        }
    }

    /// N consecutive contended passes for the same dispatch produce
    /// exactly one durable `ScopeCapReached` event, and the event carries
    /// `run_id` (integration always reserves a run row).
    #[tokio::test]
    async fn broadcaster_dedups_repeated_contention_to_one_event_per_episode() {
        let queue = Arc::new(IntegrationDispatchQueue::new());
        let dispatcher = Arc::new(RecordingDispatcher::new(
            IntegrationDispatchReason::Completed,
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

        let runner = IntegrationDispatchRunner::new(queue.clone(), dispatcher.clone(), 4)
            .with_concurrency_gate(gate)
            .with_scope_contention_broadcaster(broadcaster);

        // PROJ-1 spawns; PROJ-2 contends on the Role scope every pass.
        queue.enqueue(req_full_with_run(1, "PROJ-1", "platform_lead", 11));
        queue.enqueue(req_full_with_run(2, "PROJ-2", "platform_lead", 22));

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
                assert_eq!(*run_id, Some(22), "integration always carries run_id");
                assert!(
                    identifier.is_none(),
                    "Run-subject events must not carry an identifier",
                );
                assert_eq!(*scope_kind, ScopeKind::Role);
                assert_eq!(scope_key, "platform_lead");
            }
            other => panic!("unexpected event: {other:?}"),
        }

        dispatcher.release_gate();
        let _ = wait_for_outcomes(&runner, 1).await;
        // Drain the deferred dispatch in a fresh pass.
        dispatcher.enable_gate();
        let _ = runner.run_pending(CancellationToken::new()).await;
        dispatcher.release_gate();
        let _ = wait_for_outcomes(&runner, 1).await;
    }

    /// Bus subscribers see the same event stream the persistence sink
    /// receives.
    #[tokio::test]
    async fn broadcaster_emits_on_bus_in_lockstep_with_persistence() {
        use futures::StreamExt;

        let queue = Arc::new(IntegrationDispatchQueue::new());
        let dispatcher = Arc::new(RecordingDispatcher::new(
            IntegrationDispatchReason::Completed,
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

        let runner = IntegrationDispatchRunner::new(queue.clone(), dispatcher.clone(), 4)
            .with_concurrency_gate(gate)
            .with_scope_contention_broadcaster(broadcaster);

        queue.enqueue(req_full_with_run(1, "PROJ-1", "platform_lead", 11));
        queue.enqueue(req_full_with_run(2, "PROJ-2", "platform_lead", 22));

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
    async fn idle_pass_terminates_episode_and_re_contention_re_emits() {
        let queue = Arc::new(IntegrationDispatchQueue::new());
        let dispatcher = Arc::new(RecordingDispatcher::new(
            IntegrationDispatchReason::Completed,
        ));

        let gate = ConcurrencyGate::new();
        gate.set_cap(Scope::Role("platform_lead".into()), 1);

        let bus = EventBus::default();
        let sink = RecordingSink::new();
        let broadcaster = Arc::new(ScopeContentionEventBroadcaster::new(
            bus,
            Some(sink.clone() as Arc<dyn ScopeContentionEventSink>),
        ));

        let runner = IntegrationDispatchRunner::new(queue.clone(), dispatcher.clone(), 4)
            .with_concurrency_gate(gate)
            .with_scope_contention_broadcaster(broadcaster);

        // Episode 1: PROJ-1 holds the Role permit, PROJ-2 contends.
        dispatcher.enable_gate();
        queue.enqueue(req_full_with_run(1, "PROJ-1", "platform_lead", 11));
        queue.enqueue(req_full_with_run(2, "PROJ-2", "platform_lead", 22));
        let _ = runner.run_pending(CancellationToken::new()).await;
        assert_eq!(sink.snapshot().len(), 1);

        // Drain PROJ-1 so the Role permit frees, then run a pass that
        // spawns the parked PROJ-2 (no contention → episode ends).
        dispatcher.release_gate();
        let _ = wait_for_outcomes(&runner, 1).await;
        dispatcher.enable_gate();
        let _ = runner.run_pending(CancellationToken::new()).await;
        assert_eq!(sink.snapshot().len(), 1);

        // A truly idle pass while PROJ-2 is in flight: still no new event.
        let _ = runner.run_pending(CancellationToken::new()).await;
        assert_eq!(sink.snapshot().len(), 1);

        // Episode 2: PROJ-2 still in flight; enqueue PROJ-3 against the
        // still-held Role permit and observe a fresh event for run 33.
        queue.enqueue(req_full_with_run(3, "PROJ-3", "platform_lead", 33));
        let _ = runner.run_pending(CancellationToken::new()).await;

        let persisted = sink.snapshot();
        assert_eq!(
            persisted.len(),
            2,
            "second episode must re-emit after the first ended; got {persisted:?}",
        );
        match &persisted[1] {
            OrchestratorEvent::ScopeCapReached { run_id, .. } => {
                assert_eq!(*run_id, Some(33));
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
