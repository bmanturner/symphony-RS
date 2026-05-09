//! Derive durable blocker specs from a QA verdict's blocker requests
//! (SPEC v2 §4.8, §4.9, §5.12).
//!
//! When a QA-gate run emits [`crate::QaVerdict::FailedWithBlockers`] (or
//! attaches advisory blocker requests alongside any failing verdict), the
//! kernel needs to convert those agent-level [`HandoffBlockerRequest`]s
//! into durable [`crate::Blocker`] rows before constructing the
//! [`crate::QaOutcome`] — the outcome's `blockers_created` references
//! ids assigned by the storage seam.
//!
//! This module supplies the pure-domain bridge:
//!
//! 1. [`QaBlockerSpec`] — pre-id shape carrying every field
//!    [`crate::Blocker::try_new`] needs, plus the QA run/role context
//!    captured as [`crate::BlockerOrigin::Run`].
//! 2. [`derive_qa_blockers`] — fold a slice of
//!    [`HandoffBlockerRequest`] into `Vec<QaBlockerSpec>`, applying the
//!    "blocked work item is the QA target" convention from SPEC §4.8 and
//!    rejecting requests that cannot be filed as durable blockers.
//! 3. [`QaBlockerSpec::into_blocker`] — finalise a spec into a
//!    [`crate::Blocker`] once the storage seam has assigned a
//!    [`crate::BlockerId`], using the same invariants as
//!    [`crate::Blocker::try_new`] so request and durable record cannot
//!    drift.
//!
//! The module deliberately rejects requests with no `blocking_id`. SPEC
//! §4.8 requires blockers to identify *what* causes the block; agent
//! requests that omit it are routed through follow-up issue creation
//! (Phase 10) to be filed first, then a fresh blocker request can target
//! the new issue. Letting unresolved requests reach the durable table
//! would either crash on `Blocker::try_new`'s self-edge guard or invent
//! a synthetic blocking id the kernel can never reconcile.

use crate::blocker::{Blocker, BlockerError, BlockerId, BlockerOrigin, BlockerSeverity, RunRef};
use crate::handoff::HandoffBlockerRequest;
use crate::role::RoleName;
use crate::work_item::WorkItemId;

/// Pre-persistence shape for a QA-filed blocker (SPEC v2 §4.8).
///
/// Mirrors [`crate::Blocker`] minus the durable [`BlockerId`] and the
/// lifecycle status (always `open` at creation). Hand to the storage
/// seam to assign an id, then call [`Self::into_blocker`] to obtain the
/// validated durable record.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct QaBlockerSpec {
    /// Work item that *causes* the block. Required: SPEC §4.8 forbids
    /// dangling blockers.
    pub blocking_id: WorkItemId,
    /// Work item that is *waiting* on the blocker. Always the QA target.
    pub blocked_id: WorkItemId,
    /// Operator-facing reason copied from the QA blocker request. Already
    /// validated non-empty by [`derive_qa_blockers`].
    pub reason: String,
    /// QA run + role that authored the blocker. Always
    /// [`BlockerOrigin::Run`] with the QA role attached so SPEC §4.8's
    /// "QA-filed" routing applies.
    pub origin: BlockerOrigin,
    /// Severity hint copied from the request.
    pub severity: BlockerSeverity,
}

impl QaBlockerSpec {
    /// Finalise the spec into a durable [`Blocker`], applying
    /// [`Blocker::try_new`]'s invariants (no self-edge, non-empty reason).
    pub fn into_blocker(self, id: BlockerId) -> Result<Blocker, BlockerError> {
        Blocker::try_new(
            id,
            self.blocking_id,
            self.blocked_id,
            self.reason,
            self.origin,
            self.severity,
        )
    }
}

/// Why [`derive_qa_blockers`] rejected a [`HandoffBlockerRequest`].
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum QaBlockerError {
    /// The request omitted `blocking_id`. SPEC §4.8 requires it; the
    /// kernel routes such requests through follow-up issue creation
    /// before they can become blockers.
    #[error("qa blocker request at index {index} is missing blocking_id")]
    MissingBlockingId {
        /// Position in the input slice for operator diagnostics.
        index: usize,
    },
    /// The request's `reason` was empty after trim.
    #[error("qa blocker request at index {index} has empty reason")]
    EmptyReason {
        /// Position in the input slice for operator diagnostics.
        index: usize,
    },
    /// `blocking_id == blocked_id`. The QA target cannot block itself;
    /// callers must resolve the request through follow-up creation first.
    #[error("qa blocker request at index {index} would self-block work item {work_item}")]
    SelfBlock {
        /// Position in the input slice for operator diagnostics.
        index: usize,
        /// The QA target the request was about to self-block.
        work_item: WorkItemId,
    },
}

/// Derive durable blocker specs for a QA verdict.
///
/// Each [`HandoffBlockerRequest`] becomes a [`QaBlockerSpec`] with:
///
/// * `blocked_id = work_item_id` — the QA target waits on the blocker.
/// * `blocking_id` copied from the request; rejected when `None`.
/// * `origin = BlockerOrigin::Run { run_id, role: Some(qa_role) }` so
///   SPEC §4.8 QA-filed gating applies.
/// * `reason` and `severity` copied from the request.
///
/// Returns an error on the first invalid request so the kernel can
/// surface it to the agent for repair (or route through follow-ups).
pub fn derive_qa_blockers(
    work_item_id: WorkItemId,
    run_id: RunRef,
    qa_role: &RoleName,
    requests: &[HandoffBlockerRequest],
) -> Result<Vec<QaBlockerSpec>, QaBlockerError> {
    let mut specs = Vec::with_capacity(requests.len());
    for (index, request) in requests.iter().enumerate() {
        if request.reason.trim().is_empty() {
            return Err(QaBlockerError::EmptyReason { index });
        }
        let blocking_id = request
            .blocking_id
            .ok_or(QaBlockerError::MissingBlockingId { index })?;
        if blocking_id == work_item_id {
            return Err(QaBlockerError::SelfBlock {
                index,
                work_item: work_item_id,
            });
        }
        specs.push(QaBlockerSpec {
            blocking_id,
            blocked_id: work_item_id,
            reason: request.reason.clone(),
            origin: BlockerOrigin::Run {
                run_id,
                role: Some(qa_role.clone()),
            },
            severity: request.severity,
        });
    }
    Ok(specs)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn req(
        blocking: Option<i64>,
        reason: &str,
        severity: BlockerSeverity,
    ) -> HandoffBlockerRequest {
        HandoffBlockerRequest {
            blocking_id: blocking.map(WorkItemId::new),
            reason: reason.into(),
            severity,
        }
    }

    fn qa_role() -> RoleName {
        RoleName::new("qa")
    }

    #[test]
    fn derives_one_spec_per_request_with_qa_origin() {
        let requests = vec![
            req(Some(101), "tests fail", BlockerSeverity::High),
            req(Some(102), "ui regression", BlockerSeverity::Critical),
        ];
        let specs =
            derive_qa_blockers(WorkItemId::new(50), RunRef::new(7), &qa_role(), &requests).unwrap();
        assert_eq!(specs.len(), 2);
        assert_eq!(specs[0].blocking_id, WorkItemId::new(101));
        assert_eq!(specs[0].blocked_id, WorkItemId::new(50));
        assert_eq!(specs[0].reason, "tests fail");
        assert_eq!(specs[0].severity, BlockerSeverity::High);
        match &specs[0].origin {
            BlockerOrigin::Run { run_id, role } => {
                assert_eq!(*run_id, RunRef::new(7));
                assert_eq!(role.as_ref().unwrap().as_str(), "qa");
            }
            other => panic!("unexpected origin: {other:?}"),
        }
        assert_eq!(specs[1].severity, BlockerSeverity::Critical);
    }

    #[test]
    fn empty_input_yields_empty_specs() {
        let specs =
            derive_qa_blockers(WorkItemId::new(50), RunRef::new(7), &qa_role(), &[]).unwrap();
        assert!(specs.is_empty());
    }

    #[test]
    fn missing_blocking_id_is_rejected_with_index() {
        let requests = vec![
            req(Some(101), "tests fail", BlockerSeverity::Medium),
            req(None, "tbd", BlockerSeverity::Medium),
        ];
        let err = derive_qa_blockers(WorkItemId::new(50), RunRef::new(7), &qa_role(), &requests)
            .unwrap_err();
        assert_eq!(err, QaBlockerError::MissingBlockingId { index: 1 });
    }

    #[test]
    fn empty_reason_is_rejected_with_index() {
        let requests = vec![
            req(Some(101), "tests fail", BlockerSeverity::Medium),
            req(Some(102), "   ", BlockerSeverity::Medium),
        ];
        let err = derive_qa_blockers(WorkItemId::new(50), RunRef::new(7), &qa_role(), &requests)
            .unwrap_err();
        assert_eq!(err, QaBlockerError::EmptyReason { index: 1 });
    }

    #[test]
    fn self_block_is_rejected() {
        let requests = vec![req(Some(50), "self", BlockerSeverity::Medium)];
        let err = derive_qa_blockers(WorkItemId::new(50), RunRef::new(7), &qa_role(), &requests)
            .unwrap_err();
        assert_eq!(
            err,
            QaBlockerError::SelfBlock {
                index: 0,
                work_item: WorkItemId::new(50),
            }
        );
    }

    #[test]
    fn spec_into_blocker_round_trips_invariants() {
        let requests = vec![req(Some(101), "tests fail", BlockerSeverity::High)];
        let mut specs =
            derive_qa_blockers(WorkItemId::new(50), RunRef::new(7), &qa_role(), &requests).unwrap();
        let spec = specs.remove(0);
        let blocker = spec.into_blocker(BlockerId::new(999)).unwrap();
        assert_eq!(blocker.id, BlockerId::new(999));
        assert!(blocker.is_blocking());
        assert_eq!(blocker.blocking_id, WorkItemId::new(101));
        assert_eq!(blocker.blocked_id, WorkItemId::new(50));
        assert_eq!(blocker.severity, BlockerSeverity::High);
    }

    #[test]
    fn severity_default_is_preserved_through_derivation() {
        let requests = vec![HandoffBlockerRequest {
            blocking_id: Some(WorkItemId::new(101)),
            reason: "default sev".into(),
            severity: BlockerSeverity::default(),
        }];
        let specs =
            derive_qa_blockers(WorkItemId::new(50), RunRef::new(7), &qa_role(), &requests).unwrap();
        assert_eq!(specs[0].severity, BlockerSeverity::Medium);
    }

    #[test]
    fn each_request_gets_independent_origin_with_same_run_and_role() {
        let requests = vec![
            req(Some(101), "a", BlockerSeverity::Medium),
            req(Some(102), "b", BlockerSeverity::Medium),
        ];
        let specs = derive_qa_blockers(
            WorkItemId::new(50),
            RunRef::new(42),
            &RoleName::new("quality"),
            &requests,
        )
        .unwrap();
        for spec in &specs {
            match &spec.origin {
                BlockerOrigin::Run { run_id, role } => {
                    assert_eq!(*run_id, RunRef::new(42));
                    assert_eq!(role.as_ref().unwrap().as_str(), "quality");
                }
                other => panic!("unexpected origin: {other:?}"),
            }
        }
    }
}
