//! Retry queue with exponential backoff (SPEC §4.1.7 + §8.4).
//!
//! The orchestrator's claim ledger ([`crate::state_machine::StateMachine`])
//! says *who is currently claimed*. The retry queue says *when the next
//! attempt for a `RetryQueued` claim becomes due*. They are deliberately
//! separate types: the state machine is pure storage with no notion of
//! time, and the retry queue is pure scheduling with no notion of who
//! holds a claim. The poll loop owns both and joins them per tick.
//!
//! # Why a separate module?
//!
//! Keeping retry math and timer bookkeeping out of the state machine
//! (and out of the poll loop) lets us unit-test the SPEC §8.4 backoff
//! formula in isolation, without spinning a tokio runtime or faking a
//! tracker. The poll loop's reconciliation pass (a sibling checklist
//! item) will simply call [`RetryQueue::drain_due`] on each tick and
//! feed the resulting [`RetryEntry`] list back through the dispatcher.
//!
//! # Backoff formula (SPEC §8.4)
//!
//! - **Continuation retry** (the worker exited cleanly and we just want
//!   to chase a fresh turn): a fixed short delay — default 1 s.
//! - **Failure-driven retry**: `delay = min(base * 2^(attempt - 1),
//!   max_backoff)`. Default `base = 10_000 ms` (10 s) and `max_backoff
//!   = 300_000 ms` (5 min). `attempt` is 1-based: the first retry uses
//!   `base`, the second uses `2 * base`, and so on.
//!
//! Doubling stops the moment the cap is reached; we do not overflow
//! `u32::MAX` because we saturate the shift before multiplying.
//!
//! # Replacement semantics
//!
//! Per SPEC §8.4 ("Cancel any existing retry timer for the same
//! issue"), [`RetryQueue::schedule`] always *replaces* any prior entry
//! for the same issue. There is exactly one in-flight retry per issue;
//! the orchestrator never lets two timers race.

use std::collections::HashMap;
use std::time::{Duration, Instant};

use crate::tracker::IssueId;

/// Tunable parameters for the retry queue's backoff math.
///
/// Mirrors the `agent.*` keys from SPEC §5.3 so that the
/// `WORKFLOW.md` loader can construct one of these directly without
/// reshaping types. The poll loop borrows it; nothing here mutates.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RetryConfig {
    /// Upper bound on any failure-driven backoff. SPEC default: 5 min.
    pub max_backoff: Duration,
    /// Multiplier base for failure-driven backoff. SPEC default: 10 s.
    /// The first failure-driven retry uses exactly this value; each
    /// subsequent attempt doubles it until [`Self::max_backoff`] caps.
    pub failure_base: Duration,
    /// Fixed delay used for continuation retries (the worker exited
    /// cleanly and the orchestrator wants to chase a follow-up turn).
    /// SPEC default: 1 s.
    pub continuation_delay: Duration,
}

impl RetryConfig {
    /// SPEC §8.4 defaults. Override individual fields after constructing.
    #[must_use]
    pub fn defaults() -> Self {
        Self {
            max_backoff: Duration::from_millis(300_000),
            failure_base: Duration::from_millis(10_000),
            continuation_delay: Duration::from_millis(1_000),
        }
    }
}

/// Why this retry was scheduled. Drives the backoff formula and is
/// preserved on the [`RetryEntry`] for log surfaces.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RetryReason {
    /// Worker exited normally; schedule a short continuation turn.
    Continuation,
    /// Worker failed (error, timeout, stall); back off exponentially.
    Failure,
}

/// One scheduled retry. The orchestrator holds these in
/// [`RetryQueue`] keyed by [`IssueId`], so we do not store the id
/// inside the entry itself — see [`RetryQueue::drain_due`] which
/// returns `(IssueId, RetryEntry)` pairs.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RetryEntry {
    /// Best-effort human identifier carried for status surfaces. Lifted
    /// from the tracker `Issue.identifier` at schedule time. Never used
    /// as a map key — always look up by [`IssueId`].
    pub identifier: String,
    /// 1-based attempt number that *will* be made when this entry
    /// fires. Matches SPEC §4.1.7 `RetryEntry.attempt` and the
    /// `attempt` carried in [`crate::state_machine::ClaimState::RetryQueued`].
    pub attempt: u32,
    /// Instant on the monotonic clock when this entry becomes due.
    /// `RetryQueue` compares against `Instant::now()` (or a test
    /// clock injected via [`RetryQueue::drain_due`]) to decide.
    pub due_at: Instant,
    /// Error message captured at the time of scheduling, if any. Only
    /// populated for [`RetryReason::Failure`]; `None` for continuation
    /// retries to keep their log lines tidy.
    pub error: Option<String>,
    /// What kind of retry this is — used by the orchestrator to log
    /// and (later) to gate stall reconciliation.
    pub reason: RetryReason,
}

/// Inputs to [`RetryQueue::schedule`]. Bundled into a struct rather
/// than passed positionally because the call site has too many
/// dimensions (id, attempt, reason, error, clock) for positional
/// arguments to read clearly.
#[derive(Debug, Clone)]
pub struct ScheduleRequest<'a> {
    /// Issue being scheduled. Becomes the queue key.
    pub id: IssueId,
    /// Best-effort human identifier preserved on the entry.
    pub identifier: &'a str,
    /// 1-based attempt number that *will* be made when the entry fires.
    pub attempt: u32,
    /// Continuation vs. failure — drives the backoff formula.
    pub reason: RetryReason,
    /// Optional failure message. `None` for continuation retries.
    pub error: Option<String>,
    /// Monotonic-clock reference point. The entry's `due_at` is
    /// computed as `now + backoff_for(reason, attempt, cfg)`. Tests
    /// pass a fixed `Instant::now()`; production code passes the same.
    pub now: Instant,
}

/// Outcome of [`RetryQueue::schedule_with_policy`].
///
/// Splits the "schedule a retry" path into the two SPEC §5.15 outcomes:
/// the requested attempt is within the configured `max_retries` budget
/// and was inserted into the queue, or the attempt is over budget and
/// the caller must instead enqueue a budget-pause / emit
/// `OrchestratorEvent::BudgetExceeded`. The over-cap variant carries
/// `observed`/`cap` as `f64` for symmetry with the rest of the budget
/// surface (cost is `f64`, retries are integers — both flow through the
/// same event payload as `f64`).
#[derive(Debug, Clone, PartialEq)]
pub enum RetryPolicyDecision {
    /// The retry was inserted; carries an owned copy of the entry just
    /// scheduled. The entry is also retrievable via
    /// [`RetryQueue::get`].
    ScheduleRetry(RetryEntry),
    /// The configured `max_retries` cap was exceeded; nothing was
    /// inserted into the queue. `observed` is `attempt as f64`, `cap`
    /// is `max_retries as f64`.
    BudgetExceeded {
        /// The attempt number that tripped the cap, as a float for
        /// event-payload symmetry with cost budgets.
        observed: f64,
        /// The configured `max_retries` cap, as a float.
        cap: f64,
    },
}

/// Pure scheduling table: at most one [`RetryEntry`] per issue.
///
/// Owned by the poll loop. Not `Sync`-friendly (no interior
/// mutability); the loop is single-threaded and mutates the queue
/// directly.
#[derive(Debug, Default)]
pub struct RetryQueue {
    entries: HashMap<IssueId, RetryEntry>,
}

impl RetryQueue {
    /// Empty queue.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Number of entries currently scheduled (regardless of due-ness).
    #[must_use]
    pub fn len(&self) -> usize {
        self.entries.len()
    }

    /// Whether the queue holds zero entries.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    /// Look up the scheduled entry for `id`, if any.
    #[must_use]
    pub fn get(&self, id: &IssueId) -> Option<&RetryEntry> {
        self.entries.get(id)
    }

    /// Schedule (or replace) a retry for `id`. Returns the entry just
    /// inserted so callers can log `due_at` without a follow-up
    /// lookup.
    ///
    /// Per SPEC §8.4 this *always* replaces any prior entry for the
    /// same issue: the orchestrator owns at most one in-flight timer
    /// per issue. Replacement is silent — if you need to know whether
    /// an entry was overwritten, call [`Self::get`] first.
    ///
    /// This method ignores the `max_retries` budget; it is preserved as
    /// a thin wrapper for callers that have not yet migrated to
    /// [`Self::schedule_with_policy`]. New code should prefer the
    /// policy-aware variant.
    pub fn schedule(&mut self, req: ScheduleRequest<'_>, cfg: &RetryConfig) -> &RetryEntry {
        let id = req.id.clone();
        match self.schedule_with_policy(req, cfg, u32::MAX) {
            RetryPolicyDecision::ScheduleRetry(_) => {
                // unwrap-safe: schedule_with_policy inserted under id.
                &self.entries[&id]
            }
            RetryPolicyDecision::BudgetExceeded { .. } => {
                // Unreachable: u32::MAX cannot be exceeded by a u32 attempt.
                unreachable!("schedule() passed u32::MAX cap; cannot exceed")
            }
        }
    }

    /// Schedule (or replace) a retry for `id` *subject to a
    /// `max_retries` budget*.
    ///
    /// Returns [`RetryPolicyDecision::ScheduleRetry`] with an owned
    /// copy of the inserted entry when `attempt <= max_retries`, or
    /// [`RetryPolicyDecision::BudgetExceeded`] when `attempt >
    /// max_retries`. In the over-cap case the queue is **not** mutated
    /// — no entry is inserted, and any prior entry for the same issue
    /// is left untouched. This lets the caller decide whether to leave
    /// the existing timer running, cancel it, or replace it via a
    /// separate call.
    ///
    /// `attempt` is 1-based per SPEC §4.1.7. With `max_retries = 3` the
    /// allowed attempts are 1, 2, and 3; attempt 4 trips the cap.
    pub fn schedule_with_policy(
        &mut self,
        req: ScheduleRequest<'_>,
        cfg: &RetryConfig,
        max_retries: u32,
    ) -> RetryPolicyDecision {
        if req.attempt > max_retries {
            return RetryPolicyDecision::BudgetExceeded {
                observed: f64::from(req.attempt),
                cap: f64::from(max_retries),
            };
        }
        let ScheduleRequest {
            id,
            identifier,
            attempt,
            reason,
            error,
            now,
        } = req;
        let delay = backoff_for(reason, attempt, cfg);
        let entry = RetryEntry {
            identifier: identifier.to_owned(),
            attempt,
            due_at: now + delay,
            error,
            reason,
        };
        self.entries.insert(id, entry.clone());
        RetryPolicyDecision::ScheduleRetry(entry)
    }

    /// Cancel a scheduled retry. Returns `true` iff one was removed.
    /// Idempotent: calling on an absent id is a no-op.
    pub fn cancel(&mut self, id: &IssueId) -> bool {
        self.entries.remove(id).is_some()
    }

    /// Remove and return all entries whose `due_at <= now`. The
    /// returned vector is sorted by `due_at` ascending so the poll
    /// loop dispatches the oldest-due first when the concurrency cap
    /// forces it to defer some.
    ///
    /// Entries that are not yet due remain in the queue.
    pub fn drain_due(&mut self, now: Instant) -> Vec<(IssueId, RetryEntry)> {
        let due_ids: Vec<IssueId> = self
            .entries
            .iter()
            .filter(|(_, e)| e.due_at <= now)
            .map(|(id, _)| id.clone())
            .collect();
        let mut due: Vec<(IssueId, RetryEntry)> = due_ids
            .into_iter()
            .map(|id| {
                let entry = self.entries.remove(&id).expect("just enumerated");
                (id, entry)
            })
            .collect();
        due.sort_by_key(|(_, e)| e.due_at);
        due
    }

    /// Iterate `(id, entry)` pairs without removing. Order
    /// unspecified; callers that need ordering should sort the result.
    pub fn iter(&self) -> impl Iterator<Item = (&IssueId, &RetryEntry)> {
        self.entries.iter()
    }
}

/// Compute the backoff [`Duration`] for a given attempt and reason.
///
/// Exposed for tests and for the poll loop's logging path; the queue
/// itself uses this internally during [`RetryQueue::schedule`].
#[must_use]
pub fn backoff_for(reason: RetryReason, attempt: u32, cfg: &RetryConfig) -> Duration {
    match reason {
        RetryReason::Continuation => cfg.continuation_delay,
        RetryReason::Failure => failure_backoff(attempt, cfg),
    }
}

/// `min(base * 2^(attempt - 1), max_backoff)`, computed without
/// overflowing `u128` for absurd attempt counts.
fn failure_backoff(attempt: u32, cfg: &RetryConfig) -> Duration {
    let base_ms = cfg.failure_base.as_millis();
    let cap_ms = cfg.max_backoff.as_millis();
    if attempt <= 1 {
        return Duration::from_millis(base_ms.min(cap_ms) as u64);
    }
    // Cap the shift exponent to keep 1u128 << shift well-defined and
    // far below the cap. `attempt - 1` is the doubling exponent. Once
    // `base_ms << exp >= cap_ms`, all higher attempts saturate to
    // `cap_ms` — so we can short-circuit at the first overshoot.
    let exp = attempt.saturating_sub(1);
    // 127 leaves headroom inside u128.
    let exp = exp.min(127);
    let shifted = base_ms.saturating_mul(1u128 << exp);
    let chosen = shifted.min(cap_ms);
    // chosen ≤ cap_ms ≤ u128, but cap_ms came from a Duration so it
    // fits in u64 ms after the .min.
    Duration::from_millis(chosen as u64)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn iid(s: &str) -> IssueId {
        IssueId::new(s)
    }

    fn cfg() -> RetryConfig {
        RetryConfig::defaults()
    }

    fn req<'a>(
        id: &str,
        identifier: &'a str,
        attempt: u32,
        reason: RetryReason,
        error: Option<String>,
        now: Instant,
    ) -> ScheduleRequest<'a> {
        ScheduleRequest {
            id: iid(id),
            identifier,
            attempt,
            reason,
            error,
            now,
        }
    }

    #[test]
    fn continuation_uses_fixed_short_delay() {
        let c = cfg();
        // Continuation delay is independent of attempt number.
        assert_eq!(
            backoff_for(RetryReason::Continuation, 1, &c),
            c.continuation_delay
        );
        assert_eq!(
            backoff_for(RetryReason::Continuation, 99, &c),
            c.continuation_delay
        );
    }

    #[test]
    fn failure_doubles_until_capped() {
        let c = cfg();
        // base = 10_000, cap = 300_000.
        assert_eq!(
            backoff_for(RetryReason::Failure, 1, &c),
            Duration::from_millis(10_000)
        );
        assert_eq!(
            backoff_for(RetryReason::Failure, 2, &c),
            Duration::from_millis(20_000)
        );
        assert_eq!(
            backoff_for(RetryReason::Failure, 3, &c),
            Duration::from_millis(40_000)
        );
        assert_eq!(
            backoff_for(RetryReason::Failure, 4, &c),
            Duration::from_millis(80_000)
        );
        assert_eq!(
            backoff_for(RetryReason::Failure, 5, &c),
            Duration::from_millis(160_000)
        );
        // 6th attempt would be 320_000ms, capped to 300_000.
        assert_eq!(
            backoff_for(RetryReason::Failure, 6, &c),
            Duration::from_millis(300_000)
        );
        // Astronomical attempts saturate cleanly, no panic, no overflow.
        assert_eq!(
            backoff_for(RetryReason::Failure, u32::MAX, &c),
            Duration::from_millis(300_000)
        );
    }

    #[test]
    fn failure_with_attempt_zero_treated_as_one() {
        // SPEC's `attempt` is 1-based; defensively, attempt=0 should
        // not produce a smaller-than-base backoff.
        let c = cfg();
        assert_eq!(
            backoff_for(RetryReason::Failure, 0, &c),
            Duration::from_millis(10_000)
        );
    }

    #[test]
    fn failure_cap_below_base_clamps_to_cap() {
        // Misconfiguration safety: if someone sets a max_backoff
        // smaller than the base, the very first attempt should clamp.
        let c = RetryConfig {
            max_backoff: Duration::from_millis(500),
            failure_base: Duration::from_millis(10_000),
            continuation_delay: Duration::from_millis(1_000),
        };
        assert_eq!(
            backoff_for(RetryReason::Failure, 1, &c),
            Duration::from_millis(500)
        );
        assert_eq!(
            backoff_for(RetryReason::Failure, 5, &c),
            Duration::from_millis(500)
        );
    }

    #[test]
    fn schedule_inserts_with_correct_due_at() {
        let mut q = RetryQueue::new();
        let now = Instant::now();
        let c = cfg();
        let entry = q
            .schedule(
                req(
                    "ENG-1",
                    "ENG-1",
                    1,
                    RetryReason::Failure,
                    Some("boom".into()),
                    now,
                ),
                &c,
            )
            .clone();
        assert_eq!(entry.attempt, 1);
        assert_eq!(entry.error.as_deref(), Some("boom"));
        assert_eq!(entry.reason, RetryReason::Failure);
        assert_eq!(entry.due_at, now + Duration::from_millis(10_000));
        assert_eq!(q.len(), 1);
    }

    #[test]
    fn schedule_replaces_existing_entry_silently() {
        let mut q = RetryQueue::new();
        let now = Instant::now();
        let c = cfg();
        q.schedule(
            req("ENG-1", "ENG-1", 1, RetryReason::Failure, None, now),
            &c,
        );
        // Re-schedule with a higher attempt; the old entry must be gone.
        q.schedule(
            req("ENG-1", "ENG-1", 3, RetryReason::Failure, None, now),
            &c,
        );
        assert_eq!(q.len(), 1);
        let e = q.get(&iid("ENG-1")).unwrap();
        assert_eq!(e.attempt, 3);
        assert_eq!(e.due_at, now + Duration::from_millis(40_000));
    }

    #[test]
    fn cancel_is_idempotent() {
        let mut q = RetryQueue::new();
        let now = Instant::now();
        let c = cfg();
        assert!(!q.cancel(&iid("absent")));
        q.schedule(
            req("ENG-1", "ENG-1", 1, RetryReason::Continuation, None, now),
            &c,
        );
        assert!(q.cancel(&iid("ENG-1")));
        assert!(!q.cancel(&iid("ENG-1")));
        assert!(q.is_empty());
    }

    #[test]
    fn drain_due_returns_only_due_entries_in_due_at_order() {
        let mut q = RetryQueue::new();
        let now = Instant::now();
        let c = cfg();
        // Three entries with staggered due times. Continuation = +1s,
        // Failure attempt 1 = +10s, attempt 2 = +20s.
        q.schedule(req("A", "A", 2, RetryReason::Failure, None, now), &c);
        q.schedule(req("B", "B", 1, RetryReason::Continuation, None, now), &c);
        q.schedule(req("C", "C", 1, RetryReason::Failure, None, now), &c);

        // At now + 5s: only the continuation (B) is due.
        let due = q.drain_due(now + Duration::from_secs(5));
        assert_eq!(due.len(), 1);
        assert_eq!(due[0].0, iid("B"));
        assert_eq!(q.len(), 2);

        // At now + 25s: both remaining are due, oldest-due first
        // (C's 10s before A's 20s).
        let due = q.drain_due(now + Duration::from_secs(25));
        assert_eq!(due.len(), 2);
        assert_eq!(due[0].0, iid("C"));
        assert_eq!(due[1].0, iid("A"));
        assert!(due[0].1.due_at <= due[1].1.due_at);
        assert!(q.is_empty());
    }

    #[test]
    fn schedule_with_policy_under_cap_inserts_and_returns_schedule_retry() {
        let mut q = RetryQueue::new();
        let now = Instant::now();
        let c = cfg();
        let decision = q.schedule_with_policy(
            req("ENG-1", "ENG-1", 1, RetryReason::Failure, None, now),
            &c,
            3,
        );
        match decision {
            RetryPolicyDecision::ScheduleRetry(entry) => {
                assert_eq!(entry.attempt, 1);
                assert_eq!(entry.due_at, now + Duration::from_millis(10_000));
                // Returned entry mirrors the queued entry.
                assert_eq!(q.get(&iid("ENG-1")), Some(&entry));
            }
            other => panic!("expected ScheduleRetry, got {other:?}"),
        }
        assert_eq!(q.len(), 1);
    }

    #[test]
    fn schedule_with_policy_at_cap_inserts_normally() {
        // attempt == max_retries is *within* budget (1-based).
        let mut q = RetryQueue::new();
        let now = Instant::now();
        let c = cfg();
        let decision = q.schedule_with_policy(
            req("ENG-1", "ENG-1", 3, RetryReason::Failure, None, now),
            &c,
            3,
        );
        assert!(matches!(decision, RetryPolicyDecision::ScheduleRetry(_)));
        assert_eq!(q.len(), 1);
        assert_eq!(q.get(&iid("ENG-1")).unwrap().attempt, 3);
    }

    #[test]
    fn schedule_with_policy_over_cap_returns_budget_exceeded_and_does_not_insert() {
        let mut q = RetryQueue::new();
        let now = Instant::now();
        let c = cfg();
        let decision = q.schedule_with_policy(
            req("ENG-1", "ENG-1", 4, RetryReason::Failure, None, now),
            &c,
            3,
        );
        assert_eq!(
            decision,
            RetryPolicyDecision::BudgetExceeded {
                observed: 4.0,
                cap: 3.0,
            }
        );
        assert!(q.is_empty());
        assert!(q.get(&iid("ENG-1")).is_none());
    }

    #[test]
    fn schedule_with_policy_over_cap_does_not_disturb_prior_entry() {
        let mut q = RetryQueue::new();
        let now = Instant::now();
        let c = cfg();
        // Land a legitimate prior entry at attempt=2.
        q.schedule_with_policy(
            req("ENG-1", "ENG-1", 2, RetryReason::Failure, None, now),
            &c,
            3,
        );
        let prior = q.get(&iid("ENG-1")).cloned().unwrap();

        // Over-cap call must not mutate the queue.
        let decision = q.schedule_with_policy(
            req("ENG-1", "ENG-1", 5, RetryReason::Failure, None, now),
            &c,
            3,
        );
        assert!(matches!(
            decision,
            RetryPolicyDecision::BudgetExceeded { .. }
        ));
        assert_eq!(q.len(), 1);
        assert_eq!(q.get(&iid("ENG-1")), Some(&prior));

        // Repeated over-cap calls are also no-ops on the queue.
        for attempt in [4, 6, 100, u32::MAX] {
            let _ = q.schedule_with_policy(
                req("ENG-1", "ENG-1", attempt, RetryReason::Failure, None, now),
                &c,
                3,
            );
            assert_eq!(q.len(), 1);
            assert_eq!(q.get(&iid("ENG-1")), Some(&prior));
        }
    }

    #[test]
    fn schedule_wrapper_ignores_cap_and_preserves_legacy_semantics() {
        // The pre-existing schedule() wrapper must keep behaving as a
        // cap-less insert, matching the migration note in the
        // checklist: callers that have not migrated keep their current
        // semantics.
        let mut q = RetryQueue::new();
        let now = Instant::now();
        let c = cfg();
        // attempt=999 would trip any sane cap, but schedule() ignores it.
        let entry = q
            .schedule(
                req("ENG-1", "ENG-1", 999, RetryReason::Failure, None, now),
                &c,
            )
            .clone();
        assert_eq!(entry.attempt, 999);
        assert_eq!(q.len(), 1);
        assert_eq!(q.get(&iid("ENG-1")).unwrap().attempt, 999);
    }

    #[test]
    fn drain_due_with_nothing_due_is_a_noop() {
        let mut q = RetryQueue::new();
        let now = Instant::now();
        let c = cfg();
        q.schedule(req("A", "A", 1, RetryReason::Failure, None, now), &c);
        let due = q.drain_due(now); // due_at is strictly in the future
        assert!(due.is_empty());
        assert_eq!(q.len(), 1);
    }
}
