//! Integration [`QueueTick`] — drains parents the integration owner
//! should consolidate, gated by [`IntegrationGates`]
//! (SPEC v2 §5.10, §6.4 / ARCHITECTURE v2 §7.1, §9).
//!
//! Phase 11 of the v2 checklist replaces the flat poll loop with a fan
//! of logical queues. The integration tick consumes one upstream signal
//! — the durable `IntegrationQueueRepository` view in `symphony-state`
//! — and emits one downstream artifact: an [`IntegrationDispatchRequest`]
//! per ready parent, claimed by `parent_id` so re-ticking the same view
//! does not re-emit.
//!
//! Core must not depend on `symphony-state`, so the upstream view is
//! abstracted through the [`IntegrationQueueSource`] trait. The
//! composition root supplies an adapter that calls
//! `IntegrationQueueRepository::list_ready_for_integration_with_gates`
//! and projects each row onto an [`IntegrationCandidate`]. Tests in this
//! module use a deterministic in-memory fake.
//!
//! Ready / blocked / waived behavior:
//!
//! * **Ready** — the source returns one or more candidates under the
//!   workflow's [`IntegrationGates`]; each is claimed and emitted.
//! * **Blocked** — the source returns an empty list under the default
//!   gates (e.g. a subtree blocker or a non-terminal child suppresses
//!   the parent). Nothing is emitted.
//! * **Waived** — the same parent reappears once the workflow waives
//!   the corresponding gate. The tick honors whichever
//!   [`IntegrationGates`] value it was constructed with; flipping a
//!   gate is the workflow's policy decision, not the tick's.
//!
//! Constructing a fully-formed [`crate::IntegrationRunRequest`] (with
//! workspace claim, child handoffs, merge strategy) belongs to the
//! integration runner downstream. The tick's job is to *surface* ready
//! parents and let the runner build the request when it actually
//! launches the agent.

use std::collections::HashSet;
use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use serde::{Deserialize, Serialize};

use crate::integration_request::{IntegrationGates, IntegrationRequestCause};
use crate::logical_queue::{LogicalQueue, QueueTickOutcome};
use crate::queue_tick::{QueueTick, QueueTickCadence};
use crate::work_item::WorkItemId;

/// One candidate surfaced by an [`IntegrationQueueSource`].
///
/// Minimal projection of the durable work item plus the queue's
/// readiness reason. Mirrors the on-the-wire shape of
/// `symphony_state::IntegrationQueueEntry` but lives in core so it can
/// flow through the queue tick without a state-crate dependency.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct IntegrationCandidate {
    /// Durable parent work item id.
    pub parent_id: WorkItemId,
    /// Tracker-facing identifier (e.g. `PROJ-42`).
    pub parent_identifier: String,
    /// Parent title, used by the dispatcher when building the run
    /// request and by status surfaces.
    pub parent_title: String,
    /// Why the upstream queue surfaced this parent.
    pub cause: IntegrationRequestCause,
}

/// Errors an [`IntegrationQueueSource`] may surface to the tick.
///
/// Kept opaque on purpose: the tick only needs to count errors, not
/// branch on them. State-crate adapters wrap their `StateError` into
/// the [`IntegrationQueueError::Source`] variant.
#[derive(Debug, thiserror::Error)]
pub enum IntegrationQueueError {
    /// Underlying source failed (state DB error, adapter error, …).
    #[error("integration queue source error: {0}")]
    Source(String),
}

/// Read-only view over the integration queue, abstracted so core does
/// not depend on `symphony-state`.
///
/// Implementations must respect the supplied [`IntegrationGates`]. The
/// SPEC defaults (both gates on) are enforced upstream by the state
/// crate; passing a non-default value reflects an explicit workflow
/// waiver.
pub trait IntegrationQueueSource: Send + Sync {
    /// Return every parent currently ready for the integration owner
    /// under the supplied gates, ordered FIFO. Order is preserved by
    /// the tick when emitting [`IntegrationDispatchRequest`]s.
    fn list_ready(
        &self,
        gates: IntegrationGates,
    ) -> Result<Vec<IntegrationCandidate>, IntegrationQueueError>;
}

/// One dispatch request emitted by the integration queue tick.
///
/// Plain serializable data so the eventual scheduler can persist and
/// replay it on restart. The downstream integration runner is
/// responsible for building the full
/// [`crate::IntegrationRunRequest`] (workspace claim, merge strategy,
/// child handoffs); the tick only signals *which* parent is ready and
/// *why*.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct IntegrationDispatchRequest {
    /// Durable parent work item id. Stable claim key.
    pub parent_id: WorkItemId,
    /// Tracker-facing identifier for the parent (e.g. `PROJ-42`).
    pub parent_identifier: String,
    /// Parent title.
    pub parent_title: String,
    /// Why the queue surfaced this parent.
    pub cause: IntegrationRequestCause,
    /// Durable `runs` row this dispatch will animate, when known. The
    /// integration runner uses this to acquire/release the durable lease
    /// (CHECKLIST_v2 Phase 11). `None` for legacy producers that do not
    /// reserve a run row before queueing — those dispatches bypass lease
    /// acquisition.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub run_id: Option<crate::blocker::RunRef>,
}

/// Shared FIFO of pending [`IntegrationDispatchRequest`]s.
///
/// The integration tick writes; the eventual integration runner drains
/// via [`Self::drain`]. Order preserved: oldest emission first.
#[derive(Default, Debug)]
pub struct IntegrationDispatchQueue {
    inner: Mutex<Vec<IntegrationDispatchRequest>>,
}

impl IntegrationDispatchQueue {
    /// Construct an empty queue.
    pub fn new() -> Self {
        Self::default()
    }

    /// Number of pending dispatch requests.
    pub fn len(&self) -> usize {
        self.inner
            .lock()
            .expect("integration dispatch queue mutex poisoned")
            .len()
    }

    /// True iff no pending requests.
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// Snapshot the pending requests without removing them.
    pub fn snapshot(&self) -> Vec<IntegrationDispatchRequest> {
        self.inner
            .lock()
            .expect("integration dispatch queue mutex poisoned")
            .clone()
    }

    /// Drain every pending request in FIFO order.
    pub fn drain(&self) -> Vec<IntegrationDispatchRequest> {
        std::mem::take(
            &mut *self
                .inner
                .lock()
                .expect("integration dispatch queue mutex poisoned"),
        )
    }

    /// Append a new request. Pub-crate so only the integration tick
    /// (and its tests) can write; consumers read via [`Self::drain`].
    pub(crate) fn enqueue(&self, req: IntegrationDispatchRequest) {
        self.inner
            .lock()
            .expect("integration dispatch queue mutex poisoned")
            .push(req);
    }
}

/// Integration queue tick.
///
/// One tick = one snapshot of the upstream
/// [`IntegrationQueueSource`] under the workflow's
/// [`IntegrationGates`]. Bounded work, no awaits beyond the trait
/// signature (the source call is synchronous). Cadence and fan-out
/// belong to the multi-queue scheduler.
///
/// Claim semantics: a parent emitted in tick *N* is recorded in the
/// internal claim set so it does not re-emit on tick *N+1* while it is
/// still surfaced by the source. When the source stops returning the
/// parent — typically because the integration runner promoted the work
/// item past `status_class = integration`, or the gate flipped under
/// a config change — the claim is pruned and a future re-appearance is
/// eligible to dispatch again.
pub struct IntegrationQueueTick {
    source: Arc<dyn IntegrationQueueSource>,
    gates: IntegrationGates,
    queue: Arc<IntegrationDispatchQueue>,
    cadence: QueueTickCadence,
    claimed: Mutex<HashSet<WorkItemId>>,
}

impl IntegrationQueueTick {
    /// Construct an integration tick.
    pub fn new(
        source: Arc<dyn IntegrationQueueSource>,
        gates: IntegrationGates,
        queue: Arc<IntegrationDispatchQueue>,
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
    pub fn dispatch_queue(&self) -> &Arc<IntegrationDispatchQueue> {
        &self.queue
    }

    /// Workflow gates this tick was wired with.
    pub fn gates(&self) -> IntegrationGates {
        self.gates
    }

    /// Number of parent ids currently claimed (emitted and still
    /// returned by the source).
    pub fn claimed_count(&self) -> usize {
        self.claimed
            .lock()
            .expect("integration claim set mutex poisoned")
            .len()
    }
}

#[async_trait]
impl QueueTick for IntegrationQueueTick {
    fn queue(&self) -> LogicalQueue {
        LogicalQueue::Integration
    }

    fn cadence(&self) -> QueueTickCadence {
        self.cadence
    }

    async fn tick(&mut self) -> QueueTickOutcome {
        let candidates = match self.source.list_ready(self.gates) {
            Ok(c) => c,
            Err(_e) => {
                return QueueTickOutcome {
                    queue: LogicalQueue::Integration,
                    considered: 0,
                    processed: 0,
                    deferred: 0,
                    errors: 1,
                };
            }
        };
        let considered = candidates.len();

        // Prune claims for parents the source no longer surfaces. A
        // parent that left the ready set (e.g. promoted past
        // `integration`, or now hidden behind a gate) becomes eligible
        // to re-dispatch when it reappears.
        let active_ids: HashSet<WorkItemId> = candidates.iter().map(|c| c.parent_id).collect();
        {
            let mut claimed = self
                .claimed
                .lock()
                .expect("integration claim set mutex poisoned");
            claimed.retain(|id| active_ids.contains(id));
        }

        let mut processed = 0usize;
        let mut deferred = 0usize;

        for c in candidates {
            let already_claimed = {
                let claimed = self
                    .claimed
                    .lock()
                    .expect("integration claim set mutex poisoned");
                claimed.contains(&c.parent_id)
            };
            if already_claimed {
                deferred += 1;
                continue;
            }

            self.queue.enqueue(IntegrationDispatchRequest {
                parent_id: c.parent_id,
                parent_identifier: c.parent_identifier,
                parent_title: c.parent_title,
                cause: c.cause,
                run_id: None,
            });
            self.claimed
                .lock()
                .expect("integration claim set mutex poisoned")
                .insert(c.parent_id);
            processed += 1;
        }

        QueueTickOutcome {
            queue: LogicalQueue::Integration,
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
    /// pre-loaded for the *exact* gate combination the tick passes in.
    /// Mirrors the shape of `IntegrationQueueRepository`: gates change
    /// the result; absent combinations return empty.
    type GateKey = (bool, bool);
    type GateMap = Vec<(GateKey, Vec<IntegrationCandidate>)>;

    #[derive(Default)]
    struct FakeSource {
        // (require_all_children_terminal, require_no_open_blockers) → list
        by_gates: StdMutex<GateMap>,
        fail_next: StdMutex<bool>,
    }

    impl FakeSource {
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

        fn arm_failure(&self) {
            *self.fail_next.lock().unwrap() = true;
        }

        fn clear(&self) {
            self.by_gates.lock().unwrap().clear();
        }
    }

    impl IntegrationQueueSource for FakeSource {
        fn list_ready(
            &self,
            gates: IntegrationGates,
        ) -> Result<Vec<IntegrationCandidate>, IntegrationQueueError> {
            let mut fail = self.fail_next.lock().unwrap();
            if *fail {
                *fail = false;
                return Err(IntegrationQueueError::Source("scripted failure".into()));
            }
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

    fn cadence() -> QueueTickCadence {
        QueueTickCadence::fixed(Duration::from_millis(1))
    }

    fn candidate(
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

    fn build_tick(
        source: Arc<FakeSource>,
        gates: IntegrationGates,
    ) -> (
        IntegrationQueueTick,
        Arc<FakeSource>,
        Arc<IntegrationDispatchQueue>,
    ) {
        let queue = Arc::new(IntegrationDispatchQueue::new());
        let tick = IntegrationQueueTick::new(
            source.clone() as Arc<dyn IntegrationQueueSource>,
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
            IntegrationGates::default(),
            vec![
                candidate(
                    1,
                    "PROJ-1",
                    IntegrationRequestCause::DirectIntegrationRequest,
                ),
                candidate(2, "PROJ-2", IntegrationRequestCause::AllChildrenTerminal),
            ],
        );
        let (mut t, _src, q) = build_tick(source, IntegrationGates::default());

        let outcome = t.tick().await;
        assert_eq!(outcome.queue, LogicalQueue::Integration);
        assert_eq!(outcome.considered, 2);
        assert_eq!(outcome.processed, 2);
        assert_eq!(outcome.deferred, 0);
        assert_eq!(outcome.errors, 0);

        let drained = q.drain();
        assert_eq!(drained.len(), 2);
        assert_eq!(drained[0].parent_id, WorkItemId::new(1));
        assert_eq!(
            drained[0].cause,
            IntegrationRequestCause::DirectIntegrationRequest
        );
        assert_eq!(drained[1].parent_id, WorkItemId::new(2));
        assert_eq!(
            drained[1].cause,
            IntegrationRequestCause::AllChildrenTerminal
        );
    }

    #[tokio::test]
    async fn blocked_subtree_yields_empty_emission_under_default_gates() {
        // Source mirrors what `IntegrationQueueRepository` does when
        // the subtree has an open blocker: returns empty under the
        // default (gates-on) view.
        let source = Arc::new(FakeSource::new());
        source.set(IntegrationGates::default(), Vec::new());
        let (mut t, _src, q) = build_tick(source, IntegrationGates::default());

        let outcome = t.tick().await;
        assert_eq!(outcome.queue, LogicalQueue::Integration);
        assert_eq!(outcome.considered, 0);
        assert_eq!(outcome.processed, 0);
        assert_eq!(outcome.deferred, 0);
        assert!(q.is_empty());
        assert!(outcome.is_idle());
    }

    #[tokio::test]
    async fn waived_blocker_gate_surfaces_previously_blocked_parent() {
        // Default gates: empty. Waived blocker gate: one parent.
        let source = Arc::new(FakeSource::new());
        let waived = IntegrationGates {
            require_all_children_terminal: true,
            require_no_open_blockers: false,
        };
        source.set(IntegrationGates::default(), Vec::new());
        source.set(
            waived,
            vec![candidate(
                7,
                "PROJ-7",
                IntegrationRequestCause::DirectIntegrationRequest,
            )],
        );

        // Tick wired with default gates sees nothing.
        let (mut t_default, _, q_default) = build_tick(source.clone(), IntegrationGates::default());
        let o = t_default.tick().await;
        assert_eq!(o.processed, 0);
        assert!(q_default.is_empty());

        // A second tick wired with the waived gate sees the parent.
        let (mut t_waived, _, q_waived) = build_tick(source.clone(), waived);
        let o = t_waived.tick().await;
        assert_eq!(o.processed, 1);
        let drained = q_waived.drain();
        assert_eq!(drained.len(), 1);
        assert_eq!(drained[0].parent_id, WorkItemId::new(7));
    }

    #[tokio::test]
    async fn waived_children_terminal_gate_surfaces_decomposed_parent() {
        let source = Arc::new(FakeSource::new());
        let waived = IntegrationGates {
            require_all_children_terminal: false,
            require_no_open_blockers: true,
        };
        source.set(IntegrationGates::default(), Vec::new());
        source.set(
            waived,
            vec![candidate(
                3,
                "PROJ-3",
                IntegrationRequestCause::AllChildrenTerminal,
            )],
        );
        let (mut t, _, q) = build_tick(source, waived);
        let outcome = t.tick().await;
        assert_eq!(outcome.processed, 1);
        let drained = q.drain();
        assert_eq!(drained[0].parent_id, WorkItemId::new(3));
        assert_eq!(
            drained[0].cause,
            IntegrationRequestCause::AllChildrenTerminal
        );
    }

    #[tokio::test]
    async fn already_claimed_parent_is_not_re_emitted() {
        let source = Arc::new(FakeSource::new());
        source.set(
            IntegrationGates::default(),
            vec![candidate(
                1,
                "PROJ-1",
                IntegrationRequestCause::DirectIntegrationRequest,
            )],
        );
        let (mut t, _src, q) = build_tick(source, IntegrationGates::default());

        let o1 = t.tick().await;
        assert_eq!(o1.processed, 1);
        assert_eq!(q.drain().len(), 1);

        // Re-tick with the same source: the parent is still ready
        // upstream but already claimed, so it must be deferred.
        let o2 = t.tick().await;
        assert_eq!(o2.considered, 1);
        assert_eq!(o2.processed, 0);
        assert_eq!(o2.deferred, 1);
        assert!(q.is_empty());
        assert_eq!(t.claimed_count(), 1);
    }

    #[tokio::test]
    async fn claim_pruned_when_parent_leaves_source() {
        let source = Arc::new(FakeSource::new());
        source.set(
            IntegrationGates::default(),
            vec![candidate(
                1,
                "PROJ-1",
                IntegrationRequestCause::DirectIntegrationRequest,
            )],
        );
        let (mut t, _src, q) = build_tick(source.clone(), IntegrationGates::default());

        t.tick().await;
        assert_eq!(t.claimed_count(), 1);
        let _ = q.drain();

        // Source no longer surfaces the parent (e.g. runner promoted
        // it past `integration`): claim must drop.
        source.clear();
        source.set(IntegrationGates::default(), Vec::new());
        let o = t.tick().await;
        assert_eq!(o.considered, 0);
        assert_eq!(t.claimed_count(), 0);

        // Same id reappears later: must be eligible again.
        source.clear();
        source.set(
            IntegrationGates::default(),
            vec![candidate(
                1,
                "PROJ-1",
                IntegrationRequestCause::DirectIntegrationRequest,
            )],
        );
        let o = t.tick().await;
        assert_eq!(o.processed, 1);
        let drained = q.drain();
        assert_eq!(drained.len(), 1);
        assert_eq!(drained[0].parent_id, WorkItemId::new(1));
    }

    #[tokio::test]
    async fn source_error_increments_errors_and_emits_nothing() {
        let source = Arc::new(FakeSource::new());
        source.set(
            IntegrationGates::default(),
            vec![candidate(
                1,
                "PROJ-1",
                IntegrationRequestCause::DirectIntegrationRequest,
            )],
        );
        let (mut t, src, q) = build_tick(source, IntegrationGates::default());

        src.arm_failure();
        let o = t.tick().await;
        assert_eq!(o.queue, LogicalQueue::Integration);
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
            IntegrationGates::default(),
            vec![
                candidate(
                    5,
                    "PROJ-5",
                    IntegrationRequestCause::DirectIntegrationRequest,
                ),
                candidate(2, "PROJ-2", IntegrationRequestCause::AllChildrenTerminal),
                candidate(
                    9,
                    "PROJ-9",
                    IntegrationRequestCause::DirectIntegrationRequest,
                ),
            ],
        );
        let (mut t, _src, q) = build_tick(source, IntegrationGates::default());

        t.tick().await;
        let drained = q.drain();
        let ids: Vec<i64> = drained.iter().map(|d| d.parent_id.get()).collect();
        assert_eq!(ids, vec![5, 2, 9]);
    }

    #[tokio::test]
    async fn tick_reports_integration_queue_and_configured_cadence() {
        let source = Arc::new(FakeSource::new());
        let q = Arc::new(IntegrationDispatchQueue::new());
        let cad = QueueTickCadence::from_millis(2_500, 100);
        let t = IntegrationQueueTick::new(
            source as Arc<dyn IntegrationQueueSource>,
            IntegrationGates::default(),
            q,
            cad,
        );
        assert_eq!(t.queue(), LogicalQueue::Integration);
        assert_eq!(t.cadence(), cad);
        assert_eq!(t.gates(), IntegrationGates::default());
    }

    #[tokio::test]
    async fn drives_through_dyn_queue_tick_trait_object() {
        let source = Arc::new(FakeSource::new());
        source.set(
            IntegrationGates::default(),
            vec![candidate(
                1,
                "PROJ-1",
                IntegrationRequestCause::DirectIntegrationRequest,
            )],
        );
        let (t, _src, q) = build_tick(source, IntegrationGates::default());
        let mut boxed: Box<dyn QueueTick> = Box::new(t);
        let outcomes = run_queue_tick_n(boxed.as_mut(), 3).await;
        assert_eq!(outcomes.len(), 3);
        for o in &outcomes {
            assert_eq!(o.queue, LogicalQueue::Integration);
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
        let q = IntegrationDispatchQueue::new();
        q.enqueue(IntegrationDispatchRequest {
            parent_id: WorkItemId::new(1),
            parent_identifier: "PROJ-1".into(),
            parent_title: "first".into(),
            cause: IntegrationRequestCause::DirectIntegrationRequest,
            run_id: None,
        });
        q.enqueue(IntegrationDispatchRequest {
            parent_id: WorkItemId::new(2),
            parent_identifier: "PROJ-2".into(),
            parent_title: "second".into(),
            cause: IntegrationRequestCause::AllChildrenTerminal,
            run_id: None,
        });
        assert_eq!(q.len(), 2);
        let drained = q.drain();
        assert_eq!(drained.len(), 2);
        assert_eq!(drained[0].parent_id, WorkItemId::new(1));
        assert_eq!(drained[1].parent_id, WorkItemId::new(2));
        assert!(q.is_empty());
    }

    #[test]
    fn candidate_round_trips_through_json() {
        let c = candidate(42, "PROJ-42", IntegrationRequestCause::AllChildrenTerminal);
        let json = serde_json::to_string(&c).unwrap();
        let back: IntegrationCandidate = serde_json::from_str(&json).unwrap();
        assert_eq!(back, c);
    }

    #[test]
    fn dispatch_request_round_trips_through_json() {
        let r = IntegrationDispatchRequest {
            parent_id: WorkItemId::new(7),
            parent_identifier: "PROJ-7".into(),
            parent_title: "consolidate".into(),
            cause: IntegrationRequestCause::DirectIntegrationRequest,
            run_id: None,
        };
        let json = serde_json::to_string(&r).unwrap();
        let back: IntegrationDispatchRequest = serde_json::from_str(&json).unwrap();
        assert_eq!(back, r);
    }

    #[test]
    fn dispatch_request_round_trips_when_run_id_present() {
        let r = IntegrationDispatchRequest {
            parent_id: WorkItemId::new(8),
            parent_identifier: "PROJ-8".into(),
            parent_title: "consolidate".into(),
            cause: IntegrationRequestCause::AllChildrenTerminal,
            run_id: Some(crate::blocker::RunRef::new(123)),
        };
        let json = serde_json::to_string(&r).unwrap();
        assert!(json.contains("\"run_id\":123"), "json={json}");
        let back: IntegrationDispatchRequest = serde_json::from_str(&json).unwrap();
        assert_eq!(back, r);
    }
}
