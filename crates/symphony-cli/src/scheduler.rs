//! Composition helper for the v2 multi-queue scheduler.
//!
//! Phase 11 of the v2 checklist replaces the flat [`PollLoop`] with a
//! [`SchedulerV2`] that fans logical queue ticks (intake, specialist,
//! integration, QA, follow-up approval, budget pause, recovery) under
//! one shared `polling.interval_ms` cadence. Wiring all of those at
//! once is too large for one commit; this module is the seam each
//! subsequent decomposition step extends.
//!
//! [`build_scheduler_v2`] currently registers the **intake**,
//! **recovery**, **specialist**, **integration**, and **QA** ticks
//! plus a [`SpecialistRunner`], an [`IntegrationDispatchRunner`], and
//! a [`QaDispatchRunner`] that drain the specialist, integration, and
//! QA dispatch queues under `cfg.agent.max_concurrent_agents`
//! capacity. The recovery tick is
//! driven by the shared [`ActiveSetStore`] plus an in-memory
//! [`ClaimedRunRegistry`] that stands in for the durable `runs` table
//! until a state DB is plumbed in: any claim whose `IssueId` is no
//! longer in the active set surfaces as an [`ExpiredLeaseCandidate`],
//! matching the flat poll loop's `Reconciled { dropped }`
//! reconciliation contract.
//!
//! The integration tick consults a caller-supplied
//! [`IntegrationQueueSource`] under [`IntegrationGates`] derived from
//! `cfg.integration.{require_all_children_terminal,
//! require_no_open_blockers}` and emits one
//! [`symphony_core::IntegrationDispatchRequest`] per ready parent.
//! Production wires this to a state-crate adapter
//! over `IntegrationQueueRepository`; tests wire a deterministic fake.
//! Each request is consumed by the [`IntegrationDispatchRunner`] under
//! the configured [`IntegrationDispatcher`] (production: the
//! integration-owner agent runner). The QA tick consults a caller-
//! supplied [`QaQueueSource`] under SPEC-default [`QaGates`] (blocker
//! gate on; no per-workflow knob exists yet) and dispatches each ready
//! work item through the configured [`QaDispatcher`]. Follow-up-
//! approval / budget-pause ticks layer on top of the same store and
//! runners in subsequent decomposition steps. The returned scheduler is not yet
//! wired into [`crate::run::run`]; that switch is the last
//! decomposition step. Until then the production entry point remains
//! [`PollLoop`].
//!
//! [`PollLoop`]: symphony_core::PollLoop

// Pre-wiring: `build_scheduler_v2` is exercised by this module's tests
// but not yet referenced from `run.rs`. Subsequent decomposition steps
// add the call site; until then suppress dead-code under `-D warnings`.
#![allow(dead_code)]

use std::collections::{HashMap, HashSet};
use std::sync::{Arc, Mutex as StdMutex};

use symphony_config::{self as cfg, WorkflowConfig};
use symphony_core::{
    ActiveSetStore, Dispatcher, ExpiredLeaseCandidate, IntakeQueueTick, IntegrationDispatchQueue,
    IntegrationDispatchRunner, IntegrationDispatcher, IntegrationGates, IntegrationQueueSource,
    IntegrationQueueTick, IssueId, OrphanedWorkspaceClaimCandidate, QaDispatchQueue,
    QaDispatchRunner, QaDispatcher, QaGates, QaQueueSource, QaQueueTick, QueueTick,
    QueueTickCadence, RecoveryDispatchQueue, RecoveryQueueError, RecoveryQueueSource,
    RecoveryQueueTick, RecoveryRunId, RecoveryWorkspaceClaimId, RoleKind, RoleKindLookup, RoleName,
    RoutingEngine, RoutingMatch, RoutingMatchMode, RoutingRule, RoutingTable, SchedulerV2,
    SchedulerV2Config, SpecialistDispatchQueue, SpecialistQueueTick, SpecialistRunner, TrackerRead,
    WorkItemId,
};

/// What [`build_scheduler_v2`] hands back to the composition root.
///
/// The store is exposed alongside the scheduler so subsequent
/// decomposition steps can give the same `Arc` to specialist /
/// recovery ticks without re-reading the tracker. Holding it on the
/// returned struct (instead of fishing it back out of the intake tick)
/// keeps the wiring shape uniform once those ticks are added.
pub struct SchedulerV2Bundle {
    /// The constructed scheduler, ready to drive via
    /// [`SchedulerV2::tick_once`] (tests) or [`SchedulerV2::run`]
    /// (composition root).
    pub scheduler: SchedulerV2,
    /// Shared active-set store written by the intake tick and read by
    /// every downstream queue tick.
    pub active_set: Arc<ActiveSetStore>,
    /// In-memory ledger of currently claimed runs. Recovery tick reads
    /// this against [`Self::active_set`] to surface
    /// [`ExpiredLeaseCandidate`]s for issues that have left the active
    /// set — the v2 analogue of the flat poll loop's
    /// `Reconciled { dropped }` event. Specialist / integration / QA
    /// runners (subsequent decomposition steps) record claims into the
    /// registry as they dispatch and forget them on completion. Until
    /// those runners are wired the registry stays empty in production
    /// and the recovery tick is a no-op.
    pub claimed_runs: Arc<ClaimedRunRegistry>,
    /// FIFO of pending [`symphony_core::RecoveryDispatchRequest`]s emitted by
    /// the recovery tick. Drained by the recovery runner once it is
    /// wired into the composition root; held on the bundle so tests
    /// (and the eventual `run.rs` switch) can observe it without
    /// reaching into the tick.
    pub recovery_dispatch: Arc<RecoveryDispatchQueue>,
    /// FIFO of pending [`symphony_core::SpecialistDispatchRequest`]s
    /// emitted by the specialist queue tick. Drained by
    /// [`Self::specialist_runner`] under bounded concurrency. Held on
    /// the bundle so the eventual `run.rs` switch (and tests) can
    /// observe queue state directly without reaching into the tick.
    pub specialist_dispatch: Arc<SpecialistDispatchQueue>,
    /// Bounded-concurrency consumer for [`Self::specialist_dispatch`],
    /// pre-wired with `cfg.agent.max_concurrent_agents` capacity and
    /// the supplied [`Dispatcher`] (production: `SymphonyDispatcher`).
    /// The composition root drives [`SpecialistRunner::run_pending`] and
    /// [`SpecialistRunner::reap_completed`] on its own cadence; the
    /// scheduler tick fan only enqueues requests.
    pub specialist_runner: Arc<SpecialistRunner>,
    /// FIFO of pending [`symphony_core::IntegrationDispatchRequest`]s
    /// emitted by the integration queue tick. Drained by
    /// [`Self::integration_runner`] under bounded concurrency. Held on
    /// the bundle so the eventual `run.rs` switch (and tests) can
    /// observe queue state directly without reaching into the tick.
    pub integration_dispatch: Arc<IntegrationDispatchQueue>,
    /// Bounded-concurrency consumer for [`Self::integration_dispatch`],
    /// pre-wired with `cfg.agent.max_concurrent_agents` capacity and
    /// the supplied [`IntegrationDispatcher`] (production: the
    /// integration-owner agent runner). The composition root drives
    /// [`IntegrationDispatchRunner::run_pending`] and
    /// [`IntegrationDispatchRunner::reap_completed`] on its own cadence;
    /// the scheduler tick fan only enqueues requests. The upstream
    /// [`IntegrationQueueTick`] applies workflow gates
    /// (`require_all_children_terminal`, `require_no_open_blockers`) at
    /// emission time, so every request the runner sees is authoritative.
    pub integration_runner: Arc<IntegrationDispatchRunner>,
    /// FIFO of pending [`symphony_core::QaDispatchRequest`]s emitted by
    /// the QA queue tick. Drained by [`Self::qa_runner`] under bounded
    /// concurrency. Held on the bundle so the eventual `run.rs` switch
    /// (and tests) can observe queue state directly without reaching
    /// into the tick.
    pub qa_dispatch: Arc<QaDispatchQueue>,
    /// Bounded-concurrency consumer for [`Self::qa_dispatch`], pre-wired
    /// with `cfg.agent.max_concurrent_agents` capacity and the supplied
    /// [`QaDispatcher`] (production: the QA-gate agent runner). The
    /// composition root drives [`QaDispatchRunner::run_pending`] and
    /// [`QaDispatchRunner::reap_completed`] on its own cadence; the
    /// scheduler tick fan only enqueues requests. The upstream
    /// [`QaQueueTick`] applies the workflow's [`QaGates`] at emission
    /// time, so every drained request is authoritative — the runner
    /// performs no additional gate check. Verdicts (pass / fail / waived
    /// / inconclusive) are signalled by the dispatcher's returned
    /// [`symphony_core::QaDispatchReason`].
    pub qa_runner: Arc<QaDispatchRunner>,
}

/// One in-flight claim recorded in [`ClaimedRunRegistry`].
///
/// Mirrors the columns the recovery tick needs from a future durable
/// `runs` row so the in-memory ledger and the eventual SQLite reader
/// surface identical [`ExpiredLeaseCandidate`] values without changing
/// the source contract.
#[derive(Debug, Clone)]
pub struct ClaimedRun {
    /// Durable run id. Stable claim key shared with the recovery tick.
    pub run_id: RecoveryRunId,
    /// Work item the run is operating on.
    pub work_item_id: WorkItemId,
    /// Lease holder identifier as it would appear in `runs.lease_owner`.
    pub lease_owner: String,
    /// RFC3339 lease expiration timestamp captured at claim time.
    pub lease_expires_at: String,
    /// Workspace claim associated with the run, if any.
    pub workspace_claim_id: Option<RecoveryWorkspaceClaimId>,
}

/// In-memory ledger of currently claimed runs keyed by tracker
/// [`IssueId`].
///
/// Stand-in for the durable `runs` table while the scheduler builder
/// runs without a state DB. The recovery tick treats every entry whose
/// `IssueId` is no longer in the shared [`ActiveSetStore`] as an
/// expired lease — the flat poll loop's reconciliation contract,
/// re-shaped into the v2 source signature.
///
/// Concurrency: a `std::sync::Mutex` is correct here because all
/// readers and writers are synchronous (the recovery source's
/// `list_*` methods are sync, and runners record/forget claims
/// outside an await). No awaits happen while the lock is held.
#[derive(Default, Debug)]
pub struct ClaimedRunRegistry {
    inner: StdMutex<HashMap<IssueId, ClaimedRun>>,
}

impl ClaimedRunRegistry {
    /// Construct an empty registry.
    pub fn new() -> Self {
        Self::default()
    }

    /// Record a claim under `issue`. Overwrites a prior entry with the
    /// same key — runners are expected to forget claims on completion,
    /// but a reclaim under the same `IssueId` is benign and matches the
    /// flat-loop ledger semantics.
    pub fn record(&self, issue: IssueId, claim: ClaimedRun) {
        self.inner
            .lock()
            .expect("claimed run registry mutex poisoned")
            .insert(issue, claim);
    }

    /// Drop the claim for `issue`, if present.
    pub fn forget(&self, issue: &IssueId) {
        self.inner
            .lock()
            .expect("claimed run registry mutex poisoned")
            .remove(issue);
    }

    /// Number of currently recorded claims.
    pub fn len(&self) -> usize {
        self.inner
            .lock()
            .expect("claimed run registry mutex poisoned")
            .len()
    }

    /// `true` iff the registry has no recorded claims.
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    fn snapshot(&self) -> Vec<(IssueId, ClaimedRun)> {
        self.inner
            .lock()
            .expect("claimed run registry mutex poisoned")
            .iter()
            .map(|(k, v)| (k.clone(), v.clone()))
            .collect()
    }
}

/// [`RecoveryQueueSource`] backed by [`ActiveSetStore`] +
/// [`ClaimedRunRegistry`].
///
/// For each recorded claim whose `IssueId` is no longer present in the
/// latest active-set snapshot, surface an [`ExpiredLeaseCandidate`].
/// Orphaned workspace claims are not derivable from the active set
/// alone (they require a durable view of `workspace_claims` independent
/// of `runs`) so the orphan list is empty until a durable source is
/// plumbed in. This matches the parity scope: the flat poll loop only
/// emitted `Reconciled { dropped }` events, which are the lease side.
struct ActiveSetRecoverySource {
    active_set: Arc<ActiveSetStore>,
    registry: Arc<ClaimedRunRegistry>,
}

impl RecoveryQueueSource for ActiveSetRecoverySource {
    fn list_expired_leases(&self) -> Result<Vec<ExpiredLeaseCandidate>, RecoveryQueueError> {
        let active_ids: HashSet<IssueId> = self
            .active_set
            .snapshot()
            .into_iter()
            .map(|i| i.id)
            .collect();
        let mut out: Vec<ExpiredLeaseCandidate> = self
            .registry
            .snapshot()
            .into_iter()
            .filter(|(id, _)| !active_ids.contains(id))
            .map(|(_, c)| ExpiredLeaseCandidate {
                run_id: c.run_id,
                work_item_id: c.work_item_id,
                lease_owner: c.lease_owner,
                lease_expires_at: c.lease_expires_at,
                workspace_claim_id: c.workspace_claim_id,
            })
            .collect();
        // Stable FIFO order by run id so the dispatch queue contents
        // are deterministic across HashMap iteration orders.
        out.sort_by_key(|c| c.run_id.get());
        Ok(out)
    }

    fn list_orphaned_workspace_claims(
        &self,
    ) -> Result<Vec<OrphanedWorkspaceClaimCandidate>, RecoveryQueueError> {
        Ok(Vec::new())
    }
}

/// Build a [`SchedulerV2`] with the intake, recovery, specialist, and
/// integration queue ticks wired up.
///
/// Cadence is mapped from [`WorkflowConfig::polling`]:
/// `interval_ms` → `SchedulerV2Config::interval`,
/// `jitter_ms` → `SchedulerV2Config::jitter`. Every tick reuses the
/// same cadence for now — there is no per-queue cadence config in
/// `WORKFLOW.md` yet, and SPEC v2 §5.2 names a single `polling.*` block.
///
/// The specialist tick reads the workflow's `routing.*` and `roles.*`
/// blocks: rules, default role, match mode, and per-role `kind` are
/// mirrored into the kernel-side [`RoutingEngine`] + [`RoleKindLookup`].
/// `dispatcher` is the per-issue [`Dispatcher`] (production:
/// `SymphonyDispatcher`) reused under the configured
/// `agent.max_concurrent_agents` capacity. The runner is exposed on the
/// returned bundle so the composition root can drive
/// `run_pending` / `reap_completed` on its own cadence — the scheduler
/// only enqueues dispatch requests.
///
/// The integration tick consults `integration_source` under
/// [`IntegrationGates`] mirrored from `cfg.integration.*`. Production
/// supplies a state-crate adapter over `IntegrationQueueRepository`;
/// tests pass a deterministic in-memory fake. Every emitted request is
/// pre-gated by the tick, so [`IntegrationDispatchRunner`] simply
/// invokes `integration_dispatcher` (production: the integration-owner
/// agent runner) under the configured `agent.max_concurrent_agents`
/// ceiling. Per-role caps will layer in once role-aware dispatch lands.
///
/// The QA tick consults `qa_source` under [`QaGates`] derived from
/// SPEC v2 §5.12 defaults (blocker gate on). `WorkflowConfig` does not
/// yet expose a per-workflow knob for the QA queue's blocker gate —
/// the SPEC default is the only supported value at this layer; a
/// future schema addition will mirror it the same way `cfg.integration`
/// flows into [`IntegrationGates`]. Production wires `qa_source` to a
/// state-crate adapter over `QaQueueRepository`; tests pass a
/// deterministic fake. Every emitted request is pre-gated by the tick,
/// so [`QaDispatchRunner`] simply invokes `qa_dispatcher` (production:
/// the QA-gate agent runner) under the configured
/// `agent.max_concurrent_agents` ceiling.
///
/// This function is intentionally additive: it does not register
/// follow-up approval / budget-pause ticks yet. Those come in
/// subsequent decomposition steps, which will extend this builder
/// rather than introduce a parallel one.
pub fn build_scheduler_v2(
    cfg: &WorkflowConfig,
    tracker: Arc<dyn TrackerRead>,
    dispatcher: Arc<dyn Dispatcher>,
    integration_source: Arc<dyn IntegrationQueueSource>,
    integration_dispatcher: Arc<dyn IntegrationDispatcher>,
    qa_source: Arc<dyn QaQueueSource>,
    qa_dispatcher: Arc<dyn QaDispatcher>,
) -> SchedulerV2Bundle {
    let cadence = QueueTickCadence::from_millis(cfg.polling.interval_ms, cfg.polling.jitter_ms);
    let scheduler_cfg = SchedulerV2Config::from_cadence(cadence);

    let active_set = Arc::new(ActiveSetStore::new());
    let intake: Box<dyn QueueTick> =
        Box::new(IntakeQueueTick::new(tracker, active_set.clone(), cadence));

    let claimed_runs = Arc::new(ClaimedRunRegistry::new());
    let recovery_dispatch = Arc::new(RecoveryDispatchQueue::new());
    let recovery_source = Arc::new(ActiveSetRecoverySource {
        active_set: active_set.clone(),
        registry: claimed_runs.clone(),
    }) as Arc<dyn RecoveryQueueSource>;
    let recovery: Box<dyn QueueTick> = Box::new(RecoveryQueueTick::new(
        recovery_source,
        recovery_dispatch.clone(),
        cadence,
    ));

    let specialist_dispatch = Arc::new(SpecialistDispatchQueue::new());
    let specialist: Box<dyn QueueTick> = Box::new(SpecialistQueueTick::new(
        active_set.clone(),
        routing_engine_from_config(cfg),
        role_kinds_from_config(cfg),
        specialist_dispatch.clone(),
        cadence,
    ));

    // SPEC v2 §5.5: `agent.max_concurrent_agents` is the global ceiling
    // on simultaneously running specialist sessions. Per-role caps live
    // on `RoleConfig::max_concurrent` and will layer in once role-aware
    // dispatch lands; until then the runner enforces the global cap.
    let max_concurrent = cfg.agent.max_concurrent_agents as usize;
    let specialist_runner = Arc::new(SpecialistRunner::new(
        specialist_dispatch.clone(),
        active_set.clone(),
        dispatcher,
        max_concurrent,
    ));

    let integration_dispatch = Arc::new(IntegrationDispatchQueue::new());
    let integration_gates = IntegrationGates {
        require_all_children_terminal: cfg.integration.require_all_children_terminal,
        require_no_open_blockers: cfg.integration.require_no_open_blockers,
    };
    let integration: Box<dyn QueueTick> = Box::new(IntegrationQueueTick::new(
        integration_source,
        integration_gates,
        integration_dispatch.clone(),
        cadence,
    ));
    let integration_runner = Arc::new(IntegrationDispatchRunner::new(
        integration_dispatch.clone(),
        integration_dispatcher,
        max_concurrent,
    ));

    let qa_dispatch = Arc::new(QaDispatchQueue::new());
    let qa_gates = QaGates::default();
    let qa: Box<dyn QueueTick> = Box::new(QaQueueTick::new(
        qa_source,
        qa_gates,
        qa_dispatch.clone(),
        cadence,
    ));
    let qa_runner = Arc::new(QaDispatchRunner::new(
        qa_dispatch.clone(),
        qa_dispatcher,
        max_concurrent,
    ));

    let scheduler = SchedulerV2::with_ticks(
        scheduler_cfg,
        vec![intake, recovery, specialist, integration, qa],
    );
    SchedulerV2Bundle {
        scheduler,
        active_set,
        claimed_runs,
        recovery_dispatch,
        specialist_dispatch,
        specialist_runner,
        integration_dispatch,
        integration_runner,
        qa_dispatch,
        qa_runner,
    }
}

/// Mirror `cfg.routing` into the kernel-side [`RoutingTable`].
///
/// The two crates intentionally maintain parallel `RoutingMatchMode` /
/// `RoutingRule` / `RoutingMatch` types so the kernel does not depend
/// on `symphony-config`. This converter is the seam that bridges them
/// at the composition root.
fn routing_engine_from_config(c: &WorkflowConfig) -> RoutingEngine {
    let table = RoutingTable {
        default_role: c
            .routing
            .default_role
            .as_ref()
            .map(|s| RoleName::from(s.as_str())),
        match_mode: match c.routing.match_mode {
            cfg::RoutingMatchMode::FirstMatch => RoutingMatchMode::FirstMatch,
            cfg::RoutingMatchMode::Priority => RoutingMatchMode::Priority,
        },
        rules: c
            .routing
            .rules
            .iter()
            .map(|r| RoutingRule {
                priority: r.priority,
                when: RoutingMatch {
                    labels_any: r.when.labels_any.clone(),
                    paths_any: r.when.paths_any.clone(),
                    issue_size: r.when.issue_size.clone(),
                },
                assign_role: RoleName::from(r.assign_role.as_str()),
            })
            .collect(),
    };
    RoutingEngine::new(table)
}

/// Mirror `cfg.roles` (name → kind) into the kernel-side
/// [`RoleKindLookup`] used by the specialist tick to skip kernel-special
/// roles (`integration_owner`, `qa_gate`).
fn role_kinds_from_config(c: &WorkflowConfig) -> RoleKindLookup {
    RoleKindLookup::from_pairs(c.roles.iter().map(|(name, role)| {
        let kind = match role.kind {
            cfg::RoleKind::IntegrationOwner => RoleKind::IntegrationOwner,
            cfg::RoleKind::QaGate => RoleKind::QaGate,
            cfg::RoleKind::Specialist => RoleKind::Specialist,
            cfg::RoleKind::Reviewer => RoleKind::Reviewer,
            cfg::RoleKind::Operator => RoleKind::Operator,
            cfg::RoleKind::Custom => RoleKind::Custom,
        };
        (name.clone(), kind)
    }))
}

#[cfg(test)]
mod tests {
    use super::*;
    use async_trait::async_trait;
    use std::sync::Mutex as StdMutex;
    use std::time::Duration;
    use symphony_config::{
        AgentConfig, PollingConfig, RoleConfig, RoutingConfig, RoutingMatch as CfgRoutingMatch,
        RoutingMatchMode as CfgRoutingMatchMode, RoutingRule as CfgRoutingRule,
    };
    use symphony_core::ReleaseReason;
    use symphony_core::tracker::{Issue, IssueId, IssueState};
    use symphony_core::tracker_trait::{TrackerError, TrackerResult};
    use symphony_core::{
        IntegrationCandidate, IntegrationDispatchReason, IntegrationDispatchRequest,
        IntegrationQueueError, IntegrationRequestCause, QaCandidate, QaDispatchReason,
        QaDispatchRequest, QaQueueError, QaRequestCause,
    };
    use tokio::sync::Notify;
    use tokio_util::sync::CancellationToken;

    /// No-op dispatcher used by tests that don't exercise the
    /// specialist runner directly. The builder requires a dispatcher
    /// since the specialist runner is wired unconditionally; supplying
    /// a no-op here keeps the existing intake/recovery parity tests
    /// independent of dispatcher behaviour.
    struct NoopDispatcher;

    #[async_trait]
    impl Dispatcher for NoopDispatcher {
        async fn dispatch(&self, _issue: Issue, _cancel: CancellationToken) -> ReleaseReason {
            ReleaseReason::Completed
        }
    }

    fn noop_dispatcher() -> Arc<dyn Dispatcher> {
        Arc::new(NoopDispatcher)
    }

    /// Empty integration source — never surfaces a candidate. Used by
    /// tests that only exercise intake/recovery/specialist behaviour.
    struct EmptyIntegrationSource;

    impl IntegrationQueueSource for EmptyIntegrationSource {
        fn list_ready(
            &self,
            _gates: IntegrationGates,
        ) -> Result<Vec<IntegrationCandidate>, IntegrationQueueError> {
            Ok(Vec::new())
        }
    }

    fn empty_integration_source() -> Arc<dyn IntegrationQueueSource> {
        Arc::new(EmptyIntegrationSource)
    }

    /// No-op integration dispatcher — would resolve every request as
    /// `Completed`. Used by tests that don't drive the integration runner.
    struct NoopIntegrationDispatcher;

    #[async_trait]
    impl IntegrationDispatcher for NoopIntegrationDispatcher {
        async fn dispatch(
            &self,
            _request: IntegrationDispatchRequest,
            _cancel: CancellationToken,
        ) -> IntegrationDispatchReason {
            IntegrationDispatchReason::Completed
        }
    }

    fn noop_integration_dispatcher() -> Arc<dyn IntegrationDispatcher> {
        Arc::new(NoopIntegrationDispatcher)
    }

    /// Gate-aware integration source: stores per-gate candidate lists,
    /// mirroring `IntegrationQueueRepository::list_ready_for_integration_with_gates`.
    /// The tick supplies the workflow's gates; this fake returns
    /// whichever list was pre-loaded for that exact gate combination.
    type GateKey = (bool, bool);

    #[derive(Default)]
    struct GateAwareIntegrationSource {
        by_gates: StdMutex<Vec<(GateKey, Vec<IntegrationCandidate>)>>,
    }

    impl GateAwareIntegrationSource {
        fn new() -> Self {
            Self::default()
        }

        fn set(&self, gates: IntegrationGates, candidates: Vec<IntegrationCandidate>) {
            let key = (
                gates.require_all_children_terminal,
                gates.require_no_open_blockers,
            );
            let mut by = self.by_gates.lock().unwrap();
            by.retain(|(k, _)| *k != key);
            by.push((key, candidates));
        }
    }

    impl IntegrationQueueSource for GateAwareIntegrationSource {
        fn list_ready(
            &self,
            gates: IntegrationGates,
        ) -> Result<Vec<IntegrationCandidate>, IntegrationQueueError> {
            let key = (
                gates.require_all_children_terminal,
                gates.require_no_open_blockers,
            );
            let by = self.by_gates.lock().unwrap();
            Ok(by
                .iter()
                .find(|(k, _)| *k == key)
                .map(|(_, v)| v.clone())
                .unwrap_or_default())
        }
    }

    /// Records every integration dispatch invocation and resolves with
    /// a configured reason. Mirrors the recording dispatchers in the
    /// runner-side tests.
    struct RecordingIntegrationDispatcher {
        calls: StdMutex<Vec<String>>,
        reason: IntegrationDispatchReason,
    }

    impl RecordingIntegrationDispatcher {
        fn new(reason: IntegrationDispatchReason) -> Self {
            Self {
                calls: StdMutex::new(Vec::new()),
                reason,
            }
        }
        fn calls(&self) -> Vec<String> {
            self.calls.lock().unwrap().clone()
        }
    }

    #[async_trait]
    impl IntegrationDispatcher for RecordingIntegrationDispatcher {
        async fn dispatch(
            &self,
            request: IntegrationDispatchRequest,
            _cancel: CancellationToken,
        ) -> IntegrationDispatchReason {
            self.calls
                .lock()
                .unwrap()
                .push(request.parent_identifier.clone());
            self.reason
        }
    }

    /// Empty QA source — never surfaces a candidate. Used by tests
    /// that only exercise other ticks/runners.
    struct EmptyQaSource;

    impl QaQueueSource for EmptyQaSource {
        fn list_ready(&self, _gates: QaGates) -> Result<Vec<QaCandidate>, QaQueueError> {
            Ok(Vec::new())
        }
    }

    fn empty_qa_source() -> Arc<dyn QaQueueSource> {
        Arc::new(EmptyQaSource)
    }

    /// No-op QA dispatcher — would resolve every request as `Passed`.
    /// Used by tests that don't drive the QA runner.
    struct NoopQaDispatcher;

    #[async_trait]
    impl QaDispatcher for NoopQaDispatcher {
        async fn dispatch(
            &self,
            _request: QaDispatchRequest,
            _cancel: CancellationToken,
        ) -> QaDispatchReason {
            QaDispatchReason::Passed
        }
    }

    fn noop_qa_dispatcher() -> Arc<dyn QaDispatcher> {
        Arc::new(NoopQaDispatcher)
    }

    /// Gate-aware QA source: stores per-gate candidate lists,
    /// mirroring `QaQueueRepository::list_ready_for_qa_with_gates`. The
    /// tick supplies the workflow's gates; this fake returns whichever
    /// list was pre-loaded for that exact gate value.
    #[derive(Default)]
    struct GateAwareQaSource {
        // require_no_open_blockers → list
        by_gates: StdMutex<Vec<(bool, Vec<QaCandidate>)>>,
    }

    impl GateAwareQaSource {
        fn new() -> Self {
            Self::default()
        }

        fn set(&self, gates: QaGates, candidates: Vec<QaCandidate>) {
            let key = gates.require_no_open_blockers;
            let mut by = self.by_gates.lock().unwrap();
            by.retain(|(k, _)| *k != key);
            by.push((key, candidates));
        }
    }

    impl QaQueueSource for GateAwareQaSource {
        fn list_ready(&self, gates: QaGates) -> Result<Vec<QaCandidate>, QaQueueError> {
            let key = gates.require_no_open_blockers;
            let by = self.by_gates.lock().unwrap();
            Ok(by
                .iter()
                .find(|(k, _)| *k == key)
                .map(|(_, v)| v.clone())
                .unwrap_or_default())
        }
    }

    /// Records every QA dispatch invocation and resolves with a
    /// configured reason. Mirrors the recording dispatchers in the
    /// runner-side tests.
    struct RecordingQaDispatcher {
        calls: StdMutex<Vec<String>>,
        reason: QaDispatchReason,
    }

    impl RecordingQaDispatcher {
        fn new(reason: QaDispatchReason) -> Self {
            Self {
                calls: StdMutex::new(Vec::new()),
                reason,
            }
        }
        fn calls(&self) -> Vec<String> {
            self.calls.lock().unwrap().clone()
        }
    }

    #[async_trait]
    impl QaDispatcher for RecordingQaDispatcher {
        async fn dispatch(
            &self,
            request: QaDispatchRequest,
            _cancel: CancellationToken,
        ) -> QaDispatchReason {
            self.calls.lock().unwrap().push(request.identifier.clone());
            self.reason
        }
    }

    fn qa_candidate(id: i64, identifier: &str, cause: QaRequestCause) -> QaCandidate {
        QaCandidate {
            work_item_id: WorkItemId::new(id),
            identifier: identifier.into(),
            title: format!("title {identifier}"),
            cause,
        }
    }

    async fn wait_for_qa_outcomes(
        runner: &QaDispatchRunner,
        n: usize,
    ) -> Vec<symphony_core::QaDispatchOutcome> {
        let deadline = std::time::Instant::now() + Duration::from_secs(2);
        let mut acc = Vec::new();
        while acc.len() < n {
            acc.extend(runner.reap_completed().await);
            if acc.len() >= n {
                break;
            }
            if std::time::Instant::now() >= deadline {
                panic!(
                    "timed out waiting for {n} qa outcomes; got {} so far",
                    acc.len()
                );
            }
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
        acc
    }

    fn integration_candidate(
        id: i64,
        identifier: &str,
        cause: IntegrationRequestCause,
    ) -> IntegrationCandidate {
        IntegrationCandidate {
            parent_id: WorkItemId::new(id),
            parent_identifier: identifier.into(),
            parent_title: format!("title {identifier}"),
            cause,
        }
    }

    async fn wait_for_integration_outcomes(
        runner: &IntegrationDispatchRunner,
        n: usize,
    ) -> Vec<symphony_core::IntegrationDispatchOutcome> {
        let deadline = std::time::Instant::now() + Duration::from_secs(2);
        let mut acc = Vec::new();
        while acc.len() < n {
            acc.extend(runner.reap_completed().await);
            if acc.len() >= n {
                break;
            }
            if std::time::Instant::now() >= deadline {
                panic!(
                    "timed out waiting for {n} integration outcomes; got {} so far",
                    acc.len()
                );
            }
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
        acc
    }

    /// Records every dispatch invocation and resolves with a configured
    /// [`ReleaseReason`]. Optional gate so capacity tests can keep
    /// dispatchers pending until released.
    struct RecordingDispatcher {
        calls: StdMutex<Vec<String>>,
        gate: Notify,
        gate_enabled: StdMutex<bool>,
        reason: ReleaseReason,
    }

    impl RecordingDispatcher {
        fn new(reason: ReleaseReason) -> Self {
            Self {
                calls: StdMutex::new(Vec::new()),
                gate: Notify::new(),
                gate_enabled: StdMutex::new(false),
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

    /// Minimal `TrackerRead` returning a fixed snapshot. Mirrors the
    /// shape used in `intake_tick`'s own tests so we exercise the
    /// builder against the same contract.
    struct StaticTracker {
        active: StdMutex<Vec<Issue>>,
    }

    impl StaticTracker {
        fn new(active: Vec<Issue>) -> Self {
            Self {
                active: StdMutex::new(active),
            }
        }

        fn set_active(&self, active: Vec<Issue>) {
            *self.active.lock().unwrap() = active;
        }
    }

    #[async_trait]
    impl TrackerRead for StaticTracker {
        async fn fetch_active(&self) -> TrackerResult<Vec<Issue>> {
            Ok(self.active.lock().unwrap().clone())
        }
        async fn fetch_state(&self, ids: &[IssueId]) -> TrackerResult<Vec<Issue>> {
            Ok(ids
                .iter()
                .map(|id| Issue::minimal(id.as_str(), id.as_str(), "stub", "Todo"))
                .collect())
        }
        async fn fetch_terminal_recent(
            &self,
            _terminal: &[IssueState],
        ) -> TrackerResult<Vec<Issue>> {
            Err(TrackerError::Transport("unused".into()))
        }
    }

    fn issues(n: usize) -> Vec<Issue> {
        (0..n)
            .map(|i| Issue::minimal(format!("id-{i}"), format!("ENG-{i}"), "title", "Todo"))
            .collect()
    }

    fn cfg_with(interval_ms: u64, jitter_ms: u64) -> WorkflowConfig {
        WorkflowConfig {
            polling: PollingConfig {
                interval_ms,
                jitter_ms,
                ..PollingConfig::default()
            },
            ..WorkflowConfig::default()
        }
    }

    /// Build a workflow config that routes every issue with the given
    /// label to a single specialist role, capped at `max_concurrent`
    /// simultaneous dispatches. Used by the specialist-runner tests so
    /// the tick actually emits `SpecialistDispatchRequest`s.
    fn cfg_with_specialist(
        interval_ms: u64,
        max_concurrent: u32,
        label: &str,
        role: &str,
    ) -> WorkflowConfig {
        let mut roles = std::collections::BTreeMap::new();
        roles.insert(role.to_string(), specialist_role());
        WorkflowConfig {
            polling: PollingConfig {
                interval_ms,
                jitter_ms: 0,
                ..PollingConfig::default()
            },
            agent: AgentConfig {
                max_concurrent_agents: max_concurrent,
                ..AgentConfig::default()
            },
            roles,
            routing: RoutingConfig {
                default_role: None,
                match_mode: CfgRoutingMatchMode::FirstMatch,
                rules: vec![CfgRoutingRule {
                    priority: None,
                    when: CfgRoutingMatch {
                        labels_any: vec![label.to_string()],
                        paths_any: Vec::new(),
                        issue_size: None,
                    },
                    assign_role: role.to_string(),
                }],
            },
            ..WorkflowConfig::default()
        }
    }

    fn specialist_role() -> RoleConfig {
        RoleConfig {
            kind: cfg::RoleKind::Specialist,
            description: None,
            agent: None,
            max_concurrent: None,
            can_decompose: None,
            can_assign: None,
            can_request_qa: None,
            can_close_parent: None,
            can_file_blockers: None,
            can_file_followups: None,
            required_for_done: None,
        }
    }

    fn issue_with_label(id: &str, identifier: &str, label: &str) -> Issue {
        let mut i = Issue::minimal(id, identifier, "title", "Todo");
        i.labels = vec![label.to_string()];
        i
    }

    #[tokio::test]
    async fn builder_registers_intake_recovery_and_specialist_ticks() {
        let cfg = cfg_with(50, 0);
        let tracker = Arc::new(StaticTracker::new(issues(2))) as Arc<dyn TrackerRead>;
        let bundle = build_scheduler_v2(
            &cfg,
            tracker,
            noop_dispatcher(),
            empty_integration_source(),
            noop_integration_dispatcher(),
            empty_qa_source(),
            noop_qa_dispatcher(),
        );
        // Intake + recovery + specialist + integration + QA;
        // follow-up approval / budget-pause ticks layer on in
        // subsequent steps.
        assert_eq!(bundle.scheduler.tick_count(), 5);
        assert!(bundle.claimed_runs.is_empty());
        assert!(bundle.recovery_dispatch.is_empty());
        assert!(bundle.specialist_dispatch.is_empty());
        assert!(bundle.integration_dispatch.is_empty());
        assert!(bundle.qa_dispatch.is_empty());
        assert_eq!(
            bundle.qa_runner.max_concurrent(),
            cfg.agent.max_concurrent_agents as usize,
        );
        assert_eq!(
            bundle.specialist_runner.max_concurrent(),
            cfg.agent.max_concurrent_agents as usize,
        );
        assert_eq!(
            bundle.integration_runner.max_concurrent(),
            cfg.agent.max_concurrent_agents as usize,
        );
    }

    #[tokio::test]
    async fn builder_maps_polling_config_into_scheduler_cadence() {
        let cfg = cfg_with(123, 45);
        let tracker = Arc::new(StaticTracker::new(Vec::new())) as Arc<dyn TrackerRead>;
        let bundle = build_scheduler_v2(
            &cfg,
            tracker,
            noop_dispatcher(),
            empty_integration_source(),
            noop_integration_dispatcher(),
            empty_qa_source(),
            noop_qa_dispatcher(),
        );
        let sc = bundle.scheduler.config();
        assert_eq!(sc.interval, Duration::from_millis(123));
        assert_eq!(sc.jitter, Duration::from_millis(45));
    }

    #[tokio::test]
    async fn tick_once_publishes_active_set_into_shared_store() {
        let cfg = cfg_with(10, 0);
        let tracker = Arc::new(StaticTracker::new(issues(3))) as Arc<dyn TrackerRead>;
        let bundle = build_scheduler_v2(
            &cfg,
            tracker,
            noop_dispatcher(),
            empty_integration_source(),
            noop_integration_dispatcher(),
            empty_qa_source(),
            noop_qa_dispatcher(),
        );
        let SchedulerV2Bundle {
            mut scheduler,
            active_set,
            ..
        } = bundle;

        assert!(active_set.is_empty());
        let report = scheduler.tick_once().await;
        // intake + recovery + specialist + integration + qa
        assert_eq!(report.outcomes.len(), 5);
        assert_eq!(report.outcomes[0].processed, 3);
        assert_eq!(active_set.len(), 3);
        assert_eq!(
            active_set
                .snapshot()
                .iter()
                .map(|i| i.identifier.clone())
                .collect::<Vec<_>>(),
            vec!["ENG-0", "ENG-1", "ENG-2"],
        );
    }

    #[tokio::test]
    async fn run_advances_under_configured_cadence_until_cancelled() {
        let cfg = cfg_with(2, 0);
        let tracker = Arc::new(StaticTracker::new(issues(1))) as Arc<dyn TrackerRead>;
        let bundle = build_scheduler_v2(
            &cfg,
            tracker,
            noop_dispatcher(),
            empty_integration_source(),
            noop_integration_dispatcher(),
            empty_qa_source(),
            noop_qa_dispatcher(),
        );
        let SchedulerV2Bundle {
            scheduler,
            active_set,
            ..
        } = bundle;

        let cancel = CancellationToken::new();
        let cancel_clone = cancel.clone();
        let handle = tokio::spawn(async move { scheduler.run(cancel_clone).await });

        // Wait until the intake tick has populated the store at least
        // once — proves `run` is actually driving ticks under cadence.
        for _ in 0..200 {
            if active_set.len() == 1 {
                break;
            }
            tokio::time::sleep(Duration::from_millis(2)).await;
        }
        assert_eq!(active_set.len(), 1);

        cancel.cancel();
        tokio::time::timeout(Duration::from_secs(1), handle)
            .await
            .expect("scheduler exited promptly after cancel")
            .expect("no panic");
    }

    fn claim(run: i64, work: i64) -> ClaimedRun {
        ClaimedRun {
            run_id: RecoveryRunId::new(run),
            work_item_id: WorkItemId::new(work),
            lease_owner: format!("worker-{run}"),
            lease_expires_at: "2026-05-08T00:00:00Z".into(),
            workspace_claim_id: None,
        }
    }

    fn dropped_run_ids(reqs: &[symphony_core::RecoveryDispatchRequest]) -> Vec<i64> {
        let mut out: Vec<i64> = reqs
            .iter()
            .filter_map(|r| match r {
                symphony_core::RecoveryDispatchRequest::ExpiredLease { run_id, .. } => {
                    Some(run_id.get())
                }
                _ => None,
            })
            .collect();
        out.sort();
        out
    }

    /// Parity scenario: when the tracker drops issues from the active
    /// set, the recovery tick surfaces an `ExpiredLease` for each
    /// claimed run whose issue id left the set — the v2 analogue of
    /// the flat poll loop's `Reconciled { dropped }` event.
    #[tokio::test]
    async fn recovery_tick_surfaces_expired_lease_when_issue_leaves_active_set() {
        let cfg = cfg_with(10, 0);
        let tracker_inner = Arc::new(StaticTracker::new(issues(3)));
        let bundle = build_scheduler_v2(
            &cfg,
            tracker_inner.clone() as Arc<dyn TrackerRead>,
            noop_dispatcher(),
            empty_integration_source(),
            noop_integration_dispatcher(),
            empty_qa_source(),
            noop_qa_dispatcher(),
        );
        let SchedulerV2Bundle {
            mut scheduler,
            claimed_runs,
            recovery_dispatch,
            ..
        } = bundle;

        // Three in-flight claims, one per issue id-0..id-2.
        claimed_runs.record(IssueId::new("id-0"), claim(101, 1));
        claimed_runs.record(IssueId::new("id-1"), claim(102, 2));
        claimed_runs.record(IssueId::new("id-2"), claim(103, 3));

        // Tick 1: full active set → no reconciliation.
        scheduler.tick_once().await;
        assert!(
            recovery_dispatch.is_empty(),
            "no claims should be reconciled while every issue is still active",
        );

        // Drop id-1 and id-2 from the tracker's active set.
        tracker_inner.set_active(vec![Issue::minimal("id-0", "ENG-0", "title", "Todo")]);

        // Tick 2: intake republishes the smaller set; recovery emits
        // expired-lease requests for id-1 and id-2.
        scheduler.tick_once().await;
        let drained = recovery_dispatch.drain();
        assert_eq!(drained.len(), 2);
        assert_eq!(dropped_run_ids(&drained), vec![102, 103]);

        // Tick 3: nothing new — already-claimed candidates stay
        // de-duped inside the recovery tick until the registry
        // forgets them.
        scheduler.tick_once().await;
        assert!(recovery_dispatch.is_empty());
    }

    /// Parity scenario: forgetting a claim before it leaves the active
    /// set never surfaces a reconciliation — runners that complete
    /// cleanly must not show up in recovery output.
    #[tokio::test]
    async fn recovery_tick_skips_claims_that_were_forgotten_before_leaving_active_set() {
        let cfg = cfg_with(10, 0);
        let tracker_inner = Arc::new(StaticTracker::new(issues(2)));
        let bundle = build_scheduler_v2(
            &cfg,
            tracker_inner.clone() as Arc<dyn TrackerRead>,
            noop_dispatcher(),
            empty_integration_source(),
            noop_integration_dispatcher(),
            empty_qa_source(),
            noop_qa_dispatcher(),
        );
        let SchedulerV2Bundle {
            mut scheduler,
            claimed_runs,
            recovery_dispatch,
            ..
        } = bundle;

        claimed_runs.record(IssueId::new("id-0"), claim(201, 1));
        claimed_runs.record(IssueId::new("id-1"), claim(202, 2));

        // Runner completes id-0 cleanly, then the tracker drops both
        // issues from the active set on the same cadence.
        claimed_runs.forget(&IssueId::new("id-0"));
        tracker_inner.set_active(Vec::new());

        scheduler.tick_once().await;
        let drained = recovery_dispatch.drain();
        assert_eq!(dropped_run_ids(&drained), vec![202]);
    }

    /// Parity scenario: re-adding an issue to the active set after
    /// reconciliation must allow a fresh claim to reconcile again
    /// later — matching the flat poll loop, which re-claims on the
    /// next tick once the issue reappears and then disappears again.
    #[tokio::test]
    async fn recovery_tick_re_emits_after_claim_replayed_for_returning_issue() {
        let cfg = cfg_with(10, 0);
        let tracker_inner = Arc::new(StaticTracker::new(issues(1)));
        let bundle = build_scheduler_v2(
            &cfg,
            tracker_inner.clone() as Arc<dyn TrackerRead>,
            noop_dispatcher(),
            empty_integration_source(),
            noop_integration_dispatcher(),
            empty_qa_source(),
            noop_qa_dispatcher(),
        );
        let SchedulerV2Bundle {
            mut scheduler,
            claimed_runs,
            recovery_dispatch,
            ..
        } = bundle;

        // Claim, drop, reconcile.
        claimed_runs.record(IssueId::new("id-0"), claim(301, 1));
        tracker_inner.set_active(Vec::new());
        scheduler.tick_once().await;
        assert_eq!(dropped_run_ids(&recovery_dispatch.drain()), vec![301]);

        // Issue returns; runner claims with a fresh run id; issue
        // leaves again; reconciliation must fire for the new run.
        tracker_inner.set_active(issues(1));
        // Forgetting + recording models the runner observing the
        // returned issue and starting a new run — exactly what the
        // specialist runner will do in a later step.
        claimed_runs.forget(&IssueId::new("id-0"));
        scheduler.tick_once().await; // pulls issue back into active set
        claimed_runs.record(IssueId::new("id-0"), claim(302, 1));
        tracker_inner.set_active(Vec::new());
        scheduler.tick_once().await;
        assert_eq!(dropped_run_ids(&recovery_dispatch.drain()), vec![302]);
    }

    // ---------------------------------------------------------------------
    // Specialist tick + runner end-to-end coverage.
    //
    // Each scenario builds the scheduler with a routing/role config that
    // routes a labelled issue to a specialist role, ticks the scheduler
    // (which fans the specialist tick → enqueues a dispatch request),
    // then drives the runner exposed on the bundle. The runner is the
    // load-bearing piece this checklist step adds.
    // ---------------------------------------------------------------------

    async fn tracker_with_labelled(
        active: Vec<Issue>,
    ) -> (Arc<StaticTracker>, Arc<dyn TrackerRead>) {
        let inner = Arc::new(StaticTracker::new(active));
        let dyn_ref: Arc<dyn TrackerRead> = inner.clone();
        (inner, dyn_ref)
    }

    /// Specialist runner runs requests under capacity, parks the rest
    /// across passes, and never exceeds the configured ceiling.
    #[tokio::test]
    async fn specialist_runner_defers_requests_when_capacity_saturated() {
        let cfg = cfg_with_specialist(10, 1, "frontend", "frontend");
        let (_t, tracker) = tracker_with_labelled(vec![
            issue_with_label("id-1", "ENG-1", "frontend"),
            issue_with_label("id-2", "ENG-2", "frontend"),
            issue_with_label("id-3", "ENG-3", "frontend"),
        ])
        .await;
        let dispatcher = Arc::new(RecordingDispatcher::new(ReleaseReason::Completed));
        dispatcher.enable_gate(); // hold every dispatch open

        let bundle = build_scheduler_v2(
            &cfg,
            tracker,
            dispatcher.clone() as Arc<dyn Dispatcher>,
            empty_integration_source(),
            noop_integration_dispatcher(),
            empty_qa_source(),
            noop_qa_dispatcher(),
        );
        let SchedulerV2Bundle {
            mut scheduler,
            specialist_dispatch,
            specialist_runner,
            ..
        } = bundle;

        // Tick the scheduler: intake publishes active set, specialist
        // tick enqueues 3 dispatch requests.
        scheduler.tick_once().await;
        assert_eq!(specialist_dispatch.len(), 3);

        // Pass 1: capacity = 1 → spawn 1, park 2.
        let report = specialist_runner
            .run_pending(CancellationToken::new())
            .await;
        assert_eq!(report.spawned, 1);
        assert_eq!(report.deferred_capacity, 2);
        assert_eq!(report.missing_issue, 0);
        assert_eq!(specialist_runner.deferred_count().await, 2);
        assert_eq!(specialist_runner.inflight_count().await, 1);

        // Release the gate so the in-flight dispatcher completes; reap
        // the outcome and verify the next pass spawns the next deferred.
        dispatcher.release_gate();
        let _ = wait_for_outcomes(&specialist_runner, 1).await;

        dispatcher.enable_gate();
        let r2 = specialist_runner
            .run_pending(CancellationToken::new())
            .await;
        assert_eq!(r2.spawned, 1);
        assert_eq!(r2.deferred_capacity, 1);
        dispatcher.release_gate();
        let _ = wait_for_outcomes(&specialist_runner, 1).await;

        let r3 = specialist_runner
            .run_pending(CancellationToken::new())
            .await;
        assert_eq!(r3.spawned, 1);
        assert_eq!(r3.deferred_capacity, 0);
        let _ = wait_for_outcomes(&specialist_runner, 1).await;

        let mut all = dispatcher.calls();
        all.sort();
        assert_eq!(all, vec!["ENG-1", "ENG-2", "ENG-3"]);
    }

    /// Cancelling the parent token tears down in-flight dispatchers
    /// cooperatively — the spawned future observes a cancelled child
    /// token and returns [`ReleaseReason::Canceled`].
    #[tokio::test]
    async fn specialist_runner_propagates_parent_cancellation_to_dispatch() {
        let cfg = cfg_with_specialist(10, 4, "frontend", "frontend");
        let (_t, tracker) =
            tracker_with_labelled(vec![issue_with_label("id-1", "ENG-1", "frontend")]).await;
        let dispatcher = Arc::new(RecordingDispatcher::new(ReleaseReason::Completed));
        dispatcher.enable_gate(); // hold the dispatch open

        let bundle = build_scheduler_v2(
            &cfg,
            tracker,
            dispatcher.clone() as Arc<dyn Dispatcher>,
            empty_integration_source(),
            noop_integration_dispatcher(),
            empty_qa_source(),
            noop_qa_dispatcher(),
        );
        let SchedulerV2Bundle {
            mut scheduler,
            specialist_runner,
            ..
        } = bundle;

        scheduler.tick_once().await;

        let cancel = CancellationToken::new();
        let report = specialist_runner.run_pending(cancel.clone()).await;
        assert_eq!(report.spawned, 1);
        assert_eq!(specialist_runner.inflight_count().await, 1);

        cancel.cancel();
        let outcomes = wait_for_outcomes(&specialist_runner, 1).await;
        assert_eq!(outcomes.len(), 1);
        assert_eq!(outcomes[0].identifier, "ENG-1");
        assert_eq!(outcomes[0].release_reason, ReleaseReason::Canceled);
    }

    /// `reap_completed` returns one outcome per finished dispatcher and
    /// leaves still-running tasks in place — observability surface the
    /// composition root will poll on its own cadence.
    #[tokio::test]
    async fn specialist_runner_reap_completed_returns_finished_outcomes() {
        let cfg = cfg_with_specialist(10, 4, "frontend", "frontend");
        let (_t, tracker) = tracker_with_labelled(vec![
            issue_with_label("id-1", "ENG-1", "frontend"),
            issue_with_label("id-2", "ENG-2", "frontend"),
        ])
        .await;
        let dispatcher = Arc::new(RecordingDispatcher::new(ReleaseReason::Completed));

        let bundle = build_scheduler_v2(
            &cfg,
            tracker,
            dispatcher.clone() as Arc<dyn Dispatcher>,
            empty_integration_source(),
            noop_integration_dispatcher(),
            empty_qa_source(),
            noop_qa_dispatcher(),
        );
        let SchedulerV2Bundle {
            mut scheduler,
            specialist_dispatch,
            specialist_runner,
            ..
        } = bundle;

        scheduler.tick_once().await;
        assert_eq!(specialist_dispatch.len(), 2);

        let report = specialist_runner
            .run_pending(CancellationToken::new())
            .await;
        assert_eq!(report.spawned, 2);

        let outcomes = wait_for_outcomes(&specialist_runner, 2).await;
        let mut ids: Vec<String> = outcomes.iter().map(|o| o.identifier.clone()).collect();
        ids.sort();
        assert_eq!(ids, vec!["ENG-1".to_string(), "ENG-2".to_string()]);
        assert_eq!(specialist_runner.inflight_count().await, 0);

        // Calling reap again is idempotent — nothing pending, returns
        // empty.
        assert!(specialist_runner.reap_completed().await.is_empty());
    }

    /// Wait until at least `n` outcomes have been reaped or a generous
    /// per-test timeout fires.
    async fn wait_for_outcomes(
        runner: &SpecialistRunner,
        n: usize,
    ) -> Vec<symphony_core::SpecialistDispatchOutcome> {
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

    // ---------------------------------------------------------------------
    // Integration tick + runner end-to-end coverage.
    //
    // Each scenario builds the scheduler with a `GateAwareIntegrationSource`
    // pre-loaded for the workflow's [`IntegrationGates`], ticks the
    // scheduler (which fans the integration tick → enqueues a dispatch
    // request when the gates allow it), then drives the runner exposed
    // on the bundle. The runner is the load-bearing piece this checklist
    // step adds.
    // ---------------------------------------------------------------------

    fn cfg_with_integration_gates(
        require_all_children_terminal: bool,
        require_no_open_blockers: bool,
    ) -> WorkflowConfig {
        let mut c = cfg_with(10, 0);
        c.integration.require_all_children_terminal = require_all_children_terminal;
        c.integration.require_no_open_blockers = require_no_open_blockers;
        c
    }

    /// Ready: source surfaces a candidate under the workflow's default
    /// gates → the integration tick enqueues a dispatch request → the
    /// runner invokes the dispatcher and reports `Completed`.
    #[tokio::test]
    async fn builder_dispatches_integration_candidates_when_ready() {
        let cfg = cfg_with_integration_gates(true, true);
        let tracker = Arc::new(StaticTracker::new(Vec::new())) as Arc<dyn TrackerRead>;
        let source = Arc::new(GateAwareIntegrationSource::new());
        source.set(
            IntegrationGates::default(),
            vec![integration_candidate(
                42,
                "PROJ-42",
                IntegrationRequestCause::AllChildrenTerminal,
            )],
        );
        let dispatcher = Arc::new(RecordingIntegrationDispatcher::new(
            IntegrationDispatchReason::Completed,
        ));

        let bundle = build_scheduler_v2(
            &cfg,
            tracker,
            noop_dispatcher(),
            source.clone() as Arc<dyn IntegrationQueueSource>,
            dispatcher.clone() as Arc<dyn IntegrationDispatcher>,
            empty_qa_source(),
            noop_qa_dispatcher(),
        );
        let SchedulerV2Bundle {
            mut scheduler,
            integration_dispatch,
            integration_runner,
            ..
        } = bundle;

        scheduler.tick_once().await;
        assert_eq!(integration_dispatch.len(), 1);

        let report = integration_runner
            .run_pending(CancellationToken::new())
            .await;
        assert_eq!(report.spawned, 1);
        assert_eq!(report.deferred_capacity, 0);

        let outcomes = wait_for_integration_outcomes(&integration_runner, 1).await;
        assert_eq!(outcomes.len(), 1);
        assert_eq!(outcomes[0].parent_id, WorkItemId::new(42));
        assert_eq!(outcomes[0].parent_identifier, "PROJ-42");
        assert_eq!(outcomes[0].reason, IntegrationDispatchReason::Completed);
        assert_eq!(dispatcher.calls(), vec!["PROJ-42"]);
    }

    /// Blocked: source returns nothing under default gates (e.g. a
    /// subtree blocker keeps the parent suppressed upstream) → the
    /// integration tick emits no request and the runner has nothing to
    /// dispatch.
    #[tokio::test]
    async fn builder_emits_nothing_when_subtree_blocked() {
        let cfg = cfg_with_integration_gates(true, true);
        let tracker = Arc::new(StaticTracker::new(Vec::new())) as Arc<dyn TrackerRead>;
        let source = Arc::new(GateAwareIntegrationSource::new());
        // Default gates: empty list models "subtree blocked upstream".
        source.set(IntegrationGates::default(), Vec::new());
        // The waived view *would* surface a candidate, but the workflow
        // is configured with the default (gates-on) view so the tick
        // must never see it.
        source.set(
            IntegrationGates {
                require_all_children_terminal: true,
                require_no_open_blockers: false,
            },
            vec![integration_candidate(
                7,
                "PROJ-7",
                IntegrationRequestCause::DirectIntegrationRequest,
            )],
        );
        let dispatcher = Arc::new(RecordingIntegrationDispatcher::new(
            IntegrationDispatchReason::Completed,
        ));

        let bundle = build_scheduler_v2(
            &cfg,
            tracker,
            noop_dispatcher(),
            source.clone() as Arc<dyn IntegrationQueueSource>,
            dispatcher.clone() as Arc<dyn IntegrationDispatcher>,
            empty_qa_source(),
            noop_qa_dispatcher(),
        );
        let SchedulerV2Bundle {
            mut scheduler,
            integration_dispatch,
            integration_runner,
            ..
        } = bundle;

        scheduler.tick_once().await;
        assert!(integration_dispatch.is_empty());

        let report = integration_runner
            .run_pending(CancellationToken::new())
            .await;
        assert!(report.is_idle());
        assert!(dispatcher.calls().is_empty());
    }

    /// Waived: workflow flips `require_no_open_blockers` to `false`. The
    /// integration tick now reads the waived view and surfaces a parent
    /// that the default-gates view would have suppressed → the runner
    /// dispatches it. Demonstrates the workflow gate config flowing
    /// end-to-end into the kernel-side [`IntegrationGates`].
    #[tokio::test]
    async fn builder_honors_waived_blocker_gate_from_workflow_config() {
        let cfg = cfg_with_integration_gates(true, false);
        let tracker = Arc::new(StaticTracker::new(Vec::new())) as Arc<dyn TrackerRead>;
        let source = Arc::new(GateAwareIntegrationSource::new());
        // Default gates: nothing (proves we're not falling back to the
        // default view by accident).
        source.set(IntegrationGates::default(), Vec::new());
        // Waived blocker gate: one parent surfaces.
        source.set(
            IntegrationGates {
                require_all_children_terminal: true,
                require_no_open_blockers: false,
            },
            vec![integration_candidate(
                7,
                "PROJ-7",
                IntegrationRequestCause::DirectIntegrationRequest,
            )],
        );
        let dispatcher = Arc::new(RecordingIntegrationDispatcher::new(
            IntegrationDispatchReason::Completed,
        ));

        let bundle = build_scheduler_v2(
            &cfg,
            tracker,
            noop_dispatcher(),
            source.clone() as Arc<dyn IntegrationQueueSource>,
            dispatcher.clone() as Arc<dyn IntegrationDispatcher>,
            empty_qa_source(),
            noop_qa_dispatcher(),
        );
        let SchedulerV2Bundle {
            mut scheduler,
            integration_dispatch,
            integration_runner,
            ..
        } = bundle;

        scheduler.tick_once().await;
        assert_eq!(integration_dispatch.len(), 1);

        let report = integration_runner
            .run_pending(CancellationToken::new())
            .await;
        assert_eq!(report.spawned, 1);

        let outcomes = wait_for_integration_outcomes(&integration_runner, 1).await;
        assert_eq!(outcomes.len(), 1);
        assert_eq!(outcomes[0].parent_id, WorkItemId::new(7));
        assert_eq!(dispatcher.calls(), vec!["PROJ-7"]);
    }

    // ---------------------------------------------------------------------
    // QA tick + runner end-to-end coverage.
    //
    // Each scenario builds the scheduler with a `GateAwareQaSource`
    // pre-loaded for the workflow's [`QaGates`], ticks the scheduler
    // (which fans the QA tick → enqueues a dispatch request when the
    // gates allow it), then drives the runner exposed on the bundle.
    // The runner is the load-bearing piece this checklist step adds.
    //
    // `WorkflowConfig` does not yet expose a per-workflow knob for the
    // QA blocker gate — the builder always uses [`QaGates::default()`]
    // (SPEC v2 §5.12 default: blocker gate on). The "waiver-routed
    // verdict" coverage therefore exercises the dispatcher returning
    // [`QaDispatchReason::Waived`] for an emitted request, which is
    // how a `qa.waiver_roles`-authorised role is observed at this
    // surface.
    // ---------------------------------------------------------------------

    /// Ready: source surfaces a candidate under the workflow's default
    /// gates → the QA tick enqueues a dispatch request → the runner
    /// invokes the dispatcher and reports `Passed`.
    #[tokio::test]
    async fn builder_dispatches_qa_candidates_when_ready() {
        let cfg = cfg_with(10, 0);
        let tracker = Arc::new(StaticTracker::new(Vec::new())) as Arc<dyn TrackerRead>;
        let source = Arc::new(GateAwareQaSource::new());
        source.set(
            QaGates::default(),
            vec![qa_candidate(
                42,
                "PROJ-42",
                QaRequestCause::IntegrationConsolidated,
            )],
        );
        let dispatcher = Arc::new(RecordingQaDispatcher::new(QaDispatchReason::Passed));

        let bundle = build_scheduler_v2(
            &cfg,
            tracker,
            noop_dispatcher(),
            empty_integration_source(),
            noop_integration_dispatcher(),
            source.clone() as Arc<dyn QaQueueSource>,
            dispatcher.clone() as Arc<dyn QaDispatcher>,
        );
        let SchedulerV2Bundle {
            mut scheduler,
            qa_dispatch,
            qa_runner,
            ..
        } = bundle;

        scheduler.tick_once().await;
        assert_eq!(qa_dispatch.len(), 1);

        let report = qa_runner.run_pending(CancellationToken::new()).await;
        assert_eq!(report.spawned, 1);
        assert_eq!(report.deferred_capacity, 0);

        let outcomes = wait_for_qa_outcomes(&qa_runner, 1).await;
        assert_eq!(outcomes.len(), 1);
        assert_eq!(outcomes[0].work_item_id, WorkItemId::new(42));
        assert_eq!(outcomes[0].identifier, "PROJ-42");
        assert_eq!(outcomes[0].reason, QaDispatchReason::Passed);
        assert_eq!(dispatcher.calls(), vec!["PROJ-42"]);
    }

    /// Blocked: source returns nothing under default gates (e.g. an
    /// open subtree blocker keeps QA suppressed upstream) → the QA
    /// tick emits no request and the runner has nothing to dispatch.
    #[tokio::test]
    async fn builder_emits_no_qa_request_when_subtree_blocked() {
        let cfg = cfg_with(10, 0);
        let tracker = Arc::new(StaticTracker::new(Vec::new())) as Arc<dyn TrackerRead>;
        let source = Arc::new(GateAwareQaSource::new());
        // Default gates: empty list models "subtree blocked upstream".
        source.set(QaGates::default(), Vec::new());
        // The waived view *would* surface a candidate, but the builder
        // wires the SPEC default (gates-on); the tick must never see it.
        source.set(
            QaGates {
                require_no_open_blockers: false,
            },
            vec![qa_candidate(7, "PROJ-7", QaRequestCause::DirectQaRequest)],
        );
        let dispatcher = Arc::new(RecordingQaDispatcher::new(QaDispatchReason::Passed));

        let bundle = build_scheduler_v2(
            &cfg,
            tracker,
            noop_dispatcher(),
            empty_integration_source(),
            noop_integration_dispatcher(),
            source.clone() as Arc<dyn QaQueueSource>,
            dispatcher.clone() as Arc<dyn QaDispatcher>,
        );
        let SchedulerV2Bundle {
            mut scheduler,
            qa_dispatch,
            qa_runner,
            ..
        } = bundle;

        scheduler.tick_once().await;
        assert!(qa_dispatch.is_empty());

        let report = qa_runner.run_pending(CancellationToken::new()).await;
        assert!(report.is_idle());
        assert!(dispatcher.calls().is_empty());
    }

    /// Waiver-routed: an authorised waiver role records a `Waived`
    /// verdict for an emitted request. The runner records the verdict
    /// verbatim — the kernel-side validation of the waiver itself
    /// (waiver role + reason) lives in `QaOutcome::try_new`.
    #[tokio::test]
    async fn builder_records_waiver_routed_qa_verdict() {
        let cfg = cfg_with(10, 0);
        let tracker = Arc::new(StaticTracker::new(Vec::new())) as Arc<dyn TrackerRead>;
        let source = Arc::new(GateAwareQaSource::new());
        source.set(
            QaGates::default(),
            vec![qa_candidate(
                99,
                "PROJ-99",
                QaRequestCause::IntegrationConsolidated,
            )],
        );
        let dispatcher = Arc::new(RecordingQaDispatcher::new(QaDispatchReason::Waived));

        let bundle = build_scheduler_v2(
            &cfg,
            tracker,
            noop_dispatcher(),
            empty_integration_source(),
            noop_integration_dispatcher(),
            source.clone() as Arc<dyn QaQueueSource>,
            dispatcher.clone() as Arc<dyn QaDispatcher>,
        );
        let SchedulerV2Bundle {
            mut scheduler,
            qa_dispatch,
            qa_runner,
            ..
        } = bundle;

        scheduler.tick_once().await;
        assert_eq!(qa_dispatch.len(), 1);

        let report = qa_runner.run_pending(CancellationToken::new()).await;
        assert_eq!(report.spawned, 1);

        let outcomes = wait_for_qa_outcomes(&qa_runner, 1).await;
        assert_eq!(outcomes.len(), 1);
        assert_eq!(outcomes[0].work_item_id, WorkItemId::new(99));
        assert_eq!(outcomes[0].reason, QaDispatchReason::Waived);
        assert_eq!(dispatcher.calls(), vec!["PROJ-99"]);
    }
}
