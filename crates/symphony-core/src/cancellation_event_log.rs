//! Per-run dedup of cancellation observations into durable
//! [`OrchestratorEvent::RunCancelled`] events (SPEC v2 §4.5 / Phase 11.5).
//!
//! # Why this lives in core
//!
//! Cancellation in Symphony-RS is *cooperative*: each dispatch runner
//! consults the [`crate::cancellation::CancellationQueue`] before
//! acquiring a lease and again between agent steps. A long-running
//! dispatch may observe the same pending cancel multiple times before
//! its terminal transition lands — once at the pre-lease check, again
//! at the dispatcher cooperation point, and possibly once more if a
//! retry observes the not-yet-drained entry. A propagator that fans a
//! work-item cancel to ten in-flight children will then enqueue ten
//! per-run requests, each of which a runner observes independently.
//!
//! [`OrchestratorEvent::RunCancelled`] is the *durable* projection of
//! that lifecycle: persisted to the event log, fanned out over SSE,
//! and consumed by status surfaces. To keep the durable log honest we
//! emit at most one event per `runs.id`. A run cannot cancel twice —
//! once a run has been observed in `Cancelled` (or in
//! `CancelRequested` heading there), any future observation of a
//! pending cancel for that run is a no-op for the wire stream.
//!
//! # Contract
//!
//! [`CancellationEventLog::record`] is the sole entry point. Callers
//! pass the durable run id, an optional tracker identifier snapshot,
//! and the originating [`crate::cancellation::CancelRequest`] (the
//! request whose subject is either the run itself or a parent work
//! item that cascaded). The log returns `Some(event)` on the first
//! observation for that run id, and `None` thereafter — even if the
//! cancel reason or `requested_by` differs, because a run's cancel
//! provenance is fixed at the moment the log first records it.
//!
//! Persistence of the returned event is the composition root's
//! responsibility (see `EventRepository::append_event` in
//! `symphony-state`). The dedup state itself is in-memory: on restart
//! the composition root rebuilds it by scanning durable events for
//! `run_cancelled` entries (or, equivalently, by treating any run
//! whose `runs.status` is already `cancelled` as "already emitted").
//! That is symmetrical with how
//! [`crate::scope_contention_log::ScopeContentionEventLog`] is
//! managed — the log is a *gate*, not a record of truth.

use std::collections::HashSet;

use crate::blocker::RunRef;
use crate::cancellation::CancelRequest;
use crate::events::OrchestratorEvent;

/// Dedup state mapping per-run cancel observations to durable events.
///
/// Construct one per orchestrator (cheap; just a `HashSet`). Drive
/// with [`CancellationEventLog::record`] each time a runner observes a
/// pending cancel for a run, after it has committed the run's
/// transition toward [`crate::run_status::RunStatus::Cancelled`].
#[derive(Debug, Default, Clone)]
pub struct CancellationEventLog {
    seen_runs: HashSet<RunRef>,
}

impl CancellationEventLog {
    /// Build an empty log — no run has been recorded.
    pub fn new() -> Self {
        Self::default()
    }

    /// Number of distinct runs the log has emitted an event for.
    /// Diagnostic accessor for tests / observability.
    pub fn emitted_run_count(&self) -> usize {
        self.seen_runs.len()
    }

    /// True iff a run id has already produced a durable event.
    pub fn has_emitted(&self, run_id: RunRef) -> bool {
        self.seen_runs.contains(&run_id)
    }

    /// Seed the log with run ids whose `RunCancelled` event was
    /// already persisted in a previous process. The composition root
    /// calls this at startup with the run ids extracted from the
    /// durable event log; subsequent observations for those runs
    /// emit no further events.
    ///
    /// Idempotent: re-seeding the same id is a no-op.
    pub fn seed_emitted(&mut self, run_ids: impl IntoIterator<Item = RunRef>) {
        self.seen_runs.extend(run_ids);
    }

    /// Record an observation for `run_id` and return the durable event
    /// to emit, if any.
    ///
    /// Returns `Some(OrchestratorEvent::RunCancelled { .. })` the first
    /// time this run is recorded; returns `None` on every subsequent
    /// call for the same run, regardless of whether the originating
    /// `request` differs.
    pub fn record(
        &mut self,
        run_id: RunRef,
        identifier: Option<String>,
        request: &CancelRequest,
    ) -> Option<OrchestratorEvent> {
        if !self.seen_runs.insert(run_id) {
            return None;
        }
        Some(OrchestratorEvent::RunCancelled {
            run_id: run_id.0,
            identifier,
            subject: request.subject,
            reason: request.reason.clone(),
            requested_by: request.requested_by.clone(),
            requested_at: request.requested_at.clone(),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::cancellation::CancelSubject;
    use crate::work_item::WorkItemId;

    fn run_request(run_id: i64, reason: &str) -> CancelRequest {
        CancelRequest::for_run(
            RunRef::new(run_id),
            reason,
            "tester",
            "2026-05-09T00:00:00Z",
        )
    }

    fn work_item_request(work_item_id: i64, reason: &str) -> CancelRequest {
        CancelRequest::for_work_item(
            WorkItemId::new(work_item_id),
            reason,
            "cascade:work_item:11",
            "2026-05-09T00:00:00Z",
        )
    }

    #[test]
    fn first_observation_emits_run_cancelled_event() {
        let mut log = CancellationEventLog::new();
        let req = run_request(1, "operator cancel");
        let ev = log.record(RunRef::new(1), Some("ENG-1".into()), &req);
        let ev = ev.expect("first observation must emit");
        match ev {
            OrchestratorEvent::RunCancelled {
                run_id,
                identifier,
                subject,
                reason,
                requested_by,
                requested_at,
            } => {
                assert_eq!(run_id, 1);
                assert_eq!(identifier.as_deref(), Some("ENG-1"));
                assert_eq!(subject, CancelSubject::run(RunRef::new(1)));
                assert_eq!(reason, "operator cancel");
                assert_eq!(requested_by, "tester");
                assert_eq!(requested_at, "2026-05-09T00:00:00Z");
            }
            other => panic!("unexpected event: {other:?}"),
        }
        assert_eq!(log.emitted_run_count(), 1);
        assert!(log.has_emitted(RunRef::new(1)));
    }

    #[test]
    fn repeated_observation_for_same_run_emits_no_further_event() {
        let mut log = CancellationEventLog::new();
        let req = run_request(7, "first");
        assert!(log.record(RunRef::new(7), None, &req).is_some());
        // A second observation — perhaps the dispatcher cooperation
        // check fired after the pre-lease gate — must not re-emit.
        assert!(log.record(RunRef::new(7), None, &req).is_none());
        // Even with a different request payload (reason, requested_by)
        // the dedup is keyed on run id alone; the run's cancel
        // provenance is fixed at first observation.
        let other = run_request(7, "second");
        assert!(log.record(RunRef::new(7), None, &other).is_none());
        assert_eq!(log.emitted_run_count(), 1);
    }

    #[test]
    fn distinct_runs_each_emit_independently() {
        let mut log = CancellationEventLog::new();
        let r1 = run_request(1, "a");
        let r2 = run_request(2, "b");
        assert!(log.record(RunRef::new(1), None, &r1).is_some());
        assert!(log.record(RunRef::new(2), None, &r2).is_some());
        assert_eq!(log.emitted_run_count(), 2);
    }

    #[test]
    fn cascade_request_carries_work_item_subject_through_event() {
        let mut log = CancellationEventLog::new();
        let parent_req = work_item_request(11, "parent cancelled");
        let ev = log.record(RunRef::new(99), Some("ENG-99".into()), &parent_req);
        let ev = ev.expect("first observation must emit");
        match ev {
            OrchestratorEvent::RunCancelled {
                run_id,
                subject,
                requested_by,
                ..
            } => {
                assert_eq!(run_id, 99);
                assert_eq!(subject, CancelSubject::work_item(WorkItemId::new(11)));
                assert_eq!(requested_by, "cascade:work_item:11");
            }
            other => panic!("unexpected event: {other:?}"),
        }
    }

    #[test]
    fn seed_emitted_suppresses_subsequent_first_observations() {
        // After a restart, the composition root extracts run ids from
        // the durable event log and seeds the in-memory log. Those
        // runs must be considered already-emitted so a runner that
        // re-observes a not-yet-drained pending cancel does not
        // duplicate the event on the wire.
        let mut log = CancellationEventLog::new();
        log.seed_emitted([RunRef::new(1), RunRef::new(2)]);
        assert!(log.has_emitted(RunRef::new(1)));
        assert!(log.has_emitted(RunRef::new(2)));
        let req = run_request(1, "post-restart observation");
        assert!(log.record(RunRef::new(1), None, &req).is_none());
        // An unseeded run still emits.
        let unseen = run_request(3, "fresh");
        assert!(log.record(RunRef::new(3), None, &unseen).is_some());
    }

    #[test]
    fn seed_emitted_is_idempotent() {
        let mut log = CancellationEventLog::new();
        log.seed_emitted([RunRef::new(1)]);
        log.seed_emitted([RunRef::new(1)]);
        assert_eq!(log.emitted_run_count(), 1);
    }

    #[test]
    fn has_emitted_is_false_for_unrecorded_runs() {
        let log = CancellationEventLog::new();
        assert!(!log.has_emitted(RunRef::new(404)));
    }
}
