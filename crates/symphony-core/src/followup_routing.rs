//! Route a derived [`FollowupSpec`] to either the tracker-creation path
//! or the follow-up approval queue (SPEC v2 §4.10, §5.13; ARCH §6.4).
//!
//! [`crate::derive_followups`] resolves the workflow's `default_policy`
//! against each request's `propose_only` flag and stamps the resulting
//! [`FollowupPolicy`] onto every [`FollowupSpec`]. The next seam decides
//! *where the spec goes next*:
//!
//! * [`FollowupPolicy::CreateDirectly`] → write straight through
//!   `TrackerMutations`. The durable lifecycle skips the approval queue
//!   and starts at [`crate::FollowupStatus::Approved`] per
//!   [`FollowupPolicy::initial_status`].
//! * [`FollowupPolicy::ProposeForApproval`] → park in the follow-up
//!   approval queue. The durable lifecycle starts at
//!   [`crate::FollowupStatus::Proposed`] and an explicit approval (by the
//!   role configured under `followups.approval_role`) advances it.
//!
//! This module is *pure domain*: it does no I/O and does not mutate state.
//! It returns the *intent* — "write directly" or "route to the approval
//! queue, gated by role X" — and trusts the caller (the kernel scheduler
//! seam in Phase 11 / 12) to enact the decision against the tracker or
//! durable approval-queue table.
//!
//! The decision deliberately separates from [`crate::derive_followups`]
//! so policy resolution and queue routing can evolve independently and so
//! the caller can persist the [`FollowupSpec`] (with its resolved policy)
//! before consulting the routing seam — mirroring the
//! [`crate::derive_qa_rework_routing`] pattern where the verdict is
//! durable before the routing decision is enacted.

use serde::{Deserialize, Serialize};

use crate::followup::FollowupPolicy;
use crate::followup_request::FollowupSpec;
use crate::role::RoleName;

/// Where a follow-up spec should go next.
///
/// Returned by [`route_followup`] and the batch helper
/// [`route_followups`]. The variant carries everything the caller needs
/// to enact the decision without re-resolving the workflow knob.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum FollowupRouteDecision {
    /// Policy resolved to [`FollowupPolicy::CreateDirectly`]. The kernel
    /// writes the issue through `TrackerMutations` immediately and
    /// transitions the durable record from
    /// [`crate::FollowupStatus::Approved`] to
    /// [`crate::FollowupStatus::Created`] once the tracker write
    /// succeeds.
    CreateDirectly,
    /// Policy resolved to [`FollowupPolicy::ProposeForApproval`]. The
    /// kernel parks the durable record in the follow-up approval queue;
    /// only the configured `approval_role` may advance it via
    /// [`crate::FollowupIssueRequest::approve`] /
    /// [`crate::FollowupIssueRequest::reject`].
    RouteToApprovalQueue {
        /// Role configured under `followups.approval_role`. Captured at
        /// decision time so a later config change cannot retroactively
        /// re-route an already-queued proposal.
        approval_role: RoleName,
    },
}

impl FollowupRouteDecision {
    /// Stable lowercase identifier used in operator diagnostics.
    pub fn kind(&self) -> &'static str {
        match self {
            Self::CreateDirectly => "create_directly",
            Self::RouteToApprovalQueue { .. } => "route_to_approval_queue",
        }
    }

    /// True when the decision parks the spec in the approval queue.
    pub fn requires_approval(&self) -> bool {
        matches!(self, Self::RouteToApprovalQueue { .. })
    }
}

/// Why [`route_followup`] / [`route_followups`] refused to produce a
/// decision.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum FollowupRoutingError {
    /// The spec resolved to [`FollowupPolicy::ProposeForApproval`] but
    /// the workflow did not configure `followups.approval_role`. The
    /// kernel refuses to silently route to an empty queue: an operator
    /// who picks the safer policy must also configure who approves.
    #[error("followups.approval_role is required when policy = propose_for_approval but is unset")]
    MissingApprovalRole,
}

/// Decide where a single [`FollowupSpec`] should go next.
///
/// Pure: takes the resolved policy stamped on the spec plus the
/// optional `approval_role` configured under `followups.approval_role`
/// and returns the routing intent. The spec itself is not consumed — the
/// caller persists it (with its resolved policy and initial status) and
/// then enacts the returned decision against the tracker or approval
/// queue.
///
/// Returns [`FollowupRoutingError::MissingApprovalRole`] when the spec
/// requires approval but `approval_role` is `None`. This mirrors how
/// [`crate::QaWaiverError::NoWaiverRolesConfigured`] fails closed when a
/// QA waiver names a role that is absent from the workflow.
pub fn route_followup(
    spec: &FollowupSpec,
    approval_role: Option<&RoleName>,
) -> Result<FollowupRouteDecision, FollowupRoutingError> {
    match spec.policy {
        FollowupPolicy::CreateDirectly => Ok(FollowupRouteDecision::CreateDirectly),
        FollowupPolicy::ProposeForApproval => {
            let role = approval_role
                .ok_or(FollowupRoutingError::MissingApprovalRole)?
                .clone();
            Ok(FollowupRouteDecision::RouteToApprovalQueue {
                approval_role: role,
            })
        }
    }
}

/// Decide routing for a slice of specs in input order.
///
/// Stops at the first error so the caller can surface the offending
/// spec via its position in the input slice — same shape as
/// [`crate::derive_followups`]. Returns an empty `Vec` for an empty
/// input slice, the common no-follow-ups happy path.
pub fn route_followups(
    specs: &[FollowupSpec],
    approval_role: Option<&RoleName>,
) -> Result<Vec<FollowupRouteDecision>, FollowupRoutingError> {
    let mut decisions = Vec::with_capacity(specs.len());
    for spec in specs {
        decisions.push(route_followup(spec, approval_role)?);
    }
    Ok(decisions)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::blocker::{BlockerOrigin, RunRef};
    use crate::work_item::WorkItemId;

    fn spec(policy: FollowupPolicy) -> FollowupSpec {
        FollowupSpec {
            source_work_item: WorkItemId::new(42),
            title: "rate-limit cleanup".into(),
            reason: "tighten retries on 429".into(),
            scope: "symphony-tracker GitHub adapter only".into(),
            acceptance_criteria: vec!["retries respect Retry-After".into()],
            blocking: false,
            policy,
            origin: BlockerOrigin::Run {
                run_id: RunRef::new(7),
                role: Some(RoleName::new("backend")),
            },
            require_acceptance_criteria: true,
        }
    }

    #[test]
    fn create_directly_routes_to_tracker_and_ignores_approval_role() {
        let s = spec(FollowupPolicy::CreateDirectly);
        let approval = RoleName::new("platform_lead");

        let with_role = route_followup(&s, Some(&approval)).unwrap();
        let without_role = route_followup(&s, None).unwrap();

        assert_eq!(with_role, FollowupRouteDecision::CreateDirectly);
        assert_eq!(without_role, FollowupRouteDecision::CreateDirectly);
        assert_eq!(with_role.kind(), "create_directly");
        assert!(!with_role.requires_approval());
    }

    #[test]
    fn propose_for_approval_routes_to_queue_with_configured_role() {
        let s = spec(FollowupPolicy::ProposeForApproval);
        let approval = RoleName::new("platform_lead");

        let decision = route_followup(&s, Some(&approval)).unwrap();

        assert_eq!(
            decision,
            FollowupRouteDecision::RouteToApprovalQueue {
                approval_role: approval.clone(),
            }
        );
        assert_eq!(decision.kind(), "route_to_approval_queue");
        assert!(decision.requires_approval());
    }

    #[test]
    fn propose_for_approval_without_approval_role_errors() {
        let s = spec(FollowupPolicy::ProposeForApproval);
        let err = route_followup(&s, None).unwrap_err();
        assert_eq!(err, FollowupRoutingError::MissingApprovalRole);
    }

    #[test]
    fn batch_routes_each_spec_independently() {
        let specs = vec![
            spec(FollowupPolicy::CreateDirectly),
            spec(FollowupPolicy::ProposeForApproval),
            spec(FollowupPolicy::CreateDirectly),
        ];
        let approval = RoleName::new("platform_lead");

        let decisions = route_followups(&specs, Some(&approval)).unwrap();

        assert_eq!(decisions.len(), 3);
        assert_eq!(decisions[0], FollowupRouteDecision::CreateDirectly);
        assert_eq!(
            decisions[1],
            FollowupRouteDecision::RouteToApprovalQueue {
                approval_role: approval,
            }
        );
        assert_eq!(decisions[2], FollowupRouteDecision::CreateDirectly);
    }

    #[test]
    fn batch_stops_at_first_missing_approval_role() {
        let specs = vec![
            spec(FollowupPolicy::CreateDirectly),
            spec(FollowupPolicy::ProposeForApproval),
        ];
        let err = route_followups(&specs, None).unwrap_err();
        assert_eq!(err, FollowupRoutingError::MissingApprovalRole);
    }

    #[test]
    fn empty_input_slice_yields_empty_decisions_regardless_of_role() {
        let with_role = route_followups(&[], Some(&RoleName::new("x"))).unwrap();
        let without_role = route_followups(&[], None).unwrap();
        assert!(with_role.is_empty());
        assert!(without_role.is_empty());
    }

    #[test]
    fn decision_round_trips_through_serde() {
        let direct = FollowupRouteDecision::CreateDirectly;
        let queued = FollowupRouteDecision::RouteToApprovalQueue {
            approval_role: RoleName::new("platform_lead"),
        };
        for d in [direct, queued] {
            let json = serde_json::to_string(&d).unwrap();
            let back: FollowupRouteDecision = serde_json::from_str(&json).unwrap();
            assert_eq!(d, back);
        }
    }
}
