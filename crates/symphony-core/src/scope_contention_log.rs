//! Per-pass dedup of [`crate::concurrency_gate::ScopeContended`]
//! observations into durable
//! [`OrchestratorEvent::ScopeCapReached`] events.
//!
//! # Why this lives in core
//!
//! Each dispatch runner already surfaces a typed `ScopeContendedObservation`
//! on its run report when
//! [`crate::concurrency_gate::ConcurrencyGate::try_acquire`] rejects. Those
//! observations are fine for in-process logging but are emitted *every
//! pass* a request stays parked on a contended scope. A request that
//! sits behind a saturated cap for ten consecutive scheduler ticks would
//! produce ten observations even though the operator-visible truth — "this
//! dispatch is currently blocked on this scope" — has not changed.
//!
//! [`OrchestratorEvent::ScopeCapReached`] is the *durable* projection of
//! that truth: persisted to the event log, fanned out over SSE, and
//! consumed by status surfaces. To keep the durable log honest we emit
//! at most one event per *contention episode* — defined as the unbroken
//! span over which a given dispatch keeps re-deferring against the same
//! `(scope_kind, scope_key)`. The [`ScopeContentionEventLog`] in this
//! module is the per-orchestrator state that performs that dedup.
//!
//! ## Episode boundaries
//!
//! The log is fed via [`ScopeContentionEventLog::observe_pass`] once per
//! scheduler/runner pass with the *complete* set of contention
//! observations from that pass. Observations carry a
//! [`ContentionSubject`] (run id or tracker identifier) and a
//! `(scope_kind, scope_key)` pair; together those form one episode key.
//!
//! * If a key is **new** this pass (was not active last pass), an event
//!   is emitted.
//! * If a key was **already active** last pass and is still present, no
//!   event is emitted — the episode is ongoing.
//! * If a key was active last pass and is **absent** this pass, the
//!   episode ends silently. A future pass that re-introduces the key
//!   starts a fresh episode and emits again.
//!
//! That is the contract every consumer should be able to rely on:
//! "events fire on the leading edge of a contention episode, never
//! during, never on the trailing edge."

use std::collections::HashSet;

use crate::concurrency_gate::ScopeKind;
use crate::events::OrchestratorEvent;

/// Identity carried alongside a contention observation.
///
/// Different runners key dispatches differently — specialist /
/// integration / QA runners reserve a `runs.id` (`Run`), while the
/// follow-up approval and budget pause runners key on internal ids and
/// surface only a tracker-facing identifier (`Identifier`). Either is
/// a valid episode subject; both can flow into the same log.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum ContentionSubject {
    /// Durable run row identifier (matches `runs.id`).
    Run(i64),
    /// Tracker-facing identifier when no run row is reserved.
    Identifier(String),
}

/// A single contention observation feeding the log.
///
/// One per parked dispatch per pass. The runner builds these from its
/// existing typed `ScopeContendedObservation` plus whatever subject id
/// the runner is keyed by.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ContentionObservation {
    /// Who is contended.
    pub subject: ContentionSubject,
    /// Which scope kind is at cap.
    pub scope_kind: ScopeKind,
    /// Key of the contended scope (or `""` for global).
    pub scope_key: String,
    /// In-flight permits on the contended scope at rejection.
    pub in_flight: u32,
    /// Configured cap on the contended scope.
    pub cap: u32,
}

/// Composite key identifying one contention episode.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
struct EpisodeKey {
    subject: ContentionSubject,
    scope_kind: ScopeKind,
    scope_key: String,
}

/// Dedup state mapping per-pass observations to durable events.
///
/// Construct one per orchestrator (cheap; just a `HashSet`). Drive with
/// [`ScopeContentionEventLog::observe_pass`] every pass that produces
/// contention observations. Returned events are ready to broadcast on
/// the [`crate::event_bus::EventBus`] and to persist via the state
/// crate's `EventRepository::append_event`.
#[derive(Debug, Default, Clone)]
pub struct ScopeContentionEventLog {
    active: HashSet<EpisodeKey>,
}

impl ScopeContentionEventLog {
    /// Build an empty log — no active episodes.
    pub fn new() -> Self {
        Self::default()
    }

    /// Process every contention observation from one scheduler/runner
    /// pass and return the events to emit.
    ///
    /// Callers must pass the *complete* set of contention observations
    /// for this pass. Keys present in the previous pass but absent now
    /// are treated as episode-ended; their next re-occurrence emits a
    /// new event.
    ///
    /// Duplicates within the same pass are coalesced — a runner that
    /// somehow emitted two observations for the same `(subject,
    /// scope_kind, scope_key)` triple in one pass still produces one
    /// event.
    pub fn observe_pass<I>(&mut self, observations: I) -> Vec<OrchestratorEvent>
    where
        I: IntoIterator<Item = ContentionObservation>,
    {
        let mut current: HashSet<EpisodeKey> = HashSet::new();
        let mut events = Vec::new();

        for obs in observations {
            let key = EpisodeKey {
                subject: obs.subject.clone(),
                scope_kind: obs.scope_kind,
                scope_key: obs.scope_key.clone(),
            };
            if !current.insert(key.clone()) {
                // A duplicate observation within the same pass.
                continue;
            }
            if !self.active.contains(&key) {
                events.push(observation_event(obs));
            }
        }

        self.active = current;
        events
    }

    /// Number of episodes currently active (i.e. observed on the most
    /// recent pass). Diagnostic accessor for tests / status surfaces.
    pub fn active_episode_count(&self) -> usize {
        self.active.len()
    }
}

fn observation_event(obs: ContentionObservation) -> OrchestratorEvent {
    let (run_id, identifier) = match obs.subject {
        ContentionSubject::Run(id) => (Some(id), None),
        ContentionSubject::Identifier(s) => (None, Some(s)),
    };
    OrchestratorEvent::ScopeCapReached {
        run_id,
        identifier,
        scope_kind: obs.scope_kind,
        scope_key: obs.scope_key,
        in_flight: obs.in_flight,
        cap: obs.cap,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn obs_run(
        run_id: i64,
        kind: ScopeKind,
        key: &str,
        in_flight: u32,
        cap: u32,
    ) -> ContentionObservation {
        ContentionObservation {
            subject: ContentionSubject::Run(run_id),
            scope_kind: kind,
            scope_key: key.to_owned(),
            in_flight,
            cap,
        }
    }

    fn obs_id(
        identifier: &str,
        kind: ScopeKind,
        key: &str,
        in_flight: u32,
        cap: u32,
    ) -> ContentionObservation {
        ContentionObservation {
            subject: ContentionSubject::Identifier(identifier.to_owned()),
            scope_kind: kind,
            scope_key: key.to_owned(),
            in_flight,
            cap,
        }
    }

    #[test]
    fn first_observation_emits_a_scope_cap_reached_event() {
        let mut log = ScopeContentionEventLog::new();
        let events = log.observe_pass([obs_run(1, ScopeKind::Role, "qa", 2, 2)]);
        assert_eq!(events.len(), 1);
        match &events[0] {
            OrchestratorEvent::ScopeCapReached {
                run_id,
                identifier,
                scope_kind,
                scope_key,
                in_flight,
                cap,
            } => {
                assert_eq!(*run_id, Some(1));
                assert!(identifier.is_none());
                assert_eq!(*scope_kind, ScopeKind::Role);
                assert_eq!(scope_key, "qa");
                assert_eq!(*in_flight, 2);
                assert_eq!(*cap, 2);
            }
            other => panic!("unexpected event: {other:?}"),
        }
        assert_eq!(log.active_episode_count(), 1);
    }

    #[test]
    fn identifier_subject_emits_event_with_identifier_and_no_run_id() {
        let mut log = ScopeContentionEventLog::new();
        let events = log.observe_pass([obs_id("FU-7", ScopeKind::Global, "", 1, 1)]);
        assert_eq!(events.len(), 1);
        if let OrchestratorEvent::ScopeCapReached {
            run_id, identifier, ..
        } = &events[0]
        {
            assert!(run_id.is_none());
            assert_eq!(identifier.as_deref(), Some("FU-7"));
        } else {
            panic!("expected ScopeCapReached");
        }
    }

    #[test]
    fn repeated_observations_in_same_episode_emit_only_once() {
        // Simulates a poll loop: the same dispatch keeps contending on
        // the same scope across N consecutive ticks. The durable event
        // stream must record exactly one ScopeCapReached.
        let mut log = ScopeContentionEventLog::new();
        let observation = || vec![obs_run(1, ScopeKind::Role, "qa", 1, 1)];

        let mut total_events = 0;
        for _ in 0..10 {
            total_events += log.observe_pass(observation()).len();
        }
        assert_eq!(total_events, 1, "exactly one event per contention episode");
        assert_eq!(log.active_episode_count(), 1);
    }

    #[test]
    fn episode_ends_when_observation_absent_then_re_emits_on_resume() {
        let mut log = ScopeContentionEventLog::new();

        // Tick 1: contended -> emit.
        let e1 = log.observe_pass([obs_run(1, ScopeKind::Role, "qa", 1, 1)]);
        assert_eq!(e1.len(), 1);

        // Tick 2: still contended -> no event.
        let e2 = log.observe_pass([obs_run(1, ScopeKind::Role, "qa", 1, 1)]);
        assert!(e2.is_empty());

        // Tick 3: observation absent (request dispatched) -> episode ends.
        let e3 = log.observe_pass(std::iter::empty());
        assert!(e3.is_empty());
        assert_eq!(log.active_episode_count(), 0);

        // Tick 4: contention resumes -> fresh episode, fresh event.
        let e4 = log.observe_pass([obs_run(1, ScopeKind::Role, "qa", 1, 1)]);
        assert_eq!(e4.len(), 1, "resumed contention is a new episode");
    }

    #[test]
    fn distinct_scopes_in_same_pass_each_emit_once() {
        let mut log = ScopeContentionEventLog::new();
        let events = log.observe_pass([
            obs_run(1, ScopeKind::Role, "qa", 1, 1),
            obs_run(1, ScopeKind::AgentProfile, "claude", 2, 2),
            obs_run(1, ScopeKind::Repository, "foglet/x", 1, 1),
        ]);
        assert_eq!(events.len(), 3);
        let kinds: Vec<ScopeKind> = events
            .iter()
            .filter_map(|e| match e {
                OrchestratorEvent::ScopeCapReached { scope_kind, .. } => Some(*scope_kind),
                _ => None,
            })
            .collect();
        assert!(kinds.contains(&ScopeKind::Role));
        assert!(kinds.contains(&ScopeKind::AgentProfile));
        assert!(kinds.contains(&ScopeKind::Repository));
    }

    #[test]
    fn distinct_subjects_on_same_scope_are_independent_episodes() {
        let mut log = ScopeContentionEventLog::new();
        let pass1 = log.observe_pass([
            obs_run(1, ScopeKind::Role, "qa", 1, 1),
            obs_run(2, ScopeKind::Role, "qa", 1, 1),
        ]);
        assert_eq!(pass1.len(), 2);

        // Run 1 dispatches; run 2 keeps contending. Only run 1's
        // episode ends. Run 3 newly contends and gets its own event.
        let pass2 = log.observe_pass([
            obs_run(2, ScopeKind::Role, "qa", 1, 1),
            obs_run(3, ScopeKind::Role, "qa", 1, 1),
        ]);
        assert_eq!(pass2.len(), 1, "only the new subject emits");
        if let OrchestratorEvent::ScopeCapReached { run_id, .. } = &pass2[0] {
            assert_eq!(*run_id, Some(3));
        } else {
            panic!("expected ScopeCapReached");
        }
    }

    #[test]
    fn duplicate_observations_within_one_pass_coalesce() {
        let mut log = ScopeContentionEventLog::new();
        let events = log.observe_pass([
            obs_run(1, ScopeKind::Role, "qa", 1, 1),
            obs_run(1, ScopeKind::Role, "qa", 1, 1),
        ]);
        assert_eq!(events.len(), 1);
        assert_eq!(log.active_episode_count(), 1);
    }

    #[test]
    fn run_subject_and_identifier_subject_with_same_scope_are_distinct() {
        let mut log = ScopeContentionEventLog::new();
        let events = log.observe_pass([
            obs_run(1, ScopeKind::Role, "qa", 1, 1),
            obs_id("ENG-1", ScopeKind::Role, "qa", 1, 1),
        ]);
        assert_eq!(events.len(), 2);
        assert_eq!(log.active_episode_count(), 2);
    }

    #[test]
    fn poll_loop_simulation_emits_exactly_once_per_episode() {
        // Full integration-style scenario: a scheduler drives 25 ticks.
        // Contention starts at tick 5, persists through tick 14, ends
        // at tick 15, resumes at tick 20, persists through tick 24.
        // The durable event stream must contain exactly two
        // ScopeCapReached events, in tick-5 then tick-20 order.
        let mut log = ScopeContentionEventLog::new();
        let mut emitted: Vec<(usize, OrchestratorEvent)> = Vec::new();
        for tick in 0..25 {
            let observations: Vec<ContentionObservation> =
                if (5..15).contains(&tick) || (20..25).contains(&tick) {
                    vec![obs_run(1, ScopeKind::Role, "qa", 1, 1)]
                } else {
                    Vec::new()
                };
            for ev in log.observe_pass(observations) {
                emitted.push((tick, ev));
            }
        }
        assert_eq!(emitted.len(), 2, "exactly two episodes => two events");
        assert_eq!(emitted[0].0, 5);
        assert_eq!(emitted[1].0, 20);
        for (_, ev) in &emitted {
            assert!(matches!(ev, OrchestratorEvent::ScopeCapReached { .. }));
        }
    }

    #[test]
    fn empty_pass_with_no_active_episodes_is_idle() {
        let mut log = ScopeContentionEventLog::new();
        let events = log.observe_pass(std::iter::empty());
        assert!(events.is_empty());
        assert_eq!(log.active_episode_count(), 0);
    }
}
