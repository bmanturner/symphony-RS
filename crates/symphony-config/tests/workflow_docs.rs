use std::path::Path;

use serde::Deserialize;
use std::collections::BTreeMap;
use symphony_config::{RoleConfig, RoleKind, WorkflowLoader};

const WORKFLOW_DOC: &str = include_str!("../../../docs/workflow.md");
const ROLES_DOC: &str = include_str!("../../../docs/roles.md");

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
