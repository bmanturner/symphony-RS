//! Phase 8 cross-cutting integration tests (CHECKLIST_v2 Phase 8).
//!
//! Module-level tests in `integration_request`, `integration`, `blocker_gate`,
//! and `pull_request` already exercise individual invariants in isolation.
//! This integration suite walks a *parent through the whole Phase 8 contract*
//! using only the public domain types so a future change that breaks any
//! seam between them — child-completion gate, blocker prevention, draft PR
//! creation, successful integration handoff, merge-conflict evidence,
//! merge-conflict repair (file a fresh succeeded record after a failure),
//! and the conflict/block path — surfaces here, in one place, instead of
//! requiring an archaeologist across four files to reconstruct the flow.
//!
//! The tests intentionally use `IntegrationRecord::try_new` /
//! `PullRequestRecord::try_new` rather than the storage-layer `New*` rows:
//! Phase 8's *contract* lives in the domain types, and the storage
//! repositories are tested separately in `symphony-state`.
//!
//! The flow exercised, mirroring SPEC v2 §6.4 / ARCH §9:
//!
//! ```text
//! integration queue surfaces parent
//!     → IntegrationRunRequest::try_new (children + blocker gates)
//!     → IntegrationRecord (Pending → InProgress → Succeeded | Failed | Blocked)
//!     → check_no_open_blockers(DraftPullRequest, …)
//!     → PullRequestRecord::try_new (Draft)
//! ```
//!
//! Repair-loop case: when integration produces a `Failed` record with
//! conflict evidence, the integration owner is expected to file a *fresh*
//! `Succeeded` record on a follow-up turn. Terminal records are durable
//! evidence — they are never mutated in place — so the test asserts both
//! records coexist for the same parent.

use std::path::PathBuf;

use symphony_core::{
    BlockerId, BlockerStatus, BranchOrWorkspace, GateOperation, Handoff, IntegrationChild,
    IntegrationConflict, IntegrationGates, IntegrationId, IntegrationMergeStrategy,
    IntegrationRecord, IntegrationRequestCause, IntegrationRequestError, IntegrationRunRequest,
    IntegrationStatus, IntegrationWorkspace, OpenBlockerSnapshot, PullRequestProvider,
    PullRequestRecord, PullRequestRecordId, PullRequestState, ReadyFor, RoleName, RunRef,
    WorkItemId, WorkItemStatusClass, blocker_gate, check_no_open_blockers, collect_open,
};

const PARENT_ID: i64 = 10;
const CHILD_A: i64 = 11;
const CHILD_B: i64 = 12;

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
        branch: Some(format!("feat/{identifier}")),
        latest_handoff: Some(child_handoff(identifier)),
    }
}

fn child_handoff(identifier: &str) -> Handoff {
    Handoff {
        summary: format!("implemented {identifier}"),
        changed_files: vec![format!("src/{identifier}.rs")],
        tests_run: vec!["cargo test --workspace".into()],
        verification_evidence: vec![],
        known_risks: vec![],
        blockers_created: vec![],
        followups_created_or_proposed: vec![],
        branch_or_workspace: BranchOrWorkspace {
            branch: Some(format!("feat/{identifier}")),
            workspace_path: Some(format!("/tmp/{identifier}")),
            base_ref: Some("main".into()),
        },
        ready_for: ReadyFor::Integration,
        block_reason: None,
        reporting_role: None,
        verdict_request: None,
    }
}

fn happy_request() -> IntegrationRunRequest {
    IntegrationRunRequest::try_new(
        WorkItemId::new(PARENT_ID),
        "PROJ-42",
        "ship widget",
        role(),
        IntegrationMergeStrategy::SequentialCherryPick,
        workspace(),
        vec![
            child(CHILD_A, "ENG-A", WorkItemStatusClass::Done),
            child(CHILD_B, "ENG-B", WorkItemStatusClass::Done),
        ],
        IntegrationGates::default(),
        0,
        IntegrationRequestCause::AllChildrenTerminal,
    )
    .expect("happy-path request must construct")
}

fn open_blocker(blocker: i64, blocked: i64, ident: &str) -> OpenBlockerSnapshot {
    OpenBlockerSnapshot {
        blocker_id: BlockerId(blocker),
        blocked_work_item: WorkItemId::new(blocked),
        blocked_identifier: ident.into(),
        reason: Some("flaky test".into()),
    }
}

// --- 1. Child completion gate -------------------------------------------

#[test]
fn child_completion_gate_blocks_request_when_child_unfinished() {
    let err = IntegrationRunRequest::try_new(
        WorkItemId::new(PARENT_ID),
        "PROJ-42",
        "ship widget",
        role(),
        IntegrationMergeStrategy::SequentialCherryPick,
        workspace(),
        vec![
            child(CHILD_A, "ENG-A", WorkItemStatusClass::Done),
            child(CHILD_B, "ENG-B", WorkItemStatusClass::Running),
        ],
        IntegrationGates::default(),
        0,
        IntegrationRequestCause::AllChildrenTerminal,
    )
    .expect_err("non-terminal child must reject under default gates");
    assert_eq!(
        err,
        IntegrationRequestError::ChildNotTerminal {
            child_id: WorkItemId::new(CHILD_B),
            status_class: WorkItemStatusClass::Running,
        }
    );
}

#[test]
fn child_completion_gate_can_be_waived_for_explicit_workflows() {
    // SPEC §5.10 allows a workflow to waive `require_all_children_terminal`
    // for a parent that integrates piecemeal. The request must still
    // construct — the queue waiver and request gate share the same switch.
    let req = IntegrationRunRequest::try_new(
        WorkItemId::new(PARENT_ID),
        "PROJ-42",
        "ship widget",
        role(),
        IntegrationMergeStrategy::SharedBranch,
        workspace(),
        vec![child(CHILD_B, "ENG-B", WorkItemStatusClass::Running)],
        IntegrationGates {
            require_all_children_terminal: false,
            require_no_open_blockers: true,
        },
        0,
        IntegrationRequestCause::AllChildrenTerminal,
    )
    .expect("waived terminal-children gate accepts in-flight child");
    assert_eq!(req.children.len(), 1);
}

// --- 2. Blocker prevention ---------------------------------------------

#[test]
fn blocker_prevention_rejects_integration_request_under_default_gates() {
    let err = IntegrationRunRequest::try_new(
        WorkItemId::new(PARENT_ID),
        "PROJ-42",
        "ship widget",
        role(),
        IntegrationMergeStrategy::SequentialCherryPick,
        workspace(),
        vec![
            child(CHILD_A, "ENG-A", WorkItemStatusClass::Done),
            child(CHILD_B, "ENG-B", WorkItemStatusClass::Done),
        ],
        IntegrationGates::default(),
        2,
        IntegrationRequestCause::AllChildrenTerminal,
    )
    .expect_err("open blockers must reject under default gates");
    assert_eq!(
        err,
        IntegrationRequestError::OpenBlockersPresent { count: 2 }
    );
}

#[test]
fn blocker_prevention_blocks_draft_pr_creation_seam() {
    // Even if the request was already constructed (e.g. open blockers
    // arrived between request build and side-effect), the parallel guard
    // at the draft-PR seam refuses to open the PR while open blockers
    // remain in the subtree.
    let open = vec![open_blocker(7, CHILD_A, "ENG-A")];
    let err = check_no_open_blockers(
        GateOperation::DraftPullRequest,
        WorkItemId::new(PARENT_ID),
        &open,
    )
    .expect_err("draft PR must not open with open blockers");
    let blocker_gate::BlockerGateError::OpenBlockersPresent {
        operation,
        parent,
        open,
    } = err;
    assert_eq!(operation, GateOperation::DraftPullRequest);
    assert_eq!(parent, WorkItemId::new(PARENT_ID));
    assert_eq!(open[0].blocker_id, BlockerId(7));
}

#[test]
fn blocker_prevention_resolved_blockers_filtered_before_gate() {
    // The gate trusts the caller to pass only *open* blockers. The
    // canonical caller funnels its raw rows through `collect_open`, which
    // must drop resolved/waived/cancelled rows so the parent can advance.
    let rows = vec![
        (BlockerStatus::Resolved, open_blocker(1, CHILD_A, "ENG-A")),
        (BlockerStatus::Waived, open_blocker(2, CHILD_B, "ENG-B")),
        (BlockerStatus::Cancelled, open_blocker(3, CHILD_A, "ENG-A")),
    ];
    let filtered = collect_open(rows);
    assert!(filtered.is_empty());
    check_no_open_blockers(
        GateOperation::DraftPullRequest,
        WorkItemId::new(PARENT_ID),
        &filtered,
    )
    .expect("no open blockers means draft PR seam is clear");
}

// --- 3. Successful integration handoff and draft PR creation ----------

#[test]
fn happy_path_records_succeeded_integration_then_draft_pr() {
    // Step 1: build the request — gates pass.
    let req = happy_request();
    assert_eq!(req.cause, IntegrationRequestCause::AllChildrenTerminal);
    assert!(req.has_child_handoffs());

    // Step 2: integration owner records `Pending` → `InProgress` →
    // `Succeeded`. Each stage is a fresh row; the kernel never mutates a
    // terminal record. We assert the succeeded record satisfies the
    // parent-closeout gate (`is_pass`) and carries the canonical branch
    // and operator-facing summary required by SPEC §5.10.
    let pending = IntegrationRecord::try_new(
        IntegrationId::new(1),
        WorkItemId::new(PARENT_ID),
        role(),
        None,
        IntegrationStatus::Pending,
        IntegrationMergeStrategy::SequentialCherryPick,
        None,
        None,
        None,
        None,
        vec![],
        None,
        vec![],
        vec![],
    )
    .expect("pending record constructs without run/branch");
    assert!(!pending.is_pass());

    let succeeded = IntegrationRecord::try_new(
        IntegrationId::new(2),
        WorkItemId::new(PARENT_ID),
        role(),
        Some(RunRef::new(99)),
        IntegrationStatus::Succeeded,
        IntegrationMergeStrategy::SequentialCherryPick,
        Some("symphony/integration/PROJ-42".into()),
        Some("main".into()),
        Some("deadbeef".into()),
        Some("/tmp/proj-integration".into()),
        vec![WorkItemId::new(CHILD_A), WorkItemId::new(CHILD_B)],
        Some("Consolidated 2 children, all green.".into()),
        vec![],
        vec![],
    )
    .expect("succeeded record constructs with summary + branch");
    assert!(succeeded.is_pass());
    assert_eq!(succeeded.children().count(), 2);

    // Step 3: blocker gate at draft-PR seam clears.
    check_no_open_blockers(
        GateOperation::DraftPullRequest,
        WorkItemId::new(PARENT_ID),
        &[],
    )
    .expect("no open blockers means draft PR may open");

    // Step 4: file the draft PR record. Draft state is allowed to omit the
    // forge-assigned PR number; head_branch must match the canonical
    // integration branch on the succeeded integration record.
    let pr = PullRequestRecord::try_new(
        PullRequestRecordId::new(1),
        WorkItemId::new(PARENT_ID),
        role(),
        Some(RunRef::new(99)),
        Some(succeeded.id),
        PullRequestProvider::Github,
        PullRequestState::Draft,
        None,
        None,
        succeeded
            .integration_branch
            .clone()
            .expect("succeeded carries branch"),
        Some("main".into()),
        succeeded.head_sha.clone(),
        "PROJ-42: integrate consolidation".into(),
        succeeded.summary.clone(),
        Some("queued".into()),
    )
    .expect("draft PR record constructs against succeeded integration");
    assert_eq!(pr.state, PullRequestState::Draft);
    assert_eq!(pr.integration_id, Some(succeeded.id));
    assert!(!pr.is_merged());
}

// --- 4. Merge conflict + repair loop ------------------------------------

#[test]
fn merge_conflict_records_failed_with_conflict_evidence() {
    let err = IntegrationRecord::try_new(
        IntegrationId::new(1),
        WorkItemId::new(PARENT_ID),
        role(),
        Some(RunRef::new(7)),
        IntegrationStatus::Failed,
        IntegrationMergeStrategy::SequentialCherryPick,
        Some("symphony/integration/PROJ-42".into()),
        None,
        None,
        None,
        vec![WorkItemId::new(CHILD_A)],
        Some("partial integration; ENG-B conflicts".into()),
        vec![],
        vec![],
    )
    .expect_err("failed record without evidence must fail");
    assert!(matches!(
        err,
        symphony_core::IntegrationError::MissingConflictEvidence
    ));

    let failed = IntegrationRecord::try_new(
        IntegrationId::new(2),
        WorkItemId::new(PARENT_ID),
        role(),
        Some(RunRef::new(7)),
        IntegrationStatus::Failed,
        IntegrationMergeStrategy::SequentialCherryPick,
        Some("symphony/integration/PROJ-42".into()),
        None,
        None,
        None,
        vec![WorkItemId::new(CHILD_A)],
        None,
        vec![IntegrationConflict {
            child_id: Some(WorkItemId::new(CHILD_B)),
            child_branch: Some("feat/ENG-B".into()),
            files: vec!["src/widget.rs".into()],
            message: "merge conflict in src/widget.rs".into(),
        }],
        vec![],
    )
    .expect("failed record with conflict evidence constructs");
    assert!(!failed.is_pass());
    assert_eq!(failed.conflicts.len(), 1);
}

#[test]
fn merge_conflict_repair_appends_succeeded_record_alongside_failed() {
    // Repair turn: after a failed integration, the owner files a fresh
    // succeeded record. Both rows persist so audit history shows the
    // repair, and the latest record satisfies the parent-closeout gate.
    let failed = IntegrationRecord::try_new(
        IntegrationId::new(1),
        WorkItemId::new(PARENT_ID),
        role(),
        Some(RunRef::new(7)),
        IntegrationStatus::Failed,
        IntegrationMergeStrategy::SequentialCherryPick,
        Some("symphony/integration/PROJ-42".into()),
        None,
        None,
        None,
        vec![],
        None,
        vec![IntegrationConflict {
            child_id: Some(WorkItemId::new(CHILD_B)),
            child_branch: Some("feat/ENG-B".into()),
            files: vec!["src/widget.rs".into()],
            message: "merge conflict in src/widget.rs".into(),
        }],
        vec![],
    )
    .expect("failed record");

    let succeeded = IntegrationRecord::try_new(
        IntegrationId::new(2),
        WorkItemId::new(PARENT_ID),
        role(),
        Some(RunRef::new(11)),
        IntegrationStatus::Succeeded,
        IntegrationMergeStrategy::SequentialCherryPick,
        Some("symphony/integration/PROJ-42".into()),
        Some("main".into()),
        Some("cafef00d".into()),
        Some("/tmp/proj-integration".into()),
        vec![WorkItemId::new(CHILD_A), WorkItemId::new(CHILD_B)],
        Some("Repaired ENG-B conflict; consolidated.".into()),
        vec![],
        vec![],
    )
    .expect("succeeded repair record");

    // Both ids are distinct and persist independently. The kernel reads
    // the latest (highest id) when gating closeout; the failed row stays
    // as audit evidence.
    assert_ne!(failed.id, succeeded.id);
    assert!(!failed.is_pass());
    assert!(succeeded.is_pass());
    assert!(succeeded.id > failed.id);
}

// --- 5. Conflict / block path ------------------------------------------

#[test]
fn conflict_block_path_records_blocked_with_filed_blockers() {
    // SPEC §5.10: when the consolidation cannot proceed because of an
    // external blocker (not a merge conflict), the integration owner files
    // a `Blocked` record carrying the blocker ids. Conflicts may be empty
    // — the blocker list is the routing evidence.
    let blocked = IntegrationRecord::try_new(
        IntegrationId::new(1),
        WorkItemId::new(PARENT_ID),
        role(),
        Some(RunRef::new(13)),
        IntegrationStatus::Blocked,
        IntegrationMergeStrategy::SharedBranch,
        Some("symphony/integration/PROJ-42".into()),
        None,
        None,
        None,
        vec![],
        Some("blocked on upstream".into()),
        vec![],
        vec![BlockerId::new(99), BlockerId::new(100)],
    )
    .expect("blocked record constructs without conflict evidence");
    assert!(!blocked.is_pass());
    assert_eq!(blocked.status, IntegrationStatus::Blocked);
    assert_eq!(blocked.blockers_filed.len(), 2);

    // Parallel guard: with those blockers still open at the QA seam, the
    // QA-request gate refuses to stage the parent.
    let open = blocked
        .blockers_filed
        .iter()
        .enumerate()
        .map(|(i, id)| OpenBlockerSnapshot {
            blocker_id: *id,
            blocked_work_item: WorkItemId::new(PARENT_ID),
            blocked_identifier: format!("PROJ-{i}"),
            reason: Some("blocked integration".into()),
        })
        .collect::<Vec<_>>();
    let err = check_no_open_blockers(GateOperation::QaRequest, WorkItemId::new(PARENT_ID), &open)
        .expect_err("QA request seam refuses to stage parent with open blockers");
    let blocker_gate::BlockerGateError::OpenBlockersPresent { operation, .. } = err;
    assert_eq!(operation, GateOperation::QaRequest);
}
