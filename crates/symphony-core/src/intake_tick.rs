//! Intake [`QueueTick`] — wraps `TrackerRead::fetch_active` behind the
//! v2 queue-tick abstraction (SPEC v2 §5 / ARCHITECTURE v2 §7.1).
//!
//! Phase 11 of the v2 checklist replaces the flat poll loop with a fan
//! of logical queues. The intake queue is the entry point of that fan:
//! once per cadence it asks the tracker for the current active set and
//! publishes the snapshot into a shared [`ActiveSetStore`]. Downstream
//! queue ticks (specialist, recovery, …) read from the store rather
//! than calling `fetch_active` themselves, so the scheduler issues
//! exactly one tracker read per cadence — matching the existing flat
//! `PollLoop::tick` contract.
//!
//! Observable behavior parity with the v1 [`crate::poll_loop::PollLoop`]
//! is the explicit goal of this iteration:
//!
//! * Same tracker call (`fetch_active`).
//! * Same active-set contents on success.
//! * Same fail-soft posture on tracker error: the previous snapshot is
//!   left in place and the tick reports `errors = 1` so the scheduler
//!   can log/continue without unwinding.
//!
//! Claim/dispatch/reconciliation logic still lives on the poll loop in
//! Phase 11; the specialist, integration, QA, follow-up, budget-pause,
//! and recovery queue ticks layer on top of the same store in
//! subsequent checklist items.

use std::sync::{Arc, Mutex};

use async_trait::async_trait;

use crate::logical_queue::{LogicalQueue, QueueTickOutcome};
use crate::queue_tick::{QueueTick, QueueTickCadence};
use crate::tracker::Issue;
use crate::tracker_trait::TrackerRead;

/// Shared store for the most recent active-set snapshot observed by the
/// intake queue tick.
///
/// The store is intentionally minimal: it owns one `Vec<Issue>` behind
/// a `std::sync::Mutex` and exposes `snapshot` / `len` / `is_empty` to
/// downstream queue ticks. There are no awaits while the lock is held,
/// so a synchronous mutex is correct and avoids an extra tokio
/// dependency on every reader.
///
/// Callers wrap the store in an `Arc` so it can be shared between the
/// intake tick (writer) and any number of downstream readers without
/// re-fetching from the tracker.
#[derive(Default, Debug)]
pub struct ActiveSetStore {
    inner: Mutex<Vec<Issue>>,
}

impl ActiveSetStore {
    /// Construct an empty store.
    pub fn new() -> Self {
        Self::default()
    }

    /// Clone the current snapshot. Cheap for the small active sets the
    /// orchestrator handles; if it ever becomes a bottleneck the
    /// signature lets us swap the body for an `Arc<[Issue]>` without
    /// touching callers.
    pub fn snapshot(&self) -> Vec<Issue> {
        self.inner
            .lock()
            .expect("active set store mutex poisoned")
            .clone()
    }

    /// Number of issues in the latest snapshot.
    pub fn len(&self) -> usize {
        self.inner
            .lock()
            .expect("active set store mutex poisoned")
            .len()
    }

    /// `true` iff the latest snapshot is empty (no active issues).
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// Replace the snapshot wholesale. Pub-crate so only the intake
    /// tick (and its tests) can mutate the store; downstream consumers
    /// must go through the read-only API.
    pub(crate) fn replace(&self, issues: Vec<Issue>) {
        *self.inner.lock().expect("active set store mutex poisoned") = issues;
    }
}

/// Intake queue tick.
///
/// One tick = one `fetch_active` round-trip. The tick is bounded:
/// no retries, no waits, no claim/dispatch logic. The scheduler is
/// responsible for cadence (`polling.interval_ms`, jitter) and for
/// fanning the resulting active-set snapshot out to downstream queue
/// ticks via the shared [`ActiveSetStore`].
///
/// On tracker error the tick reports `errors = 1` and intentionally
/// leaves the previous snapshot in place. That mirrors the v1 poll
/// loop's behavior — a transient tracker hiccup must not flap the
/// downstream consumers' view of the world.
pub struct IntakeQueueTick {
    tracker: Arc<dyn TrackerRead>,
    active_set: Arc<ActiveSetStore>,
    cadence: QueueTickCadence,
}

impl IntakeQueueTick {
    /// Construct an intake tick that publishes into `active_set`.
    pub fn new(
        tracker: Arc<dyn TrackerRead>,
        active_set: Arc<ActiveSetStore>,
        cadence: QueueTickCadence,
    ) -> Self {
        Self {
            tracker,
            active_set,
            cadence,
        }
    }

    /// Borrow the shared active-set store. Lets downstream queue ticks
    /// be wired from a single intake instance without the composition
    /// root having to thread the `Arc` separately.
    pub fn active_set(&self) -> &Arc<ActiveSetStore> {
        &self.active_set
    }
}

#[async_trait]
impl QueueTick for IntakeQueueTick {
    fn queue(&self) -> LogicalQueue {
        LogicalQueue::Intake
    }

    fn cadence(&self) -> QueueTickCadence {
        self.cadence
    }

    async fn tick(&mut self) -> QueueTickOutcome {
        match self.tracker.fetch_active().await {
            Ok(issues) => {
                let n = issues.len();
                self.active_set.replace(issues);
                QueueTickOutcome {
                    queue: LogicalQueue::Intake,
                    considered: n,
                    processed: n,
                    deferred: 0,
                    errors: 0,
                }
            }
            Err(_err) => QueueTickOutcome {
                queue: LogicalQueue::Intake,
                considered: 0,
                processed: 0,
                deferred: 0,
                errors: 1,
            },
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::queue_tick::run_queue_tick_n;
    use crate::tracker::{Issue, IssueId, IssueState};
    use crate::tracker_trait::{TrackerError, TrackerResult};
    use std::sync::Mutex as StdMutex;
    use std::time::Duration;

    /// Tracker that returns a fixed (mutable) snapshot. Mirrors the
    /// shape of `poll_loop::tests::StaticTracker` so the parity tests
    /// drive both code paths from an identical source of truth.
    struct StaticTracker {
        active: StdMutex<Vec<Issue>>,
        fail_next: StdMutex<bool>,
    }

    impl StaticTracker {
        fn new(active: Vec<Issue>) -> Self {
            Self {
                active: StdMutex::new(active),
                fail_next: StdMutex::new(false),
            }
        }
        fn replace(&self, active: Vec<Issue>) {
            *self.active.lock().unwrap() = active;
        }
        fn arm_failure(&self) {
            *self.fail_next.lock().unwrap() = true;
        }
    }

    #[async_trait]
    impl TrackerRead for StaticTracker {
        async fn fetch_active(&self) -> TrackerResult<Vec<Issue>> {
            let mut fail = self.fail_next.lock().unwrap();
            if *fail {
                *fail = false;
                return Err(TrackerError::Transport("scripted failure".into()));
            }
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
            Ok(Vec::new())
        }
    }

    fn issues(n: usize) -> Vec<Issue> {
        (0..n)
            .map(|i| Issue::minimal(format!("id-{i}"), format!("ENG-{i}"), "title", "Todo"))
            .collect()
    }

    fn cadence() -> QueueTickCadence {
        QueueTickCadence::fixed(Duration::from_millis(1))
    }

    #[tokio::test]
    async fn tick_publishes_active_snapshot_into_store() {
        let tracker = Arc::new(StaticTracker::new(issues(3)));
        let store = Arc::new(ActiveSetStore::new());
        let mut intake = IntakeQueueTick::new(
            tracker.clone() as Arc<dyn TrackerRead>,
            store.clone(),
            cadence(),
        );

        let outcome = intake.tick().await;
        assert_eq!(outcome.queue, LogicalQueue::Intake);
        assert_eq!(outcome.considered, 3);
        assert_eq!(outcome.processed, 3);
        assert_eq!(outcome.deferred, 0);
        assert_eq!(outcome.errors, 0);
        assert_eq!(store.len(), 3);
        assert_eq!(
            store
                .snapshot()
                .iter()
                .map(|i| i.identifier.clone())
                .collect::<Vec<_>>(),
            vec!["ENG-0", "ENG-1", "ENG-2"],
        );
    }

    #[tokio::test]
    async fn tick_idle_when_active_set_is_empty() {
        let tracker = Arc::new(StaticTracker::new(Vec::new()));
        let store = Arc::new(ActiveSetStore::new());
        let mut intake =
            IntakeQueueTick::new(tracker as Arc<dyn TrackerRead>, store.clone(), cadence());

        let outcome = intake.tick().await;
        assert!(outcome.is_idle());
        assert_eq!(outcome.queue, LogicalQueue::Intake);
        assert!(store.is_empty());
    }

    #[tokio::test]
    async fn tick_replaces_previous_snapshot_each_call() {
        let tracker = Arc::new(StaticTracker::new(issues(2)));
        let store = Arc::new(ActiveSetStore::new());
        let mut intake = IntakeQueueTick::new(
            tracker.clone() as Arc<dyn TrackerRead>,
            store.clone(),
            cadence(),
        );

        intake.tick().await;
        assert_eq!(store.len(), 2);

        // Tracker now returns a different (smaller) set: store must
        // wholesale-replace, not append.
        tracker.replace(issues(1));
        intake.tick().await;
        let snap = store.snapshot();
        assert_eq!(snap.len(), 1);
        assert_eq!(snap[0].identifier, "ENG-0");
    }

    #[tokio::test]
    async fn tick_error_path_increments_errors_and_preserves_store() {
        let tracker = Arc::new(StaticTracker::new(issues(2)));
        let store = Arc::new(ActiveSetStore::new());
        let mut intake = IntakeQueueTick::new(
            tracker.clone() as Arc<dyn TrackerRead>,
            store.clone(),
            cadence(),
        );

        // Seed a known snapshot.
        intake.tick().await;
        assert_eq!(store.len(), 2);

        // Next call fails: previous snapshot must remain intact.
        tracker.arm_failure();
        let outcome = intake.tick().await;
        assert_eq!(outcome.queue, LogicalQueue::Intake);
        assert_eq!(outcome.errors, 1);
        assert_eq!(outcome.considered, 0);
        assert_eq!(outcome.processed, 0);
        assert_eq!(
            store.len(),
            2,
            "store must keep last good snapshot on error"
        );
    }

    #[tokio::test]
    async fn tick_reports_intake_queue_and_configured_cadence() {
        let tracker = Arc::new(StaticTracker::new(Vec::new()));
        let store = Arc::new(ActiveSetStore::new());
        let cad = QueueTickCadence::from_millis(2_500, 100);
        let intake = IntakeQueueTick::new(tracker as Arc<dyn TrackerRead>, store, cad);
        assert_eq!(intake.queue(), LogicalQueue::Intake);
        assert_eq!(intake.cadence(), cad);
    }

    #[tokio::test]
    async fn run_queue_tick_n_drives_intake_through_trait_object() {
        // Confirms IntakeQueueTick composes with the deterministic
        // harness: the scheduler will store ticks as `Box<dyn QueueTick>`.
        let tracker = Arc::new(StaticTracker::new(issues(2)));
        let store = Arc::new(ActiveSetStore::new());
        let mut boxed: Box<dyn QueueTick> = Box::new(IntakeQueueTick::new(
            tracker as Arc<dyn TrackerRead>,
            store.clone(),
            cadence(),
        ));

        let outcomes = run_queue_tick_n(boxed.as_mut(), 3).await;
        assert_eq!(outcomes.len(), 3);
        for o in &outcomes {
            assert_eq!(o.queue, LogicalQueue::Intake);
            assert_eq!(o.considered, 2);
            assert_eq!(o.processed, 2);
        }
        assert_eq!(store.len(), 2);
    }

    // -----------------------------------------------------------------
    // Parity with the v1 flat poll loop's `fetch_active` step.
    //
    // The intake tick must observe *exactly* the same active-set
    // membership and counts that `PollLoop::tick` records as
    // `TickReport::observed`. These tests pin that contract so the v2
    // scheduler can drop intake into place without changing what
    // downstream consumers see.
    // -----------------------------------------------------------------

    use crate::poll_loop::{Dispatcher, PollLoop, PollLoopConfig};
    use crate::state_machine::ReleaseReason;
    use tokio_util::sync::CancellationToken;

    /// Dispatcher that resolves immediately; lets the parity tests run
    /// `PollLoop::tick` without leaking gated tasks.
    struct InstantDispatcher;
    #[async_trait]
    impl Dispatcher for InstantDispatcher {
        async fn dispatch(&self, _issue: Issue, _cancel: CancellationToken) -> ReleaseReason {
            ReleaseReason::Completed
        }
    }

    #[tokio::test]
    async fn parity_intake_tick_matches_poll_loop_observed_count() {
        let active = issues(4);
        let tracker_a = Arc::new(StaticTracker::new(active.clone()));
        let tracker_b = Arc::new(StaticTracker::new(active.clone()));

        // v1 path: drive the existing flat poll loop tick.
        let mut loop_ = PollLoop::new(
            tracker_a.clone() as Arc<dyn TrackerRead>,
            Arc::new(InstantDispatcher) as Arc<dyn Dispatcher>,
            PollLoopConfig {
                interval: Duration::from_millis(1),
                jitter_ratio: 0.0,
                max_concurrent: 0, // do not actually claim — we only care about `observed`
                drain_deadline: Duration::from_millis(10),
            },
        );
        let cancel = CancellationToken::new();
        let report = loop_.tick(&cancel).await.unwrap();

        // v2 path: drive the new intake queue tick against an
        // equivalently configured tracker.
        let store = Arc::new(ActiveSetStore::new());
        let mut intake =
            IntakeQueueTick::new(tracker_b as Arc<dyn TrackerRead>, store.clone(), cadence());
        let outcome = intake.tick().await;

        // Same shape: `observed` from the flat loop equals
        // `considered`/`processed` from the intake tick, and the store
        // contents match the issues the tracker returned.
        assert_eq!(report.observed, outcome.considered);
        assert_eq!(report.observed, outcome.processed);
        assert_eq!(report.observed, active.len());
        let snap_ids: Vec<_> = store.snapshot().iter().map(|i| i.id.clone()).collect();
        let active_ids: Vec<_> = active.iter().map(|i| i.id.clone()).collect();
        assert_eq!(snap_ids, active_ids);

        cancel.cancel();
    }

    #[tokio::test]
    async fn parity_intake_tick_tracks_active_set_changes_like_poll_loop() {
        let tracker_a = Arc::new(StaticTracker::new(issues(3)));
        let tracker_b = Arc::new(StaticTracker::new(issues(3)));

        let mut loop_ = PollLoop::new(
            tracker_a.clone() as Arc<dyn TrackerRead>,
            Arc::new(InstantDispatcher) as Arc<dyn Dispatcher>,
            PollLoopConfig {
                interval: Duration::from_millis(1),
                jitter_ratio: 0.0,
                max_concurrent: 0,
                drain_deadline: Duration::from_millis(10),
            },
        );
        let store = Arc::new(ActiveSetStore::new());
        let mut intake = IntakeQueueTick::new(
            tracker_b.clone() as Arc<dyn TrackerRead>,
            store.clone(),
            cadence(),
        );
        let cancel = CancellationToken::new();

        // First tick: both observe 3.
        let r1 = loop_.tick(&cancel).await.unwrap();
        let o1 = intake.tick().await;
        assert_eq!(r1.observed, 3);
        assert_eq!(o1.considered, 3);
        assert_eq!(store.len(), 3);

        // Active set shrinks: both must observe the new size.
        tracker_a.replace(issues(1));
        tracker_b.replace(issues(1));
        let r2 = loop_.tick(&cancel).await.unwrap();
        let o2 = intake.tick().await;
        assert_eq!(r2.observed, 1);
        assert_eq!(o2.considered, 1);
        assert_eq!(store.len(), 1);

        // Active set empties: both observe zero / idle.
        tracker_a.replace(Vec::new());
        tracker_b.replace(Vec::new());
        let r3 = loop_.tick(&cancel).await.unwrap();
        let o3 = intake.tick().await;
        assert_eq!(r3.observed, 0);
        assert!(o3.is_idle());
        assert!(store.is_empty());

        cancel.cancel();
    }
}
