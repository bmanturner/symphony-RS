//! Per-queue tick trait abstraction (SPEC v2 §5 / ARCHITECTURE v2 §7.1).
//!
//! [`LogicalQueue`] / [`QueueTickOutcome`] (see [`crate::logical_queue`])
//! gave the v2 scheduler fan its *identity* and per-tick *report shape*.
//! This module supplies the **per-queue contract**:
//!
//! * [`QueueTickCadence`] — the cadence input every tick consumes.
//! * [`QueueTick`] — an async trait describing one queue's tick step.
//! * [`ScriptedQueueTick`] — a deterministic fake used by the test harness
//!   and by downstream queue-tick implementations as a reference shape.
//! * [`run_queue_tick_n`] — a helper that drives a tick `n` times and
//!   collects the outcomes. Lets tests assert deterministic per-tick
//!   behavior without any wall-clock sleeping.
//!
//! The concrete queue ticks (intake, specialist, integration, QA,
//! follow-up approval, budget pause, recovery) and the multi-queue
//! Scheduler v2 wiring land in subsequent checklist items; isolating the
//! trait + harness here keeps each concrete tick focused on its own
//! domain logic and avoids re-deriving the cadence / outcome plumbing.

use std::time::Duration;

use async_trait::async_trait;
use serde::{Deserialize, Serialize};

use crate::logical_queue::{LogicalQueue, QueueTickOutcome};

/// Cadence knobs applied to a single logical queue's tick.
///
/// Mirrors `polling.interval_ms` + `polling.jitter_ms` from SPEC v2
/// §5.2 but is expressed as `Duration`s so the orchestrator never has
/// to re-multiply by `1_000_000` at every site. The scheduler may use
/// the same cadence for every queue (the default Phase 11 wiring) or
/// override per queue once differentiated cadences land.
///
/// Both fields are `Duration` instead of `u64` to make zero-jitter,
/// fixed-interval test setups self-documenting:
/// `QueueTickCadence::fixed(Duration::from_millis(50))`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct QueueTickCadence {
    /// Mean wall-clock interval between successive ticks.
    pub interval: Duration,
    /// Half-width of the symmetric jitter window applied around
    /// `interval`. `Duration::ZERO` disables jitter, matching the
    /// deterministic test harness.
    pub jitter: Duration,
}

impl QueueTickCadence {
    /// Cadence with `interval` and no jitter — the canonical shape for
    /// deterministic tests and for callers that want pure fixed pacing.
    pub fn fixed(interval: Duration) -> Self {
        Self {
            interval,
            jitter: Duration::ZERO,
        }
    }

    /// Cadence built directly from `polling.interval_ms` /
    /// `polling.jitter_ms` config values, saving callers the
    /// `Duration::from_millis` boilerplate at every wiring site.
    pub fn from_millis(interval_ms: u64, jitter_ms: u64) -> Self {
        Self {
            interval: Duration::from_millis(interval_ms),
            jitter: Duration::from_millis(jitter_ms),
        }
    }
}

impl Default for QueueTickCadence {
    /// Defaults match SPEC v2 §5.2: 30 s interval, no jitter. Concrete
    /// schedulers override `jitter` from `polling.jitter_ms` once they
    /// have a config in hand.
    fn default() -> Self {
        Self::fixed(Duration::from_secs(30))
    }
}

/// One per-queue tick step.
///
/// Every concrete logical queue (intake, specialist, integration, QA,
/// follow-up approval, budget pause, recovery) implements this trait so
/// the v2 scheduler can drive each on its own cadence while sharing one
/// outcome shape. The trait stays intentionally narrow — it does not
/// touch wall-clock sleeping, retries, or cancellation. Those concerns
/// belong to the multi-queue scheduler that fans the ticks; ticks
/// themselves should be one bounded, deterministic step.
///
/// Implementations are `async` because most ticks reach into adapters
/// (tracker reads, repository scans, agent dispatch). Synchronous
/// implementations remain trivial: just `async {}` the body.
#[async_trait]
pub trait QueueTick: Send + Sync {
    /// Which logical queue this tick represents. Used by the scheduler
    /// to label outcomes, by status surfaces to group reports, and by
    /// tests to assert exhaustiveness.
    fn queue(&self) -> LogicalQueue;

    /// Cadence config controlling how often the scheduler invokes this
    /// tick. Returned by reference-by-value so concrete ticks can store
    /// it inline; the scheduler is free to call this once at wiring
    /// time and cache the result.
    fn cadence(&self) -> QueueTickCadence;

    /// Perform exactly one tick step and report what happened.
    ///
    /// The returned [`QueueTickOutcome`] **must** carry the same
    /// [`LogicalQueue`] that [`Self::queue`] reports; the test harness
    /// asserts this invariant so labeling drift cannot creep in.
    async fn tick(&mut self) -> QueueTickOutcome;
}

/// Deterministic [`QueueTick`] used by the test harness and as a
/// reference implementation for upcoming concrete ticks.
///
/// The fake replays a pre-programmed script of [`QueueTickOutcome`]
/// values, one per call to [`QueueTick::tick`]. Once the script is
/// exhausted, every subsequent tick yields [`QueueTickOutcome::idle`]
/// for the configured queue — that mirrors the expected steady-state
/// behavior of real ticks (idle when there is nothing to do).
///
/// Two construction shapes:
///
/// * [`ScriptedQueueTick::new`] — pass a queue, cadence, and an
///   explicit script vector.
/// * [`ScriptedQueueTick::idle`] — an empty script that always reports
///   idle. Useful when the test only cares about scheduler wiring.
pub struct ScriptedQueueTick {
    queue: LogicalQueue,
    cadence: QueueTickCadence,
    /// Remaining outcomes, popped from the front on each tick. Stored
    /// in reverse so we can `pop()` in O(1) without shifting.
    remaining: Vec<QueueTickOutcome>,
    /// Total number of ticks observed, including idle replays past the
    /// end of the script. Lets the harness assert call counts without
    /// relying on the script contents.
    ticks: usize,
}

impl ScriptedQueueTick {
    /// Build a scripted tick that will replay `script` in order.
    ///
    /// Scripts may carry any [`QueueTickOutcome`] — the constructor
    /// rewrites every entry's `queue` field to match `queue` so test
    /// authors can use [`QueueTickOutcome::default`] without tripping
    /// the labeling invariant. Once `script` is exhausted, every
    /// further call to [`QueueTick::tick`] yields an idle outcome for
    /// `queue`.
    pub fn new(
        queue: LogicalQueue,
        cadence: QueueTickCadence,
        script: Vec<QueueTickOutcome>,
    ) -> Self {
        let mut normalized: Vec<_> = script
            .into_iter()
            .map(|mut o| {
                o.queue = queue;
                o
            })
            .collect();
        // Reverse once so we can pop from the back == replay from the front.
        normalized.reverse();
        Self {
            queue,
            cadence,
            remaining: normalized,
            ticks: 0,
        }
    }

    /// Convenience: a scripted tick with no outcomes — every tick
    /// reports idle. Equivalent to `new(queue, cadence, vec![])`.
    pub fn idle(queue: LogicalQueue, cadence: QueueTickCadence) -> Self {
        Self::new(queue, cadence, Vec::new())
    }

    /// Total number of ticks observed so far, including idle replays
    /// past the end of the script.
    pub fn tick_count(&self) -> usize {
        self.ticks
    }

    /// Number of scripted outcomes still queued for replay.
    pub fn remaining_script(&self) -> usize {
        self.remaining.len()
    }
}

#[async_trait]
impl QueueTick for ScriptedQueueTick {
    fn queue(&self) -> LogicalQueue {
        self.queue
    }

    fn cadence(&self) -> QueueTickCadence {
        self.cadence
    }

    async fn tick(&mut self) -> QueueTickOutcome {
        self.ticks += 1;
        self.remaining
            .pop()
            .unwrap_or_else(|| QueueTickOutcome::idle(self.queue))
    }
}

/// Drive `tick` exactly `n` times and collect the outcomes in call
/// order. Asserts (in debug builds) that every outcome's `queue` field
/// matches `tick.queue()` — concrete ticks must not relabel their
/// outcomes mid-flight.
///
/// This is the canonical deterministic test harness for queue ticks:
/// it does not sleep, does not consult cadence, and does not introduce
/// concurrency. It exists so that per-tick logic can be exercised in
/// isolation; cadence sequencing is the multi-queue scheduler's job and
/// is tested separately.
pub async fn run_queue_tick_n<T: QueueTick + ?Sized>(
    tick: &mut T,
    n: usize,
) -> Vec<QueueTickOutcome> {
    let expected_queue = tick.queue();
    let mut out = Vec::with_capacity(n);
    for _ in 0..n {
        let outcome = tick.tick().await;
        debug_assert_eq!(
            outcome.queue, expected_queue,
            "QueueTick::tick() returned an outcome whose queue ({:?}) \
             differs from QueueTick::queue() ({:?}); concrete ticks must \
             label outcomes consistently.",
            outcome.queue, expected_queue,
        );
        out.push(outcome);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cadence_fixed_has_zero_jitter() {
        let c = QueueTickCadence::fixed(Duration::from_millis(250));
        assert_eq!(c.interval, Duration::from_millis(250));
        assert_eq!(c.jitter, Duration::ZERO);
    }

    #[test]
    fn cadence_from_millis_round_trips_to_durations() {
        let c = QueueTickCadence::from_millis(30_000, 500);
        assert_eq!(c.interval, Duration::from_millis(30_000));
        assert_eq!(c.jitter, Duration::from_millis(500));
    }

    #[test]
    fn cadence_default_is_spec_v2_default() {
        // SPEC v2 §5.2 defaults: 30 s interval, no jitter (jitter is
        // opt-in via polling.jitter_ms).
        let c = QueueTickCadence::default();
        assert_eq!(c.interval, Duration::from_secs(30));
        assert_eq!(c.jitter, Duration::ZERO);
    }

    #[test]
    fn cadence_serializes_with_duration_fields() {
        let c = QueueTickCadence::from_millis(1_000, 250);
        let json = serde_json::to_string(&c).unwrap();
        let back: QueueTickCadence = serde_json::from_str(&json).unwrap();
        assert_eq!(back, c);
    }

    fn outcome(
        queue: LogicalQueue,
        considered: usize,
        processed: usize,
        deferred: usize,
        errors: usize,
    ) -> QueueTickOutcome {
        QueueTickOutcome {
            queue,
            considered,
            processed,
            deferred,
            errors,
        }
    }

    #[tokio::test]
    async fn scripted_tick_replays_script_in_order() {
        let cadence = QueueTickCadence::fixed(Duration::from_millis(10));
        let script = vec![
            outcome(LogicalQueue::Specialist, 3, 2, 1, 0),
            outcome(LogicalQueue::Specialist, 5, 5, 0, 0),
        ];
        let mut t = ScriptedQueueTick::new(LogicalQueue::Specialist, cadence, script.clone());

        let r1 = t.tick().await;
        let r2 = t.tick().await;
        assert_eq!(r1, script[0]);
        assert_eq!(r2, script[1]);
        assert_eq!(t.tick_count(), 2);
        assert_eq!(t.remaining_script(), 0);
    }

    #[tokio::test]
    async fn scripted_tick_relabels_outcomes_to_match_queue() {
        // Default outcome carries `LogicalQueue::Intake`; the scripted
        // tick must rewrite it to its own queue so the labeling
        // invariant holds.
        let cadence = QueueTickCadence::fixed(Duration::from_millis(1));
        let mut t = ScriptedQueueTick::new(
            LogicalQueue::Qa,
            cadence,
            vec![QueueTickOutcome {
                queue: LogicalQueue::Intake,
                considered: 7,
                processed: 4,
                deferred: 2,
                errors: 1,
            }],
        );
        let r = t.tick().await;
        assert_eq!(r.queue, LogicalQueue::Qa);
        assert_eq!(r.considered, 7);
        assert_eq!(r.processed, 4);
        assert_eq!(r.deferred, 2);
        assert_eq!(r.errors, 1);
    }

    #[tokio::test]
    async fn scripted_tick_idle_after_script_exhausted() {
        let cadence = QueueTickCadence::fixed(Duration::from_millis(1));
        let mut t = ScriptedQueueTick::new(
            LogicalQueue::Integration,
            cadence,
            vec![outcome(LogicalQueue::Integration, 1, 1, 0, 0)],
        );
        let _ = t.tick().await;
        let post1 = t.tick().await;
        let post2 = t.tick().await;
        assert!(post1.is_idle());
        assert_eq!(post1.queue, LogicalQueue::Integration);
        assert!(post2.is_idle());
        assert_eq!(t.tick_count(), 3);
    }

    #[tokio::test]
    async fn scripted_idle_constructor_always_reports_idle() {
        let cadence = QueueTickCadence::fixed(Duration::from_millis(1));
        let mut t = ScriptedQueueTick::idle(LogicalQueue::Recovery, cadence);
        for _ in 0..5 {
            let r = t.tick().await;
            assert!(r.is_idle());
            assert_eq!(r.queue, LogicalQueue::Recovery);
        }
        assert_eq!(t.tick_count(), 5);
    }

    #[tokio::test]
    async fn run_queue_tick_n_collects_n_outcomes_in_order() {
        let cadence = QueueTickCadence::fixed(Duration::from_millis(1));
        let script = vec![
            outcome(LogicalQueue::FollowupApproval, 2, 1, 1, 0),
            outcome(LogicalQueue::FollowupApproval, 0, 0, 0, 0),
            outcome(LogicalQueue::FollowupApproval, 3, 3, 0, 0),
        ];
        let mut t = ScriptedQueueTick::new(LogicalQueue::FollowupApproval, cadence, script.clone());

        let collected = run_queue_tick_n(&mut t, 3).await;
        assert_eq!(collected, script);
        assert_eq!(t.tick_count(), 3);
    }

    #[tokio::test]
    async fn run_queue_tick_n_pads_with_idle_past_script_end() {
        // Asks for more ticks than the script has — the extras must be
        // idle outcomes for the same queue. Mirrors the scheduler's
        // expected steady state.
        let cadence = QueueTickCadence::fixed(Duration::from_millis(1));
        let mut t = ScriptedQueueTick::new(
            LogicalQueue::BudgetPause,
            cadence,
            vec![outcome(LogicalQueue::BudgetPause, 1, 0, 1, 0)],
        );
        let collected = run_queue_tick_n(&mut t, 3).await;
        assert_eq!(collected.len(), 3);
        assert_eq!(collected[0].processed, 0);
        assert_eq!(collected[0].deferred, 1);
        assert!(collected[1].is_idle());
        assert!(collected[2].is_idle());
        for o in &collected {
            assert_eq!(o.queue, LogicalQueue::BudgetPause);
        }
    }

    #[tokio::test]
    async fn run_queue_tick_n_zero_returns_empty_without_calling_tick() {
        let cadence = QueueTickCadence::fixed(Duration::from_millis(1));
        let mut t = ScriptedQueueTick::idle(LogicalQueue::Intake, cadence);
        let collected = run_queue_tick_n(&mut t, 0).await;
        assert!(collected.is_empty());
        assert_eq!(t.tick_count(), 0);
    }

    #[tokio::test]
    async fn scripted_tick_reports_its_queue_and_cadence() {
        let cadence = QueueTickCadence::from_millis(2_000, 100);
        let t = ScriptedQueueTick::idle(LogicalQueue::Qa, cadence);
        assert_eq!(t.queue(), LogicalQueue::Qa);
        assert_eq!(t.cadence(), cadence);
    }

    #[tokio::test]
    async fn run_queue_tick_n_works_through_dyn_trait_object() {
        // Confirms the harness's `?Sized` bound: schedulers will store
        // `Box<dyn QueueTick>` and the harness must drive them too.
        let cadence = QueueTickCadence::fixed(Duration::from_millis(1));
        let mut boxed: Box<dyn QueueTick> = Box::new(ScriptedQueueTick::new(
            LogicalQueue::Specialist,
            cadence,
            vec![outcome(LogicalQueue::Specialist, 1, 1, 0, 0)],
        ));
        let collected = run_queue_tick_n(boxed.as_mut(), 2).await;
        assert_eq!(collected.len(), 2);
        assert_eq!(collected[0].processed, 1);
        assert!(collected[1].is_idle());
    }
}
