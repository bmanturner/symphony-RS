//! Derive durable follow-up specs from agent-emitted handoff intent
//! (SPEC v2 §4.10, §5.13, §6.6).
//!
//! When an agent run emits a [`HandoffFollowupRequest`] in its handoff,
//! the kernel needs to convert that minimal wire-level intent into a
//! durable [`FollowupIssueRequest`] before persistence. The handoff type
//! intentionally stays small (title, summary, blocking, propose_only) so
//! the agent's output schema is stable across policy changes; SPEC §5.13
//! requires the durable record to additionally carry scope, acceptance
//! criteria, and the relationship to the source work item.
//!
//! The kernel resolves that gap at the seam by combining the handoff
//! intent with workflow-supplied scope and acceptance criteria into a
//! richer [`FollowupRequestInput`], then folding a slice of those inputs
//! into pre-id [`FollowupSpec`]s via [`derive_followups`]. The storage
//! seam assigns ids and finalises each spec via
//! [`FollowupSpec::into_followup`].
//!
//! Mirrors the [`crate::derive_qa_blockers`] / [`crate::QaBlockerSpec`]
//! pattern so QA-filed blockers and any-role-filed follow-ups share the
//! same shape: validate intent → produce pre-id specs → finalise into
//! durable rows once storage assigns ids.
//!
//! Policy resolution rules (SPEC §5.13):
//!
//! * `default_policy = create_directly`, request `propose_only = false` →
//!   spec carries [`FollowupPolicy::CreateDirectly`].
//! * `default_policy = create_directly`, request `propose_only = true` →
//!   spec carries [`FollowupPolicy::ProposeForApproval`]. The agent can
//!   *opt up* to the safer route but never *opt down* past the workflow.
//! * `default_policy = propose_for_approval` → spec always carries
//!   [`FollowupPolicy::ProposeForApproval`] regardless of the request's
//!   `propose_only` flag.
//!
//! In short: the workflow chooses the floor; the agent may raise it.

use crate::blocker::{BlockerOrigin, RunRef};
use crate::followup::{FollowupError, FollowupId, FollowupIssueRequest, FollowupPolicy};
use crate::handoff::HandoffFollowupRequest;
use crate::role::{RoleAuthority, RoleName};
use crate::work_item::WorkItemId;

/// Richer in-kernel input combining a [`HandoffFollowupRequest`] with the
/// scope and acceptance criteria the workflow supplies at the seam.
///
/// The handoff struct stays minimal for schema stability; the kernel
/// builds a [`FollowupRequestInput`] per request before invoking
/// [`derive_followups`]. Tests that exercise the derivation directly
/// construct these by hand.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FollowupRequestInput {
    /// Short tracker title copied from the handoff intent.
    pub title: String,
    /// Operator-facing reason (typically the handoff `summary`).
    pub reason: String,
    /// Bounded scope description distinguishing the follow-up from the
    /// source item. Kernel-supplied — the handoff type does not yet carry
    /// it (Phase 10 wires the structured handoff path).
    pub scope: String,
    /// Acceptance criteria bullets. May be empty when the workflow's
    /// `followups.require_acceptance_criteria` is `false`.
    pub acceptance_criteria: Vec<String>,
    /// True if the agent flagged the follow-up as gating current
    /// acceptance.
    pub blocking: bool,
    /// True if the agent explicitly opted into the approval queue. Per
    /// SPEC §5.13 the agent can raise to `propose_for_approval` but not
    /// override a workflow that already requires approval.
    pub propose_only: bool,
}

impl FollowupRequestInput {
    /// Build a [`FollowupRequestInput`] by joining a handoff intent with
    /// kernel-supplied scope and acceptance criteria.
    ///
    /// Provided as an explicit bridge so the kernel seam reads obviously
    /// at the call site and tests do not have to know the field layout.
    pub fn from_handoff(
        handoff: &HandoffFollowupRequest,
        scope: impl Into<String>,
        acceptance_criteria: Vec<String>,
    ) -> Self {
        Self {
            title: handoff.title.clone(),
            reason: handoff.summary.clone(),
            scope: scope.into(),
            acceptance_criteria,
            blocking: handoff.blocking,
            propose_only: handoff.propose_only,
        }
    }
}

/// Pre-persistence shape for a follow-up issue request (SPEC v2 §4.10).
///
/// Carries every field [`FollowupIssueRequest::try_new`] needs except the
/// durable [`FollowupId`]. Hand to the storage seam to assign an id, then
/// call [`Self::into_followup`] to obtain the validated durable record.
///
/// The resolved [`FollowupPolicy`] is captured here so the storage seam
/// (and any observers) cannot accidentally re-resolve policy and drift
/// from what was decided at handoff time.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FollowupSpec {
    /// Work item that *spawned* the follow-up.
    pub source_work_item: WorkItemId,
    /// Short tracker title.
    pub title: String,
    /// Operator-facing reason.
    pub reason: String,
    /// Bounded scope description.
    pub scope: String,
    /// Acceptance criteria bullets.
    pub acceptance_criteria: Vec<String>,
    /// True when the follow-up gates current acceptance.
    pub blocking: bool,
    /// Resolved creation policy (workflow default + per-request opt-up).
    pub policy: FollowupPolicy,
    /// Run + role that authored the request. Always
    /// [`BlockerOrigin::Run`] so the audit trail mirrors blocker origins.
    pub origin: BlockerOrigin,
    /// Captured copy of the workflow's
    /// `followups.require_acceptance_criteria` knob — passed through to
    /// [`FollowupIssueRequest::try_new`] when finalising the spec.
    pub require_acceptance_criteria: bool,
}

impl FollowupSpec {
    /// Finalise the spec into a durable [`FollowupIssueRequest`],
    /// applying [`FollowupIssueRequest::try_new`]'s invariants.
    pub fn into_followup(self, id: FollowupId) -> Result<FollowupIssueRequest, FollowupError> {
        FollowupIssueRequest::try_new(
            id,
            self.source_work_item,
            self.title,
            self.reason,
            self.scope,
            self.acceptance_criteria,
            self.blocking,
            self.policy,
            self.origin,
            self.require_acceptance_criteria,
        )
    }
}

/// Why [`derive_followups`] rejected a [`FollowupRequestInput`].
///
/// Each variant carries the input index so operator diagnostics can point
/// at the offending request without re-deriving order from a different
/// collection. Mirrors [`crate::QaBlockerError`].
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum FollowupRequestError {
    /// `title` was empty after trim.
    #[error("followup request at index {index} has empty title")]
    EmptyTitle {
        /// Position in the input slice.
        index: usize,
    },
    /// `reason` was empty after trim.
    #[error("followup request at index {index} has empty reason")]
    EmptyReason {
        /// Position in the input slice.
        index: usize,
    },
    /// `scope` was empty after trim.
    #[error("followup request at index {index} has empty scope")]
    EmptyScope {
        /// Position in the input slice.
        index: usize,
    },
    /// One of the `acceptance_criteria` entries was empty after trim.
    #[error("followup request at index {index} has empty acceptance criterion")]
    EmptyAcceptanceCriterion {
        /// Position in the input slice.
        index: usize,
    },
    /// `acceptance_criteria` was empty when the workflow demanded at
    /// least one (`followups.require_acceptance_criteria = true`).
    #[error("followup request at index {index} requires at least one acceptance criterion")]
    AcceptanceCriteriaRequired {
        /// Position in the input slice.
        index: usize,
    },
    /// The reporting role's [`RoleAuthority::can_file_followups`] flag is
    /// `false`. SPEC §6.6 makes follow-up filing universal *by default*,
    /// but operators may opt a role out via
    /// [`crate::RoleAuthorityOverrides::can_file_followups`]; once they
    /// do, the kernel refuses to derive follow-ups for that role rather
    /// than silently dropping the request.
    #[error("role {role} lacks can_file_followups authority; cannot derive follow-up requests")]
    RoleLacksFollowupAuthority {
        /// Operator-chosen role label that emitted the requests.
        role: RoleName,
    },
}

/// Resolve workflow default policy against a request's `propose_only`
/// flag per SPEC §5.13. The workflow sets the floor; the agent may raise.
fn resolve_policy(default_policy: FollowupPolicy, propose_only: bool) -> FollowupPolicy {
    match (default_policy, propose_only) {
        (FollowupPolicy::ProposeForApproval, _) => FollowupPolicy::ProposeForApproval,
        (FollowupPolicy::CreateDirectly, true) => FollowupPolicy::ProposeForApproval,
        (FollowupPolicy::CreateDirectly, false) => FollowupPolicy::CreateDirectly,
    }
}

/// Derive durable follow-up specs for the agent-emitted requests carried
/// by a single run's handoff.
///
/// Each [`FollowupRequestInput`] becomes a [`FollowupSpec`] with:
///
/// * `source_work_item = source_work_item` — the run's target item.
/// * `policy` resolved from `default_policy` and the request's
///   `propose_only` flag (workflow floor + agent opt-up).
/// * `origin = BlockerOrigin::Run { run_id, role: Some(reporting_role) }`
///   so the audit trail records *which run and role* filed the
///   follow-up, mirroring [`crate::QaBlockerSpec`].
/// * `require_acceptance_criteria` captured so finalisation re-applies
///   the same knob, even if workflow config changes between derivation
///   and persistence.
///
/// Validation runs in input order and stops at the first invalid request
/// so the kernel can surface the offending index to the agent for repair
/// (SPEC §7 malformed handoff handling). Returns an empty `Vec` for an
/// empty input slice — the common no-follow-ups happy path.
///
/// Per SPEC §6.6 every role may identify follow-up work, so the default
/// authority granted by [`RoleAuthority::defaults_for`] is `true` for
/// every kind. The check still runs because operators can opt a role out
/// explicitly via [`crate::RoleAuthorityOverrides`]: when the resolved
/// `can_file_followups` is `false`, derivation refuses the entire batch
/// before any per-request validation so the diagnostic points at the
/// authority gap rather than an incidental shape error. An empty
/// `requests` slice is allowed regardless of authority — there is
/// nothing to file.
pub fn derive_followups(
    source_work_item: WorkItemId,
    run_id: RunRef,
    reporting_role: &RoleName,
    role_authority: &RoleAuthority,
    default_policy: FollowupPolicy,
    require_acceptance_criteria: bool,
    requests: &[FollowupRequestInput],
) -> Result<Vec<FollowupSpec>, FollowupRequestError> {
    if !requests.is_empty() && !role_authority.can_file_followups {
        return Err(FollowupRequestError::RoleLacksFollowupAuthority {
            role: reporting_role.clone(),
        });
    }
    let mut specs = Vec::with_capacity(requests.len());
    for (index, request) in requests.iter().enumerate() {
        if request.title.trim().is_empty() {
            return Err(FollowupRequestError::EmptyTitle { index });
        }
        if request.reason.trim().is_empty() {
            return Err(FollowupRequestError::EmptyReason { index });
        }
        if request.scope.trim().is_empty() {
            return Err(FollowupRequestError::EmptyScope { index });
        }
        for criterion in &request.acceptance_criteria {
            if criterion.trim().is_empty() {
                return Err(FollowupRequestError::EmptyAcceptanceCriterion { index });
            }
        }
        if require_acceptance_criteria && request.acceptance_criteria.is_empty() {
            return Err(FollowupRequestError::AcceptanceCriteriaRequired { index });
        }
        specs.push(FollowupSpec {
            source_work_item,
            title: request.title.clone(),
            reason: request.reason.clone(),
            scope: request.scope.clone(),
            acceptance_criteria: request.acceptance_criteria.clone(),
            blocking: request.blocking,
            policy: resolve_policy(default_policy, request.propose_only),
            origin: BlockerOrigin::Run {
                run_id,
                role: Some(reporting_role.clone()),
            },
            require_acceptance_criteria,
        });
    }
    Ok(specs)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::followup::FollowupStatus;
    use crate::role::RoleKind;

    fn role() -> RoleName {
        RoleName::new("backend")
    }

    fn specialist_authority() -> RoleAuthority {
        RoleAuthority::defaults_for(RoleKind::Specialist)
    }

    fn handoff(
        title: &str,
        summary: &str,
        blocking: bool,
        propose_only: bool,
    ) -> HandoffFollowupRequest {
        HandoffFollowupRequest {
            title: title.into(),
            summary: summary.into(),
            blocking,
            propose_only,
        }
    }

    fn input(
        title: &str,
        reason: &str,
        scope: &str,
        ac: Vec<String>,
        blocking: bool,
        propose_only: bool,
    ) -> FollowupRequestInput {
        FollowupRequestInput {
            title: title.into(),
            reason: reason.into(),
            scope: scope.into(),
            acceptance_criteria: ac,
            blocking,
            propose_only,
        }
    }

    #[test]
    fn from_handoff_copies_all_fields_and_attaches_kernel_supplied_context() {
        let h = handoff("rate-limit", "tighten retries on 429", true, false);
        let input = FollowupRequestInput::from_handoff(
            &h,
            "symphony-tracker GitHub adapter only",
            vec!["retries respect Retry-After".into()],
        );
        assert_eq!(input.title, "rate-limit");
        assert_eq!(input.reason, "tighten retries on 429");
        assert_eq!(input.scope, "symphony-tracker GitHub adapter only");
        assert_eq!(
            input.acceptance_criteria,
            vec!["retries respect Retry-After"]
        );
        assert!(input.blocking);
        assert!(!input.propose_only);
    }

    #[test]
    fn resolve_policy_workflow_floor_then_agent_opt_up() {
        assert_eq!(
            resolve_policy(FollowupPolicy::CreateDirectly, false),
            FollowupPolicy::CreateDirectly
        );
        assert_eq!(
            resolve_policy(FollowupPolicy::CreateDirectly, true),
            FollowupPolicy::ProposeForApproval
        );
        assert_eq!(
            resolve_policy(FollowupPolicy::ProposeForApproval, false),
            FollowupPolicy::ProposeForApproval
        );
        assert_eq!(
            resolve_policy(FollowupPolicy::ProposeForApproval, true),
            FollowupPolicy::ProposeForApproval
        );
    }

    #[test]
    fn derives_one_spec_per_request_with_run_and_role_origin() {
        let inputs = vec![
            input("a", "ra", "sa", vec!["ac1".into()], false, false),
            input("b", "rb", "sb", vec!["ac1".into()], true, true),
        ];
        let specs = derive_followups(
            WorkItemId::new(50),
            RunRef::new(7),
            &role(),
            &specialist_authority(),
            FollowupPolicy::CreateDirectly,
            true,
            &inputs,
        )
        .unwrap();
        assert_eq!(specs.len(), 2);
        assert_eq!(specs[0].source_work_item, WorkItemId::new(50));
        assert_eq!(specs[0].policy, FollowupPolicy::CreateDirectly);
        // propose_only=true raises the floor.
        assert_eq!(specs[1].policy, FollowupPolicy::ProposeForApproval);
        for spec in &specs {
            match &spec.origin {
                BlockerOrigin::Run { run_id, role } => {
                    assert_eq!(*run_id, RunRef::new(7));
                    assert_eq!(role.as_ref().unwrap().as_str(), "backend");
                }
                other => panic!("unexpected origin: {other:?}"),
            }
            assert!(spec.require_acceptance_criteria);
        }
    }

    #[test]
    fn workflow_propose_for_approval_overrides_request_propose_only_false() {
        let inputs = vec![input("t", "r", "s", vec!["ac".into()], false, false)];
        let specs = derive_followups(
            WorkItemId::new(1),
            RunRef::new(2),
            &role(),
            &specialist_authority(),
            FollowupPolicy::ProposeForApproval,
            true,
            &inputs,
        )
        .unwrap();
        assert_eq!(specs[0].policy, FollowupPolicy::ProposeForApproval);
    }

    #[test]
    fn empty_input_slice_yields_empty_specs() {
        let specs = derive_followups(
            WorkItemId::new(1),
            RunRef::new(2),
            &role(),
            &specialist_authority(),
            FollowupPolicy::CreateDirectly,
            true,
            &[],
        )
        .unwrap();
        assert!(specs.is_empty());
    }

    #[test]
    fn empty_title_rejected_with_index() {
        let inputs = vec![
            input("ok", "r", "s", vec!["ac".into()], false, false),
            input("  ", "r", "s", vec!["ac".into()], false, false),
        ];
        let err = derive_followups(
            WorkItemId::new(1),
            RunRef::new(2),
            &role(),
            &specialist_authority(),
            FollowupPolicy::CreateDirectly,
            true,
            &inputs,
        )
        .unwrap_err();
        assert_eq!(err, FollowupRequestError::EmptyTitle { index: 1 });
    }

    #[test]
    fn empty_reason_rejected_with_index() {
        let inputs = vec![input("t", "", "s", vec!["ac".into()], false, false)];
        let err = derive_followups(
            WorkItemId::new(1),
            RunRef::new(2),
            &role(),
            &specialist_authority(),
            FollowupPolicy::CreateDirectly,
            true,
            &inputs,
        )
        .unwrap_err();
        assert_eq!(err, FollowupRequestError::EmptyReason { index: 0 });
    }

    #[test]
    fn empty_scope_rejected_with_index() {
        let inputs = vec![input("t", "r", "  ", vec!["ac".into()], false, false)];
        let err = derive_followups(
            WorkItemId::new(1),
            RunRef::new(2),
            &role(),
            &specialist_authority(),
            FollowupPolicy::CreateDirectly,
            true,
            &inputs,
        )
        .unwrap_err();
        assert_eq!(err, FollowupRequestError::EmptyScope { index: 0 });
    }

    #[test]
    fn empty_acceptance_criterion_rejected_with_index() {
        let inputs = vec![input(
            "t",
            "r",
            "s",
            vec!["ok".into(), "  ".into()],
            false,
            false,
        )];
        let err = derive_followups(
            WorkItemId::new(1),
            RunRef::new(2),
            &role(),
            &specialist_authority(),
            FollowupPolicy::CreateDirectly,
            true,
            &inputs,
        )
        .unwrap_err();
        assert_eq!(
            err,
            FollowupRequestError::EmptyAcceptanceCriterion { index: 0 }
        );
    }

    #[test]
    fn missing_acceptance_criteria_rejected_when_workflow_requires_them() {
        let inputs = vec![input("t", "r", "s", vec![], false, false)];
        let err = derive_followups(
            WorkItemId::new(1),
            RunRef::new(2),
            &role(),
            &specialist_authority(),
            FollowupPolicy::CreateDirectly,
            true,
            &inputs,
        )
        .unwrap_err();
        assert_eq!(
            err,
            FollowupRequestError::AcceptanceCriteriaRequired { index: 0 }
        );
    }

    #[test]
    fn missing_acceptance_criteria_allowed_when_workflow_does_not_require_them() {
        let inputs = vec![input("t", "r", "s", vec![], false, false)];
        let specs = derive_followups(
            WorkItemId::new(1),
            RunRef::new(2),
            &role(),
            &specialist_authority(),
            FollowupPolicy::CreateDirectly,
            false,
            &inputs,
        )
        .unwrap();
        assert_eq!(specs.len(), 1);
        assert!(specs[0].acceptance_criteria.is_empty());
        assert!(!specs[0].require_acceptance_criteria);
    }

    #[test]
    fn spec_into_followup_round_trips_invariants_and_initial_status() {
        let inputs = vec![
            input("a", "ra", "sa", vec!["ac1".into()], false, false),
            input("b", "rb", "sb", vec!["ac1".into()], true, true),
        ];
        let specs = derive_followups(
            WorkItemId::new(50),
            RunRef::new(7),
            &role(),
            &specialist_authority(),
            FollowupPolicy::CreateDirectly,
            true,
            &inputs,
        )
        .unwrap();
        let f0 = specs[0]
            .clone()
            .into_followup(FollowupId::new(101))
            .unwrap();
        assert_eq!(f0.id, FollowupId::new(101));
        assert_eq!(f0.policy, FollowupPolicy::CreateDirectly);
        assert_eq!(f0.status, FollowupStatus::Approved);
        assert_eq!(f0.source_work_item, WorkItemId::new(50));

        let f1 = specs[1]
            .clone()
            .into_followup(FollowupId::new(102))
            .unwrap();
        assert_eq!(f1.policy, FollowupPolicy::ProposeForApproval);
        assert_eq!(f1.status, FollowupStatus::Proposed);
        assert!(f1.blocking);
    }

    #[test]
    fn spec_into_followup_propagates_followup_invariant_errors() {
        // Build a spec by hand to exercise the finalisation invariants
        // even if `derive_followups` would have rejected the input first.
        let spec = FollowupSpec {
            source_work_item: WorkItemId::new(1),
            title: "  ".into(),
            reason: "r".into(),
            scope: "s".into(),
            acceptance_criteria: vec!["ac".into()],
            blocking: false,
            policy: FollowupPolicy::CreateDirectly,
            origin: BlockerOrigin::Run {
                run_id: RunRef::new(2),
                role: Some(role()),
            },
            require_acceptance_criteria: true,
        };
        let err = spec.into_followup(FollowupId::new(1)).unwrap_err();
        assert_eq!(err, FollowupError::EmptyTitle);
    }

    #[test]
    fn every_role_kind_can_derive_followups_with_default_authority() {
        // SPEC §6.6: any role may identify follow-up work. Locks in that
        // the default authority granted by RoleAuthority::defaults_for is
        // sufficient for every role kind, not just qa_gate.
        let inputs = vec![input("t", "r", "s", vec!["ac".into()], false, false)];
        for kind in RoleKind::ALL {
            let authority = RoleAuthority::defaults_for(kind);
            let specs = derive_followups(
                WorkItemId::new(1),
                RunRef::new(2),
                &role(),
                &authority,
                FollowupPolicy::CreateDirectly,
                true,
                &inputs,
            )
            .unwrap_or_else(|e| panic!("{kind} should derive follow-ups: {e}"));
            assert_eq!(specs.len(), 1);
            assert_eq!(specs[0].policy, FollowupPolicy::CreateDirectly);
        }
    }

    #[test]
    fn role_with_can_file_followups_disabled_is_rejected() {
        // Operators may explicitly opt a role out of follow-up filing
        // via RoleAuthorityOverrides; once they do, derivation refuses
        // the batch with RoleLacksFollowupAuthority rather than silently
        // dropping the request.
        let authority = RoleAuthority {
            can_file_followups: false,
            ..RoleAuthority::defaults_for(RoleKind::Specialist)
        };
        let inputs = vec![input("t", "r", "s", vec!["ac".into()], false, false)];
        let err = derive_followups(
            WorkItemId::new(1),
            RunRef::new(2),
            &role(),
            &authority,
            FollowupPolicy::CreateDirectly,
            true,
            &inputs,
        )
        .unwrap_err();
        assert_eq!(
            err,
            FollowupRequestError::RoleLacksFollowupAuthority { role: role() },
        );
    }

    #[test]
    fn empty_request_slice_is_allowed_even_without_authority() {
        // No requests means nothing to file, so the authority gate is
        // a no-op. This keeps the kernel from spuriously erroring on
        // runs that emit zero follow-ups regardless of authority.
        let authority = RoleAuthority {
            can_file_followups: false,
            ..RoleAuthority::NONE
        };
        let specs = derive_followups(
            WorkItemId::new(1),
            RunRef::new(2),
            &role(),
            &authority,
            FollowupPolicy::CreateDirectly,
            true,
            &[],
        )
        .unwrap();
        assert!(specs.is_empty());
    }
}
