//! Phase 13 deterministic unresolved-child closeout scenario.
//!
//! The integration owner is the only role allowed to close broad parent work,
//! but that authority is gated by dependency truth. A parent with any required
//! child still in flight cannot be promoted into integration and cannot be
//! marked done by a direct closeout attempt.

use std::collections::{BTreeSet, HashMap};
use std::path::PathBuf;

use symphony_core::{
    BranchOrWorkspace, ChildKey, ChildProposal, ChildSnapshot, DecompositionId,
    DecompositionProposal, DependencyApplicationEvidence, FollowupPolicy, Handoff,
    IntegrationChild, IntegrationGates, IntegrationMergeStrategy, IntegrationRequestCause,
    IntegrationRequestError, IntegrationRunRequest, IntegrationWorkspace, ParentCloseError,
    ReadyFor, RoleName, WorkItemId, WorkItemStatusClass, check_parent_can_close,
};

const PARENT: i64 = 1_300;
const CHILD_API: i64 = 1_301;
const CHILD_CLI: i64 = 1_302;

fn integration_role() -> RoleName {
    RoleName::new("platform_lead")
}

fn specialist_role() -> RoleName {
    RoleName::new("backend_engineer")
}

fn child(key: &str, title: &str) -> ChildProposal {
    ChildProposal::try_new(
        key,
        title,
        format!("Implement {title}"),
        specialist_role(),
        None,
        vec![format!("{title} is complete")],
        BTreeSet::new(),
        true,
        Some(format!("feat/{key}")),
        true,
    )
    .expect("valid child proposal")
}

fn workspace() -> IntegrationWorkspace {
    IntegrationWorkspace {
        path: PathBuf::from("/tmp/symphony/PROJ-1300-integration"),
        strategy: "git_worktree".into(),
        branch: Some("symphony/integration/PROJ-1300".into()),
        base_ref: Some("main".into()),
    }
}

fn handoff(identifier: &str, branch: &str) -> Handoff {
    Handoff {
        summary: format!("{identifier} complete"),
        changed_files: vec![format!("src/{identifier}.rs")],
        tests_run: vec!["cargo test -p symphony-core".into()],
        verification_evidence: vec![format!("{identifier} focused tests passed")],
        known_risks: vec![],
        blockers_created: vec![],
        followups_created_or_proposed: vec![],
        branch_or_workspace: BranchOrWorkspace {
            branch: Some(branch.into()),
            workspace_path: Some(format!("/tmp/{identifier}")),
            base_ref: Some("main".into()),
        },
        ready_for: ReadyFor::Integration,
        block_reason: None,
        reporting_role: Some(specialist_role()),
        verdict_request: None,
    }
}

fn integration_child(
    id: i64,
    identifier: &str,
    status_class: WorkItemStatusClass,
) -> IntegrationChild {
    IntegrationChild {
        child_id: WorkItemId::new(id),
        identifier: identifier.into(),
        title: format!("{identifier} child"),
        status_class,
        branch: Some(format!("feat/{identifier}")),
        latest_handoff: status_class
            .is_terminal()
            .then(|| handoff(identifier, &format!("feat/{identifier}"))),
    }
}

#[test]
fn integration_owner_cannot_close_parent_with_unresolved_required_child() {
    let mut proposal = DecompositionProposal::try_new(
        DecompositionId::new(130),
        WorkItemId::new(PARENT),
        integration_role(),
        "split parent into API and CLI children",
        vec![
            child("api", "durable parent close API"),
            child("cli", "operator closeout surface"),
        ],
        FollowupPolicy::CreateDirectly,
    )
    .expect("integration owner can decompose broad parent");
    proposal
        .mark_applied(
            HashMap::from([
                (ChildKey::new("api"), WorkItemId::new(CHILD_API)),
                (ChildKey::new("cli"), WorkItemId::new(CHILD_CLI)),
            ]),
            DependencyApplicationEvidence::none_required(),
        )
        .expect("child issues are durable");

    let integration_err = IntegrationRunRequest::try_new(
        WorkItemId::new(PARENT),
        "PROJ-1300",
        "Ship gated parent closeout",
        integration_role(),
        IntegrationMergeStrategy::SequentialCherryPick,
        workspace(),
        vec![
            integration_child(CHILD_API, "PROJ-1301", WorkItemStatusClass::Done),
            integration_child(CHILD_CLI, "PROJ-1302", WorkItemStatusClass::Running),
        ],
        IntegrationGates::default(),
        0,
        IntegrationRequestCause::AllChildrenTerminal,
    )
    .expect_err("integration owner cannot run while a required child is unresolved");
    assert_eq!(
        integration_err,
        IntegrationRequestError::ChildNotTerminal {
            child_id: WorkItemId::new(CHILD_CLI),
            status_class: WorkItemStatusClass::Running,
        }
    );

    let close_err = check_parent_can_close(
        WorkItemStatusClass::Done,
        &[
            ChildSnapshot {
                id: WorkItemId::new(CHILD_API),
                identifier: "PROJ-1301".into(),
                status_class: WorkItemStatusClass::Done,
                required: true,
            },
            ChildSnapshot {
                id: WorkItemId::new(CHILD_CLI),
                identifier: "PROJ-1302".into(),
                status_class: WorkItemStatusClass::Running,
                required: true,
            },
        ],
    )
    .expect_err("parent done is gated by every required child");

    let ParentCloseError::ChildrenNotTerminal { non_terminal } = close_err else {
        panic!("expected unresolved child closeout error");
    };
    assert_eq!(non_terminal.len(), 1);
    assert_eq!(non_terminal[0].id, WorkItemId::new(CHILD_CLI));
    assert_eq!(non_terminal[0].identifier, "PROJ-1302");
    assert_eq!(non_terminal[0].status_class, WorkItemStatusClass::Running);
}
