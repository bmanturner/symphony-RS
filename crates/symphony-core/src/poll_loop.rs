//! Orchestrator poll loop (SPEC §3.1 "Orchestrator", §7, §16).
//!
//! The poll loop is the heartbeat of Symphony. On every tick it:
//!
//! 1. Reaps any dispatch tasks that finished since the previous tick and
//!    releases their claims back to the [`StateMachine`].
//! 2. Asks the [`IssueTracker`] for the current set of *active* issues.
//! 3. Walks that set, claiming any issue that is not already claimed and
//!    spawning a [`Dispatcher`] task for it — up to a hard concurrency
//!    cap drawn from `agent.max_concurrent_agents` (SPEC §5.3.5).
//! 4. Sleeps until the next tick, with a small randomised jitter to
//!    avoid lockstep behaviour across multiple Symphony deployments
//!    polling the same tracker (SPEC §16.2).
//!
//! ## Reconciliation
//!
//! On every tick — *before* claiming new issues — the loop diffs the
//! current claim ledger against the freshly-fetched `active_set`. Any
//! issue still claimed but absent from the active set has, per SPEC §7,
//! "left active states" since the previous tick (closed, archived,
//! moved to a non-active status, or deleted). The loop reacts by:
//!
//! 1. Stamping a [`ReleaseReason::NoLongerActive`] override onto that
//!    dispatch's bookkeeping handle.
//! 2. Firing the per-dispatch [`CancellationToken`] so the in-flight
//!    `Dispatcher::dispatch` future tears down promptly.
//!
//! The dispatch task still resolves to whatever [`ReleaseReason`] the
//! dispatcher returned (typically `Canceled`), but on reap the loop
//! prefers the override — preserving the *cause* (the issue went
//! inactive) rather than the *proximate effect* (we cancelled).
//!
//! ## What this module deliberately does NOT do
//!
//! - **Retry queue.** A failed dispatch is released as
//!   [`ReleaseReason::Completed`] in this iteration. Exponential-backoff
//!   retries land in their own checklist item.
//! - **Agent dispatch protocol.** The [`Dispatcher`] trait is
//!   intentionally minimal — `dispatch(issue) -> ReleaseReason` — so
//!   this module can be tested without pulling in the agent-runner or
//!   workspace crates. The `symphony-cli` composition root will provide
//!   a real implementation that drives an `AgentRunner` against a
//!   `WorkspaceManager`.
//!
//! ## Concurrency model
//!
//! The loop runs on a single tokio task. Per-issue work runs in tasks
//! tracked in a [`JoinSet`]. The [`StateMachine`] is owned by the loop
//! and never escapes it; spawned dispatch tasks return their result
//! (the [`IssueId`] plus a [`ReleaseReason`]) and the loop applies the
//! release. This keeps the single-authority invariant from SPEC §7
//! mechanically true: only the loop ever mutates the claim ledger.

use std::collections::{HashMap, HashSet};
use std::sync::Arc;
use std::time::{Duration, Instant};

use async_trait::async_trait;
use tokio::task::JoinSet;
use tokio_util::sync::CancellationToken;
use tracing::{debug, info, warn};

use crate::state_machine::{ReleaseReason, StateMachine};
use crate::tracker::{Issue, IssueId};
use crate::tracker_trait::{IssueTracker, TrackerError};

/// Pluggable handler invoked by the poll loop for each newly-claimed issue.
///
/// The composition root (`symphony-cli`) wires this up to drive an
/// `AgentRunner` against a `WorkspaceManager`. Tests and snapshot tools
/// substitute simpler implementations — record-only, scripted-failure,
/// or manual-completion via channels.
///
/// ## Contract
///
/// - The returned [`ReleaseReason`] is what the poll loop will record
///   on the state machine's `release` call. Callers decide the reason:
///   `Completed` for normal exit, `Canceled` for cooperative shutdown,
///   etc. The poll loop does not second-guess it.
/// - The implementation MUST honour the supplied [`CancellationToken`]:
///   when it fires, drop in-flight work and return promptly. The loop
///   uses this for SIGINT-style shutdown.
/// - The implementation MUST NOT call back into the state machine. That
///   would violate the single-authority invariant; the loop does all
///   ledger mutation.
#[async_trait]
pub trait Dispatcher: Send + Sync + 'static {
    /// Run one full agent session for `issue`. Resolves when the
    /// session ends — successfully, in error, or because `cancel`
    /// fired.
    async fn dispatch(&self, issue: Issue, cancel: CancellationToken) -> ReleaseReason;
}

/// Knobs that control the poll loop's tick cadence and concurrency.
#[derive(Debug, Clone)]
pub struct PollLoopConfig {
    /// Mean wall-clock interval between successive ticks. The actual
    /// sleep is `interval ± (interval * jitter_ratio)`.
    pub interval: Duration,
    /// Fraction of `interval` to use as jitter half-width. `0.0`
    /// disables jitter; `0.1` means each sleep is in
    /// `[0.9 * interval, 1.1 * interval]`. Values are clamped to
    /// `[0.0, 1.0)` to keep the next tick strictly in the future.
    pub jitter_ratio: f64,
    /// Hard cap on simultaneously-claimed issues. Mirrors
    /// `agent.max_concurrent_agents` from `WORKFLOW.md`.
    pub max_concurrent: usize,
}

impl PollLoopConfig {
    /// Convenience for SPEC §5.3 defaults: 30 s interval, ±10 % jitter,
    /// 10-way concurrency. Tests typically override `interval`.
    pub fn defaults() -> Self {
        Self {
            interval: Duration::from_secs(30),
            jitter_ratio: 0.1,
            max_concurrent: 10,
        }
    }
}

/// Per-tick observability summary returned by [`PollLoop::tick`].
///
/// The fields are deliberately small integers rather than `Vec<...>`
/// so callers (today: tests; tomorrow: the Phase-8 event bus) can log
/// or assert on them cheaply without snapshotting issue payloads.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct TickReport {
    /// Issues newly claimed this tick.
    pub claimed: usize,
    /// Dispatch tasks that finished and were released this tick.
    pub released: usize,
    /// Active issues observed this tick (regardless of claim state).
    pub observed: usize,
    /// Issues skipped because the concurrency cap was already reached.
    pub deferred_capacity: usize,
    /// Claims that left the active set this tick. Their dispatches were
    /// cancelled and will be reaped on a subsequent tick (or in the
    /// `run` drain) with [`ReleaseReason::NoLongerActive`].
    pub reconciled: usize,
    /// Release reasons recorded this tick, in reap order. Lets callers
    /// (tests today; the Phase-8 event bus tomorrow) observe the
    /// reconciliation override without re-hooking the dispatcher.
    pub release_reasons: Vec<(IssueId, ReleaseReason)>,
}

/// Per-issue bookkeeping for an in-flight dispatch.
///
/// Lets the loop signal cancellation to a specific dispatch task and
/// override the [`ReleaseReason`] that will eventually be recorded —
/// reconciliation needs the latter so the ledger reflects *why* a claim
/// ended (the issue went inactive) rather than *how* (we cancelled).
struct DispatchHandle {
    /// Child cancellation token threaded into the dispatcher future.
    cancel: CancellationToken,
    /// If `Some`, the loop will use this reason instead of the one the
    /// dispatcher returned when reaping the task. Set by the
    /// reconciliation pass; left `None` for a normal completion.
    override_reason: Option<ReleaseReason>,
}

/// The orchestrator's poll loop.
///
/// One instance per running daemon. Holds the [`StateMachine`] (owned,
/// not shared) and a [`JoinSet`] of in-flight dispatch tasks. Step the
/// loop with [`PollLoop::tick`] for unit tests, or run it to completion
/// with [`PollLoop::run`] in production.
pub struct PollLoop {
    tracker: Arc<dyn IssueTracker>,
    dispatcher: Arc<dyn Dispatcher>,
    config: PollLoopConfig,
    state: StateMachine,
    in_flight: JoinSet<(IssueId, ReleaseReason)>,
    /// Per-issue dispatch bookkeeping. Keyed identically to the
    /// [`StateMachine`] claim ledger; entries are inserted on dispatch
    /// and removed on reap. Lets reconciliation cancel a specific run
    /// and override its release reason.
    handles: HashMap<IssueId, DispatchHandle>,
    /// Monotonic counter used to derive jitter without pulling in a
    /// random-number generator. See `jittered_sleep_for`.
    tick_seq: u64,
}

impl PollLoop {
    /// Construct a fresh poll loop. The `state` ledger starts empty.
    pub fn new(
        tracker: Arc<dyn IssueTracker>,
        dispatcher: Arc<dyn Dispatcher>,
        config: PollLoopConfig,
    ) -> Self {
        Self {
            tracker,
            dispatcher,
            config,
            state: StateMachine::new(),
            in_flight: JoinSet::new(),
            handles: HashMap::new(),
            tick_seq: 0,
        }
    }

    /// Read-only access to the claim ledger. Used by snapshot APIs and
    /// by tests that want to assert on the currently-running set.
    pub fn state(&self) -> &StateMachine {
        &self.state
    }

    /// Run a single tick: reap finished dispatches, fetch active issues,
    /// claim and dispatch up to the concurrency cap. Returns a
    /// [`TickReport`] describing what happened, or a [`TrackerError`]
    /// if the tracker call failed (the loop continues running on the
    /// next tick — this method does not retry internally).
    ///
    /// Tests drive the loop through this entry point because it does
    /// no waiting and no spawning of the timer task; production code
    /// uses [`PollLoop::run`] which calls `tick` on each interval.
    pub async fn tick(&mut self, cancel: &CancellationToken) -> Result<TickReport, TrackerError> {
        self.tick_seq = self.tick_seq.wrapping_add(1);

        // 1. Reap completed dispatches before fetching, so the
        //    concurrency budget reflects the freshest possible state.
        // 2. Ask the tracker. The loop survives transport hiccups —
        //    return the error to the caller (which logs and continues).
        let release_reasons = self.reap_finished();
        let released = release_reasons.len();
        let active = self.tracker.fetch_active().await?;

        // 3. Reconciliation: any claim whose issue is no longer in
        //    `active` should be torn down. We mark the override first
        //    (so the eventual reap records `NoLongerActive`) and then
        //    fire the cancel token. The dispatch task may resolve this
        //    tick or a later one; either way the ledger entry stays
        //    around until the spawned future returns.
        let active_ids: HashSet<&IssueId> = active.iter().map(|i| &i.id).collect();
        let mut reconciled = 0usize;
        let stale: Vec<IssueId> = self
            .handles
            .keys()
            .filter(|id| !active_ids.contains(*id))
            .cloned()
            .collect();
        for id in &stale {
            if let Some(handle) = self.handles.get_mut(id)
                && handle.override_reason.is_none()
            {
                handle.override_reason = Some(ReleaseReason::NoLongerActive);
                handle.cancel.cancel();
                reconciled += 1;
                debug!(issue = %id, "reconciliation: issue left active set; cancelling dispatch");
            }
        }

        let mut report = TickReport {
            released,
            observed: active.len(),
            reconciled,
            release_reasons,
            ..TickReport::default()
        };

        // 4. Claim and dispatch up to the cap.
        for issue in active {
            if self.state.get(&issue.id).is_some() {
                // Already claimed: skip silently. Reconciliation will
                // handle the case where it should be released.
                continue;
            }
            if self.state.running_len() >= self.config.max_concurrent {
                // Bounded concurrency. Bookkeep how many we punted on
                // for observability, then fall through to the next
                // tick — the order of `active` is tracker-defined and
                // we trust the adapter's prioritisation.
                report.deferred_capacity += 1;
                continue;
            }
            // claim() is fallible only if the issue is already in the
            // ledger, which we just checked. Treat any error here as a
            // logic bug, not a runtime condition.
            if let Err(err) = self.state.claim(issue.id.clone()) {
                warn!(error = %err, identifier = %issue.identifier, "unexpected claim failure; skipping");
                continue;
            }
            self.spawn_dispatch(issue, cancel.child_token());
            report.claimed += 1;
        }

        Ok(report)
    }

    /// Drive the loop forever until `cancel` fires. On cancellation,
    /// stops dispatching new issues, waits for in-flight tasks to
    /// drain (they receive child tokens that fire on the same
    /// signal), and returns once the join set is empty.
    pub async fn run(mut self, cancel: CancellationToken) {
        info!(
            interval_ms = self.config.interval.as_millis() as u64,
            max_concurrent = self.config.max_concurrent,
            "poll loop starting"
        );
        loop {
            tokio::select! {
                biased;
                _ = cancel.cancelled() => {
                    info!("poll loop cancellation requested; draining");
                    break;
                }
                result = self.tick(&cancel) => {
                    if let Err(err) = result {
                        warn!(error = %err, "tick failed; continuing");
                    }
                }
            }
            // Sleep with jitter, but wake immediately on cancellation.
            let nap = jittered_sleep_for(
                self.config.interval,
                self.config.jitter_ratio,
                self.tick_seq,
            );
            tokio::select! {
                biased;
                _ = cancel.cancelled() => {
                    info!("poll loop cancellation requested mid-sleep; draining");
                    break;
                }
                _ = tokio::time::sleep(nap) => {}
            }
        }
        // Drain in-flight: child tokens already fired with the parent.
        while let Some(joined) = self.in_flight.join_next().await {
            match joined {
                Ok((id, dispatcher_reason)) => {
                    // Honour any pending reconciliation override
                    // exactly as `reap_finished` does — a reconciled
                    // run that races with shutdown should still record
                    // `NoLongerActive`, not the dispatcher's reason.
                    let reason = self
                        .handles
                        .remove(&id)
                        .and_then(|h| h.override_reason)
                        .unwrap_or(dispatcher_reason);
                    let _ = self.state.release(id.clone(), reason);
                    debug!(issue = %id, ?reason, "drained dispatch on shutdown");
                }
                Err(err) => warn!(error = %err, "dispatch task panicked during drain"),
            }
        }
        info!("poll loop drained");
    }

    /// Reap completed dispatches without blocking. Returns the
    /// (id, reason) pairs that were released so the caller can fold
    /// them into a [`TickReport`].
    fn reap_finished(&mut self) -> Vec<(IssueId, ReleaseReason)> {
        let mut releases = Vec::new();
        while let Some(res) = self.in_flight.try_join_next() {
            match res {
                Ok((id, dispatcher_reason)) => {
                    // Reconciliation may have stamped an override on
                    // this issue's handle (e.g. `NoLongerActive` when
                    // the tracker dropped it from the active set); if
                    // present, the override wins so the ledger records
                    // *why* the run ended rather than just the
                    // proximate effect (cancellation).
                    let reason = self
                        .handles
                        .remove(&id)
                        .and_then(|h| h.override_reason)
                        .unwrap_or(dispatcher_reason);
                    if self.state.release(id.clone(), reason).is_ok() {
                        releases.push((id, reason));
                    } else {
                        // Releasing an absent issue is a logic bug —
                        // nothing else removes from the ledger except
                        // this method. Log loudly and move on rather
                        // than poisoning the loop.
                        warn!(issue = %id, "dispatch resolved but claim was absent");
                    }
                }
                Err(err) => warn!(error = %err, "dispatch task panicked"),
            }
        }
        releases
    }

    fn spawn_dispatch(&mut self, issue: Issue, cancel: CancellationToken) {
        let dispatcher = Arc::clone(&self.dispatcher);
        let id = issue.id.clone();
        // Record the handle BEFORE spawning so a fast-finishing task
        // can never out-race us — `try_join_next` is only consulted
        // from `reap_finished` on the loop task itself.
        self.handles.insert(
            id.clone(),
            DispatchHandle {
                cancel: cancel.clone(),
                override_reason: None,
            },
        );
        self.in_flight.spawn(async move {
            let reason = dispatcher.dispatch(issue, cancel).await;
            (id, reason)
        });
    }
}

/// Compute the next sleep duration with `±jitter_ratio` half-width.
///
/// We deliberately avoid pulling in a random-number generator just for
/// jitter. The loop's `tick_seq` is a monotonic counter, hashed
/// together with the wall-clock nanoseconds to derive a pseudo-random
/// offset in `[-1.0, 1.0)`. This is good enough for jitter (the goal
/// is decorrelation across deployments, not cryptographic randomness)
/// and keeps the dependency budget tight.
fn jittered_sleep_for(interval: Duration, jitter_ratio: f64, tick_seq: u64) -> Duration {
    if interval.is_zero() {
        return Duration::ZERO;
    }
    let clamped = jitter_ratio.clamp(0.0, 0.999);
    if clamped == 0.0 {
        return interval;
    }
    let offset_unit = pseudo_unit(tick_seq); // [-1.0, 1.0)
    let factor = 1.0 + clamped * offset_unit;
    let nanos = interval.as_secs_f64() * factor;
    Duration::from_secs_f64(nanos.max(0.0))
}

/// Map (`tick_seq`, monotonic-clock-nanos) to a value in `[-1.0, 1.0)`.
/// Public-ish for tests but not exported.
fn pseudo_unit(tick_seq: u64) -> f64 {
    // SplitMix64-style mixer. We don't need to track the seed across
    // calls because we mix in the wall-clock nanos each time.
    let now = Instant::now().elapsed().as_nanos() as u64;
    let mut x = tick_seq.wrapping_mul(0x9E37_79B9_7F4A_7C15) ^ now;
    x = (x ^ (x >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
    x = (x ^ (x >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
    x ^= x >> 31;
    // Keep the top 53 bits → fraction in [0, 1) → shift to [-1, 1).
    let frac = (x >> 11) as f64 / (1u64 << 53) as f64;
    frac * 2.0 - 1.0
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tracker::{Issue, IssueState};
    use std::sync::Mutex;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use tokio::sync::Notify;

    /// Tracker that returns a fixed snapshot — minimal surface for
    /// driving `tick` in unit tests without pulling `MockTracker`'s
    /// scripting machinery in.
    struct StaticTracker {
        active: Mutex<Vec<Issue>>,
    }

    impl StaticTracker {
        fn new(active: Vec<Issue>) -> Self {
            Self {
                active: Mutex::new(active),
            }
        }
        fn replace(&self, active: Vec<Issue>) {
            *self.active.lock().unwrap() = active;
        }
    }

    #[async_trait]
    impl IssueTracker for StaticTracker {
        async fn fetch_active(&self) -> crate::tracker_trait::TrackerResult<Vec<Issue>> {
            Ok(self.active.lock().unwrap().clone())
        }
        async fn fetch_state(
            &self,
            ids: &[IssueId],
        ) -> crate::tracker_trait::TrackerResult<Vec<Issue>> {
            Ok(ids
                .iter()
                .map(|id| Issue::minimal(id.as_str(), id.as_str(), "stub", "Todo"))
                .collect())
        }
        async fn fetch_terminal_recent(
            &self,
            _terminal_states: &[IssueState],
        ) -> crate::tracker_trait::TrackerResult<Vec<Issue>> {
            Ok(Vec::new())
        }
    }

    /// Dispatcher that records each invocation and waits on a Notify
    /// before completing. Lets a test drive concurrency state
    /// deterministically.
    struct GatedDispatcher {
        started: AtomicUsize,
        gate: Arc<Notify>,
    }

    impl GatedDispatcher {
        fn new(gate: Arc<Notify>) -> Self {
            Self {
                started: AtomicUsize::new(0),
                gate,
            }
        }
    }

    #[async_trait]
    impl Dispatcher for GatedDispatcher {
        async fn dispatch(&self, _issue: Issue, _cancel: CancellationToken) -> ReleaseReason {
            self.started.fetch_add(1, Ordering::SeqCst);
            self.gate.notified().await;
            ReleaseReason::Completed
        }
    }

    /// Dispatcher that returns immediately with a configurable reason.
    struct InstantDispatcher(ReleaseReason);

    #[async_trait]
    impl Dispatcher for InstantDispatcher {
        async fn dispatch(&self, _issue: Issue, _cancel: CancellationToken) -> ReleaseReason {
            self.0
        }
    }

    fn issues(n: usize) -> Vec<Issue> {
        (0..n)
            .map(|i| Issue::minimal(format!("id-{i}"), format!("ENG-{i}"), "title", "Todo"))
            .collect()
    }

    #[tokio::test]
    async fn tick_claims_up_to_concurrency_cap() {
        let tracker = Arc::new(StaticTracker::new(issues(5)));
        let gate = Arc::new(Notify::new());
        let dispatcher = Arc::new(GatedDispatcher::new(Arc::clone(&gate)));
        let cfg = PollLoopConfig {
            interval: Duration::from_millis(1),
            jitter_ratio: 0.0,
            max_concurrent: 2,
        };
        let mut loop_ = PollLoop::new(tracker, Arc::clone(&dispatcher) as Arc<dyn Dispatcher>, cfg);
        let cancel = CancellationToken::new();

        let report = loop_.tick(&cancel).await.unwrap();
        assert_eq!(report.observed, 5);
        assert_eq!(report.claimed, 2);
        assert_eq!(report.deferred_capacity, 3);
        assert_eq!(loop_.state().running_len(), 2);

        // Release one task and tick again — one more should claim.
        gate.notify_one();
        // Yield until the released task has actually completed.
        for _ in 0..50 {
            tokio::task::yield_now().await;
            if dispatcher.started.load(Ordering::SeqCst) >= 2 {
                break;
            }
        }
        // Wait briefly for the gated future to actually resolve.
        tokio::time::sleep(Duration::from_millis(5)).await;

        let report = loop_.tick(&cancel).await.unwrap();
        assert_eq!(report.released, 1);
        assert_eq!(report.claimed, 1);
        assert_eq!(loop_.state().running_len(), 2);

        // Drain the rest so the test doesn't leak tasks.
        gate.notify_waiters();
        cancel.cancel();
    }

    #[tokio::test]
    async fn tick_skips_already_claimed() {
        let tracker = Arc::new(StaticTracker::new(issues(2)));
        let gate = Arc::new(Notify::new());
        let dispatcher = Arc::new(GatedDispatcher::new(Arc::clone(&gate)));
        let cfg = PollLoopConfig {
            interval: Duration::from_millis(1),
            jitter_ratio: 0.0,
            max_concurrent: 10,
        };
        let mut loop_ = PollLoop::new(tracker, Arc::clone(&dispatcher) as Arc<dyn Dispatcher>, cfg);
        let cancel = CancellationToken::new();

        let r1 = loop_.tick(&cancel).await.unwrap();
        assert_eq!(r1.claimed, 2);
        // Same tracker output → claims must NOT double-fire.
        let r2 = loop_.tick(&cancel).await.unwrap();
        assert_eq!(r2.claimed, 0);
        assert_eq!(r2.observed, 2);

        gate.notify_waiters();
        cancel.cancel();
    }

    #[tokio::test]
    async fn tick_propagates_tracker_errors_without_panicking() {
        struct Failing;
        #[async_trait]
        impl IssueTracker for Failing {
            async fn fetch_active(&self) -> crate::tracker_trait::TrackerResult<Vec<Issue>> {
                Err(TrackerError::Transport("nope".into()))
            }
            async fn fetch_state(
                &self,
                _ids: &[IssueId],
            ) -> crate::tracker_trait::TrackerResult<Vec<Issue>> {
                Ok(Vec::new())
            }
            async fn fetch_terminal_recent(
                &self,
                _t: &[IssueState],
            ) -> crate::tracker_trait::TrackerResult<Vec<Issue>> {
                Ok(Vec::new())
            }
        }
        let dispatcher = Arc::new(InstantDispatcher(ReleaseReason::Completed));
        let mut loop_ = PollLoop::new(
            Arc::new(Failing),
            dispatcher as Arc<dyn Dispatcher>,
            PollLoopConfig::defaults(),
        );
        let cancel = CancellationToken::new();
        let err = loop_.tick(&cancel).await.unwrap_err();
        assert!(matches!(err, TrackerError::Transport(_)));
        assert_eq!(loop_.state().running_len(), 0);
    }

    #[tokio::test]
    async fn run_drains_in_flight_on_cancel() {
        let tracker = Arc::new(StaticTracker::new(issues(3)));
        let gate = Arc::new(Notify::new());
        let dispatcher = Arc::new(GatedDispatcher::new(Arc::clone(&gate)));
        let cfg = PollLoopConfig {
            interval: Duration::from_millis(5),
            jitter_ratio: 0.0,
            max_concurrent: 3,
        };
        let cancel = CancellationToken::new();
        let cancel_clone = cancel.clone();
        let dispatcher_clone = Arc::clone(&dispatcher);
        let tracker_clone = Arc::clone(&tracker);
        let handle = tokio::spawn(async move {
            let loop_ = PollLoop::new(tracker_clone, dispatcher_clone as Arc<dyn Dispatcher>, cfg);
            loop_.run(cancel_clone).await;
        });

        // Wait until all three dispatches start.
        for _ in 0..200 {
            if dispatcher.started.load(Ordering::SeqCst) == 3 {
                break;
            }
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
        assert_eq!(dispatcher.started.load(Ordering::SeqCst), 3);

        // Release the gates so dispatch tasks can finish, then cancel.
        gate.notify_waiters();
        cancel.cancel();

        // run() should drain and return.
        tokio::time::timeout(Duration::from_secs(2), handle)
            .await
            .expect("run drained")
            .expect("no panic");

        // Suppress unused warning on tracker (the loop owns its own clone).
        tracker.replace(Vec::new());
    }

    /// Dispatcher that resolves only when its cancellation token fires,
    /// returning [`ReleaseReason::Canceled`]. Used to verify that
    /// reconciliation actually cancels in-flight runs and that the
    /// recorded release reason wins out over what the dispatcher
    /// returned (`NoLongerActive` over `Canceled`).
    struct CancellableDispatcher {
        started: AtomicUsize,
    }

    impl CancellableDispatcher {
        fn new() -> Self {
            Self {
                started: AtomicUsize::new(0),
            }
        }
    }

    #[async_trait]
    impl Dispatcher for CancellableDispatcher {
        async fn dispatch(&self, _issue: Issue, cancel: CancellationToken) -> ReleaseReason {
            self.started.fetch_add(1, Ordering::SeqCst);
            cancel.cancelled().await;
            ReleaseReason::Canceled
        }
    }

    #[tokio::test]
    async fn reconciliation_cancels_runs_that_left_active_set() {
        let tracker = Arc::new(StaticTracker::new(issues(3)));
        let dispatcher = Arc::new(CancellableDispatcher::new());
        let cfg = PollLoopConfig {
            interval: Duration::from_millis(1),
            jitter_ratio: 0.0,
            max_concurrent: 10,
        };
        let mut loop_ = PollLoop::new(
            Arc::clone(&tracker) as Arc<dyn IssueTracker>,
            Arc::clone(&dispatcher) as Arc<dyn Dispatcher>,
            cfg,
        );
        let cancel = CancellationToken::new();

        // First tick: claim all three.
        let r1 = loop_.tick(&cancel).await.unwrap();
        assert_eq!(r1.claimed, 3);
        assert_eq!(r1.reconciled, 0);
        assert_eq!(loop_.state().running_len(), 3);

        // Wait until each dispatch has actually started awaiting cancel.
        for _ in 0..200 {
            if dispatcher.started.load(Ordering::SeqCst) == 3 {
                break;
            }
            tokio::time::sleep(Duration::from_millis(2)).await;
        }
        assert_eq!(dispatcher.started.load(Ordering::SeqCst), 3);

        // Tracker now drops `id-2` from the active set. Next tick must
        // detect the orphan claim, override the release reason to
        // `NoLongerActive`, and fire the per-run cancel token.
        tracker.replace(issues(2));
        let r2 = loop_.tick(&cancel).await.unwrap();
        assert_eq!(r2.observed, 2);
        assert_eq!(r2.reconciled, 1);
        // The cancelled task may not have been reaped yet on this tick
        // (the dispatch future has to round-trip through tokio).
        // Ledger still holds it; it'll release on the next tick.

        // Give the cancelled future a chance to resolve.
        for _ in 0..200 {
            if loop_.state().running_len() < 3 {
                break;
            }
            tokio::time::sleep(Duration::from_millis(2)).await;
        }

        let r3 = loop_.tick(&cancel).await.unwrap();
        assert_eq!(r3.released, 1);
        // The override must win: dispatcher returned `Canceled` but
        // reconciliation overrode to `NoLongerActive`.
        assert_eq!(
            r3.release_reasons,
            vec![(IssueId::new("id-2"), ReleaseReason::NoLongerActive)]
        );
        assert_eq!(loop_.state().running_len(), 2);
        assert!(loop_.state().get(&IssueId::new("id-2")).is_none());

        cancel.cancel();
    }

    #[tokio::test]
    async fn reconciliation_is_idempotent_across_ticks() {
        // If a reconciled run hasn't reaped yet, a second tick that
        // still doesn't see the issue must not re-fire the cancel or
        // double-count the reconciliation.
        let tracker = Arc::new(StaticTracker::new(issues(2)));
        let dispatcher = Arc::new(CancellableDispatcher::new());
        let cfg = PollLoopConfig {
            interval: Duration::from_millis(1),
            jitter_ratio: 0.0,
            max_concurrent: 10,
        };
        let mut loop_ = PollLoop::new(
            Arc::clone(&tracker) as Arc<dyn IssueTracker>,
            Arc::clone(&dispatcher) as Arc<dyn Dispatcher>,
            cfg,
        );
        let cancel = CancellationToken::new();

        loop_.tick(&cancel).await.unwrap();
        for _ in 0..200 {
            if dispatcher.started.load(Ordering::SeqCst) == 2 {
                break;
            }
            tokio::time::sleep(Duration::from_millis(2)).await;
        }

        // Empty the active set. First tick reconciles both. Note: we
        // do NOT yield long enough between ticks for reaps; the
        // override must remain set.
        tracker.replace(Vec::new());
        let r2 = loop_.tick(&cancel).await.unwrap();
        assert_eq!(r2.reconciled, 2);

        // Immediate second tick before the cancelled futures get a
        // chance to be reaped: still no active issues; the handles are
        // still present with override already set; reconciled must be 0.
        let r3 = loop_.tick(&cancel).await.unwrap();
        assert_eq!(r3.reconciled, 0);

        cancel.cancel();
    }

    #[test]
    fn jitter_zero_returns_exact_interval() {
        let d = jittered_sleep_for(Duration::from_secs(10), 0.0, 1);
        assert_eq!(d, Duration::from_secs(10));
    }

    #[test]
    fn jitter_stays_within_bounds() {
        let interval = Duration::from_secs(10);
        let ratio = 0.2;
        for seq in 0..1000 {
            let d = jittered_sleep_for(interval, ratio, seq);
            assert!(
                d.as_secs_f64() >= 8.0 && d.as_secs_f64() <= 12.0,
                "seq={seq} d={d:?}"
            );
        }
    }

    #[test]
    fn jitter_is_clamped_to_keep_sleep_strictly_positive() {
        // Even an absurd ratio must not produce a zero-or-negative
        // sleep — the loop would busy-spin.
        let d = jittered_sleep_for(Duration::from_millis(100), 5.0, 7);
        assert!(d > Duration::ZERO);
    }
}
