use std::path::Path;

use symphony_config::WorkflowLoader;

const WORKFLOW_DOC: &str = include_str!("../../../docs/workflow.md");

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
