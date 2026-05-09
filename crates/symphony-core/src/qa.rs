//! Domain model for QA verdicts (SPEC v2 §4.9, §5.12).
//!
//! A QA verdict is the durable, gate-bearing result of a QA run. Unlike
//! a free-form comment, a verdict carries:
//!
//! * a typed [`QaVerdict`] outcome the kernel branches on;
//! * structured [`QaEvidence`] satisfying `qa.evidence_required`;
//! * an [`AcceptanceCriterionTrace`] vector that proves *which* acceptance
//!   criteria were exercised and how;
//! * a list of [`crate::BlockerId`]s the verdict filed (so closeout gating
//!   can find them later);
//! * waiver metadata when the verdict is [`QaVerdict::Waived`].
//!
//! This module supplies only the *domain* shape. Persistence lives in
//! `symphony-state` (the `qa_verdicts` table); the kernel uses [`QaOutcome`]
//! to enforce shape invariants once at the construction seam, the same
//! pattern as [`crate::Blocker::try_new`].
//!
//! The five verdicts in [`QaVerdict::ALL`] mirror SPEC §4.9. The current
//! `qa_verdicts` schema (Phase 2) collapses verdicts into three storage
//! states; the Phase 9 wiring widens the storage surface to match this
//! domain. Until then, callers map [`QaVerdict`] → storage at the
//! repository seam — keeping the typed shape stable across that change.

use serde::{Deserialize, Serialize};

use crate::blocker::{BlockerId, RunRef};
use crate::role::RoleName;
use crate::work_item::WorkItemId;

/// Strongly-typed primary key for a [`QaOutcome`] row.
///
/// Matches the `i64` row id used by the `qa_verdicts` table in
/// `symphony-state`. Newtyped to keep an accidental swap with a work item
/// id or run id a type error rather than a runtime mystery.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(transparent)]
pub struct QaVerdictId(pub i64);

impl QaVerdictId {
    /// Construct a [`QaVerdictId`] from a raw row id.
    pub fn new(id: i64) -> Self {
        Self(id)
    }

    /// Borrow the inner row id.
    pub fn get(self) -> i64 {
        self.0
    }
}

impl std::fmt::Display for QaVerdictId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        std::fmt::Display::fmt(&self.0, f)
    }
}

/// QA outcome (SPEC v2 §4.9).
///
/// `passed` is the only success state. The two failure variants are kept
/// distinct because they route differently:
///
/// * [`Self::FailedWithBlockers`] requires at least one filed blocker; the
///   kernel uses those blockers for routing and parent-closeout gating.
/// * [`Self::FailedNeedsRework`] signals "QA rejects without a structural
///   blocker" — the kernel routes back to the prior role for repair without
///   a durable edge.
///
/// [`Self::Inconclusive`] keeps the work item in a QA-pending state; the
/// kernel does not treat it as a pass.
///
/// [`Self::Waived`] is valid only when produced by a role listed in
/// `qa.waiver_roles` and accompanied by a non-empty reason. SPEC §5.12.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum QaVerdict {
    /// QA verified all acceptance criteria; the work item may proceed.
    Passed,
    /// QA rejected the work and filed at least one blocker. The blockers
    /// are recorded in [`QaOutcome::blockers_created`] and gate parent
    /// completion per SPEC §8.2.
    FailedWithBlockers,
    /// QA rejected the work without a structural blocker. The kernel
    /// routes back to the prior role for rework.
    FailedNeedsRework,
    /// QA could not reach a decision (missing evidence, environment
    /// failure, …). The work item stays in a QA-pending state.
    Inconclusive,
    /// A configured waiver role intentionally accepted a non-passing
    /// outcome. SPEC §5.12 — requires a waiver role and a reason.
    Waived,
}

impl QaVerdict {
    /// All variants in declaration order.
    pub const ALL: [Self; 5] = [
        Self::Passed,
        Self::FailedWithBlockers,
        Self::FailedNeedsRework,
        Self::Inconclusive,
        Self::Waived,
    ];

    /// Stable lowercase identifier used in YAML config and JSON payloads.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Passed => "passed",
            Self::FailedWithBlockers => "failed_with_blockers",
            Self::FailedNeedsRework => "failed_needs_rework",
            Self::Inconclusive => "inconclusive",
            Self::Waived => "waived",
        }
    }

    /// True for verdicts that allow the parent gate to proceed.
    ///
    /// Only [`Self::Passed`] and [`Self::Waived`] satisfy the QA gate.
    /// SPEC §8.1.
    pub fn is_pass(self) -> bool {
        matches!(self, Self::Passed | Self::Waived)
    }

    /// True for verdicts that route back for rework or remain pending.
    pub fn is_failure(self) -> bool {
        matches!(self, Self::FailedWithBlockers | Self::FailedNeedsRework)
    }

    /// True when the variant requires at least one filed blocker. Only
    /// [`Self::FailedWithBlockers`] does — other failures may still attach
    /// blockers but are not required to.
    pub fn requires_blockers(self) -> bool {
        matches!(self, Self::FailedWithBlockers)
    }

    /// True when the variant requires a waiver role and reason.
    pub fn requires_waiver(self) -> bool {
        matches!(self, Self::Waived)
    }
}

impl std::fmt::Display for QaVerdict {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

/// Per-criterion verification status (SPEC v2 §4.9).
///
/// QA must trace every acceptance criterion (SPEC §6.5). The status set is
/// kept small and orthogonal so verdicts compose cleanly: [`Self::Verified`]
/// or [`Self::NotApplicable`] are passing states; [`Self::Failed`] and
/// [`Self::NotVerified`] are non-passing.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AcceptanceCriterionStatus {
    /// QA exercised the criterion and it held.
    Verified,
    /// QA exercised the criterion and it failed.
    Failed,
    /// The criterion does not apply to this work item (e.g. UI criterion
    /// on a backend-only change). Reason should be recorded in
    /// [`AcceptanceCriterionTrace::notes`].
    NotApplicable,
    /// QA could not exercise the criterion (missing harness, blocked
    /// dependency). Counts as non-passing for gating.
    NotVerified,
}

impl AcceptanceCriterionStatus {
    /// All variants in declaration order.
    pub const ALL: [Self; 4] = [
        Self::Verified,
        Self::Failed,
        Self::NotApplicable,
        Self::NotVerified,
    ];

    /// Stable lowercase identifier.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Verified => "verified",
            Self::Failed => "failed",
            Self::NotApplicable => "not_applicable",
            Self::NotVerified => "not_verified",
        }
    }

    /// True when the status counts as a pass for QA gating.
    pub fn is_passing(self) -> bool {
        matches!(self, Self::Verified | Self::NotApplicable)
    }
}

impl std::fmt::Display for AcceptanceCriterionStatus {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

/// One row of the acceptance-criteria trace (SPEC v2 §6.5).
///
/// Every acceptance criterion on the work item should appear in
/// [`QaOutcome::acceptance_trace`] exactly once. The criterion text is
/// the operator-facing string copied verbatim from the work item so the
/// trace remains readable even if the source criterion is later edited.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AcceptanceCriterionTrace {
    /// Operator-facing criterion text. Required and non-empty.
    pub criterion: String,
    /// Verification status.
    pub status: AcceptanceCriterionStatus,
    /// Pointers to the evidence that supports the status (test names,
    /// log/screenshot paths, PR comments, …).
    #[serde(default)]
    pub evidence: Vec<String>,
    /// Optional free-form notes (why `not_applicable`, what failed, …).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub notes: Option<String>,
}

/// Structured QA evidence (SPEC v2 §5.12 `evidence_required`).
///
/// Mirrors the `evidence_required` substructure in workflow config:
/// every required category has a slot here so the kernel can verify
/// presence without parsing free-form text. UI/TUI runs SHOULD populate
/// [`Self::runtime_evidence`]; the workflow MAY accept
/// [`Self::accepted_limitation`] in lieu of runtime evidence per
/// SPEC §4.9.
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct QaEvidence {
    /// Test commands QA executed (or attempted) against the integration
    /// branch/worktree.
    #[serde(default)]
    pub tests_run: Vec<String>,
    /// Files QA reviewed in the changed-files surface.
    #[serde(default)]
    pub changed_files_reviewed: Vec<String>,
    /// CI/check status references QA recorded as evidence (e.g. workflow
    /// run URLs, exit codes).
    #[serde(default)]
    pub ci_status: Vec<String>,
    /// Visual/runtime evidence pointers (screenshots, terminal recordings,
    /// harness logs, manual exercise notes).
    #[serde(default)]
    pub runtime_evidence: Vec<String>,
    /// Free-form additional notes the verdict author wants persisted.
    #[serde(default)]
    pub notes: Vec<String>,
    /// Explicit workflow-approved limitation that excuses missing
    /// runtime evidence. SPEC §4.9 — must be set when QA cannot capture
    /// runtime output for runtime-sensitive work.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub accepted_limitation: Option<String>,
}

impl QaEvidence {
    /// True when every category is empty (no evidence at all). Useful for
    /// the kernel to detect verdicts that require evidence per
    /// `qa.evidence_required` but supplied none.
    pub fn is_empty(&self) -> bool {
        self.tests_run.is_empty()
            && self.changed_files_reviewed.is_empty()
            && self.ci_status.is_empty()
            && self.runtime_evidence.is_empty()
            && self.notes.is_empty()
            && self
                .accepted_limitation
                .as_deref()
                .is_none_or(str::is_empty)
    }
}

/// Invariant violations for [`QaOutcome::try_new`].
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum QaError {
    /// `failed_with_blockers` verdict was constructed without any
    /// referenced blockers. SPEC §4.9.
    #[error("verdict failed_with_blockers requires at least one blocker reference")]
    MissingBlockers,
    /// A non-`failed_with_blockers` verdict carried blocker references.
    /// We reject the mismatch so the routing semantics remain unambiguous;
    /// other failure variants attach blockers via the durable
    /// [`crate::Blocker`] table directly, not through the verdict.
    #[error("verdict {verdict} must not carry blockers_created (got {count})")]
    UnexpectedBlockers {
        /// The verdict that was constructed.
        verdict: QaVerdict,
        /// How many blocker ids were attached.
        count: usize,
    },
    /// `waived` verdict was constructed without a waiver role and reason.
    /// SPEC §5.12 requires both.
    #[error("waived verdict requires both a waiver role and a non-empty reason")]
    InvalidWaiver,
    /// A non-`waived` verdict carried waiver metadata. Prevents silent
    /// mis-routing where the kernel would treat the verdict as waived.
    #[error("verdict {0} must not carry waiver metadata")]
    UnexpectedWaiver(QaVerdict),
    /// An [`AcceptanceCriterionTrace::criterion`] was empty.
    #[error("acceptance criterion text must not be empty")]
    EmptyCriterion,
    /// A `passed` verdict was constructed but at least one acceptance
    /// criterion was not in a passing state. The kernel cannot accept a
    /// pass that left criteria failed/unverified.
    #[error("verdict passed requires every acceptance criterion to be verified or not_applicable")]
    PassWithUnpassedCriterion,
}

/// Durable QA verdict record (SPEC v2 §4.9).
///
/// The struct mirrors the `qa_verdicts` row plus the blocker-reference
/// list and the waiver metadata required by SPEC §5.12. Construct via
/// [`Self::try_new`] so the verdict-vs-evidence invariants are enforced
/// once at creation time.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct QaOutcome {
    /// Durable id (matches the storage row id).
    pub id: QaVerdictId,
    /// The work item the verdict gated.
    pub work_item_id: WorkItemId,
    /// The QA run that emitted the verdict.
    pub run_id: RunRef,
    /// The role that authored the verdict (typically the `qa_gate` role).
    pub role: RoleName,
    /// The typed outcome.
    pub verdict: QaVerdict,
    /// Structured evidence the verdict relied on.
    pub evidence: QaEvidence,
    /// One trace row per acceptance criterion the work item declared.
    /// May be empty when the work item has no acceptance criteria.
    pub acceptance_trace: Vec<AcceptanceCriterionTrace>,
    /// Blockers QA filed alongside the verdict. Required for
    /// [`QaVerdict::FailedWithBlockers`]; forbidden for other variants.
    #[serde(default)]
    pub blockers_created: Vec<BlockerId>,
    /// Role that authored a [`QaVerdict::Waived`] verdict. Must be a
    /// member of `qa.waiver_roles`; this crate only checks presence.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub waiver_role: Option<RoleName>,
    /// Operator-facing reason. Required for [`QaVerdict::Waived`] and for
    /// every non-passing verdict so the durable record explains itself.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
}

impl QaOutcome {
    /// Construct a verdict, enforcing the verdict-vs-evidence invariants.
    ///
    /// * `failed_with_blockers` requires at least one blocker id and
    ///   forbids waiver metadata.
    /// * `waived` requires a waiver role and a non-empty reason and
    ///   forbids blocker references.
    /// * `passed` requires every supplied acceptance trace row to be in a
    ///   passing state ([`AcceptanceCriterionStatus::is_passing`]).
    /// * Every acceptance trace row must have non-empty `criterion` text.
    #[allow(clippy::too_many_arguments)]
    pub fn try_new(
        id: QaVerdictId,
        work_item_id: WorkItemId,
        run_id: RunRef,
        role: RoleName,
        verdict: QaVerdict,
        evidence: QaEvidence,
        acceptance_trace: Vec<AcceptanceCriterionTrace>,
        blockers_created: Vec<BlockerId>,
        waiver_role: Option<RoleName>,
        reason: Option<String>,
    ) -> Result<Self, QaError> {
        for trace in &acceptance_trace {
            if trace.criterion.trim().is_empty() {
                return Err(QaError::EmptyCriterion);
            }
        }

        match verdict {
            QaVerdict::FailedWithBlockers => {
                if blockers_created.is_empty() {
                    return Err(QaError::MissingBlockers);
                }
            }
            QaVerdict::Passed => {
                if !blockers_created.is_empty() {
                    return Err(QaError::UnexpectedBlockers {
                        verdict,
                        count: blockers_created.len(),
                    });
                }
                if acceptance_trace.iter().any(|t| !t.status.is_passing()) {
                    return Err(QaError::PassWithUnpassedCriterion);
                }
            }
            QaVerdict::FailedNeedsRework | QaVerdict::Inconclusive | QaVerdict::Waived => {
                if !blockers_created.is_empty() {
                    return Err(QaError::UnexpectedBlockers {
                        verdict,
                        count: blockers_created.len(),
                    });
                }
            }
        }

        let has_reason = reason.as_deref().is_some_and(|r| !r.trim().is_empty());
        if verdict.requires_waiver() {
            if waiver_role.is_none() || !has_reason {
                return Err(QaError::InvalidWaiver);
            }
        } else if waiver_role.is_some() {
            return Err(QaError::UnexpectedWaiver(verdict));
        }

        Ok(Self {
            id,
            work_item_id,
            run_id,
            role,
            verdict,
            evidence,
            acceptance_trace,
            blockers_created,
            waiver_role,
            reason,
        })
    }

    /// True when the verdict satisfies the QA gate for the work item.
    pub fn is_pass(&self) -> bool {
        self.verdict.is_pass()
    }

    /// Iterate the acceptance trace rows that were not in a passing state.
    pub fn unpassed_criteria(&self) -> impl Iterator<Item = &AcceptanceCriterionTrace> {
        self.acceptance_trace
            .iter()
            .filter(|t| !t.status.is_passing())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn passing_trace() -> AcceptanceCriterionTrace {
        AcceptanceCriterionTrace {
            criterion: "tests pass".into(),
            status: AcceptanceCriterionStatus::Verified,
            evidence: vec!["cargo test --workspace".into()],
            notes: None,
        }
    }

    fn minimal_evidence() -> QaEvidence {
        QaEvidence {
            tests_run: vec!["cargo test".into()],
            changed_files_reviewed: vec!["src/foo.rs".into()],
            ..QaEvidence::default()
        }
    }

    fn pass(id: i64) -> QaOutcome {
        QaOutcome::try_new(
            QaVerdictId::new(id),
            WorkItemId::new(10),
            RunRef::new(7),
            RoleName::new("qa"),
            QaVerdict::Passed,
            minimal_evidence(),
            vec![passing_trace()],
            vec![],
            None,
            None,
        )
        .unwrap()
    }

    #[test]
    fn qa_verdict_round_trips_through_serde_for_every_variant() {
        for verdict in QaVerdict::ALL {
            let json = serde_json::to_string(&verdict).unwrap();
            assert_eq!(json, format!("\"{}\"", verdict.as_str()));
            let back: QaVerdict = serde_json::from_str(&json).unwrap();
            assert_eq!(back, verdict);
        }
    }

    #[test]
    fn qa_verdict_partitions_pass_failure_correctly() {
        assert!(QaVerdict::Passed.is_pass());
        assert!(QaVerdict::Waived.is_pass());
        assert!(QaVerdict::FailedWithBlockers.is_failure());
        assert!(QaVerdict::FailedNeedsRework.is_failure());
        assert!(!QaVerdict::Inconclusive.is_pass());
        assert!(!QaVerdict::Inconclusive.is_failure());
        assert!(QaVerdict::FailedWithBlockers.requires_blockers());
        assert!(!QaVerdict::Passed.requires_blockers());
        assert!(QaVerdict::Waived.requires_waiver());
        assert!(!QaVerdict::Passed.requires_waiver());
    }

    #[test]
    fn acceptance_status_round_trips_and_partitions_passing() {
        for status in AcceptanceCriterionStatus::ALL {
            let json = serde_json::to_string(&status).unwrap();
            assert_eq!(json, format!("\"{}\"", status.as_str()));
            let back: AcceptanceCriterionStatus = serde_json::from_str(&json).unwrap();
            assert_eq!(back, status);
        }
        assert!(AcceptanceCriterionStatus::Verified.is_passing());
        assert!(AcceptanceCriterionStatus::NotApplicable.is_passing());
        assert!(!AcceptanceCriterionStatus::Failed.is_passing());
        assert!(!AcceptanceCriterionStatus::NotVerified.is_passing());
    }

    #[test]
    fn qa_evidence_is_empty_detects_total_absence() {
        assert!(QaEvidence::default().is_empty());
        assert!(!minimal_evidence().is_empty());
        let only_limitation = QaEvidence {
            accepted_limitation: Some("no display available".into()),
            ..QaEvidence::default()
        };
        assert!(!only_limitation.is_empty());
    }

    #[test]
    fn passed_verdict_constructs_with_only_passing_criteria() {
        let outcome = pass(1);
        assert!(outcome.is_pass());
        assert_eq!(outcome.unpassed_criteria().count(), 0);
    }

    #[test]
    fn passed_verdict_rejects_unpassed_criterion() {
        let trace = AcceptanceCriterionTrace {
            criterion: "ui renders".into(),
            status: AcceptanceCriterionStatus::Failed,
            evidence: vec![],
            notes: None,
        };
        let err = QaOutcome::try_new(
            QaVerdictId::new(1),
            WorkItemId::new(10),
            RunRef::new(7),
            RoleName::new("qa"),
            QaVerdict::Passed,
            minimal_evidence(),
            vec![passing_trace(), trace],
            vec![],
            None,
            None,
        )
        .unwrap_err();
        assert_eq!(err, QaError::PassWithUnpassedCriterion);
    }

    #[test]
    fn failed_with_blockers_requires_blocker_references() {
        let err = QaOutcome::try_new(
            QaVerdictId::new(1),
            WorkItemId::new(10),
            RunRef::new(7),
            RoleName::new("qa"),
            QaVerdict::FailedWithBlockers,
            minimal_evidence(),
            vec![],
            vec![],
            None,
            Some("tests fail".into()),
        )
        .unwrap_err();
        assert_eq!(err, QaError::MissingBlockers);

        QaOutcome::try_new(
            QaVerdictId::new(1),
            WorkItemId::new(10),
            RunRef::new(7),
            RoleName::new("qa"),
            QaVerdict::FailedWithBlockers,
            minimal_evidence(),
            vec![],
            vec![BlockerId::new(42)],
            None,
            Some("tests fail".into()),
        )
        .expect("blockers attached should be accepted");
    }

    #[test]
    fn non_failed_with_blockers_rejects_attached_blockers() {
        for verdict in [
            QaVerdict::Passed,
            QaVerdict::FailedNeedsRework,
            QaVerdict::Inconclusive,
            QaVerdict::Waived,
        ] {
            let waiver = if verdict == QaVerdict::Waived {
                Some(RoleName::new("platform_lead"))
            } else {
                None
            };
            let reason = if verdict == QaVerdict::Waived {
                Some("accepted risk".into())
            } else {
                None
            };
            let err = QaOutcome::try_new(
                QaVerdictId::new(1),
                WorkItemId::new(10),
                RunRef::new(7),
                RoleName::new("qa"),
                verdict,
                minimal_evidence(),
                vec![],
                vec![BlockerId::new(42)],
                waiver,
                reason,
            )
            .unwrap_err();
            assert_eq!(err, QaError::UnexpectedBlockers { verdict, count: 1 });
        }
    }

    #[test]
    fn waived_requires_role_and_reason() {
        let err = QaOutcome::try_new(
            QaVerdictId::new(1),
            WorkItemId::new(10),
            RunRef::new(7),
            RoleName::new("qa"),
            QaVerdict::Waived,
            minimal_evidence(),
            vec![],
            vec![],
            None,
            Some("ok".into()),
        )
        .unwrap_err();
        assert_eq!(err, QaError::InvalidWaiver);

        let err = QaOutcome::try_new(
            QaVerdictId::new(1),
            WorkItemId::new(10),
            RunRef::new(7),
            RoleName::new("qa"),
            QaVerdict::Waived,
            minimal_evidence(),
            vec![],
            vec![],
            Some(RoleName::new("platform_lead")),
            None,
        )
        .unwrap_err();
        assert_eq!(err, QaError::InvalidWaiver);

        let err = QaOutcome::try_new(
            QaVerdictId::new(1),
            WorkItemId::new(10),
            RunRef::new(7),
            RoleName::new("qa"),
            QaVerdict::Waived,
            minimal_evidence(),
            vec![],
            vec![],
            Some(RoleName::new("platform_lead")),
            Some("   ".into()),
        )
        .unwrap_err();
        assert_eq!(err, QaError::InvalidWaiver);

        QaOutcome::try_new(
            QaVerdictId::new(1),
            WorkItemId::new(10),
            RunRef::new(7),
            RoleName::new("qa"),
            QaVerdict::Waived,
            minimal_evidence(),
            vec![],
            vec![],
            Some(RoleName::new("platform_lead")),
            Some("accepted risk".into()),
        )
        .expect("waiver role + reason accepted");
    }

    #[test]
    fn non_waived_rejects_waiver_role() {
        let err = QaOutcome::try_new(
            QaVerdictId::new(1),
            WorkItemId::new(10),
            RunRef::new(7),
            RoleName::new("qa"),
            QaVerdict::Passed,
            minimal_evidence(),
            vec![passing_trace()],
            vec![],
            Some(RoleName::new("platform_lead")),
            None,
        )
        .unwrap_err();
        assert_eq!(err, QaError::UnexpectedWaiver(QaVerdict::Passed));
    }

    #[test]
    fn empty_criterion_is_rejected() {
        let trace = AcceptanceCriterionTrace {
            criterion: "   ".into(),
            status: AcceptanceCriterionStatus::Verified,
            evidence: vec![],
            notes: None,
        };
        let err = QaOutcome::try_new(
            QaVerdictId::new(1),
            WorkItemId::new(10),
            RunRef::new(7),
            RoleName::new("qa"),
            QaVerdict::Passed,
            minimal_evidence(),
            vec![trace],
            vec![],
            None,
            None,
        )
        .unwrap_err();
        assert_eq!(err, QaError::EmptyCriterion);
    }

    #[test]
    fn unpassed_criteria_iter_returns_only_failures() {
        let outcome = QaOutcome::try_new(
            QaVerdictId::new(1),
            WorkItemId::new(10),
            RunRef::new(7),
            RoleName::new("qa"),
            QaVerdict::FailedNeedsRework,
            minimal_evidence(),
            vec![
                passing_trace(),
                AcceptanceCriterionTrace {
                    criterion: "ui flows".into(),
                    status: AcceptanceCriterionStatus::Failed,
                    evidence: vec![],
                    notes: Some("404 on submit".into()),
                },
                AcceptanceCriterionTrace {
                    criterion: "screenshot".into(),
                    status: AcceptanceCriterionStatus::NotVerified,
                    evidence: vec![],
                    notes: None,
                },
            ],
            vec![],
            None,
            Some("rerun after fix".into()),
        )
        .unwrap();
        let unpassed: Vec<_> = outcome.unpassed_criteria().map(|t| &t.criterion).collect();
        assert_eq!(unpassed, vec!["ui flows", "screenshot"]);
    }

    #[test]
    fn qa_outcome_round_trips_through_json() {
        let outcome = pass(99);
        let json = serde_json::to_string(&outcome).unwrap();
        let back: QaOutcome = serde_json::from_str(&json).unwrap();
        assert_eq!(back, outcome);
    }

    #[test]
    fn qa_verdict_id_round_trips_as_bare_integer() {
        let id = QaVerdictId::new(42);
        let json = serde_json::to_string(&id).unwrap();
        assert_eq!(json, "42");
        let back: QaVerdictId = serde_json::from_str(&json).unwrap();
        assert_eq!(back, id);
    }

    #[test]
    fn waived_verdict_with_blockers_is_rejected() {
        let err = QaOutcome::try_new(
            QaVerdictId::new(1),
            WorkItemId::new(10),
            RunRef::new(7),
            RoleName::new("qa"),
            QaVerdict::Waived,
            minimal_evidence(),
            vec![],
            vec![BlockerId::new(1)],
            Some(RoleName::new("platform_lead")),
            Some("accepted risk".into()),
        )
        .unwrap_err();
        assert_eq!(
            err,
            QaError::UnexpectedBlockers {
                verdict: QaVerdict::Waived,
                count: 1,
            }
        );
    }
}
