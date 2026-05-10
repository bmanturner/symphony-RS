//! Phase 13 deterministic non-blocking follow-up scenario.
//!
//! A specialist finishes current work and files an adjacent non-blocking
//! follow-up. The follow-up is durable and linked, but it does not block
//! integration, QA, PR readiness, or parent close.

use std::collections::{BTreeSet, HashMap};
use std::path::PathBuf;

use symphony_core::{
    AcceptanceCriterionStatus, AcceptanceCriterionTrace, BranchOrWorkspace, ChildKey,
    ChildProposal, ChildSnapshot, DecompositionId, DecompositionProposal,
    DependencyApplicationEvidence, FollowupId, FollowupPolicy, FollowupRequestInput,
    FollowupRouteDecision, FollowupStatus, Handoff, HandoffFollowupRequest, IntegrationChild,
    IntegrationGates, IntegrationId, IntegrationMergeStrategy, IntegrationRecord,
    IntegrationRequestCause, IntegrationRunRequest, IntegrationStatus, IntegrationWorkspace,
    PullRequestProvider, PullRequestRecord, PullRequestRecordId, PullRequestState,
    QaDraftPullRequest, QaEvidence, QaOutcome, QaRequestCause, QaRunRequest, QaVerdict,
    QaVerdictId, QaWorkspace, ReadyFor, RoleAuthority, RoleKind, RoleName, RunRef, WorkItemId,
    WorkItemStatusClass, check_blocking_followups_at_parent_close, check_parent_can_close,
    derive_followups, route_followup,
};

const PARENT: i64 = 920;
const CHILD: i64 = 921;
const FOLLOWUP: i64 = 9_200;
const FOLLOWUP_TRACKER: i64 = 9_201;

fn integration_role() -> RoleName {
    RoleName::new("platform_lead")
}

fn qa_role() -> RoleName {
    RoleName::new("qa")
}

fn specialist_role() -> RoleName {
    RoleName::new("backend_engineer")
}

fn integration_workspace() -> IntegrationWorkspace {
    IntegrationWorkspace {
        path: PathBuf::from("/tmp/symphony/PROJ-920-integration"),
        strategy: "git_worktree".into(),
        branch: Some("symphony/integration/PROJ-920".into()),
        base_ref: Some("main".into()),
    }
}

fn qa_workspace() -> QaWorkspace {
    QaWorkspace {
        path: PathBuf::from("/tmp/symphony/PROJ-920-integration"),
        strategy: "git_worktree".into(),
        branch: Some("symphony/integration/PROJ-920".into()),
        base_ref: Some("main".into()),
    }
}

fn specialist_handoff(followup: HandoffFollowupRequest) -> Handoff {
    Handoff {
        summary: "Specialist implemented durable retry telemetry.".into(),
        changed_files: vec!["crates/symphony-core/src/retry.rs".into()],
        tests_run: vec!["cargo test -p symphony-core retry".into()],
        verification_evidence: vec!["retry telemetry unit tests passed".into()],
        known_risks: vec![],
        blockers_created: vec![],
        followups_created_or_proposed: vec![followup],
        branch_or_workspace: BranchOrWorkspace {
            branch: Some("feat/retry-telemetry".into()),
            workspace_path: Some("/tmp/symphony/PROJ-921".into()),
            base_ref: Some("main".into()),
        },
        ready_for: ReadyFor::Integration,
        block_reason: None,
        reporting_role: Some(specialist_role()),
        verdict_request: None,
    }
}

fn integration_record() -> IntegrationRecord {
    IntegrationRecord::try_new(
        IntegrationId::new(300),
        WorkItemId::new(PARENT),
        integration_role(),
        Some(RunRef::new(3_000)),
        IntegrationStatus::Succeeded,
        IntegrationMergeStrategy::SequentialCherryPick,
        Some("symphony/integration/PROJ-920".into()),
        Some("main".into()),
        Some("head-920".into()),
        Some("/tmp/symphony/PROJ-920-integration".into()),
        vec![WorkItemId::new(CHILD)],
        Some("Integrated retry telemetry child.".into()),
        vec![],
        vec![],
    )
    .expect("valid integration record")
}

fn draft_pr(integration: &IntegrationRecord, state: PullRequestState) -> PullRequestRecord {
    PullRequestRecord::try_new(
        PullRequestRecordId::new(310),
        WorkItemId::new(PARENT),
        integration_role(),
        Some(RunRef::new(3_100)),
        Some(integration.id),
        PullRequestProvider::Github,
        state,
        Some(920),
        Some("https://github.example/org/repo/pull/920".into()),
        integration.integration_branch.clone().unwrap(),
        integration.base_ref.clone(),
        integration.head_sha.clone(),
        "PROJ-920: ship retry telemetry".into(),
        integration.summary.clone(),
        Some("checks: success".into()),
    )
    .expect("valid pull request record")
}

fn qa_request(integration: &IntegrationRecord, pr: &PullRequestRecord) -> QaRunRequest {
    QaRunRequest::try_new(
        WorkItemId::new(PARENT),
        "PROJ-920",
        "Ship retry telemetry",
        qa_role(),
        qa_workspace(),
        Some(integration.id),
        Some(QaDraftPullRequest {
            provider: pr.provider,
            state: pr.state,
            number: pr.number,
            url: pr.url.clone(),
            head_branch: pr.head_branch.clone(),
            base_branch: pr.base_branch.clone(),
            head_sha: pr.head_sha.clone(),
        }),
        vec!["Retry telemetry records attempts without changing behavior".into()],
        vec!["crates/symphony-core/src/retry.rs".into()],
        pr.ci_status.clone(),
        vec![Handoff {
            summary: integration.summary.clone().unwrap(),
            changed_files: vec!["crates/symphony-core/src/retry.rs".into()],
            tests_run: vec!["cargo test --workspace".into()],
            verification_evidence: vec!["checks: success".into()],
            known_risks: vec![],
            blockers_created: vec![],
            followups_created_or_proposed: vec![],
            branch_or_workspace: BranchOrWorkspace {
                branch: integration.integration_branch.clone(),
                workspace_path: integration.workspace_path.clone(),
                base_ref: integration.base_ref.clone(),
            },
            ready_for: ReadyFor::Qa,
            block_reason: None,
            reporting_role: Some(integration_role()),
            verdict_request: None,
        }],
        QaRequestCause::IntegrationConsolidated,
    )
    .expect("QA receives integrated branch and draft PR")
}

#[test]
fn specialist_non_blocking_followup_is_linked_while_current_work_reaches_done() {
    let mut proposal = DecompositionProposal::try_new(
        DecompositionId::new(920),
        WorkItemId::new(PARENT),
        integration_role(),
        "single backend child owns retry telemetry",
        vec![
            ChildProposal::try_new(
                "backend",
                "retry telemetry",
                "Record retry attempts without changing retry behavior",
                specialist_role(),
                None,
                vec!["Retry telemetry records attempts without changing behavior".into()],
                BTreeSet::new(),
                true,
                Some("feat/retry-telemetry".into()),
                true,
            )
            .expect("valid child proposal"),
        ],
        FollowupPolicy::CreateDirectly,
    )
    .expect("integration owner can decompose parent");
    proposal
        .mark_applied(
            HashMap::from([(ChildKey::new("backend"), WorkItemId::new(CHILD))]),
            DependencyApplicationEvidence::none_required(),
        )
        .expect("child issue is durable");

    let followup_request = HandoffFollowupRequest {
        title: "Add retry jitter histogram".into(),
        summary: "Telemetry shipped attempts; histogram buckets are useful but outside acceptance."
            .into(),
        blocking: false,
        propose_only: false,
    };
    let handoff = specialist_handoff(followup_request.clone());
    handoff
        .validate()
        .expect("specialist handoff with follow-up is valid");

    let followup_input = FollowupRequestInput::from_handoff(
        &followup_request,
        "retry observability extension only",
        vec!["histogram buckets are visible in status output".into()],
    );
    let specs = derive_followups(
        WorkItemId::new(CHILD),
        RunRef::new(2_920),
        &specialist_role(),
        &RoleAuthority::defaults_for(RoleKind::Specialist),
        FollowupPolicy::CreateDirectly,
        true,
        std::slice::from_ref(&followup_input),
    )
    .expect("specialist may emit follow-ups");
    assert_eq!(specs.len(), 1);
    assert!(!specs[0].blocking);
    assert_eq!(
        route_followup(&specs[0], None).unwrap(),
        FollowupRouteDecision::CreateDirectly
    );

    let mut durable_followup = specs[0]
        .clone()
        .into_followup(FollowupId::new(FOLLOWUP))
        .expect("storage finalises follow-up after assigning id");
    assert_eq!(durable_followup.status, FollowupStatus::Approved);
    durable_followup
        .mark_created(WorkItemId::new(FOLLOWUP_TRACKER))
        .expect("direct follow-up creates tracker issue");
    let link = durable_followup
        .source_link()
        .expect("created follow-up exposes provenance edge");
    assert_eq!(link.source_work_item, WorkItemId::new(CHILD));
    assert_eq!(link.followup_work_item, WorkItemId::new(FOLLOWUP_TRACKER));
    assert!(!link.blocking);

    check_blocking_followups_at_parent_close(WorkItemId::new(PARENT), &[])
        .expect("non-blocking follow-up is not part of parent close blocker snapshot");

    let child = IntegrationChild {
        child_id: WorkItemId::new(CHILD),
        identifier: "PROJ-921".into(),
        title: "retry telemetry".into(),
        status_class: WorkItemStatusClass::Done,
        branch: Some("feat/retry-telemetry".into()),
        latest_handoff: Some(handoff),
    };
    let integration_request = IntegrationRunRequest::try_new(
        WorkItemId::new(PARENT),
        "PROJ-920",
        "Ship retry telemetry",
        integration_role(),
        IntegrationMergeStrategy::SequentialCherryPick,
        integration_workspace(),
        vec![child],
        IntegrationGates::default(),
        0,
        IntegrationRequestCause::AllChildrenTerminal,
    )
    .expect("non-blocking follow-up does not prevent integration");
    assert!(integration_request.has_child_handoffs());

    let integration = integration_record();
    assert!(integration.is_pass());
    let draft = draft_pr(&integration, PullRequestState::Draft);
    let qa = qa_request(&integration, &draft);
    assert!(qa.has_draft_pr());
    let verdict = QaOutcome::try_new(
        QaVerdictId::new(320),
        WorkItemId::new(PARENT),
        RunRef::new(3_200),
        qa_role(),
        QaVerdict::Passed,
        QaEvidence {
            tests_run: vec!["cargo test --workspace".into()],
            changed_files_reviewed: qa.changed_files.clone(),
            ci_status: vec!["checks: success".into()],
            ..QaEvidence::default()
        },
        qa.acceptance_criteria
            .iter()
            .map(|criterion| AcceptanceCriterionTrace {
                criterion: criterion.clone(),
                status: AcceptanceCriterionStatus::Verified,
                evidence: vec!["verified on integrated branch".into()],
                notes: None,
            })
            .collect(),
        vec![],
        None,
        None,
    )
    .expect("QA pass carries verified acceptance trace");
    assert!(verdict.is_pass());

    let ready = draft_pr(&integration, PullRequestState::Ready);
    assert_eq!(ready.state, PullRequestState::Ready);
    check_parent_can_close(
        WorkItemStatusClass::Done,
        &[ChildSnapshot {
            id: WorkItemId::new(CHILD),
            identifier: "PROJ-921".into(),
            status_class: WorkItemStatusClass::Done,
            required: true,
        }],
    )
    .expect("parent closes after child, integration, QA, and PR readiness despite follow-up");
}
