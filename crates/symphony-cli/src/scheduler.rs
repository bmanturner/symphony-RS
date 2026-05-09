//! Composition helper for the v2 multi-queue scheduler.
//!
//! Phase 11 of the v2 checklist replaces the flat [`PollLoop`] with a
//! [`SchedulerV2`] that fans logical queue ticks (intake, specialist,
//! integration, QA, follow-up approval, budget pause, recovery) under
//! one shared `polling.interval_ms` cadence. Wiring all of those at
//! once is too large for one commit; this module is the seam each
//! subsequent decomposition step extends.
//!
//! [`build_scheduler_v2`] currently registers the **intake** tick only,
//! and returns the shared [`ActiveSetStore`] alongside the scheduler so
//! later steps can plumb it into specialist / recovery / etc. The
//! returned scheduler is not yet wired into [`crate::run::run`]; that
//! switch is the last decomposition step. Until then the production
//! entry point remains [`PollLoop`].
//!
//! [`PollLoop`]: symphony_core::PollLoop

// Pre-wiring: `build_scheduler_v2` is exercised by this module's tests
// but not yet referenced from `run.rs`. Subsequent decomposition steps
// add the call site; until then suppress dead-code under `-D warnings`.
#![allow(dead_code)]

use std::sync::Arc;

use symphony_config::WorkflowConfig;
use symphony_core::{
    ActiveSetStore, IntakeQueueTick, QueueTick, QueueTickCadence, SchedulerV2, SchedulerV2Config,
    TrackerRead,
};

/// What [`build_scheduler_v2`] hands back to the composition root.
///
/// The store is exposed alongside the scheduler so subsequent
/// decomposition steps can give the same `Arc` to specialist /
/// recovery ticks without re-reading the tracker. Holding it on the
/// returned struct (instead of fishing it back out of the intake tick)
/// keeps the wiring shape uniform once those ticks are added.
pub struct SchedulerV2Bundle {
    /// The constructed scheduler, ready to drive via
    /// [`SchedulerV2::tick_once`] (tests) or [`SchedulerV2::run`]
    /// (composition root).
    pub scheduler: SchedulerV2,
    /// Shared active-set store written by the intake tick and read by
    /// every downstream queue tick.
    pub active_set: Arc<ActiveSetStore>,
}

/// Build a [`SchedulerV2`] with the intake tick wired up.
///
/// Cadence is mapped from [`WorkflowConfig::polling`]:
/// `interval_ms` → `SchedulerV2Config::interval`,
/// `jitter_ms` → `SchedulerV2Config::jitter`. The intake tick reuses
/// the same cadence for now — there is no per-queue cadence config in
/// `WORKFLOW.md` yet, and SPEC v2 §5.2 names a single `polling.*`
/// block.
///
/// This function is intentionally additive: it does not register
/// specialist / integration / QA / follow-up approval / budget-pause /
/// recovery ticks yet. Those come in subsequent decomposition steps,
/// which will extend this builder rather than introduce a parallel one.
pub fn build_scheduler_v2(
    cfg: &WorkflowConfig,
    tracker: Arc<dyn TrackerRead>,
) -> SchedulerV2Bundle {
    let cadence = QueueTickCadence::from_millis(cfg.polling.interval_ms, cfg.polling.jitter_ms);
    let scheduler_cfg = SchedulerV2Config::from_cadence(cadence);

    let active_set = Arc::new(ActiveSetStore::new());
    let intake: Box<dyn QueueTick> =
        Box::new(IntakeQueueTick::new(tracker, active_set.clone(), cadence));

    let scheduler = SchedulerV2::with_ticks(scheduler_cfg, vec![intake]);
    SchedulerV2Bundle {
        scheduler,
        active_set,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use async_trait::async_trait;
    use std::sync::Mutex as StdMutex;
    use std::time::Duration;
    use symphony_config::PollingConfig;
    use symphony_core::tracker::{Issue, IssueId, IssueState};
    use symphony_core::tracker_trait::{TrackerError, TrackerResult};
    use tokio_util::sync::CancellationToken;

    /// Minimal `TrackerRead` returning a fixed snapshot. Mirrors the
    /// shape used in `intake_tick`'s own tests so we exercise the
    /// builder against the same contract.
    struct StaticTracker {
        active: StdMutex<Vec<Issue>>,
    }

    impl StaticTracker {
        fn new(active: Vec<Issue>) -> Self {
            Self {
                active: StdMutex::new(active),
            }
        }
    }

    #[async_trait]
    impl TrackerRead for StaticTracker {
        async fn fetch_active(&self) -> TrackerResult<Vec<Issue>> {
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
            Err(TrackerError::Transport("unused".into()))
        }
    }

    fn issues(n: usize) -> Vec<Issue> {
        (0..n)
            .map(|i| Issue::minimal(format!("id-{i}"), format!("ENG-{i}"), "title", "Todo"))
            .collect()
    }

    fn cfg_with(interval_ms: u64, jitter_ms: u64) -> WorkflowConfig {
        WorkflowConfig {
            polling: PollingConfig {
                interval_ms,
                jitter_ms,
                ..PollingConfig::default()
            },
            ..WorkflowConfig::default()
        }
    }

    #[tokio::test]
    async fn builder_registers_intake_tick_only() {
        let cfg = cfg_with(50, 0);
        let tracker = Arc::new(StaticTracker::new(issues(2))) as Arc<dyn TrackerRead>;
        let bundle = build_scheduler_v2(&cfg, tracker);
        // Intake-only seam: subsequent decomposition steps grow this.
        assert_eq!(bundle.scheduler.tick_count(), 1);
    }

    #[tokio::test]
    async fn builder_maps_polling_config_into_scheduler_cadence() {
        let cfg = cfg_with(123, 45);
        let tracker = Arc::new(StaticTracker::new(Vec::new())) as Arc<dyn TrackerRead>;
        let bundle = build_scheduler_v2(&cfg, tracker);
        let sc = bundle.scheduler.config();
        assert_eq!(sc.interval, Duration::from_millis(123));
        assert_eq!(sc.jitter, Duration::from_millis(45));
    }

    #[tokio::test]
    async fn tick_once_publishes_active_set_into_shared_store() {
        let cfg = cfg_with(10, 0);
        let tracker = Arc::new(StaticTracker::new(issues(3))) as Arc<dyn TrackerRead>;
        let bundle = build_scheduler_v2(&cfg, tracker);
        let SchedulerV2Bundle {
            mut scheduler,
            active_set,
        } = bundle;

        assert!(active_set.is_empty());
        let report = scheduler.tick_once().await;
        assert_eq!(report.outcomes.len(), 1);
        assert_eq!(report.outcomes[0].processed, 3);
        assert_eq!(active_set.len(), 3);
        assert_eq!(
            active_set
                .snapshot()
                .iter()
                .map(|i| i.identifier.clone())
                .collect::<Vec<_>>(),
            vec!["ENG-0", "ENG-1", "ENG-2"],
        );
    }

    #[tokio::test]
    async fn run_advances_under_configured_cadence_until_cancelled() {
        let cfg = cfg_with(2, 0);
        let tracker = Arc::new(StaticTracker::new(issues(1))) as Arc<dyn TrackerRead>;
        let bundle = build_scheduler_v2(&cfg, tracker);
        let SchedulerV2Bundle {
            scheduler,
            active_set,
        } = bundle;

        let cancel = CancellationToken::new();
        let cancel_clone = cancel.clone();
        let handle = tokio::spawn(async move { scheduler.run(cancel_clone).await });

        // Wait until the intake tick has populated the store at least
        // once — proves `run` is actually driving ticks under cadence.
        for _ in 0..200 {
            if active_set.len() == 1 {
                break;
            }
            tokio::time::sleep(Duration::from_millis(2)).await;
        }
        assert_eq!(active_set.len(), 1);

        cancel.cancel();
        tokio::time::timeout(Duration::from_secs(1), handle)
            .await
            .expect("scheduler exited promptly after cancel")
            .expect("no panic");
    }
}
