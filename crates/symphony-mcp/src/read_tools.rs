//! Read-only MCP tool handlers.

use std::sync::Arc;

use async_trait::async_trait;
use serde::Deserialize;
use serde_json::{Value, json};
use symphony_config::{InstructionPackBundle, RoleCatalogBuilder, WorkflowConfig};
use symphony_core::tracker_trait::{TrackerError, TrackerRead};

use crate::handler::{ToolError, ToolHandler, ToolResult};

/// Shared read-tool context for one workflow/run.
#[derive(Clone)]
pub struct ReadToolContext {
    /// Live tracker read adapter.
    pub tracker: Arc<dyn TrackerRead>,
    /// Loaded workflow config.
    pub workflow: WorkflowConfig,
    /// Loaded v4 instruction packs for catalog summaries.
    pub instruction_packs: InstructionPackBundle,
    /// Current role name used when rendering role catalog eligibility.
    pub current_role: String,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct IssueInput {
    issue: String,
}

/// `get_issue_details`
pub struct GetIssueDetailsHandler {
    ctx: ReadToolContext,
}

impl GetIssueDetailsHandler {
    pub fn new(ctx: ReadToolContext) -> Self {
        Self { ctx }
    }
}

#[async_trait]
impl ToolHandler for GetIssueDetailsHandler {
    fn name(&self) -> &str {
        "get_issue_details"
    }

    async fn call(&self, input: Value) -> Result<ToolResult, ToolError> {
        let input: IssueInput = parse_input(input)?;
        let issue = self
            .ctx
            .tracker
            .get_issue(&input.issue)
            .await
            .map_err(map_tracker_error)?;
        Ok(ToolResult::json(json!({ "issue": issue })))
    }
}

/// `list_comments`
pub struct ListCommentsHandler {
    ctx: ReadToolContext,
}

impl ListCommentsHandler {
    pub fn new(ctx: ReadToolContext) -> Self {
        Self { ctx }
    }
}

#[async_trait]
impl ToolHandler for ListCommentsHandler {
    fn name(&self) -> &str {
        "list_comments"
    }

    async fn call(&self, input: Value) -> Result<ToolResult, ToolError> {
        let input: IssueInput = parse_input(input)?;
        let comments = self
            .ctx
            .tracker
            .list_comments(&input.issue)
            .await
            .map_err(map_tracker_error)?;
        Ok(ToolResult::json(json!({ "comments": comments })))
    }
}

/// `get_related_issues`
pub struct GetRelatedIssuesHandler {
    ctx: ReadToolContext,
}

impl GetRelatedIssuesHandler {
    pub fn new(ctx: ReadToolContext) -> Self {
        Self { ctx }
    }
}

#[async_trait]
impl ToolHandler for GetRelatedIssuesHandler {
    fn name(&self) -> &str {
        "get_related_issues"
    }

    async fn call(&self, input: Value) -> Result<ToolResult, ToolError> {
        let input: IssueInput = parse_input(input)?;
        let related = self
            .ctx
            .tracker
            .get_related(&input.issue)
            .await
            .map_err(map_tracker_error)?;
        Ok(ToolResult::json(json!({ "related": related })))
    }
}

/// `list_available_roles`
pub struct ListAvailableRolesHandler {
    ctx: ReadToolContext,
}

impl ListAvailableRolesHandler {
    pub fn new(ctx: ReadToolContext) -> Self {
        Self { ctx }
    }
}

#[async_trait]
impl ToolHandler for ListAvailableRolesHandler {
    fn name(&self) -> &str {
        "list_available_roles"
    }

    async fn call(&self, _input: Value) -> Result<ToolResult, ToolError> {
        let catalog = RoleCatalogBuilder {
            workflow: &self.ctx.workflow,
            instruction_packs: &self.ctx.instruction_packs,
            current_role: &self.ctx.current_role,
        }
        .build()
        .render_for_prompt();
        Ok(ToolResult::json(json!({ "catalog": catalog })))
    }
}

fn parse_input<T: for<'de> Deserialize<'de>>(input: Value) -> Result<T, ToolError> {
    serde_json::from_value(input).map_err(|err| ToolError::InvalidPayload {
        field: "$".into(),
        message: err.to_string(),
    })
}

fn map_tracker_error(err: TrackerError) -> ToolError {
    match err {
        TrackerError::Unsupported(capability) => ToolError::CapabilityUnsupported { capability },
        other => ToolError::Internal(other.to_string()),
    }
}

#[cfg(test)]
mod tests {
    use serde_json::json;
    use symphony_core::tracker::{Issue, IssueComment, RelatedIssues};
    use symphony_tracker::MockTracker;

    use super::*;

    fn ctx(tracker: MockTracker) -> ReadToolContext {
        let workflow: WorkflowConfig = serde_yaml::from_str(
            r#"
roles:
  platform_lead:
    kind: integration_owner
  backend:
    kind: specialist
    assignment:
      owns: [Rust core]
"#,
        )
        .unwrap();
        ReadToolContext {
            tracker: Arc::new(tracker),
            workflow,
            instruction_packs: InstructionPackBundle::default(),
            current_role: "platform_lead".into(),
        }
    }

    #[tokio::test]
    async fn read_handlers_return_fixture_shapes() {
        let tracker = MockTracker::new();
        let issue = Issue::minimal("id-1", "ENG-1", "Title", "Todo");
        tracker.set_state_lookup(issue.clone());
        tracker.set_comments(
            "ENG-1",
            vec![IssueComment {
                id: "c1".into(),
                author: "alice".into(),
                body: "hello".into(),
                created_at: Some("2026-05-10T00:00:00Z".into()),
            }],
        );
        tracker.set_related(
            "ENG-1",
            RelatedIssues {
                parent: None,
                children: vec![Issue::minimal("id-2", "ENG-2", "Child", "Todo")],
                blockers: Vec::new(),
            },
        );
        let ctx = ctx(tracker);

        let issue_result = GetIssueDetailsHandler::new(ctx.clone())
            .call(json!({ "issue": "ENG-1" }))
            .await
            .unwrap();
        assert_eq!(issue_result.content["issue"]["identifier"], "ENG-1");

        let comments = ListCommentsHandler::new(ctx.clone())
            .call(json!({ "issue": "ENG-1" }))
            .await
            .unwrap();
        assert_eq!(comments.content["comments"][0]["author"], "alice");

        let related = GetRelatedIssuesHandler::new(ctx.clone())
            .call(json!({ "issue": "ENG-1" }))
            .await
            .unwrap();
        assert_eq!(
            related.content["related"]["children"][0]["identifier"],
            "ENG-2"
        );

        let catalog = ListAvailableRolesHandler::new(ctx)
            .call(json!({}))
            .await
            .unwrap();
        assert!(
            catalog.content["catalog"]
                .as_str()
                .unwrap()
                .contains("backend")
        );
    }

    #[tokio::test]
    async fn unsupported_read_handler_maps_to_capability_error() {
        #[derive(Debug)]
        struct UnsupportedTracker;

        #[async_trait]
        impl TrackerRead for UnsupportedTracker {
            async fn fetch_active(
                &self,
            ) -> symphony_core::tracker_trait::TrackerResult<Vec<Issue>> {
                Ok(Vec::new())
            }

            async fn fetch_state(
                &self,
                _ids: &[symphony_core::tracker::IssueId],
            ) -> symphony_core::tracker_trait::TrackerResult<Vec<Issue>> {
                Ok(Vec::new())
            }

            async fn fetch_terminal_recent(
                &self,
                _terminal_states: &[symphony_core::tracker::IssueState],
            ) -> symphony_core::tracker_trait::TrackerResult<Vec<Issue>> {
                Ok(Vec::new())
            }
        }

        let ctx = ReadToolContext {
            tracker: Arc::new(UnsupportedTracker),
            workflow: WorkflowConfig::default(),
            instruction_packs: InstructionPackBundle::default(),
            current_role: "platform_lead".into(),
        };
        let err = ListCommentsHandler::new(ctx)
            .call(json!({ "issue": "ENG-1" }))
            .await
            .unwrap_err();
        assert!(matches!(err, ToolError::CapabilityUnsupported { .. }));
    }
}
