//! Per-pass dedup of [`crate::retry::RetryPolicyDecision::BudgetExceeded`]
//! decisions into durable [`OrchestratorEvent::BudgetExceeded`] events,
//! with side-effect bridging into [`BudgetPauseDispatchQueue`].
//!
//! # Why this lives in core
//!
//! The retry queue ([`crate::retry`]) returns
//! [`crate::retry::RetryPolicyDecision::BudgetExceeded`] when an issue's
//! `attempt` exceeds the configured `budgets.max_retries`. That decision
//! is the *kernel-side* truth that the cap tripped, but it is not yet:
//!
//! 1. an enqueued [`BudgetPauseDispatchRequest`] that the budget-pause
//!    runner can pick up,
//! 2. a durable [`OrchestratorEvent::BudgetExceeded`] history record, or
//! 3. a broadcast event on the [`EventBus`] for SSE/TUI subscribers.
//!
//! [`BudgetExceededBridge`] is the kernel-side glue that turns one
//! decision per retry-driving pass into all three at once, while
//! preserving the same per-episode dedup contract that
//! [`crate::scope_contention_log::ScopeContentionEventLog`] enforces for
//! [`OrchestratorEvent::ScopeCapReached`]: events fire on the leading
//! edge of a contention episode, never during, never on the trailing
//! edge.
//!
//! ## Episode keying
//!
//! Episodes are keyed on `(ContentionSubject, budget_kind)`. For
//! `max_retries` the typical caller passes
//! `ContentionSubject::Identifier(issue_id)` so a long stream of
//! cap-exceeded ticks against the same issue dedups to one event, even
//! when individual run rows differ. Other budget kinds (cost, turns) can
//! pick a different subject if their dedup granularity is per-run.
//!
//! ## Idempotency of the dispatch queue
//!
//! [`BudgetPauseDispatchQueue`]'s enqueue is a raw push — it has no
//! built-in `(budget_kind, identifier)` dedup. Idempotency is provided
//! by the bridge: only the *first* observation of a `(subject,
//! budget_kind)` episode emits an event, and the bridge enqueues the
//! pause request only when an event is emitted. Repeated over-cap calls
//! within the same episode are silently dropped at the dedup step, so
//! the queue does not grow.

use std::sync::{Arc, Mutex};

use std::collections::HashSet;

use crate::blocker::RunRef;
use crate::budget_pause_tick::{
    BudgetPauseDispatchQueue, BudgetPauseDispatchRequest, BudgetPauseId,
};
use crate::event_bus::EventBus;
use crate::events::OrchestratorEvent;
use crate::retry::RetryPolicyDecision;
use crate::scope_contention_log::ContentionSubject;
use crate::work_item::WorkItemId;

/// One observation of a budget-exceeded condition.
///
/// Subject identity is decoupled from event payload identity on purpose:
/// dedup may happen at issue granularity (`subject =
/// Identifier(issue_id)`) even when a specific run row tripped the cap
/// (`run_id = Some(...)`).
#[derive(Debug, Clone, PartialEq)]
pub struct BudgetExceededObservation {
    /// Episode subject for dedup keying.
    pub subject: ContentionSubject,
    /// Stable string discriminator for the cap (e.g. `"max_retries"`).
    pub budget_kind: String,
    /// Tracker-facing identifier carried on the durable event payload.
    pub identifier: String,
    /// Optional durable run row that tripped the cap.
    pub run_id: Option<i64>,
    /// Observed value at exceedance, in the cap's natural units.
    pub observed: f64,
    /// Configured cap, in the same units as `observed`.
    pub cap: f64,
}

/// Composite key identifying one budget-exceeded episode.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
struct EpisodeKey {
    subject: ContentionSubject,
    budget_kind: String,
}

/// Per-pass dedup state mapping [`BudgetExceededObservation`]s to
/// durable [`OrchestratorEvent::BudgetExceeded`] events.
///
/// Mirrors [`crate::scope_contention_log::ScopeContentionEventLog`]: at
/// most one event per `(subject, budget_kind)` episode, where an episode
/// is the unbroken span of consecutive passes in which the same key is
/// observed.
#[derive(Debug, Default, Clone)]
pub struct BudgetExceededEventLog {
    active: HashSet<EpisodeKey>,
}

impl BudgetExceededEventLog {
    /// Empty log — no active episodes.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Process every observation from one pass; return the events to emit.
    ///
    /// Callers must pass the *complete* observation set for this pass;
    /// keys present last pass but absent now are treated as
    /// episode-ended. Duplicates within the same pass coalesce.
    pub fn observe_pass<I>(&mut self, observations: I) -> Vec<OrchestratorEvent>
    where
        I: IntoIterator<Item = BudgetExceededObservation>,
    {
        let mut current: HashSet<EpisodeKey> = HashSet::new();
        let mut events = Vec::new();
        for obs in observations {
            let key = EpisodeKey {
                subject: obs.subject.clone(),
                budget_kind: obs.budget_kind.clone(),
            };
            if !current.insert(key.clone()) {
                continue;
            }
            if !self.active.contains(&key) {
                events.push(OrchestratorEvent::BudgetExceeded {
                    run_id: obs.run_id,
                    identifier: obs.identifier,
                    budget_kind: obs.budget_kind,
                    observed: obs.observed,
                    cap: obs.cap,
                });
            }
        }
        self.active = current;
        events
    }

    /// Number of episodes currently active.
    #[must_use]
    pub fn active_episode_count(&self) -> usize {
        self.active.len()
    }
}

/// Backend-side error surfaced from a [`BudgetExceededEventSink`].
#[derive(Debug, thiserror::Error)]
#[error("budget exceeded sink backend error: {0}")]
pub struct BudgetExceededSinkError(pub String);

/// Kernel-side persistence trait for
/// [`OrchestratorEvent::BudgetExceeded`] events.
///
/// `symphony-core` does not depend on `symphony-state`, so the
/// composition root supplies an implementation that forwards into
/// `EventRepository::append_event`. A bridge constructed without a sink
/// skips persistence — the event is still broadcast on the bus and the
/// pause request is still enqueued.
pub trait BudgetExceededEventSink: Send + Sync {
    /// Persist a [`OrchestratorEvent::BudgetExceeded`] and return the
    /// assigned sequence. Implementations may panic-on-mismatch for
    /// other variants; the bridge only ever calls this with
    /// `BudgetExceeded`.
    fn append_budget_exceeded(
        &self,
        event: &OrchestratorEvent,
    ) -> Result<i64, BudgetExceededSinkError>;
}

/// Per-decision bridge input for retry-cap exceedances.
///
/// The retry-driving pass collects one of these per request whose
/// [`RetryPolicyDecision`] came back as
/// [`RetryPolicyDecision::BudgetExceeded`], pairs it with the decision,
/// and hands the pair stream to
/// [`BudgetExceededBridge::observe_retry_pass`].
#[derive(Debug, Clone, PartialEq)]
pub struct RetryBudgetBridgeRequest {
    /// Tracker-facing identifier for the work item the cap pertains to.
    /// Carried on the durable event payload and used as the typical
    /// dedup subject.
    pub identifier: String,
    /// Work item id for the [`BudgetPauseDispatchRequest`].
    pub work_item_id: WorkItemId,
    /// Durable budget-pause row id for the dispatch request. The state
    /// crate is expected to upsert a `budget_pauses` row by `(work_item,
    /// budget_kind)` before invoking the bridge and pass the row id
    /// here; on dedup the row already exists and remains untouched.
    pub pause_id: BudgetPauseId,
    /// Optional run row that tripped the cap. Carried into both the
    /// dispatch request (for lease acquisition) and the durable event
    /// payload.
    pub run_id: Option<i64>,
    /// Episode subject for dedup. Typical caller passes
    /// `ContentionSubject::Identifier(identifier.clone())` so
    /// per-issue dedup holds across distinct runs of the same issue.
    pub subject: ContentionSubject,
}

/// Stable budget-kind discriminator for retry-cap exceedances.
pub const MAX_RETRIES_BUDGET_KIND: &str = "max_retries";

/// Bridge that collapses retry-cap exceedances into durable +
/// dispatched + broadcast effects, with per-episode dedup.
///
/// Cheap to clone — internally an `Arc` over the inner state.
#[derive(Clone)]
pub struct BudgetExceededBridge {
    inner: Arc<Inner>,
}

struct Inner {
    log: Mutex<BudgetExceededEventLog>,
    bus: EventBus,
    sink: Option<Arc<dyn BudgetExceededEventSink>>,
    queue: Arc<BudgetPauseDispatchQueue>,
}

impl std::fmt::Debug for BudgetExceededBridge {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("BudgetExceededBridge")
            .field("has_sink", &self.inner.sink.is_some())
            .finish()
    }
}

impl BudgetExceededBridge {
    /// Build a bridge that broadcasts on `bus`, persists through `sink`
    /// when present, and enqueues pause dispatch requests onto `queue`.
    #[must_use]
    pub fn new(
        bus: EventBus,
        queue: Arc<BudgetPauseDispatchQueue>,
        sink: Option<Arc<dyn BudgetExceededEventSink>>,
    ) -> Self {
        Self {
            inner: Arc::new(Inner {
                log: Mutex::new(BudgetExceededEventLog::new()),
                bus,
                sink,
                queue,
            }),
        }
    }

    /// Process one retry-driving pass.
    ///
    /// Walks `(request, decision)` pairs once. Pairs whose decision is
    /// [`RetryPolicyDecision::ScheduleRetry`] are ignored (within budget
    /// — no event, no enqueue, no persist). Pairs whose decision is
    /// [`RetryPolicyDecision::BudgetExceeded`] are funnelled through
    /// the dedup log; the *first* pair of each `(subject, budget_kind)`
    /// episode produces:
    ///
    /// 1. an enqueue onto the pause dispatch queue,
    /// 2. a `BudgetExceeded` event broadcast on the bus,
    /// 3. a `BudgetExceeded` event persisted via the sink (when set).
    ///
    /// Subsequent pairs in the same episode are silently dropped at the
    /// dedup step; the queue does not grow and no duplicate events fire.
    /// Returns the events emitted this pass — useful for tests and for
    /// callers that want to forward them further.
    pub fn observe_retry_pass<I>(&self, items: I) -> Vec<OrchestratorEvent>
    where
        I: IntoIterator<Item = (RetryBudgetBridgeRequest, RetryPolicyDecision)>,
    {
        let mut observations: Vec<BudgetExceededObservation> = Vec::new();
        // Parallel arrays so we can correlate emitted events with the
        // request that produced them. The dedup primitive may drop
        // entries (re-occurrence of an active episode); we walk events
        // in order and pair each with the first matching pending entry
        // by (subject, budget_kind).
        let mut pending: Vec<(RetryBudgetBridgeRequest, f64, f64)> = Vec::new();

        for (req, decision) in items {
            if let RetryPolicyDecision::BudgetExceeded { observed, cap } = decision {
                observations.push(BudgetExceededObservation {
                    subject: req.subject.clone(),
                    budget_kind: MAX_RETRIES_BUDGET_KIND.to_owned(),
                    identifier: req.identifier.clone(),
                    run_id: req.run_id,
                    observed,
                    cap,
                });
                pending.push((req, observed, cap));
            }
        }

        let events = self.observe_observation_pass(observations);

        for event in &events {
            if let OrchestratorEvent::BudgetExceeded {
                identifier,
                budget_kind,
                ..
            } = event
                && let Some(idx) = pending.iter().position(|(req, _, _)| {
                    req.identifier == *identifier && budget_kind == MAX_RETRIES_BUDGET_KIND
                })
            {
                let (req, observed, cap) = pending.remove(idx);
                self.inner.queue.enqueue(BudgetPauseDispatchRequest {
                    pause_id: req.pause_id,
                    work_item_id: req.work_item_id,
                    budget_kind: MAX_RETRIES_BUDGET_KIND.to_owned(),
                    limit_value: cap,
                    observed,
                    run_id: req.run_id.map(RunRef::new),
                    role: None,
                    agent_profile: None,
                    repository: None,
                });
            }
        }

        events
    }

    /// Process a pass of pre-built observations.
    ///
    /// Lower-level entry point for callers that already have
    /// [`BudgetExceededObservation`]s in hand (e.g. budget kinds beyond
    /// `max_retries`). No dispatch-queue side effects — only dedup,
    /// broadcast, and persist. Use [`Self::observe_retry_pass`] for
    /// retry-cap exceedances that should also enqueue a pause.
    pub fn observe_observation_pass<I>(&self, observations: I) -> Vec<OrchestratorEvent>
    where
        I: IntoIterator<Item = BudgetExceededObservation>,
    {
        let events = {
            let mut log = self
                .inner
                .log
                .lock()
                .expect("budget exceeded log mutex poisoned");
            log.observe_pass(observations)
        };

        for event in &events {
            self.inner.bus.emit(event.clone());
            if let Some(sink) = &self.inner.sink
                && let Err(err) = sink.append_budget_exceeded(event)
            {
                tracing::warn!(
                    target: "symphony::budget_exceeded",
                    error = %err,
                    "failed to persist BudgetExceeded event",
                );
            }
        }

        events
    }

    /// Number of episodes currently active. Diagnostic accessor for
    /// tests.
    #[must_use]
    pub fn active_episode_count(&self) -> usize {
        self.inner
            .log
            .lock()
            .expect("budget exceeded log mutex poisoned")
            .active_episode_count()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::retry::{RetryEntry, RetryReason};
    use futures::StreamExt;
    use std::sync::Mutex as StdMutex;
    use std::time::Instant;

    #[derive(Default)]
    struct RecordingSink {
        events: StdMutex<Vec<OrchestratorEvent>>,
        next_seq: StdMutex<i64>,
    }

    impl RecordingSink {
        fn new() -> Arc<Self> {
            Arc::new(Self::default())
        }

        fn snapshot(&self) -> Vec<OrchestratorEvent> {
            self.events.lock().unwrap().clone()
        }
    }

    impl BudgetExceededEventSink for RecordingSink {
        fn append_budget_exceeded(
            &self,
            event: &OrchestratorEvent,
        ) -> Result<i64, BudgetExceededSinkError> {
            assert!(matches!(event, OrchestratorEvent::BudgetExceeded { .. }));
            self.events.lock().unwrap().push(event.clone());
            let mut seq = self.next_seq.lock().unwrap();
            *seq += 1;
            Ok(*seq)
        }
    }

    struct FailingSink;

    impl BudgetExceededEventSink for FailingSink {
        fn append_budget_exceeded(
            &self,
            _event: &OrchestratorEvent,
        ) -> Result<i64, BudgetExceededSinkError> {
            Err(BudgetExceededSinkError("boom".into()))
        }
    }

    fn req(
        identifier: &str,
        pause_id: i64,
        work: i64,
        run_id: Option<i64>,
    ) -> RetryBudgetBridgeRequest {
        RetryBudgetBridgeRequest {
            identifier: identifier.to_owned(),
            work_item_id: WorkItemId::new(work),
            pause_id: BudgetPauseId::new(pause_id),
            run_id,
            subject: ContentionSubject::Identifier(identifier.to_owned()),
        }
    }

    fn schedule_retry_decision() -> RetryPolicyDecision {
        // A within-budget decision used to prove non-cap pairs are ignored.
        RetryPolicyDecision::ScheduleRetry(RetryEntry {
            identifier: "X".into(),
            attempt: 1,
            due_at: Instant::now(),
            error: None,
            reason: RetryReason::Failure,
        })
    }

    #[test]
    fn log_emits_event_on_first_observation_only() {
        let mut log = BudgetExceededEventLog::new();
        let obs = BudgetExceededObservation {
            subject: ContentionSubject::Identifier("ENG-1".into()),
            budget_kind: MAX_RETRIES_BUDGET_KIND.into(),
            identifier: "ENG-1".into(),
            run_id: Some(7),
            observed: 4.0,
            cap: 3.0,
        };
        let e1 = log.observe_pass([obs.clone()]);
        assert_eq!(e1.len(), 1);
        if let OrchestratorEvent::BudgetExceeded {
            run_id,
            identifier,
            budget_kind,
            observed,
            cap,
        } = &e1[0]
        {
            assert_eq!(*run_id, Some(7));
            assert_eq!(identifier, "ENG-1");
            assert_eq!(budget_kind, MAX_RETRIES_BUDGET_KIND);
            assert_eq!(*observed, 4.0);
            assert_eq!(*cap, 3.0);
        } else {
            panic!("expected BudgetExceeded");
        }
        let e2 = log.observe_pass([obs.clone()]);
        assert!(e2.is_empty(), "second observation in same episode dedups");
        assert_eq!(log.active_episode_count(), 1);
    }

    #[test]
    fn log_episode_ends_on_idle_pass_then_re_emits_on_resume() {
        let mut log = BudgetExceededEventLog::new();
        let obs = BudgetExceededObservation {
            subject: ContentionSubject::Identifier("ENG-1".into()),
            budget_kind: MAX_RETRIES_BUDGET_KIND.into(),
            identifier: "ENG-1".into(),
            run_id: None,
            observed: 4.0,
            cap: 3.0,
        };
        assert_eq!(log.observe_pass([obs.clone()]).len(), 1);
        assert!(log.observe_pass([obs.clone()]).is_empty());
        // Idle pass — episode terminates.
        let idle = log.observe_pass(std::iter::empty());
        assert!(idle.is_empty());
        assert_eq!(log.active_episode_count(), 0);
        // Re-cap — fresh episode emits again.
        let resumed = log.observe_pass([obs]);
        assert_eq!(resumed.len(), 1);
    }

    #[test]
    fn log_distinct_budget_kinds_are_independent_episodes() {
        let mut log = BudgetExceededEventLog::new();
        let retries = BudgetExceededObservation {
            subject: ContentionSubject::Identifier("ENG-1".into()),
            budget_kind: MAX_RETRIES_BUDGET_KIND.into(),
            identifier: "ENG-1".into(),
            run_id: None,
            observed: 4.0,
            cap: 3.0,
        };
        let cost = BudgetExceededObservation {
            subject: ContentionSubject::Identifier("ENG-1".into()),
            budget_kind: "max_cost_per_issue_usd".into(),
            identifier: "ENG-1".into(),
            run_id: None,
            observed: 12.5,
            cap: 10.0,
        };
        let e = log.observe_pass([retries, cost]);
        assert_eq!(e.len(), 2);
    }

    #[test]
    fn log_duplicates_within_one_pass_coalesce() {
        let mut log = BudgetExceededEventLog::new();
        let obs = BudgetExceededObservation {
            subject: ContentionSubject::Identifier("ENG-1".into()),
            budget_kind: MAX_RETRIES_BUDGET_KIND.into(),
            identifier: "ENG-1".into(),
            run_id: None,
            observed: 4.0,
            cap: 3.0,
        };
        let e = log.observe_pass([obs.clone(), obs]);
        assert_eq!(e.len(), 1);
        assert_eq!(log.active_episode_count(), 1);
    }

    #[tokio::test]
    async fn bridge_first_cap_emits_enqueues_persists_broadcasts() {
        let bus = EventBus::default();
        let mut rx = bus.subscribe();
        let sink = RecordingSink::new();
        let queue = Arc::new(BudgetPauseDispatchQueue::new());
        let bx = BudgetExceededBridge::new(
            bus.clone(),
            queue.clone(),
            Some(sink.clone() as Arc<dyn BudgetExceededEventSink>),
        );

        let events = bx.observe_retry_pass([(
            req("ENG-1", 99, 100, Some(7)),
            RetryPolicyDecision::BudgetExceeded {
                observed: 4.0,
                cap: 3.0,
            },
        )]);
        assert_eq!(events.len(), 1);

        // Bus saw it.
        let bus_event = rx.next().await.expect("item").expect("not lagged");
        match bus_event {
            OrchestratorEvent::BudgetExceeded {
                run_id,
                identifier,
                budget_kind,
                observed,
                cap,
            } => {
                assert_eq!(run_id, Some(7));
                assert_eq!(identifier, "ENG-1");
                assert_eq!(budget_kind, MAX_RETRIES_BUDGET_KIND);
                assert_eq!(observed, 4.0);
                assert_eq!(cap, 3.0);
            }
            other => panic!("unexpected event: {other:?}"),
        }

        // Sink persisted it.
        let persisted = sink.snapshot();
        assert_eq!(persisted.len(), 1);

        // Queue holds the dispatch request keyed on ("max_retries", issue_id).
        let drained = queue.drain();
        assert_eq!(drained.len(), 1);
        assert_eq!(drained[0].pause_id, BudgetPauseId::new(99));
        assert_eq!(drained[0].work_item_id, WorkItemId::new(100));
        assert_eq!(drained[0].budget_kind, MAX_RETRIES_BUDGET_KIND);
        assert_eq!(drained[0].limit_value, 3.0);
        assert_eq!(drained[0].observed, 4.0);
        assert_eq!(drained[0].run_id, Some(RunRef::new(7)));
    }

    #[test]
    fn bridge_repeated_over_cap_in_same_episode_dedups_and_does_not_re_enqueue() {
        let bus = EventBus::default();
        let sink = RecordingSink::new();
        let queue = Arc::new(BudgetPauseDispatchQueue::new());
        let bx = BudgetExceededBridge::new(
            bus,
            queue.clone(),
            Some(sink.clone() as Arc<dyn BudgetExceededEventSink>),
        );

        for _ in 0..5 {
            bx.observe_retry_pass([(
                req("ENG-1", 99, 100, None),
                RetryPolicyDecision::BudgetExceeded {
                    observed: 4.0,
                    cap: 3.0,
                },
            )]);
        }
        assert_eq!(sink.snapshot().len(), 1, "exactly one persisted event");
        assert_eq!(queue.len(), 1, "exactly one enqueued pause request");
        assert_eq!(bx.active_episode_count(), 1);
    }

    #[test]
    fn bridge_idle_pass_terminates_episode_and_re_cap_re_emits() {
        let bus = EventBus::default();
        let sink = RecordingSink::new();
        let queue = Arc::new(BudgetPauseDispatchQueue::new());
        let bx = BudgetExceededBridge::new(
            bus,
            queue.clone(),
            Some(sink.clone() as Arc<dyn BudgetExceededEventSink>),
        );

        bx.observe_retry_pass([(
            req("ENG-1", 99, 100, None),
            RetryPolicyDecision::BudgetExceeded {
                observed: 4.0,
                cap: 3.0,
            },
        )]);
        // Idle pass — no decisions.
        bx.observe_retry_pass(std::iter::empty::<(
            RetryBudgetBridgeRequest,
            RetryPolicyDecision,
        )>());
        assert_eq!(bx.active_episode_count(), 0);

        // Re-cap on the same key — fresh episode.
        bx.observe_retry_pass([(
            req("ENG-1", 100, 100, None),
            RetryPolicyDecision::BudgetExceeded {
                observed: 5.0,
                cap: 3.0,
            },
        )]);

        assert_eq!(sink.snapshot().len(), 2, "two episodes => two events");
        let drained = queue.drain();
        assert_eq!(drained.len(), 2);
        assert_eq!(drained[0].pause_id, BudgetPauseId::new(99));
        assert_eq!(drained[1].pause_id, BudgetPauseId::new(100));
        assert_eq!(drained[1].observed, 5.0);
    }

    #[test]
    fn bridge_schedule_retry_decisions_are_ignored() {
        let bus = EventBus::default();
        let sink = RecordingSink::new();
        let queue = Arc::new(BudgetPauseDispatchQueue::new());
        let bx = BudgetExceededBridge::new(
            bus,
            queue.clone(),
            Some(sink.clone() as Arc<dyn BudgetExceededEventSink>),
        );

        let events =
            bx.observe_retry_pass([(req("ENG-1", 99, 100, None), schedule_retry_decision())]);
        assert!(events.is_empty());
        assert!(sink.snapshot().is_empty());
        assert!(queue.is_empty());
        assert_eq!(bx.active_episode_count(), 0);
    }

    #[test]
    fn bridge_distinct_subjects_in_one_pass_each_emit_and_enqueue() {
        let bus = EventBus::default();
        let sink = RecordingSink::new();
        let queue = Arc::new(BudgetPauseDispatchQueue::new());
        let bx = BudgetExceededBridge::new(
            bus,
            queue.clone(),
            Some(sink.clone() as Arc<dyn BudgetExceededEventSink>),
        );

        let events = bx.observe_retry_pass([
            (
                req("ENG-1", 1, 100, None),
                RetryPolicyDecision::BudgetExceeded {
                    observed: 4.0,
                    cap: 3.0,
                },
            ),
            (
                req("ENG-2", 2, 101, None),
                RetryPolicyDecision::BudgetExceeded {
                    observed: 4.0,
                    cap: 3.0,
                },
            ),
        ]);
        assert_eq!(events.len(), 2);
        assert_eq!(sink.snapshot().len(), 2);
        let drained = queue.drain();
        assert_eq!(drained.len(), 2);
        assert_eq!(drained[0].pause_id, BudgetPauseId::new(1));
        assert_eq!(drained[1].pause_id, BudgetPauseId::new(2));
    }

    #[test]
    fn bridge_missing_sink_skips_persistence_but_still_enqueues_and_broadcasts() {
        let bus = EventBus::default();
        let queue = Arc::new(BudgetPauseDispatchQueue::new());
        let bx = BudgetExceededBridge::new(bus, queue.clone(), None);
        let events = bx.observe_retry_pass([(
            req("ENG-1", 99, 100, None),
            RetryPolicyDecision::BudgetExceeded {
                observed: 4.0,
                cap: 3.0,
            },
        )]);
        assert_eq!(events.len(), 1);
        assert_eq!(queue.len(), 1);
    }

    #[test]
    fn bridge_sink_failure_does_not_panic_or_break_dedup() {
        let bus = EventBus::default();
        let queue = Arc::new(BudgetPauseDispatchQueue::new());
        let bx = BudgetExceededBridge::new(
            bus,
            queue.clone(),
            Some(Arc::new(FailingSink) as Arc<dyn BudgetExceededEventSink>),
        );

        // Two passes on the same episode. First emits + fails to persist;
        // second is deduped at the log level (no event, no enqueue),
        // proving the dedup state advances even when persistence errors.
        let first = bx.observe_retry_pass([(
            req("ENG-1", 99, 100, None),
            RetryPolicyDecision::BudgetExceeded {
                observed: 4.0,
                cap: 3.0,
            },
        )]);
        let second = bx.observe_retry_pass([(
            req("ENG-1", 99, 100, None),
            RetryPolicyDecision::BudgetExceeded {
                observed: 5.0,
                cap: 3.0,
            },
        )]);
        assert_eq!(first.len(), 1);
        assert!(second.is_empty());
        assert_eq!(queue.len(), 1, "only first pass enqueued");
    }

    #[test]
    fn bridge_idle_pass_with_no_active_episode_is_noop() {
        let bus = EventBus::default();
        let queue = Arc::new(BudgetPauseDispatchQueue::new());
        let bx = BudgetExceededBridge::new(bus, queue.clone(), None);
        let events = bx.observe_retry_pass(std::iter::empty::<(
            RetryBudgetBridgeRequest,
            RetryPolicyDecision,
        )>());
        assert!(events.is_empty());
        assert!(queue.is_empty());
        assert_eq!(bx.active_episode_count(), 0);
    }

    #[test]
    fn bridge_run_id_is_preserved_on_event_and_dispatch_request() {
        let bus = EventBus::default();
        let sink = RecordingSink::new();
        let queue = Arc::new(BudgetPauseDispatchQueue::new());
        let bx = BudgetExceededBridge::new(
            bus,
            queue.clone(),
            Some(sink.clone() as Arc<dyn BudgetExceededEventSink>),
        );

        bx.observe_retry_pass([(
            req("ENG-1", 99, 100, Some(42)),
            RetryPolicyDecision::BudgetExceeded {
                observed: 4.0,
                cap: 3.0,
            },
        )]);

        let persisted = sink.snapshot();
        if let OrchestratorEvent::BudgetExceeded { run_id, .. } = &persisted[0] {
            assert_eq!(*run_id, Some(42));
        } else {
            panic!("expected BudgetExceeded");
        }
        let drained = queue.drain();
        assert_eq!(drained[0].run_id, Some(RunRef::new(42)));
    }
}
