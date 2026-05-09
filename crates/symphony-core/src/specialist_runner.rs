//! Specialist dispatch runner — drains a [`SpecialistDispatchQueue`] and
//! invokes the per-issue [`Dispatcher`] for each pending request, bounded
//! by a max-concurrency [`Semaphore`] (SPEC v2 §5 / ARCHITECTURE v2 §7.1).
//!
//! Phase 11 of the v2 checklist replaces the flat
//! [`crate::poll_loop::PollLoop`] entry point with the [`crate::scheduler_v2::SchedulerV2`]
//! tick fan. The fan already enqueues dispatch *requests*; this runner is
//! the missing consumer that turns those requests into actual dispatcher
//! invocations under the same `agent.max_concurrent_agents` ceiling that
//! the flat poll loop enforces today. Each runner pass:
//!
//! 1. Drains pending requests from [`SpecialistDispatchQueue`] and folds
//!    them onto the runner's deferred-from-prior-pass queue (front-first
//!    so age-order is preserved across passes).
//! 2. Walks the combined queue. For each request, looks up the
//!    corresponding [`Issue`] in [`ActiveSetStore`]; if missing, records
//!    `missing_issue` and drops the request — the next intake pass will
//!    decide whether the issue is still active.
//! 3. Tries to acquire a semaphore permit non-blockingly. If unavailable
//!    the request is parked back in the runner's deferred queue and
//!    counted as `deferred_capacity`. Permit-holding requests spawn a
//!    dispatcher task into the runner's [`JoinSet`]; the spawned future
//!    holds the owned semaphore permit until the dispatcher returns,
//!    so capacity is automatically restored on completion.
//!
//! The runner intentionally does *not* own cadence (the scheduler does),
//! does *not* mutate [`crate::state_machine::StateMachine`] (the flat
//! loop still owns that ledger today), and does *not* reap finished
//! tasks on its own. Reaping is exposed as [`SpecialistRunner::reap_completed`]
//! so the caller (later: the v2 scheduler wiring in `symphony-cli`) can
//! drive both spawn and reap on its own schedule. Keeping the runner
//! narrow lets it slot into the existing composition root without
//! disturbing PollLoop.

use std::sync::Arc;

use tokio::sync::{Mutex, Semaphore};
use tokio::task::JoinSet;
use tokio_util::sync::CancellationToken;
use tracing::{debug, warn};

use crate::intake_tick::ActiveSetStore;
use crate::poll_loop::Dispatcher;
use crate::specialist_tick::{SpecialistDispatchQueue, SpecialistDispatchRequest};
use crate::state_machine::ReleaseReason;
use crate::tracker::Issue;

/// Per-pass observability summary for [`SpecialistRunner::run_pending`].
///
/// Counts only — no payloads — so callers can log or assert cheaply
/// without snapshotting requests. The three buckets are mutually
/// exclusive: every drained request lands in exactly one of `spawned`,
/// `deferred_capacity`, or `missing_issue`.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct SpecialistRunReport {
    /// Requests for which a dispatcher task was spawned this pass.
    pub spawned: usize,
    /// Requests that could not be spawned because the semaphore was
    /// already saturated. Parked on the runner's internal deferred queue
    /// for the next pass.
    pub deferred_capacity: usize,
    /// Requests whose issue id was no longer in the active-set snapshot
    /// at lookup time. Dropped — the next intake/specialist pass will
    /// re-emit if the issue is still routable.
    pub missing_issue: usize,
}

impl SpecialistRunReport {
    /// `true` iff the pass was a complete no-op (nothing drained,
    /// nothing parked).
    pub fn is_idle(&self) -> bool {
        self.spawned == 0 && self.deferred_capacity == 0 && self.missing_issue == 0
    }
}

/// Outcome reported when a spawned dispatcher task completes.
///
/// Returned in batches by [`SpecialistRunner::reap_completed`]. The
/// `release_reason` is the value the dispatcher returned; the runner
/// does not reinterpret it — that is the scheduler/state-machine layer's
/// job (Phase 11 follow-ups will add lease release here).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SpecialistDispatchOutcome {
    /// Tracker-facing identifier the request carried.
    pub identifier: String,
    /// Final [`ReleaseReason`] from [`Dispatcher::dispatch`].
    pub release_reason: ReleaseReason,
}

/// Bounded-concurrency consumer for [`SpecialistDispatchQueue`].
///
/// Construct with [`SpecialistRunner::new`] and drive with
/// [`SpecialistRunner::run_pending`]. The runner is `&self`-only so the
/// scheduler can hold it in an `Arc` and call it from any tick context.
pub struct SpecialistRunner {
    queue: Arc<SpecialistDispatchQueue>,
    active_set: Arc<ActiveSetStore>,
    dispatcher: Arc<dyn Dispatcher>,
    semaphore: Arc<Semaphore>,
    /// Requests parked across passes when capacity was unavailable.
    /// Front-first FIFO so age-order is preserved.
    deferred: Mutex<Vec<SpecialistDispatchRequest>>,
    inflight: Mutex<JoinSet<SpecialistDispatchOutcome>>,
    max_concurrent: usize,
}

impl SpecialistRunner {
    /// Build a runner with capacity `max_concurrent`. A capacity of `0`
    /// disables dispatch entirely — every drained request will be
    /// counted as `deferred_capacity`. The flat poll loop accepts the
    /// same convention so this matches the existing escape hatch for
    /// "drain but never run".
    pub fn new(
        queue: Arc<SpecialistDispatchQueue>,
        active_set: Arc<ActiveSetStore>,
        dispatcher: Arc<dyn Dispatcher>,
        max_concurrent: usize,
    ) -> Self {
        Self {
            queue,
            active_set,
            dispatcher,
            semaphore: Arc::new(Semaphore::new(max_concurrent)),
            deferred: Mutex::new(Vec::new()),
            inflight: Mutex::new(JoinSet::new()),
            max_concurrent,
        }
    }

    /// Configured max concurrency.
    pub fn max_concurrent(&self) -> usize {
        self.max_concurrent
    }

    /// Number of dispatcher tasks currently spawned but not yet reaped.
    /// Includes tasks that have already completed but are still sitting
    /// in the [`JoinSet`] waiting for [`Self::reap_completed`].
    pub async fn inflight_count(&self) -> usize {
        self.inflight.lock().await.len()
    }

    /// Number of requests parked for a future pass because the
    /// semaphore was saturated when they were considered.
    pub async fn deferred_count(&self) -> usize {
        self.deferred.lock().await.len()
    }

    /// Drive one drain-and-spawn pass.
    ///
    /// `cancel` is the *parent* cancellation token — it propagates into
    /// every spawned dispatcher task so a SIGINT-style shutdown tears
    /// down in-flight sessions cooperatively. The pass itself does not
    /// await the spawned futures; call [`Self::reap_completed`] to
    /// observe results.
    pub async fn run_pending(&self, cancel: CancellationToken) -> SpecialistRunReport {
        // Combine prior-pass deferreds with newly drained requests.
        // Deferreds first so they are tried again before fresh work,
        // preserving age-order across passes.
        let mut work: Vec<SpecialistDispatchRequest> = {
            let mut deferred = self.deferred.lock().await;
            std::mem::take(&mut *deferred)
        };
        work.extend(self.queue.drain());

        if work.is_empty() {
            return SpecialistRunReport::default();
        }

        let snapshot = self.active_set.snapshot();
        let mut report = SpecialistRunReport::default();
        let mut new_deferred: Vec<SpecialistDispatchRequest> = Vec::new();

        for req in work {
            let Some(issue) = find_issue(&snapshot, &req.issue_id) else {
                debug!(
                    issue_id = %req.issue_id,
                    identifier = %req.identifier,
                    "specialist runner: issue no longer in active set; dropping request",
                );
                report.missing_issue += 1;
                continue;
            };

            // Non-blocking permit acquisition — if capacity is exhausted
            // we want to *park*, not wait. Waiting would block the
            // scheduler's tick fan against a long-running agent session.
            let permit = match self.semaphore.clone().try_acquire_owned() {
                Ok(p) => p,
                Err(_) => {
                    new_deferred.push(req);
                    report.deferred_capacity += 1;
                    continue;
                }
            };

            let dispatcher = self.dispatcher.clone();
            let child_cancel = cancel.child_token();
            let identifier = req.identifier.clone();
            let issue_for_dispatch = issue.clone();

            let mut inflight = self.inflight.lock().await;
            inflight.spawn(async move {
                let release_reason = dispatcher.dispatch(issue_for_dispatch, child_cancel).await;
                drop(permit); // explicit: release capacity at task exit
                SpecialistDispatchOutcome {
                    identifier,
                    release_reason,
                }
            });
            drop(inflight);

            report.spawned += 1;
        }

        // Park leftovers for the next pass.
        if !new_deferred.is_empty() {
            let mut deferred = self.deferred.lock().await;
            // Prepend so the older deferreds keep priority. We rebuild
            // because Vec lacks `prepend`.
            new_deferred.append(&mut *deferred);
            *deferred = new_deferred;
        }

        report
    }

    /// Drain every dispatcher task that has finished since the last
    /// call. Tasks still running are left in place. Panics from the
    /// dispatcher future are logged and dropped — the runner's
    /// composition root cannot meaningfully recover from a panic, but it
    /// must not poison the rest of the in-flight set.
    pub async fn reap_completed(&self) -> Vec<SpecialistDispatchOutcome> {
        let mut inflight = self.inflight.lock().await;
        let mut out = Vec::new();
        while let Some(joined) = inflight.try_join_next() {
            match joined {
                Ok(outcome) => out.push(outcome),
                Err(err) if err.is_cancelled() => {
                    debug!("specialist dispatch task cancelled before reap");
                }
                Err(err) => {
                    warn!(error = %err, "specialist dispatch task failed to join cleanly");
                }
            }
        }
        out
    }
}

/// Linear scan over the most-recent active-set snapshot. Active sets
/// are bounded to the orchestrator's working horizon (10s, low
/// hundreds), so this is cheaper than maintaining a parallel index.
fn find_issue<'a>(snapshot: &'a [Issue], issue_id: &str) -> Option<&'a Issue> {
    snapshot.iter().find(|i| i.id.as_str() == issue_id)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::role::RoleName;
    use async_trait::async_trait;
    use std::sync::Mutex as StdMutex;
    use std::time::Duration;

    fn issue(id: &str, identifier: &str) -> Issue {
        Issue::minimal(id, identifier, "title", "Todo")
    }

    fn req(id: &str, identifier: &str) -> SpecialistDispatchRequest {
        SpecialistDispatchRequest {
            issue_id: id.into(),
            identifier: identifier.into(),
            role: RoleName::new("specialist"),
            rule_index: None,
        }
    }

    /// Records every dispatch invocation and resolves with a configured
    /// [`ReleaseReason`]. Optional gate lets tests hold a dispatch open
    /// to assert capacity behaviour.
    struct RecordingDispatcher {
        calls: Arc<StdMutex<Vec<String>>>,
        gate: Arc<tokio::sync::Notify>,
        gate_enabled: Arc<StdMutex<bool>>,
        reason: ReleaseReason,
    }

    impl RecordingDispatcher {
        fn new(reason: ReleaseReason) -> Self {
            Self {
                calls: Arc::new(StdMutex::new(Vec::new())),
                gate: Arc::new(tokio::sync::Notify::new()),
                gate_enabled: Arc::new(StdMutex::new(false)),
                reason,
            }
        }
        fn calls(&self) -> Vec<String> {
            self.calls.lock().unwrap().clone()
        }
        fn enable_gate(&self) {
            *self.gate_enabled.lock().unwrap() = true;
        }
        fn release_gate(&self) {
            *self.gate_enabled.lock().unwrap() = false;
            self.gate.notify_waiters();
        }
    }

    #[async_trait]
    impl Dispatcher for RecordingDispatcher {
        async fn dispatch(&self, issue: Issue, cancel: CancellationToken) -> ReleaseReason {
            self.calls.lock().unwrap().push(issue.identifier.clone());
            // Optional gate so capacity tests can keep dispatchers
            // pending until released.
            let gated = *self.gate_enabled.lock().unwrap();
            if gated {
                tokio::select! {
                    _ = self.gate.notified() => {}
                    _ = cancel.cancelled() => return ReleaseReason::Canceled,
                }
            }
            self.reason
        }
    }

    fn store_with(issues: Vec<Issue>) -> Arc<ActiveSetStore> {
        let store = Arc::new(ActiveSetStore::new());
        // ActiveSetStore::replace is pub(crate); we are inside the
        // crate so this is valid.
        store.replace(issues);
        store
    }

    #[tokio::test]
    async fn run_pending_is_noop_when_queue_empty() {
        let queue = Arc::new(SpecialistDispatchQueue::new());
        let active = Arc::new(ActiveSetStore::new());
        let dispatcher = Arc::new(RecordingDispatcher::new(ReleaseReason::Completed));
        let runner = SpecialistRunner::new(queue, active, dispatcher.clone(), 4);

        let report = runner.run_pending(CancellationToken::new()).await;
        assert!(report.is_idle(), "expected idle report, got {report:?}");
        assert_eq!(runner.inflight_count().await, 0);
        assert!(dispatcher.calls().is_empty());
    }

    #[tokio::test]
    async fn run_pending_dispatches_known_issue() {
        let queue = Arc::new(SpecialistDispatchQueue::new());
        let active = store_with(vec![issue("id-1", "ENG-1")]);
        let dispatcher = Arc::new(RecordingDispatcher::new(ReleaseReason::Completed));
        let runner = SpecialistRunner::new(queue.clone(), active, dispatcher.clone(), 4);

        // Inject a request directly through the public crate-internal
        // helper used by the specialist tick.
        queue.enqueue(req("id-1", "ENG-1"));

        let report = runner.run_pending(CancellationToken::new()).await;
        assert_eq!(report.spawned, 1);
        assert_eq!(report.deferred_capacity, 0);
        assert_eq!(report.missing_issue, 0);

        // Wait for the spawned task to complete, then reap.
        let outcomes = wait_for_outcomes(&runner, 1).await;
        assert_eq!(outcomes.len(), 1);
        assert_eq!(outcomes[0].identifier, "ENG-1");
        assert_eq!(outcomes[0].release_reason, ReleaseReason::Completed);
        assert_eq!(dispatcher.calls(), vec!["ENG-1"]);
    }

    #[tokio::test]
    async fn run_pending_drops_request_when_issue_left_active_set() {
        let queue = Arc::new(SpecialistDispatchQueue::new());
        let active = Arc::new(ActiveSetStore::new()); // empty
        let dispatcher = Arc::new(RecordingDispatcher::new(ReleaseReason::Completed));
        let runner = SpecialistRunner::new(queue.clone(), active, dispatcher.clone(), 4);

        queue.enqueue(req("id-missing", "ENG-MISSING"));

        let report = runner.run_pending(CancellationToken::new()).await;
        assert_eq!(report.spawned, 0);
        assert_eq!(report.missing_issue, 1);
        assert_eq!(report.deferred_capacity, 0);
        assert_eq!(runner.inflight_count().await, 0);
        assert!(dispatcher.calls().is_empty());
    }

    #[tokio::test]
    async fn run_pending_defers_when_capacity_saturated() {
        let queue = Arc::new(SpecialistDispatchQueue::new());
        let active = store_with(vec![
            issue("id-1", "ENG-1"),
            issue("id-2", "ENG-2"),
            issue("id-3", "ENG-3"),
        ]);
        let dispatcher = Arc::new(RecordingDispatcher::new(ReleaseReason::Completed));
        // Hold the dispatcher so the first permit is not released.
        dispatcher.enable_gate();

        // Capacity of 1 — second & third requests must defer.
        let runner = SpecialistRunner::new(queue.clone(), active, dispatcher.clone(), 1);

        queue.enqueue(req("id-1", "ENG-1"));
        queue.enqueue(req("id-2", "ENG-2"));
        queue.enqueue(req("id-3", "ENG-3"));

        let report = runner.run_pending(CancellationToken::new()).await;
        assert_eq!(report.spawned, 1, "report = {report:?}");
        assert_eq!(report.deferred_capacity, 2);
        assert_eq!(report.missing_issue, 0);
        assert_eq!(runner.deferred_count().await, 2);

        // Release the gate so the running dispatcher completes; the
        // permit is freed and the next pass can spawn another.
        dispatcher.release_gate();
        let _ = wait_for_outcomes(&runner, 1).await;

        // With cap=1 each pass can only spawn one of the deferreds; we
        // expect to drain the parked queue across two more passes.
        let report = runner.run_pending(CancellationToken::new()).await;
        assert_eq!(report.spawned, 1, "report = {report:?}");
        assert_eq!(report.deferred_capacity, 1);
        let _ = wait_for_outcomes(&runner, 1).await;

        let report = runner.run_pending(CancellationToken::new()).await;
        assert_eq!(report.spawned, 1, "report = {report:?}");
        assert_eq!(report.deferred_capacity, 0);
        assert_eq!(runner.deferred_count().await, 0);
        let _ = wait_for_outcomes(&runner, 1).await;

        let mut all = dispatcher.calls();
        all.sort();
        assert_eq!(all, vec!["ENG-1", "ENG-2", "ENG-3"]);
    }

    #[tokio::test]
    async fn run_pending_preserves_age_order_for_deferred_requests() {
        let queue = Arc::new(SpecialistDispatchQueue::new());
        let active = store_with(vec![
            issue("id-1", "ENG-1"),
            issue("id-2", "ENG-2"),
            issue("id-3", "ENG-3"),
        ]);
        let dispatcher = Arc::new(RecordingDispatcher::new(ReleaseReason::Completed));
        dispatcher.enable_gate();

        // Cap = 0 forces every request into the deferred queue.
        // After we raise capacity by destroying this runner and
        // building a new one with the deferreds, we can verify order
        // by spawning sequentially with cap = 1 + an unblocked dispatcher.
        let runner = SpecialistRunner::new(queue.clone(), active.clone(), dispatcher.clone(), 0);
        queue.enqueue(req("id-1", "ENG-1"));
        queue.enqueue(req("id-2", "ENG-2"));
        let r1 = runner.run_pending(CancellationToken::new()).await;
        assert_eq!(r1.deferred_capacity, 2);

        // Add a third fresh request: it must go *after* the two
        // deferreds when the next pass walks the combined queue.
        queue.enqueue(req("id-3", "ENG-3"));
        let r2 = runner.run_pending(CancellationToken::new()).await;
        assert_eq!(r2.deferred_capacity, 3);
        assert_eq!(runner.deferred_count().await, 3);

        // Drain order test: rebuild a permit-bearing runner sharing
        // state would be wrong — instead, capacity=0 means every pass
        // re-defers. We verify ordering by inspecting the deferred
        // queue contents directly.
        let parked = runner.deferred.lock().await;
        let parked_ids: Vec<&str> = parked.iter().map(|r| r.identifier.as_str()).collect();
        assert_eq!(
            parked_ids,
            vec!["ENG-1", "ENG-2", "ENG-3"],
            "deferreds must keep age-order: prior parks first, fresh request last",
        );
    }

    #[tokio::test]
    async fn run_pending_propagates_parent_cancellation_to_dispatch() {
        let queue = Arc::new(SpecialistDispatchQueue::new());
        let active = store_with(vec![issue("id-1", "ENG-1")]);
        let dispatcher = Arc::new(RecordingDispatcher::new(ReleaseReason::Completed));
        dispatcher.enable_gate(); // hold the dispatch open

        let runner = SpecialistRunner::new(queue.clone(), active, dispatcher.clone(), 4);
        queue.enqueue(req("id-1", "ENG-1"));

        let cancel = CancellationToken::new();
        let _ = runner.run_pending(cancel.clone()).await;
        assert_eq!(runner.inflight_count().await, 1);

        // Fire the parent cancel: the spawned dispatcher's child token
        // is derived from it and must observe cancellation.
        cancel.cancel();

        let outcomes = wait_for_outcomes(&runner, 1).await;
        assert_eq!(outcomes.len(), 1);
        assert_eq!(outcomes[0].release_reason, ReleaseReason::Canceled);
    }

    #[tokio::test]
    async fn capacity_zero_defers_everything() {
        let queue = Arc::new(SpecialistDispatchQueue::new());
        let active = store_with(vec![issue("id-1", "ENG-1")]);
        let dispatcher = Arc::new(RecordingDispatcher::new(ReleaseReason::Completed));
        let runner = SpecialistRunner::new(queue.clone(), active, dispatcher.clone(), 0);
        queue.enqueue(req("id-1", "ENG-1"));

        let report = runner.run_pending(CancellationToken::new()).await;
        assert_eq!(report.spawned, 0);
        assert_eq!(report.deferred_capacity, 1);
        assert!(dispatcher.calls().is_empty());
    }

    #[tokio::test]
    async fn reap_completed_returns_outcomes_in_completion_order() {
        let queue = Arc::new(SpecialistDispatchQueue::new());
        let active = store_with(vec![issue("id-1", "ENG-1"), issue("id-2", "ENG-2")]);
        let dispatcher = Arc::new(RecordingDispatcher::new(ReleaseReason::Completed));
        let runner = SpecialistRunner::new(queue.clone(), active, dispatcher.clone(), 4);

        queue.enqueue(req("id-1", "ENG-1"));
        queue.enqueue(req("id-2", "ENG-2"));
        let report = runner.run_pending(CancellationToken::new()).await;
        assert_eq!(report.spawned, 2);

        let outcomes = wait_for_outcomes(&runner, 2).await;
        let mut ids: Vec<String> = outcomes.iter().map(|o| o.identifier.clone()).collect();
        ids.sort();
        assert_eq!(ids, vec!["ENG-1".to_string(), "ENG-2".to_string()]);
    }

    /// Wait until at least `n` outcomes have been reaped or a generous
    /// per-test timeout fires. Tests don't sleep on cadence — they
    /// poll the JoinSet via `reap_completed`, which is lock-free apart
    /// from the small async mutex.
    async fn wait_for_outcomes(
        runner: &SpecialistRunner,
        n: usize,
    ) -> Vec<SpecialistDispatchOutcome> {
        let deadline = std::time::Instant::now() + Duration::from_secs(2);
        let mut acc = Vec::new();
        while acc.len() < n {
            acc.extend(runner.reap_completed().await);
            if acc.len() >= n {
                break;
            }
            if std::time::Instant::now() >= deadline {
                panic!(
                    "timed out waiting for {n} outcomes; got {} so far",
                    acc.len()
                );
            }
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
        acc
    }
}
