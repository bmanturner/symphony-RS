//! Rework routing after a QA verdict (SPEC v2 §4.9, §6.5; ARCH §6.3).
//!
//! When the QA-gate role authors a [`QaOutcome`], the kernel needs to
//! decide *where* the work goes next. The five SPEC §4.9 verdicts route
//! differently:
//!
//! * [`QaVerdict::Passed`] / [`QaVerdict::Waived`] — no rework. The work
//!   item is free to advance through any remaining gates.
//! * [`QaVerdict::FailedWithBlockers`] — QA filed at least one structural
//!   blocker. Each blocker's *blocked* work item moves to
//!   [`WorkItemStatusClass::Rework`]; the assigned role of that work item
//!   is the natural rework target. Re-integration happens later through
//!   the QA queue's "new succeeded integration record after the latest
//!   verdict" rule (see `symphony_state::QaQueueRepository`).
//! * [`QaVerdict::FailedNeedsRework`] — QA rejected without filing a
//!   structural blocker. The kernel routes back to the *prior role* — the
//!   most recent contributor to the work item that is not the QA role
//!   itself — and moves the work item to
//!   [`WorkItemStatusClass::Rework`].
//! * [`QaVerdict::Inconclusive`] — QA could not reach a decision. The
//!   work item stays in `qa` so the queue resurfaces it on the next
//!   eligible tick.
//!
//! This module is *pure domain*: it does no I/O and does not mutate
//! state. The kernel materialises [`PriorRunSummary`] rows from the
//! `runs` table and [`BlockerReworkInput`] rows by joining
//! `work_item_edges` (filed blockers) to `work_items` (their blocked-side
//! assigned role). The decision is then a single function call so the
//! same routing rule is enforced regardless of which dispatcher path
//! produced the verdict.
//!
//! The module deliberately does *not* enforce queue admission rules
//! (those live in `symphony_state::QaQueueRepository`) or status-class
//! transition validity (that lives in [`crate::StateMachine`]). It
//! returns the *intent* — "move this work item to `rework` and route to
//! role X" — and trusts the caller to apply the transition through the
//! existing state-machine seam.

use serde::{Deserialize, Serialize};

use crate::blocker::{BlockerId, RunRef};
use crate::qa::{QaOutcome, QaVerdict};
use crate::role::RoleName;
use crate::work_item::{WorkItemId, WorkItemStatusClass};

/// Slim view of one prior run that contributed to the work item under
/// QA, ordered by the caller from oldest to newest.
///
/// The kernel materialises this from the `runs` table joined to the
/// authoring role and work item. Only the fields the routing decision
/// needs cross the seam — the run's status, evidence, and timing stay in
/// state.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PriorRunSummary {
    /// Durable run reference.
    pub run_id: RunRef,
    /// Role the run executed under.
    pub role: RoleName,
    /// Work item the run produced output against. May differ from the
    /// QA-gated work item when the contributor worked on a child issue.
    pub work_item_id: WorkItemId,
}

/// Slim view of one QA-filed blocker that the routing decision needs.
///
/// Carries the durable [`BlockerId`] (so the route can be correlated to
/// the durable [`crate::Blocker`] row), the work item the blocker is
/// filed *against* (the blocked side), and the assigned role of that
/// blocked work item — which is the natural rework target.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct BlockerReworkInput {
    /// Durable id of the blocker the QA verdict filed.
    pub blocker_id: BlockerId,
    /// Work item the blocker is filed against.
    pub blocked_work_item: WorkItemId,
    /// Currently-assigned role of the blocked work item, when one exists.
    /// `None` when the work item has no assigned role yet — the kernel
    /// surfaces the route without a target so an operator can re-route.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub blocked_assigned_role: Option<RoleName>,
}

/// One per-blocker rework route returned by
/// [`derive_qa_rework_routing`] for a [`QaVerdict::FailedWithBlockers`]
/// verdict.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct BlockerReworkRoute {
    /// Durable blocker the route corresponds to.
    pub blocker_id: BlockerId,
    /// Work item the blocker blocks. Moves to
    /// [`WorkItemStatusClass::Rework`] under this route.
    pub blocked_work_item: WorkItemId,
    /// Rework target. `None` when the blocked work item had no assigned
    /// role at decision time; the kernel surfaces the unassigned route
    /// rather than silently dropping it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub target_role: Option<RoleName>,
}

/// Routing decision for a single QA verdict.
///
/// One variant per SPEC §4.9 verdict family. Construct via
/// [`derive_qa_rework_routing`]; the variants are public so dispatchers
/// can pattern-match on the kernel's intent.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum QaReworkDecision {
    /// `passed` or `waived` — the verdict satisfies the QA gate. No
    /// rework routing is required.
    NoReworkNeeded {
        /// The verdict that produced this decision.
        verdict: QaVerdict,
    },
    /// `failed_with_blockers` — each filed blocker drives one
    /// [`BlockerReworkRoute`]. The QA-gated work item itself does not
    /// move; the *blocked* work items move to
    /// [`WorkItemStatusClass::Rework`].
    BlockersFiled {
        /// The verdict that produced this decision.
        verdict: QaVerdict,
        /// Status class the blocked work items should transition to.
        /// Always [`WorkItemStatusClass::Rework`].
        next_status_class: WorkItemStatusClass,
        /// One route per blocker, in [`QaOutcome::blockers_created`]
        /// order so the kernel can reproduce the routing deterministically.
        routes: Vec<BlockerReworkRoute>,
    },
    /// `failed_needs_rework` — route the QA-gated work item back to the
    /// most-recent prior contributor that is not the QA role.
    NeedsRework {
        /// The verdict that produced this decision.
        verdict: QaVerdict,
        /// Status class the QA-gated work item should transition to.
        /// Always [`WorkItemStatusClass::Rework`].
        next_status_class: WorkItemStatusClass,
        /// Work item to re-route. Equals [`QaOutcome::work_item_id`].
        work_item_id: WorkItemId,
        /// Role to route the rework to.
        target_role: RoleName,
        /// Run the rework target was inferred from.
        from_prior_run: RunRef,
    },
    /// `inconclusive` — keep the work item in the QA queue.
    StaysInQa {
        /// The verdict that produced this decision.
        verdict: QaVerdict,
    },
}

/// Errors raised by [`derive_qa_rework_routing`].
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum QaReworkError {
    /// `failed_needs_rework` was authored but no eligible prior run was
    /// supplied. "Eligible" means the prior run's role is not the
    /// authoring QA role — the kernel will not route work back to the
    /// QA gate that just rejected it.
    #[error(
        "verdict failed_needs_rework requires at least one prior run authored \
         by a non-QA role"
    )]
    MissingPriorRunForRework,
    /// `failed_with_blockers` referenced a blocker id that the caller
    /// did not supply context for. The blocker-route table must cover
    /// every blocker on the verdict.
    #[error(
        "verdict failed_with_blockers references blocker {blocker_id} that has \
         no matching BlockerReworkInput"
    )]
    BlockerInputMissing {
        /// The blocker id the verdict referenced.
        blocker_id: BlockerId,
    },
}

/// Derive the rework routing decision for `outcome`.
///
/// `prior_runs` MUST be ordered oldest → newest by run start time. The
/// helper picks the *last* entry whose role is not the verdict's
/// authoring role for [`QaVerdict::FailedNeedsRework`].
///
/// `blocker_inputs` MUST cover every blocker referenced in
/// [`QaOutcome::blockers_created`] for [`QaVerdict::FailedWithBlockers`];
/// any missing id is reported as
/// [`QaReworkError::BlockerInputMissing`]. Extra entries are tolerated
/// so the caller can pass a wider snapshot without filtering.
pub fn derive_qa_rework_routing(
    outcome: &QaOutcome,
    prior_runs: &[PriorRunSummary],
    blocker_inputs: &[BlockerReworkInput],
) -> Result<QaReworkDecision, QaReworkError> {
    match outcome.verdict {
        QaVerdict::Passed | QaVerdict::Waived => Ok(QaReworkDecision::NoReworkNeeded {
            verdict: outcome.verdict,
        }),
        QaVerdict::Inconclusive => Ok(QaReworkDecision::StaysInQa {
            verdict: outcome.verdict,
        }),
        QaVerdict::FailedNeedsRework => {
            let prior = prior_runs
                .iter()
                .rev()
                .find(|r| r.role != outcome.role)
                .ok_or(QaReworkError::MissingPriorRunForRework)?;
            Ok(QaReworkDecision::NeedsRework {
                verdict: outcome.verdict,
                next_status_class: WorkItemStatusClass::Rework,
                work_item_id: outcome.work_item_id,
                target_role: prior.role.clone(),
                from_prior_run: prior.run_id,
            })
        }
        QaVerdict::FailedWithBlockers => {
            let mut routes = Vec::with_capacity(outcome.blockers_created.len());
            for blocker_id in &outcome.blockers_created {
                let input = blocker_inputs
                    .iter()
                    .find(|b| b.blocker_id == *blocker_id)
                    .ok_or(QaReworkError::BlockerInputMissing {
                        blocker_id: *blocker_id,
                    })?;
                routes.push(BlockerReworkRoute {
                    blocker_id: input.blocker_id,
                    blocked_work_item: input.blocked_work_item,
                    target_role: input.blocked_assigned_role.clone(),
                });
            }
            Ok(QaReworkDecision::BlockersFiled {
                verdict: outcome.verdict,
                next_status_class: WorkItemStatusClass::Rework,
                routes,
            })
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::qa::{
        AcceptanceCriterionStatus, AcceptanceCriterionTrace, QaEvidence, QaOutcome, QaVerdictId,
    };

    fn evidence() -> QaEvidence {
        QaEvidence {
            tests_run: vec!["cargo test".into()],
            changed_files_reviewed: vec!["src/foo.rs".into()],
            ..QaEvidence::default()
        }
    }

    fn passing_trace() -> AcceptanceCriterionTrace {
        AcceptanceCriterionTrace {
            criterion: "tests pass".into(),
            status: AcceptanceCriterionStatus::Verified,
            evidence: vec!["cargo test".into()],
            notes: None,
        }
    }

    fn outcome(
        verdict: QaVerdict,
        blockers: Vec<BlockerId>,
        waiver: Option<RoleName>,
        reason: Option<String>,
    ) -> QaOutcome {
        let trace = match verdict {
            QaVerdict::Passed => vec![passing_trace()],
            _ => vec![],
        };
        QaOutcome::try_new(
            QaVerdictId::new(1),
            WorkItemId::new(10),
            RunRef::new(99),
            RoleName::new("qa"),
            verdict,
            evidence(),
            trace,
            blockers,
            waiver,
            reason,
        )
        .expect("test outcome should construct")
    }

    fn prior(role: &str, run: i64, work_item: i64) -> PriorRunSummary {
        PriorRunSummary {
            run_id: RunRef::new(run),
            role: RoleName::new(role),
            work_item_id: WorkItemId::new(work_item),
        }
    }

    #[test]
    fn passed_verdict_yields_no_rework() {
        let out = outcome(QaVerdict::Passed, vec![], None, None);
        let decision = derive_qa_rework_routing(&out, &[], &[]).unwrap();
        assert_eq!(
            decision,
            QaReworkDecision::NoReworkNeeded {
                verdict: QaVerdict::Passed
            }
        );
    }

    #[test]
    fn waived_verdict_yields_no_rework() {
        let out = outcome(
            QaVerdict::Waived,
            vec![],
            Some(RoleName::new("platform_lead")),
            Some("accepted risk".into()),
        );
        let decision = derive_qa_rework_routing(&out, &[], &[]).unwrap();
        assert_eq!(
            decision,
            QaReworkDecision::NoReworkNeeded {
                verdict: QaVerdict::Waived
            }
        );
    }

    #[test]
    fn inconclusive_keeps_item_in_qa() {
        let out = outcome(
            QaVerdict::Inconclusive,
            vec![],
            None,
            Some("env down".into()),
        );
        let decision = derive_qa_rework_routing(&out, &[], &[]).unwrap();
        assert_eq!(
            decision,
            QaReworkDecision::StaysInQa {
                verdict: QaVerdict::Inconclusive
            }
        );
    }

    #[test]
    fn failed_needs_rework_routes_to_most_recent_non_qa_prior_run() {
        let out = outcome(
            QaVerdict::FailedNeedsRework,
            vec![],
            None,
            Some("ui broken".into()),
        );
        let runs = [
            prior("backend", 1, 10),
            prior("frontend", 2, 10),
            prior("platform_lead", 3, 10),
            prior("qa", 4, 10), // QA's own previous attempt — must be skipped
        ];
        let decision = derive_qa_rework_routing(&out, &runs, &[]).unwrap();
        match decision {
            QaReworkDecision::NeedsRework {
                verdict,
                next_status_class,
                work_item_id,
                target_role,
                from_prior_run,
            } => {
                assert_eq!(verdict, QaVerdict::FailedNeedsRework);
                assert_eq!(next_status_class, WorkItemStatusClass::Rework);
                assert_eq!(work_item_id, WorkItemId::new(10));
                assert_eq!(target_role, RoleName::new("platform_lead"));
                assert_eq!(from_prior_run, RunRef::new(3));
            }
            other => panic!("expected NeedsRework, got {other:?}"),
        }
    }

    #[test]
    fn failed_needs_rework_with_only_qa_prior_runs_errors() {
        let out = outcome(
            QaVerdict::FailedNeedsRework,
            vec![],
            None,
            Some("missing evidence".into()),
        );
        let runs = [prior("qa", 1, 10), prior("qa", 2, 10)];
        let err = derive_qa_rework_routing(&out, &runs, &[]).unwrap_err();
        assert_eq!(err, QaReworkError::MissingPriorRunForRework);
    }

    #[test]
    fn failed_needs_rework_with_no_prior_runs_errors() {
        let out = outcome(
            QaVerdict::FailedNeedsRework,
            vec![],
            None,
            Some("no prior context".into()),
        );
        let err = derive_qa_rework_routing(&out, &[], &[]).unwrap_err();
        assert_eq!(err, QaReworkError::MissingPriorRunForRework);
    }

    #[test]
    fn failed_with_blockers_emits_one_route_per_blocker_in_order() {
        let out = outcome(
            QaVerdict::FailedWithBlockers,
            vec![BlockerId::new(7), BlockerId::new(11), BlockerId::new(3)],
            None,
            Some("regressions".into()),
        );
        let inputs = vec![
            BlockerReworkInput {
                blocker_id: BlockerId::new(3),
                blocked_work_item: WorkItemId::new(30),
                blocked_assigned_role: Some(RoleName::new("backend")),
            },
            BlockerReworkInput {
                blocker_id: BlockerId::new(7),
                blocked_work_item: WorkItemId::new(70),
                blocked_assigned_role: Some(RoleName::new("frontend")),
            },
            BlockerReworkInput {
                blocker_id: BlockerId::new(11),
                blocked_work_item: WorkItemId::new(110),
                blocked_assigned_role: None,
            },
        ];
        let decision = derive_qa_rework_routing(&out, &[], &inputs).unwrap();
        match decision {
            QaReworkDecision::BlockersFiled {
                verdict,
                next_status_class,
                routes,
            } => {
                assert_eq!(verdict, QaVerdict::FailedWithBlockers);
                assert_eq!(next_status_class, WorkItemStatusClass::Rework);
                assert_eq!(
                    routes,
                    vec![
                        BlockerReworkRoute {
                            blocker_id: BlockerId::new(7),
                            blocked_work_item: WorkItemId::new(70),
                            target_role: Some(RoleName::new("frontend")),
                        },
                        BlockerReworkRoute {
                            blocker_id: BlockerId::new(11),
                            blocked_work_item: WorkItemId::new(110),
                            target_role: None,
                        },
                        BlockerReworkRoute {
                            blocker_id: BlockerId::new(3),
                            blocked_work_item: WorkItemId::new(30),
                            target_role: Some(RoleName::new("backend")),
                        },
                    ]
                );
            }
            other => panic!("expected BlockersFiled, got {other:?}"),
        }
    }

    #[test]
    fn failed_with_blockers_without_input_for_referenced_blocker_errors() {
        let out = outcome(
            QaVerdict::FailedWithBlockers,
            vec![BlockerId::new(7), BlockerId::new(11)],
            None,
            Some("regressions".into()),
        );
        let inputs = vec![BlockerReworkInput {
            blocker_id: BlockerId::new(7),
            blocked_work_item: WorkItemId::new(70),
            blocked_assigned_role: Some(RoleName::new("frontend")),
        }];
        let err = derive_qa_rework_routing(&out, &[], &inputs).unwrap_err();
        assert_eq!(
            err,
            QaReworkError::BlockerInputMissing {
                blocker_id: BlockerId::new(11),
            }
        );
    }

    #[test]
    fn failed_with_blockers_tolerates_extra_blocker_inputs() {
        let out = outcome(
            QaVerdict::FailedWithBlockers,
            vec![BlockerId::new(7)],
            None,
            Some("regression".into()),
        );
        let inputs = vec![
            BlockerReworkInput {
                blocker_id: BlockerId::new(7),
                blocked_work_item: WorkItemId::new(70),
                blocked_assigned_role: Some(RoleName::new("frontend")),
            },
            BlockerReworkInput {
                blocker_id: BlockerId::new(99),
                blocked_work_item: WorkItemId::new(990),
                blocked_assigned_role: Some(RoleName::new("backend")),
            },
        ];
        let decision = derive_qa_rework_routing(&out, &[], &inputs).unwrap();
        match decision {
            QaReworkDecision::BlockersFiled { routes, .. } => {
                assert_eq!(routes.len(), 1);
                assert_eq!(routes[0].blocker_id, BlockerId::new(7));
            }
            other => panic!("expected BlockersFiled, got {other:?}"),
        }
    }

    #[test]
    fn decision_round_trips_through_json_for_each_variant() {
        let cases = vec![
            QaReworkDecision::NoReworkNeeded {
                verdict: QaVerdict::Passed,
            },
            QaReworkDecision::StaysInQa {
                verdict: QaVerdict::Inconclusive,
            },
            QaReworkDecision::NeedsRework {
                verdict: QaVerdict::FailedNeedsRework,
                next_status_class: WorkItemStatusClass::Rework,
                work_item_id: WorkItemId::new(10),
                target_role: RoleName::new("backend"),
                from_prior_run: RunRef::new(3),
            },
            QaReworkDecision::BlockersFiled {
                verdict: QaVerdict::FailedWithBlockers,
                next_status_class: WorkItemStatusClass::Rework,
                routes: vec![BlockerReworkRoute {
                    blocker_id: BlockerId::new(7),
                    blocked_work_item: WorkItemId::new(70),
                    target_role: Some(RoleName::new("frontend")),
                }],
            },
        ];
        for decision in cases {
            let json = serde_json::to_string(&decision).unwrap();
            let back: QaReworkDecision = serde_json::from_str(&json).unwrap();
            assert_eq!(back, decision);
        }
    }

    #[test]
    fn most_recent_non_qa_prior_run_wins_over_earlier_non_qa_runs() {
        let out = outcome(
            QaVerdict::FailedNeedsRework,
            vec![],
            None,
            Some("ui broken".into()),
        );
        let runs = [
            prior("frontend", 1, 10),
            prior("backend", 2, 10),
            prior("qa", 3, 10),
            prior("backend", 4, 10), // most recent non-qa
            prior("qa", 5, 10),
        ];
        let decision = derive_qa_rework_routing(&out, &runs, &[]).unwrap();
        match decision {
            QaReworkDecision::NeedsRework {
                from_prior_run,
                target_role,
                ..
            } => {
                assert_eq!(from_prior_run, RunRef::new(4));
                assert_eq!(target_role, RoleName::new("backend"));
            }
            other => panic!("expected NeedsRework, got {other:?}"),
        }
    }
}
