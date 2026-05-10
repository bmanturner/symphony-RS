use std::path::Path;

use serde::Deserialize;
use std::collections::BTreeMap;
use symphony_config::{RoleConfig, RoleKind, TrackerKind, WorkflowLoader};

const WORKFLOW_DOC: &str = include_str!("../../../docs/workflow.md");
const ROLES_DOC: &str = include_str!("../../../docs/roles.md");
const WORKSPACES_DOC: &str = include_str!("../../../docs/workspaces.md");
const QA_DOC: &str = include_str!("../../../docs/qa.md");
const UPGRADE_DOC: &str = include_str!("../../../docs/upgrade.md");
const README_DOC: &str = include_str!("../../../README.md");
const SEQUENTIAL_DECOMPOSITION_SCENARIO: &str =
    include_str!("../../../tests/fixtures/decomposition-sequential/SCENARIO.md");
const PARALLEL_SEQUENTIAL_DECOMPOSITION_SCENARIO: &str =
    include_str!("../../../tests/fixtures/decomposition-parallel-sequential/SCENARIO.md");

#[test]
fn complete_workflow_example_in_docs_loads_and_validates() {
    let example = extract_complete_example(WORKFLOW_DOC);
    let loaded = WorkflowLoader::from_str_with_path(example, Path::new("docs/workflow.md"))
        .expect("documented complete workflow example should load");

    loaded
        .config
        .validate()
        .expect("documented complete workflow example should validate");

    assert!(
        loaded
            .prompt_template
            .starts_with("# Symphony Agent Instructions"),
        "documented workflow should include a prompt body"
    );
}

fn extract_complete_example(doc: &str) -> &str {
    let marked = doc
        .split_once("<!-- workflow-example:start -->")
        .and_then(|(_, rest)| {
            rest.split_once("<!-- workflow-example:end -->")
                .map(|(s, _)| s)
        })
        .expect("workflow example markers must exist");

    let fenced = marked
        .split_once("```markdown")
        .and_then(|(_, rest)| rest.split_once("```").map(|(s, _)| s))
        .expect("workflow example must be fenced as markdown");

    fenced.trim()
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct RolesDocExample {
    roles: BTreeMap<String, RoleConfig>,
}

#[test]
fn roles_doc_example_parses_as_role_config() {
    let example = extract_marked_yaml(ROLES_DOC, "roles-example");
    let parsed: RolesDocExample =
        serde_yaml::from_str(example).expect("documented roles example should parse");

    let lead = parsed
        .roles
        .get("platform_lead")
        .expect("roles doc declares platform_lead");
    assert_eq!(lead.kind, RoleKind::IntegrationOwner);
    assert_eq!(lead.agent.as_deref(), Some("lead_agent"));
    assert_eq!(lead.can_decompose, Some(true));
    assert_eq!(lead.can_assign, Some(true));
    assert_eq!(lead.can_request_qa, Some(true));
    assert_eq!(lead.can_close_parent, Some(true));

    let qa = parsed.roles.get("qa").expect("roles doc declares qa");
    assert_eq!(qa.kind, RoleKind::QaGate);
    assert_eq!(qa.can_file_blockers, Some(true));
    assert_eq!(qa.can_file_followups, Some(true));
    assert_eq!(qa.required_for_done, Some(true));

    assert_eq!(
        parsed
            .roles
            .get("backend_engineer")
            .expect("roles doc declares backend specialist")
            .kind,
        RoleKind::Specialist
    );
    assert_eq!(
        parsed
            .roles
            .get("security_reviewer")
            .expect("roles doc declares reviewer")
            .kind,
        RoleKind::Reviewer
    );
    assert_eq!(
        parsed
            .roles
            .get("release_operator")
            .expect("roles doc declares operator")
            .kind,
        RoleKind::Operator
    );
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct WorkspacesDocExample {
    workspace: symphony_config::WorkspacePolicyConfig,
    branching: symphony_config::BranchPolicyConfig,
}

#[test]
fn workspaces_doc_example_parses_as_workspace_and_branch_config() {
    let example = extract_marked_yaml(WORKSPACES_DOC, "workspaces-example");
    let parsed: WorkspacesDocExample =
        serde_yaml::from_str(example).expect("documented workspaces example should parse");

    assert_eq!(
        parsed.workspace.default_strategy.as_deref(),
        Some("issue_worktree")
    );
    assert!(parsed.workspace.require_cwd_in_workspace);
    assert!(parsed.workspace.forbid_untracked_outside_workspace);

    let issue = parsed
        .workspace
        .strategies
        .get("issue_worktree")
        .expect("workspaces doc declares issue_worktree");
    assert_eq!(
        issue.kind,
        symphony_config::WorkspaceStrategyKind::GitWorktree
    );
    assert_eq!(issue.base.as_deref(), Some("main"));
    assert_eq!(
        issue.branch_template.as_deref(),
        Some("symphony/{{identifier}}")
    );

    let integration = parsed
        .workspace
        .strategies
        .get("integration_worktree")
        .expect("workspaces doc declares integration_worktree");
    assert_eq!(
        integration.kind,
        symphony_config::WorkspaceStrategyKind::ExistingWorktree
    );
    assert_eq!(
        integration.require_branch.as_deref(),
        Some("symphony/integration/{{parent_identifier}}")
    );

    assert!(!parsed.branching.allow_same_branch_for_children);
    assert!(parsed.branching.require_clean_tree_before_run);
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct QaDocExample {
    roles: BTreeMap<String, RoleConfig>,
    qa: symphony_config::QaConfig,
}

#[test]
fn qa_doc_example_parses_as_qa_config() {
    let example = extract_marked_yaml(QA_DOC, "qa-example");
    let parsed: QaDocExample =
        serde_yaml::from_str(example).expect("documented QA example should parse");

    assert_eq!(
        parsed
            .roles
            .get("qa")
            .expect("QA doc declares qa role")
            .kind,
        RoleKind::QaGate
    );
    assert_eq!(
        parsed
            .roles
            .get("platform_lead")
            .expect("QA doc declares waiver role")
            .kind,
        RoleKind::IntegrationOwner
    );

    assert_eq!(parsed.qa.owner_role.as_deref(), Some("qa"));
    assert!(parsed.qa.required);
    assert!(parsed.qa.exhaustive);
    assert!(!parsed.qa.allow_static_only);
    assert!(parsed.qa.can_file_blockers);
    assert!(parsed.qa.can_file_followups);
    assert_eq!(parsed.qa.waiver_roles, vec!["platform_lead".to_string()]);
    assert!(parsed.qa.evidence_required.tests);
    assert!(parsed.qa.evidence_required.changed_files_review);
    assert!(parsed.qa.evidence_required.acceptance_criteria_trace);
    assert!(
        parsed
            .qa
            .evidence_required
            .visual_or_runtime_evidence_when_applicable
    );
}

#[test]
fn upgrade_doc_covers_v2_migration_contract() {
    for required in [
        "tests/fixtures/sample-workflow/WORKFLOW.md",
        "tests/fixtures/quickstart-workflow/WORKFLOW.md",
        "docs/legacy/",
        "docs/workflow.md",
        "SPEC_v3.md",
        "ARCHITECTURE_v3.md",
        "PLAN_v3.md",
        "CHECKLIST_v3.md",
        "strategy: tandem",
        "Keep Mocks Test-Only",
        "kind: integration_owner",
        "kind: qa_gate",
        "symphony validate WORKFLOW.md",
    ] {
        assert!(
            UPGRADE_DOC.contains(required),
            "upgrade doc should mention {required:?}"
        );
    }
}

#[test]
fn quickstart_fixture_loads_and_readme_covers_v2_walkthrough() {
    let fixture_path = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../../tests/fixtures/quickstart-workflow/WORKFLOW.md");
    let loaded = WorkflowLoader::from_path(&fixture_path)
        .expect("quickstart WORKFLOW.md fixture should load");

    loaded
        .config
        .validate()
        .expect("quickstart WORKFLOW.md fixture should validate");

    assert_eq!(loaded.config.tracker.kind, TrackerKind::Mock);
    assert_eq!(
        loaded
            .config
            .roles
            .get("platform_lead")
            .expect("quickstart declares platform_lead")
            .kind,
        RoleKind::IntegrationOwner
    );
    assert_eq!(
        loaded
            .config
            .roles
            .get("qa")
            .expect("quickstart declares qa")
            .kind,
        RoleKind::QaGate
    );
    assert!(
        loaded.config.observability.event_bus,
        "quickstart should keep durable event broadcasting enabled"
    );

    for required in [
        "tests/fixtures/quickstart-workflow/",
        "symphony validate tests/fixtures/quickstart-workflow/WORKFLOW.md",
        "symphony status tests/fixtures/quickstart-workflow/WORKFLOW.md",
        "symphony run tests/fixtures/quickstart-workflow/WORKFLOW.md",
        "kind: integration_owner",
        "kind: qa_gate",
        "Mock adapters are test-only",
        "docs/workflow.md",
        "SPEC_v3.md",
    ] {
        assert!(
            README_DOC.contains(required),
            "README quickstart should mention {required:?}"
        );
    }
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct DecompositionScenario {
    parent: ScenarioParent,
    children: Vec<ScenarioChild>,
    expected_blocks_edges: Vec<ScenarioBlocksEdge>,
    dispatch_order: ScenarioDispatchOrder,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct ScenarioParent {
    key: String,
    title: String,
    integration_owner: String,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct ScenarioChild {
    key: String,
    title: String,
    role: String,
    scope: String,
    depends_on: Vec<String>,
    acceptance_criteria: Vec<String>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct ScenarioBlocksEdge {
    blocker: String,
    blocked: String,
    reason: String,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct ScenarioDispatchOrder {
    initially_eligible: Vec<String>,
    blocked_until_terminal: BTreeMap<String, Vec<String>>,
}

#[test]
fn sequential_decomposition_fixture_documents_canonical_dependency_chain() {
    let scenario = extract_marked_yaml(SEQUENTIAL_DECOMPOSITION_SCENARIO, "decomposition-scenario");
    let parsed: DecompositionScenario =
        serde_yaml::from_str(scenario).expect("sequential decomposition scenario should parse");

    assert_eq!(parsed.parent.key, "parent");
    assert_eq!(parsed.parent.integration_owner, "platform_lead");
    assert_eq!(
        parsed
            .children
            .iter()
            .map(|c| c.key.as_str())
            .collect::<Vec<_>>(),
        vec!["A", "B", "C"]
    );
    assert_eq!(
        parsed
            .children
            .iter()
            .map(|c| (c.key.clone(), c.depends_on.clone()))
            .collect::<Vec<_>>(),
        vec![
            ("A".to_string(), Vec::<String>::new()),
            ("B".to_string(), vec!["A".to_string()]),
            ("C".to_string(), vec!["B".to_string()]),
        ]
    );
    assert_eq!(
        parsed
            .expected_blocks_edges
            .iter()
            .map(|edge| (edge.blocker.as_str(), edge.blocked.as_str()))
            .collect::<Vec<_>>(),
        vec![("A", "B"), ("B", "C")],
        "fixture should encode prerequisite -> waiting-child blocks edges"
    );
    assert_eq!(parsed.dispatch_order.initially_eligible, vec!["A"]);
    assert_eq!(
        parsed.dispatch_order.blocked_until_terminal.get("B"),
        Some(&vec!["A".to_string()])
    );
    assert_eq!(
        parsed.dispatch_order.blocked_until_terminal.get("C"),
        Some(&vec!["B".to_string()])
    );

    for child in &parsed.children {
        assert!(!child.title.trim().is_empty());
        assert!(!child.role.trim().is_empty());
        assert!(!child.scope.trim().is_empty());
        assert!(!child.acceptance_criteria.is_empty());
    }
    for edge in &parsed.expected_blocks_edges {
        assert!(
            edge.reason
                .contains(&format!("{} depends on {}", edge.blocked, edge.blocker)),
            "edge reason should keep the dependency direction visible"
        );
    }
    assert!(!parsed.parent.title.trim().is_empty());
}

#[test]
fn parallel_sequential_decomposition_fixture_documents_mixed_dependency_graph() {
    let scenario = extract_marked_yaml(
        PARALLEL_SEQUENTIAL_DECOMPOSITION_SCENARIO,
        "decomposition-scenario",
    );
    let parsed: DecompositionScenario = serde_yaml::from_str(scenario)
        .expect("parallel plus sequential decomposition scenario should parse");

    assert_eq!(parsed.parent.key, "parent");
    assert_eq!(parsed.parent.integration_owner, "platform_lead");
    assert_eq!(
        parsed
            .children
            .iter()
            .map(|c| (c.key.clone(), c.depends_on.clone()))
            .collect::<Vec<_>>(),
        vec![
            ("A".to_string(), Vec::<String>::new()),
            ("B".to_string(), Vec::<String>::new()),
            ("C".to_string(), vec!["A".to_string(), "B".to_string()]),
        ],
        "fixture should encode two parallel roots and one dependent child"
    );
    assert_eq!(
        parsed
            .expected_blocks_edges
            .iter()
            .map(|edge| (edge.blocker.as_str(), edge.blocked.as_str()))
            .collect::<Vec<_>>(),
        vec![("A", "C"), ("B", "C")],
        "both roots should block the waiting child"
    );
    assert_eq!(parsed.dispatch_order.initially_eligible, vec!["A", "B"]);
    assert_eq!(
        parsed.dispatch_order.blocked_until_terminal.get("C"),
        Some(&vec!["A".to_string(), "B".to_string()])
    );

    for child in &parsed.children {
        assert!(!child.title.trim().is_empty());
        assert!(!child.role.trim().is_empty());
        assert!(!child.scope.trim().is_empty());
        assert!(!child.acceptance_criteria.is_empty());
    }
    for edge in &parsed.expected_blocks_edges {
        assert!(
            edge.reason
                .contains(&format!("{} depends on {}", edge.blocked, edge.blocker)),
            "edge reason should keep the dependency direction visible"
        );
    }
    assert!(!parsed.parent.title.trim().is_empty());
}

fn extract_marked_yaml<'a>(doc: &'a str, marker: &str) -> &'a str {
    let start = format!("<!-- {marker}:start -->");
    let end = format!("<!-- {marker}:end -->");
    let marked = doc
        .split_once(&start)
        .and_then(|(_, rest)| rest.split_once(&end).map(|(s, _)| s))
        .unwrap_or_else(|| panic!("{marker} markers must exist"));

    let fenced = marked
        .split_once("```yaml")
        .and_then(|(_, rest)| rest.split_once("```").map(|(s, _)| s))
        .unwrap_or_else(|| panic!("{marker} example must be fenced as yaml"));

    fenced.trim()
}
