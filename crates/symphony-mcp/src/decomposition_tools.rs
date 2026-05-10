//! MCP write tools for decomposition proposals.

use std::collections::{BTreeMap, BTreeSet};
use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use symphony_config::RoleCatalog;
use symphony_core::decomposition_applier::AppliedDecomposition;
use symphony_core::{
    ChildDraft, DecompositionDraft, DecompositionError, DecompositionId, DecompositionProposal,
    DecompositionRunner, RoleName, RunnerError, WorkItemId,
};
use symphony_state::StateDb;
use symphony_state::decomposition_apply::persist_applied_decomposition;

use crate::handler::{ToolError, ToolHandler, ToolResult};

/// MCP payload for `propose_decomposition`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DecompositionDraftWire {
    /// One-line decomposition strategy summary.
    pub summary: String,
    /// Proposed children in declaration order.
    pub children: Vec<ChildDraftWire>,
}

/// MCP payload for one decomposition child.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ChildDraftWire {
    /// Proposal-local key referenced by sibling `depends_on` entries.
    pub key: String,
    /// Tracker title.
    pub title: String,
    /// Bounded child scope.
    pub scope: String,
    /// Role name resolved against the live v4 role catalog.
    pub assigned_role: String,
    /// Optional long-form description.
    #[serde(default)]
    pub description: Option<String>,
    /// Child acceptance criteria.
    #[serde(default)]
    pub acceptance_criteria: Vec<String>,
    /// Proposal-local keys this child depends on.
    #[serde(default)]
    pub depends_on: Vec<String>,
    /// Whether this child blocks parent closeout.
    #[serde(default = "default_blocking")]
    pub blocking: bool,
    /// Optional branch hint.
    #[serde(default)]
    pub branch_hint: Option<String>,
}

/// Persist accepted decomposition proposals.
#[async_trait]
pub trait DecompositionProposalSink: Send + Sync {
    /// Persist the accepted proposal.
    async fn persist(
        &self,
        proposal: DecompositionProposal,
    ) -> Result<DecompositionProposal, ToolError>;
}

/// In-memory proposal sink used by tests and direct validation callers.
#[derive(Debug, Default)]
pub struct MemoryDecompositionProposalSink {
    proposals: Mutex<Vec<DecompositionProposal>>,
}

impl MemoryDecompositionProposalSink {
    /// Return a snapshot of persisted proposals.
    pub fn proposals(&self) -> Vec<DecompositionProposal> {
        self.proposals.lock().expect("proposal sink lock").clone()
    }
}

#[async_trait]
impl DecompositionProposalSink for MemoryDecompositionProposalSink {
    async fn persist(
        &self,
        proposal: DecompositionProposal,
    ) -> Result<DecompositionProposal, ToolError> {
        self.proposals
            .lock()
            .expect("proposal sink lock")
            .push(proposal.clone());
        Ok(proposal)
    }
}

/// State-backed sink that applies tracker-created decomposition children
/// through `symphony-state::decomposition_apply`.
pub struct StateDecompositionApplySink {
    db: Mutex<StateDb>,
    applied: AppliedDecomposition,
    tracker_id: String,
    now: String,
}

impl StateDecompositionApplySink {
    /// Construct a state-backed sink.
    pub fn new(
        db: StateDb,
        applied: AppliedDecomposition,
        tracker_id: impl Into<String>,
        now: impl Into<String>,
    ) -> Self {
        Self {
            db: Mutex::new(db),
            applied,
            tracker_id: tracker_id.into(),
            now: now.into(),
        }
    }

    /// Read the underlying database while holding the sink lock.
    pub fn with_db<R>(&self, f: impl FnOnce(&StateDb) -> R) -> R {
        let db = self.db.lock().expect("state apply sink lock");
        f(&db)
    }
}

#[async_trait]
impl DecompositionProposalSink for StateDecompositionApplySink {
    async fn persist(
        &self,
        mut proposal: DecompositionProposal,
    ) -> Result<DecompositionProposal, ToolError> {
        let mut db = self.db.lock().expect("state apply sink lock");
        let persisted = persist_applied_decomposition(
            &mut db,
            &proposal,
            &self.applied,
            &self.tracker_id,
            &self.now,
        )
        .map_err(|err| ToolError::Internal(err.to_string()))?;
        proposal
            .mark_applied(
                persisted.child_work_item_ids(),
                persisted.local_dependency_evidence(),
            )
            .map_err(|err| ToolError::Internal(err.to_string()))?;
        Ok(proposal)
    }
}

/// Context needed by `propose_decomposition`.
#[derive(Clone)]
pub struct ProposeDecompositionContext {
    /// Pure decomposition runner configured from workflow policy.
    pub runner: DecompositionRunner,
    /// Proposal persistence boundary.
    pub sink: Arc<dyn DecompositionProposalSink>,
    /// Live v4 role catalog.
    pub role_catalog: RoleCatalog,
    /// Parent work item id.
    pub parent: WorkItemId,
    /// Current parent decomposition depth.
    pub parent_depth: u32,
    /// Calling role.
    pub author_role: RoleName,
    /// Allocated decomposition id for this call.
    pub decomposition_id: DecompositionId,
}

/// `propose_decomposition`
pub struct ProposeDecompositionHandler {
    ctx: ProposeDecompositionContext,
}

impl ProposeDecompositionHandler {
    /// Construct a handler from context.
    pub fn new(ctx: ProposeDecompositionContext) -> Self {
        Self { ctx }
    }
}

#[async_trait]
impl ToolHandler for ProposeDecompositionHandler {
    fn name(&self) -> &str {
        "propose_decomposition"
    }

    async fn call(&self, input: Value) -> Result<ToolResult, ToolError> {
        let wire: DecompositionDraftWire =
            serde_json::from_value(input).map_err(|err| ToolError::InvalidPayload {
                field: "$".into(),
                message: err.to_string(),
            })?;
        validate_child_graph(&wire.children)?;
        let draft = lower_draft(
            wire,
            &self.ctx.role_catalog,
            self.ctx.parent,
            self.ctx.author_role.clone(),
        )?;
        let proposal = self
            .ctx
            .runner
            .accept(self.ctx.decomposition_id, draft, self.ctx.parent_depth)
            .map_err(map_runner_error)?;
        let proposal = self.ctx.sink.persist(proposal).await?;
        Ok(ToolResult::json(json!({
            "decomposition_id": proposal.id,
            "child_count": proposal.children.len(),
            "status": proposal.status.as_str(),
        })))
    }
}

fn default_blocking() -> bool {
    true
}

fn lower_draft(
    wire: DecompositionDraftWire,
    role_catalog: &RoleCatalog,
    parent: WorkItemId,
    author_role: RoleName,
) -> Result<DecompositionDraft, ToolError> {
    let catalog_roles: BTreeSet<&str> = role_catalog
        .entries
        .iter()
        .map(|entry| entry.name.as_str())
        .collect();
    let mut children = Vec::with_capacity(wire.children.len());
    for (idx, child) in wire.children.into_iter().enumerate() {
        if !catalog_roles.contains(child.assigned_role.as_str()) {
            return Err(ToolError::InvalidPayload {
                field: format!("children[{idx}].assigned_role"),
                message: format!("unknown child role `{}`", child.assigned_role),
            });
        }
        children.push(ChildDraft {
            key: child.key,
            title: child.title,
            scope: child.scope,
            assigned_role: Some(RoleName::new(child.assigned_role)),
            description: child.description,
            acceptance_criteria: child.acceptance_criteria,
            depends_on: child.depends_on,
            blocking: child.blocking,
            branch_hint: child.branch_hint,
        });
    }

    Ok(DecompositionDraft {
        parent,
        author_role,
        summary: wire.summary,
        children,
    })
}

fn validate_child_graph(children: &[ChildDraftWire]) -> Result<(), ToolError> {
    let mut key_to_index: BTreeMap<&str, usize> = BTreeMap::new();
    for (idx, child) in children.iter().enumerate() {
        if child.key.trim().is_empty() {
            return Err(ToolError::InvalidPayload {
                field: format!("children[{idx}].key"),
                message: "child key must not be empty".into(),
            });
        }
        if key_to_index.insert(child.key.as_str(), idx).is_some() {
            return Err(ToolError::InvalidPayload {
                field: format!("children[{idx}].key"),
                message: format!("duplicate child key `{}`", child.key),
            });
        }
    }

    for (idx, child) in children.iter().enumerate() {
        for (dep_idx, dependency) in child.depends_on.iter().enumerate() {
            if dependency == &child.key {
                return Err(ToolError::InvalidPayload {
                    field: format!("children[{idx}].depends_on[{dep_idx}]"),
                    message: "child cannot depend on itself".into(),
                });
            }
            if !key_to_index.contains_key(dependency.as_str()) {
                return Err(ToolError::InvalidPayload {
                    field: format!("children[{idx}].depends_on[{dep_idx}]"),
                    message: format!("unknown dependency `{dependency}`"),
                });
            }
        }
    }

    let mut visiting = BTreeSet::new();
    let mut visited = BTreeSet::new();
    for child in children {
        detect_cycle(
            child.key.as_str(),
            &key_to_index,
            children,
            &mut visiting,
            &mut visited,
        )?;
    }
    Ok(())
}

fn detect_cycle<'a>(
    key: &'a str,
    key_to_index: &BTreeMap<&'a str, usize>,
    children: &'a [ChildDraftWire],
    visiting: &mut BTreeSet<&'a str>,
    visited: &mut BTreeSet<&'a str>,
) -> Result<(), ToolError> {
    if visited.contains(key) {
        return Ok(());
    }
    if !visiting.insert(key) {
        let idx = key_to_index[key];
        return Err(ToolError::InvalidPayload {
            field: format!("children[{idx}].depends_on"),
            message: format!("dependency graph is cyclic at `{key}`"),
        });
    }
    let idx = key_to_index[key];
    for dependency in &children[idx].depends_on {
        detect_cycle(
            dependency.as_str(),
            key_to_index,
            children,
            visiting,
            visited,
        )?;
    }
    visiting.remove(key);
    visited.insert(key);
    Ok(())
}

fn map_runner_error(err: RunnerError) -> ToolError {
    match err {
        RunnerError::PolicyDisabled => ToolError::PolicyViolation {
            gate: "decomposition.enabled".into(),
            message: err.to_string(),
        },
        RunnerError::NoOwnerRoleConfigured => ToolError::PolicyViolation {
            gate: "decomposition.owner_role".into(),
            message: err.to_string(),
        },
        RunnerError::AuthorRoleMismatch { .. } => ToolError::PolicyViolation {
            gate: "decomposition.owner_role".into(),
            message: err.to_string(),
        },
        RunnerError::MaxDepthExceeded { .. } => ToolError::PolicyViolation {
            gate: "decomposition.max_depth".into(),
            message: err.to_string(),
        },
        RunnerError::ChildMissingRole { ref key } => ToolError::InvalidPayload {
            field: format!("children[{key}].assigned_role"),
            message: err.to_string(),
        },
        RunnerError::Domain(domain) => map_domain_error(domain),
    }
}

fn map_domain_error(err: DecompositionError) -> ToolError {
    match err {
        DecompositionError::AcceptanceCriteriaRequired { key } => ToolError::PolicyViolation {
            gate: "decomposition.require_acceptance_criteria_per_child".into(),
            message: format!("child {key} must include acceptance criteria"),
        },
        other => ToolError::InvalidPayload {
            field: "$".into(),
            message: other.to_string(),
        },
    }
}

#[cfg(test)]
mod tests {
    use serde_json::json;
    use symphony_config::{RoleCatalogEntry, RoleCatalogSourceSummary};
    use symphony_core::decomposition::ChildKey;
    use symphony_core::decomposition_applier::AppliedChild;
    use symphony_core::{DecompositionPolicy, DecompositionTriggers};
    use symphony_state::migrations::migrations;
    use symphony_state::repository::{NewWorkItem, WorkItemRepository};

    use super::*;

    const NOW: &str = "2026-05-10T00:00:00Z";

    fn catalog() -> RoleCatalog {
        RoleCatalog {
            entries: vec![RoleCatalogEntry {
                name: "backend".into(),
                kind: symphony_config::RoleKind::Specialist,
                description: None,
                agent_profile: None,
                agent_backend: None,
                max_concurrent: None,
                tools: Vec::new(),
                owns: vec!["Rust core".into()],
                does_not_own: Vec::new(),
                requires: Vec::new(),
                handoff_expectations: Vec::new(),
                routing_hints: Default::default(),
                matching_global_rules: Vec::new(),
                instruction_pack_summary: Default::default(),
                used_fallback_assignment: false,
            }],
            source_summary: RoleCatalogSourceSummary::default(),
        }
    }

    fn runner() -> DecompositionRunner {
        DecompositionRunner::new(DecompositionPolicy {
            enabled: true,
            owner_role: Some(RoleName::new("platform_lead")),
            triggers: DecompositionTriggers::default(),
            max_depth: 2,
            require_acceptance_criteria_per_child: true,
            child_issue_policy: symphony_core::FollowupPolicy::CreateDirectly,
        })
    }

    fn handler(
        parent_depth: u32,
    ) -> (
        ProposeDecompositionHandler,
        Arc<MemoryDecompositionProposalSink>,
    ) {
        let sink = Arc::new(MemoryDecompositionProposalSink::default());
        (
            ProposeDecompositionHandler::new(ProposeDecompositionContext {
                runner: runner(),
                sink: sink.clone(),
                role_catalog: catalog(),
                parent: WorkItemId::new(1),
                parent_depth,
                author_role: RoleName::new("platform_lead"),
                decomposition_id: DecompositionId::new(99),
            }),
            sink,
        )
    }

    fn state_handler() -> (
        ProposeDecompositionHandler,
        Arc<StateDecompositionApplySink>,
    ) {
        let mut db = StateDb::open_in_memory().unwrap();
        db.migrate(migrations()).unwrap();
        db.create_work_item(NewWorkItem {
            tracker_id: "github",
            identifier: "OWNER/REPO#1",
            parent_id: None,
            title: "parent",
            status_class: "intake",
            tracker_status: "open",
            assigned_role: Some("platform_lead"),
            assigned_agent: None,
            priority: None,
            workspace_policy: None,
            branch_policy: None,
            now: NOW,
        })
        .unwrap();
        let applied = AppliedDecomposition {
            proposal_id: DecompositionId::new(99),
            children: vec![
                AppliedChild {
                    key: ChildKey::new("schema"),
                    tracker_id: symphony_core::tracker::IssueId::new("100"),
                    identifier: "OWNER/REPO#100".into(),
                    url: None,
                    linked_parent: false,
                },
                AppliedChild {
                    key: ChildKey::new("api"),
                    tracker_id: symphony_core::tracker::IssueId::new("101"),
                    identifier: "OWNER/REPO#101".into(),
                    url: None,
                    linked_parent: false,
                },
            ],
        };
        let sink = Arc::new(StateDecompositionApplySink::new(db, applied, "github", NOW));
        (
            ProposeDecompositionHandler::new(ProposeDecompositionContext {
                runner: runner(),
                sink: sink.clone(),
                role_catalog: catalog(),
                parent: WorkItemId::new(1),
                parent_depth: 0,
                author_role: RoleName::new("platform_lead"),
                decomposition_id: DecompositionId::new(99),
            }),
            sink,
        )
    }

    fn valid_payload() -> Value {
        json!({
            "summary": "Split backend work",
            "children": [
                {
                    "key": "schema",
                    "title": "Build schema",
                    "scope": "Database schema",
                    "assigned_role": "backend",
                    "acceptance_criteria": ["schema migrates"]
                },
                {
                    "key": "api",
                    "title": "Build API",
                    "scope": "HTTP API",
                    "assigned_role": "backend",
                    "acceptance_criteria": ["endpoint works"],
                    "depends_on": ["schema"]
                }
            ]
        })
    }

    #[tokio::test]
    async fn propose_decomposition_accepts_valid_payload_and_persists() {
        let (handler, sink) = state_handler();
        let result = handler.call(valid_payload()).await.unwrap();
        assert_eq!(result.content["decomposition_id"], 99);
        assert_eq!(result.content["child_count"], 2);
        assert_eq!(result.content["status"], "applied");
        sink.with_db(|db| {
            let children = db
                .list_children(symphony_state::repository::WorkItemId(1))
                .unwrap();
            assert_eq!(children.len(), 2);
        });
    }

    #[tokio::test]
    async fn propose_decomposition_rejects_unknown_role_with_field_path() {
        let (handler, _sink) = handler(0);
        let mut payload = valid_payload();
        payload["children"][0]["assigned_role"] = json!("ghost");
        let err = handler.call(payload).await.unwrap_err();
        assert!(matches!(
            err,
            ToolError::InvalidPayload { field, .. } if field == "children[0].assigned_role"
        ));
    }

    #[tokio::test]
    async fn propose_decomposition_rejects_cyclic_depends_on() {
        let (handler, _sink) = handler(0);
        let mut payload = valid_payload();
        payload["children"][0]["depends_on"] = json!(["api"]);
        let err = handler.call(payload).await.unwrap_err();
        assert!(matches!(err, ToolError::InvalidPayload { .. }));
    }

    #[tokio::test]
    async fn propose_decomposition_maps_missing_acceptance_to_policy_violation() {
        let (handler, _sink) = handler(0);
        let mut payload = valid_payload();
        payload["children"][0]["acceptance_criteria"] = json!([]);
        let err = handler.call(payload).await.unwrap_err();
        assert!(matches!(
            err,
            ToolError::PolicyViolation { gate, .. }
                if gate == "decomposition.require_acceptance_criteria_per_child"
        ));
    }

    #[tokio::test]
    async fn propose_decomposition_maps_max_depth_to_policy_violation() {
        let (handler, _sink) = handler(2);
        let err = handler.call(valid_payload()).await.unwrap_err();
        assert!(matches!(
            err,
            ToolError::PolicyViolation { gate, .. } if gate == "decomposition.max_depth"
        ));
    }
}
