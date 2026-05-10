//! Cooperative cancellation primitives (SPEC v2 §4.5 / Phase 11.5).
//!
//! Cancellation in Symphony-RS is *cooperative*: an operator (or a parent
//! cancel cascading to a child) enqueues a [`CancelRequest`] keyed on a
//! [`CancelSubject`], and each dispatch runner observes pending requests
//! at safe points (before lease acquisition, between agent steps). When a
//! runner observes a pending cancel, it releases its lease + scope
//! permits + workspace claim and transitions the run to
//! [`crate::RunStatus::Cancelled`] via the typed status gate.
//!
//! This module defines two pieces:
//!
//! * [`CancelRequest`] — the durable shape of a cancel intent. Carries the
//!   subject (run or work item), an operator-supplied reason, the
//!   identity that requested the cancel, and an opaque RFC3339 timestamp
//!   string. The kernel stays free of `chrono`/`time` — production
//!   callers supply the timestamp from their own clock (mirroring the
//!   convention in [`crate::lease::LeaseClock`]).
//! * [`CancellationQueue`] — the in-memory dispatch surface. Holds
//!   pending requests keyed by [`CancelSubject`], exposes lookup by run
//!   id and work-item id for runner observation, and a `drain_for_run`
//!   method that consumes a run-keyed entry without disturbing
//!   work-item-keyed entries.
//!
//! Persistence lives in `symphony-state::cancel_requests`: the
//! repository mirrors every `enqueue` / `drain_*` call into the
//! `cancel_requests` table, and [`CancellationQueue::from_pending`]
//! rebuilds this in-memory primitive at startup from
//! `CancelRequestRepository::list_pending_cancel_requests`. Together
//! they guarantee that an operator-issued cancel survives a scheduler
//! crash and is still observable to runners after recovery.
//!
//! Note: work-item cancellation cascading to in-flight child runs is the
//! responsibility of a separate `CancellationPropagator` (also a later
//! subtask). The queue itself does not resolve `WorkItem` subjects to
//! their runs — callers are expected to look up the runs and enqueue
//! per-run requests in addition to the parent work-item request, so a
//! later runner that picks up a child run still observes a pending
//! cancel even if the propagator has not yet fanned out.

use std::collections::HashMap;

use serde::{Deserialize, Serialize};

use crate::blocker::RunRef;
use crate::work_item::WorkItemId;

/// Subject of a [`CancelRequest`].
///
/// A cancel request targets either a single run row (operator cancels a
/// specific dispatch) or a work item (operator cancels the whole issue,
/// cascading to all in-flight runs). The two variants live in distinct
/// keyspaces inside [`CancellationQueue`] so a run-targeted request and a
/// work-item-targeted request that happen to share a numeric id never
/// collide.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum CancelSubject {
    /// Cancel a single run by its `runs.id`.
    Run { run_id: RunRef },
    /// Cancel a work item by its `work_items.id`. The kernel-side
    /// propagator is responsible for fanning this out to the in-flight
    /// child runs.
    WorkItem { work_item_id: WorkItemId },
}

impl CancelSubject {
    /// Construct a [`CancelSubject::Run`] from a [`RunRef`].
    pub fn run(run_id: RunRef) -> Self {
        Self::Run { run_id }
    }

    /// Construct a [`CancelSubject::WorkItem`] from a [`WorkItemId`].
    pub fn work_item(work_item_id: WorkItemId) -> Self {
        Self::WorkItem { work_item_id }
    }
}

/// Operator-visible cancel request.
///
/// Identity (`requested_by`) and timestamp (`requested_at`) are stored as
/// strings: the kernel does not own a clock, and the operator-identity
/// surface is not strongly typed yet (the CLI will pass `$USER` or a
/// configured override). Both fields round-trip through serde so
/// persistence is a transparent forward.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CancelRequest {
    /// Run or work item being cancelled.
    pub subject: CancelSubject,
    /// Operator-supplied reason. The CLI requires `--reason` so this
    /// should never be empty in practice; the kernel does not enforce
    /// non-emptiness because integration tests sometimes use a sentinel.
    pub reason: String,
    /// Identity that requested the cancel (e.g. `$USER`, a CLI override,
    /// or `"cascade:work_item:42"` for a propagated cancel).
    pub requested_by: String,
    /// RFC3339-style timestamp supplied by the caller's clock. The
    /// kernel treats this as opaque text; downstream consumers may parse
    /// it for display purposes.
    pub requested_at: String,
}

impl CancelRequest {
    /// Construct a [`CancelRequest`] for a specific run.
    pub fn for_run(
        run_id: RunRef,
        reason: impl Into<String>,
        requested_by: impl Into<String>,
        requested_at: impl Into<String>,
    ) -> Self {
        Self {
            subject: CancelSubject::run(run_id),
            reason: reason.into(),
            requested_by: requested_by.into(),
            requested_at: requested_at.into(),
        }
    }

    /// Construct a [`CancelRequest`] for a work item.
    pub fn for_work_item(
        work_item_id: WorkItemId,
        reason: impl Into<String>,
        requested_by: impl Into<String>,
        requested_at: impl Into<String>,
    ) -> Self {
        Self {
            subject: CancelSubject::work_item(work_item_id),
            reason: reason.into(),
            requested_by: requested_by.into(),
            requested_at: requested_at.into(),
        }
    }
}

/// In-memory queue of pending [`CancelRequest`]s.
///
/// Runners hold a shared reference to this queue (typically wrapped in
/// `Arc<Mutex<_>>` at the composition root) and consult it before
/// acquiring a lease and between agent steps. A pending entry survives
/// repeated lookup; only [`Self::drain_for_run`] consumes an entry.
///
/// Idempotency: re-enqueuing a request for a subject that already has a
/// pending entry replaces the stored reason / requested_by / timestamp
/// in-place rather than producing a duplicate. This matches the durable
/// uniqueness constraint planned for the `cancel_requests` table
/// (`UNIQUE(subject_kind, subject_id) WHERE state='pending'`) so the
/// in-memory and durable layers never disagree about how many pending
/// requests exist for a subject.
#[derive(Debug, Default)]
pub struct CancellationQueue {
    by_run: HashMap<RunRef, CancelRequest>,
    by_work_item: HashMap<WorkItemId, CancelRequest>,
}

impl CancellationQueue {
    /// Construct an empty queue.
    pub fn new() -> Self {
        Self::default()
    }

    /// Reconstruct a queue from a set of already-pending cancel
    /// requests — typically rows the persistence layer hydrated at
    /// startup. Last-write-wins on duplicate subjects, matching
    /// [`Self::enqueue`]'s replace semantics; the durable layer's
    /// partial unique index makes that case unreachable in practice,
    /// but the constructor stays defensive so a tampered DB cannot
    /// produce two pending entries for the same subject in memory.
    pub fn from_pending(requests: impl IntoIterator<Item = CancelRequest>) -> Self {
        let mut queue = Self::new();
        for request in requests {
            queue.enqueue(request);
        }
        queue
    }

    /// Insert (or replace) a pending request keyed on its subject.
    ///
    /// Returns `true` if this enqueue introduced a new pending entry,
    /// `false` if it replaced an existing one. The boolean lets callers
    /// distinguish "first cancel observed" (durable event should fire,
    /// propagator should fan out) from "operator nudged an already-
    /// pending cancel" (no-op for downstream effects).
    pub fn enqueue(&mut self, request: CancelRequest) -> bool {
        match request.subject {
            CancelSubject::Run { run_id } => self.by_run.insert(run_id, request).is_none(),
            CancelSubject::WorkItem { work_item_id } => {
                self.by_work_item.insert(work_item_id, request).is_none()
            }
        }
    }

    /// Look up a pending request for a specific run, without consuming.
    /// Used by runners observing cancel intent at safe points.
    pub fn pending_for_run(&self, run_id: RunRef) -> Option<&CancelRequest> {
        self.by_run.get(&run_id)
    }

    /// Look up a pending request for a specific work item, without
    /// consuming. Used by the propagator and by runners that want to
    /// short-circuit a dispatch whose parent has been cancelled even if
    /// no per-run request has been enqueued yet.
    pub fn pending_for_work_item(&self, work_item_id: WorkItemId) -> Option<&CancelRequest> {
        self.by_work_item.get(&work_item_id)
    }

    /// Consume the run-keyed pending entry for `run_id`, if any.
    ///
    /// Runners call this once they have observed the cancel and
    /// transitioned the run to a terminal state. Work-item-keyed
    /// entries are unaffected — the parent cancel remains pending so
    /// the propagator can finish fanning it out and the operator can
    /// still see the parent cancel in the queue until it is explicitly
    /// completed.
    pub fn drain_for_run(&mut self, run_id: RunRef) -> Option<CancelRequest> {
        self.by_run.remove(&run_id)
    }

    /// Consume the work-item-keyed pending entry for `work_item_id`, if
    /// any. The propagator calls this after fanning the parent cancel
    /// out into per-run requests.
    pub fn drain_for_work_item(&mut self, work_item_id: WorkItemId) -> Option<CancelRequest> {
        self.by_work_item.remove(&work_item_id)
    }

    /// Total pending entries across both keyspaces. Useful for
    /// observability and tests.
    pub fn len(&self) -> usize {
        self.by_run.len() + self.by_work_item.len()
    }

    /// True when no pending entries remain.
    pub fn is_empty(&self) -> bool {
        self.by_run.is_empty() && self.by_work_item.is_empty()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn req_for_run(id: i64, reason: &str) -> CancelRequest {
        CancelRequest::for_run(RunRef::new(id), reason, "tester", "2026-05-09T00:00:00Z")
    }

    fn req_for_work_item(id: i64, reason: &str) -> CancelRequest {
        CancelRequest::for_work_item(
            WorkItemId::new(id),
            reason,
            "tester",
            "2026-05-09T00:00:00Z",
        )
    }

    #[test]
    fn enqueue_observe_drain_lifecycle_for_run() {
        let mut queue = CancellationQueue::new();
        assert!(queue.is_empty());

        assert!(queue.enqueue(req_for_run(1, "operator cancel")));
        assert_eq!(queue.len(), 1);

        let observed = queue.pending_for_run(RunRef::new(1)).expect("pending");
        assert_eq!(observed.reason, "operator cancel");

        // Observation is non-consuming.
        assert!(queue.pending_for_run(RunRef::new(1)).is_some());

        let drained = queue.drain_for_run(RunRef::new(1)).expect("drain");
        assert_eq!(drained.reason, "operator cancel");
        assert!(queue.pending_for_run(RunRef::new(1)).is_none());
        assert!(queue.is_empty());
    }

    #[test]
    fn enqueue_observe_drain_lifecycle_for_work_item() {
        let mut queue = CancellationQueue::new();

        assert!(queue.enqueue(req_for_work_item(7, "parent cancel")));
        let observed = queue
            .pending_for_work_item(WorkItemId::new(7))
            .expect("pending");
        assert_eq!(observed.reason, "parent cancel");

        let drained = queue
            .drain_for_work_item(WorkItemId::new(7))
            .expect("drain");
        assert_eq!(drained.reason, "parent cancel");
        assert!(queue.is_empty());
    }

    #[test]
    fn re_enqueue_for_same_run_is_idempotent_and_replaces_payload() {
        let mut queue = CancellationQueue::new();

        assert!(queue.enqueue(req_for_run(1, "first")));
        assert!(!queue.enqueue(req_for_run(1, "second")));
        assert_eq!(queue.len(), 1, "duplicate must collapse to one entry");

        let observed = queue.pending_for_run(RunRef::new(1)).expect("pending");
        assert_eq!(observed.reason, "second", "payload must be replaced");
    }

    #[test]
    fn re_enqueue_for_same_work_item_is_idempotent_and_replaces_payload() {
        let mut queue = CancellationQueue::new();

        assert!(queue.enqueue(req_for_work_item(7, "first")));
        assert!(!queue.enqueue(req_for_work_item(7, "second")));
        assert_eq!(queue.len(), 1);

        let observed = queue
            .pending_for_work_item(WorkItemId::new(7))
            .expect("pending");
        assert_eq!(observed.reason, "second");
    }

    #[test]
    fn run_and_work_item_subjects_share_numeric_id_without_collision() {
        let mut queue = CancellationQueue::new();
        // Same numeric id (42), distinct subject kinds. They must not
        // shadow each other — runs and work items live in separate
        // keyspaces.
        assert!(queue.enqueue(req_for_run(42, "run cancel")));
        assert!(queue.enqueue(req_for_work_item(42, "work item cancel")));

        assert_eq!(queue.len(), 2);
        assert_eq!(
            queue.pending_for_run(RunRef::new(42)).map(|r| &r.reason),
            Some(&"run cancel".to_string()),
        );
        assert_eq!(
            queue
                .pending_for_work_item(WorkItemId::new(42))
                .map(|r| &r.reason),
            Some(&"work item cancel".to_string()),
        );
    }

    #[test]
    fn drain_for_run_does_not_affect_work_item_keyspace() {
        let mut queue = CancellationQueue::new();
        queue.enqueue(req_for_run(1, "run cancel"));
        queue.enqueue(req_for_work_item(1, "work item cancel"));

        let drained = queue.drain_for_run(RunRef::new(1)).expect("drained");
        assert_eq!(drained.reason, "run cancel");

        // The work-item-keyed entry is still pending — propagation is a
        // separate concern and the queue must not silently dispose of
        // it.
        assert!(queue.pending_for_work_item(WorkItemId::new(1)).is_some());
        assert_eq!(queue.len(), 1);
    }

    #[test]
    fn drain_for_run_on_missing_subject_returns_none() {
        let mut queue = CancellationQueue::new();
        assert!(queue.drain_for_run(RunRef::new(99)).is_none());
    }

    #[test]
    fn drain_for_work_item_on_missing_subject_returns_none() {
        let mut queue = CancellationQueue::new();
        assert!(queue.drain_for_work_item(WorkItemId::new(99)).is_none());
    }

    #[test]
    fn enqueue_returns_true_on_first_insert_and_false_on_replace() {
        let mut queue = CancellationQueue::new();
        assert!(queue.enqueue(req_for_run(5, "first")));
        assert!(!queue.enqueue(req_for_run(5, "second")));
        assert!(queue.enqueue(req_for_work_item(5, "wi-first")));
        assert!(!queue.enqueue(req_for_work_item(5, "wi-second")));
    }

    #[test]
    fn cancel_request_round_trips_through_serde_for_both_subject_kinds() {
        let run_req = req_for_run(11, "operator");
        let json = serde_json::to_string(&run_req).expect("serialize");
        let back: CancelRequest = serde_json::from_str(&json).expect("deserialize");
        assert_eq!(back, run_req);

        let wi_req = req_for_work_item(22, "cascade");
        let json = serde_json::to_string(&wi_req).expect("serialize");
        let back: CancelRequest = serde_json::from_str(&json).expect("deserialize");
        assert_eq!(back, wi_req);
    }

    #[test]
    fn from_pending_seeds_both_keyspaces_and_preserves_order_independence() {
        let queue = CancellationQueue::from_pending([
            req_for_run(1, "run-cancel"),
            req_for_work_item(1, "wi-cancel"),
        ]);
        assert_eq!(queue.len(), 2);
        assert_eq!(
            queue.pending_for_run(RunRef::new(1)).map(|r| &r.reason),
            Some(&"run-cancel".to_string()),
        );
        assert_eq!(
            queue
                .pending_for_work_item(WorkItemId::new(1))
                .map(|r| &r.reason),
            Some(&"wi-cancel".to_string()),
        );
    }

    #[test]
    fn from_pending_is_idempotent_and_last_write_wins_on_duplicate_subject() {
        let queue =
            CancellationQueue::from_pending([req_for_run(1, "first"), req_for_run(1, "second")]);
        assert_eq!(queue.len(), 1);
        assert_eq!(
            queue.pending_for_run(RunRef::new(1)).map(|r| &r.reason),
            Some(&"second".to_string()),
        );
    }

    #[test]
    fn cancel_subject_serializes_with_snake_case_kind_tag() {
        let run = CancelSubject::run(RunRef::new(1));
        let json = serde_json::to_string(&run).expect("serialize");
        assert_eq!(json, r#"{"kind":"run","run_id":1}"#);

        let wi = CancelSubject::work_item(WorkItemId::new(2));
        let json = serde_json::to_string(&wi).expect("serialize");
        assert_eq!(json, r#"{"kind":"work_item","work_item_id":2}"#);
    }
}
