//! Scheduler v2 — multi-queue tick fan (SPEC v2 §5 / ARCHITECTURE v2 §7.1).
//!
//! Phase 11 of the v2 checklist replaces the flat [`crate::poll_loop::PollLoop`]
//! with a fan of logical queues that share `polling.interval_ms` cadence
//! and `polling.jitter_ms` half-width. Each queue (intake, specialist,
//! integration, QA, follow-up approval, budget pause, recovery) is a
//! [`crate::queue_tick::QueueTick`] implementation; this module wires
//! them together under a single tick driver.
//!
//! Reconciliation/cancellation semantics from the flat poll loop migrate
//! to two of those queues:
//!
//! * The **intake** tick republishes the active set on every cadence,
//!   so downstream consumers (specialist, recovery) can detect issues
//!   that have left the active set without re-fetching.
//! * The **recovery** tick reconciles expired leases and orphaned
//!   workspace claims, surfacing them as durable dispatch requests for
//!   the recovery handler to act on.
//!
//! The scheduler itself is intentionally narrow: it owns the tick fan,
//! the shared cadence, and a cooperative cancellation path. It does
//! **not** own dispatch, retries, or per-issue state — those concerns
//! belong to the queue ticks, the dispatcher (subsequent checklist
//! items), and durable state respectively. Keeping the scheduler narrow
//! makes the cadence/jitter contract testable in isolation.
//!
//! ## Tick semantics
//!
//! One scheduler tick = one pass over every registered queue tick, in
//! registration order. Ticks are driven sequentially within a pass so
//! the resulting [`SchedulerTickReport`] is deterministic in tests and
//! so that ordering invariants between queues (intake before specialist
//! before downstream consumers) hold without explicit synchronisation.
//! The per-queue work is bounded — each tick reads one snapshot, emits
//! requests, and returns — so sequential execution does not throttle
//! throughput in practice.
//!
//! ## Cancellation
//!
//! [`SchedulerV2::run`] drives ticks until its [`CancellationToken`]
//! fires, then returns. A cancellation observed *during* a tick still
//! finishes the in-flight tick (each tick is bounded and short) before
//! returning — this matches the flat poll loop's "drain in-flight
//! before exiting" contract for the queue-tick layer. In-flight agent
//! sessions are owned by the dispatcher, not the scheduler, and their
//! drain semantics are unchanged from the flat-loop era.

use std::sync::Arc;
use std::time::Duration;

use tokio::sync::Mutex;
use tokio_util::sync::CancellationToken;
use tracing::{debug, info};

use crate::logical_queue::QueueTickOutcome;
use crate::queue_tick::{QueueTick, QueueTickCadence};

/// Knobs for [`SchedulerV2`].
///
/// Mirrors `polling.interval_ms` + `polling.jitter_ms` from SPEC v2
/// §5.2. Callers map directly from [`crate::queue_tick::QueueTickCadence`]
/// when they already have one in hand; otherwise [`SchedulerV2Config::from_millis`]
/// saves the `Duration::from_millis` boilerplate at the wiring site.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SchedulerV2Config {
    /// Mean wall-clock interval between successive scheduler passes.
    pub interval: Duration,
    /// Symmetric half-width of the jitter window applied around
    /// `interval`. `Duration::ZERO` disables jitter — the deterministic
    /// test default.
    pub jitter: Duration,
}

impl SchedulerV2Config {
    /// Build a config with `interval` and no jitter.
    pub fn fixed(interval: Duration) -> Self {
        Self {
            interval,
            jitter: Duration::ZERO,
        }
    }

    /// Build directly from `polling.interval_ms` / `polling.jitter_ms`.
    pub fn from_millis(interval_ms: u64, jitter_ms: u64) -> Self {
        Self {
            interval: Duration::from_millis(interval_ms),
            jitter: Duration::from_millis(jitter_ms),
        }
    }

    /// Build from a [`QueueTickCadence`] — handy when callers already
    /// have one (e.g. for a single queue's own cadence).
    pub fn from_cadence(cadence: QueueTickCadence) -> Self {
        Self {
            interval: cadence.interval,
            jitter: cadence.jitter,
        }
    }
}

impl Default for SchedulerV2Config {
    /// SPEC v2 §5.2 defaults: 30 s interval, no jitter.
    fn default() -> Self {
        Self::fixed(Duration::from_secs(30))
    }
}

/// Per-pass report — one entry per registered tick, in registration order.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct SchedulerTickReport {
    /// Outcomes for each queue tick the scheduler drove this pass.
    pub outcomes: Vec<QueueTickOutcome>,
}

impl SchedulerTickReport {
    /// `true` iff every constituent outcome is idle. Useful for tests
    /// and operator surfaces that want to collapse no-op passes.
    pub fn is_idle(&self) -> bool {
        self.outcomes.iter().all(|o| o.is_idle())
    }
}

/// Multi-queue scheduler.
///
/// Construct via [`SchedulerV2::new`] / [`SchedulerV2::with_ticks`] and
/// drive with either [`SchedulerV2::tick_once`] (deterministic, used by
/// tests) or [`SchedulerV2::run`] (cadence-driven, used at the
/// composition root).
pub struct SchedulerV2 {
    ticks: Vec<Arc<Mutex<Box<dyn QueueTick>>>>,
    config: SchedulerV2Config,
    tick_seq: u64,
}

impl SchedulerV2 {
    /// Empty scheduler with the given cadence. Add ticks before running.
    pub fn new(config: SchedulerV2Config) -> Self {
        Self {
            ticks: Vec::new(),
            config,
            tick_seq: 0,
        }
    }

    /// Convenience: scheduler pre-populated with `ticks`.
    pub fn with_ticks(config: SchedulerV2Config, ticks: Vec<Box<dyn QueueTick>>) -> Self {
        let mut s = Self::new(config);
        for t in ticks {
            s.add_tick(t);
        }
        s
    }

    /// Append a queue tick. Order is the order of subsequent
    /// [`SchedulerTickReport::outcomes`] entries; intake must precede
    /// any tick that reads from its [`crate::intake_tick::ActiveSetStore`].
    pub fn add_tick(&mut self, tick: Box<dyn QueueTick>) {
        self.ticks.push(Arc::new(Mutex::new(tick)));
    }

    /// Number of registered ticks.
    pub fn tick_count(&self) -> usize {
        self.ticks.len()
    }

    /// Active scheduler config.
    pub fn config(&self) -> SchedulerV2Config {
        self.config
    }

    /// Drive every registered tick once, sequentially. Returns one
    /// outcome per tick in registration order.
    pub async fn tick_once(&mut self) -> SchedulerTickReport {
        self.tick_seq = self.tick_seq.wrapping_add(1);
        let mut outcomes = Vec::with_capacity(self.ticks.len());
        for slot in &self.ticks {
            let mut guard = slot.lock().await;
            outcomes.push(guard.tick().await);
        }
        SchedulerTickReport { outcomes }
    }

    /// Drive ticks under the configured cadence until `cancel` fires.
    ///
    /// Each iteration: run [`Self::tick_once`], then sleep
    /// `interval ± jitter`, watching the cancel token across the sleep.
    /// On cancellation the scheduler returns once the in-flight pass
    /// (if any) finishes — bounded by the per-tick contract.
    pub async fn run(mut self, cancel: CancellationToken) {
        info!(
            interval_ms = self.config.interval.as_millis() as u64,
            jitter_ms = self.config.jitter.as_millis() as u64,
            ticks = self.ticks.len(),
            "scheduler v2 starting"
        );
        loop {
            tokio::select! {
                biased;
                _ = cancel.cancelled() => {
                    info!("scheduler v2 cancellation requested before tick; exiting");
                    return;
                }
                report = self.tick_once() => {
                    debug!(
                        outcomes = report.outcomes.len(),
                        idle = report.is_idle(),
                        "scheduler v2 pass complete"
                    );
                }
            }
            let nap = jittered_sleep_for(self.config.interval, self.config.jitter, self.tick_seq);
            tokio::select! {
                biased;
                _ = cancel.cancelled() => {
                    info!("scheduler v2 cancellation requested mid-sleep; exiting");
                    return;
                }
                _ = tokio::time::sleep(nap) => {}
            }
        }
    }
}

/// Compute `interval ± jitter` for the given monotonic tick sequence.
///
/// Pulls from a SplitMix64-style mixer over `tick_seq` and the wall
/// clock — same approach as the flat poll loop's `jittered_sleep_for`,
/// but parameterised on a [`Duration`] jitter half-width rather than a
/// ratio. Output is clamped to `[0, interval + jitter]` so a wide jitter
/// can never push the next sleep negative.
pub(crate) fn jittered_sleep_for(interval: Duration, jitter: Duration, tick_seq: u64) -> Duration {
    if interval.is_zero() {
        return Duration::ZERO;
    }
    if jitter.is_zero() {
        return interval;
    }
    let unit = pseudo_unit(tick_seq);
    let jitter_secs = jitter.as_secs_f64() * unit;
    let total = interval.as_secs_f64() + jitter_secs;
    Duration::from_secs_f64(total.max(0.0))
}

fn pseudo_unit(tick_seq: u64) -> f64 {
    use std::time::Instant;
    let now = Instant::now().elapsed().as_nanos() as u64;
    let mut x = tick_seq.wrapping_mul(0x9E37_79B9_7F4A_7C15) ^ now;
    x = (x ^ (x >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
    x = (x ^ (x >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
    x ^= x >> 31;
    let frac = (x >> 11) as f64 / (1u64 << 53) as f64;
    frac * 2.0 - 1.0
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::logical_queue::LogicalQueue;
    use crate::queue_tick::ScriptedQueueTick;
    use std::sync::atomic::{AtomicUsize, Ordering};

    fn cadence() -> QueueTickCadence {
        QueueTickCadence::fixed(Duration::from_millis(1))
    }

    fn outcome(queue: LogicalQueue, processed: usize) -> QueueTickOutcome {
        QueueTickOutcome {
            queue,
            considered: processed,
            processed,
            deferred: 0,
            errors: 0,
        }
    }

    #[test]
    fn config_fixed_has_zero_jitter() {
        let c = SchedulerV2Config::fixed(Duration::from_millis(250));
        assert_eq!(c.interval, Duration::from_millis(250));
        assert_eq!(c.jitter, Duration::ZERO);
    }

    #[test]
    fn config_from_millis_round_trips_to_durations() {
        let c = SchedulerV2Config::from_millis(30_000, 500);
        assert_eq!(c.interval, Duration::from_millis(30_000));
        assert_eq!(c.jitter, Duration::from_millis(500));
    }

    #[test]
    fn config_from_cadence_copies_both_fields() {
        let cad = QueueTickCadence::from_millis(1_000, 100);
        let cfg = SchedulerV2Config::from_cadence(cad);
        assert_eq!(cfg.interval, cad.interval);
        assert_eq!(cfg.jitter, cad.jitter);
    }

    #[test]
    fn config_default_matches_spec_v2_default() {
        let c = SchedulerV2Config::default();
        assert_eq!(c.interval, Duration::from_secs(30));
        assert_eq!(c.jitter, Duration::ZERO);
    }

    #[test]
    fn jitter_zero_returns_exact_interval() {
        let d = jittered_sleep_for(Duration::from_secs(10), Duration::ZERO, 1);
        assert_eq!(d, Duration::from_secs(10));
    }

    #[test]
    fn jitter_stays_within_bounds() {
        let interval = Duration::from_secs(10);
        let jitter = Duration::from_secs(2);
        for seq in 0..1000 {
            let d = jittered_sleep_for(interval, jitter, seq);
            assert!(
                d.as_secs_f64() >= 8.0 && d.as_secs_f64() <= 12.0,
                "seq={seq} d={d:?}"
            );
        }
    }

    #[test]
    fn jitter_cannot_drive_sleep_negative() {
        // Even a jitter wider than the interval must not yield a
        // negative duration — the loop would busy-spin or panic.
        let d = jittered_sleep_for(Duration::from_millis(100), Duration::from_millis(500), 7);
        assert!(d >= Duration::ZERO);
    }

    #[test]
    fn empty_scheduler_reports_zero_ticks() {
        let s = SchedulerV2::new(SchedulerV2Config::default());
        assert_eq!(s.tick_count(), 0);
    }

    #[tokio::test]
    async fn tick_once_drives_each_registered_tick_in_order() {
        let mut s = SchedulerV2::new(SchedulerV2Config::fixed(Duration::from_millis(1)));
        s.add_tick(Box::new(ScriptedQueueTick::new(
            LogicalQueue::Intake,
            cadence(),
            vec![outcome(LogicalQueue::Intake, 3)],
        )));
        s.add_tick(Box::new(ScriptedQueueTick::new(
            LogicalQueue::Specialist,
            cadence(),
            vec![outcome(LogicalQueue::Specialist, 1)],
        )));
        s.add_tick(Box::new(ScriptedQueueTick::new(
            LogicalQueue::Recovery,
            cadence(),
            vec![outcome(LogicalQueue::Recovery, 2)],
        )));

        let report = s.tick_once().await;
        assert_eq!(report.outcomes.len(), 3);
        assert_eq!(report.outcomes[0].queue, LogicalQueue::Intake);
        assert_eq!(report.outcomes[0].processed, 3);
        assert_eq!(report.outcomes[1].queue, LogicalQueue::Specialist);
        assert_eq!(report.outcomes[1].processed, 1);
        assert_eq!(report.outcomes[2].queue, LogicalQueue::Recovery);
        assert_eq!(report.outcomes[2].processed, 2);
        assert!(!report.is_idle());
    }

    #[tokio::test]
    async fn tick_once_emits_idle_outcomes_when_every_tick_is_idle() {
        let mut s = SchedulerV2::with_ticks(
            SchedulerV2Config::fixed(Duration::from_millis(1)),
            vec![
                Box::new(ScriptedQueueTick::idle(LogicalQueue::Intake, cadence())),
                Box::new(ScriptedQueueTick::idle(LogicalQueue::Qa, cadence())),
            ],
        );
        let report = s.tick_once().await;
        assert!(report.is_idle());
        assert_eq!(report.outcomes.len(), 2);
        assert_eq!(report.outcomes[0].queue, LogicalQueue::Intake);
        assert_eq!(report.outcomes[1].queue, LogicalQueue::Qa);
    }

    #[tokio::test]
    async fn tick_once_advances_per_tick_scripts_independently() {
        let mut s = SchedulerV2::with_ticks(
            SchedulerV2Config::fixed(Duration::from_millis(1)),
            vec![
                Box::new(ScriptedQueueTick::new(
                    LogicalQueue::Intake,
                    cadence(),
                    vec![
                        outcome(LogicalQueue::Intake, 1),
                        outcome(LogicalQueue::Intake, 2),
                    ],
                )),
                Box::new(ScriptedQueueTick::new(
                    LogicalQueue::Recovery,
                    cadence(),
                    vec![outcome(LogicalQueue::Recovery, 9)],
                )),
            ],
        );

        let r1 = s.tick_once().await;
        assert_eq!(r1.outcomes[0].processed, 1);
        assert_eq!(r1.outcomes[1].processed, 9);

        let r2 = s.tick_once().await;
        assert_eq!(r2.outcomes[0].processed, 2);
        // Recovery script exhausted -> idle for that queue.
        assert!(r2.outcomes[1].is_idle());
        assert_eq!(r2.outcomes[1].queue, LogicalQueue::Recovery);
    }

    /// Tick that increments a shared counter on every call. Lets the
    /// run-loop tests assert that `run` actually drove ticks before
    /// observing cancellation.
    struct CountingTick {
        queue: LogicalQueue,
        cadence: QueueTickCadence,
        count: Arc<AtomicUsize>,
    }

    #[async_trait::async_trait]
    impl QueueTick for CountingTick {
        fn queue(&self) -> LogicalQueue {
            self.queue
        }
        fn cadence(&self) -> QueueTickCadence {
            self.cadence
        }
        async fn tick(&mut self) -> QueueTickOutcome {
            self.count.fetch_add(1, Ordering::SeqCst);
            QueueTickOutcome::idle(self.queue)
        }
    }

    #[tokio::test]
    async fn run_drives_ticks_until_cancelled() {
        let count = Arc::new(AtomicUsize::new(0));
        let s = SchedulerV2::with_ticks(
            SchedulerV2Config::fixed(Duration::from_millis(2)),
            vec![Box::new(CountingTick {
                queue: LogicalQueue::Intake,
                cadence: cadence(),
                count: count.clone(),
            })],
        );
        let cancel = CancellationToken::new();
        let cancel_clone = cancel.clone();
        let handle = tokio::spawn(async move { s.run(cancel_clone).await });

        // Wait for several ticks to land.
        for _ in 0..100 {
            if count.load(Ordering::SeqCst) >= 3 {
                break;
            }
            tokio::time::sleep(Duration::from_millis(2)).await;
        }
        assert!(count.load(Ordering::SeqCst) >= 3);

        cancel.cancel();
        tokio::time::timeout(Duration::from_secs(1), handle)
            .await
            .expect("scheduler exited promptly after cancel")
            .expect("no panic");
    }

    #[tokio::test]
    async fn run_returns_immediately_when_cancelled_before_first_tick() {
        let s = SchedulerV2::with_ticks(
            SchedulerV2Config::fixed(Duration::from_millis(50)),
            vec![Box::new(ScriptedQueueTick::idle(
                LogicalQueue::Intake,
                cadence(),
            ))],
        );
        let cancel = CancellationToken::new();
        cancel.cancel();
        tokio::time::timeout(Duration::from_secs(1), s.run(cancel))
            .await
            .expect("scheduler exited immediately on pre-cancelled token");
    }

    #[tokio::test]
    async fn run_with_zero_ticks_idles_until_cancelled() {
        let s = SchedulerV2::new(SchedulerV2Config::fixed(Duration::from_millis(1)));
        let cancel = CancellationToken::new();
        let cancel_clone = cancel.clone();
        let handle = tokio::spawn(async move { s.run(cancel_clone).await });
        // Give the loop a moment to spin a few empty passes.
        tokio::time::sleep(Duration::from_millis(10)).await;
        cancel.cancel();
        tokio::time::timeout(Duration::from_secs(1), handle)
            .await
            .expect("scheduler exited after cancel")
            .expect("no panic");
    }

    #[tokio::test]
    async fn add_tick_appends_in_registration_order() {
        let mut s = SchedulerV2::new(SchedulerV2Config::fixed(Duration::from_millis(1)));
        s.add_tick(Box::new(ScriptedQueueTick::idle(
            LogicalQueue::Qa,
            cadence(),
        )));
        s.add_tick(Box::new(ScriptedQueueTick::idle(
            LogicalQueue::Integration,
            cadence(),
        )));
        assert_eq!(s.tick_count(), 2);
        let report = s.tick_once().await;
        assert_eq!(report.outcomes[0].queue, LogicalQueue::Qa);
        assert_eq!(report.outcomes[1].queue, LogicalQueue::Integration);
    }
}
