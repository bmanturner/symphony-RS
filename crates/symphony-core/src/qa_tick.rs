//! QA [`QueueTick`] — drains work items the QA-gate role should verify,
//! gated by [`QaGates`] (SPEC v2 §5.12, §6.5 / ARCHITECTURE v2 §7.1, §10).
//!
//! Phase 11 of the v2 checklist replaces the flat poll loop with a fan
//! of logical queues. The QA tick consumes one upstream signal — the
//! durable `QaQueueRepository` view in `symphony-state` — and emits one
//! downstream artifact: a [`QaDispatchRequest`] per ready work item,
//! claimed by `work_item_id` so re-ticking the same view does not
//! re-emit the same item while it remains surfaced.
//!
//! Core must not depend on `symphony-state`, so the upstream view is
//! abstracted through the [`QaQueueSource`] trait. The composition root
//! supplies an adapter that calls
//! `QaQueueRepository::list_ready_for_qa_with_gates` and projects each
//! row onto a [`QaCandidate`]. Tests in this module use a deterministic
//! in-memory fake.
//!
//! Ready / blocked behavior:
//!
//! * **Ready** — the source returns one or more candidates under the
//!   workflow's [`QaGates`]; each is claimed and emitted in FIFO order.
//! * **Blocked** — the source returns an empty list (e.g. an open
//!   `blocks` edge in the work item's subtree suppresses it under the
//!   default gates). Nothing is emitted.
//! * **Waived** — flipping [`QaGates::require_no_open_blockers`] off
//!   surfaces previously blocked items. The tick honors whichever
//!   [`QaGates`] value it was constructed with; flipping a gate is the
//!   workflow's policy decision, not the tick's.
//!
//! Constructing a fully-formed [`crate::QaRunRequest`] (with workspace
//! claim, draft PR projection, prior handoffs, acceptance criteria)
//! belongs to the QA runner downstream. The tick's job is to *surface*
//! ready work items and let the runner build the request when it
//! actually launches the agent.

use std::collections::HashSet;
use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use serde::{Deserialize, Serialize};

use crate::logical_queue::{LogicalQueue, QueueTickOutcome};
use crate::qa_request::QaRequestCause;
use crate::queue_tick::{QueueTick, QueueTickCadence};
use crate::work_item::WorkItemId;

/// Workflow-level gate switches that decide which work items the QA
/// queue tick surfaces (SPEC v2 §5.12).
///
/// The default matches the SPEC: blocker gate *on*. A workflow may
/// waive it by setting [`Self::require_no_open_blockers`] to `false`.
/// Mirrors `symphony_state::QaQueueGates`; kept independent so core
/// does not depend on the state crate.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct QaGates {
    /// When `true` (default), any open `blocks` edge anywhere in the
    /// work item's subtree excludes it from the queue. When `false`,
    /// the gate is waived.
    pub require_no_open_blockers: bool,
}

impl Default for QaGates {
    fn default() -> Self {
        Self {
            require_no_open_blockers: true,
        }
    }
}

/// One candidate surfaced by a [`QaQueueSource`].
///
/// Minimal projection of the durable work item plus the queue's
/// readiness reason. Mirrors the on-the-wire shape of
/// `symphony_state::QaQueueEntry` but lives in core so it can flow
/// through the queue tick without a state-crate dependency.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct QaCandidate {
    /// Durable work item id.
    pub work_item_id: WorkItemId,
    /// Tracker-facing identifier (e.g. `PROJ-42`).
    pub identifier: String,
    /// Work item title, used by the dispatcher when building the run
    /// request and by status surfaces.
    pub title: String,
    /// Why the upstream queue surfaced this work item.
    pub cause: QaRequestCause,
}

/// Errors a [`QaQueueSource`] may surface to the tick.
///
/// Kept opaque on purpose: the tick only needs to count errors, not
/// branch on them. State-crate adapters wrap their `StateError` into
/// the [`QaQueueError::Source`] variant.
#[derive(Debug, thiserror::Error)]
pub enum QaQueueError {
    /// Underlying source failed (state DB error, adapter error, …).
    #[error("qa queue source error: {0}")]
    Source(String),
}

/// Read-only view over the QA queue, abstracted so core does not
/// depend on `symphony-state`.
///
/// Implementations must respect the supplied [`QaGates`]. The SPEC
/// default (blocker gate on) is enforced upstream by the state crate;
/// passing a non-default value reflects an explicit workflow waiver.
pub trait QaQueueSource: Send + Sync {
    /// Return every work item currently ready for the QA-gate role
    /// under the supplied gates, ordered FIFO. Order is preserved by
    /// the tick when emitting [`QaDispatchRequest`]s.
    fn list_ready(&self, gates: QaGates) -> Result<Vec<QaCandidate>, QaQueueError>;
}

/// One dispatch request emitted by the QA queue tick.
///
/// Plain serializable data so the eventual scheduler can persist and
/// replay it on restart. The downstream QA runner is responsible for
/// building the full [`crate::QaRunRequest`] (workspace claim, draft
/// PR projection, prior handoffs); the tick only signals *which* work
/// item is ready and *why*.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct QaDispatchRequest {
    /// Durable work item id. Stable claim key.
    pub work_item_id: WorkItemId,
    /// Tracker-facing identifier (e.g. `PROJ-42`).
    pub identifier: String,
    /// Work item title.
    pub title: String,
    /// Why the queue surfaced this work item.
    pub cause: QaRequestCause,
    /// Durable `runs` row this dispatch will animate, when known. The
    /// QA runner uses this to acquire/release the durable lease
    /// (CHECKLIST_v2 Phase 11). `None` for legacy producers that do not
    /// reserve a run row before queueing — those dispatches bypass lease
    /// acquisition.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub run_id: Option<crate::blocker::RunRef>,
}

/// Shared FIFO of pending [`QaDispatchRequest`]s.
///
/// The QA tick writes; the eventual QA runner drains via
/// [`Self::drain`]. Order preserved: oldest emission first.
#[derive(Default, Debug)]
pub struct QaDispatchQueue {
    inner: Mutex<Vec<QaDispatchRequest>>,
}

impl QaDispatchQueue {
    /// Construct an empty queue.
    pub fn new() -> Self {
        Self::default()
    }

    /// Number of pending dispatch requests.
    pub fn len(&self) -> usize {
        self.inner
            .lock()
            .expect("qa dispatch queue mutex poisoned")
            .len()
    }

    /// True iff no pending requests.
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// Snapshot the pending requests without removing them.
    pub fn snapshot(&self) -> Vec<QaDispatchRequest> {
        self.inner
            .lock()
            .expect("qa dispatch queue mutex poisoned")
            .clone()
    }

    /// Drain every pending request in FIFO order.
    pub fn drain(&self) -> Vec<QaDispatchRequest> {
        std::mem::take(&mut *self.inner.lock().expect("qa dispatch queue mutex poisoned"))
    }

    /// Append a new request. Pub-crate so only the QA tick (and its
    /// tests) can write; consumers read via [`Self::drain`].
    pub(crate) fn enqueue(&self, req: QaDispatchRequest) {
        self.inner
            .lock()
            .expect("qa dispatch queue mutex poisoned")
            .push(req);
    }
}

/// QA queue tick.
///
/// One tick = one snapshot of the upstream [`QaQueueSource`] under the
/// workflow's [`QaGates`]. Bounded work, no awaits beyond the trait
/// signature (the source call is synchronous). Cadence and fan-out
/// belong to the multi-queue scheduler.
///
/// Claim semantics: a work item emitted in tick *N* is recorded in the
/// internal claim set so it does not re-emit on tick *N+1* while it
/// remains surfaced by the source. When the source stops returning the
/// item — typically because the QA runner promoted the work item past
/// `status_class = qa`, a verdict was filed since the latest
/// integration record, or the gate flipped under a config change — the
/// claim is pruned and a future re-appearance is eligible to dispatch
/// again.
pub struct QaQueueTick {
    source: Arc<dyn QaQueueSource>,
    gates: QaGates,
    queue: Arc<QaDispatchQueue>,
    cadence: QueueTickCadence,
    claimed: Mutex<HashSet<WorkItemId>>,
}

impl QaQueueTick {
    /// Construct a QA tick.
    pub fn new(
        source: Arc<dyn QaQueueSource>,
        gates: QaGates,
        queue: Arc<QaDispatchQueue>,
        cadence: QueueTickCadence,
    ) -> Self {
        Self {
            source,
            gates,
            queue,
            cadence,
            claimed: Mutex::new(HashSet::new()),
        }
    }

    /// Borrow the shared dispatch queue. Lets the composition root
    /// wire the consumer side without separately threading the `Arc`.
    pub fn dispatch_queue(&self) -> &Arc<QaDispatchQueue> {
        &self.queue
    }

    /// Workflow gates this tick was wired with.
    pub fn gates(&self) -> QaGates {
        self.gates
    }

    /// Number of work items currently claimed (emitted and still
    /// returned by the source).
    pub fn claimed_count(&self) -> usize {
        self.claimed
            .lock()
            .expect("qa claim set mutex poisoned")
            .len()
    }
}

#[async_trait]
impl QueueTick for QaQueueTick {
    fn queue(&self) -> LogicalQueue {
        LogicalQueue::Qa
    }

    fn cadence(&self) -> QueueTickCadence {
        self.cadence
    }

    async fn tick(&mut self) -> QueueTickOutcome {
        let candidates = match self.source.list_ready(self.gates) {
            Ok(c) => c,
            Err(_e) => {
                return QueueTickOutcome {
                    queue: LogicalQueue::Qa,
                    considered: 0,
                    processed: 0,
                    deferred: 0,
                    errors: 1,
                };
            }
        };
        let considered = candidates.len();

        // Prune claims for work items the source no longer surfaces.
        let active_ids: HashSet<WorkItemId> = candidates.iter().map(|c| c.work_item_id).collect();
        {
            let mut claimed = self.claimed.lock().expect("qa claim set mutex poisoned");
            claimed.retain(|id| active_ids.contains(id));
        }

        let mut processed = 0usize;
        let mut deferred = 0usize;

        for c in candidates {
            let already_claimed = {
                let claimed = self.claimed.lock().expect("qa claim set mutex poisoned");
                claimed.contains(&c.work_item_id)
            };
            if already_claimed {
                deferred += 1;
                continue;
            }

            self.queue.enqueue(QaDispatchRequest {
                work_item_id: c.work_item_id,
                identifier: c.identifier,
                title: c.title,
                cause: c.cause,
                run_id: None,
            });
            self.claimed
                .lock()
                .expect("qa claim set mutex poisoned")
                .insert(c.work_item_id);
            processed += 1;
        }

        QueueTickOutcome {
            queue: LogicalQueue::Qa,
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

    /// Fake source that returns whichever candidate set the test has
    /// pre-loaded for the *exact* gate value the tick passes in.
    /// Mirrors the shape of `QaQueueRepository`: the gate changes the
    /// result; absent values return empty.
    type GateMap = Vec<(bool, Vec<QaCandidate>)>;

    #[derive(Default)]
    struct FakeSource {
        // require_no_open_blockers → list
        by_gates: StdMutex<GateMap>,
        fail_next: StdMutex<bool>,
    }

    impl FakeSource {
        fn new() -> Self {
            Self::default()
        }

        fn set(&self, gates: QaGates, candidates: Vec<QaCandidate>) {
            let key = gates.require_no_open_blockers;
            let mut by = self.by_gates.lock().unwrap();
            by.retain(|(k, _)| *k != key);
            by.push((key, candidates));
        }

        fn arm_failure(&self) {
            *self.fail_next.lock().unwrap() = true;
        }

        fn clear(&self) {
            self.by_gates.lock().unwrap().clear();
        }
    }

    impl QaQueueSource for FakeSource {
        fn list_ready(&self, gates: QaGates) -> Result<Vec<QaCandidate>, QaQueueError> {
            let mut fail = self.fail_next.lock().unwrap();
            if *fail {
                *fail = false;
                return Err(QaQueueError::Source("scripted failure".into()));
            }
            let key = gates.require_no_open_blockers;
            let by = self.by_gates.lock().unwrap();
            Ok(by
                .iter()
                .find(|(k, _)| *k == key)
                .map(|(_, v)| v.clone())
                .unwrap_or_default())
        }
    }

    fn cadence() -> QueueTickCadence {
        QueueTickCadence::fixed(Duration::from_millis(1))
    }

    fn candidate(id: i64, identifier: &str, cause: QaRequestCause) -> QaCandidate {
        QaCandidate {
            work_item_id: WorkItemId::new(id),
            identifier: identifier.into(),
            title: format!("title {identifier}"),
            cause,
        }
    }

    fn build_tick(
        source: Arc<FakeSource>,
        gates: QaGates,
    ) -> (QaQueueTick, Arc<FakeSource>, Arc<QaDispatchQueue>) {
        let queue = Arc::new(QaDispatchQueue::new());
        let tick = QaQueueTick::new(
            source.clone() as Arc<dyn QaQueueSource>,
            gates,
            queue.clone(),
            cadence(),
        );
        (tick, source, queue)
    }

    #[tokio::test]
    async fn ready_candidates_are_emitted_under_default_gates() {
        let source = Arc::new(FakeSource::new());
        source.set(
            QaGates::default(),
            vec![
                candidate(1, "PROJ-1", QaRequestCause::DirectQaRequest),
                candidate(2, "PROJ-2", QaRequestCause::IntegrationConsolidated),
            ],
        );
        let (mut t, _src, q) = build_tick(source, QaGates::default());

        let outcome = t.tick().await;
        assert_eq!(outcome.queue, LogicalQueue::Qa);
        assert_eq!(outcome.considered, 2);
        assert_eq!(outcome.processed, 2);
        assert_eq!(outcome.deferred, 0);
        assert_eq!(outcome.errors, 0);

        let drained = q.drain();
        assert_eq!(drained.len(), 2);
        assert_eq!(drained[0].work_item_id, WorkItemId::new(1));
        assert_eq!(drained[0].cause, QaRequestCause::DirectQaRequest);
        assert_eq!(drained[1].work_item_id, WorkItemId::new(2));
        assert_eq!(drained[1].cause, QaRequestCause::IntegrationConsolidated);
    }

    #[tokio::test]
    async fn blocked_subtree_yields_empty_emission_under_default_gates() {
        // Source mirrors what `QaQueueRepository` does when the subtree
        // has an open blocker: returns empty under the default
        // (gate-on) view.
        let source = Arc::new(FakeSource::new());
        source.set(QaGates::default(), Vec::new());
        let (mut t, _src, q) = build_tick(source, QaGates::default());

        let outcome = t.tick().await;
        assert_eq!(outcome.queue, LogicalQueue::Qa);
        assert_eq!(outcome.considered, 0);
        assert_eq!(outcome.processed, 0);
        assert_eq!(outcome.deferred, 0);
        assert!(q.is_empty());
        assert!(outcome.is_idle());
    }

    #[tokio::test]
    async fn waived_blocker_gate_surfaces_previously_blocked_item() {
        // Default gate: empty. Waived blocker gate: one work item.
        let source = Arc::new(FakeSource::new());
        let waived = QaGates {
            require_no_open_blockers: false,
        };
        source.set(QaGates::default(), Vec::new());
        source.set(
            waived,
            vec![candidate(7, "PROJ-7", QaRequestCause::DirectQaRequest)],
        );

        // Tick wired with default gates sees nothing.
        let (mut t_default, _, q_default) = build_tick(source.clone(), QaGates::default());
        let o = t_default.tick().await;
        assert_eq!(o.processed, 0);
        assert!(q_default.is_empty());

        // A second tick wired with the waived gate sees the item.
        let (mut t_waived, _, q_waived) = build_tick(source.clone(), waived);
        let o = t_waived.tick().await;
        assert_eq!(o.processed, 1);
        let drained = q_waived.drain();
        assert_eq!(drained.len(), 1);
        assert_eq!(drained[0].work_item_id, WorkItemId::new(7));
    }

    #[tokio::test]
    async fn already_claimed_item_is_not_re_emitted() {
        let source = Arc::new(FakeSource::new());
        source.set(
            QaGates::default(),
            vec![candidate(1, "PROJ-1", QaRequestCause::DirectQaRequest)],
        );
        let (mut t, _src, q) = build_tick(source, QaGates::default());

        let o1 = t.tick().await;
        assert_eq!(o1.processed, 1);
        assert_eq!(q.drain().len(), 1);

        // Re-tick with the same source: still ready upstream but
        // already claimed, so it must be deferred.
        let o2 = t.tick().await;
        assert_eq!(o2.considered, 1);
        assert_eq!(o2.processed, 0);
        assert_eq!(o2.deferred, 1);
        assert!(q.is_empty());
        assert_eq!(t.claimed_count(), 1);
    }

    #[tokio::test]
    async fn claim_pruned_when_item_leaves_source() {
        let source = Arc::new(FakeSource::new());
        source.set(
            QaGates::default(),
            vec![candidate(1, "PROJ-1", QaRequestCause::DirectQaRequest)],
        );
        let (mut t, _src, q) = build_tick(source.clone(), QaGates::default());

        t.tick().await;
        assert_eq!(t.claimed_count(), 1);
        let _ = q.drain();

        // Source no longer surfaces the item (e.g. runner promoted it
        // past `qa` or a verdict was filed): claim must drop.
        source.clear();
        source.set(QaGates::default(), Vec::new());
        let o = t.tick().await;
        assert_eq!(o.considered, 0);
        assert_eq!(t.claimed_count(), 0);

        // Same id reappears later (e.g. re-integration after rework):
        // must be eligible again.
        source.clear();
        source.set(
            QaGates::default(),
            vec![candidate(
                1,
                "PROJ-1",
                QaRequestCause::IntegrationConsolidated,
            )],
        );
        let o = t.tick().await;
        assert_eq!(o.processed, 1);
        let drained = q.drain();
        assert_eq!(drained.len(), 1);
        assert_eq!(drained[0].work_item_id, WorkItemId::new(1));
        assert_eq!(drained[0].cause, QaRequestCause::IntegrationConsolidated);
    }

    #[tokio::test]
    async fn source_error_increments_errors_and_emits_nothing() {
        let source = Arc::new(FakeSource::new());
        source.set(
            QaGates::default(),
            vec![candidate(1, "PROJ-1", QaRequestCause::DirectQaRequest)],
        );
        let (mut t, src, q) = build_tick(source, QaGates::default());

        src.arm_failure();
        let o = t.tick().await;
        assert_eq!(o.queue, LogicalQueue::Qa);
        assert_eq!(o.errors, 1);
        assert_eq!(o.processed, 0);
        assert_eq!(o.considered, 0);
        assert!(q.is_empty());
        assert_eq!(t.claimed_count(), 0);
    }

    #[tokio::test]
    async fn fifo_emission_order_matches_source_order() {
        let source = Arc::new(FakeSource::new());
        source.set(
            QaGates::default(),
            vec![
                candidate(5, "PROJ-5", QaRequestCause::DirectQaRequest),
                candidate(2, "PROJ-2", QaRequestCause::IntegrationConsolidated),
                candidate(9, "PROJ-9", QaRequestCause::DirectQaRequest),
            ],
        );
        let (mut t, _src, q) = build_tick(source, QaGates::default());

        t.tick().await;
        let drained = q.drain();
        let ids: Vec<i64> = drained.iter().map(|d| d.work_item_id.get()).collect();
        assert_eq!(ids, vec![5, 2, 9]);
    }

    #[tokio::test]
    async fn tick_reports_qa_queue_and_configured_cadence() {
        let source = Arc::new(FakeSource::new());
        let q = Arc::new(QaDispatchQueue::new());
        let cad = QueueTickCadence::from_millis(2_500, 100);
        let t = QaQueueTick::new(source as Arc<dyn QaQueueSource>, QaGates::default(), q, cad);
        assert_eq!(t.queue(), LogicalQueue::Qa);
        assert_eq!(t.cadence(), cad);
        assert_eq!(t.gates(), QaGates::default());
    }

    #[tokio::test]
    async fn drives_through_dyn_queue_tick_trait_object() {
        let source = Arc::new(FakeSource::new());
        source.set(
            QaGates::default(),
            vec![candidate(1, "PROJ-1", QaRequestCause::DirectQaRequest)],
        );
        let (t, _src, q) = build_tick(source, QaGates::default());
        let mut boxed: Box<dyn QueueTick> = Box::new(t);
        let outcomes = run_queue_tick_n(boxed.as_mut(), 3).await;
        assert_eq!(outcomes.len(), 3);
        for o in &outcomes {
            assert_eq!(o.queue, LogicalQueue::Qa);
        }
        // Tick 1 emits, ticks 2/3 see it claimed and defer.
        assert_eq!(outcomes[0].processed, 1);
        assert_eq!(outcomes[1].processed, 0);
        assert_eq!(outcomes[1].deferred, 1);
        assert_eq!(outcomes[2].processed, 0);
        assert_eq!(outcomes[2].deferred, 1);
        assert_eq!(q.len(), 1);
    }

    #[tokio::test]
    async fn dispatch_queue_drain_returns_fifo_and_empties_queue() {
        let q = QaDispatchQueue::new();
        q.enqueue(QaDispatchRequest {
            work_item_id: WorkItemId::new(1),
            identifier: "PROJ-1".into(),
            title: "first".into(),
            cause: QaRequestCause::DirectQaRequest,
            run_id: None,
        });
        q.enqueue(QaDispatchRequest {
            work_item_id: WorkItemId::new(2),
            identifier: "PROJ-2".into(),
            title: "second".into(),
            cause: QaRequestCause::IntegrationConsolidated,
            run_id: None,
        });
        assert_eq!(q.len(), 2);
        let drained = q.drain();
        assert_eq!(drained.len(), 2);
        assert_eq!(drained[0].work_item_id, WorkItemId::new(1));
        assert_eq!(drained[1].work_item_id, WorkItemId::new(2));
        assert!(q.is_empty());
    }

    #[test]
    fn default_gates_match_spec() {
        let gates = QaGates::default();
        assert!(gates.require_no_open_blockers);
    }

    #[test]
    fn gates_round_trip_through_json() {
        for gates in [
            QaGates::default(),
            QaGates {
                require_no_open_blockers: false,
            },
        ] {
            let json = serde_json::to_string(&gates).unwrap();
            let back: QaGates = serde_json::from_str(&json).unwrap();
            assert_eq!(back, gates);
        }
    }

    #[test]
    fn candidate_round_trips_through_json() {
        let c = candidate(42, "PROJ-42", QaRequestCause::IntegrationConsolidated);
        let json = serde_json::to_string(&c).unwrap();
        let back: QaCandidate = serde_json::from_str(&json).unwrap();
        assert_eq!(back, c);
    }

    #[test]
    fn dispatch_request_round_trips_through_json() {
        let r = QaDispatchRequest {
            work_item_id: WorkItemId::new(7),
            identifier: "PROJ-7".into(),
            title: "verify".into(),
            cause: QaRequestCause::DirectQaRequest,
            run_id: None,
        };
        let json = serde_json::to_string(&r).unwrap();
        let back: QaDispatchRequest = serde_json::from_str(&json).unwrap();
        assert_eq!(back, r);
    }

    #[test]
    fn dispatch_request_round_trips_when_run_id_present() {
        let r = QaDispatchRequest {
            work_item_id: WorkItemId::new(8),
            identifier: "PROJ-8".into(),
            title: "verify".into(),
            cause: QaRequestCause::IntegrationConsolidated,
            run_id: Some(crate::blocker::RunRef::new(123)),
        };
        let json = serde_json::to_string(&r).unwrap();
        assert!(json.contains("\"run_id\":123"), "json={json}");
        let back: QaDispatchRequest = serde_json::from_str(&json).unwrap();
        assert_eq!(back, r);
    }
}
