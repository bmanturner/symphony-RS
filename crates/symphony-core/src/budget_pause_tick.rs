//! Budget-pause [`QueueTick`] — surfaces durable budget pauses awaiting
//! operator/policy resume (SPEC v2 §5.15 / ARCHITECTURE v2 §5.8, §7.1).
//!
//! Phase 11 of the v2 checklist replaces the flat poll loop with a fan
//! of logical queues. The budget-pause tick consumes one upstream
//! signal — durable rows in the `budget_pauses` table whose
//! [`BudgetPauseStatus::Active`] still blocks further work — and emits
//! one downstream artifact: a [`BudgetPauseDispatchRequest`] per active
//! pause, claimed by [`BudgetPauseId`] so re-ticking the same view does
//! not re-emit the same pause while it remains active.
//!
//! Core must not depend on `symphony-state`, so the upstream view is
//! abstracted through the [`BudgetPauseQueueSource`] trait. The
//! composition root will eventually supply an adapter that selects from
//! `budget_pauses WHERE status = 'active'`, ordered FIFO by id. Tests
//! in this module use a deterministic in-memory fake.
//!
//! Resume conditions:
//!
//! * **Operator resume** — an operator explicitly continues the work
//!   item; the durable record transitions
//!   [`BudgetPauseStatus::Active`] → [`BudgetPauseStatus::Resolved`].
//! * **Policy waiver** — a configured waiver role declines to enforce
//!   the cap; the record transitions
//!   [`BudgetPauseStatus::Active`] → [`BudgetPauseStatus::Waived`].
//!
//! Both transitions move the record out of `Active`, so the source stops
//! returning it on the next tick. The claim set is pruned and the pause
//! does not re-emit. Resuming is *external* to this tick — the tick's
//! job is to *surface* active pauses for the operator/policy handler
//! that drives the resume decision.

use std::collections::HashSet;
use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use serde::{Deserialize, Serialize};

use crate::logical_queue::{LogicalQueue, QueueTickOutcome};
use crate::queue_tick::{QueueTick, QueueTickCadence};
use crate::role::RoleName;
use crate::work_item::WorkItemId;

/// Durable budget-pause id. Stable claim key for the queue tick and the
/// `budget_pauses.id` foreign key in the durable schema (ARCH v2 §5.8).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(transparent)]
pub struct BudgetPauseId(i64);

impl BudgetPauseId {
    /// Wrap a raw row id.
    pub fn new(id: i64) -> Self {
        Self(id)
    }

    /// Underlying raw id, primarily for state-crate adapters.
    pub fn get(self) -> i64 {
        self.0
    }
}

/// Lifecycle of a durable budget pause. Mirrors the
/// `budget_pauses.status` CHECK constraint in the v2 schema migration
/// (ARCH v2 §5.8); kept in core so the queue tick can reason about it
/// without a state-crate dependency.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum BudgetPauseStatus {
    /// The cap is currently blocking further runs against the work item.
    /// Only `Active` pauses surface through the queue.
    Active,
    /// An operator explicitly resumed the work item. Cleared.
    Resolved,
    /// Policy waived the cap (per [`crate::QaBlockerPolicy`]-style waiver
    /// flow, but for budget). Cleared.
    Waived,
}

/// One pending pause surfaced by a [`BudgetPauseQueueSource`].
///
/// Minimal projection of the durable row: enough for the operator/policy
/// handler to render a decision prompt without re-querying state. The
/// shape mirrors the spec's `budget_pauses` columns (ARCH v2 §5.8).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct BudgetPauseCandidate {
    /// Durable pause id. Stable claim key.
    pub pause_id: BudgetPauseId,
    /// Work item this cap blocks.
    pub work_item_id: WorkItemId,
    /// Cap label (`max_cost_per_issue_usd`, `max_retries`, …). Free
    /// text because budget kinds are workflow-defined (SPEC v2 §5.15).
    pub budget_kind: String,
    /// Configured cap. Stored as `f64` to cover both integer counters
    /// (turns, retries) and currency (USD); exact representation is the
    /// state crate's problem.
    pub limit_value: f64,
    /// Observed value at the moment the pause was filed.
    pub observed: f64,
}

/// One dispatch request emitted by the budget-pause queue tick.
///
/// Plain serializable data so a future scheduler can persist and replay
/// it on restart. Carries the same fields the operator/policy handler
/// needs; the durable record is the source of truth and is keyed by
/// [`Self::pause_id`].
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct BudgetPauseDispatchRequest {
    /// Durable pause id. Stable claim key.
    pub pause_id: BudgetPauseId,
    /// Work item this cap blocks.
    pub work_item_id: WorkItemId,
    /// Cap label.
    pub budget_kind: String,
    /// Configured cap.
    pub limit_value: f64,
    /// Observed value at pause time.
    pub observed: f64,
    /// Durable `runs` row this dispatch will animate, when known. The
    /// budget-pause runner uses this to acquire/release the durable
    /// lease (CHECKLIST_v2 Phase 11). `None` for legacy producers that
    /// do not reserve a run row before queueing — those dispatches
    /// bypass lease acquisition.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub run_id: Option<crate::blocker::RunRef>,
    /// Optional role key for the multi-scope concurrency gate
    /// (CHECKLIST_v2 Phase 11). When `Some`, the budget-pause runner
    /// builds a [`crate::concurrency_gate::DispatchTriple`] from
    /// `(role, agent_profile, repository)` and gates dispatch through
    /// the configured [`crate::concurrency_gate::ConcurrencyGate`].
    /// Producers typically populate this with the configured budget
    /// resume/waiver role so resume dispatches respect that role's cap.
    /// `None` bypasses gate acquisition entirely.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub role: Option<RoleName>,
    /// Optional agent-profile scope key. Paired with `role` for gate
    /// acquisition; ignored when `role` is `None`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub agent_profile: Option<String>,
    /// Optional repository scope key. Paired with `role` for gate
    /// acquisition; ignored when `role` is `None`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub repository: Option<String>,
}

/// Errors a [`BudgetPauseQueueSource`] may surface to the tick.
///
/// Kept opaque on purpose: the tick only needs to count errors, not
/// branch on them.
#[derive(Debug, thiserror::Error)]
pub enum BudgetPauseQueueError {
    /// Underlying source failed (state DB error, adapter error, …).
    #[error("budget pause queue source error: {0}")]
    Source(String),
}

/// Read-only view over the budget-pause queue, abstracted so core does
/// not depend on `symphony-state`.
pub trait BudgetPauseQueueSource: Send + Sync {
    /// Return every budget pause currently in
    /// [`BudgetPauseStatus::Active`], in FIFO order by id. Order is
    /// preserved by the tick when emitting dispatch requests.
    fn list_active(&self) -> Result<Vec<BudgetPauseCandidate>, BudgetPauseQueueError>;
}

/// Shared FIFO of pending [`BudgetPauseDispatchRequest`]s.
#[derive(Default, Debug)]
pub struct BudgetPauseDispatchQueue {
    inner: Mutex<Vec<BudgetPauseDispatchRequest>>,
}

impl BudgetPauseDispatchQueue {
    /// Construct an empty queue.
    pub fn new() -> Self {
        Self::default()
    }

    /// Number of pending dispatch requests.
    pub fn len(&self) -> usize {
        self.inner
            .lock()
            .expect("budget pause dispatch queue mutex poisoned")
            .len()
    }

    /// True iff no pending requests.
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// Snapshot the pending requests without removing them.
    pub fn snapshot(&self) -> Vec<BudgetPauseDispatchRequest> {
        self.inner
            .lock()
            .expect("budget pause dispatch queue mutex poisoned")
            .clone()
    }

    /// Drain every pending request in FIFO order.
    pub fn drain(&self) -> Vec<BudgetPauseDispatchRequest> {
        std::mem::take(
            &mut *self
                .inner
                .lock()
                .expect("budget pause dispatch queue mutex poisoned"),
        )
    }

    pub(crate) fn enqueue(&self, req: BudgetPauseDispatchRequest) {
        self.inner
            .lock()
            .expect("budget pause dispatch queue mutex poisoned")
            .push(req);
    }
}

/// Budget-pause queue tick.
///
/// One tick = one snapshot of the upstream
/// [`BudgetPauseQueueSource`]. Bounded work, no awaits beyond the trait
/// signature (the source call is synchronous). Cadence and fan-out
/// belong to the multi-queue scheduler.
///
/// Claim semantics: a pause emitted in tick *N* is recorded in the
/// internal claim set so it does not re-emit on tick *N+1* while the
/// source still surfaces it. When the source stops returning it —
/// because the operator resumed (status → [`BudgetPauseStatus::Resolved`])
/// or policy waived (status → [`BudgetPauseStatus::Waived`]) — the claim
/// is pruned.
pub struct BudgetPauseQueueTick {
    source: Arc<dyn BudgetPauseQueueSource>,
    queue: Arc<BudgetPauseDispatchQueue>,
    cadence: QueueTickCadence,
    claimed: Mutex<HashSet<BudgetPauseId>>,
}

impl BudgetPauseQueueTick {
    /// Construct a budget-pause tick.
    pub fn new(
        source: Arc<dyn BudgetPauseQueueSource>,
        queue: Arc<BudgetPauseDispatchQueue>,
        cadence: QueueTickCadence,
    ) -> Self {
        Self {
            source,
            queue,
            cadence,
            claimed: Mutex::new(HashSet::new()),
        }
    }

    /// Borrow the shared dispatch queue.
    pub fn dispatch_queue(&self) -> &Arc<BudgetPauseDispatchQueue> {
        &self.queue
    }

    /// Number of pauses currently claimed.
    pub fn claimed_count(&self) -> usize {
        self.claimed
            .lock()
            .expect("budget pause claim set mutex poisoned")
            .len()
    }
}

#[async_trait]
impl QueueTick for BudgetPauseQueueTick {
    fn queue(&self) -> LogicalQueue {
        LogicalQueue::BudgetPause
    }

    fn cadence(&self) -> QueueTickCadence {
        self.cadence
    }

    async fn tick(&mut self) -> QueueTickOutcome {
        let candidates = match self.source.list_active() {
            Ok(c) => c,
            Err(_e) => {
                return QueueTickOutcome {
                    queue: LogicalQueue::BudgetPause,
                    considered: 0,
                    processed: 0,
                    deferred: 0,
                    errors: 1,
                };
            }
        };
        let considered = candidates.len();

        // Prune claims for pauses the source no longer surfaces — typically
        // because an operator resumed or policy waived them.
        let active_ids: HashSet<BudgetPauseId> = candidates.iter().map(|c| c.pause_id).collect();
        {
            let mut claimed = self
                .claimed
                .lock()
                .expect("budget pause claim set mutex poisoned");
            claimed.retain(|id| active_ids.contains(id));
        }

        let mut processed = 0usize;
        let mut deferred = 0usize;

        for c in candidates {
            let already_claimed = {
                let claimed = self
                    .claimed
                    .lock()
                    .expect("budget pause claim set mutex poisoned");
                claimed.contains(&c.pause_id)
            };
            if already_claimed {
                deferred += 1;
                continue;
            }

            self.queue.enqueue(BudgetPauseDispatchRequest {
                pause_id: c.pause_id,
                work_item_id: c.work_item_id,
                budget_kind: c.budget_kind,
                limit_value: c.limit_value,
                observed: c.observed,
                run_id: None,
                role: None,
                agent_profile: None,
                repository: None,
            });
            self.claimed
                .lock()
                .expect("budget pause claim set mutex poisoned")
                .insert(c.pause_id);
            processed += 1;
        }

        QueueTickOutcome {
            queue: LogicalQueue::BudgetPause,
            considered,
            processed,
            deferred,
            errors: 0,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::queue_tick::run_queue_tick_n;
    use std::sync::Mutex as StdMutex;
    use std::time::Duration;

    /// Durable record the fake source maintains. Mirrors the shape of
    /// `budget_pauses` rows: id + work item + cap fields + status.
    /// Resume helpers flip the status to drive lifecycle transitions
    /// the same way the operator/policy handler will in production.
    #[derive(Clone)]
    struct PauseRecord {
        pause_id: BudgetPauseId,
        work_item_id: WorkItemId,
        budget_kind: String,
        limit_value: f64,
        observed: f64,
        status: BudgetPauseStatus,
    }

    struct FakeSource {
        records: StdMutex<Vec<PauseRecord>>,
        fail_next: StdMutex<bool>,
    }

    impl FakeSource {
        fn new() -> Self {
            Self {
                records: StdMutex::new(Vec::new()),
                fail_next: StdMutex::new(false),
            }
        }

        fn insert(&self, record: PauseRecord) {
            self.records.lock().unwrap().push(record);
        }

        fn arm_failure(&self) {
            *self.fail_next.lock().unwrap() = true;
        }

        fn resolve(&self, id: BudgetPauseId) {
            let mut recs = self.records.lock().unwrap();
            let r = recs.iter_mut().find(|r| r.pause_id == id).expect("missing");
            r.status = BudgetPauseStatus::Resolved;
        }

        fn waive(&self, id: BudgetPauseId) {
            let mut recs = self.records.lock().unwrap();
            let r = recs.iter_mut().find(|r| r.pause_id == id).expect("missing");
            r.status = BudgetPauseStatus::Waived;
        }
    }

    impl BudgetPauseQueueSource for FakeSource {
        fn list_active(&self) -> Result<Vec<BudgetPauseCandidate>, BudgetPauseQueueError> {
            let mut fail = self.fail_next.lock().unwrap();
            if *fail {
                *fail = false;
                return Err(BudgetPauseQueueError::Source("scripted failure".into()));
            }
            let recs = self.records.lock().unwrap();
            Ok(recs
                .iter()
                .filter(|r| r.status == BudgetPauseStatus::Active)
                .map(|r| BudgetPauseCandidate {
                    pause_id: r.pause_id,
                    work_item_id: r.work_item_id,
                    budget_kind: r.budget_kind.clone(),
                    limit_value: r.limit_value,
                    observed: r.observed,
                })
                .collect())
        }
    }

    fn cadence() -> QueueTickCadence {
        QueueTickCadence::fixed(Duration::from_millis(1))
    }

    fn record(id: i64, work: i64, kind: &str, limit_value: f64, observed: f64) -> PauseRecord {
        PauseRecord {
            pause_id: BudgetPauseId::new(id),
            work_item_id: WorkItemId::new(work),
            budget_kind: kind.into(),
            limit_value,
            observed,
            status: BudgetPauseStatus::Active,
        }
    }

    fn build_tick(
        source: Arc<FakeSource>,
    ) -> (
        BudgetPauseQueueTick,
        Arc<FakeSource>,
        Arc<BudgetPauseDispatchQueue>,
    ) {
        let queue = Arc::new(BudgetPauseDispatchQueue::new());
        let tick = BudgetPauseQueueTick::new(
            source.clone() as Arc<dyn BudgetPauseQueueSource>,
            queue.clone(),
            cadence(),
        );
        (tick, source, queue)
    }

    #[tokio::test]
    async fn active_pauses_are_emitted_in_fifo_order() {
        let src = Arc::new(FakeSource::new());
        src.insert(record(1, 100, "max_cost_per_issue_usd", 10.0, 12.5));
        src.insert(record(2, 101, "max_turns_per_run", 20.0, 21.0));
        let (mut t, _src, q) = build_tick(src);

        let outcome = t.tick().await;
        assert_eq!(outcome.queue, LogicalQueue::BudgetPause);
        assert_eq!(outcome.considered, 2);
        assert_eq!(outcome.processed, 2);
        assert_eq!(outcome.deferred, 0);
        assert_eq!(outcome.errors, 0);

        let drained = q.drain();
        assert_eq!(drained.len(), 2);
        assert_eq!(drained[0].pause_id, BudgetPauseId::new(1));
        assert_eq!(drained[0].budget_kind, "max_cost_per_issue_usd");
        assert_eq!(drained[0].limit_value, 10.0);
        assert_eq!(drained[0].observed, 12.5);
        assert_eq!(drained[1].pause_id, BudgetPauseId::new(2));
        assert_eq!(drained[1].work_item_id, WorkItemId::new(101));
    }

    #[tokio::test]
    async fn resolved_and_waived_pauses_are_excluded_from_the_queue() {
        let src = Arc::new(FakeSource::new());
        let mut resolved = record(1, 100, "max_retries", 3.0, 3.0);
        resolved.status = BudgetPauseStatus::Resolved;
        let mut waived = record(2, 100, "max_retries", 3.0, 3.0);
        waived.status = BudgetPauseStatus::Waived;
        src.insert(resolved);
        src.insert(waived);
        src.insert(record(3, 100, "max_retries", 3.0, 4.0));
        let (mut t, _src, q) = build_tick(src);

        let outcome = t.tick().await;
        assert_eq!(outcome.considered, 1);
        assert_eq!(outcome.processed, 1);
        let drained = q.drain();
        assert_eq!(drained.len(), 1);
        assert_eq!(drained[0].pause_id, BudgetPauseId::new(3));
    }

    #[tokio::test]
    async fn operator_resume_drops_pause_and_prunes_claim() {
        let src = Arc::new(FakeSource::new());
        src.insert(record(1, 100, "max_cost_per_issue_usd", 10.0, 12.5));
        let (mut t, src, q) = build_tick(src);

        let o1 = t.tick().await;
        assert_eq!(o1.processed, 1);
        let drained = q.drain();
        assert_eq!(drained.len(), 1);
        assert_eq!(drained[0].pause_id, BudgetPauseId::new(1));
        assert_eq!(t.claimed_count(), 1);

        // Operator resumes: status → Resolved → source no longer surfaces it.
        src.resolve(BudgetPauseId::new(1));
        let o2 = t.tick().await;
        assert_eq!(o2.considered, 0);
        assert_eq!(o2.processed, 0);
        assert_eq!(o2.deferred, 0);
        assert_eq!(t.claimed_count(), 0);
        assert!(q.is_empty());
    }

    #[tokio::test]
    async fn policy_waiver_drops_pause_and_prunes_claim() {
        let src = Arc::new(FakeSource::new());
        src.insert(record(7, 100, "max_turns_per_run", 20.0, 25.0));
        let (mut t, src, q) = build_tick(src);

        let o1 = t.tick().await;
        assert_eq!(o1.processed, 1);
        assert_eq!(q.drain().len(), 1);
        assert_eq!(t.claimed_count(), 1);

        src.waive(BudgetPauseId::new(7));
        let o2 = t.tick().await;
        assert_eq!(o2.considered, 0);
        assert_eq!(o2.processed, 0);
        assert_eq!(t.claimed_count(), 0);
        assert!(q.is_empty());
    }

    #[tokio::test]
    async fn already_claimed_pause_is_not_re_emitted() {
        let src = Arc::new(FakeSource::new());
        src.insert(record(1, 100, "max_retries", 3.0, 4.0));
        let (mut t, _src, q) = build_tick(src);

        let o1 = t.tick().await;
        assert_eq!(o1.processed, 1);
        assert_eq!(q.drain().len(), 1);

        // Re-tick: still Active → claimed → deferred, no re-emit.
        let o2 = t.tick().await;
        assert_eq!(o2.considered, 1);
        assert_eq!(o2.processed, 0);
        assert_eq!(o2.deferred, 1);
        assert!(q.is_empty());
        assert_eq!(t.claimed_count(), 1);
    }

    #[tokio::test]
    async fn empty_queue_is_idle() {
        let src = Arc::new(FakeSource::new());
        let (mut t, _src, q) = build_tick(src);

        let outcome = t.tick().await;
        assert!(outcome.is_idle());
        assert_eq!(outcome.queue, LogicalQueue::BudgetPause);
        assert!(q.is_empty());
        assert_eq!(t.claimed_count(), 0);
    }

    #[tokio::test]
    async fn source_error_increments_errors_and_emits_nothing() {
        let src = Arc::new(FakeSource::new());
        src.insert(record(1, 100, "max_retries", 3.0, 4.0));
        let (mut t, src, q) = build_tick(src);

        src.arm_failure();
        let o = t.tick().await;
        assert_eq!(o.queue, LogicalQueue::BudgetPause);
        assert_eq!(o.errors, 1);
        assert_eq!(o.processed, 0);
        assert_eq!(o.considered, 0);
        assert!(q.is_empty());
        assert_eq!(t.claimed_count(), 0);
    }

    #[tokio::test]
    async fn drives_through_dyn_queue_tick_trait_object() {
        let src = Arc::new(FakeSource::new());
        src.insert(record(1, 100, "max_retries", 3.0, 4.0));
        let (t, _src, q) = build_tick(src);
        let mut boxed: Box<dyn QueueTick> = Box::new(t);
        let outcomes = run_queue_tick_n(boxed.as_mut(), 3).await;
        assert_eq!(outcomes.len(), 3);
        for o in &outcomes {
            assert_eq!(o.queue, LogicalQueue::BudgetPause);
        }
        assert_eq!(outcomes[0].processed, 1);
        assert_eq!(outcomes[1].processed, 0);
        assert_eq!(outcomes[1].deferred, 1);
        assert_eq!(outcomes[2].deferred, 1);
        assert_eq!(q.len(), 1);
    }

    #[tokio::test]
    async fn tick_reports_queue_and_configured_cadence() {
        let src = Arc::new(FakeSource::new());
        let q = Arc::new(BudgetPauseDispatchQueue::new());
        let cad = QueueTickCadence::from_millis(2_500, 100);
        let t = BudgetPauseQueueTick::new(src as Arc<dyn BudgetPauseQueueSource>, q, cad);
        assert_eq!(t.queue(), LogicalQueue::BudgetPause);
        assert_eq!(t.cadence(), cad);
    }

    #[tokio::test]
    async fn dispatch_queue_drain_is_fifo_and_empties() {
        let q = BudgetPauseDispatchQueue::new();
        q.enqueue(BudgetPauseDispatchRequest {
            pause_id: BudgetPauseId::new(1),
            work_item_id: WorkItemId::new(100),
            budget_kind: "max_retries".into(),
            limit_value: 3.0,
            observed: 4.0,
            run_id: None,
            role: None,
            agent_profile: None,
            repository: None,
        });
        q.enqueue(BudgetPauseDispatchRequest {
            pause_id: BudgetPauseId::new(2),
            work_item_id: WorkItemId::new(101),
            budget_kind: "max_cost_per_issue_usd".into(),
            limit_value: 10.0,
            observed: 11.5,
            run_id: None,
            role: None,
            agent_profile: None,
            repository: None,
        });
        assert_eq!(q.len(), 2);
        let drained = q.drain();
        assert_eq!(drained.len(), 2);
        assert_eq!(drained[0].pause_id, BudgetPauseId::new(1));
        assert_eq!(drained[1].pause_id, BudgetPauseId::new(2));
        assert!(q.is_empty());
    }

    #[test]
    fn pause_id_round_trips_through_json() {
        let id = BudgetPauseId::new(42);
        let json = serde_json::to_string(&id).unwrap();
        assert_eq!(json, "42");
        let back: BudgetPauseId = serde_json::from_str(&json).unwrap();
        assert_eq!(back, id);
    }

    #[test]
    fn status_round_trips_through_json_with_snake_case() {
        for s in [
            BudgetPauseStatus::Active,
            BudgetPauseStatus::Resolved,
            BudgetPauseStatus::Waived,
        ] {
            let json = serde_json::to_string(&s).unwrap();
            let back: BudgetPauseStatus = serde_json::from_str(&json).unwrap();
            assert_eq!(back, s);
        }
        assert_eq!(
            serde_json::to_string(&BudgetPauseStatus::Active).unwrap(),
            "\"active\""
        );
        assert_eq!(
            serde_json::to_string(&BudgetPauseStatus::Resolved).unwrap(),
            "\"resolved\""
        );
        assert_eq!(
            serde_json::to_string(&BudgetPauseStatus::Waived).unwrap(),
            "\"waived\""
        );
    }

    #[test]
    fn candidate_round_trips_through_json() {
        let c = BudgetPauseCandidate {
            pause_id: BudgetPauseId::new(7),
            work_item_id: WorkItemId::new(100),
            budget_kind: "max_cost_per_issue_usd".into(),
            limit_value: 10.0,
            observed: 12.5,
        };
        let json = serde_json::to_string(&c).unwrap();
        let back: BudgetPauseCandidate = serde_json::from_str(&json).unwrap();
        assert_eq!(back, c);
    }

    #[test]
    fn dispatch_request_round_trips_through_json() {
        let r = BudgetPauseDispatchRequest {
            pause_id: BudgetPauseId::new(9),
            work_item_id: WorkItemId::new(100),
            budget_kind: "max_retries".into(),
            limit_value: 3.0,
            observed: 4.0,
            run_id: None,
            role: None,
            agent_profile: None,
            repository: None,
        };
        let json = serde_json::to_string(&r).unwrap();
        // `run_id: None` should be skipped when serializing.
        assert!(!json.contains("run_id"));
        assert!(!json.contains("role"));
        let back: BudgetPauseDispatchRequest = serde_json::from_str(&json).unwrap();
        assert_eq!(back, r);
    }

    #[test]
    fn dispatch_request_round_trips_when_run_id_present() {
        let r = BudgetPauseDispatchRequest {
            pause_id: BudgetPauseId::new(9),
            work_item_id: WorkItemId::new(100),
            budget_kind: "max_retries".into(),
            limit_value: 3.0,
            observed: 4.0,
            run_id: Some(crate::blocker::RunRef::new(99)),
            role: None,
            agent_profile: None,
            repository: None,
        };
        let json = serde_json::to_string(&r).unwrap();
        assert!(json.contains("run_id"));
        let back: BudgetPauseDispatchRequest = serde_json::from_str(&json).unwrap();
        assert_eq!(back, r);
    }

    #[test]
    fn dispatch_request_round_trips_when_scope_keys_present() {
        let r = BudgetPauseDispatchRequest {
            pause_id: BudgetPauseId::new(11),
            work_item_id: WorkItemId::new(100),
            budget_kind: "max_cost_per_issue_usd".into(),
            limit_value: 10.0,
            observed: 12.5,
            run_id: None,
            role: Some(RoleName::new("platform_lead")),
            agent_profile: Some("claude".into()),
            repository: Some("acme/widgets".into()),
        };
        let json = serde_json::to_string(&r).unwrap();
        let back: BudgetPauseDispatchRequest = serde_json::from_str(&json).unwrap();
        assert_eq!(back, r);
        assert!(json.contains("agent_profile"));
        assert!(json.contains("repository"));
    }
}
