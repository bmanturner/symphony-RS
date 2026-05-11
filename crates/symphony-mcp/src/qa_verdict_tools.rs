//! MCP write tools for QA verdicts.

use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use symphony_core::tracker::IssueId;
use symphony_core::tracker_trait::{
    AddBlockerRequest, AddBlockerResponse, TrackerError, TrackerMutations, TrackerRead,
};
use symphony_core::{
    AcceptanceCriterionTrace, BlockerId, HandoffBlockerRequest, QaEvidence, QaVerdict, RoleKind,
    RoleName, WorkItemStatusClass, validate_qa_waiver,
};
use symphony_state::StateDb;
use symphony_state::edges::{EdgeSource, EdgeType, NewWorkItemEdge, TrackerEdgeSyncSuccess};
use symphony_state::events::NewEvent;
use symphony_state::qa_verdicts::{NewQaVerdict, QaVerdictRecord};
use symphony_state::repository::{RunId, WorkItemId, WorkItemRepository};

use crate::handler::{ToolError, ToolHandler, ToolResult};

/// MCP payload for `record_qa_verdict`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RecordQaVerdictPayload {
    /// Typed verdict outcome.
    pub verdict: QaVerdict,
    /// Structured evidence for the verdict.
    #[serde(default)]
    pub evidence: QaEvidence,
    /// Acceptance-criterion trace rows.
    #[serde(default)]
    pub acceptance_trace: Vec<AcceptanceCriterionTrace>,
    /// QA blocker requests to file alongside a failing verdict.
    #[serde(default)]
    pub blockers_created: Vec<HandoffBlockerRequest>,
    /// Waiver authoring role, required only for waived verdicts.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub waiver_role: Option<RoleName>,
    /// Operator-facing verdict reason.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
}

/// Persisted result of a QA verdict submission.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SubmittedQaVerdict {
    /// Durable verdict row.
    pub record: QaVerdictRecord,
    /// QA blocker edge ids filed in the same transaction.
    pub blockers_created: Vec<BlockerId>,
    /// Work item status after routing.
    pub status_class: WorkItemStatusClass,
}

/// Persistence boundary for `record_qa_verdict`.
#[async_trait]
pub trait QaVerdictSink: Send + Sync {
    /// Persist a validated QA verdict payload.
    async fn submit(
        &self,
        payload: RecordQaVerdictPayload,
        blocker_ids: Vec<BlockerId>,
    ) -> Result<SubmittedQaVerdict, ToolError>;
}

/// State-backed QA verdict sink.
pub struct StateQaVerdictSink {
    db: Mutex<StateDb>,
    run_id: RunId,
    work_item_id: WorkItemId,
    role: RoleName,
    tracker: Arc<dyn QaVerdictTracker>,
    now: String,
}

pub trait QaVerdictTracker: TrackerRead + TrackerMutations {}

impl<T> QaVerdictTracker for T where T: TrackerRead + TrackerMutations {}

impl StateQaVerdictSink {
    /// Construct a state-backed sink for one QA run.
    pub fn new(
        db: StateDb,
        run_id: RunId,
        work_item_id: WorkItemId,
        role: RoleName,
        tracker: Arc<dyn QaVerdictTracker>,
        now: impl Into<String>,
    ) -> Self {
        Self {
            db: Mutex::new(db),
            run_id,
            work_item_id,
            role,
            tracker,
            now: now.into(),
        }
    }

    /// Read the underlying database while holding the sink lock.
    pub fn with_db<R>(&self, f: impl FnOnce(&StateDb) -> R) -> R {
        let db = self.db.lock().expect("qa verdict sink lock");
        f(&db)
    }
}

#[async_trait]
impl QaVerdictSink for StateQaVerdictSink {
    async fn submit(
        &self,
        payload: RecordQaVerdictPayload,
        _blocker_ids: Vec<BlockerId>,
    ) -> Result<SubmittedQaVerdict, ToolError> {
        let evidence = encode_json(&payload.evidence, "evidence")?;
        let acceptance_trace = encode_json_array(&payload.acceptance_trace, "acceptance_trace")?;
        let status_class = status_for_verdict(payload.verdict);
        let tracker_blockers = self.resolve_tracker_blockers(&payload).await?;

        let mut db = self.db.lock().expect("qa verdict sink lock");
        let submitted = db
            .transaction(|tx| {
                let mut blocker_ids = Vec::with_capacity(payload.blockers_created.len());
                for synced in &tracker_blockers {
                    let edge = tx.create_edge(NewWorkItemEdge {
                        parent_id: synced.blocking_work_item_id,
                        child_id: self.work_item_id,
                        edge_type: EdgeType::Blocks,
                        reason: Some(&synced.reason),
                        status: "open",
                        source: EdgeSource::Qa,
                        now: &self.now,
                    })?;
                    tx.record_tracker_edge_sync_success(TrackerEdgeSyncSuccess {
                        edge_id: edge.id,
                        tracker_edge_id: synced.tracker_edge_id.as_deref(),
                        attempted_at: &self.now,
                    })?;
                    blocker_ids.push(BlockerId::new(edge.id.0));
                }
                let blockers_created = encode_json_array(&blocker_ids, "blockers_created")
                    .map_err(|err| symphony_state::StateError::Invariant(err.to_string()))?;
                let record = tx.create_qa_verdict(NewQaVerdict {
                    work_item_id: self.work_item_id,
                    run_id: self.run_id,
                    role: self.role.as_str(),
                    verdict: payload.verdict.as_str(),
                    waiver_role: payload.waiver_role.as_ref().map(RoleName::as_str),
                    reason: payload.reason.as_deref(),
                    evidence: evidence.as_deref(),
                    acceptance_trace: acceptance_trace.as_deref(),
                    blockers_created: blockers_created.as_deref(),
                    now: &self.now,
                })?;
                tx.update_run_status(self.run_id, "completed")?;
                tx.update_work_item_status(
                    self.work_item_id,
                    status_class.as_str(),
                    status_class.as_str(),
                    &self.now,
                )?;
                tx.append_event(NewEvent {
                    event_type: "qa.verdict_recorded",
                    work_item_id: Some(self.work_item_id),
                    run_id: Some(self.run_id),
                    payload: &json!({
                        "qa_verdict_id": record.id,
                        "verdict": record.verdict,
                        "blockers_created": blocker_ids.iter().map(|id| id.get()).collect::<Vec<_>>(),
                        "status_class": status_class.as_str(),
                    })
                    .to_string(),
                    now: &self.now,
                })?;
                Ok((record, blocker_ids))
            })
            .map_err(|err| ToolError::Internal(err.to_string()))?;

        Ok(SubmittedQaVerdict {
            record: submitted.0,
            blockers_created: submitted.1,
            status_class,
        })
    }
}

#[derive(Debug, Clone)]
struct TrackerSyncedQaBlocker {
    blocking_work_item_id: WorkItemId,
    reason: String,
    tracker_edge_id: Option<String>,
}

impl StateQaVerdictSink {
    async fn resolve_tracker_blockers(
        &self,
        payload: &RecordQaVerdictPayload,
    ) -> Result<Vec<TrackerSyncedQaBlocker>, ToolError> {
        if payload.blockers_created.is_empty() {
            return Ok(Vec::new());
        }

        let (blocked_identifier, blockers) = {
            let db = self.db.lock().expect("qa verdict sink lock");
            let blocked = db
                .get_work_item(self.work_item_id)
                .map_err(|err| ToolError::Internal(err.to_string()))?
                .ok_or_else(|| ToolError::Internal("qa work item not found".into()))?;
            let mut blockers = Vec::with_capacity(payload.blockers_created.len());
            for request in &payload.blockers_created {
                let blocking_id = request
                    .blocking_id
                    .ok_or_else(|| ToolError::Internal("qa blocker missing blocking_id".into()))?;
                let blocker = db
                    .get_work_item(WorkItemId(blocking_id.get()))
                    .map_err(|err| ToolError::Internal(err.to_string()))?
                    .ok_or_else(|| {
                        ToolError::Internal(format!("qa blocker work item {blocking_id} not found"))
                    })?;
                blockers.push((blocker.id, blocker.identifier, request.reason.clone()));
            }
            (blocked.identifier, blockers)
        };

        let blocked_issue = self
            .tracker
            .get_issue(&blocked_identifier)
            .await
            .map_err(map_tracker_error)?;
        let mut synced = Vec::with_capacity(blockers.len());
        for (blocking_work_item_id, blocker_identifier, reason) in blockers {
            let blocker_issue = self
                .tracker
                .get_issue(&blocker_identifier)
                .await
                .map_err(map_tracker_error)?;
            let mirrored = self
                .tracker
                .add_blocker(AddBlockerRequest {
                    blocked: blocked_issue.id.clone(),
                    blocker: blocker_issue.id,
                    reason: Some(reason.clone()),
                })
                .await
                .map_err(map_tracker_error)?;
            synced.push(TrackerSyncedQaBlocker {
                blocking_work_item_id,
                reason,
                tracker_edge_id: mirrored.edge_id,
            });
        }
        Ok(synced)
    }
}

/// Context for `record_qa_verdict`.
#[derive(Clone)]
pub struct RecordQaVerdictContext {
    /// Persistence boundary for this run.
    pub sink: Arc<dyn QaVerdictSink>,
    /// Calling role label.
    pub role: RoleName,
    /// Calling role kind.
    pub role_kind: RoleKind,
    /// Configured QA waiver roles.
    pub waiver_roles: Vec<RoleName>,
}

/// `record_qa_verdict`
pub struct RecordQaVerdictHandler {
    ctx: RecordQaVerdictContext,
}

impl RecordQaVerdictHandler {
    /// Construct a handler from context.
    pub fn new(ctx: RecordQaVerdictContext) -> Self {
        Self { ctx }
    }
}

#[async_trait]
impl ToolHandler for RecordQaVerdictHandler {
    fn name(&self) -> &str {
        "record_qa_verdict"
    }

    async fn call(&self, input: Value) -> Result<ToolResult, ToolError> {
        if self.ctx.role_kind != RoleKind::QaGate {
            return Err(ToolError::CapabilityRefused {
                role: self.ctx.role.as_str().into(),
                tool: self.name().into(),
            });
        }

        let payload: RecordQaVerdictPayload =
            serde_json::from_value(input).map_err(|err| ToolError::InvalidPayload {
                field: "$".into(),
                message: err.to_string(),
            })?;
        validate_payload(&payload, &self.ctx.waiver_roles)?;
        let blocker_ids = synthetic_blocker_ids(payload.blockers_created.len());
        let submitted = self.ctx.sink.submit(payload, blocker_ids).await?;
        Ok(ToolResult::json(json!({
            "qa_verdict_id": submitted.record.id,
            "verdict": submitted.record.verdict,
            "blockers_created": submitted.blockers_created.iter().map(|id| id.get()).collect::<Vec<_>>(),
            "status_class": submitted.status_class.as_str(),
        })))
    }
}

fn validate_payload(
    payload: &RecordQaVerdictPayload,
    waiver_roles: &[RoleName],
) -> Result<(), ToolError> {
    for (index, trace) in payload.acceptance_trace.iter().enumerate() {
        if trace.criterion.trim().is_empty() {
            return invalid(
                format!("acceptance_trace[{index}].criterion"),
                "must not be empty",
            );
        }
        if payload.verdict == QaVerdict::Passed && !trace.status.is_passing() {
            return invalid(
                "acceptance_trace",
                "passed verdict requires all criteria to pass",
            );
        }
    }
    for (index, blocker) in payload.blockers_created.iter().enumerate() {
        if blocker.reason.trim().is_empty() {
            return invalid(
                format!("blockers_created[{index}].reason"),
                "must not be empty",
            );
        }
        if blocker.blocking_id.is_none() {
            return invalid(
                format!("blockers_created[{index}].blocking_id"),
                "is required",
            );
        }
    }

    match payload.verdict {
        QaVerdict::FailedWithBlockers if payload.blockers_created.is_empty() => {
            invalid("blockers_created", "failed_with_blockers requires blockers")
        }
        QaVerdict::Passed | QaVerdict::Waived if !payload.blockers_created.is_empty() => invalid(
            "blockers_created",
            "passing and waived verdicts must not file blockers",
        ),
        QaVerdict::Waived => {
            let role = payload
                .waiver_role
                .as_ref()
                .ok_or_else(|| ToolError::InvalidPayload {
                    field: "waiver_role".into(),
                    message: "is required for waived verdicts".into(),
                })?;
            let reason = payload.reason.as_deref().unwrap_or("");
            validate_qa_waiver(waiver_roles, role, reason).map_err(|err| {
                ToolError::PolicyViolation {
                    gate: "qa_waiver".into(),
                    message: err.to_string(),
                }
            })
        }
        verdict if payload.waiver_role.is_some() => Err(ToolError::InvalidPayload {
            field: "waiver_role".into(),
            message: format!("{verdict} verdict must not carry waiver metadata"),
        }),
        _ => Ok(()),
    }
}

fn invalid(field: impl Into<String>, message: impl Into<String>) -> Result<(), ToolError> {
    Err(ToolError::InvalidPayload {
        field: field.into(),
        message: message.into(),
    })
}

fn encode_json<T: Serialize>(value: &T, field: &str) -> Result<Option<String>, ToolError> {
    serde_json::to_string(value)
        .map(Some)
        .map_err(|err| ToolError::InvalidPayload {
            field: field.into(),
            message: err.to_string(),
        })
}

fn encode_json_array<T: Serialize>(values: &[T], field: &str) -> Result<Option<String>, ToolError> {
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

fn synthetic_blocker_ids(count: usize) -> Vec<BlockerId> {
    (0..count)
        .map(|index| BlockerId::new(index as i64 + 1))
        .collect()
}

fn status_for_verdict(verdict: QaVerdict) -> WorkItemStatusClass {
    match verdict {
        QaVerdict::Passed | QaVerdict::Waived => WorkItemStatusClass::Done,
        QaVerdict::FailedWithBlockers | QaVerdict::FailedNeedsRework => WorkItemStatusClass::Rework,
        QaVerdict::Inconclusive => WorkItemStatusClass::Qa,
    }
}

fn map_tracker_error(err: TrackerError) -> ToolError {
    ToolError::Internal(err.to_string())
}

#[cfg(test)]
mod tests {
    use std::sync::Mutex as StdMutex;

    use async_trait::async_trait;
    use serde_json::json;
    use symphony_core::tracker::{Issue, IssueComment, IssueState, RelatedIssues};
    use symphony_core::tracker_trait::{
        AddCommentRequest, AddCommentResponse, AttachArtifactRequest, AttachArtifactResponse,
        LinkParentChildRequest, LinkParentChildResponse, TrackerCapabilities, UpdateIssueRequest,
        UpdateIssueResponse,
    };
    use symphony_state::edges::WorkItemEdgeRepository;
    use symphony_state::events::EventRepository;
    use symphony_state::migrations::migrations;
    use symphony_state::qa_verdicts::QaVerdictRepository;
    use symphony_state::repository::{NewRun, NewWorkItem, RunRepository, WorkItemRepository};

    use super::*;

    const NOW: &str = "2026-05-10T01:00:00Z";

    #[derive(Debug, Clone, PartialEq, Eq)]
    struct RecordedBlockerCall {
        blocked: String,
        blocker: String,
        reason: Option<String>,
    }

    #[derive(Default)]
    struct RecordingTracker {
        blocker_calls: StdMutex<Vec<RecordedBlockerCall>>,
    }

    impl RecordingTracker {
        fn blocker_calls(&self) -> Vec<RecordedBlockerCall> {
            self.blocker_calls.lock().expect("tracker calls").clone()
        }
    }

    #[async_trait]
    impl TrackerRead for RecordingTracker {
        async fn fetch_active(&self) -> Result<Vec<Issue>, TrackerError> {
            Ok(Vec::new())
        }

        async fn fetch_state(&self, _ids: &[IssueId]) -> Result<Vec<Issue>, TrackerError> {
            Ok(Vec::new())
        }

        async fn fetch_terminal_recent(
            &self,
            _terminal_states: &[IssueState],
        ) -> Result<Vec<Issue>, TrackerError> {
            Ok(Vec::new())
        }

        async fn get_issue(&self, id_or_identifier: &str) -> Result<Issue, TrackerError> {
            Ok(Issue::minimal(
                id_or_identifier,
                id_or_identifier,
                format!("title for {id_or_identifier}"),
                "Todo",
            ))
        }

        async fn list_comments(
            &self,
            _id_or_identifier: &str,
        ) -> Result<Vec<IssueComment>, TrackerError> {
            Ok(Vec::new())
        }

        async fn get_related(
            &self,
            _id_or_identifier: &str,
        ) -> Result<RelatedIssues, TrackerError> {
            Ok(RelatedIssues::default())
        }

        fn capabilities(&self) -> TrackerCapabilities {
            TrackerCapabilities::FULL
        }
    }

    #[async_trait]
    impl TrackerMutations for RecordingTracker {
        async fn create_issue(
            &self,
            _request: symphony_core::tracker_trait::CreateIssueRequest,
        ) -> Result<symphony_core::tracker_trait::CreateIssueResponse, TrackerError> {
            unreachable!("qa verdict sink never creates issues")
        }

        async fn update_issue(
            &self,
            _request: UpdateIssueRequest,
        ) -> Result<UpdateIssueResponse, TrackerError> {
            unreachable!("qa verdict sink does not update issues in this phase slice")
        }

        async fn add_comment(
            &self,
            _request: AddCommentRequest,
        ) -> Result<AddCommentResponse, TrackerError> {
            unreachable!("qa verdict sink never adds comments")
        }

        async fn add_blocker(
            &self,
            request: AddBlockerRequest,
        ) -> Result<AddBlockerResponse, TrackerError> {
            self.blocker_calls
                .lock()
                .expect("tracker calls")
                .push(RecordedBlockerCall {
                    blocked: request.blocked.to_string(),
                    blocker: request.blocker.to_string(),
                    reason: request.reason.clone(),
                });
            Ok(AddBlockerResponse {
                edge_id: Some(format!("edge:{}->{}", request.blocker, request.blocked)),
            })
        }

        async fn link_parent_child(
            &self,
            _request: LinkParentChildRequest,
        ) -> Result<LinkParentChildResponse, TrackerError> {
            unreachable!("qa verdict sink never links parent/child")
        }

        async fn attach_artifact(
            &self,
            _request: AttachArtifactRequest,
        ) -> Result<AttachArtifactResponse, TrackerError> {
            unreachable!("qa verdict sink never attaches artifacts")
        }
    }

    fn open() -> (StateDb, WorkItemId, RunId, WorkItemId) {
        let mut db = StateDb::open_in_memory().unwrap();
        db.migrate(migrations()).unwrap();
        let item = db
            .create_work_item(NewWorkItem {
                tracker_id: "github",
                identifier: "OWNER/REPO#1",
                parent_id: None,
                title: "feature",
                status_class: "qa",
                tracker_status: "qa",
                assigned_role: Some("qa"),
                assigned_agent: Some("claude"),
                priority: None,
                workspace_policy: None,
                branch_policy: None,
                now: NOW,
            })
            .unwrap();
        let blocker = db
            .create_work_item(NewWorkItem {
                tracker_id: "github",
                identifier: "OWNER/REPO#2",
                parent_id: None,
                title: "fix failing test",
                status_class: "ready",
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
                role: "qa",
                agent: "claude",
                status: "running",
                workspace_claim_id: None,
                now: NOW,
            })
            .unwrap();
        (db, item.id, run.id, blocker.id)
    }

    fn handler() -> (
        RecordQaVerdictHandler,
        Arc<StateQaVerdictSink>,
        Arc<RecordingTracker>,
        WorkItemId,
        WorkItemId,
    ) {
        let (db, work_item_id, run_id, blocker_id) = open();
        let tracker = Arc::new(RecordingTracker::default());
        let sink = Arc::new(StateQaVerdictSink::new(
            db,
            run_id,
            work_item_id,
            RoleName::new("qa"),
            tracker.clone(),
            NOW,
        ));
        (
            RecordQaVerdictHandler::new(RecordQaVerdictContext {
                sink: sink.clone(),
                role: RoleName::new("qa"),
                role_kind: RoleKind::QaGate,
                waiver_roles: vec![RoleName::new("platform_lead")],
            }),
            sink,
            tracker,
            work_item_id,
            blocker_id,
        )
    }

    fn passing_trace() -> Value {
        json!([{
            "criterion": "tests pass",
            "status": "verified",
            "evidence": ["cargo test"]
        }])
    }

    #[tokio::test]
    async fn record_qa_verdict_persists_pass() {
        let (handler, sink, _tracker, work_item_id, _blocker_id) = handler();
        let result = handler
            .call(json!({
                "verdict": "passed",
                "evidence": { "tests_run": ["cargo test"] },
                "acceptance_trace": passing_trace()
            }))
            .await
            .unwrap();

        assert_eq!(result.content["verdict"], "passed");
        assert_eq!(result.content["status_class"], "done");
        sink.with_db(|db| {
            let rows = db.list_qa_verdicts_for_work_item(work_item_id).unwrap();
            assert_eq!(rows.len(), 1);
            assert_eq!(rows[0].verdict, "passed");
            assert_eq!(
                db.get_work_item(work_item_id)
                    .unwrap()
                    .unwrap()
                    .status_class,
                "done"
            );
            assert_eq!(db.list_events_for_work_item(work_item_id).unwrap().len(), 1);
        });
    }

    #[tokio::test]
    async fn record_qa_verdict_files_blockers_for_failure() {
        let (handler, sink, tracker, work_item_id, blocker_id) = handler();
        let result = handler
            .call(json!({
                "verdict": "failed_with_blockers",
                "evidence": { "tests_run": ["cargo test"] },
                "blockers_created": [{
                    "blocking_id": blocker_id.0,
                    "reason": "regression",
                    "severity": "high"
                }],
                "reason": "QA found a regression"
            }))
            .await
            .unwrap();

        assert_eq!(result.content["status_class"], "rework");
        assert_eq!(
            result.content["blockers_created"].as_array().unwrap().len(),
            1
        );
        assert_eq!(
            tracker.blocker_calls(),
            vec![RecordedBlockerCall {
                blocked: "OWNER/REPO#1".into(),
                blocker: "OWNER/REPO#2".into(),
                reason: Some("regression".into()),
            }]
        );
        sink.with_db(|db| {
            let rows = db.list_qa_verdicts_for_work_item(work_item_id).unwrap();
            assert_eq!(rows[0].verdict, "failed_with_blockers");
            assert!(
                rows[0]
                    .blockers_created
                    .as_deref()
                    .unwrap()
                    .starts_with('[')
            );
            let blockers = db
                .list_incoming(work_item_id, EdgeType::Blocks)
                .expect("incoming blockers");
            assert_eq!(blockers.len(), 1);
            assert_eq!(
                blockers[0].tracker_edge_id.as_deref(),
                Some("edge:OWNER/REPO#2->OWNER/REPO#1")
            );
            assert_eq!(blockers[0].tracker_sync_status.as_deref(), Some("synced"));
        });
    }

    #[tokio::test]
    async fn record_qa_verdict_keeps_inconclusive_in_qa() {
        let (handler, _sink, _tracker, _work_item_id, _blocker_id) = handler();
        let result = handler
            .call(json!({
                "verdict": "inconclusive",
                "evidence": { "notes": ["environment unavailable"] },
                "reason": "needs another run"
            }))
            .await
            .unwrap();
        assert_eq!(result.content["status_class"], "qa");
    }

    #[tokio::test]
    async fn record_qa_verdict_accepts_configured_waiver_role() {
        let (handler, _sink, _tracker, _work_item_id, _blocker_id) = handler();
        let result = handler
            .call(json!({
                "verdict": "waived",
                "evidence": { "notes": ["accepted risk"] },
                "waiver_role": "platform_lead",
                "reason": "accepted release risk"
            }))
            .await
            .unwrap();
        assert_eq!(result.content["status_class"], "done");
    }

    #[tokio::test]
    async fn record_qa_verdict_rejects_unconfigured_waiver_role() {
        let (handler, _sink, _tracker, _work_item_id, _blocker_id) = handler();
        let err = handler
            .call(json!({
                "verdict": "waived",
                "evidence": { "notes": ["accepted risk"] },
                "waiver_role": "backend",
                "reason": "accepted release risk"
            }))
            .await
            .unwrap_err();
        assert!(matches!(
            err,
            ToolError::PolicyViolation { gate, .. } if gate == "qa_waiver"
        ));
    }

    #[tokio::test]
    async fn record_qa_verdict_refuses_wrong_role() {
        let (_handler, sink, _tracker, _work_item_id, _blocker_id) = handler();
        let handler = RecordQaVerdictHandler::new(RecordQaVerdictContext {
            sink,
            role: RoleName::new("backend"),
            role_kind: RoleKind::Specialist,
            waiver_roles: Vec::new(),
        });
        let err = handler
            .call(json!({ "verdict": "passed" }))
            .await
            .unwrap_err();
        assert!(matches!(err, ToolError::CapabilityRefused { .. }));
    }
}
