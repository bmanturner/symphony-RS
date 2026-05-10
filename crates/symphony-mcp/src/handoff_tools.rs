//! MCP write tools for structured run handoffs.

use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use serde_json::{Value, json};
use symphony_core::{Handoff, HandoffError, ReadyForConsequence};
use symphony_state::StateDb;
use symphony_state::events::NewEvent;
use symphony_state::handoffs::{HandoffRecord, NewHandoff};
use symphony_state::repository::{RunId, WorkItemId};

use crate::handler::{ToolError, ToolHandler, ToolResult};

/// Persisted result of a submitted handoff.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SubmittedHandoff {
    /// Durable handoff row.
    pub record: HandoffRecord,
    /// Kernel-side consequence computed from `ready_for`.
    pub consequence: ReadyForConsequence,
    /// True when this call replayed an already persisted handoff for
    /// the run.
    pub replayed: bool,
}

/// Persistence boundary for `submit_handoff`.
#[async_trait]
pub trait HandoffSink: Send + Sync {
    /// Persist a validated handoff or return an idempotent replay.
    async fn submit(&self, handoff: Handoff) -> Result<SubmittedHandoff, ToolError>;

    /// Whether this run already submitted a handoff through MCP. Phase
    /// 11's text-extraction fallback uses this to avoid double counting.
    fn has_submitted(&self) -> bool;
}

/// State-backed handoff sink.
pub struct StateHandoffSink {
    db: Mutex<StateDb>,
    run_id: RunId,
    work_item_id: WorkItemId,
    now: String,
}

impl StateHandoffSink {
    /// Construct a state-backed sink for one run.
    pub fn new(
        db: StateDb,
        run_id: RunId,
        work_item_id: WorkItemId,
        now: impl Into<String>,
    ) -> Self {
        Self {
            db: Mutex::new(db),
            run_id,
            work_item_id,
            now: now.into(),
        }
    }

    /// Read the underlying database while holding the sink lock.
    pub fn with_db<R>(&self, f: impl FnOnce(&StateDb) -> R) -> R {
        let db = self.db.lock().expect("handoff sink lock");
        f(&db)
    }
}

#[async_trait]
impl HandoffSink for StateHandoffSink {
    async fn submit(&self, handoff: Handoff) -> Result<SubmittedHandoff, ToolError> {
        use symphony_state::handoffs::HandoffRepository;

        let mut db = self.db.lock().expect("handoff sink lock");
        if let Some(existing) = db
            .list_handoffs_for_work_item(self.work_item_id)
            .map_err(|err| ToolError::Internal(err.to_string()))?
            .into_iter()
            .find(|record| record.run_id == self.run_id)
        {
            let consequence = parse_consequence(&existing.ready_for);
            return Ok(SubmittedHandoff {
                record: existing,
                consequence,
                replayed: true,
            });
        }

        let consequence = handoff.consequence();
        let changed_files = encode_json_array(&handoff.changed_files, "changed_files")?;
        let tests_run = encode_json_array(&handoff.tests_run, "tests_run")?;
        let evidence = encode_json_array(&handoff.verification_evidence, "verification_evidence")?;
        let known_risks = encode_json_array(&handoff.known_risks, "known_risks")?;
        let event_payload = serde_json::to_string(&json!({
            "ready_for": handoff.ready_for.as_str(),
            "consequence": consequence.as_str(),
            "summary": handoff.summary,
        }))
        .map_err(|err| ToolError::Internal(format!("encode handoff event: {err}")))?;
        let status = status_for_consequence(consequence);

        let record = db
            .transaction(|tx| {
                let record = tx.create_handoff(NewHandoff {
                    run_id: self.run_id,
                    work_item_id: self.work_item_id,
                    ready_for: handoff.ready_for.as_str(),
                    summary: &handoff.summary,
                    changed_files: changed_files.as_deref(),
                    tests_run: tests_run.as_deref(),
                    evidence: evidence.as_deref(),
                    known_risks: known_risks.as_deref(),
                    now: &self.now,
                })?;
                tx.update_run_status(self.run_id, "completed")?;
                tx.update_work_item_status(self.work_item_id, status, status, &self.now)?;
                tx.append_event(NewEvent {
                    event_type: "run.completed",
                    work_item_id: Some(self.work_item_id),
                    run_id: Some(self.run_id),
                    payload: &event_payload,
                    now: &self.now,
                })?;
                Ok(record)
            })
            .map_err(|err| ToolError::Internal(err.to_string()))?;

        Ok(SubmittedHandoff {
            record,
            consequence,
            replayed: false,
        })
    }

    fn has_submitted(&self) -> bool {
        use symphony_state::handoffs::HandoffRepository;

        self.db
            .lock()
            .expect("handoff sink lock")
            .list_handoffs_for_work_item(self.work_item_id)
            .map(|records| records.iter().any(|record| record.run_id == self.run_id))
            .unwrap_or(false)
    }
}

/// Context for `submit_handoff`.
#[derive(Clone)]
pub struct SubmitHandoffContext {
    /// Persistence boundary for this run.
    pub sink: Arc<dyn HandoffSink>,
}

/// `submit_handoff`
pub struct SubmitHandoffHandler {
    ctx: SubmitHandoffContext,
}

impl SubmitHandoffHandler {
    /// Construct a handler from context.
    pub fn new(ctx: SubmitHandoffContext) -> Self {
        Self { ctx }
    }
}

#[async_trait]
impl ToolHandler for SubmitHandoffHandler {
    fn name(&self) -> &str {
        "submit_handoff"
    }

    async fn call(&self, input: Value) -> Result<ToolResult, ToolError> {
        let handoff: Handoff =
            serde_json::from_value(input).map_err(|err| ToolError::InvalidPayload {
                field: "$".into(),
                message: err.to_string(),
            })?;
        handoff.validate().map_err(map_handoff_error)?;
        let submitted = self.ctx.sink.submit(handoff).await?;
        Ok(ToolResult::json(json!({
            "handoff_id": submitted.record.id.0,
            "ready_for": submitted.record.ready_for,
            "consequence": submitted.consequence.as_str(),
            "replayed": submitted.replayed,
        })))
    }
}

fn encode_json_array(values: &[String], field: &str) -> Result<Option<String>, ToolError> {
    if values.is_empty() {
        return Ok(None);
    }
    serde_json::to_string(values)
        .map(Some)
        .map_err(|err| ToolError::InvalidPayload {
            field: field.into(),
            message: err.to_string(),
        })
}

fn status_for_consequence(consequence: ReadyForConsequence) -> &'static str {
    match consequence {
        ReadyForConsequence::EnqueueIntegration => "integration",
        ReadyForConsequence::EnqueueQa => "qa",
        ReadyForConsequence::PauseAwaitingHumanReview => "human_review",
        ReadyForConsequence::HoldBlocked => "blocked",
        ReadyForConsequence::AttemptDone => "done",
    }
}

fn parse_consequence(ready_for: &str) -> ReadyForConsequence {
    match ready_for {
        "integration" => ReadyForConsequence::EnqueueIntegration,
        "qa" => ReadyForConsequence::EnqueueQa,
        "human_review" => ReadyForConsequence::PauseAwaitingHumanReview,
        "blocked" => ReadyForConsequence::HoldBlocked,
        "done" => ReadyForConsequence::AttemptDone,
        _ => ReadyForConsequence::AttemptDone,
    }
}

fn map_handoff_error(err: HandoffError) -> ToolError {
    let field = match err {
        HandoffError::EmptySummary => "summary",
        HandoffError::BlockedWithoutEvidence => "ready_for",
        HandoffError::NonBlockedBlockReason(_) => "block_reason",
        HandoffError::EmptyChangedFile => "changed_files",
        HandoffError::EmptyBlockerReason => "blockers_created[].reason",
        HandoffError::EmptyFollowupTitle => "followups_created_or_proposed[].title",
        HandoffError::EmptyBranchOrWorkspace => "branch_or_workspace",
        HandoffError::EmptyVerdictCriterion => "verdict_request.acceptance_trace[].criterion",
        HandoffError::VerdictMissingBlockers => "verdict_request.blockers_created",
        HandoffError::VerdictUnexpectedBlockers { .. } => "verdict_request.blockers_created",
        HandoffError::VerdictInvalidWaiver => "verdict_request.waiver_role",
        HandoffError::VerdictUnexpectedWaiver(_) => "verdict_request.waiver_role",
        HandoffError::VerdictPassWithUnpassedCriterion => "verdict_request.acceptance_trace",
    };
    ToolError::InvalidPayload {
        field: field.into(),
        message: err.to_string(),
    }
}

#[cfg(test)]
mod tests {
    use serde_json::json;
    use symphony_state::events::EventRepository;
    use symphony_state::handoffs::HandoffRepository;
    use symphony_state::migrations::migrations;
    use symphony_state::repository::{NewRun, NewWorkItem, RunRepository, WorkItemRepository};

    use super::*;

    const NOW: &str = "2026-05-10T00:00:00Z";

    fn open() -> (StateDb, WorkItemId, RunId) {
        let mut db = StateDb::open_in_memory().unwrap();
        db.migrate(migrations()).unwrap();
        let item = db
            .create_work_item(NewWorkItem {
                tracker_id: "github",
                identifier: "OWNER/REPO#1",
                parent_id: None,
                title: "child",
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

    fn valid_payload() -> Value {
        json!({
            "summary": "Implemented the API",
            "changed_files": ["src/api.rs"],
            "tests_run": ["cargo test"],
            "verification_evidence": ["test log"],
            "known_risks": ["none"],
            "branch_or_workspace": { "branch": "feat/api" },
            "ready_for": "integration"
        })
    }

    fn handler() -> (
        SubmitHandoffHandler,
        Arc<StateHandoffSink>,
        WorkItemId,
        RunId,
    ) {
        let (db, work_item_id, run_id) = open();
        let sink = Arc::new(StateHandoffSink::new(db, run_id, work_item_id, NOW));
        (
            SubmitHandoffHandler::new(SubmitHandoffContext { sink: sink.clone() }),
            sink,
            work_item_id,
            run_id,
        )
    }

    #[tokio::test]
    async fn submit_handoff_persists_handoff_event_and_queue_status() {
        let (handler, sink, work_item_id, run_id) = handler();
        let result = handler.call(valid_payload()).await.unwrap();

        assert_eq!(result.content["ready_for"], "integration");
        assert_eq!(result.content["consequence"], "enqueue_integration");
        assert_eq!(result.content["replayed"], false);
        assert!(sink.has_submitted());

        sink.with_db(|db| {
            let handoff = db
                .latest_handoff_for_work_item(work_item_id)
                .unwrap()
                .unwrap();
            assert_eq!(handoff.run_id, run_id);
            assert_eq!(handoff.summary, "Implemented the API");
            assert_eq!(handoff.changed_files.as_deref(), Some(r#"["src/api.rs"]"#));
            let run = db.get_run(run_id).unwrap().unwrap();
            assert_eq!(run.status, "completed");
            let item = db.get_work_item(work_item_id).unwrap().unwrap();
            assert_eq!(item.status_class, "integration");
            let events = db.list_events_for_work_item(work_item_id).unwrap();
            assert_eq!(events.len(), 1);
            assert_eq!(events[0].event_type, "run.completed");
        });
    }

    #[tokio::test]
    async fn submit_handoff_rejects_invalid_shape_with_field_path() {
        let (handler, _sink, _work_item_id, _run_id) = handler();
        let mut payload = valid_payload();
        payload["summary"] = json!("");
        let err = handler.call(payload).await.unwrap_err();
        assert!(matches!(
            err,
            ToolError::InvalidPayload { field, .. } if field == "summary"
        ));
    }

    #[tokio::test]
    async fn submit_handoff_replays_existing_handoff_for_same_run() {
        let (handler, sink, work_item_id, _run_id) = handler();
        let first = handler.call(valid_payload()).await.unwrap();
        let second = handler.call(valid_payload()).await.unwrap();

        assert_eq!(first.content["handoff_id"], second.content["handoff_id"]);
        assert_eq!(second.content["replayed"], true);
        sink.with_db(|db| {
            let handoffs = db.list_handoffs_for_work_item(work_item_id).unwrap();
            assert_eq!(handoffs.len(), 1);
            let events = db.list_events_for_work_item(work_item_id).unwrap();
            assert_eq!(events.len(), 1);
        });
    }
}
