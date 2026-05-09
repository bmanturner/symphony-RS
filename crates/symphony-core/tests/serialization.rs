//! Public-domain-type serialization sweep (Phase 3 deliverable).
//!
//! Every type re-exported by `symphony_core::lib` that derives
//! `Serialize` + `Deserialize` is round-tripped here through `serde_json`.
//! Module-level tests already exercise individual variants and edge cases;
//! this integration suite asserts the *cross-cutting* guarantee that the
//! public surface — exactly the names the rest of the workspace consumes —
//! survives JSON without loss.
//!
//! Two invariants are checked per type:
//!
//! 1. Round-trip equality for `PartialEq` types, semantic equivalence for
//!    case-insensitive newtypes (`IssueState`, `TrackerStatus`).
//! 2. Wire-format guarantees the kernel relies on: snake_case enum tags,
//!    transparent newtype representation for id types.

use serde::{Deserialize, Serialize};

use symphony_core::tracker::IssueState;
use symphony_core::work_item::TrackerStatus;
use symphony_core::{
    AcceptanceCriterionStatus, AcceptanceCriterionTrace, Blocker, BlockerId, BlockerOrigin,
    BlockerRef, BlockerSeverity, BlockerStatus, BranchOrWorkspace, ChildKey, ChildProposal,
    DecompositionId, DecompositionProposal, DecompositionStatus, FollowupId, FollowupIssueRequest,
    FollowupPolicy, FollowupStatus, Handoff, HandoffBlockerRequest, HandoffFollowupRequest,
    IntegrationConflict, IntegrationId, IntegrationMergeStrategy, IntegrationRecord,
    IntegrationStatus, Issue, IssueId, QaEvidence, QaOutcome, QaVerdict, QaVerdictId, ReadyFor,
    RoleAuthority, RoleAuthorityOverrides, RoleContext, RoleKind, RoleName, RunRef,
    UnknownStatePolicy, WorkItem, WorkItemId, WorkItemStatusClass,
};

fn round_trip<T>(value: &T) -> T
where
    T: Serialize + for<'de> Deserialize<'de>,
{
    let json = serde_json::to_string(value).expect("serialize");
    serde_json::from_str(&json).expect("deserialize")
}

fn assert_round_trip<T>(value: T)
where
    T: Serialize + for<'de> Deserialize<'de> + PartialEq + std::fmt::Debug,
{
    let back = round_trip(&value);
    assert_eq!(back, value);
}

// ---------- ids: transparent newtypes ----------

#[test]
fn id_newtypes_serialize_transparently() {
    assert_eq!(serde_json::to_string(&WorkItemId::new(7)).unwrap(), "7");
    assert_eq!(serde_json::to_string(&BlockerId::new(7)).unwrap(), "7");
    assert_eq!(serde_json::to_string(&RunRef::new(7)).unwrap(), "7");
    assert_eq!(serde_json::to_string(&QaVerdictId::new(7)).unwrap(), "7");
    assert_eq!(serde_json::to_string(&IntegrationId::new(7)).unwrap(), "7");
    assert_eq!(serde_json::to_string(&FollowupId::new(7)).unwrap(), "7");
    assert_eq!(
        serde_json::to_string(&DecompositionId::new(7)).unwrap(),
        "7"
    );
    assert_eq!(
        serde_json::to_string(&ChildKey::new("backend")).unwrap(),
        "\"backend\""
    );
    assert_eq!(
        serde_json::to_string(&IssueId::new("ENG-1")).unwrap(),
        "\"ENG-1\""
    );
    assert_eq!(
        serde_json::to_string(&RoleName::new("qa")).unwrap(),
        "\"qa\""
    );

    assert_round_trip(WorkItemId::new(42));
    assert_round_trip(BlockerId::new(42));
    assert_round_trip(RunRef::new(42));
    assert_round_trip(QaVerdictId::new(42));
    assert_round_trip(IntegrationId::new(42));
    assert_round_trip(FollowupId::new(42));
    assert_round_trip(DecompositionId::new(42));
    assert_round_trip(ChildKey::new("ui"));
    assert_round_trip(IssueId::new("abc-123"));
    assert_round_trip(RoleName::new("integration_owner"));
}

// ---------- enums: snake_case wire form ----------

#[test]
fn work_item_status_class_round_trips_every_variant() {
    for class in WorkItemStatusClass::ALL {
        let json = serde_json::to_string(&class).unwrap();
        assert_eq!(json, format!("\"{}\"", class.as_str()));
        assert_eq!(
            serde_json::from_str::<WorkItemStatusClass>(&json).unwrap(),
            class
        );
    }
}

#[test]
fn unknown_state_policy_round_trips_every_variant() {
    for policy in [UnknownStatePolicy::Error, UnknownStatePolicy::Ignore] {
        assert_round_trip(policy);
    }
    assert_eq!(
        serde_json::to_string(&UnknownStatePolicy::Ignore).unwrap(),
        "\"ignore\""
    );
}

#[test]
fn role_kind_round_trips_every_variant() {
    for kind in RoleKind::ALL {
        let json = serde_json::to_string(&kind).unwrap();
        assert_eq!(json, format!("\"{}\"", kind.as_str()));
        assert_eq!(serde_json::from_str::<RoleKind>(&json).unwrap(), kind);
    }
}

#[test]
fn blocker_status_round_trips_every_variant() {
    for status in BlockerStatus::ALL {
        let json = serde_json::to_string(&status).unwrap();
        assert_eq!(json, format!("\"{}\"", status.as_str()));
        assert_eq!(
            serde_json::from_str::<BlockerStatus>(&json).unwrap(),
            status
        );
    }
}

#[test]
fn blocker_severity_round_trips_every_variant() {
    for sev in BlockerSeverity::ALL {
        let json = serde_json::to_string(&sev).unwrap();
        assert_eq!(json, format!("\"{}\"", sev.as_str()));
        assert_eq!(serde_json::from_str::<BlockerSeverity>(&json).unwrap(), sev);
    }
}

#[test]
fn ready_for_round_trips_every_variant() {
    for r in ReadyFor::ALL {
        let json = serde_json::to_string(&r).unwrap();
        assert_eq!(json, format!("\"{}\"", r.as_str()));
        assert_eq!(serde_json::from_str::<ReadyFor>(&json).unwrap(), r);
    }
}

#[test]
fn qa_verdict_round_trips_every_variant() {
    for v in QaVerdict::ALL {
        let json = serde_json::to_string(&v).unwrap();
        assert_eq!(json, format!("\"{}\"", v.as_str()));
        assert_eq!(serde_json::from_str::<QaVerdict>(&json).unwrap(), v);
    }
}

#[test]
fn acceptance_criterion_status_round_trips_every_variant() {
    for s in AcceptanceCriterionStatus::ALL {
        let json = serde_json::to_string(&s).unwrap();
        assert_eq!(json, format!("\"{}\"", s.as_str()));
        assert_eq!(
            serde_json::from_str::<AcceptanceCriterionStatus>(&json).unwrap(),
            s
        );
    }
}

#[test]
fn integration_status_round_trips_every_variant() {
    for s in IntegrationStatus::ALL {
        let json = serde_json::to_string(&s).unwrap();
        assert_eq!(json, format!("\"{}\"", s.as_str()));
        assert_eq!(serde_json::from_str::<IntegrationStatus>(&json).unwrap(), s);
    }
}

#[test]
fn integration_merge_strategy_round_trips_every_variant() {
    for s in IntegrationMergeStrategy::ALL {
        let json = serde_json::to_string(&s).unwrap();
        assert_eq!(json, format!("\"{}\"", s.as_str()));
        assert_eq!(
            serde_json::from_str::<IntegrationMergeStrategy>(&json).unwrap(),
            s
        );
    }
}

#[test]
fn followup_policy_round_trips_every_variant() {
    for p in FollowupPolicy::ALL {
        let json = serde_json::to_string(&p).unwrap();
        assert_eq!(json, format!("\"{}\"", p.as_str()));
        assert_eq!(serde_json::from_str::<FollowupPolicy>(&json).unwrap(), p);
    }
}

#[test]
fn followup_status_round_trips_every_variant() {
    for s in FollowupStatus::ALL {
        let json = serde_json::to_string(&s).unwrap();
        assert_eq!(json, format!("\"{}\"", s.as_str()));
        assert_eq!(serde_json::from_str::<FollowupStatus>(&json).unwrap(), s);
    }
}

// ---------- structs ----------

#[test]
fn blocker_origin_round_trips_each_variant() {
    let run = BlockerOrigin::Run {
        run_id: RunRef::new(1),
        role: Some(RoleName::new("qa")),
    };
    let agent = BlockerOrigin::Agent {
        role: RoleName::new("watchdog"),
        agent_profile: Some("recovery".into()),
    };
    let human = BlockerOrigin::Human {
        actor: "alice".into(),
    };
    assert_round_trip(run);
    assert_round_trip(agent);
    assert_round_trip(human);
}

#[test]
fn blocker_round_trips_with_full_payload() {
    let mut b = Blocker::try_new(
        BlockerId::new(1),
        WorkItemId::new(10),
        WorkItemId::new(20),
        "missing migration",
        BlockerOrigin::Run {
            run_id: RunRef::new(7),
            role: Some(RoleName::new("qa")),
        },
        BlockerSeverity::High,
    )
    .unwrap();
    b.transition_status(
        BlockerStatus::Waived,
        Some(RoleName::new("platform_lead")),
        Some("waived for hotfix window".into()),
    )
    .unwrap();
    assert_round_trip(b);
}

#[test]
fn role_authority_and_overrides_round_trip() {
    assert_round_trip(RoleAuthority::defaults_for(RoleKind::IntegrationOwner));
    assert_round_trip(RoleAuthority::defaults_for(RoleKind::QaGate));
    assert_round_trip(RoleAuthority::NONE);
    assert_round_trip(RoleAuthorityOverrides {
        can_decompose: Some(true),
        can_assign: Some(false),
        can_request_qa: None,
        can_close_parent: Some(true),
        can_file_blockers: None,
        can_file_followups: Some(true),
        required_for_done: Some(false),
    });
}

#[test]
fn role_context_round_trips() {
    let ctx = RoleContext {
        name: RoleName::new("platform_lead"),
        kind: RoleKind::IntegrationOwner,
        authority: RoleAuthority::defaults_for(RoleKind::IntegrationOwner),
        agent_profile: Some("claude-haiku".into()),
        max_concurrent: Some(2),
        description: Some("Owns integration".into()),
    };
    assert_round_trip(ctx);
    // bare-bones default-shaped variant
    assert_round_trip(RoleContext::from_kind("qa", RoleKind::QaGate));
}

#[test]
fn handoff_request_subtypes_round_trip() {
    assert_round_trip(BranchOrWorkspace {
        branch: Some("feat/foo".into()),
        workspace_path: Some("../wt-foo".into()),
        base_ref: Some("main".into()),
    });
    assert_round_trip(BranchOrWorkspace::default());
    assert_round_trip(HandoffBlockerRequest {
        blocking_id: Some(WorkItemId::new(11)),
        reason: "ci failed".into(),
        severity: BlockerSeverity::Medium,
    });
    assert_round_trip(HandoffFollowupRequest {
        title: "extract helper".into(),
        summary: "duplicated logic".into(),
        blocking: false,
        propose_only: true,
    });
}

#[test]
fn handoff_round_trips_with_full_payload() {
    let h = Handoff {
        summary: "implemented schema".into(),
        changed_files: vec!["src/a.rs".into(), "src/b.rs".into()],
        tests_run: vec!["cargo test --workspace".into()],
        verification_evidence: vec!["log://run/42".into()],
        known_risks: vec!["large diff".into()],
        blockers_created: vec![HandoffBlockerRequest {
            blocking_id: None,
            reason: "needs schema review".into(),
            severity: BlockerSeverity::High,
        }],
        followups_created_or_proposed: vec![HandoffFollowupRequest {
            title: "refactor".into(),
            summary: "follow-up".into(),
            blocking: false,
            propose_only: true,
        }],
        branch_or_workspace: BranchOrWorkspace {
            branch: Some("feat/x".into()),
            workspace_path: None,
            base_ref: Some("main".into()),
        },
        ready_for: ReadyFor::Qa,
        block_reason: None,
        reporting_role: Some(RoleName::new("specialist-rust")),
        verdict_request: None,
    };
    h.validate().unwrap();
    assert_round_trip(h);
}

#[test]
fn qa_subtypes_and_outcome_round_trip() {
    let trace = AcceptanceCriterionTrace {
        criterion: "tests pass".into(),
        status: AcceptanceCriterionStatus::Verified,
        evidence: vec!["cargo test".into()],
        notes: Some("clean run".into()),
    };
    assert_round_trip(trace.clone());
    let evidence = QaEvidence {
        tests_run: vec!["cargo test".into()],
        changed_files_reviewed: vec!["src/a.rs".into()],
        ci_status: vec!["ci://ok".into()],
        runtime_evidence: vec!["screenshot.png".into()],
        notes: vec!["looked good".into()],
        accepted_limitation: None,
    };
    assert_round_trip(evidence.clone());

    let outcome = QaOutcome::try_new(
        QaVerdictId::new(1),
        WorkItemId::new(10),
        RunRef::new(7),
        RoleName::new("qa"),
        QaVerdict::Passed,
        evidence,
        vec![trace],
        vec![],
        None,
        None,
    )
    .unwrap();
    assert_round_trip(outcome);
}

#[test]
fn integration_subtypes_and_record_round_trip() {
    assert_round_trip(IntegrationConflict {
        child_id: Some(WorkItemId::new(11)),
        child_branch: Some("feat/child".into()),
        files: vec!["src/a.rs".into()],
        message: "merge conflict".into(),
    });

    let rec = IntegrationRecord::try_new(
        IntegrationId::new(1),
        WorkItemId::new(99),
        RoleName::new("platform_lead"),
        Some(RunRef::new(7)),
        IntegrationStatus::Succeeded,
        IntegrationMergeStrategy::SequentialCherryPick,
        Some("symphony/integration/PROJ-42".into()),
        Some("main".into()),
        Some("deadbeef".into()),
        Some("../wt-int".into()),
        vec![WorkItemId::new(11), WorkItemId::new(12)],
        Some("Consolidated 2 children".into()),
        vec![],
        vec![],
    )
    .unwrap();
    assert_round_trip(rec);
}

#[test]
fn followup_request_round_trips_for_each_policy() {
    for policy in FollowupPolicy::ALL {
        let f = FollowupIssueRequest::try_new(
            FollowupId::new(1),
            WorkItemId::new(10),
            "extract helper",
            "duplicated parsing across two callers",
            "carve helper out of `parse_thing`",
            vec!["helper has unit tests".into()],
            false,
            policy,
            BlockerOrigin::Human {
                actor: "alice".into(),
            },
            true,
        )
        .unwrap();
        assert_round_trip(f);
    }
}

#[test]
fn work_item_round_trips_with_optional_fields() {
    let item = WorkItem {
        id: WorkItemId::new(1),
        tracker_id: "github".into(),
        identifier: "owner/repo#42".into(),
        title: "Add feature".into(),
        description: Some("body".into()),
        tracker_status: TrackerStatus::new("In Progress"),
        status_class: WorkItemStatusClass::Running,
        priority: Some(2),
        labels: vec!["backend".into(), "p1".into()],
        url: Some("https://example/42".into()),
        parent_id: Some(WorkItemId::new(99)),
    };
    assert_round_trip(item);

    let minimal = WorkItem::minimal(
        2,
        "linear",
        "ENG-7",
        "t",
        "Todo",
        WorkItemStatusClass::Intake,
    );
    assert_round_trip(minimal);
}

#[test]
fn tracker_status_preserves_casing_and_compares_case_insensitively() {
    let status = TrackerStatus::new("In Progress");
    let json = serde_json::to_string(&status).unwrap();
    assert_eq!(json, "\"In Progress\"");
    let back: TrackerStatus = serde_json::from_str(&json).unwrap();
    assert_eq!(back.as_str(), "In Progress");
    // Case-insensitive equality survives the round-trip.
    assert_eq!(back, TrackerStatus::new("in progress"));
}

#[test]
fn issue_state_preserves_casing_and_compares_case_insensitively() {
    let state = IssueState::new("In Progress");
    let json = serde_json::to_string(&state).unwrap();
    assert_eq!(json, "\"In Progress\"");
    let back: IssueState = serde_json::from_str(&json).unwrap();
    assert_eq!(back.as_str(), "In Progress");
    assert_eq!(back, IssueState::new("IN PROGRESS"));
}

#[test]
fn blocker_ref_round_trips_with_partial_population() {
    assert_round_trip(BlockerRef {
        id: Some(IssueId::new("xyz")),
        identifier: Some("ENG-7".into()),
        state: Some(IssueState::new("Done")),
    });
    assert_round_trip(BlockerRef {
        id: None,
        identifier: None,
        state: None,
    });
}

#[test]
fn issue_round_trips_with_full_and_minimal_payloads() {
    let full = Issue {
        id: IssueId::new("uuid-1"),
        identifier: "ENG-1".into(),
        title: "do thing".into(),
        description: Some("body".into()),
        priority: Some(1),
        state: IssueState::new("In Progress"),
        branch_name: Some("feat/eng-1".into()),
        url: Some("https://example/eng-1".into()),
        labels: vec!["bug".into()],
        blocked_by: vec![BlockerRef {
            id: Some(IssueId::new("uuid-2")),
            identifier: Some("ENG-2".into()),
            state: Some(IssueState::new("Todo")),
        }],
        created_at: Some("2025-01-01T00:00:00Z".into()),
        updated_at: Some("2025-01-02T00:00:00Z".into()),
    };
    assert_round_trip(full);
    assert_round_trip(Issue::minimal("uuid-2", "ENG-2", "title", "Todo"));
}

#[test]
fn decomposition_status_round_trips_every_variant() {
    for status in DecompositionStatus::ALL {
        let json = serde_json::to_string(&status).unwrap();
        assert_eq!(json, format!("\"{}\"", status.as_str()));
        assert_eq!(
            serde_json::from_str::<DecompositionStatus>(&json).unwrap(),
            status
        );
    }
}

#[test]
fn decomposition_proposal_round_trips() {
    let mut deps = std::collections::BTreeSet::new();
    deps.insert(ChildKey::new("a"));
    let a = ChildProposal::try_new(
        "a",
        "implement backend",
        "models + endpoints",
        RoleName::new("backend"),
        Some("a body".into()),
        vec!["passes integration tests".into()],
        std::collections::BTreeSet::new(),
        true,
        Some("symphony/eng-1-backend".into()),
        true,
    )
    .unwrap();
    let b = ChildProposal::try_new(
        "b",
        "implement ui",
        "react surface",
        RoleName::new("frontend"),
        None,
        vec!["screenshot in PR".into()],
        deps,
        false,
        None,
        true,
    )
    .unwrap();
    let proposal = DecompositionProposal::try_new(
        DecompositionId::new(9),
        WorkItemId::new(100),
        RoleName::new("platform_lead"),
        "split parent into backend + ui",
        vec![a, b],
        FollowupPolicy::ProposeForApproval,
    )
    .unwrap();
    assert_round_trip(proposal);
}
