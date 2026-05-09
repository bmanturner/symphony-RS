//! Phase 10 cross-cutting follow-up flow tests (CHECKLIST_v2 Phase 10).
//!
//! Module-level tests under `followup`, `followup_request`, `followup_routing`,
//! and `followup_policy` exercise individual invariants in isolation. This
//! suite walks a follow-up *through the whole Phase 10 contract* using only
//! public domain types so a future change that breaks any seam — handoff
//! validation, request derivation, policy resolution, queue routing, durable
//! finalisation, approval lifecycle, or the source-link provenance edge —
//! surfaces in one place instead of requiring an archaeologist across four
//! files.
//!
//! The three scenarios mirror SPEC v2 §6.6's claim that follow-up filing is
//! a universal agent literacy:
//!
//! 1. **Specialist** files a non-blocking follow-up under
//!    `default_policy = create_directly`. The spec routes straight to the
//!    tracker; the durable record skips the approval queue and finalises
//!    via `mark_created`.
//! 2. **Integration owner** files a follow-up under
//!    `default_policy = propose_for_approval`. The spec routes to the
//!    approval queue keyed on `followups.approval_role`; the durable record
//!    starts as `Proposed`, an approver advances it to `Approved`, and the
//!    tracker write completes the lifecycle to `Created`.
//! 3. **QA gate** files a blocking follow-up alongside its verdict request
//!    under `default_policy = create_directly` with `propose_only = true`
//!    on the request — the workflow floor permits direct creation but the
//!    agent opts up to the approval queue. Locks in that the agent's
//!    `propose_only` flag *raises* the floor and that QA is a first-class
//!    follow-up filer just like any other role.
//!
//! The flow exercised, mirroring SPEC v2 §4.10 / §5.13 / §6.6 / ARCH §6.4:
//!
//! ```text
//! Run emits Handoff with followups_created_or_proposed
//!     → Handoff::validate (shape)
//!     → FollowupRequestInput::from_handoff (kernel attaches scope + AC)
//!     → derive_followups (resolved policy + run/role origin)
//!     → route_followup (CreateDirectly | RouteToApprovalQueue)
//!     → FollowupSpec::into_followup (durable record, initial status)
//!     → approve (queue path) → mark_created → source_link (provenance edge)
//! ```

use symphony_core::{
    BlockerOrigin, BranchOrWorkspace, FollowupId, FollowupPolicy, FollowupRequestInput,
    FollowupRouteDecision, FollowupStatus, Handoff, HandoffFollowupRequest, ReadyFor,
    RoleAuthority, RoleKind, RoleName, RunRef, WorkItemId, derive_followups, route_followup,
};

const PARENT_ID: i64 = 50;
const SPECIALIST_RUN: i64 = 600;
const INTEGRATION_RUN: i64 = 650;
const QA_RUN: i64 = 700;
const SPECIALIST_FOLLOWUP_ID: i64 = 9_001;
const INTEGRATION_FOLLOWUP_ID: i64 = 9_002;
const QA_FOLLOWUP_ID: i64 = 9_003;
const SPECIALIST_TRACKER_ID: i64 = 5_001;
const INTEGRATION_TRACKER_ID: i64 = 5_002;
const QA_TRACKER_ID: i64 = 5_003;

fn specialist_role() -> RoleName {
    RoleName::new("backend")
}

fn integration_role() -> RoleName {
    RoleName::new("platform_lead")
}

fn qa_role() -> RoleName {
    RoleName::new("qa")
}

fn approval_role() -> RoleName {
    // SPEC §5.13: `followups.approval_role` is workflow-configured. Use a
    // role distinct from each filer to make accidental self-approval
    // surface as a value mismatch.
    RoleName::new("triage_lead")
}

fn handoff_with_followup(
    role: RoleName,
    ready_for: ReadyFor,
    followup: HandoffFollowupRequest,
) -> Handoff {
    Handoff {
        summary: format!("{} run", role),
        changed_files: vec!["src/lib.rs".into()],
        tests_run: vec!["cargo test --workspace".into()],
        verification_evidence: vec![],
        known_risks: vec![],
        blockers_created: vec![],
        followups_created_or_proposed: vec![followup],
        branch_or_workspace: BranchOrWorkspace {
            branch: Some("symphony/work/PROJ-42".into()),
            workspace_path: Some("/tmp/proj".into()),
            base_ref: Some("main".into()),
        },
        ready_for,
        block_reason: None,
        reporting_role: Some(role),
        verdict_request: None,
    }
}

// --- 1. Specialist follow-up: create_directly --------------------------

#[test]
fn specialist_followup_creates_directly_and_skips_approval_queue() {
    // Specialist surfaces an adjacent rate-limit cleanup while shipping
    // PROJ-42. Workflow has `default_policy = create_directly` and the
    // agent did not opt up via `propose_only`.
    let request = HandoffFollowupRequest {
        title: "rate-limit cleanup".into(),
        summary: "tighten retries on 429".into(),
        blocking: false,
        propose_only: false,
    };
    let handoff = handoff_with_followup(specialist_role(), ReadyFor::Integration, request.clone());
    handoff
        .validate()
        .expect("specialist handoff is well-formed");

    let input = FollowupRequestInput::from_handoff(
        &request,
        "symphony-tracker GitHub adapter only",
        vec!["retries respect Retry-After".into()],
    );

    let specs = derive_followups(
        WorkItemId::new(PARENT_ID),
        RunRef::new(SPECIALIST_RUN),
        &specialist_role(),
        &RoleAuthority::defaults_for(RoleKind::Specialist),
        FollowupPolicy::CreateDirectly,
        true,
        std::slice::from_ref(&input),
    )
    .expect("specialist authority defaults to can_file_followups=true");
    assert_eq!(specs.len(), 1);
    assert_eq!(specs[0].policy, FollowupPolicy::CreateDirectly);
    assert!(!specs[0].blocking);
    match &specs[0].origin {
        BlockerOrigin::Run { run_id, role } => {
            assert_eq!(*run_id, RunRef::new(SPECIALIST_RUN));
            assert_eq!(role.as_ref().unwrap(), &specialist_role());
        }
        other => panic!("specialist follow-up must record run+role origin: {other:?}"),
    }

    // Routing: create_directly bypasses the approval queue.
    let decision = route_followup(&specs[0], Some(&approval_role())).unwrap();
    assert_eq!(decision, FollowupRouteDecision::CreateDirectly);
    assert!(!decision.requires_approval());

    // Finalise into the durable record. Initial status = Approved per
    // FollowupPolicy::initial_status; the kernel then marks it Created
    // once the tracker write succeeds.
    let mut durable = specs[0]
        .clone()
        .into_followup(FollowupId::new(SPECIALIST_FOLLOWUP_ID))
        .expect("spec finalises under default invariants");
    assert_eq!(durable.status, FollowupStatus::Approved);
    assert!(durable.approval_role.is_none());

    durable
        .mark_created(WorkItemId::new(SPECIALIST_TRACKER_ID))
        .expect("create_directly path goes Approved -> Created without a separate approve()");
    assert_eq!(durable.status, FollowupStatus::Created);
    assert_eq!(
        durable.created_issue_id,
        Some(WorkItemId::new(SPECIALIST_TRACKER_ID))
    );

    // Provenance edge: non-blocking follow-up still records the link so
    // operators can navigate from the tracker issue back to the source.
    let link = durable
        .source_link()
        .expect("Created follow-ups expose source_link");
    assert_eq!(link.source_work_item, WorkItemId::new(PARENT_ID));
    assert_eq!(
        link.followup_work_item,
        WorkItemId::new(SPECIALIST_TRACKER_ID)
    );
    assert!(!link.blocking);
}

// --- 2. Integration-owner follow-up: propose_for_approval --------------

#[test]
fn integration_owner_followup_routes_through_approval_queue() {
    // Integration owner notices a deferred refactor while consolidating
    // children. Workflow has `default_policy = propose_for_approval` so
    // every follow-up is queued for triage regardless of `propose_only`.
    let request = HandoffFollowupRequest {
        title: "split tracker mutations crate".into(),
        summary: "extract write paths into symphony-tracker-mutations".into(),
        blocking: false,
        propose_only: false,
    };
    let handoff = handoff_with_followup(integration_role(), ReadyFor::Qa, request.clone());
    handoff
        .validate()
        .expect("integration handoff is well-formed");

    let input = FollowupRequestInput::from_handoff(
        &request,
        "symphony-tracker write surface only",
        vec!["existing TrackerMutations callers compile unchanged".into()],
    );

    let specs = derive_followups(
        WorkItemId::new(PARENT_ID),
        RunRef::new(INTEGRATION_RUN),
        &integration_role(),
        &RoleAuthority::defaults_for(RoleKind::IntegrationOwner),
        FollowupPolicy::ProposeForApproval,
        true,
        std::slice::from_ref(&input),
    )
    .expect("integration owner has can_file_followups by default");
    assert_eq!(specs[0].policy, FollowupPolicy::ProposeForApproval);

    // Routing: the workflow floor parks the spec in the approval queue
    // keyed on the configured approval role.
    let decision = route_followup(&specs[0], Some(&approval_role())).unwrap();
    assert_eq!(
        decision,
        FollowupRouteDecision::RouteToApprovalQueue {
            approval_role: approval_role(),
        }
    );
    assert!(decision.requires_approval());

    // Durable record starts as Proposed; the lifecycle requires explicit
    // approve() before mark_created() will succeed.
    let mut durable = specs[0]
        .clone()
        .into_followup(FollowupId::new(INTEGRATION_FOLLOWUP_ID))
        .expect("spec finalises under default invariants");
    assert_eq!(durable.status, FollowupStatus::Proposed);

    let premature = durable.mark_created(WorkItemId::new(INTEGRATION_TRACKER_ID));
    assert!(
        premature.is_err(),
        "Proposed follow-ups must not be markable as Created before approval"
    );
    assert_eq!(durable.status, FollowupStatus::Proposed);

    durable
        .approve(approval_role(), "triage accepted; ship in next cycle")
        .expect("approval role advances Proposed -> Approved");
    assert_eq!(durable.status, FollowupStatus::Approved);
    assert_eq!(durable.approval_role.as_ref(), Some(&approval_role()));

    durable
        .mark_created(WorkItemId::new(INTEGRATION_TRACKER_ID))
        .expect("Approved follow-ups advance to Created on tracker write");
    assert_eq!(durable.status, FollowupStatus::Created);

    let link = durable.source_link().unwrap();
    assert_eq!(
        link.followup_work_item,
        WorkItemId::new(INTEGRATION_TRACKER_ID)
    );
    assert!(!link.blocking);
}

// --- 3. QA-gate follow-up: workflow create_directly + agent propose_only ---

#[test]
fn qa_gate_blocking_followup_propose_only_raises_floor_to_approval_queue() {
    // QA finds a missing acceptance test while reviewing the integration
    // PR. The workflow allows direct creation, but QA flags the work as
    // not-yet-triaged and sets `propose_only = true`; SPEC §5.13 says the
    // workflow chooses the floor and the agent may raise it.
    let request = HandoffFollowupRequest {
        title: "add e2e regression for blocker waiver flow".into(),
        summary: "QA-only manual repro today; needs a deterministic test".into(),
        blocking: true,
        propose_only: true,
    };
    let handoff = handoff_with_followup(qa_role(), ReadyFor::Qa, request.clone());
    handoff.validate().expect("qa handoff is well-formed");

    let input = FollowupRequestInput::from_handoff(
        &request,
        "symphony-core qa_blocker_policy module + flow tests",
        vec!["test fails before fix and passes after".into()],
    );

    let specs = derive_followups(
        WorkItemId::new(PARENT_ID),
        RunRef::new(QA_RUN),
        &qa_role(),
        &RoleAuthority::defaults_for(RoleKind::QaGate),
        FollowupPolicy::CreateDirectly,
        true,
        std::slice::from_ref(&input),
    )
    .expect("qa_gate has can_file_followups by default");
    assert_eq!(
        specs[0].policy,
        FollowupPolicy::ProposeForApproval,
        "propose_only=true raises the workflow floor"
    );
    assert!(specs[0].blocking, "QA flagged the follow-up as gating");

    let decision = route_followup(&specs[0], Some(&approval_role())).unwrap();
    assert_eq!(
        decision,
        FollowupRouteDecision::RouteToApprovalQueue {
            approval_role: approval_role(),
        }
    );

    let mut durable = specs[0]
        .clone()
        .into_followup(FollowupId::new(QA_FOLLOWUP_ID))
        .expect("spec finalises under default invariants");
    assert_eq!(durable.status, FollowupStatus::Proposed);
    assert!(
        durable.is_acceptance_gating(),
        "blocking QA follow-up gates parent close while in the approval pipeline"
    );

    durable
        .approve(approval_role(), "accepted; gate the parent until merged")
        .unwrap();
    assert!(durable.is_acceptance_gating());

    durable
        .mark_created(WorkItemId::new(QA_TRACKER_ID))
        .unwrap();
    assert!(
        durable.is_acceptance_gating(),
        "Created blocking follow-ups still gate parent close until the spawned issue terminates"
    );

    let link = durable.source_link().unwrap();
    assert_eq!(link.source_work_item, WorkItemId::new(PARENT_ID));
    assert_eq!(link.followup_work_item, WorkItemId::new(QA_TRACKER_ID));
    assert!(link.blocking);
}
