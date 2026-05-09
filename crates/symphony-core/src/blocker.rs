//! Domain model for blockers (SPEC v2 §4.8).
//!
//! A blocker is a *durable* dependency edge plus a reason: work item A
//! cannot move forward until work item B reaches a satisfying terminal
//! state (or the blocker is explicitly waived). Blockers are first-class
//! workflow objects, not free-form comments — the kernel reads them to
//! decide whether a parent may close, whether an integration may proceed,
//! and whether a draft PR may flip out of `draft` state.
//!
//! This module supplies only the *domain* shape:
//!
//! * [`BlockerId`], [`Blocker`] — the durable edge record;
//! * [`BlockerStatus`] — the open/resolved/waived/cancelled lifecycle;
//! * [`BlockerSeverity`] — operator-visible priority hint;
//! * [`BlockerOrigin`] — who filed it (run, agent, human);
//! * [`BlockerError`] — invariant violations surfaced by [`Blocker::try_new`]
//!   and [`Blocker::transition_status`].
//!
//! Persistence (the `work_item_edges` table with `edge_type = 'blocks'`)
//! lives in `symphony-state` and stores blockers structurally; this crate
//! enforces the runtime invariants the kernel branches on.

use serde::{Deserialize, Serialize};

use crate::role::RoleName;
use crate::work_item::WorkItemId;

/// Strongly-typed primary key for a [`Blocker`] record.
///
/// Matches the `i64` row id used by the `work_item_edges` table in
/// `symphony-state`. Kept as a separate Rust type from the storage row id
/// (per the same rationale as [`WorkItemId`]) so an accidental swap with
/// a work item id surfaces as a type error at the storage seam.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(transparent)]
pub struct BlockerId(pub i64);

impl BlockerId {
    /// Construct a [`BlockerId`] from a raw row id.
    pub fn new(id: i64) -> Self {
        Self(id)
    }

    /// Borrow the inner row id.
    pub fn get(self) -> i64 {
        self.0
    }
}

impl std::fmt::Display for BlockerId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        std::fmt::Display::fmt(&self.0, f)
    }
}

/// Domain-side reference to a `runs` row, used by [`BlockerOrigin::Run`].
///
/// Mirrors `symphony-state::RunId` without depending on it, by the same
/// rule as [`WorkItemId`] vs the storage row id.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(transparent)]
pub struct RunRef(pub i64);

impl RunRef {
    /// Construct a [`RunRef`] from a raw run row id.
    pub fn new(id: i64) -> Self {
        Self(id)
    }

    /// Borrow the inner row id.
    pub fn get(self) -> i64 {
        self.0
    }
}

impl std::fmt::Display for RunRef {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        std::fmt::Display::fmt(&self.0, f)
    }
}

/// Lifecycle of a blocker (SPEC v2 §4.8).
///
/// Transitions are linear: [`Self::Open`] is the only starting state, and
/// each terminal variant is sticky — the durable record preserves *why*
/// the blocker stopped blocking even after it stops gating progress.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum BlockerStatus {
    /// Active blocker. Blocks the parent per workflow policy.
    Open,
    /// The blocking item reached a satisfying state and the blocker no
    /// longer applies.
    Resolved,
    /// A configured waiver role intentionally bypassed the blocker. SPEC
    /// v2 §5.12 — `waived` MUST include a reason and a waiver role.
    Waived,
    /// The blocker was filed in error or rendered moot before resolution.
    Cancelled,
}

impl BlockerStatus {
    /// All variants in declaration order.
    pub const ALL: [Self; 4] = [Self::Open, Self::Resolved, Self::Waived, Self::Cancelled];

    /// Stable lowercase identifier used in YAML config and JSON payloads.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Open => "open",
            Self::Resolved => "resolved",
            Self::Waived => "waived",
            Self::Cancelled => "cancelled",
        }
    }

    /// True for statuses that no longer block progress.
    pub fn is_terminal(self) -> bool {
        !matches!(self, Self::Open)
    }

    /// True when a parent gate must continue waiting on this blocker.
    pub fn is_blocking(self) -> bool {
        matches!(self, Self::Open)
    }
}

impl std::fmt::Display for BlockerStatus {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

/// Operator-visible severity hint (SPEC v2 §4.8).
///
/// Severity does not change kernel gating — an `Open` blocker blocks
/// regardless of severity — but it surfaces in CLI/TUI output and shapes
/// retry/budget heuristics.
#[derive(
    Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Default, Serialize, Deserialize,
)]
#[serde(rename_all = "snake_case")]
pub enum BlockerSeverity {
    /// Nice-to-have follow-up that QA still flagged as gating.
    Low,
    /// Default severity when none is supplied.
    #[default]
    Medium,
    /// Likely user-visible regression or correctness issue.
    High,
    /// Data loss, security, or production-impacting issue.
    Critical,
}

impl BlockerSeverity {
    /// All variants in ascending severity order.
    pub const ALL: [Self; 4] = [Self::Low, Self::Medium, Self::High, Self::Critical];

    /// Stable lowercase identifier used in YAML config and JSON payloads.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Low => "low",
            Self::Medium => "medium",
            Self::High => "high",
            Self::Critical => "critical",
        }
    }
}

impl std::fmt::Display for BlockerSeverity {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

/// Identifier for the actor that filed a blocker.
///
/// SPEC v2 §4.8 requires recording who created a blocker. Three sources
/// matter for the kernel: an in-flight orchestrator run (most common),
/// an autonomous policy/agent decision outside a run, or an explicit
/// human operator (CLI/TUI).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum BlockerOrigin {
    /// Filed by an orchestrator run. The kernel can join back to
    /// `runs.id` for evidence and rerun routing.
    Run {
        /// The run that filed the blocker.
        run_id: RunRef,
        /// Role that owned the run, when known. Helps the kernel decide
        /// whether the blocker came from a QA gate (`required_for_done`
        /// applies) versus a specialist heads-up.
        role: Option<RoleName>,
    },
    /// Filed by automated policy outside an in-flight run (e.g. a
    /// recovery sweep that detected an inconsistency).
    Agent {
        /// Role kind/agent profile that took the action.
        role: RoleName,
        /// Free-form agent profile name from `WorkflowConfig::agents`.
        agent_profile: Option<String>,
    },
    /// Filed by a human operator (CLI/TUI/mutation API).
    Human {
        /// Operator-facing identifier (`username`, email, …).
        actor: String,
    },
}

impl BlockerOrigin {
    /// True when the origin carries a [`RoleName`] the kernel can use to
    /// decide gating policy (e.g. QA-filed blockers per SPEC §4.8).
    pub fn role(&self) -> Option<&RoleName> {
        match self {
            Self::Run { role, .. } => role.as_ref(),
            Self::Agent { role, .. } => Some(role),
            Self::Human { .. } => None,
        }
    }
}

/// Invariant violations for [`Blocker::try_new`] and
/// [`Blocker::transition_status`].
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum BlockerError {
    /// `blocking_id == blocked_id`. Self-blocks would prevent the work
    /// item from ever progressing and break propagation logic.
    #[error("blocker self-edge is forbidden (work item {0} cannot block itself)")]
    SelfEdge(WorkItemId),
    /// Blocker was constructed with an empty reason. SPEC §4.8 lists the
    /// reason as a required field.
    #[error("blocker reason must not be empty")]
    EmptyReason,
    /// Blocker was constructed with [`BlockerStatus::Waived`] but no
    /// reason or waiver role recorded. SPEC §5.12 requires both for a
    /// waived verdict; we extend the same rule to the underlying blocker.
    #[error("waived blocker must include both a waiver role and a reason")]
    InvalidWaiver,
    /// Attempted a status transition from a terminal state. Once a
    /// blocker is resolved/waived/cancelled, the durable record is
    /// frozen — operators must file a new blocker rather than mutate the
    /// terminal one.
    #[error("blocker {id} is already {current}; cannot transition to {requested}")]
    AlreadyTerminal {
        /// The blocker being mutated.
        id: BlockerId,
        /// The terminal status the blocker is currently in.
        current: BlockerStatus,
        /// The status the caller tried to set.
        requested: BlockerStatus,
    },
}

/// Durable blocker record (SPEC v2 §4.8).
///
/// Every required field from SPEC §4.8 is non-optional. Optional fields
/// (`severity` defaults to [`BlockerSeverity::Medium`], the resolution
/// metadata, the waiver role) live alongside the required surface.
///
/// Construct via [`Self::try_new`] so the edge invariants are enforced
/// once at creation time. Status transitions go through
/// [`Self::transition_status`] which preserves the "terminal is sticky"
/// rule.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Blocker {
    /// Durable id (matches the storage row id).
    pub id: BlockerId,
    /// Work item that *causes* the block.
    pub blocking_id: WorkItemId,
    /// Work item that is *waiting* on the blocker.
    pub blocked_id: WorkItemId,
    /// Operator-facing reason. Required and non-empty.
    pub reason: String,
    /// Who filed the blocker.
    pub origin: BlockerOrigin,
    /// Operator-visible severity hint.
    pub severity: BlockerSeverity,
    /// Lifecycle status. Open at creation; transitions are linear.
    pub status: BlockerStatus,
    /// Role that waived the blocker, when [`BlockerStatus::Waived`].
    /// SPEC §5.12 requires this be a member of `qa.waiver_roles`; this
    /// crate only checks that *some* role was recorded.
    pub waiver_role: Option<RoleName>,
    /// Free-form note recorded at terminal transition (resolution
    /// summary, waiver justification, cancellation reason).
    pub resolution_note: Option<String>,
}

impl Blocker {
    /// Construct an [`BlockerStatus::Open`] blocker, enforcing the
    /// no-self-edge and non-empty-reason invariants.
    pub fn try_new(
        id: BlockerId,
        blocking_id: WorkItemId,
        blocked_id: WorkItemId,
        reason: impl Into<String>,
        origin: BlockerOrigin,
        severity: BlockerSeverity,
    ) -> Result<Self, BlockerError> {
        if blocking_id == blocked_id {
            return Err(BlockerError::SelfEdge(blocking_id));
        }
        let reason = reason.into();
        if reason.trim().is_empty() {
            return Err(BlockerError::EmptyReason);
        }
        Ok(Self {
            id,
            blocking_id,
            blocked_id,
            reason,
            origin,
            severity,
            status: BlockerStatus::Open,
            waiver_role: None,
            resolution_note: None,
        })
    }

    /// True when this blocker still gates the parent.
    pub fn is_blocking(&self) -> bool {
        self.status.is_blocking()
    }

    /// Transition to a terminal status, enforcing the "terminal is
    /// sticky" rule.
    ///
    /// * [`BlockerStatus::Resolved`] / [`BlockerStatus::Cancelled`]:
    ///   `note` becomes [`Self::resolution_note`].
    /// * [`BlockerStatus::Waived`]: requires a non-empty `note` *and* a
    ///   waiver role; SPEC §5.12.
    /// * [`BlockerStatus::Open`]: not a valid target — blockers cannot
    ///   re-open through this API; file a new blocker instead.
    pub fn transition_status(
        &mut self,
        target: BlockerStatus,
        waiver_role: Option<RoleName>,
        note: Option<String>,
    ) -> Result<(), BlockerError> {
        if self.status.is_terminal() {
            return Err(BlockerError::AlreadyTerminal {
                id: self.id,
                current: self.status,
                requested: target,
            });
        }
        match target {
            BlockerStatus::Open => {
                return Err(BlockerError::AlreadyTerminal {
                    id: self.id,
                    current: self.status,
                    requested: target,
                });
            }
            BlockerStatus::Waived => {
                let has_note = note.as_ref().is_some_and(|n| !n.trim().is_empty());
                if waiver_role.is_none() || !has_note {
                    return Err(BlockerError::InvalidWaiver);
                }
                self.waiver_role = waiver_role;
                self.resolution_note = note;
            }
            BlockerStatus::Resolved | BlockerStatus::Cancelled => {
                self.resolution_note = note;
            }
        }
        self.status = target;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn run_origin() -> BlockerOrigin {
        BlockerOrigin::Run {
            run_id: RunRef::new(7),
            role: Some(RoleName::new("qa")),
        }
    }

    fn open_blocker() -> Blocker {
        Blocker::try_new(
            BlockerId::new(1),
            WorkItemId::new(10),
            WorkItemId::new(20),
            "missing migration",
            run_origin(),
            BlockerSeverity::High,
        )
        .unwrap()
    }

    #[test]
    fn blocker_status_round_trips_through_serde_for_every_variant() {
        for status in BlockerStatus::ALL {
            let json = serde_json::to_string(&status).unwrap();
            assert_eq!(json, format!("\"{}\"", status.as_str()));
            let back: BlockerStatus = serde_json::from_str(&json).unwrap();
            assert_eq!(back, status);
        }
    }

    #[test]
    fn blocker_status_partitions_into_blocking_and_terminal() {
        for status in BlockerStatus::ALL {
            assert_eq!(status.is_blocking(), !status.is_terminal());
        }
        assert!(BlockerStatus::Open.is_blocking());
        assert!(BlockerStatus::Resolved.is_terminal());
        assert!(BlockerStatus::Waived.is_terminal());
        assert!(BlockerStatus::Cancelled.is_terminal());
    }

    #[test]
    fn blocker_severity_default_is_medium() {
        assert_eq!(BlockerSeverity::default(), BlockerSeverity::Medium);
    }

    #[test]
    fn blocker_origin_role_extracts_role_for_run_and_agent() {
        let run = run_origin();
        assert_eq!(run.role(), Some(&RoleName::new("qa")));

        let agent = BlockerOrigin::Agent {
            role: RoleName::new("recovery"),
            agent_profile: None,
        };
        assert_eq!(agent.role(), Some(&RoleName::new("recovery")));

        let human = BlockerOrigin::Human {
            actor: "alice".into(),
        };
        assert_eq!(human.role(), None);
    }

    #[test]
    fn try_new_rejects_self_edge() {
        let err = Blocker::try_new(
            BlockerId::new(1),
            WorkItemId::new(5),
            WorkItemId::new(5),
            "loop",
            run_origin(),
            BlockerSeverity::Medium,
        )
        .unwrap_err();
        assert_eq!(err, BlockerError::SelfEdge(WorkItemId::new(5)));
    }

    #[test]
    fn try_new_rejects_empty_reason_including_whitespace() {
        let cases = ["", "   ", "\n\t"];
        for reason in cases {
            let err = Blocker::try_new(
                BlockerId::new(1),
                WorkItemId::new(10),
                WorkItemId::new(20),
                reason,
                run_origin(),
                BlockerSeverity::Medium,
            )
            .unwrap_err();
            assert_eq!(err, BlockerError::EmptyReason);
        }
    }

    #[test]
    fn try_new_starts_open_with_no_waiver_or_resolution() {
        let b = open_blocker();
        assert_eq!(b.status, BlockerStatus::Open);
        assert!(b.is_blocking());
        assert!(b.waiver_role.is_none());
        assert!(b.resolution_note.is_none());
    }

    #[test]
    fn transition_to_resolved_sets_note_and_clears_blocking() {
        let mut b = open_blocker();
        b.transition_status(BlockerStatus::Resolved, None, Some("fixed in #42".into()))
            .unwrap();
        assert_eq!(b.status, BlockerStatus::Resolved);
        assert!(!b.is_blocking());
        assert_eq!(b.resolution_note.as_deref(), Some("fixed in #42"));
        assert!(b.waiver_role.is_none());
    }

    #[test]
    fn transition_to_cancelled_records_note() {
        let mut b = open_blocker();
        b.transition_status(
            BlockerStatus::Cancelled,
            None,
            Some("filed in error".into()),
        )
        .unwrap();
        assert_eq!(b.status, BlockerStatus::Cancelled);
        assert_eq!(b.resolution_note.as_deref(), Some("filed in error"));
    }

    #[test]
    fn transition_to_waived_requires_role_and_note() {
        let mut b = open_blocker();
        let err = b
            .transition_status(BlockerStatus::Waived, None, Some("ok".into()))
            .unwrap_err();
        assert_eq!(err, BlockerError::InvalidWaiver);
        assert_eq!(b.status, BlockerStatus::Open, "failed transition is no-op");

        let err = b
            .transition_status(BlockerStatus::Waived, Some(RoleName::new("lead")), None)
            .unwrap_err();
        assert_eq!(err, BlockerError::InvalidWaiver);

        let err = b
            .transition_status(
                BlockerStatus::Waived,
                Some(RoleName::new("lead")),
                Some("   ".into()),
            )
            .unwrap_err();
        assert_eq!(err, BlockerError::InvalidWaiver);

        b.transition_status(
            BlockerStatus::Waived,
            Some(RoleName::new("lead")),
            Some("accepted risk".into()),
        )
        .unwrap();
        assert_eq!(b.status, BlockerStatus::Waived);
        assert_eq!(b.waiver_role.as_ref().unwrap().as_str(), "lead");
        assert_eq!(b.resolution_note.as_deref(), Some("accepted risk"));
    }

    #[test]
    fn transition_from_terminal_is_rejected() {
        let mut b = open_blocker();
        b.transition_status(BlockerStatus::Resolved, None, None)
            .unwrap();
        let err = b
            .transition_status(BlockerStatus::Cancelled, None, None)
            .unwrap_err();
        assert!(matches!(
            err,
            BlockerError::AlreadyTerminal {
                current: BlockerStatus::Resolved,
                requested: BlockerStatus::Cancelled,
                ..
            }
        ));
    }

    #[test]
    fn cannot_transition_to_open() {
        let mut b = open_blocker();
        let err = b
            .transition_status(BlockerStatus::Open, None, None)
            .unwrap_err();
        assert!(matches!(
            err,
            BlockerError::AlreadyTerminal {
                current: BlockerStatus::Open,
                requested: BlockerStatus::Open,
                ..
            }
        ));
    }

    #[test]
    fn blocker_round_trips_through_json_with_all_origin_variants() {
        let cases = [
            run_origin(),
            BlockerOrigin::Agent {
                role: RoleName::new("recovery"),
                agent_profile: Some("hermes".into()),
            },
            BlockerOrigin::Human {
                actor: "alice".into(),
            },
        ];
        for origin in cases {
            let b = Blocker::try_new(
                BlockerId::new(2),
                WorkItemId::new(11),
                WorkItemId::new(22),
                "needs schema sign-off",
                origin,
                BlockerSeverity::Critical,
            )
            .unwrap();
            let json = serde_json::to_string(&b).unwrap();
            let back: Blocker = serde_json::from_str(&json).unwrap();
            assert_eq!(b, back);
        }
    }

    #[test]
    fn blocker_id_round_trips_through_serde_as_a_bare_integer() {
        let id = BlockerId::new(99);
        let json = serde_json::to_string(&id).unwrap();
        assert_eq!(json, "99");
        let back: BlockerId = serde_json::from_str(&json).unwrap();
        assert_eq!(back, id);
    }
}
