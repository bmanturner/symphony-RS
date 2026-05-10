//! Phase 13 deterministic QA-blocker rework scenario.
//!
//! A broad parent is integrated, QA rejects with a structured blocker, the
//! blocked child routes back to its specialist, integration reruns after the
//! blocker is resolved, QA passes, and only then may the parent close.

use std::collections::{BTreeSet, HashMap};
use std::path::PathBuf;

use symphony_core::{
    AcceptanceCriterionStatus, AcceptanceCriterionTrace, BlockerId, BlockerReworkInput,
    BlockerSeverity, BlockerStatus, BranchOrWorkspace, ChildKey, ChildProposal, ChildSnapshot,
    DecompositionId, DecompositionProposal, FollowupPolicy, GateOperation, Handoff,
    HandoffBlockerRequest, IntegrationChild, IntegrationGates, IntegrationId,
    IntegrationMergeStrategy, IntegrationRecord, IntegrationRequestCause, IntegrationRunRequest,
    IntegrationStatus, IntegrationWorkspace, OpenBlockerSnapshot, PullRequestProvider,
    PullRequestRecord, PullRequestRecordId, PullRequestState, QaBlockerPolicy, QaBlockerSnapshot,
    QaDraftPullRequest, QaEvidence, QaOutcome, QaRequestCause, QaReworkDecision, QaRunRequest,
    QaVerdict, QaVerdictId, QaWorkspace, ReadyFor, RoleName, RunRef, WorkItemId,
    WorkItemStatusClass, check_no_open_blockers, check_parent_can_close,
    check_qa_blockers_at_parent_close, collect_open, derive_qa_blockers, derive_qa_rework_routing,
};

const PARENT: i64 = 900;
const CHILD_API: i64 = 901;
const CHILD_CLI: i64 = 902;

fn integration_role() -> RoleName {
    RoleName::new("platform_lead")
}

fn qa_role() -> RoleName {
    RoleName::new("qa")
}

fn api_role() -> RoleName {
    RoleName::new("backend_engineer")
}

fn cli_role() -> RoleName {
    RoleName::new("cli_engineer")
}

fn child(key: &str, role: RoleName, title: &str, criterion: &str) -> ChildProposal {
    ChildProposal::try_new(
        key,
        title,
        format!("Implement {title}"),
        role,
        None,
        vec![criterion.into()],
        BTreeSet::new(),
        true,
        Some(format!("feat/{key}")),
        true,
    )
    .expect("valid child proposal")
}

fn integration_workspace() -> IntegrationWorkspace {
    IntegrationWorkspace {
        path: PathBuf::from("/tmp/symphony/PROJ-900-integration"),
        strategy: "git_worktree".into(),
        branch: Some("symphony/integration/PROJ-900".into()),
        base_ref: Some("main".into()),
    }
}

fn qa_workspace() -> QaWorkspace {
    QaWorkspace {
        path: PathBuf::from("/tmp/symphony/PROJ-900-integration"),
        strategy: "git_worktree".into(),
        branch: Some("symphony/integration/PROJ-900".into()),
        base_ref: Some("main".into()),
    }
}

fn handoff(role: RoleName, identifier: &str, branch: &str, file: &str) -> Handoff {
    Handoff {
        summary: format!("{identifier} implemented"),
        changed_files: vec![file.into()],
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
        reporting_role: Some(role),
        verdict_request: None,
    }
}

fn integration_request(
    children: Vec<IntegrationChild>,
    open_blockers: u32,
) -> IntegrationRunRequest {
    IntegrationRunRequest::try_new(
        WorkItemId::new(PARENT),
        "PROJ-900",
        "Ship gated QA rework loop",
        integration_role(),
        IntegrationMergeStrategy::SequentialCherryPick,
        integration_workspace(),
        children,
        IntegrationGates::default(),
        open_blockers,
        IntegrationRequestCause::AllChildrenTerminal,
    )
    .expect("integration request passes when children are terminal and blockers clear")
}

fn integration_child(
    id: i64,
    identifier: &str,
    role: RoleName,
    branch: &str,
    file: &str,
) -> IntegrationChild {
    IntegrationChild {
        child_id: WorkItemId::new(id),
        identifier: identifier.into(),
        title: format!("{identifier} child"),
        status_class: WorkItemStatusClass::Done,
        branch: Some(branch.into()),
        latest_handoff: Some(handoff(role, identifier, branch, file)),
    }
}

fn draft_pr(
    integration: &IntegrationRecord,
    id: i64,
    state: PullRequestState,
) -> PullRequestRecord {
    PullRequestRecord::try_new(
        PullRequestRecordId::new(id),
        WorkItemId::new(PARENT),
        integration_role(),
        Some(RunRef::new(10_000 + id)),
        Some(integration.id),
        PullRequestProvider::Github,
        state,
        Some(900),
        Some("https://github.example/org/repo/pull/900".into()),
        integration.integration_branch.clone().unwrap(),
        integration.base_ref.clone(),
        integration.head_sha.clone(),
        "PROJ-900: ship gated QA rework loop".into(),
        integration.summary.clone(),
        Some("checks: success".into()),
    )
    .expect("valid pull request record")
}

fn qa_request(integration: &IntegrationRecord, pr: &PullRequestRecord) -> QaRunRequest {
    QaRunRequest::try_new(
        WorkItemId::new(PARENT),
        "PROJ-900",
        "Ship gated QA rework loop",
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
        vec![
            "API preserves blocker evidence".into(),
            "CLI shows rework status".into(),
        ],
        vec![
            "crates/symphony-core/src/blocker.rs".into(),
            "crates/symphony-cli/src/status.rs".into(),
        ],
        pr.ci_status.clone(),
        vec![Handoff {
            summary: integration.summary.clone().unwrap(),
            changed_files: vec![
                "crates/symphony-core/src/blocker.rs".into(),
                "crates/symphony-cli/src/status.rs".into(),
            ],
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

fn integration_record(
    id: i64,
    run: i64,
    summary: &str,
    children: Vec<WorkItemId>,
) -> IntegrationRecord {
    IntegrationRecord::try_new(
        IntegrationId::new(id),
        WorkItemId::new(PARENT),
        integration_role(),
        Some(RunRef::new(run)),
        IntegrationStatus::Succeeded,
        IntegrationMergeStrategy::SequentialCherryPick,
        Some("symphony/integration/PROJ-900".into()),
        Some("main".into()),
        Some(format!("head-{id}")),
        Some("/tmp/symphony/PROJ-900-integration".into()),
        children,
        Some(summary.into()),
        vec![],
        vec![],
    )
    .expect("valid succeeded integration record")
}

#[test]
fn qa_files_blocker_routes_specialist_rework_integration_reruns_and_qa_passes() {
    let mut proposal = DecompositionProposal::try_new(
        DecompositionId::new(90),
        WorkItemId::new(PARENT),
        integration_role(),
        "split parent into API and CLI child issues",
        vec![
            child(
                "api",
                api_role(),
                "durable blocker evidence API",
                "API preserves blocker evidence",
            ),
            child(
                "cli",
                cli_role(),
                "operator rework status",
                "CLI shows rework status",
            ),
        ],
        FollowupPolicy::CreateDirectly,
    )
    .expect("integration owner can decompose broad parent");
    proposal
        .mark_applied(HashMap::from([
            (ChildKey::new("api"), WorkItemId::new(CHILD_API)),
            (ChildKey::new("cli"), WorkItemId::new(CHILD_CLI)),
        ]))
        .expect("child issues are durable");

    let first_children = vec![
        integration_child(
            CHILD_API,
            "PROJ-901",
            api_role(),
            "feat/api-blocker-evidence",
            "crates/symphony-core/src/blocker.rs",
        ),
        integration_child(
            CHILD_CLI,
            "PROJ-902",
            cli_role(),
            "feat/cli-rework-status",
            "crates/symphony-cli/src/status.rs",
        ),
    ];
    let first_request = integration_request(first_children, 0);
    assert!(first_request.has_child_handoffs());

    let first_integration = integration_record(
        100,
        1_000,
        "Integrated initial API and CLI children.",
        vec![WorkItemId::new(CHILD_API), WorkItemId::new(CHILD_CLI)],
    );
    check_no_open_blockers(GateOperation::QaRequest, WorkItemId::new(PARENT), &[])
        .expect("QA request starts only after blocker gate is clear");

    let first_pr = draft_pr(&first_integration, 110, PullRequestState::Draft);
    let first_qa_request = qa_request(&first_integration, &first_pr);
    assert!(first_qa_request.has_draft_pr());

    let blocker_specs = derive_qa_blockers(
        first_qa_request.work_item_id,
        RunRef::new(2_000),
        &first_qa_request.qa_role,
        &[HandoffBlockerRequest {
            blocking_id: Some(WorkItemId::new(CHILD_API)),
            reason: "API loses blocker evidence after restart".into(),
            severity: BlockerSeverity::Critical,
        }],
    )
    .expect("QA can file a structured blocker");
    let blocker = blocker_specs[0]
        .clone()
        .into_blocker(BlockerId::new(500))
        .expect("storage assigns durable blocker id");

    let failed = QaOutcome::try_new(
        QaVerdictId::new(120),
        first_qa_request.work_item_id,
        RunRef::new(2_000),
        qa_role(),
        QaVerdict::FailedWithBlockers,
        QaEvidence {
            tests_run: vec!["cargo test -p symphony-state restart".into()],
            changed_files_reviewed: first_qa_request.changed_files.clone(),
            ci_status: vec!["checks: success".into()],
            ..QaEvidence::default()
        },
        vec![AcceptanceCriterionTrace {
            criterion: "API preserves blocker evidence".into(),
            status: AcceptanceCriterionStatus::Failed,
            evidence: vec!["restart repro dropped the blocker row".into()],
            notes: Some("blocking correctness defect".into()),
        }],
        vec![blocker.id],
        None,
        Some("critical API blocker".into()),
    )
    .expect("failed QA verdict carries blocker ids");
    assert!(failed.verdict.requires_blockers());

    let rework = derive_qa_rework_routing(
        &failed,
        &[],
        &[BlockerReworkInput {
            blocker_id: blocker.id,
            blocked_work_item: WorkItemId::new(CHILD_API),
            blocked_assigned_role: Some(api_role()),
        }],
    )
    .expect("QA blocker routes to the blocked specialist");
    match rework {
        QaReworkDecision::BlockersFiled {
            next_status_class,
            routes,
            ..
        } => {
            assert_eq!(next_status_class, WorkItemStatusClass::Rework);
            assert_eq!(routes.len(), 1);
            assert_eq!(routes[0].blocked_work_item, WorkItemId::new(CHILD_API));
            assert_eq!(routes[0].target_role.as_ref().unwrap(), &api_role());
        }
        other => panic!("expected blocker rework routing, got {other:?}"),
    }

    let open_qa_blockers = vec![QaBlockerSnapshot {
        blocker_id: blocker.id,
        blocked_work_item: WorkItemId::new(CHILD_API),
        blocked_identifier: "PROJ-901".into(),
        origin_role: qa_role(),
        reason: Some(blocker.reason.clone()),
    }];
    check_qa_blockers_at_parent_close(
        QaBlockerPolicy::BlocksParent,
        WorkItemId::new(PARENT),
        &open_qa_blockers,
    )
    .expect_err("QA-created blockers block parent completion");

    let resolved = collect_open(vec![(
        BlockerStatus::Resolved,
        OpenBlockerSnapshot {
            blocker_id: blocker.id,
            blocked_work_item: WorkItemId::new(CHILD_API),
            blocked_identifier: "PROJ-901".into(),
            reason: Some(blocker.reason.clone()),
        },
    )]);
    assert!(resolved.is_empty());
    check_no_open_blockers(GateOperation::QaRequest, WorkItemId::new(PARENT), &resolved)
        .expect("resolved QA blocker allows integration to rerun");

    let reworked_children = vec![
        integration_child(
            CHILD_API,
            "PROJ-901",
            api_role(),
            "feat/api-blocker-evidence-v2",
            "crates/symphony-core/src/blocker.rs",
        ),
        integration_child(
            CHILD_CLI,
            "PROJ-902",
            cli_role(),
            "feat/cli-rework-status",
            "crates/symphony-cli/src/status.rs",
        ),
    ];
    let second_request = integration_request(reworked_children, resolved.len() as u32);
    assert_eq!(
        second_request.cause,
        IntegrationRequestCause::AllChildrenTerminal
    );

    let second_integration = integration_record(
        101,
        1_001,
        "Re-integrated API rework and preserved CLI child output.",
        vec![WorkItemId::new(CHILD_API), WorkItemId::new(CHILD_CLI)],
    );
    assert!(second_integration.is_pass());

    let second_pr = draft_pr(&second_integration, 111, PullRequestState::Draft);
    let second_qa_request = qa_request(&second_integration, &second_pr);
    let passed = QaOutcome::try_new(
        QaVerdictId::new(121),
        second_qa_request.work_item_id,
        RunRef::new(2_001),
        qa_role(),
        QaVerdict::Passed,
        QaEvidence {
            tests_run: vec!["cargo test --workspace".into()],
            changed_files_reviewed: second_qa_request.changed_files.clone(),
            ci_status: vec!["checks: success".into()],
            ..QaEvidence::default()
        },
        second_qa_request
            .acceptance_criteria
            .iter()
            .map(|criterion| AcceptanceCriterionTrace {
                criterion: criterion.clone(),
                status: AcceptanceCriterionStatus::Verified,
                evidence: vec!["verified after specialist rework".into()],
                notes: None,
            })
            .collect(),
        vec![],
        None,
        None,
    )
    .expect("QA pass after rework carries verified criteria");
    assert!(passed.is_pass());

    let ready_pr = draft_pr(&second_integration, 112, PullRequestState::Ready);
    assert_eq!(ready_pr.state, PullRequestState::Ready);
    check_qa_blockers_at_parent_close(QaBlockerPolicy::BlocksParent, WorkItemId::new(PARENT), &[])
        .expect("resolved QA blocker lets the QA close gate open");
    check_parent_can_close(
        WorkItemStatusClass::Done,
        &[
            ChildSnapshot {
                id: WorkItemId::new(CHILD_API),
                identifier: "PROJ-901".into(),
                status_class: WorkItemStatusClass::Done,
                required: true,
            },
            ChildSnapshot {
                id: WorkItemId::new(CHILD_CLI),
                identifier: "PROJ-902".into(),
                status_class: WorkItemStatusClass::Done,
                required: true,
            },
        ],
    )
    .expect("parent done is gated by children, integration, resolved blockers, and QA pass");
}
