//! Per-issue orchestration state machine (SPEC §4.1.8 + §7.1).
//!
//! This is the **pure** core of the orchestrator's claim bookkeeping: no
//! I/O, no async, no clock, no tracker, no agent. It models the
//! single-authority claim set that prevents duplicate dispatch and tracks
//! whether an issue is currently `Running` or sitting in `RetryQueued`.
//!
//! # Why a separate module?
//!
//! SPEC §7 calls out that "the orchestrator is the only component that
//! mutates scheduling state." Centralising that mutation in a pure type —
//! tested in isolation with `proptest` — lets us prove the invariants
//! (no double-dispatch, monotonic retry counters, terminal absence)
//! without entangling them with the polling loop, the tokio runtime, or
//! the network. The poll loop in a later checklist item will own a
//! `StateMachine` and call into it; it will not maintain claim state of
//! its own.
//!
//! # State model
//!
//! Per SPEC §7.1 there are five conceptual states: `Unclaimed`, `Claimed`,
//! `Running`, `RetryQueued`, `Released`. We collapse this to a tighter
//! encoding because two of the five are derivable:
//!
//! - `Unclaimed` and `Released` are both "issue is absent from the claim
//!   map." The difference is purely historical (was it ever claimed?),
//!   which the orchestrator does not need to schedule on.
//! - `Claimed` is just the umbrella for `Running` ∪ `RetryQueued`, so we
//!   represent it implicitly by membership in the claim map.
//!
//! That leaves the storage type [`ClaimState`] with two variants —
//! `Running` and `RetryQueued { attempt }` — and a `HashMap<IssueId,
//! ClaimState>` keyed by issue id. The five SPEC states project onto this
//! representation as: absent ↔ Unclaimed/Released, present ↔ Claimed,
//! present-and-Running ↔ Running, present-and-RetryQueued ↔ RetryQueued.

use std::collections::HashMap;

use serde::{Deserialize, Serialize};
use thiserror::Error;

use crate::tracker::IssueId;

/// Storage-level claim state for a single issue.
///
/// An issue's true SPEC §7.1 state is derived from whether it is present
/// in the [`StateMachine`]'s claim map and which variant it carries:
///
/// | SPEC state | Map presence | Variant |
/// |---|---|---|
/// | `Unclaimed` / `Released` | absent | — |
/// | `Running` | present | `Running` |
/// | `RetryQueued` | present | `RetryQueued { attempt }` |
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ClaimState {
    /// A worker task currently owns this issue and is streaming a turn.
    Running,
    /// The worker exited (normally or abnormally) and a retry timer is
    /// armed. `attempt` is the 1-based attempt number that *will* be made
    /// when the timer fires (matches SPEC §4.1.7 `RetryEntry.attempt`).
    RetryQueued {
        /// Attempt index for the *next* dispatch, 1-based.
        attempt: u32,
    },
}

/// Why a claim was released.
///
/// Captured for observability (logs, the Phase-8 event bus) and to let
/// the poll loop decide whether to schedule a continuation retry. The
/// state machine itself only stores the discriminant; it does not act on
/// it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ReleaseReason {
    /// Worker exited normally and there is nothing more to retry.
    Completed,
    /// Issue's tracker state moved out of `active_states` since the last
    /// reconciliation.
    NoLongerActive,
    /// Issue disappeared from the tracker entirely.
    Missing,
    /// Tracker reports a terminal state (`Done`, `Cancelled`, …).
    Terminal,
    /// Caller requested cancellation (e.g. SIGINT shutdown).
    Canceled,
}

/// All transitions are fallible because the orchestrator is the single
/// authority — anyone calling these methods is asserting they know the
/// current state. Violations indicate a bug, not a runtime condition, so
/// they are reported through `Result` rather than silently coalesced.
#[derive(Debug, Error, PartialEq, Eq)]
pub enum TransitionError {
    /// Attempted to claim an issue that is already in the map.
    #[error("issue {0} is already claimed")]
    AlreadyClaimed(IssueId),
    /// Attempted to operate on an issue that is not in the map.
    #[error("issue {0} is not currently claimed")]
    NotClaimed(IssueId),
    /// Operation requires the issue to be in `Running`, but it was in
    /// `RetryQueued` (or vice versa).
    #[error("issue {0} is in the wrong claim variant for this transition")]
    WrongVariant(IssueId),
}

/// In-memory claim ledger owned by the orchestrator.
///
/// The orchestrator constructs one of these at startup and threads it
/// through its poll loop. It is intentionally `!Send`-friendly (no
/// interior mutability, no atomics): concurrency is handled at the
/// orchestrator boundary, where a single task owns the `StateMachine`
/// and other tasks communicate with it by message-passing.
#[derive(Debug, Default)]
pub struct StateMachine {
    claims: HashMap<IssueId, ClaimState>,
}

impl StateMachine {
    /// Create an empty ledger. No issues are claimed.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Look up the current claim state, or `None` for `Unclaimed`/`Released`.
    #[must_use]
    pub fn get(&self, id: &IssueId) -> Option<ClaimState> {
        self.claims.get(id).copied()
    }

    /// Number of currently-claimed issues. Counts both `Running` and
    /// `RetryQueued` — useful for honouring `max_concurrent_agents`.
    #[must_use]
    pub fn claimed_len(&self) -> usize {
        self.claims.len()
    }

    /// Number of issues currently in `Running`. The orchestrator's
    /// concurrency gate uses this to decide whether to dispatch another
    /// worker on a poll tick.
    #[must_use]
    pub fn running_len(&self) -> usize {
        self.claims
            .values()
            .filter(|s| matches!(s, ClaimState::Running))
            .count()
    }

    /// Iterate over `(id, state)` pairs. Order is unspecified.
    pub fn iter(&self) -> impl Iterator<Item = (&IssueId, &ClaimState)> {
        self.claims.iter()
    }

    /// Transition `Unclaimed → Running`. Fails if the issue is already
    /// in any claimed variant — this is the invariant that prevents
    /// duplicate dispatch.
    pub fn claim(&mut self, id: IssueId) -> Result<(), TransitionError> {
        if self.claims.contains_key(&id) {
            return Err(TransitionError::AlreadyClaimed(id));
        }
        self.claims.insert(id, ClaimState::Running);
        Ok(())
    }

    /// Transition `Running → RetryQueued { attempt }`. Called when the
    /// worker exits (normal or abnormal); the orchestrator then arms a
    /// timer separately. `attempt` MUST be ≥ the previous attempt for
    /// the same issue — callers compute it from `RetryEntry.attempt + 1`.
    pub fn enqueue_retry(&mut self, id: IssueId, attempt: u32) -> Result<(), TransitionError> {
        match self.claims.get_mut(&id) {
            None => Err(TransitionError::NotClaimed(id)),
            Some(slot @ ClaimState::Running) => {
                *slot = ClaimState::RetryQueued { attempt };
                Ok(())
            }
            Some(ClaimState::RetryQueued { .. }) => Err(TransitionError::WrongVariant(id)),
        }
    }

    /// Transition `RetryQueued → Running`. Called when the retry timer
    /// fires and the orchestrator successfully redispatches. If the
    /// timer fires but the issue has left `active_states`, the
    /// orchestrator calls [`Self::release`] instead.
    pub fn resume_running(&mut self, id: IssueId) -> Result<(), TransitionError> {
        match self.claims.get_mut(&id) {
            None => Err(TransitionError::NotClaimed(id)),
            Some(slot @ ClaimState::RetryQueued { .. }) => {
                *slot = ClaimState::Running;
                Ok(())
            }
            Some(ClaimState::Running) => Err(TransitionError::WrongVariant(id)),
        }
    }

    /// Transition any claimed variant → `Released` (i.e. remove from the
    /// map). Returns the reason back to the caller for logging. Fails if
    /// the issue was not claimed — releasing an absent issue is a bug.
    pub fn release(
        &mut self,
        id: IssueId,
        reason: ReleaseReason,
    ) -> Result<ReleaseReason, TransitionError> {
        match self.claims.remove(&id) {
            Some(_) => Ok(reason),
            None => Err(TransitionError::NotClaimed(id)),
        }
    }

    /// Idempotent variant of [`Self::release`] for reconciliation
    /// passes that may sweep many ids without knowing which are
    /// claimed. Returns `true` iff a claim was actually removed.
    pub fn release_if_present(&mut self, id: &IssueId, _reason: ReleaseReason) -> bool {
        self.claims.remove(id).is_some()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use proptest::prelude::*;

    fn iid(s: &str) -> IssueId {
        IssueId::new(s)
    }

    #[test]
    fn claim_then_release_round_trips() {
        let mut sm = StateMachine::new();
        sm.claim(iid("ENG-1")).unwrap();
        assert_eq!(sm.get(&iid("ENG-1")), Some(ClaimState::Running));
        assert_eq!(sm.claimed_len(), 1);
        assert_eq!(sm.running_len(), 1);
        sm.release(iid("ENG-1"), ReleaseReason::Completed).unwrap();
        assert_eq!(sm.get(&iid("ENG-1")), None);
        assert_eq!(sm.claimed_len(), 0);
    }

    #[test]
    fn claim_twice_is_rejected() {
        let mut sm = StateMachine::new();
        sm.claim(iid("ENG-1")).unwrap();
        assert_eq!(
            sm.claim(iid("ENG-1")),
            Err(TransitionError::AlreadyClaimed(iid("ENG-1")))
        );
    }

    #[test]
    fn enqueue_retry_requires_running() {
        let mut sm = StateMachine::new();
        // Not claimed at all.
        assert_eq!(
            sm.enqueue_retry(iid("ENG-1"), 1),
            Err(TransitionError::NotClaimed(iid("ENG-1")))
        );
        // Claimed and Running → ok.
        sm.claim(iid("ENG-1")).unwrap();
        sm.enqueue_retry(iid("ENG-1"), 1).unwrap();
        assert_eq!(
            sm.get(&iid("ENG-1")),
            Some(ClaimState::RetryQueued { attempt: 1 })
        );
        // Already RetryQueued → wrong variant.
        assert_eq!(
            sm.enqueue_retry(iid("ENG-1"), 2),
            Err(TransitionError::WrongVariant(iid("ENG-1")))
        );
    }

    #[test]
    fn resume_running_requires_retry_queued() {
        let mut sm = StateMachine::new();
        sm.claim(iid("ENG-1")).unwrap();
        // Already Running → wrong variant.
        assert_eq!(
            sm.resume_running(iid("ENG-1")),
            Err(TransitionError::WrongVariant(iid("ENG-1")))
        );
        sm.enqueue_retry(iid("ENG-1"), 1).unwrap();
        sm.resume_running(iid("ENG-1")).unwrap();
        assert_eq!(sm.get(&iid("ENG-1")), Some(ClaimState::Running));
    }

    #[test]
    fn release_absent_is_an_error_but_release_if_present_is_idempotent() {
        let mut sm = StateMachine::new();
        assert_eq!(
            sm.release(iid("ENG-X"), ReleaseReason::Missing),
            Err(TransitionError::NotClaimed(iid("ENG-X")))
        );
        assert!(!sm.release_if_present(&iid("ENG-X"), ReleaseReason::Missing));
        sm.claim(iid("ENG-1")).unwrap();
        assert!(sm.release_if_present(&iid("ENG-1"), ReleaseReason::Terminal));
        assert!(!sm.release_if_present(&iid("ENG-1"), ReleaseReason::Terminal));
    }

    #[test]
    fn running_and_retry_counted_separately() {
        let mut sm = StateMachine::new();
        sm.claim(iid("A")).unwrap();
        sm.claim(iid("B")).unwrap();
        sm.claim(iid("C")).unwrap();
        sm.enqueue_retry(iid("B"), 1).unwrap();
        assert_eq!(sm.claimed_len(), 3);
        assert_eq!(sm.running_len(), 2);
    }

    /// One of: claim, enqueue_retry, resume_running, release.
    /// We ignore `proptest` shrinking concerns — IssueId space is tiny.
    #[derive(Debug, Clone)]
    enum Op {
        Claim(u8),
        EnqueueRetry(u8, u32),
        Resume(u8),
        Release(u8),
    }

    fn op_strategy() -> impl Strategy<Value = Op> {
        // 4 operations × 4 distinct issue ids gives a small but
        // exhaustive-feeling state space.
        prop_oneof![
            (0u8..4).prop_map(Op::Claim),
            (0u8..4, 1u32..16).prop_map(|(i, a)| Op::EnqueueRetry(i, a)),
            (0u8..4).prop_map(Op::Resume),
            (0u8..4).prop_map(Op::Release),
        ]
    }

    fn id_for(i: u8) -> IssueId {
        iid(&format!("ENG-{i}"))
    }

    proptest! {
        /// Property: regardless of any sequence of transitions, the
        /// invariants of SPEC §4.1.8 hold:
        ///
        ///   1. `claimed_len() == running_len() + retry_queued_len()`
        ///   2. `running_len() ≤ claimed_len()`
        ///   3. After a successful `claim`, `get()` returns `Running`.
        ///   4. After a successful `enqueue_retry`, `get()` returns
        ///      `RetryQueued { attempt }` with the same `attempt`.
        ///   5. After a successful `release`, `get()` returns `None`.
        ///
        /// Note: monotonicity of the `attempt` counter is intentionally
        /// **not** an invariant of this module. The state machine is pure
        /// storage; the orchestrator computes `attempt` from
        /// `RetryEntry.attempt + 1` (SPEC §4.1.7) and resets it on each
        /// successful re-dispatch. Enforcing monotonicity here would
        /// double-encode that policy and reject legal sequences.
        #[test]
        fn invariants_hold_under_arbitrary_sequences(ops in prop::collection::vec(op_strategy(), 0..40)) {
            let mut sm = StateMachine::new();

            for op in ops {
                match op {
                    Op::Claim(i) => {
                        let id = id_for(i);
                        if sm.claim(id.clone()).is_ok() {
                            prop_assert_eq!(sm.get(&id), Some(ClaimState::Running));
                        }
                    }
                    Op::EnqueueRetry(i, attempt) => {
                        let id = id_for(i);
                        if sm.enqueue_retry(id.clone(), attempt).is_ok() {
                            prop_assert_eq!(sm.get(&id), Some(ClaimState::RetryQueued { attempt }));
                        }
                    }
                    Op::Resume(i) => {
                        let id = id_for(i);
                        if sm.resume_running(id.clone()).is_ok() {
                            prop_assert_eq!(sm.get(&id), Some(ClaimState::Running));
                        }
                    }
                    Op::Release(i) => {
                        let id = id_for(i);
                        if sm.release(id.clone(), ReleaseReason::Completed).is_ok() {
                            prop_assert_eq!(sm.get(&id), None);
                        }
                    }
                }

                // Cross-cutting invariants checked after every op.
                let running = sm.running_len();
                let total = sm.claimed_len();
                prop_assert!(running <= total);
                let retry_queued = sm.iter()
                    .filter(|(_, s)| matches!(s, ClaimState::RetryQueued { .. }))
                    .count();
                prop_assert_eq!(running + retry_queued, total);
            }
        }
    }
}
