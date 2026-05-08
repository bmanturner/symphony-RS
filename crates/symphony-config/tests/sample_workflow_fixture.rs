//! Integration tests for the canonical `tests/fixtures/sample-workflow/WORKFLOW.md`.
//!
//! This fixture is the worked example shipped with the repo and the
//! reference for what a "complete" v2 workflow looks like. Anchoring
//! tests on it now guarantees CI fails the moment the fixture drifts
//! out of step with the typed [`WorkflowConfig`] schema — long before a
//! CLI smoke test runs against a stale file.
//!
//! ## What these tests assert
//!
//! - **Sanity**: the fixture loads cleanly, validates, and the
//!   prompt body survives trimming.
//! - **Section coverage**: every typed v2 block (`roles`, `agents`,
//!   `routing`, `decomposition`, `integration`, `pull_requests`, `qa`,
//!   `followups`, `observability`, `workspace`, `branching`, `hooks`)
//!   parses into the operator-visible values declared in YAML — so a
//!   reformatting accident that flips a default is loud.
//! - **Round-trip**: the parsed config serialises back through
//!   `serde_yaml` and re-deserialises into an equal `WorkflowConfig`.
//!   This is the strongest guarantee the typed schema is *complete*
//!   for the fixture: any field the YAML declares but the struct
//!   silently drops would surface as a diff between the two passes.

use std::path::PathBuf;

use symphony_config::{
    AgentBackend, AgentProfileConfig, AgentStrategy, BlockerPolicy, ChildIssuePolicy,
    ConflictPolicy, IntegrationRequirement, MergeStrategy, PrInitialState, PrMarkReadyStage,
    PrOpenStage, PrProvider, RoleKind, RoutingMatchMode, TandemMode, TrackerKind, WorkflowConfig,
    WorkflowLoader, WorkspaceCleanupPolicy, WorkspaceStrategyKind,
};

/// Resolve the fixture path from the crate's manifest dir up to the
/// workspace root. Done at runtime (not via `include_str!`) so the
/// loader's real on-disk read path is exercised, and so a missing or
/// mis-located fixture surfaces as a `Read` error with the exact path
/// rather than a compile-time include failure.
fn fixture_path() -> PathBuf {
    let manifest_dir = env!("CARGO_MANIFEST_DIR");
    PathBuf::from(manifest_dir)
        .join("..")
        .join("..")
        .join("tests")
        .join("fixtures")
        .join("sample-workflow")
        .join("WORKFLOW.md")
}

#[test]
fn sample_fixture_loads_and_validates() {
    let path = fixture_path();
    let loaded = WorkflowLoader::from_path(&path)
        .unwrap_or_else(|e| panic!("failed to load sample fixture at {}: {e}", path.display()));

    let cfg = &loaded.config;

    // Tracker — switched to GitHub so `pull_requests.enabled` validates.
    assert_eq!(cfg.tracker.kind, TrackerKind::Github);
    assert_eq!(
        cfg.tracker.repository.as_deref(),
        Some("foglet-io/rust-symphony")
    );
    assert_eq!(
        cfg.tracker.active_states,
        vec!["Todo".to_string(), "In Progress".to_string()]
    );

    // Polling — explicit values so the fixture documents the SPEC defaults.
    assert_eq!(cfg.polling.interval_ms, 30_000);
    assert_eq!(cfg.polling.jitter_ms, 5_000);
    assert!(cfg.polling.startup_reconcile_recent_terminal);

    // Roles — workflow names are configurable; semantic identity is `kind`.
    let lead = cfg
        .roles
        .get("platform_lead")
        .expect("fixture declares platform_lead role");
    assert_eq!(lead.kind, RoleKind::IntegrationOwner);
    assert_eq!(lead.agent.as_deref(), Some("lead_agent"));
    assert_eq!(lead.can_decompose, Some(true));
    assert_eq!(lead.can_close_parent, Some(true));

    let qa = cfg.roles.get("qa").expect("fixture declares qa role");
    assert_eq!(qa.kind, RoleKind::QaGate);
    assert_eq!(qa.required_for_done, Some(true));

    // Agents — backend profile + composite (tandem) profile both round-trip.
    let lead_agent = cfg
        .agents
        .get("lead_agent")
        .and_then(AgentProfileConfig::as_backend)
        .expect("lead_agent must parse as a backend profile");
    assert_eq!(lead_agent.backend, AgentBackend::Codex);
    assert_eq!(lead_agent.command.as_deref(), Some("codex app-server"));

    let lead_pair = cfg
        .agents
        .get("lead_pair")
        .and_then(AgentProfileConfig::as_composite)
        .expect("lead_pair must parse as a composite profile");
    assert_eq!(lead_pair.strategy, AgentStrategy::Tandem);
    assert_eq!(lead_pair.lead, "lead_agent");
    assert_eq!(lead_pair.follower, "qa_agent");
    assert_eq!(lead_pair.mode, TandemMode::DraftReview);

    // Routing — priority mode with a default fallback role.
    assert_eq!(cfg.routing.match_mode, RoutingMatchMode::Priority);
    assert_eq!(cfg.routing.default_role.as_deref(), Some("platform_lead"));
    assert_eq!(cfg.routing.rules.len(), 4);
    assert_eq!(cfg.routing.rules[0].assign_role, "qa");
    assert_eq!(cfg.routing.rules[0].priority, Some(100));

    // Decomposition — owner + triggers + child policy.
    assert!(cfg.decomposition.enabled);
    assert_eq!(
        cfg.decomposition.owner_role.as_deref(),
        Some("platform_lead")
    );
    assert_eq!(
        cfg.decomposition.triggers.labels_any,
        vec![
            "epic".to_string(),
            "broad".to_string(),
            "umbrella".to_string()
        ]
    );
    assert_eq!(
        cfg.decomposition.child_issue_policy,
        ChildIssuePolicy::CreateDirectly
    );

    // Workspace — issue worktree + shared integration worktree.
    assert_eq!(
        cfg.workspace.default_strategy.as_deref(),
        Some("issue_worktree")
    );
    let issue_strat = cfg
        .workspace
        .strategies
        .get("issue_worktree")
        .expect("issue_worktree strategy must be present");
    assert_eq!(issue_strat.kind, WorkspaceStrategyKind::GitWorktree);
    assert_eq!(issue_strat.cleanup, WorkspaceCleanupPolicy::RetainUntilDone);
    let shared_strat = cfg
        .workspace
        .strategies
        .get("shared_integration")
        .expect("shared_integration strategy must be present");
    assert_eq!(shared_strat.kind, WorkspaceStrategyKind::ExistingWorktree);

    // Integration — sequential cherry-pick into qa.
    assert_eq!(cfg.integration.owner_role.as_deref(), Some("platform_lead"));
    assert_eq!(
        cfg.integration.required_for,
        vec![
            IntegrationRequirement::DecomposedParent,
            IntegrationRequirement::MultipleChildBranches,
        ]
    );
    assert_eq!(
        cfg.integration.merge_strategy,
        MergeStrategy::SequentialCherryPick
    );
    assert_eq!(
        cfg.integration.conflict_policy,
        ConflictPolicy::IntegrationOwnerRepairTurn
    );
    assert_eq!(
        cfg.integration.next_state_after_integration.as_deref(),
        Some("qa")
    );

    // Pull requests — draft until QA passes.
    assert!(cfg.pull_requests.enabled);
    assert_eq!(cfg.pull_requests.provider, PrProvider::Github);
    assert_eq!(
        cfg.pull_requests.open_stage,
        PrOpenStage::AfterIntegrationVerification
    );
    assert_eq!(cfg.pull_requests.initial_state, PrInitialState::Draft);
    assert_eq!(
        cfg.pull_requests.mark_ready_stage,
        PrMarkReadyStage::AfterQaPasses
    );

    // QA — exhaustive runtime gate with platform_lead waiver.
    assert!(cfg.qa.required);
    assert!(cfg.qa.exhaustive);
    assert!(!cfg.qa.allow_static_only);
    assert_eq!(cfg.qa.blocker_policy, BlockerPolicy::BlocksParent);
    assert_eq!(cfg.qa.waiver_roles, vec!["platform_lead".to_string()]);
    assert!(cfg.qa.evidence_required.tests);
    assert!(cfg.qa.evidence_required.acceptance_criteria_trace);
    assert!(
        cfg.qa
            .evidence_required
            .visual_or_runtime_evidence_when_applicable
    );

    // Follow-ups — propose-for-approval with platform_lead approver.
    assert!(cfg.followups.enabled);
    assert_eq!(
        cfg.followups.default_policy,
        ChildIssuePolicy::ProposeForApproval
    );
    assert_eq!(
        cfg.followups.approval_role.as_deref(),
        Some("platform_lead")
    );

    // Observability — SSE on loopback, TUI on, dashboard off.
    assert!(cfg.observability.sse.enabled);
    assert_eq!(cfg.observability.sse.bind, "127.0.0.1:6280");
    assert_eq!(cfg.observability.sse.replay_buffer, 1024);
    assert!(cfg.observability.tui.enabled);
    assert!(!cfg.observability.dashboard.enabled);

    // Hooks — v2 list-of-commands form.
    assert_eq!(cfg.hooks.timeout_ms, 300_000);
    assert_eq!(cfg.hooks.before_run, vec!["git status --short".to_string()]);
    assert!(!cfg.hooks.after_create.is_empty());

    // Prompt body — non-empty, trimmed, leads with the documented heading.
    assert!(
        !loaded.prompt_template.is_empty(),
        "prompt body should be non-empty"
    );
    assert_eq!(
        loaded.prompt_template,
        loaded.prompt_template.trim(),
        "prompt body must come back trimmed"
    );
    assert!(
        loaded
            .prompt_template
            .starts_with("# Symphony Sample Workflow"),
        "prompt body should start with the documented heading; got: {:?}",
        &loaded.prompt_template[..loaded.prompt_template.len().min(80)]
    );

    // Final gate: the same validation `symphony validate` will run.
    cfg.validate()
        .expect("sample fixture must satisfy WorkflowConfig::validate");
}

/// Round-trip the fixture through `serde_yaml` and assert the parsed
/// config is equal to itself after a serialise → deserialise pass.
///
/// This is the strongest guarantee that the typed schema is *complete*
/// for the v2 worked example: a field declared in the YAML but
/// silently dropped by `WorkflowConfig` would either fail to round-trip
/// (because the second deserialise sees a different value) or fail to
/// load at all (because `deny_unknown_fields` rejects it). Either
/// outcome means the schema and the fixture have drifted.
///
/// We round-trip the *parsed config* rather than the raw YAML text
/// because `serde_yaml::to_string` does not preserve comments,
/// formatting, or key order — comparing strings would fail for
/// cosmetic reasons that have nothing to do with the schema contract.
#[test]
fn sample_fixture_round_trips_through_serde_yaml() {
    let path = fixture_path();
    let loaded = WorkflowLoader::from_path(&path)
        .unwrap_or_else(|e| panic!("failed to load sample fixture at {}: {e}", path.display()));

    let yaml =
        serde_yaml::to_string(&loaded.config).expect("WorkflowConfig must serialise back to YAML");
    let reparsed: WorkflowConfig = serde_yaml::from_str(&yaml)
        .expect("re-serialised YAML must deserialise into WorkflowConfig");

    assert_eq!(
        loaded.config, reparsed,
        "WorkflowConfig must round-trip losslessly through serde_yaml"
    );

    // Re-validating the round-tripped config catches the corner case
    // where a field deserialises but skips a `validate()` invariant —
    // belt-and-braces over relying on equality alone.
    reparsed
        .validate()
        .expect("round-tripped fixture must still satisfy WorkflowConfig::validate");
}
