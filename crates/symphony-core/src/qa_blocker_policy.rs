//! QA blocker / waiver policy for parent close (SPEC v2 §5.12).
//!
//! Two pure-domain rules live here:
//!
//! 1. [`check_qa_blockers_at_parent_close`] — under
//!    [`QaBlockerPolicy::BlocksParent`] (the default), the kernel MUST
//!    refuse to close a parent while any QA-filed blocker on that parent's
//!    subtree is still [`crate::BlockerStatus::Open`]. Under
//!    [`QaBlockerPolicy::Advisory`], the gate is bypassed; QA's voice is
//!    surfaced as comments/labels but never gates close.
//!
//! 2. [`validate_qa_waiver`] — every `waived` decision (whether on a QA
//!    verdict per [`crate::QaVerdict::Waived`] or a per-blocker waiver
//!    transition per [`crate::Blocker::transition_status`]) MUST be
//!    authored by a role listed in `qa.waiver_roles` *and* carry a
//!    non-empty reason. The blocker/verdict primitives already enforce
//!    "role + reason supplied"; this module enforces the orthogonal
//!    membership rule, which the workflow config owns.
//!
//! Together the two functions are the single place the kernel checks "QA
//! blockers block parent completion by default; QA waivers require a
//! configured waiver role and reason." Other call sites (parent-closeout,
//! integration owner, operator force-close) consult these helpers rather
//! than re-implementing the rule.
//!
//! The blocker primitive is policy-agnostic: a [`crate::Blocker`] does
//! not know whether the workflow it belongs to runs `BlocksParent` or
//! `Advisory`. That decision is workflow config, not durable state, so
//! the gate takes the policy as a parameter.

use serde::{Deserialize, Serialize};

use crate::blocker::BlockerId;
use crate::role::RoleName;
use crate::work_item::WorkItemId;

/// What QA-filed blockers mean for parent completion (SPEC v2 §5.12
/// `blocker_policy`).
///
/// Mirrors `symphony_config::BlockerPolicy`. The kernel does not depend
/// on the config crate; the policy crosses the seam as this typed enum.
#[derive(
    Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Default, Serialize, Deserialize,
)]
#[serde(rename_all = "snake_case")]
pub enum QaBlockerPolicy {
    /// Open QA-filed blockers gate parent completion until they are
    /// resolved or waived. Default per SPEC §5.12.
    #[default]
    BlocksParent,
    /// Open QA-filed blockers are recorded but do not gate parent
    /// completion. Reserved for advisory pipelines.
    Advisory,
}

impl QaBlockerPolicy {
    /// All variants in declaration order.
    pub const ALL: [Self; 2] = [Self::BlocksParent, Self::Advisory];

    /// Stable lowercase identifier used in error messages and event
    /// payloads.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::BlocksParent => "blocks_parent",
            Self::Advisory => "advisory",
        }
    }

    /// True when the policy gates parent completion on open QA blockers.
    pub fn gates_parent_close(self) -> bool {
        matches!(self, Self::BlocksParent)
    }
}

impl std::fmt::Display for QaBlockerPolicy {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

/// Minimal view of an open QA-filed blocker required by
/// [`check_qa_blockers_at_parent_close`].
///
/// The kernel materialises one of these per open `blocks` edge in the
/// parent's subtree whose origin role is the configured QA role. The
/// snapshot is a point-in-time read of `work_item_edges` joined to the
/// authoring run + role; the gate has no opinion about how the caller
/// performed that join.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct QaBlockerSnapshot {
    /// Domain primary key of the blocker.
    pub blocker_id: BlockerId,
    /// Work item the blocker is filed against.
    pub blocked_work_item: WorkItemId,
    /// Tracker-facing identifier of the blocked work item (e.g. `ENG-7`).
    pub blocked_identifier: String,
    /// QA role that authored the blocker. Carried for operator output;
    /// the gate trusts the caller's filter for "QA-filed" semantics.
    pub origin_role: RoleName,
    /// Free-form reason the blocker exists, when one was recorded.
    pub reason: Option<String>,
}

/// Errors produced by [`check_qa_blockers_at_parent_close`].
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum QaParentCloseError {
    /// At least one QA-filed blocker is open on the parent's subtree and
    /// the policy is [`QaBlockerPolicy::BlocksParent`].
    #[error(
        "parent {parent} cannot close: {} open QA blocker(s) under blocks_parent policy",
        open.len()
    )]
    QaBlockersOpen {
        /// Parent the gate was checked for.
        parent: WorkItemId,
        /// Open QA blockers in caller-supplied order.
        open: Vec<QaBlockerSnapshot>,
    },
}

/// Return `Ok(())` iff the parent may close given QA-filed blocker state.
///
/// Rules:
///
/// * [`QaBlockerPolicy::Advisory`] — never gates; `Ok(())` regardless of
///   `open_qa_blockers`.
/// * [`QaBlockerPolicy::BlocksParent`] — `Ok(())` only when
///   `open_qa_blockers` is empty.
///
/// The caller is responsible for supplying the QA-filed open blocker
/// list. SPEC §4.8 identifies QA-filed blockers by
/// [`crate::BlockerOrigin::Run`] with a role kind of
/// [`crate::RoleKind::QaGate`]; the kernel materialises that filter at
/// the storage seam, the gate consumes the result.
pub fn check_qa_blockers_at_parent_close(
    policy: QaBlockerPolicy,
    parent: WorkItemId,
    open_qa_blockers: &[QaBlockerSnapshot],
) -> Result<(), QaParentCloseError> {
    if !policy.gates_parent_close() || open_qa_blockers.is_empty() {
        return Ok(());
    }
    Err(QaParentCloseError::QaBlockersOpen {
        parent,
        open: open_qa_blockers.to_vec(),
    })
}

/// Errors produced by [`validate_qa_waiver`].
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum QaWaiverError {
    /// The waiver author is not in the configured `qa.waiver_roles` list.
    #[error("role `{role}` is not authorised to waive QA gates")]
    UnauthorisedRole {
        /// The role that attempted the waiver.
        role: RoleName,
    },
    /// The waiver reason was empty after trim. SPEC §5.12 requires a
    /// non-empty operator-facing reason on every waiver.
    #[error("QA waiver requires a non-empty reason")]
    EmptyReason,
    /// `qa.waiver_roles` is empty: the workflow has not authorised any
    /// role to record a waiver. Reject up-front so the operator sees the
    /// real cause rather than a generic "not authorised".
    #[error("workflow has no configured QA waiver roles")]
    NoWaiverRolesConfigured,
}

/// Return `Ok(())` iff the supplied waiver is authorised by config.
///
/// The two checks are orthogonal to the structural waiver invariants on
/// [`crate::Blocker::transition_status`] / [`crate::QaOutcome::try_new`]:
///
/// * `configured_waiver_roles` is the workflow's `qa.waiver_roles` list.
///   Empty means the workflow refuses waivers entirely; the gate fails
///   fast in that case so the operator-facing message names the real
///   cause.
/// * `role` MUST be a member of `configured_waiver_roles`.
/// * `reason` MUST be non-empty after trim.
pub fn validate_qa_waiver(
    configured_waiver_roles: &[RoleName],
    role: &RoleName,
    reason: &str,
) -> Result<(), QaWaiverError> {
    if configured_waiver_roles.is_empty() {
        return Err(QaWaiverError::NoWaiverRolesConfigured);
    }
    if !configured_waiver_roles.iter().any(|r| r == role) {
        return Err(QaWaiverError::UnauthorisedRole { role: role.clone() });
    }
    if reason.trim().is_empty() {
        return Err(QaWaiverError::EmptyReason);
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn snap(id: i64, blocked: i64, ident: &str, role: &str) -> QaBlockerSnapshot {
        QaBlockerSnapshot {
            blocker_id: BlockerId::new(id),
            blocked_work_item: WorkItemId::new(blocked),
            blocked_identifier: ident.into(),
            origin_role: RoleName::new(role),
            reason: None,
        }
    }

    #[test]
    fn policy_default_is_blocks_parent() {
        assert_eq!(QaBlockerPolicy::default(), QaBlockerPolicy::BlocksParent);
        assert!(QaBlockerPolicy::BlocksParent.gates_parent_close());
        assert!(!QaBlockerPolicy::Advisory.gates_parent_close());
    }

    #[test]
    fn policy_round_trips_through_serde_for_every_variant() {
        for policy in QaBlockerPolicy::ALL {
            let json = serde_json::to_string(&policy).unwrap();
            assert_eq!(json, format!("\"{}\"", policy.as_str()));
            let back: QaBlockerPolicy = serde_json::from_str(&json).unwrap();
            assert_eq!(back, policy);
        }
    }

    #[test]
    fn blocks_parent_with_no_open_qa_blockers_passes() {
        check_qa_blockers_at_parent_close(QaBlockerPolicy::BlocksParent, WorkItemId::new(1), &[])
            .unwrap();
    }

    #[test]
    fn blocks_parent_with_open_qa_blocker_rejects_close() {
        let open = [snap(7, 3, "ENG-3", "qa")];
        let err = check_qa_blockers_at_parent_close(
            QaBlockerPolicy::BlocksParent,
            WorkItemId::new(1),
            &open,
        )
        .unwrap_err();
        match err {
            QaParentCloseError::QaBlockersOpen { parent, open } => {
                assert_eq!(parent, WorkItemId::new(1));
                assert_eq!(open.len(), 1);
                assert_eq!(open[0].blocked_identifier, "ENG-3");
            }
        }
    }

    #[test]
    fn advisory_never_gates_close_even_with_open_qa_blockers() {
        let open = [
            snap(1, 10, "ENG-10", "qa"),
            snap(2, 11, "ENG-11", "quality"),
        ];
        check_qa_blockers_at_parent_close(QaBlockerPolicy::Advisory, WorkItemId::new(1), &open)
            .unwrap();
    }

    #[test]
    fn open_qa_blockers_reported_in_input_order() {
        let open = [
            snap(1, 10, "ENG-10", "qa"),
            snap(2, 11, "ENG-11", "qa"),
            snap(3, 12, "ENG-12", "qa"),
        ];
        let err = check_qa_blockers_at_parent_close(
            QaBlockerPolicy::BlocksParent,
            WorkItemId::new(99),
            &open,
        )
        .unwrap_err();
        let QaParentCloseError::QaBlockersOpen { open, .. } = err;
        let idents: Vec<_> = open.iter().map(|b| b.blocked_identifier.as_str()).collect();
        assert_eq!(idents, vec!["ENG-10", "ENG-11", "ENG-12"]);
    }

    #[test]
    fn error_message_names_count_and_policy() {
        let open = [snap(1, 2, "ENG-2", "qa"), snap(2, 3, "ENG-3", "qa")];
        let err = check_qa_blockers_at_parent_close(
            QaBlockerPolicy::BlocksParent,
            WorkItemId::new(1),
            &open,
        )
        .unwrap_err();
        let msg = err.to_string();
        assert!(msg.contains("2"), "msg should include count: {msg}");
        assert!(
            msg.contains("blocks_parent"),
            "msg should name policy: {msg}"
        );
    }

    #[test]
    fn validate_qa_waiver_accepts_configured_role_with_reason() {
        let configured = vec![RoleName::new("platform_lead"), RoleName::new("release")];
        validate_qa_waiver(
            &configured,
            &RoleName::new("platform_lead"),
            "accepted risk",
        )
        .unwrap();
    }

    #[test]
    fn validate_qa_waiver_rejects_unauthorised_role() {
        let configured = vec![RoleName::new("platform_lead")];
        let err = validate_qa_waiver(&configured, &RoleName::new("ghost"), "ok").unwrap_err();
        assert_eq!(
            err,
            QaWaiverError::UnauthorisedRole {
                role: RoleName::new("ghost"),
            }
        );
    }

    #[test]
    fn validate_qa_waiver_rejects_empty_reason() {
        let configured = vec![RoleName::new("platform_lead")];
        for reason in ["", "   ", "\n\t"] {
            let err = validate_qa_waiver(&configured, &RoleName::new("platform_lead"), reason)
                .unwrap_err();
            assert_eq!(err, QaWaiverError::EmptyReason);
        }
    }

    #[test]
    fn validate_qa_waiver_rejects_when_no_roles_configured() {
        let err =
            validate_qa_waiver(&[], &RoleName::new("platform_lead"), "accepted risk").unwrap_err();
        assert_eq!(err, QaWaiverError::NoWaiverRolesConfigured);
    }

    #[test]
    fn validate_qa_waiver_no_roles_takes_priority_over_other_checks() {
        // No roles configured: the operator should see the
        // "workflow has no configured QA waiver roles" message rather
        // than a generic "role not authorised" — the latter would imply
        // a config-level fix that isn't actually wrong.
        let err = validate_qa_waiver(&[], &RoleName::new("anyone"), "").unwrap_err();
        assert_eq!(err, QaWaiverError::NoWaiverRolesConfigured);
    }

    #[test]
    fn snapshot_round_trips_through_json() {
        let s = QaBlockerSnapshot {
            blocker_id: BlockerId::new(5),
            blocked_work_item: WorkItemId::new(2),
            blocked_identifier: "ENG-2".into(),
            origin_role: RoleName::new("qa"),
            reason: Some("regression".into()),
        };
        let json = serde_json::to_string(&s).unwrap();
        let back: QaBlockerSnapshot = serde_json::from_str(&json).unwrap();
        assert_eq!(back, s);
    }
}
