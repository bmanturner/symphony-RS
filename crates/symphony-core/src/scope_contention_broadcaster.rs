//! Shared sink-broadcaster scaffolding that turns per-runner
//! [`crate::scope_contention_log::ScopeContentionEventLog`] dedup output
//! into durable + fanned-out
//! [`OrchestratorEvent::ScopeCapReached`] events.
//!
//! # Why this lives in core
//!
//! Each dispatch runner already produces typed `ScopeContended*`
//! observations on its run report and the dedup primitive
//! ([`ScopeContentionEventLog`]) collapses repeated observations into
//! one event per contention episode. What was missing was a shared
//! plumbing surface that:
//!
//! 1. Holds the per-orchestrator dedup state behind a `Mutex` so any
//!    runner can call into it from a `&self` context.
//! 2. Broadcasts the resulting events on the [`EventBus`] so SSE/TUI
//!    subscribers see them in real time.
//! 3. Persists the same events through a kernel-side
//!    [`ScopeContentionEventSink`] so `EventRepository::append_event`
//!    in `symphony-state` (or any other backend) can turn the broadcast
//!    into durable history.
//!
//! `symphony-core` does not depend on `symphony-state`, so the sink is
//! a narrow trait the composition root supplies. A broadcaster
//! constructed without a sink is still useful — bus subscribers see
//! events, the dedup invariant still holds, but nothing is persisted.
//!
//! # Two helpers, two subject kinds
//!
//! Different runners key dispatches differently. Specialist /
//! integration / QA runners reserve a `runs.id` (`Run` subject) before
//! attempting to acquire concurrency permits. Follow-up approval and
//! budget pause runners do not reserve a run row and key only on a
//! tracker-facing identifier (`Identifier` subject). [`observe_run_pass`]
//! and [`observe_identifier_pass`] are the two helpers that lift each
//! runner's typed observation type onto a [`ContentionObservation`]
//! before handing it to the dedup log.
//!
//! [`observe_run_pass`]: ScopeContentionEventBroadcaster::observe_run_pass
//! [`observe_identifier_pass`]: ScopeContentionEventBroadcaster::observe_identifier_pass

use std::sync::{Arc, Mutex};

use crate::concurrency_gate::ScopeKind;
use crate::event_bus::EventBus;
use crate::events::OrchestratorEvent;
pub use crate::scope_contention_log::ContentionSubject;
use crate::scope_contention_log::{ContentionObservation, ScopeContentionEventLog};

/// Scope-side fields each runner observation contributes.
///
/// The runner-specific identity (run id / tracker identifier /
/// follow-up id / pause id) is supplied alongside this struct by the
/// caller — see [`ScopeContentionEventBroadcaster::observe_run_pass`]
/// and [`ScopeContentionEventBroadcaster::observe_identifier_pass`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ScopeFields {
    /// Which scope kind hit cap.
    pub scope_kind: ScopeKind,
    /// Key of the contended scope (or `""` for [`ScopeKind::Global`]).
    pub scope_key: String,
    /// In-flight permits on the contended scope at rejection.
    pub in_flight: u32,
    /// Configured cap on the contended scope.
    pub cap: u32,
}

/// Backend-side error surfaced from a [`ScopeContentionEventSink`].
///
/// Wraps a textual description because the kernel does not depend on
/// the concrete state crate's error type. The composition root is
/// expected to log/handle the failure; the broadcaster surfaces it
/// untouched and stops persisting that pass's events.
#[derive(Debug, thiserror::Error)]
#[error("scope contention sink backend error: {0}")]
pub struct ScopeContentionSinkError(pub String);

/// Kernel-side persistence trait for
/// [`OrchestratorEvent::ScopeCapReached`] events.
///
/// The composition root (today: `symphony-cli` / `symphony-state`)
/// provides an implementation that forwards into
/// `EventRepository::append_event` and returns the assigned sequence.
/// A broadcaster constructed with `None` for the sink simply skips
/// persistence — the event is still broadcast on the bus. With a sink
/// configured, the broadcaster honours the persist-before-broadcast
/// invariant: a sink error suppresses the bus emission so live
/// subscribers cannot drift ahead of the durable log (Phase 12 §139).
pub trait ScopeContentionEventSink: Send + Sync {
    /// Persist a [`OrchestratorEvent::ScopeCapReached`] event and
    /// return the sequence assigned by the underlying event repository.
    ///
    /// Implementations may panic-on-mismatch if the event is not a
    /// `ScopeCapReached`; the broadcaster only ever calls this with
    /// that variant.
    fn append_scope_cap_reached(
        &self,
        event: &OrchestratorEvent,
    ) -> Result<i64, ScopeContentionSinkError>;
}

/// Shared broadcaster owning the per-orchestrator dedup log, the event
/// bus handle, and an optional persistence sink.
///
/// Cheap to clone — internally an `Arc` over the inner state.
#[derive(Clone)]
pub struct ScopeContentionEventBroadcaster {
    inner: Arc<Inner>,
}

struct Inner {
    log: Mutex<ScopeContentionEventLog>,
    bus: EventBus,
    sink: Option<Arc<dyn ScopeContentionEventSink>>,
}

impl std::fmt::Debug for ScopeContentionEventBroadcaster {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ScopeContentionEventBroadcaster")
            .field("has_sink", &self.inner.sink.is_some())
            .finish()
    }
}

impl ScopeContentionEventBroadcaster {
    /// Build a broadcaster that broadcasts on `bus` and persists
    /// through `sink` when present.
    #[must_use]
    pub fn new(bus: EventBus, sink: Option<Arc<dyn ScopeContentionEventSink>>) -> Self {
        Self {
            inner: Arc::new(Inner {
                log: Mutex::new(ScopeContentionEventLog::new()),
                bus,
                sink,
            }),
        }
    }

    /// Process a run-keyed dispatch pass.
    ///
    /// Each item is `(run_id, scope_fields)` — the runner has already
    /// reserved a `runs.id` row and surfaces the contended scope
    /// details from its typed `ScopeContendedObservation`. The helper
    /// lifts each pair onto a [`ContentionObservation`], runs the
    /// dedup log, broadcasts every emitted event on the bus, and
    /// persists each event via the sink (when configured).
    ///
    /// Returns the events emitted this pass — useful for tests and for
    /// callers that want to attach further metadata before forgetting
    /// the events.
    pub fn observe_run_pass<I>(&self, observations: I) -> Vec<OrchestratorEvent>
    where
        I: IntoIterator<Item = (i64, ScopeFields)>,
    {
        let observations = observations
            .into_iter()
            .map(|(run_id, fields)| ContentionObservation {
                subject: ContentionSubject::Run(run_id),
                scope_kind: fields.scope_kind,
                scope_key: fields.scope_key,
                in_flight: fields.in_flight,
                cap: fields.cap,
            });
        self.observe_pass(observations)
    }

    /// Process an identifier-keyed dispatch pass.
    ///
    /// Counterpart to [`Self::observe_run_pass`] for runners (today
    /// follow-up approval, budget pause) that key dispatches on a
    /// tracker-facing identifier rather than a `runs.id`.
    pub fn observe_identifier_pass<I>(&self, observations: I) -> Vec<OrchestratorEvent>
    where
        I: IntoIterator<Item = (String, ScopeFields)>,
    {
        let observations =
            observations
                .into_iter()
                .map(|(identifier, fields)| ContentionObservation {
                    subject: ContentionSubject::Identifier(identifier),
                    scope_kind: fields.scope_kind,
                    scope_key: fields.scope_key,
                    in_flight: fields.in_flight,
                    cap: fields.cap,
                });
        self.observe_pass(observations)
    }

    /// Process a pass whose observations may carry either subject kind.
    ///
    /// The specialist runner reserves a `runs.id` only for some of its
    /// dispatches (e.g. once a request flows through `RunRepository`),
    /// so a single pass can mix `Run` and `Identifier` subjects. The
    /// dedup contract requires *one* call per pass with the complete
    /// observation set — calling [`Self::observe_run_pass`] and then
    /// [`Self::observe_identifier_pass`] back-to-back would prematurely
    /// end episodes from the first call. This helper feeds them through
    /// a single [`ScopeContentionEventLog::observe_pass`] invocation.
    pub fn observe_mixed_pass<I>(&self, observations: I) -> Vec<OrchestratorEvent>
    where
        I: IntoIterator<Item = (ContentionSubject, ScopeFields)>,
    {
        let observations =
            observations
                .into_iter()
                .map(|(subject, fields)| ContentionObservation {
                    subject,
                    scope_kind: fields.scope_kind,
                    scope_key: fields.scope_key,
                    in_flight: fields.in_flight,
                    cap: fields.cap,
                });
        self.observe_pass(observations)
    }

    fn observe_pass<I>(&self, observations: I) -> Vec<OrchestratorEvent>
    where
        I: IntoIterator<Item = ContentionObservation>,
    {
        let events = {
            let mut log = self
                .inner
                .log
                .lock()
                .expect("scope contention log mutex poisoned");
            log.observe_pass(observations)
        };

        for event in &events {
            // Persist-before-broadcast (Phase 12 §139): the durable
            // event log is the source of truth for SSE replay, audit,
            // and recovery. A subscriber must never observe an event
            // that does not already exist in the log, otherwise an SSE
            // consumer reconnecting and replaying from `after_sequence`
            // would silently miss frames it had already seen live. When
            // the sink errors we log loudly and *suppress* the
            // broadcast for that event; the dedup log still advances
            // because the contention episode was observed.
            if let Some(sink) = &self.inner.sink
                && let Err(err) = sink.append_scope_cap_reached(event)
            {
                tracing::warn!(
                    target: "symphony::scope_contention",
                    error = %err,
                    "failed to persist ScopeCapReached event; suppressing broadcast",
                );
                continue;
            }
            self.inner.bus.emit(event.clone());
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
            .expect("scope contention log mutex poisoned")
            .active_episode_count()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use futures::StreamExt;
    use std::sync::Mutex as StdMutex;

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

    impl ScopeContentionEventSink for RecordingSink {
        fn append_scope_cap_reached(
            &self,
            event: &OrchestratorEvent,
        ) -> Result<i64, ScopeContentionSinkError> {
            assert!(matches!(event, OrchestratorEvent::ScopeCapReached { .. }));
            self.events.lock().unwrap().push(event.clone());
            let mut seq = self.next_seq.lock().unwrap();
            *seq += 1;
            Ok(*seq)
        }
    }

    struct FailingSink;

    impl ScopeContentionEventSink for FailingSink {
        fn append_scope_cap_reached(
            &self,
            _event: &OrchestratorEvent,
        ) -> Result<i64, ScopeContentionSinkError> {
            Err(ScopeContentionSinkError("boom".into()))
        }
    }

    fn fields(kind: ScopeKind, key: &str, in_flight: u32, cap: u32) -> ScopeFields {
        ScopeFields {
            scope_kind: kind,
            scope_key: key.to_owned(),
            in_flight,
            cap,
        }
    }

    #[tokio::test]
    async fn run_pass_emits_event_with_run_id_and_no_identifier() {
        let bus = EventBus::default();
        let mut rx = bus.subscribe();
        let sink = RecordingSink::new();
        let bx = ScopeContentionEventBroadcaster::new(
            bus.clone(),
            Some(sink.clone() as Arc<dyn ScopeContentionEventSink>),
        );

        let events = bx.observe_run_pass([(42_i64, fields(ScopeKind::Role, "qa", 1, 1))]);
        assert_eq!(events.len(), 1);

        let bus_event = rx.next().await.expect("item").expect("not lagged");
        match bus_event {
            OrchestratorEvent::ScopeCapReached {
                run_id, identifier, ..
            } => {
                assert_eq!(run_id, Some(42));
                assert!(identifier.is_none());
            }
            other => panic!("unexpected event: {other:?}"),
        }

        let persisted = sink.snapshot();
        assert_eq!(persisted.len(), 1);
    }

    #[tokio::test]
    async fn identifier_pass_emits_event_with_identifier_and_no_run_id() {
        let bus = EventBus::default();
        let mut rx = bus.subscribe();
        let sink = RecordingSink::new();
        let bx = ScopeContentionEventBroadcaster::new(
            bus.clone(),
            Some(sink.clone() as Arc<dyn ScopeContentionEventSink>),
        );

        bx.observe_identifier_pass([("FU-7".to_owned(), fields(ScopeKind::Global, "", 1, 1))]);

        let bus_event = rx.next().await.expect("item").expect("not lagged");
        match bus_event {
            OrchestratorEvent::ScopeCapReached {
                run_id, identifier, ..
            } => {
                assert!(run_id.is_none());
                assert_eq!(identifier.as_deref(), Some("FU-7"));
            }
            other => panic!("unexpected event: {other:?}"),
        }
        assert_eq!(sink.snapshot().len(), 1);
    }

    #[test]
    fn dedup_holds_across_multi_pass_driving() {
        let bus = EventBus::default();
        let sink = RecordingSink::new();
        let bx = ScopeContentionEventBroadcaster::new(
            bus,
            Some(sink.clone() as Arc<dyn ScopeContentionEventSink>),
        );

        // Episode 1 — runs through five contended passes.
        for _ in 0..5 {
            bx.observe_run_pass([(1_i64, fields(ScopeKind::Role, "qa", 1, 1))]);
        }
        // Episode ends.
        bx.observe_run_pass(std::iter::empty::<(i64, ScopeFields)>());
        // Episode 2 — same scope, fresh contention.
        for _ in 0..3 {
            bx.observe_run_pass([(1_i64, fields(ScopeKind::Role, "qa", 1, 1))]);
        }

        let persisted = sink.snapshot();
        assert_eq!(
            persisted.len(),
            2,
            "exactly one append per contention episode"
        );
    }

    #[test]
    fn missing_sink_skips_persistence_but_bus_still_fires() {
        let bus = EventBus::default();
        let bx = ScopeContentionEventBroadcaster::new(bus.clone(), None);
        let events = bx.observe_run_pass([(7_i64, fields(ScopeKind::Repository, "x/y", 1, 1))]);
        assert_eq!(events.len(), 1);
        // No assertions on persistence — there is no sink. Just prove
        // that emitting without a sink does not panic and reports the
        // event back to the caller for downstream use.
    }

    #[test]
    fn sink_failure_is_logged_but_does_not_panic_or_break_dedup() {
        let bus = EventBus::default();
        let bx = ScopeContentionEventBroadcaster::new(
            bus,
            Some(Arc::new(FailingSink) as Arc<dyn ScopeContentionEventSink>),
        );

        // Emit twice on the same episode. The dedup invariant must
        // still hold even though the sink failed on the first append:
        // the broadcaster does not roll back the dedup log on sink
        // failure. With persist-before-broadcast, the bus does *not*
        // see the failed event (see `sink_failure_suppresses_broadcast`
        // below); this test only asserts dedup advancement.
        let first = bx.observe_run_pass([(1_i64, fields(ScopeKind::Role, "qa", 1, 1))]);
        let second = bx.observe_run_pass([(1_i64, fields(ScopeKind::Role, "qa", 1, 1))]);
        assert_eq!(first.len(), 1);
        assert!(
            second.is_empty(),
            "second pass on same episode is still deduped"
        );
    }

    /// Sink that records, for each append call, the set of events
    /// already visible to a pre-attached bus subscriber. Persist-before-
    /// broadcast requires that the subscriber sees *nothing* by the
    /// time the sink runs; broadcast-before-persist would leak the
    /// event into the receiver before the append.
    struct OrderingSink {
        rx: StdMutex<tokio::sync::broadcast::Receiver<OrchestratorEvent>>,
        seen_before_persist: StdMutex<Vec<usize>>,
    }

    impl OrderingSink {
        fn arc(bus: &EventBus) -> Arc<Self> {
            Arc::new(Self {
                rx: StdMutex::new(bus.raw_subscribe()),
                seen_before_persist: StdMutex::new(Vec::new()),
            })
        }
    }

    impl ScopeContentionEventSink for OrderingSink {
        fn append_scope_cap_reached(
            &self,
            _event: &OrchestratorEvent,
        ) -> Result<i64, ScopeContentionSinkError> {
            let mut rx = self.rx.lock().unwrap();
            let mut count = 0;
            while rx.try_recv().is_ok() {
                count += 1;
            }
            self.seen_before_persist.lock().unwrap().push(count);
            Ok(1)
        }
    }

    #[test]
    fn persist_runs_before_broadcast() {
        let bus = EventBus::default();
        let sink = OrderingSink::arc(&bus);
        let bx = ScopeContentionEventBroadcaster::new(
            bus,
            Some(sink.clone() as Arc<dyn ScopeContentionEventSink>),
        );

        bx.observe_run_pass([(1_i64, fields(ScopeKind::Role, "qa", 1, 1))]);

        let observed = sink.seen_before_persist.lock().unwrap().clone();
        assert_eq!(
            observed,
            vec![0],
            "persist must observe an empty bus — broadcast comes after"
        );
    }

    #[tokio::test]
    async fn sink_failure_suppresses_broadcast() {
        let bus = EventBus::default();
        let mut rx = bus.subscribe();
        let bx = ScopeContentionEventBroadcaster::new(
            bus.clone(),
            Some(Arc::new(FailingSink) as Arc<dyn ScopeContentionEventSink>),
        );

        bx.observe_run_pass([(1_i64, fields(ScopeKind::Role, "qa", 1, 1))]);

        // The bus must remain empty when persistence failed.
        let next = tokio::time::timeout(std::time::Duration::from_millis(50), rx.next()).await;
        assert!(
            next.is_err(),
            "broadcast must be suppressed when persist fails"
        );
        assert_eq!(bus.subscriber_count(), 1);
    }

    #[test]
    fn run_subject_and_identifier_subject_with_same_scope_are_distinct() {
        let bus = EventBus::default();
        let sink = RecordingSink::new();
        let bx = ScopeContentionEventBroadcaster::new(
            bus,
            Some(sink.clone() as Arc<dyn ScopeContentionEventSink>),
        );
        bx.observe_run_pass([(1_i64, fields(ScopeKind::Role, "qa", 1, 1))]);
        bx.observe_identifier_pass([("ENG-1".to_owned(), fields(ScopeKind::Role, "qa", 1, 1))]);
        let persisted = sink.snapshot();
        assert_eq!(persisted.len(), 2);
        let mut saw_run_subject = false;
        let mut saw_identifier_subject = false;
        for ev in persisted {
            if let OrchestratorEvent::ScopeCapReached {
                run_id, identifier, ..
            } = ev
            {
                if run_id == Some(1) && identifier.is_none() {
                    saw_run_subject = true;
                }
                if run_id.is_none() && identifier.as_deref() == Some("ENG-1") {
                    saw_identifier_subject = true;
                }
            }
        }
        assert!(
            saw_run_subject && saw_identifier_subject,
            "both subject kinds emit independently",
        );
    }
}
