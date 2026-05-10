//! Phase 13 deterministic happy-path scenario.
//!
//! This is the first full product-loop consumer: a broad parent is
//! decomposed by the integration owner, two specialist children finish,
//! the owner integrates them into a canonical branch, opens a draft PR,
//! QA passes against that integrated artifact, the PR is marked ready,
//! and only then may the parent close.

use std::collections::{BTreeSet, HashMap};
use std::path::PathBuf;

use symphony_core::{
    AcceptanceCriterionStatus, AcceptanceCriterionTrace, BranchOrWorkspace, ChildKey,
    ChildProposal, ChildSnapshot, DecompositionId, DecompositionProposal, DecompositionStatus,
    FollowupPolicy, GateOperation, Handoff, IntegrationChild, IntegrationGates, IntegrationId,
    IntegrationMergeStrategy, IntegrationRecord, IntegrationRequestCause, IntegrationRunRequest,
    IntegrationStatus, IntegrationWorkspace, PullRequestProvider, PullRequestRecord,
    PullRequestRecordId, PullRequestState, QaDraftPullRequest, QaEvidence, QaOutcome,
    QaRequestCause, QaRunRequest, QaVerdict, QaVerdictId, QaWorkspace, ReadyFor, RoleName, RunRef,
    WorkItemId, WorkItemStatusClass, check_no_open_blockers, check_parent_can_close,
    check_qa_blockers_at_parent_close,
};

const PARENT: i64 = 42;
const CHILD_BACKEND: i64 = 43;
const CHILD_UI: i64 = 44;

fn integration_role() -> RoleName {
    RoleName::new("platform_lead")
}

fn qa_role() -> RoleName {
    RoleName::new("qa")
}

fn child_role() -> RoleName {
    RoleName::new("backend_engineer")
}

fn child(key: &str, title: &str, scope: &str, criteria: &[&str]) -> ChildProposal {
    ChildProposal::try_new(
        key,
        title,
        scope,
        child_role(),
        None,
        criteria.iter().map(|c| (*c).to_string()).collect(),
        BTreeSet::new(),
        true,
        Some(format!("feat/{key}")),
        true,
    )
    .expect("valid child proposal")
}

fn specialist_handoff(identifier: &str, branch: &str, file: &str) -> Handoff {
    Handoff {
        summary: format!("{identifier} implemented and verified"),
        changed_files: vec![file.into()],
        tests_run: vec!["cargo test --workspace".into()],
        verification_evidence: vec![format!("{identifier} unit tests passed")],
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
        reporting_role: Some(child_role()),
        verdict_request: None,
    }
}

fn integration_workspace() -> IntegrationWorkspace {
    IntegrationWorkspace {
        path: PathBuf::from("/tmp/symphony/PROJ-42-integration"),
        strategy: "git_worktree".into(),
        branch: Some("symphony/integration/PROJ-42".into()),
        base_ref: Some("main".into()),
    }
}

fn qa_workspace() -> QaWorkspace {
    QaWorkspace {
        path: PathBuf::from("/tmp/symphony/PROJ-42-integration"),
        strategy: "git_worktree".into(),
        branch: Some("symphony/integration/PROJ-42".into()),
        base_ref: Some("main".into()),
    }
}

#[test]
fn broad_parent_decomposes_integrates_opens_pr_passes_qa_marks_ready_and_closes() {
    let backend = child(
        "backend",
        "Add durable orchestration API",
        "backend state transitions and persistence-facing API",
        &["backend API persists orchestration evidence"],
    );
    let ui = child(
        "operator",
        "Expose operator status view",
        "operator-facing status over durable orchestration state",
        &["operator status shows integration and QA evidence"],
    );

    let mut proposal = DecompositionProposal::try_new(
        DecompositionId::new(1),
        WorkItemId::new(PARENT),
        integration_role(),
        "split parent into backend API and operator surface",
        vec![backend, ui],
        FollowupPolicy::CreateDirectly,
    )
    .expect("integration owner can decompose broad parent");
    assert_eq!(proposal.status, DecompositionStatus::Approved);
    assert_eq!(proposal.roots().len(), 2);

    proposal
        .mark_applied(HashMap::from([
            (ChildKey::new("backend"), WorkItemId::new(CHILD_BACKEND)),
            (ChildKey::new("operator"), WorkItemId::new(CHILD_UI)),
        ]))
        .expect("all proposed children get durable tracker ids");
    assert_eq!(proposal.status, DecompositionStatus::Applied);

    let child_handoffs = [
        specialist_handoff("PROJ-43", "feat/backend", "crates/symphony-core/src/api.rs"),
        specialist_handoff(
            "PROJ-44",
            "feat/operator",
            "crates/symphony-cli/src/status.rs",
        ),
    ];
    for handoff in &child_handoffs {
        handoff
            .validate()
            .expect("specialist handoff is structured");
    }

    let integration_request = IntegrationRunRequest::try_new(
        WorkItemId::new(PARENT),
        "PROJ-42",
        "Ship broad orchestration feature",
        integration_role(),
        IntegrationMergeStrategy::SequentialCherryPick,
        integration_workspace(),
        vec![
            IntegrationChild {
                child_id: WorkItemId::new(CHILD_BACKEND),
                identifier: "PROJ-43".into(),
                title: "Add durable orchestration API".into(),
                status_class: WorkItemStatusClass::Done,
                branch: Some("feat/backend".into()),
                latest_handoff: Some(child_handoffs[0].clone()),
            },
            IntegrationChild {
                child_id: WorkItemId::new(CHILD_UI),
                identifier: "PROJ-44".into(),
                title: "Expose operator status view".into(),
                status_class: WorkItemStatusClass::Done,
                branch: Some("feat/operator".into()),
                latest_handoff: Some(child_handoffs[1].clone()),
            },
        ],
        IntegrationGates::default(),
        0,
        IntegrationRequestCause::AllChildrenTerminal,
    )
    .expect("all children terminal and blocker-free means integration can run");
    assert!(integration_request.has_child_handoffs());

    let integration = IntegrationRecord::try_new(
        IntegrationId::new(10),
        WorkItemId::new(PARENT),
        integration_role(),
        Some(RunRef::new(700)),
        IntegrationStatus::Succeeded,
        IntegrationMergeStrategy::SequentialCherryPick,
        Some("symphony/integration/PROJ-42".into()),
        Some("main".into()),
        Some("abc123".into()),
        Some("/tmp/symphony/PROJ-42-integration".into()),
        vec![WorkItemId::new(CHILD_BACKEND), WorkItemId::new(CHILD_UI)],
        Some("Consolidated two child branches; verification green.".into()),
        vec![],
        vec![],
    )
    .expect("successful integration records canonical branch evidence");
    assert!(integration.is_pass());

    check_no_open_blockers(
        GateOperation::DraftPullRequest,
        WorkItemId::new(PARENT),
        &[],
    )
    .expect("draft PR cannot open with blockers, and there are none");

    let draft_pr = PullRequestRecord::try_new(
        PullRequestRecordId::new(20),
        WorkItemId::new(PARENT),
        integration_role(),
        Some(RunRef::new(700)),
        Some(integration.id),
        PullRequestProvider::Github,
        PullRequestState::Draft,
        Some(123),
        Some("https://github.example/org/repo/pull/123".into()),
        integration.integration_branch.clone().unwrap(),
        Some("main".into()),
        integration.head_sha.clone(),
        "PROJ-42: ship broad orchestration feature".into(),
        integration.summary.clone(),
        Some("checks: success".into()),
    )
    .expect("integration owner opens a draft PR before QA");
    assert_eq!(draft_pr.state, PullRequestState::Draft);

    let qa_request = QaRunRequest::try_new(
        WorkItemId::new(PARENT),
        "PROJ-42",
        "Ship broad orchestration feature",
        qa_role(),
        qa_workspace(),
        Some(integration.id),
        Some(QaDraftPullRequest {
            provider: draft_pr.provider,
            state: draft_pr.state,
            number: draft_pr.number,
            url: draft_pr.url.clone(),
            head_branch: draft_pr.head_branch.clone(),
            base_branch: draft_pr.base_branch.clone(),
            head_sha: draft_pr.head_sha.clone(),
        }),
        vec![
            "backend API persists orchestration evidence".into(),
            "operator status shows integration and QA evidence".into(),
        ],
        vec![
            "crates/symphony-core/src/api.rs".into(),
            "crates/symphony-cli/src/status.rs".into(),
        ],
        draft_pr.ci_status.clone(),
        vec![Handoff {
            summary: integration.summary.clone().unwrap(),
            changed_files: vec![
                "crates/symphony-core/src/api.rs".into(),
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
    .expect("QA receives the final integrated artifact");
    assert!(qa_request.has_draft_pr());

    let qa_outcome = QaOutcome::try_new(
        QaVerdictId::new(30),
        WorkItemId::new(PARENT),
        RunRef::new(800),
        qa_role(),
        QaVerdict::Passed,
        QaEvidence {
            tests_run: vec!["cargo test --workspace".into()],
            changed_files_reviewed: qa_request.changed_files.clone(),
            ci_status: vec!["checks: success".into()],
            ..QaEvidence::default()
        },
        qa_request
            .acceptance_criteria
            .iter()
            .map(|criterion| AcceptanceCriterionTrace {
                criterion: criterion.clone(),
                status: AcceptanceCriterionStatus::Verified,
                evidence: vec!["reviewed integrated branch and CI".into()],
                notes: None,
            })
            .collect(),
        vec![],
        None,
        None,
    )
    .expect("passing QA verdict carries full acceptance trace");
    assert!(qa_outcome.is_pass());
    check_qa_blockers_at_parent_close(
        symphony_core::QaBlockerPolicy::BlocksParent,
        WorkItemId::new(PARENT),
        &[],
    )
    .expect("QA created no blockers, so the QA close gate is open");

    let ready_pr = PullRequestRecord::try_new(
        PullRequestRecordId::new(21),
        WorkItemId::new(PARENT),
        integration_role(),
        Some(RunRef::new(701)),
        Some(integration.id),
        PullRequestProvider::Github,
        PullRequestState::Ready,
        draft_pr.number,
        draft_pr.url.clone(),
        draft_pr.head_branch.clone(),
        draft_pr.base_branch.clone(),
        draft_pr.head_sha.clone(),
        draft_pr.title.clone(),
        draft_pr.body.clone(),
        Some("checks: success".into()),
    )
    .expect("QA pass allows integration owner to mark PR ready");
    assert_eq!(ready_pr.state, PullRequestState::Ready);

    check_parent_can_close(
        WorkItemStatusClass::Done,
        &[
            ChildSnapshot {
                id: WorkItemId::new(CHILD_BACKEND),
                identifier: "PROJ-43".into(),
                status_class: WorkItemStatusClass::Done,
                required: true,
            },
            ChildSnapshot {
                id: WorkItemId::new(CHILD_UI),
                identifier: "PROJ-44".into(),
                status_class: WorkItemStatusClass::Done,
                required: true,
            },
        ],
    )
    .expect("parent done requires all children, integration, QA, and blockers clear");
}
