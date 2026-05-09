//! Integration-owner run request (SPEC v2 §6.4, ARCH §9).
//!
//! When the integration queue (see `symphony_state::IntegrationQueueRepository`)
//! surfaces a parent work item that needs consolidation, the kernel builds an
//! [`IntegrationRunRequest`] and hands it to the integration-owner agent
//! runner. The request bundles every input the owner needs to perform a
//! deterministic consolidation:
//!
//! * the parent work item identity (id, tracker key, title);
//! * the role under which the run executes;
//! * the merge strategy the workflow has chosen (SPEC v2 §5.10);
//! * the canonical integration [`IntegrationWorkspace`] claim — path, branch,
//!   base ref, strategy — so the runner can verify cwd/branch before
//!   launching the agent (SPEC v2 §8.3);
//! * one [`IntegrationChild`] per child that contributes to the consolidation,
//!   carrying the child's status class, branch, and (when present) the latest
//!   structured [`Handoff`] the specialist emitted (ARCH §9 step 5);
//! * the [`IntegrationGates`] policy switches mirrored from
//!   `symphony_config::IntegrationConfig`; and
//! * the open-blocker subtree count plus the [`IntegrationRequestCause`]
//!   surfaced by the queue — recorded so events and tests can show *why* the
//!   item was promoted into integration.
//!
//! This module is the *pure domain* shape: no I/O, no agent invocation, no
//! tracker mutation. Construct via [`IntegrationRunRequest::try_new`] so the
//! cross-field invariants are enforced once at creation time, the same
//! pattern as [`crate::IntegrationRecord::try_new`] and
//! [`crate::DecompositionProposal`]. Phase 8's remaining checklist items
//! wire the request into the agent runner output path and the integration
//! record persistence layer.
//!
//! ## Invariants enforced at construction
//!
//! 1. `owner_role.as_str()` is non-empty.
//! 2. `parent_identifier`, `parent_title` are non-empty.
//! 3. `workspace.path` is non-empty and `workspace.strategy` is non-empty.
//! 4. No duplicate `child_id` in [`IntegrationRunRequest::children`].
//! 5. The parent's own id never appears in `children`.
//! 6. Each child's `identifier` is non-empty and any embedded
//!    [`Handoff`] passes [`Handoff::validate`].
//! 7. When `gates.require_all_children_terminal` is `true` and the queue
//!    cause is [`IntegrationRequestCause::AllChildrenTerminal`], every
//!    child's `status_class` MUST be terminal (`done` or `cancelled`).
//! 8. When `gates.require_no_open_blockers` is `true`, `open_blocker_count`
//!    MUST be `0`.
//! 9. When the cause is [`IntegrationRequestCause::AllChildrenTerminal`],
//!    `children` MUST be non-empty — there is nothing to consolidate
//!    otherwise. [`IntegrationRequestCause::DirectIntegrationRequest`] is
//!    allowed to carry no children (the parent itself is in
//!    `status_class = integration` and the owner consolidates whatever it
//!    finds in the canonical workspace).

use std::path::PathBuf;

use serde::{Deserialize, Serialize};

use crate::handoff::{Handoff, HandoffError};
use crate::integration::IntegrationMergeStrategy;
use crate::role::RoleName;
use crate::work_item::{WorkItemId, WorkItemStatusClass};

/// Why the integration owner is being invoked for this parent.
///
/// Mirrors the `IntegrationQueueCause` enum in `symphony-state` on the wire
/// (same `as_str()` values) so events and tests can correlate the queue
/// reason with the request without depending on the state crate from core.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum IntegrationRequestCause {
    /// The parent work item itself is in `status_class = integration` —
    /// typically because a specialist's handoff signalled
    /// `ready_for: integration` and the kernel promoted it.
    DirectIntegrationRequest,
    /// The parent decomposes into one or more children and every child has
    /// reached a terminal status class. The owner consolidates the
    /// children's outputs.
    AllChildrenTerminal,
}

impl IntegrationRequestCause {
    /// All variants in declaration order.
    pub const ALL: [Self; 2] = [Self::DirectIntegrationRequest, Self::AllChildrenTerminal];

    /// Stable lowercase identifier used in event payloads.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::DirectIntegrationRequest => "direct_integration_request",
            Self::AllChildrenTerminal => "all_children_terminal",
        }
    }
}

impl std::fmt::Display for IntegrationRequestCause {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

/// Slim mirror of `symphony_workspace::WorkspaceClaim` for the integration
/// run request.
///
/// Core must not depend on the workspace crate, so the dispatcher projects
/// the relevant fields onto this struct when building a request.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct IntegrationWorkspace {
    /// Absolute path to the canonical integration workspace.
    pub path: PathBuf,
    /// Strategy in force (e.g. `git_worktree`, `existing_worktree`,
    /// `shared_branch`). Recorded so the integration record can capture
    /// *how* consolidation was set up.
    pub strategy: String,
    /// Branch the run must be on at agent-launch time, when applicable.
    /// `None` for strategies without a branch (e.g. plain directory).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub branch: Option<String>,
    /// Base ref the integration branch was cut from (e.g. `main`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub base_ref: Option<String>,
}

/// One child contributing to a consolidation.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct IntegrationChild {
    /// Durable child id.
    pub child_id: WorkItemId,
    /// Tracker-facing identifier (e.g. `ENG-7`).
    pub identifier: String,
    /// Child title.
    pub title: String,
    /// Child's normalized status class at request-build time.
    pub status_class: WorkItemStatusClass,
    /// Branch carrying the child's contribution, when known.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub branch: Option<String>,
    /// Latest structured handoff the child emitted, when persisted. Allows
    /// the integration owner to read the specialist's summary, changed-files
    /// list, tests run, and `ready_for` hint without re-fetching state.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub latest_handoff: Option<Handoff>,
}

/// Policy switches mirrored from `symphony_config::IntegrationConfig`.
///
/// Duplicated as a lightweight value type so core can enforce the gate
/// invariants without taking a dep on the config crate.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct IntegrationGates {
    /// SPEC v2 §5.10 `require_all_children_terminal`. When `true`, the
    /// request rejects any child whose status class is not terminal.
    pub require_all_children_terminal: bool,
    /// SPEC v2 §5.10 `require_no_open_blockers`. When `true`, the request
    /// rejects construction if `open_blocker_count > 0`.
    pub require_no_open_blockers: bool,
}

impl Default for IntegrationGates {
    fn default() -> Self {
        Self {
            require_all_children_terminal: true,
            require_no_open_blockers: true,
        }
    }
}

/// Invariant violations for [`IntegrationRunRequest::try_new`].
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum IntegrationRequestError {
    /// The owning role's name was empty/whitespace.
    #[error("owner_role must not be empty")]
    EmptyOwnerRole,
    /// The parent identifier was empty/whitespace.
    #[error("parent_identifier must not be empty")]
    EmptyParentIdentifier,
    /// The parent title was empty/whitespace.
    #[error("parent_title must not be empty")]
    EmptyParentTitle,
    /// The workspace path was empty.
    #[error("workspace.path must not be empty")]
    EmptyWorkspacePath,
    /// The workspace strategy label was empty/whitespace.
    #[error("workspace.strategy must not be empty")]
    EmptyWorkspaceStrategy,
    /// A child's tracker identifier was empty/whitespace.
    #[error("child {0} has an empty identifier")]
    EmptyChildIdentifier(WorkItemId),
    /// A child appeared more than once in `children`.
    #[error("duplicate child {0} in integration request")]
    DuplicateChild(WorkItemId),
    /// The parent id appeared in `children`.
    #[error("integration request parent {0} cannot consolidate itself")]
    SelfConsolidation(WorkItemId),
    /// `gates.require_all_children_terminal` is on and a child has a
    /// non-terminal status class. The kernel must not promote the parent
    /// to integration in that case.
    #[error("child {child_id} has non-terminal status class {status_class}")]
    ChildNotTerminal {
        /// Offending child's id.
        child_id: WorkItemId,
        /// Status class observed.
        status_class: WorkItemStatusClass,
    },
    /// `gates.require_no_open_blockers` is on and the subtree has open
    /// blockers. The kernel must resolve those before invoking the owner.
    #[error("integration request has {count} open blockers; must be zero")]
    OpenBlockersPresent {
        /// Open blocker count from the queue.
        count: u32,
    },
    /// `cause: all_children_terminal` was passed with an empty `children`
    /// list. There is nothing to consolidate.
    #[error("cause `all_children_terminal` requires at least one child")]
    NoChildrenForDecomposed,
    /// An embedded latest handoff failed [`Handoff::validate`].
    #[error("child {child_id} latest handoff is malformed: {source}")]
    InvalidChildHandoff {
        /// Child carrying the malformed handoff.
        child_id: WorkItemId,
        /// Underlying handoff validation error.
        #[source]
        source: HandoffError,
    },
}

/// Integration-owner run request (SPEC v2 §6.4, ARCH §9).
///
/// Construct via [`Self::try_new`] so the cross-field invariants are checked
/// once. The struct is plain serializable data so the dispatcher can persist
/// it for replay and diagnostics.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct IntegrationRunRequest {
    /// Parent work item being integrated.
    pub parent_id: WorkItemId,
    /// Tracker-facing identifier for the parent (e.g. `ENG-42`).
    pub parent_identifier: String,
    /// Parent title.
    pub parent_title: String,
    /// Role the run executes under. Typically the workflow's
    /// `integration_owner` role.
    pub owner_role: RoleName,
    /// Merge strategy the owner should use.
    pub merge_strategy: IntegrationMergeStrategy,
    /// Canonical integration workspace claim summary.
    pub workspace: IntegrationWorkspace,
    /// Children contributing to the consolidation. Order is significant
    /// for [`IntegrationMergeStrategy::SequentialCherryPick`]; the kernel
    /// preserves it.
    #[serde(default)]
    pub children: Vec<IntegrationChild>,
    /// Policy switches mirrored from `IntegrationConfig`.
    pub gates: IntegrationGates,
    /// Open-blocker count across the parent's subtree at request-build
    /// time. Used by `gates.require_no_open_blockers`.
    pub open_blocker_count: u32,
    /// Why the queue surfaced this parent.
    pub cause: IntegrationRequestCause,
}

impl IntegrationRunRequest {
    /// Construct a request, enforcing every invariant listed in the module
    /// docs. Returns the offending [`IntegrationRequestError`] on the first
    /// violation.
    #[allow(clippy::too_many_arguments)]
    pub fn try_new(
        parent_id: WorkItemId,
        parent_identifier: impl Into<String>,
        parent_title: impl Into<String>,
        owner_role: RoleName,
        merge_strategy: IntegrationMergeStrategy,
        workspace: IntegrationWorkspace,
        children: Vec<IntegrationChild>,
        gates: IntegrationGates,
        open_blocker_count: u32,
        cause: IntegrationRequestCause,
    ) -> Result<Self, IntegrationRequestError> {
        if owner_role.as_str().trim().is_empty() {
            return Err(IntegrationRequestError::EmptyOwnerRole);
        }
        let parent_identifier = parent_identifier.into();
        let parent_title = parent_title.into();
        if parent_identifier.trim().is_empty() {
            return Err(IntegrationRequestError::EmptyParentIdentifier);
        }
        if parent_title.trim().is_empty() {
            return Err(IntegrationRequestError::EmptyParentTitle);
        }
        if workspace.path.as_os_str().is_empty() {
            return Err(IntegrationRequestError::EmptyWorkspacePath);
        }
        if workspace.strategy.trim().is_empty() {
            return Err(IntegrationRequestError::EmptyWorkspaceStrategy);
        }

        let mut seen: Vec<WorkItemId> = Vec::with_capacity(children.len());
        for child in &children {
            if child.child_id == parent_id {
                return Err(IntegrationRequestError::SelfConsolidation(child.child_id));
            }
            if seen.contains(&child.child_id) {
                return Err(IntegrationRequestError::DuplicateChild(child.child_id));
            }
            seen.push(child.child_id);
            if child.identifier.trim().is_empty() {
                return Err(IntegrationRequestError::EmptyChildIdentifier(
                    child.child_id,
                ));
            }
            if let Some(handoff) = &child.latest_handoff {
                handoff.validate().map_err(|source| {
                    IntegrationRequestError::InvalidChildHandoff {
                        child_id: child.child_id,
                        source,
                    }
                })?;
            }
            if gates.require_all_children_terminal
                && cause == IntegrationRequestCause::AllChildrenTerminal
                && !child.status_class.is_terminal()
            {
                return Err(IntegrationRequestError::ChildNotTerminal {
                    child_id: child.child_id,
                    status_class: child.status_class,
                });
            }
        }

        if cause == IntegrationRequestCause::AllChildrenTerminal && children.is_empty() {
            return Err(IntegrationRequestError::NoChildrenForDecomposed);
        }

        if gates.require_no_open_blockers && open_blocker_count > 0 {
            return Err(IntegrationRequestError::OpenBlockersPresent {
                count: open_blocker_count,
            });
        }

        Ok(Self {
            parent_id,
            parent_identifier,
            parent_title,
            owner_role,
            merge_strategy,
            workspace,
            children,
            gates,
            open_blocker_count,
            cause,
        })
    }

    /// Iterate children in insertion order.
    pub fn children(&self) -> impl Iterator<Item = &IntegrationChild> {
        self.children.iter()
    }

    /// True when at least one child contributes a structured handoff.
    pub fn has_child_handoffs(&self) -> bool {
        self.children.iter().any(|c| c.latest_handoff.is_some())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::handoff::{BranchOrWorkspace, Handoff, ReadyFor};

    fn role() -> RoleName {
        RoleName::new("platform_lead")
    }

    fn workspace() -> IntegrationWorkspace {
        IntegrationWorkspace {
            path: PathBuf::from("/tmp/proj-integration"),
            strategy: "git_worktree".into(),
            branch: Some("symphony/integration/PROJ-42".into()),
            base_ref: Some("main".into()),
        }
    }

    fn child(id: i64, identifier: &str, status: WorkItemStatusClass) -> IntegrationChild {
        IntegrationChild {
            child_id: WorkItemId::new(id),
            identifier: identifier.into(),
            title: format!("child {identifier}"),
            status_class: status,
            branch: Some(format!("symphony/{identifier}")),
            latest_handoff: None,
        }
    }

    fn valid_handoff() -> Handoff {
        Handoff {
            summary: "implemented widget".into(),
            changed_files: vec!["src/widget.rs".into()],
            tests_run: vec!["cargo test --package widget".into()],
            verification_evidence: vec![],
            known_risks: vec![],
            blockers_created: vec![],
            followups_created_or_proposed: vec![],
            branch_or_workspace: BranchOrWorkspace {
                branch: Some("symphony/ENG-2".into()),
                workspace_path: Some("/tmp/proj-eng-2".into()),
                base_ref: Some("main".into()),
            },
            ready_for: ReadyFor::Integration,
            block_reason: None,
            reporting_role: None,
            verdict_request: None,
        }
    }

    fn ok_request() -> IntegrationRunRequest {
        IntegrationRunRequest::try_new(
            WorkItemId::new(1),
            "PROJ-42",
            "consolidate widget work",
            role(),
            IntegrationMergeStrategy::SequentialCherryPick,
            workspace(),
            vec![
                child(2, "ENG-2", WorkItemStatusClass::Done),
                child(3, "ENG-3", WorkItemStatusClass::Done),
            ],
            IntegrationGates::default(),
            0,
            IntegrationRequestCause::AllChildrenTerminal,
        )
        .expect("valid request should construct")
    }

    #[test]
    fn cause_round_trips_via_as_str() {
        for cause in IntegrationRequestCause::ALL {
            let s = cause.as_str();
            let json = serde_json::to_string(&cause).unwrap();
            assert_eq!(json, format!("\"{s}\""));
            let back: IntegrationRequestCause = serde_json::from_str(&json).unwrap();
            assert_eq!(back, cause);
        }
    }

    #[test]
    fn valid_request_round_trips_through_json() {
        let req = ok_request();
        let json = serde_json::to_string(&req).unwrap();
        let back: IntegrationRunRequest = serde_json::from_str(&json).unwrap();
        assert_eq!(back, req);
        assert_eq!(req.children().count(), 2);
        assert!(!req.has_child_handoffs());
    }

    #[test]
    fn empty_owner_role_is_rejected() {
        let err = IntegrationRunRequest::try_new(
            WorkItemId::new(1),
            "PROJ-42",
            "title",
            RoleName::new("   "),
            IntegrationMergeStrategy::MergeCommits,
            workspace(),
            vec![child(2, "ENG-2", WorkItemStatusClass::Done)],
            IntegrationGates::default(),
            0,
            IntegrationRequestCause::AllChildrenTerminal,
        )
        .unwrap_err();
        assert_eq!(err, IntegrationRequestError::EmptyOwnerRole);
    }

    #[test]
    fn empty_parent_identifier_or_title_is_rejected() {
        let err = IntegrationRunRequest::try_new(
            WorkItemId::new(1),
            "  ",
            "title",
            role(),
            IntegrationMergeStrategy::MergeCommits,
            workspace(),
            vec![child(2, "ENG-2", WorkItemStatusClass::Done)],
            IntegrationGates::default(),
            0,
            IntegrationRequestCause::AllChildrenTerminal,
        )
        .unwrap_err();
        assert_eq!(err, IntegrationRequestError::EmptyParentIdentifier);

        let err = IntegrationRunRequest::try_new(
            WorkItemId::new(1),
            "PROJ-42",
            "",
            role(),
            IntegrationMergeStrategy::MergeCommits,
            workspace(),
            vec![child(2, "ENG-2", WorkItemStatusClass::Done)],
            IntegrationGates::default(),
            0,
            IntegrationRequestCause::AllChildrenTerminal,
        )
        .unwrap_err();
        assert_eq!(err, IntegrationRequestError::EmptyParentTitle);
    }

    #[test]
    fn empty_workspace_path_or_strategy_is_rejected() {
        let mut ws = workspace();
        ws.path = PathBuf::new();
        let err = IntegrationRunRequest::try_new(
            WorkItemId::new(1),
            "PROJ-42",
            "title",
            role(),
            IntegrationMergeStrategy::MergeCommits,
            ws,
            vec![child(2, "ENG-2", WorkItemStatusClass::Done)],
            IntegrationGates::default(),
            0,
            IntegrationRequestCause::AllChildrenTerminal,
        )
        .unwrap_err();
        assert_eq!(err, IntegrationRequestError::EmptyWorkspacePath);

        let mut ws = workspace();
        ws.strategy = "  ".into();
        let err = IntegrationRunRequest::try_new(
            WorkItemId::new(1),
            "PROJ-42",
            "title",
            role(),
            IntegrationMergeStrategy::MergeCommits,
            ws,
            vec![child(2, "ENG-2", WorkItemStatusClass::Done)],
            IntegrationGates::default(),
            0,
            IntegrationRequestCause::AllChildrenTerminal,
        )
        .unwrap_err();
        assert_eq!(err, IntegrationRequestError::EmptyWorkspaceStrategy);
    }

    #[test]
    fn duplicate_or_self_child_is_rejected() {
        let err = IntegrationRunRequest::try_new(
            WorkItemId::new(1),
            "PROJ-42",
            "title",
            role(),
            IntegrationMergeStrategy::MergeCommits,
            workspace(),
            vec![
                child(2, "ENG-2", WorkItemStatusClass::Done),
                child(2, "ENG-2", WorkItemStatusClass::Done),
            ],
            IntegrationGates::default(),
            0,
            IntegrationRequestCause::AllChildrenTerminal,
        )
        .unwrap_err();
        assert_eq!(
            err,
            IntegrationRequestError::DuplicateChild(WorkItemId::new(2))
        );

        let err = IntegrationRunRequest::try_new(
            WorkItemId::new(1),
            "PROJ-42",
            "title",
            role(),
            IntegrationMergeStrategy::MergeCommits,
            workspace(),
            vec![child(1, "ENG-1", WorkItemStatusClass::Done)],
            IntegrationGates::default(),
            0,
            IntegrationRequestCause::AllChildrenTerminal,
        )
        .unwrap_err();
        assert_eq!(
            err,
            IntegrationRequestError::SelfConsolidation(WorkItemId::new(1))
        );
    }

    #[test]
    fn empty_child_identifier_is_rejected() {
        let err = IntegrationRunRequest::try_new(
            WorkItemId::new(1),
            "PROJ-42",
            "title",
            role(),
            IntegrationMergeStrategy::MergeCommits,
            workspace(),
            vec![child(2, "  ", WorkItemStatusClass::Done)],
            IntegrationGates::default(),
            0,
            IntegrationRequestCause::AllChildrenTerminal,
        )
        .unwrap_err();
        assert_eq!(
            err,
            IntegrationRequestError::EmptyChildIdentifier(WorkItemId::new(2))
        );
    }

    #[test]
    fn non_terminal_child_rejected_under_decomposed_cause() {
        let err = IntegrationRunRequest::try_new(
            WorkItemId::new(1),
            "PROJ-42",
            "title",
            role(),
            IntegrationMergeStrategy::MergeCommits,
            workspace(),
            vec![
                child(2, "ENG-2", WorkItemStatusClass::Done),
                child(3, "ENG-3", WorkItemStatusClass::Running),
            ],
            IntegrationGates::default(),
            0,
            IntegrationRequestCause::AllChildrenTerminal,
        )
        .unwrap_err();
        assert_eq!(
            err,
            IntegrationRequestError::ChildNotTerminal {
                child_id: WorkItemId::new(3),
                status_class: WorkItemStatusClass::Running,
            }
        );
    }

    #[test]
    fn non_terminal_child_allowed_when_gate_disabled() {
        let req = IntegrationRunRequest::try_new(
            WorkItemId::new(1),
            "PROJ-42",
            "title",
            role(),
            IntegrationMergeStrategy::MergeCommits,
            workspace(),
            vec![child(3, "ENG-3", WorkItemStatusClass::Running)],
            IntegrationGates {
                require_all_children_terminal: false,
                require_no_open_blockers: true,
            },
            0,
            IntegrationRequestCause::AllChildrenTerminal,
        )
        .expect("waived gate accepts non-terminal child");
        assert_eq!(req.children.len(), 1);
    }

    #[test]
    fn open_blockers_rejected_when_gate_on() {
        let err = IntegrationRunRequest::try_new(
            WorkItemId::new(1),
            "PROJ-42",
            "title",
            role(),
            IntegrationMergeStrategy::MergeCommits,
            workspace(),
            vec![child(2, "ENG-2", WorkItemStatusClass::Done)],
            IntegrationGates::default(),
            3,
            IntegrationRequestCause::AllChildrenTerminal,
        )
        .unwrap_err();
        assert_eq!(
            err,
            IntegrationRequestError::OpenBlockersPresent { count: 3 }
        );
    }

    #[test]
    fn empty_children_under_all_children_terminal_is_rejected() {
        let err = IntegrationRunRequest::try_new(
            WorkItemId::new(1),
            "PROJ-42",
            "title",
            role(),
            IntegrationMergeStrategy::MergeCommits,
            workspace(),
            vec![],
            IntegrationGates::default(),
            0,
            IntegrationRequestCause::AllChildrenTerminal,
        )
        .unwrap_err();
        assert_eq!(err, IntegrationRequestError::NoChildrenForDecomposed);
    }

    #[test]
    fn direct_integration_request_allows_no_children() {
        let req = IntegrationRunRequest::try_new(
            WorkItemId::new(1),
            "PROJ-42",
            "title",
            role(),
            IntegrationMergeStrategy::SharedBranch,
            workspace(),
            vec![],
            IntegrationGates::default(),
            0,
            IntegrationRequestCause::DirectIntegrationRequest,
        )
        .expect("direct integration request may have no children");
        assert!(req.children.is_empty());
        assert_eq!(req.cause, IntegrationRequestCause::DirectIntegrationRequest);
    }

    #[test]
    fn embedded_handoff_must_validate() {
        let mut bad = valid_handoff();
        bad.summary.clear();
        let mut child2 = child(2, "ENG-2", WorkItemStatusClass::Done);
        child2.latest_handoff = Some(bad);
        let err = IntegrationRunRequest::try_new(
            WorkItemId::new(1),
            "PROJ-42",
            "title",
            role(),
            IntegrationMergeStrategy::MergeCommits,
            workspace(),
            vec![child2],
            IntegrationGates::default(),
            0,
            IntegrationRequestCause::AllChildrenTerminal,
        )
        .unwrap_err();
        match err {
            IntegrationRequestError::InvalidChildHandoff { child_id, .. } => {
                assert_eq!(child_id, WorkItemId::new(2));
            }
            other => panic!("expected InvalidChildHandoff, got {other:?}"),
        }
    }

    #[test]
    fn has_child_handoffs_reflects_embedded_handoff() {
        let mut child2 = child(2, "ENG-2", WorkItemStatusClass::Done);
        child2.latest_handoff = Some(valid_handoff());
        let req = IntegrationRunRequest::try_new(
            WorkItemId::new(1),
            "PROJ-42",
            "title",
            role(),
            IntegrationMergeStrategy::SequentialCherryPick,
            workspace(),
            vec![child2],
            IntegrationGates::default(),
            0,
            IntegrationRequestCause::AllChildrenTerminal,
        )
        .unwrap();
        assert!(req.has_child_handoffs());
    }

    #[test]
    fn child_order_is_preserved_for_sequential_cherry_pick() {
        let req = IntegrationRunRequest::try_new(
            WorkItemId::new(1),
            "PROJ-42",
            "title",
            role(),
            IntegrationMergeStrategy::SequentialCherryPick,
            workspace(),
            vec![
                child(5, "ENG-5", WorkItemStatusClass::Done),
                child(2, "ENG-2", WorkItemStatusClass::Done),
                child(9, "ENG-9", WorkItemStatusClass::Done),
            ],
            IntegrationGates::default(),
            0,
            IntegrationRequestCause::AllChildrenTerminal,
        )
        .unwrap();
        let ids: Vec<i64> = req.children().map(|c| c.child_id.get()).collect();
        assert_eq!(ids, vec![5, 2, 9]);
    }
}
