//! Phase 9 cross-cutting QA-gate flow tests (CHECKLIST_v2 Phase 9).
//!
//! Module-level tests under `qa`, `qa_blocker`, `qa_blocker_policy`, and
//! `qa_rework` exercise individual invariants in isolation. This suite walks
//! a QA run *through the whole Phase 9 contract* using only public domain
//! types so a future change that breaks any seam between them — request
//! construction, blocker derivation, verdict invariants, rework routing,
//! parent-close gating, or waiver authorisation — surfaces here, in one
//! place, instead of requiring an archaeologist across four files to
//! reconstruct the flow.
//!
//! The four scenarios mirror the SPEC v2 §4.9 verdict family:
//!
//! 1. **QA pass** — `passed` verdict on a consolidated integration; no
//!    rework, no blockers, parent close gate is open.
//! 2. **QA fail with blockers** — `failed_with_blockers` verdict files
//!    durable blockers, derives one rework route per blocker, and gates
//!    parent close until the QA blocker subtree is empty.
//! 3. **Inconclusive verdict** — `inconclusive` verdict keeps the work item
//!    in the QA queue; no blockers, no rework routing, no parent advance.
//! 4. **Waiver policy** — `waived` verdict requires a configured waiver
//!    role and a non-empty reason; the policy gate then accepts close.
//!
//! The flow exercised, mirroring SPEC v2 §6.5 / ARCH §10:
//!
//! ```text
//! QA queue surfaces work item
//!     → QaRunRequest::try_new (workspace + handoffs + acceptance trace)
//!     → derive_qa_blockers (HandoffBlockerRequest → durable specs)
//!     → QaOutcome::try_new (typed verdict + invariants)
//!     → derive_qa_rework_routing (per-verdict routing decision)
//!     → check_qa_blockers_at_parent_close + validate_qa_waiver (policy)
//! ```

use std::path::PathBuf;

use symphony_core::{
    AcceptanceCriterionStatus, AcceptanceCriterionTrace, BlockerId, BlockerReworkInput,
    BlockerSeverity, BranchOrWorkspace, Handoff, HandoffBlockerRequest, IntegrationId,
    PriorRunSummary, PullRequestProvider, PullRequestState, QaBlockerPolicy, QaBlockerSnapshot,
    QaDraftPullRequest, QaEvidence, QaOutcome, QaParentCloseError, QaRequestCause,
    QaReworkDecision, QaRunRequest, QaVerdict, QaVerdictId, QaWaiverError, QaWorkspace, ReadyFor,
    RoleName, RunRef, WorkItemId, WorkItemStatusClass, check_qa_blockers_at_parent_close,
    derive_qa_blockers, derive_qa_rework_routing, validate_qa_waiver,
};

const PARENT_ID: i64 = 50;
const CHILD_A: i64 = 51;
const CHILD_B: i64 = 52;
const QA_RUN: i64 = 700;
const SPECIALIST_RUN: i64 = 600;
const INTEGRATION_RUN: i64 = 650;

fn qa_role() -> RoleName {
    RoleName::new("qa")
}

fn integration_role() -> RoleName {
    RoleName::new("platform_lead")
}

fn specialist_role() -> RoleName {
    RoleName::new("backend")
}

fn workspace() -> QaWorkspace {
    QaWorkspace {
        path: PathBuf::from("/tmp/proj-integration"),
        strategy: "git_worktree".into(),
        branch: Some("symphony/integration/PROJ-42".into()),
        base_ref: Some("main".into()),
    }
}

fn draft_pr() -> QaDraftPullRequest {
    QaDraftPullRequest {
        provider: PullRequestProvider::Github,
        state: PullRequestState::Draft,
        number: Some(123),
        url: Some("https://example.test/pr/123".into()),
        head_branch: "symphony/integration/PROJ-42".into(),
        base_branch: Some("main".into()),
        head_sha: Some("deadbeef".into()),
    }
}

fn integration_handoff() -> Handoff {
    Handoff {
        summary: "consolidated children A and B".into(),
        changed_files: vec!["src/a.rs".into(), "src/b.rs".into()],
        tests_run: vec!["cargo test --workspace".into()],
        verification_evidence: vec!["green CI".into()],
        known_risks: vec![],
        blockers_created: vec![],
        followups_created_or_proposed: vec![],
        branch_or_workspace: BranchOrWorkspace {
            branch: Some("symphony/integration/PROJ-42".into()),
            workspace_path: Some("/tmp/proj-integration".into()),
            base_ref: Some("main".into()),
        },
        ready_for: ReadyFor::Qa,
        block_reason: None,
        reporting_role: Some(integration_role()),
        verdict_request: None,
    }
}

fn qa_request(cause: QaRequestCause) -> QaRunRequest {
    QaRunRequest::try_new(
        WorkItemId::new(PARENT_ID),
        "PROJ-42",
        "ship widget",
        qa_role(),
        workspace(),
        Some(IntegrationId::new(9)),
        Some(draft_pr()),
        vec![
            "all child specialists report green".into(),
            "integration branch has clean diff vs main".into(),
        ],
        vec!["src/a.rs".into(), "src/b.rs".into()],
        Some("github-actions: success".into()),
        vec![integration_handoff()],
        cause,
    )
    .expect("QA request should construct under the happy path")
}

fn passing_trace(criterion: &str) -> AcceptanceCriterionTrace {
    AcceptanceCriterionTrace {
        criterion: criterion.into(),
        status: AcceptanceCriterionStatus::Verified,
        evidence: vec!["cargo test --workspace".into()],
        notes: None,
    }
}

fn evidence() -> QaEvidence {
    QaEvidence {
        tests_run: vec!["cargo test --workspace".into()],
        changed_files_reviewed: vec!["src/a.rs".into(), "src/b.rs".into()],
        ci_status: vec!["github-actions: success".into()],
        ..QaEvidence::default()
    }
}

// --- 1. QA pass --------------------------------------------------------

#[test]
fn qa_pass_flow_yields_no_rework_and_opens_parent_close_gate() {
    // Build the QA request the queue would hand the runner.
    let request = qa_request(QaRequestCause::IntegrationConsolidated);
    assert!(request.has_draft_pr());
    assert!(request.has_prior_handoffs());

    // QA verifies every acceptance criterion and authors a passed verdict.
    let trace: Vec<AcceptanceCriterionTrace> = request
        .acceptance_criteria
        .iter()
        .map(|c| passing_trace(c))
        .collect();

    let outcome = QaOutcome::try_new(
        QaVerdictId::new(1),
        request.work_item_id,
        RunRef::new(QA_RUN),
        request.qa_role.clone(),
        QaVerdict::Passed,
        evidence(),
        trace,
        vec![],
        None,
        None,
    )
    .expect("passed outcome with all-passing trace must construct");

    assert!(outcome.is_pass());
    assert_eq!(outcome.unpassed_criteria().count(), 0);
    assert!(outcome.blockers_created.is_empty());

    // Routing decision is "no rework needed".
    let decision = derive_qa_rework_routing(&outcome, &[], &[]).unwrap();
    assert_eq!(
        decision,
        QaReworkDecision::NoReworkNeeded {
            verdict: QaVerdict::Passed,
        }
    );

    // Parent close gate sees no QA-filed blockers — close is allowed.
    check_qa_blockers_at_parent_close(
        QaBlockerPolicy::BlocksParent,
        WorkItemId::new(PARENT_ID),
        &[],
    )
    .expect("no open QA blockers ⇒ parent close gate open");
}

// --- 2. QA fail with blockers ------------------------------------------

#[test]
fn qa_fail_with_blockers_files_durable_blockers_routes_rework_and_gates_parent_close() {
    let request = qa_request(QaRequestCause::IntegrationConsolidated);

    // Agent emits two blocker requests targeting child specialists.
    let requests = vec![
        HandoffBlockerRequest {
            blocking_id: Some(WorkItemId::new(CHILD_A)),
            reason: "tests for ENG-A regress on integration branch".into(),
            severity: BlockerSeverity::High,
        },
        HandoffBlockerRequest {
            blocking_id: Some(WorkItemId::new(CHILD_B)),
            reason: "ui broken on submit in ENG-B".into(),
            severity: BlockerSeverity::Critical,
        },
    ];

    // Kernel derives the durable blocker specs.
    let specs = derive_qa_blockers(
        request.work_item_id,
        RunRef::new(QA_RUN),
        &request.qa_role,
        &requests,
    )
    .expect("well-formed blocker requests should derive");
    assert_eq!(specs.len(), 2);

    // Storage seam assigns ids; specs become durable blockers.
    let blocker_a = specs[0]
        .clone()
        .into_blocker(BlockerId::new(801))
        .expect("spec → blocker invariants must hold");
    let blocker_b = specs[1]
        .clone()
        .into_blocker(BlockerId::new(802))
        .expect("spec → blocker invariants must hold");
    assert_eq!(blocker_a.blocking_id, WorkItemId::new(CHILD_A));
    assert_eq!(blocker_b.severity, BlockerSeverity::Critical);

    // Verdict references the assigned ids.
    let outcome = QaOutcome::try_new(
        QaVerdictId::new(2),
        request.work_item_id,
        RunRef::new(QA_RUN),
        request.qa_role.clone(),
        QaVerdict::FailedWithBlockers,
        evidence(),
        vec![],
        vec![blocker_a.id, blocker_b.id],
        None,
        Some("two failing children".into()),
    )
    .expect("failed_with_blockers with attached blockers must construct");
    assert!(outcome.verdict.is_failure());
    assert!(outcome.verdict.requires_blockers());

    // Routing produces one rework route per blocker, in declaration order,
    // each carrying the blocked child's assigned role.
    let inputs = vec![
        BlockerReworkInput {
            blocker_id: blocker_a.id,
            blocked_work_item: WorkItemId::new(CHILD_A),
            blocked_assigned_role: Some(specialist_role()),
        },
        BlockerReworkInput {
            blocker_id: blocker_b.id,
            blocked_work_item: WorkItemId::new(CHILD_B),
            blocked_assigned_role: Some(RoleName::new("frontend")),
        },
    ];
    let decision = derive_qa_rework_routing(&outcome, &[], &inputs).unwrap();
    match decision {
        QaReworkDecision::BlockersFiled {
            verdict,
            next_status_class,
            routes,
        } => {
            assert_eq!(verdict, QaVerdict::FailedWithBlockers);
            assert_eq!(next_status_class, WorkItemStatusClass::Rework);
            assert_eq!(routes.len(), 2);
            assert_eq!(routes[0].blocked_work_item, WorkItemId::new(CHILD_A));
            assert_eq!(routes[0].target_role.as_ref().unwrap().as_str(), "backend");
            assert_eq!(routes[1].target_role.as_ref().unwrap().as_str(), "frontend");
        }
        other => panic!("expected BlockersFiled routing, got {other:?}"),
    }

    // Parent close gate sees open QA blockers — close is refused under the
    // default `blocks_parent` policy.
    let open = vec![
        QaBlockerSnapshot {
            blocker_id: blocker_a.id,
            blocked_work_item: WorkItemId::new(CHILD_A),
            blocked_identifier: "ENG-A".into(),
            origin_role: qa_role(),
            reason: Some("tests for ENG-A regress on integration branch".into()),
        },
        QaBlockerSnapshot {
            blocker_id: blocker_b.id,
            blocked_work_item: WorkItemId::new(CHILD_B),
            blocked_identifier: "ENG-B".into(),
            origin_role: qa_role(),
            reason: Some("ui broken on submit in ENG-B".into()),
        },
    ];
    let err = check_qa_blockers_at_parent_close(
        QaBlockerPolicy::BlocksParent,
        WorkItemId::new(PARENT_ID),
        &open,
    )
    .expect_err("open QA blockers must gate parent close");
    match err {
        QaParentCloseError::QaBlockersOpen { parent, open } => {
            assert_eq!(parent, WorkItemId::new(PARENT_ID));
            assert_eq!(open.len(), 2);
        }
    }

    // Once both QA blockers resolve, the gate re-opens.
    check_qa_blockers_at_parent_close(
        QaBlockerPolicy::BlocksParent,
        WorkItemId::new(PARENT_ID),
        &[],
    )
    .expect("with no open QA blockers the parent gate is open again");
}

// --- 3. Inconclusive verdict -------------------------------------------

#[test]
fn inconclusive_verdict_keeps_work_in_qa_with_no_blockers_and_no_rework() {
    let request = qa_request(QaRequestCause::IntegrationConsolidated);

    let outcome = QaOutcome::try_new(
        QaVerdictId::new(3),
        request.work_item_id,
        RunRef::new(QA_RUN),
        request.qa_role.clone(),
        QaVerdict::Inconclusive,
        // Inconclusive may carry partial trace — none of it must be
        // "passing" in aggregate, but the construction is permissive.
        QaEvidence {
            notes: vec!["no display available; cannot screenshot".into()],
            ..QaEvidence::default()
        },
        vec![AcceptanceCriterionTrace {
            criterion: "ui smoke".into(),
            status: AcceptanceCriterionStatus::NotVerified,
            evidence: vec![],
            notes: Some("display unavailable".into()),
        }],
        vec![],
        None,
        Some("could not exercise UI".into()),
    )
    .expect("inconclusive verdict must construct without blockers or waiver");

    assert!(!outcome.is_pass());
    assert!(!outcome.verdict.is_failure());
    assert!(!outcome.verdict.requires_blockers());

    // Routing keeps the work item in QA — no rework, no transition.
    let decision = derive_qa_rework_routing(&outcome, &[], &[]).unwrap();
    assert_eq!(
        decision,
        QaReworkDecision::StaysInQa {
            verdict: QaVerdict::Inconclusive,
        }
    );

    // Parent close gate is *not* satisfied: there are no QA blockers, but
    // the verdict itself is non-passing. The blocker-policy gate is
    // orthogonal to the verdict gate; here we only assert that absent QA
    // blockers, this gate does not contribute a failure.
    check_qa_blockers_at_parent_close(
        QaBlockerPolicy::BlocksParent,
        WorkItemId::new(PARENT_ID),
        &[],
    )
    .expect("inconclusive without open QA blockers ⇒ blocker gate open");
}

// --- 4. Waiver policy --------------------------------------------------

#[test]
fn waived_verdict_requires_configured_waiver_role_and_reason_then_allows_parent_close() {
    let request = qa_request(QaRequestCause::IntegrationConsolidated);

    // Workflow config: integration owner is the only configured waiver role.
    let configured = vec![integration_role()];

    // Step 1 — unauthorised role is rejected.
    let unauthorised = validate_qa_waiver(&configured, &RoleName::new("ghost"), "ok").unwrap_err();
    assert_eq!(
        unauthorised,
        QaWaiverError::UnauthorisedRole {
            role: RoleName::new("ghost"),
        }
    );

    // Step 2 — empty reason is rejected even for an authorised role.
    let empty = validate_qa_waiver(&configured, &integration_role(), "   ").unwrap_err();
    assert_eq!(empty, QaWaiverError::EmptyReason);

    // Step 3 — empty `waiver_roles` rejects up-front so the operator sees
    // the real cause.
    let unconfigured = validate_qa_waiver(&[], &integration_role(), "accepted risk").unwrap_err();
    assert_eq!(unconfigured, QaWaiverError::NoWaiverRolesConfigured);

    // Step 4 — authorised role with a non-empty reason is accepted.
    validate_qa_waiver(
        &configured,
        &integration_role(),
        "accepted residual ui risk pre-launch",
    )
    .expect("configured role + reason must authorise the waiver");

    // The QaOutcome carries the same role + reason; constructing a Waived
    // verdict without them is a structural error (covered in unit tests).
    let outcome = QaOutcome::try_new(
        QaVerdictId::new(4),
        request.work_item_id,
        RunRef::new(QA_RUN),
        request.qa_role.clone(),
        QaVerdict::Waived,
        evidence(),
        vec![],
        vec![],
        Some(integration_role()),
        Some("accepted residual ui risk pre-launch".into()),
    )
    .expect("waived verdict with role+reason must construct");
    assert!(outcome.is_pass());
    assert!(outcome.verdict.requires_waiver());
    assert_eq!(
        outcome.waiver_role.as_ref().unwrap().as_str(),
        "platform_lead"
    );

    // Routing for `waived` is "no rework needed".
    let decision = derive_qa_rework_routing(&outcome, &[], &[]).unwrap();
    assert_eq!(
        decision,
        QaReworkDecision::NoReworkNeeded {
            verdict: QaVerdict::Waived,
        }
    );

    // With the waiver in force and no open QA blockers, the parent close
    // gate is open under either policy.
    check_qa_blockers_at_parent_close(
        QaBlockerPolicy::BlocksParent,
        WorkItemId::new(PARENT_ID),
        &[],
    )
    .expect("waived verdict + no QA blockers ⇒ parent gate open");
    check_qa_blockers_at_parent_close(QaBlockerPolicy::Advisory, WorkItemId::new(PARENT_ID), &[])
        .expect("advisory policy never gates close");
}

// --- 5. Cross-cutting: failed_needs_rework re-routes to prior contributor ---

#[test]
fn failed_needs_rework_routes_to_prior_non_qa_run() {
    let request = qa_request(QaRequestCause::DirectQaRequest);

    let outcome = QaOutcome::try_new(
        QaVerdictId::new(5),
        request.work_item_id,
        RunRef::new(QA_RUN),
        request.qa_role.clone(),
        QaVerdict::FailedNeedsRework,
        evidence(),
        vec![AcceptanceCriterionTrace {
            criterion: "ui flows".into(),
            status: AcceptanceCriterionStatus::Failed,
            evidence: vec![],
            notes: Some("404 on submit".into()),
        }],
        vec![],
        None,
        Some("rerun after fix".into()),
    )
    .expect("failed_needs_rework requires no blockers, but must construct");

    // Prior runs ordered oldest → newest. The QA-authored run is filtered
    // out; the most recent non-QA contributor is the integration owner.
    let prior_runs = vec![
        PriorRunSummary {
            run_id: RunRef::new(SPECIALIST_RUN),
            role: specialist_role(),
            work_item_id: WorkItemId::new(CHILD_A),
        },
        PriorRunSummary {
            run_id: RunRef::new(INTEGRATION_RUN),
            role: integration_role(),
            work_item_id: WorkItemId::new(PARENT_ID),
        },
        PriorRunSummary {
            run_id: RunRef::new(QA_RUN),
            role: qa_role(),
            work_item_id: WorkItemId::new(PARENT_ID),
        },
    ];

    let decision = derive_qa_rework_routing(&outcome, &prior_runs, &[]).unwrap();
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
            assert_eq!(work_item_id, WorkItemId::new(PARENT_ID));
            assert_eq!(target_role.as_str(), "platform_lead");
            assert_eq!(from_prior_run, RunRef::new(INTEGRATION_RUN));
        }
        other => panic!("expected NeedsRework routing, got {other:?}"),
    }
}
