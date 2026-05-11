//! MCP write tools for follow-up issue requests.

use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use symphony_core::{
    FollowupPolicy, FollowupRequestInput, FollowupRouteDecision, HandoffFollowupRequest,
    RoleAuthority, RoleName, RunRef, WorkItemId as CoreWorkItemId, derive_followups, route_followups,
};
use symphony_state::StateDb;
use symphony_state::events::NewEvent;
use symphony_state::followups::NewFollowup;
use symphony_state::repository::{RunId, WorkItemId};

use crate::handler::{ToolError, ToolHandler, ToolResult};

/// MCP payload for one `file_followup` request.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct FileFollowupPayload {
    /// Agent follow-up intent.
    #[serde(flatten)]
    pub request: HandoffFollowupRequest,
    /// Kernel-required bounded scope.
    pub scope: String,
    /// Follow-up acceptance criteria.
    #[serde(default)]
    pub acceptance_criteria: Vec<String>,
}

/// One persisted or proposed follow-up result.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SubmittedFollowup {
    /// Resolved policy route.
    pub route: FollowupRouteDecision,
    /// Durable follow-up row id.
    pub followup_id: i64,
}

/// Persistence boundary for `file_followup`.
#[async_trait]
pub trait FollowupSink: Send + Sync {
    /// Persist or propose the derived follow-up.
    async fn submit(
        &self,
        input: FollowupRequestInput,
        route: FollowupRouteDecision,
    ) -> Result<SubmittedFollowup, ToolError>;
}

/// State-backed follow-up sink.
pub struct StateFollowupSink {
    db: Mutex<StateDb>,
    source_work_item: WorkItemId,
    run_id: RunId,
    role: String,
    now: String,
}

impl StateFollowupSink {
    /// Construct a state-backed sink.
    pub fn new(
        db: StateDb,
        source_work_item: WorkItemId,
        run_id: RunId,
        role: impl Into<String>,
        now: impl Into<String>,
    ) -> Self {
        Self {
            db: Mutex::new(db),
            source_work_item,
            run_id,
            role: role.into(),
            now: now.into(),
        }
    }

    /// Read the underlying database while holding the sink lock.
    pub fn with_db<R>(&self, f: impl FnOnce(&StateDb) -> R) -> R {
        let db = self.db.lock().expect("followup sink lock");
        f(&db)
    }
}

#[async_trait]
impl FollowupSink for StateFollowupSink {
    async fn submit(
        &self,
        input: FollowupRequestInput,
        route: FollowupRouteDecision,
    ) -> Result<SubmittedFollowup, ToolError> {
        let mut db = self.db.lock().expect("followup sink lock");
        let acceptance_criteria = serde_json::to_string(&input.acceptance_criteria)
            .map_err(|err| ToolError::Internal(err.to_string()))?;
        let result = db
            .transaction(|tx| match &route {
                FollowupRouteDecision::CreateDirectly => {
                    let followup = tx.create_followup(NewFollowup {
                        source_work_item_id: self.source_work_item,
                        title: &input.title,
                        reason: &input.reason,
                        scope: &input.scope,
                        acceptance_criteria: &acceptance_criteria,
                        blocking: input.blocking,
                        policy: FollowupPolicy::CreateDirectly.as_str(),
                        origin_kind: "run",
                        origin_run_id: Some(self.run_id),
                        origin_role: Some(self.role.as_str()),
                        origin_agent_profile: None,
                        origin_human_actor: None,
                        status: "approved",
                        created_issue_id: None,
                        approval_role: None,
                        approval_note: None,
                        now: &self.now,
                    })?;
                    tx.append_event(NewEvent {
                        event_type: "followup.approved",
                        work_item_id: Some(self.source_work_item),
                        run_id: Some(self.run_id),
                        payload: &json!({
                            "title": input.title,
                            "followup_id": followup.id.get(),
                            "route": route.kind(),
                        })
                        .to_string(),
                        now: &self.now,
                    })?;
                    Ok(followup.id.get())
                }
                FollowupRouteDecision::RouteToApprovalQueue { approval_role } => {
                    let followup = tx.create_followup(NewFollowup {
                        source_work_item_id: self.source_work_item,
                        title: &input.title,
                        reason: &input.reason,
                        scope: &input.scope,
                        acceptance_criteria: &acceptance_criteria,
                        blocking: input.blocking,
                        policy: FollowupPolicy::ProposeForApproval.as_str(),
                        origin_kind: "run",
                        origin_run_id: Some(self.run_id),
                        origin_role: Some(self.role.as_str()),
                        origin_agent_profile: None,
                        origin_human_actor: None,
                        status: "proposed",
                        created_issue_id: None,
                        approval_role: None,
                        approval_note: None,
                        now: &self.now,
                    })?;
                    tx.append_event(NewEvent {
                        event_type: "followup.proposed",
                        work_item_id: Some(self.source_work_item),
                        run_id: Some(self.run_id),
                        payload: &json!({
                            "title": input.title,
                            "followup_id": followup.id.get(),
                            "scope": input.scope,
                            "approval_role": approval_role.as_str(),
                            "route": route.kind(),
                        })
                        .to_string(),
                        now: &self.now,
                    })?;
                    Ok(followup.id.get())
                }
            })
            .map_err(|err| ToolError::Internal(err.to_string()))?;
        Ok(SubmittedFollowup {
            route,
            followup_id: result,
        })
    }
}

/// Context for `file_followup`.
#[derive(Clone)]
pub struct FileFollowupContext {
    /// Persistence boundary.
    pub sink: Arc<dyn FollowupSink>,
    /// Source work item for the running agent.
    pub source_work_item: CoreWorkItemId,
    /// Run id for origin/audit.
    pub run_id: RunRef,
    /// Reporting role.
    pub role: RoleName,
    /// Resolved role authority.
    pub role_authority: RoleAuthority,
    /// Workflow follow-up default policy.
    pub default_policy: FollowupPolicy,
    /// Workflow acceptance-criteria requirement.
    pub require_acceptance_criteria: bool,
    /// Workflow approval role, required for approval-routed requests.
    pub approval_role: Option<RoleName>,
}

/// `file_followup`
pub struct FileFollowupHandler {
    ctx: FileFollowupContext,
}

impl FileFollowupHandler {
    /// Construct a handler from context.
    pub fn new(ctx: FileFollowupContext) -> Self {
        Self { ctx }
    }
}

#[async_trait]
impl ToolHandler for FileFollowupHandler {
    fn name(&self) -> &str {
        "file_followup"
    }

    async fn call(&self, input: Value) -> Result<ToolResult, ToolError> {
        let payload: FileFollowupPayload =
            serde_json::from_value(input).map_err(|err| ToolError::InvalidPayload {
                field: "$".into(),
                message: err.to_string(),
            })?;
        let followup_input = FollowupRequestInput::from_handoff(
            &payload.request,
            payload.scope,
            payload.acceptance_criteria,
        );
        let specs = derive_followups(
            self.ctx.source_work_item,
            self.ctx.run_id,
            &self.ctx.role,
            &self.ctx.role_authority,
            self.ctx.default_policy,
            self.ctx.require_acceptance_criteria,
            std::slice::from_ref(&followup_input),
        )
        .map_err(map_followup_error)?;
        let routes = route_followups(&specs, self.ctx.approval_role.as_ref()).map_err(|err| {
            ToolError::PolicyViolation {
                gate: "followup_routing".into(),
                message: err.to_string(),
            }
        })?;
        let submitted = self
            .ctx
            .sink
            .submit(followup_input, routes[0].clone())
            .await?;
        Ok(ToolResult::json(json!({
            "route": submitted.route.kind(),
            "requires_approval": submitted.route.requires_approval(),
            "followup_id": submitted.followup_id,
            "work_item_id": Value::Null,
        })))
    }
}

fn map_followup_error(err: symphony_core::FollowupRequestError) -> ToolError {
    let field = match err {
        symphony_core::FollowupRequestError::EmptyTitle { .. } => "title",
        symphony_core::FollowupRequestError::EmptyReason { .. } => "summary",
        symphony_core::FollowupRequestError::EmptyScope { .. } => "scope",
        symphony_core::FollowupRequestError::EmptyAcceptanceCriterion { .. } => {
            "acceptance_criteria"
        }
        symphony_core::FollowupRequestError::AcceptanceCriteriaRequired { .. } => {
            "acceptance_criteria"
        }
        symphony_core::FollowupRequestError::RoleLacksFollowupAuthority { .. } => {
            return ToolError::CapabilityRefused {
                role: "unknown".into(),
                tool: "file_followup".into(),
            };
        }
    };
    ToolError::InvalidPayload {
        field: field.into(),
        message: err.to_string(),
    }
}

#[cfg(test)]
mod tests {
    use serde_json::json;
    use symphony_core::{RoleAuthority, RoleKind};
    use symphony_state::events::EventRepository;
    use symphony_state::followups::FollowupRepository;
    use symphony_state::migrations::migrations;
    use symphony_state::repository::{NewRun, NewWorkItem, RunRepository, WorkItemRepository};

    use super::*;

    const NOW: &str = "2026-05-10T02:00:00Z";

    fn open() -> (StateDb, WorkItemId, RunId) {
        let mut db = StateDb::open_in_memory().unwrap();
        db.migrate(migrations()).unwrap();
        let item = db
            .create_work_item(NewWorkItem {
                tracker_id: "github",
                identifier: "OWNER/REPO#1",
                parent_id: None,
                title: "source",
                status_class: "running",
                tracker_status: "open",
                assigned_role: Some("backend"),
                assigned_agent: Some("claude"),
                priority: None,
                workspace_policy: None,
                branch_policy: None,
                now: NOW,
            })
            .unwrap();
        let run = db
            .create_run(NewRun {
                work_item_id: item.id,
                role: "backend",
                agent: "claude",
                status: "running",
                workspace_claim_id: None,
                now: NOW,
            })
            .unwrap();
        (db, item.id, run.id)
    }

    fn handler(
        policy: FollowupPolicy,
        approval_role: Option<RoleName>,
    ) -> (FileFollowupHandler, Arc<StateFollowupSink>, WorkItemId) {
        let (db, source, run) = open();
        let sink = Arc::new(StateFollowupSink::new(db, source, run, "backend", NOW));
        (
            FileFollowupHandler::new(FileFollowupContext {
                sink: sink.clone(),
                source_work_item: CoreWorkItemId::new(source.0),
                run_id: RunRef::new(run.0),
                role: RoleName::new("backend"),
                role_authority: RoleAuthority::defaults_for(RoleKind::Specialist),
                default_policy: policy,
                require_acceptance_criteria: true,
                approval_role,
            }),
            sink,
            source,
        )
    }

    fn payload(propose_only: bool) -> Value {
        json!({
            "title": "Add retry coverage",
            "summary": "API retries need a regression test",
            "blocking": false,
            "propose_only": propose_only,
            "scope": "tests for retry behaviour only",
            "acceptance_criteria": ["retry branch has coverage"]
        })
    }

    #[tokio::test]
    async fn file_followup_persists_approved_followup_when_policy_allows() {
        let (handler, sink, source) = handler(FollowupPolicy::CreateDirectly, None);
        let result = handler.call(payload(false)).await.unwrap();

        assert_eq!(result.content["route"], "create_directly");
        assert!(result.content["followup_id"].as_i64().is_some());
        assert!(result.content["work_item_id"].is_null());
        sink.with_db(|db| {
            let events = db.list_events_for_work_item(source).unwrap();
            assert_eq!(events[0].event_type, "followup.approved");
            let created = db
                .get_followup(symphony_core::FollowupId::new(
                    result.content["followup_id"].as_i64().unwrap(),
                ))
                .unwrap()
                .unwrap();
            assert_eq!(created.status, "approved");
            assert_eq!(created.title, "Add retry coverage");
        });
    }

    #[tokio::test]
    async fn file_followup_routes_to_approval_when_policy_requires() {
        let (handler, sink, source) = handler(
            FollowupPolicy::ProposeForApproval,
            Some(RoleName::new("platform_lead")),
        );
        let result = handler.call(payload(false)).await.unwrap();

        assert_eq!(result.content["route"], "route_to_approval_queue");
        assert_eq!(result.content["requires_approval"], true);
        assert!(result.content["work_item_id"].is_null());
        assert!(result.content["followup_id"].as_i64().is_some());
        sink.with_db(|db| {
            let events = db.list_events_for_work_item(source).unwrap();
            assert_eq!(events[0].event_type, "followup.proposed");
            let created = db
                .get_followup(symphony_core::FollowupId::new(
                    result.content["followup_id"].as_i64().unwrap(),
                ))
                .unwrap()
                .unwrap();
            assert_eq!(created.status, "proposed");
        });
    }
}
