//! Composition helper for the v2 multi-queue scheduler.
//!
//! Phase 11 of the v2 checklist replaces the flat [`PollLoop`] with a
//! [`SchedulerV2`] that fans logical queue ticks (intake, specialist,
//! integration, QA, follow-up approval, budget pause, recovery) under
//! one shared `polling.interval_ms` cadence. Wiring all of those at
//! once is too large for one commit; this module is the seam each
//! subsequent decomposition step extends.
//!
//! [`build_scheduler_v2`] currently registers the **intake** and
//! **recovery** ticks. The recovery tick is driven by the shared
//! [`ActiveSetStore`] plus an in-memory [`ClaimedRunRegistry`] that
//! stands in for the durable `runs` table until a state DB is plumbed
//! in: any claim whose `IssueId` is no longer in the active set
//! surfaces as an [`ExpiredLeaseCandidate`], matching the flat poll
//! loop's `Reconciled { dropped }` reconciliation contract. Specialist
//! / integration / QA / follow-up-approval / budget-pause ticks layer
//! on top of the same store and registry in subsequent decomposition
//! steps. The returned scheduler is not yet wired into
//! [`crate::run::run`]; that switch is the last decomposition step.
//! Until then the production entry point remains [`PollLoop`].
//!
//! [`PollLoop`]: symphony_core::PollLoop

// Pre-wiring: `build_scheduler_v2` is exercised by this module's tests
// but not yet referenced from `run.rs`. Subsequent decomposition steps
// add the call site; until then suppress dead-code under `-D warnings`.
#![allow(dead_code)]

use std::collections::{HashMap, HashSet};
use std::sync::{Arc, Mutex as StdMutex};

use symphony_config::WorkflowConfig;
use symphony_core::{
    ActiveSetStore, ExpiredLeaseCandidate, IntakeQueueTick, IssueId,
    OrphanedWorkspaceClaimCandidate, QueueTick, QueueTickCadence, RecoveryDispatchQueue,
    RecoveryQueueError, RecoveryQueueSource, RecoveryQueueTick, RecoveryRunId,
    RecoveryWorkspaceClaimId, SchedulerV2, SchedulerV2Config, TrackerRead, WorkItemId,
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

/// Build a [`SchedulerV2`] with the intake tick wired up.
///
/// Cadence is mapped from [`WorkflowConfig::polling`]:
/// `interval_ms` → `SchedulerV2Config::interval`,
/// `jitter_ms` → `SchedulerV2Config::jitter`. The intake tick reuses
/// the same cadence for now — there is no per-queue cadence config in
/// `WORKFLOW.md` yet, and SPEC v2 §5.2 names a single `polling.*`
/// block.
///
/// This function is intentionally additive: it does not register
/// specialist / integration / QA / follow-up approval / budget-pause /
/// recovery ticks yet. Those come in subsequent decomposition steps,
/// which will extend this builder rather than introduce a parallel one.
pub fn build_scheduler_v2(
    cfg: &WorkflowConfig,
    tracker: Arc<dyn TrackerRead>,
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

    let scheduler = SchedulerV2::with_ticks(scheduler_cfg, vec![intake, recovery]);
    SchedulerV2Bundle {
        scheduler,
        active_set,
        claimed_runs,
        recovery_dispatch,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use async_trait::async_trait;
    use std::sync::Mutex as StdMutex;
    use std::time::Duration;
    use symphony_config::PollingConfig;
    use symphony_core::tracker::{Issue, IssueId, IssueState};
    use symphony_core::tracker_trait::{TrackerError, TrackerResult};
    use tokio_util::sync::CancellationToken;

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

    #[tokio::test]
    async fn builder_registers_intake_and_recovery_ticks() {
        let cfg = cfg_with(50, 0);
        let tracker = Arc::new(StaticTracker::new(issues(2))) as Arc<dyn TrackerRead>;
        let bundle = build_scheduler_v2(&cfg, tracker);
        // Intake + recovery: subsequent decomposition steps grow this.
        assert_eq!(bundle.scheduler.tick_count(), 2);
        assert!(bundle.claimed_runs.is_empty());
        assert!(bundle.recovery_dispatch.is_empty());
    }

    #[tokio::test]
    async fn builder_maps_polling_config_into_scheduler_cadence() {
        let cfg = cfg_with(123, 45);
        let tracker = Arc::new(StaticTracker::new(Vec::new())) as Arc<dyn TrackerRead>;
        let bundle = build_scheduler_v2(&cfg, tracker);
        let sc = bundle.scheduler.config();
        assert_eq!(sc.interval, Duration::from_millis(123));
        assert_eq!(sc.jitter, Duration::from_millis(45));
    }

    #[tokio::test]
    async fn tick_once_publishes_active_set_into_shared_store() {
        let cfg = cfg_with(10, 0);
        let tracker = Arc::new(StaticTracker::new(issues(3))) as Arc<dyn TrackerRead>;
        let bundle = build_scheduler_v2(&cfg, tracker);
        let SchedulerV2Bundle {
            mut scheduler,
            active_set,
            ..
        } = bundle;

        assert!(active_set.is_empty());
        let report = scheduler.tick_once().await;
        assert_eq!(report.outcomes.len(), 2);
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
        let bundle = build_scheduler_v2(&cfg, tracker);
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
        let bundle = build_scheduler_v2(&cfg, tracker_inner.clone() as Arc<dyn TrackerRead>);
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
        let bundle = build_scheduler_v2(&cfg, tracker_inner.clone() as Arc<dyn TrackerRead>);
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
        let bundle = build_scheduler_v2(&cfg, tracker_inner.clone() as Arc<dyn TrackerRead>);
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
}
