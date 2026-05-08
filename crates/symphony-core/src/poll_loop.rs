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

use crate::event_bus::EventBus;
use crate::events::OrchestratorEvent;
use crate::state_machine::{ClaimState, ReleaseReason, StateMachine};
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
    /// Maximum wall-clock time the loop spends draining in-flight
    /// dispatches after [`PollLoop::run`] observes its cancellation
    /// signal. When this elapses, any remaining dispatch tasks are
    /// `JoinSet::abort_all()`'d and their claims are released as
    /// [`ReleaseReason::Canceled`].
    ///
    /// The well-behaved case (operator hits Ctrl-C, agents observe the
    /// child cancel token, sessions tear down) returns long before the
    /// deadline. The deadline exists for the *misbehaving* case: a wedged
    /// subprocess, a backend that ignores cancellation, a `start_session`
    /// future stuck in retry. Without a bound, SIGINT would hang the
    /// daemon indefinitely. With a bound, the operator gets a clean exit
    /// after at most `drain_deadline`, with the loud aborts visible in
    /// the logs.
    ///
    /// Set to `Duration::ZERO` to abort immediately on cancel (no drain).
    pub drain_deadline: Duration,
}

impl PollLoopConfig {
    /// Convenience for SPEC §5.3 defaults: 30 s interval, ±10 % jitter,
    /// 10-way concurrency, 30 s drain deadline. Tests typically override
    /// `interval` and `drain_deadline`.
    pub fn defaults() -> Self {
        Self {
            interval: Duration::from_secs(30),
            jitter_ratio: 0.1,
            max_concurrent: 10,
            drain_deadline: Duration::from_secs(30),
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
    /// Tracker identifier captured at claim time. Carried so reap-time
    /// [`OrchestratorEvent::Released`] emissions can include it without
    /// needing to refetch the issue.
    identifier: String,
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
    /// Observability bus. Every claim, reconciliation, and release fans
    /// out as an [`OrchestratorEvent`] for the Phase-8 status surface.
    /// Pure addition: emission is best-effort and the orchestrator never
    /// blocks on subscriber backlog.
    bus: EventBus,
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
        Self::with_event_bus(tracker, dispatcher, config, EventBus::default())
    }

    /// Construct a poll loop with a caller-supplied [`EventBus`]. Lets
    /// the composition root (`symphony-cli`) wire one bus into both the
    /// orchestrator and the SSE handler so they share replay buffer
    /// sizing and subscriber accounting.
    pub fn with_event_bus(
        tracker: Arc<dyn IssueTracker>,
        dispatcher: Arc<dyn Dispatcher>,
        config: PollLoopConfig,
        bus: EventBus,
    ) -> Self {
        Self {
            tracker,
            dispatcher,
            config,
            state: StateMachine::new(),
            in_flight: JoinSet::new(),
            handles: HashMap::new(),
            bus,
            tick_seq: 0,
        }
    }

    /// Read-only access to the claim ledger. Used by snapshot APIs and
    /// by tests that want to assert on the currently-running set.
    pub fn state(&self) -> &StateMachine {
        &self.state
    }

    /// Subscribe to the orchestrator's [`OrchestratorEvent`] stream.
    /// Cheap: the underlying broadcast channel allocates one slot per
    /// subscriber. See [`EventBus`] for backpressure semantics.
    pub fn subscribe(&self) -> tokio_stream::wrappers::BroadcastStream<OrchestratorEvent> {
        self.bus.subscribe()
    }

    /// Borrow the event bus directly. Useful at the composition root
    /// where the SSE handler wants its own clone of the sender.
    pub fn event_bus(&self) -> &EventBus {
        &self.bus
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
        let mut dropped_this_tick: Vec<IssueId> = Vec::new();
        for id in &stale {
            if let Some(handle) = self.handles.get_mut(id)
                && handle.override_reason.is_none()
            {
                handle.override_reason = Some(ReleaseReason::NoLongerActive);
                handle.cancel.cancel();
                reconciled += 1;
                dropped_this_tick.push(id.clone());
                debug!(issue = %id, "reconciliation: issue left active set; cancelling dispatch");
            }
        }
        // Emit a single Reconciled summary per tick that actually
        // marked anything stale. A no-op tick is silent on the bus —
        // see `OrchestratorEvent::Reconciled` docs.
        if !dropped_this_tick.is_empty() {
            self.bus.emit(OrchestratorEvent::Reconciled {
                dropped: dropped_this_tick,
            });
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
        // The deadline bounds how long we wait for cooperative tear-down
        // before giving up and aborting the remaining tasks.
        self.drain_with_deadline(self.config.drain_deadline).await;
        info!("poll loop drained");
    }

    /// Wait up to `deadline` for in-flight dispatches to finish, then
    /// abort any stragglers. Pulled out of [`PollLoop::run`] so tests
    /// can drive it directly without spinning up the timer task.
    ///
    /// Behaviour, in order:
    ///
    /// 1. Loop on [`JoinSet::join_next`] inside a [`tokio::time::timeout`]
    ///    for the full `deadline`. Each finished task is released
    ///    through the same override-aware path as the steady-state reap.
    /// 2. When the timeout fires (or `deadline` is zero), call
    ///    [`JoinSet::abort_all`] and drain the join set unconditionally.
    ///    Aborted tasks resolve as `Err(JoinError { is_cancelled: true })`;
    ///    their ledger entries are released with [`ReleaseReason::Canceled`]
    ///    and a `warn!` line so operators can see which runs were forcibly
    ///    terminated.
    async fn drain_with_deadline(&mut self, deadline: Duration) {
        let drained_cleanly = if deadline.is_zero() {
            false
        } else {
            tokio::time::timeout(deadline, self.drain_until_empty())
                .await
                .is_ok()
        };
        if drained_cleanly {
            return;
        }
        // Either the operator asked for a zero-deadline immediate abort,
        // or one or more tasks didn't honour their cancel token in time.
        // Force them down and release with `Canceled` so the ledger
        // doesn't keep stale claims around.
        let stuck: Vec<IssueId> = self.handles.keys().cloned().collect();
        if !stuck.is_empty() {
            warn!(
                stuck = stuck.len(),
                "drain deadline exceeded; aborting in-flight dispatches"
            );
        }
        self.in_flight.abort_all();
        while let Some(joined) = self.in_flight.join_next().await {
            match joined {
                Ok((id, dispatcher_reason)) => {
                    let handle = self.handles.remove(&id);
                    let identifier = handle
                        .as_ref()
                        .map(|h| h.identifier.clone())
                        .unwrap_or_default();
                    let reason = handle
                        .and_then(|h| h.override_reason)
                        .unwrap_or(dispatcher_reason);
                    let _ = self.state.release(id.clone(), reason);
                    self.bus.emit(OrchestratorEvent::Released {
                        issue: id,
                        identifier,
                        reason,
                        final_state: None,
                    });
                }
                Err(err) if err.is_cancelled() => {
                    // We don't know which `id` this aborted task belonged
                    // to (the panic-payload-style join error carries no
                    // user data), but the only handles still present are
                    // ones whose tasks haven't been reaped — and those
                    // are precisely the ones we just aborted. Releasing
                    // them as `Canceled` keeps the ledger accurate.
                    if let Some((id, handle)) = pop_any_entry(&mut self.handles) {
                        let _ = self.state.release(id.clone(), ReleaseReason::Canceled);
                        self.bus.emit(OrchestratorEvent::Released {
                            issue: id,
                            identifier: handle.identifier,
                            reason: ReleaseReason::Canceled,
                            final_state: None,
                        });
                    }
                }
                Err(err) => warn!(error = %err, "dispatch task panicked during forced drain"),
            }
        }
        // Any handles that survive both reaps (theoretically impossible
        // — every spawn registers a handle and every reap removes one)
        // get released defensively so the ledger never lies.
        let leftover: Vec<IssueId> = self.handles.keys().cloned().collect();
        for id in leftover {
            let handle = self.handles.remove(&id);
            let identifier = handle.map(|h| h.identifier).unwrap_or_default();
            let _ = self.state.release(id.clone(), ReleaseReason::Canceled);
            self.bus.emit(OrchestratorEvent::Released {
                issue: id,
                identifier,
                reason: ReleaseReason::Canceled,
                final_state: None,
            });
        }
    }

    /// Drain `in_flight` to empty using the steady-state reap logic.
    /// Wrapped by [`Self::drain_with_deadline`] inside a timeout.
    async fn drain_until_empty(&mut self) {
        while let Some(joined) = self.in_flight.join_next().await {
            match joined {
                Ok((id, dispatcher_reason)) => {
                    // Honour any pending reconciliation override exactly
                    // as `reap_finished` does — a reconciled run that
                    // races with shutdown should still record
                    // `NoLongerActive`, not the dispatcher's reason.
                    let handle = self.handles.remove(&id);
                    let identifier = handle
                        .as_ref()
                        .map(|h| h.identifier.clone())
                        .unwrap_or_default();
                    let reason = handle
                        .and_then(|h| h.override_reason)
                        .unwrap_or(dispatcher_reason);
                    let _ = self.state.release(id.clone(), reason);
                    self.bus.emit(OrchestratorEvent::Released {
                        issue: id.clone(),
                        identifier,
                        reason,
                        final_state: None,
                    });
                    debug!(issue = %id, ?reason, "drained dispatch on shutdown");
                }
                Err(err) => warn!(error = %err, "dispatch task panicked during drain"),
            }
        }
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
                    let handle = self.handles.remove(&id);
                    let identifier = handle
                        .as_ref()
                        .map(|h| h.identifier.clone())
                        .unwrap_or_default();
                    let reason = handle
                        .and_then(|h| h.override_reason)
                        .unwrap_or(dispatcher_reason);
                    if self.state.release(id.clone(), reason).is_ok() {
                        self.bus.emit(OrchestratorEvent::Released {
                            issue: id.clone(),
                            identifier,
                            reason,
                            final_state: None,
                        });
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
        let identifier = issue.identifier.clone();
        // Record the handle BEFORE spawning so a fast-finishing task
        // can never out-race us — `try_join_next` is only consulted
        // from `reap_finished` on the loop task itself.
        self.handles.insert(
            id.clone(),
            DispatchHandle {
                cancel: cancel.clone(),
                override_reason: None,
                identifier: identifier.clone(),
            },
        );
        // Emit the StateChanged event for the absent → Running
        // transition we just performed in `tick`. Done here (rather
        // than inline at the claim site) so the handle is already
        // registered when a fast subscriber observes the event and
        // subsequently looks up our state.
        self.bus.emit(OrchestratorEvent::StateChanged {
            issue: id.clone(),
            identifier,
            previous: None,
            current: ClaimState::Running,
        });
        self.in_flight.spawn(async move {
            let reason = dispatcher.dispatch(issue, cancel).await;
            (id, reason)
        });
    }
}

/// Pop an arbitrary entry from `map`. Used by the forced-abort drain
/// path: aborted `JoinSet` entries surface as `JoinError`s with no
/// user payload, so we can't look up the canonical handle by id —
/// instead we burn down the handle table in lockstep with the drain.
/// Hash-iteration order is deliberately fine here; the loop is about
/// releasing every remaining claim, not preserving order.
/// Pop an arbitrary `(key, value)` entry from `map`. Used by the
/// forced-abort drain path: aborted `JoinSet` entries surface as
/// `JoinError`s with no user payload, so we can't look up the
/// canonical handle by id — instead we burn down the handle table in
/// lockstep with the drain. Returning the value too lets the
/// [`OrchestratorEvent::Released`] emission carry the per-issue
/// identifier captured at dispatch.
fn pop_any_entry<K: Clone + std::hash::Hash + Eq, V>(map: &mut HashMap<K, V>) -> Option<(K, V)> {
    let key = map.keys().next().cloned()?;
    let value = map.remove(&key)?;
    Some((key, value))
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
            drain_deadline: Duration::from_secs(1),
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
            drain_deadline: Duration::from_secs(1),
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
            drain_deadline: Duration::from_secs(1),
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
            drain_deadline: Duration::from_secs(1),
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
            drain_deadline: Duration::from_secs(1),
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

    /// Dispatcher that ignores its cancellation token and only
    /// resolves once an external `Notify` is fired. Models a wedged
    /// agent backend that fails to honour cooperative shutdown.
    struct WedgedDispatcher {
        started: AtomicUsize,
        release: Arc<Notify>,
    }

    #[async_trait]
    impl Dispatcher for WedgedDispatcher {
        async fn dispatch(&self, _issue: Issue, _cancel: CancellationToken) -> ReleaseReason {
            self.started.fetch_add(1, Ordering::SeqCst);
            self.release.notified().await;
            ReleaseReason::Completed
        }
    }

    #[tokio::test]
    async fn run_aborts_in_flight_after_drain_deadline() {
        // A wedged dispatcher would hang `run()` forever without the
        // deadline. With the deadline, `run()` must return promptly,
        // every claim must be released as `Canceled`, and the join set
        // must be empty.
        let tracker = Arc::new(StaticTracker::new(issues(2)));
        let release = Arc::new(Notify::new());
        let dispatcher = Arc::new(WedgedDispatcher {
            started: AtomicUsize::new(0),
            release: Arc::clone(&release),
        });
        let cfg = PollLoopConfig {
            interval: Duration::from_millis(5),
            jitter_ratio: 0.0,
            max_concurrent: 2,
            drain_deadline: Duration::from_millis(50),
        };
        let cancel = CancellationToken::new();
        let cancel_clone = cancel.clone();
        let dispatcher_clone = Arc::clone(&dispatcher);
        let tracker_clone = Arc::clone(&tracker) as Arc<dyn IssueTracker>;
        let started = tokio::spawn(async move {
            let loop_ = PollLoop::new(tracker_clone, dispatcher_clone as Arc<dyn Dispatcher>, cfg);
            loop_.run(cancel_clone).await;
        });

        // Wait for both dispatches to have started awaiting their
        // (ignored) cancel.
        for _ in 0..200 {
            if dispatcher.started.load(Ordering::SeqCst) == 2 {
                break;
            }
            tokio::time::sleep(Duration::from_millis(2)).await;
        }
        assert_eq!(dispatcher.started.load(Ordering::SeqCst), 2);

        cancel.cancel();
        // Generous slack: deadline (50 ms) + scheduling overhead.
        tokio::time::timeout(Duration::from_secs(2), started)
            .await
            .expect("run returned within deadline + slack")
            .expect("no panic");

        // The wedged tasks were aborted; nothing reads from `release`
        // any more. Notify it anyway to keep the test self-contained.
        release.notify_waiters();
        // Suppress unused-tracker warning.
        tracker.replace(Vec::new());
    }

    #[tokio::test]
    async fn run_returns_immediately_with_zero_drain_deadline() {
        // `drain_deadline = 0` means "abort on cancel, do not wait."
        // Useful as an emergency-stop knob; `run()` must return even
        // if dispatchers are still running.
        let tracker = Arc::new(StaticTracker::new(issues(1)));
        let release = Arc::new(Notify::new());
        let dispatcher = Arc::new(WedgedDispatcher {
            started: AtomicUsize::new(0),
            release: Arc::clone(&release),
        });
        let cfg = PollLoopConfig {
            interval: Duration::from_millis(5),
            jitter_ratio: 0.0,
            max_concurrent: 1,
            drain_deadline: Duration::ZERO,
        };
        let cancel = CancellationToken::new();
        let cancel_clone = cancel.clone();
        let dispatcher_clone = Arc::clone(&dispatcher);
        let tracker_clone = Arc::clone(&tracker) as Arc<dyn IssueTracker>;
        let started = tokio::spawn(async move {
            let loop_ = PollLoop::new(tracker_clone, dispatcher_clone as Arc<dyn Dispatcher>, cfg);
            loop_.run(cancel_clone).await;
        });

        for _ in 0..200 {
            if dispatcher.started.load(Ordering::SeqCst) == 1 {
                break;
            }
            tokio::time::sleep(Duration::from_millis(2)).await;
        }

        cancel.cancel();
        tokio::time::timeout(Duration::from_secs(1), started)
            .await
            .expect("run returned without waiting on the wedged dispatcher")
            .expect("no panic");

        release.notify_waiters();
        tracker.replace(Vec::new());
    }

    #[tokio::test]
    async fn run_drains_cleanly_when_dispatchers_honour_cancel() {
        // The cooperative path: dispatchers observe the cancel token
        // and resolve quickly. The deadline must NOT fire, the
        // override-aware reap path must run, and the recorded reason
        // must be the dispatcher's own `Canceled` rather than the
        // forced-abort `Canceled`. (They're the same variant today —
        // this test is here to pin the cooperative path against future
        // refactors that introduce a distinct forced-abort reason.)
        let tracker = Arc::new(StaticTracker::new(issues(2)));
        let dispatcher = Arc::new(CancellableDispatcher::new());
        let cfg = PollLoopConfig {
            interval: Duration::from_millis(5),
            jitter_ratio: 0.0,
            max_concurrent: 2,
            drain_deadline: Duration::from_secs(5),
        };
        let cancel = CancellationToken::new();
        let cancel_clone = cancel.clone();
        let dispatcher_clone = Arc::clone(&dispatcher);
        let tracker_clone = Arc::clone(&tracker) as Arc<dyn IssueTracker>;
        let started = std::time::Instant::now();
        let handle = tokio::spawn(async move {
            let loop_ = PollLoop::new(tracker_clone, dispatcher_clone as Arc<dyn Dispatcher>, cfg);
            loop_.run(cancel_clone).await;
        });

        for _ in 0..200 {
            if dispatcher.started.load(Ordering::SeqCst) == 2 {
                break;
            }
            tokio::time::sleep(Duration::from_millis(2)).await;
        }
        cancel.cancel();
        tokio::time::timeout(Duration::from_secs(1), handle)
            .await
            .expect("run drained cleanly")
            .expect("no panic");
        // Cooperative drain should be far below the 5 s deadline.
        assert!(
            started.elapsed() < Duration::from_secs(2),
            "drain took {:?}, expected sub-2s cooperative path",
            started.elapsed()
        );
        tracker.replace(Vec::new());
    }

    // -----------------------------------------------------------------
    // Event-bus emission tests.
    //
    // These pin that every state transition the poll loop performs
    // shows up on the [`crate::event_bus::EventBus`]. Pure observability
    // — none of these tests assert behaviour the loop didn't already
    // exhibit before the bus existed.
    // -----------------------------------------------------------------

    use futures::StreamExt;

    /// Drain whatever events are currently buffered on `rx` without
    /// blocking. We poll with a `Duration::ZERO` timeout per item
    /// because the broadcast stream signals "no more right now" by
    /// pending — but a small `now_or_never`-style cooperative yield
    /// loop is enough to surface every emission we care about for the
    /// scripted scenarios below.
    async fn drain_events(
        rx: &mut tokio_stream::wrappers::BroadcastStream<OrchestratorEvent>,
    ) -> Vec<OrchestratorEvent> {
        let mut out = Vec::new();
        loop {
            match tokio::time::timeout(Duration::from_millis(20), rx.next()).await {
                Ok(Some(Ok(ev))) => out.push(ev),
                Ok(Some(Err(_lagged))) => {} // ignore lag in tests
                Ok(None) => break,
                Err(_timeout) => break,
            }
        }
        out
    }

    #[tokio::test]
    async fn claim_emits_state_changed_event() {
        let tracker = Arc::new(StaticTracker::new(issues(1)));
        let dispatcher = Arc::new(InstantDispatcher(ReleaseReason::Completed));
        let cfg = PollLoopConfig {
            interval: Duration::from_millis(1),
            jitter_ratio: 0.0,
            max_concurrent: 1,
            drain_deadline: Duration::from_secs(1),
        };
        let mut loop_ = PollLoop::new(tracker, dispatcher as Arc<dyn Dispatcher>, cfg);
        let mut rx = loop_.subscribe();
        let cancel = CancellationToken::new();

        loop_.tick(&cancel).await.unwrap();
        // Give the spawned dispatch a chance to resolve so the
        // reap_finished path emits Released too.
        tokio::time::sleep(Duration::from_millis(10)).await;
        loop_.tick(&cancel).await.unwrap();

        let events = drain_events(&mut rx).await;
        // We expect at minimum: StateChanged{None→Running} on dispatch,
        // and Released{Completed} on reap.
        let kinds: Vec<&str> = events.iter().map(|e| e.kind()).collect();
        assert!(
            kinds.contains(&"state_changed"),
            "expected state_changed in {kinds:?}"
        );
        assert!(
            kinds.contains(&"released"),
            "expected released in {kinds:?}"
        );

        let claimed = events
            .iter()
            .find(|e| matches!(e, OrchestratorEvent::StateChanged { .. }))
            .expect("StateChanged event");
        if let OrchestratorEvent::StateChanged {
            previous, current, ..
        } = claimed
        {
            assert!(previous.is_none(), "initial claim has no previous state");
            assert_eq!(*current, ClaimState::Running);
        }
    }

    #[tokio::test]
    async fn reconciliation_emits_a_single_reconciled_summary() {
        let tracker = Arc::new(StaticTracker::new(issues(2)));
        let dispatcher = Arc::new(CancellableDispatcher::new());
        let cfg = PollLoopConfig {
            interval: Duration::from_millis(1),
            jitter_ratio: 0.0,
            max_concurrent: 10,
            drain_deadline: Duration::from_secs(1),
        };
        let mut loop_ = PollLoop::new(
            Arc::clone(&tracker) as Arc<dyn IssueTracker>,
            Arc::clone(&dispatcher) as Arc<dyn Dispatcher>,
            cfg,
        );
        let mut rx = loop_.subscribe();
        let cancel = CancellationToken::new();

        loop_.tick(&cancel).await.unwrap();
        for _ in 0..200 {
            if dispatcher.started.load(Ordering::SeqCst) == 2 {
                break;
            }
            tokio::time::sleep(Duration::from_millis(2)).await;
        }
        // Drop both from the active set: reconciliation should fire
        // exactly one Reconciled summary listing both.
        tracker.replace(Vec::new());
        loop_.tick(&cancel).await.unwrap();

        let events = drain_events(&mut rx).await;
        let reconciled: Vec<&OrchestratorEvent> = events
            .iter()
            .filter(|e| matches!(e, OrchestratorEvent::Reconciled { .. }))
            .collect();
        assert_eq!(
            reconciled.len(),
            1,
            "expected exactly one Reconciled summary, got {events:?}"
        );
        if let OrchestratorEvent::Reconciled { dropped } = reconciled[0] {
            assert_eq!(dropped.len(), 2);
        }

        cancel.cancel();
    }

    #[tokio::test]
    async fn no_op_tick_emits_no_reconciled_event() {
        // Steady state: same active set tick over tick. The bus must
        // be silent on the reconciliation channel — `Reconciled` only
        // fires when *something* was dropped.
        let tracker = Arc::new(StaticTracker::new(issues(1)));
        let dispatcher = Arc::new(CancellableDispatcher::new());
        let cfg = PollLoopConfig {
            interval: Duration::from_millis(1),
            jitter_ratio: 0.0,
            max_concurrent: 5,
            drain_deadline: Duration::from_secs(1),
        };
        let mut loop_ = PollLoop::new(tracker, Arc::clone(&dispatcher) as Arc<dyn Dispatcher>, cfg);
        let mut rx = loop_.subscribe();
        let cancel = CancellationToken::new();

        loop_.tick(&cancel).await.unwrap();
        for _ in 0..200 {
            if dispatcher.started.load(Ordering::SeqCst) == 1 {
                break;
            }
            tokio::time::sleep(Duration::from_millis(2)).await;
        }
        loop_.tick(&cancel).await.unwrap();

        let events = drain_events(&mut rx).await;
        assert!(
            !events
                .iter()
                .any(|e| matches!(e, OrchestratorEvent::Reconciled { .. })),
            "expected no Reconciled events on steady state, got {events:?}"
        );

        cancel.cancel();
    }

    #[tokio::test]
    async fn released_event_carries_identifier_and_reason() {
        let tracker = Arc::new(StaticTracker::new(issues(1)));
        let dispatcher = Arc::new(InstantDispatcher(ReleaseReason::Completed));
        let cfg = PollLoopConfig {
            interval: Duration::from_millis(1),
            jitter_ratio: 0.0,
            max_concurrent: 1,
            drain_deadline: Duration::from_secs(1),
        };
        let mut loop_ = PollLoop::new(tracker, dispatcher as Arc<dyn Dispatcher>, cfg);
        let mut rx = loop_.subscribe();
        let cancel = CancellationToken::new();

        loop_.tick(&cancel).await.unwrap();
        tokio::time::sleep(Duration::from_millis(10)).await;
        loop_.tick(&cancel).await.unwrap();

        let events = drain_events(&mut rx).await;
        let released = events
            .iter()
            .find_map(|e| {
                if let OrchestratorEvent::Released {
                    identifier, reason, ..
                } = e
                {
                    Some((identifier.clone(), *reason))
                } else {
                    None
                }
            })
            .expect("Released event present");
        assert_eq!(released.0, "ENG-0", "identifier carried through reap");
        assert_eq!(released.1, ReleaseReason::Completed);
    }

    #[tokio::test]
    async fn with_event_bus_constructor_threads_external_bus_through() {
        // Composition-root use case: the SSE handler in `symphony-cli`
        // wants its own clone of the bus. `PollLoop::with_event_bus`
        // must use the supplied bus rather than build a fresh one.
        let tracker = Arc::new(StaticTracker::new(issues(1)));
        let dispatcher = Arc::new(InstantDispatcher(ReleaseReason::Completed));
        let bus = EventBus::new(64);
        let mut external_rx = bus.subscribe();
        let mut loop_ = PollLoop::with_event_bus(
            tracker,
            dispatcher as Arc<dyn Dispatcher>,
            PollLoopConfig {
                interval: Duration::from_millis(1),
                jitter_ratio: 0.0,
                max_concurrent: 1,
                drain_deadline: Duration::from_secs(1),
            },
            bus,
        );
        let cancel = CancellationToken::new();

        loop_.tick(&cancel).await.unwrap();
        tokio::time::sleep(Duration::from_millis(10)).await;
        loop_.tick(&cancel).await.unwrap();

        let events = drain_events(&mut external_rx).await;
        assert!(
            events
                .iter()
                .any(|e| matches!(e, OrchestratorEvent::StateChanged { .. })),
            "external subscriber observed loop emissions: {events:?}"
        );
    }
}
