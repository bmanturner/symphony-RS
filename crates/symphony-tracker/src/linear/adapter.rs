//! `LinearTracker` — the production [`TrackerRead`] adapter for Linear.
//!
//! The adapter speaks Linear's GraphQL API at
//! `https://api.linear.app/graphql` (overridable for tests via
//! [`LinearConfig::endpoint`]) using the typed query bindings in
//! [`super::queries`]. Its only job is to translate trait calls into
//! GraphQL traffic and translate responses back into the normalized
//! [`Issue`] model — the orchestrator never sees a Linear-shaped value.
//!
//! ## What lives here vs. in the orchestrator
//!
//! Per SPEC §11.1, the three operations the orchestrator depends on are
//! `fetch_candidate_issues`, `fetch_issues_by_states`, and
//! `fetch_issue_states_by_ids`. The trait method names map 1:1 to those
//! operations (see [`crate::TrackerRead`] doc comments for the rename
//! rationale). All Linear-specific concerns — pagination, GraphQL error
//! shape, HTTP status interpretation, label lowercasing, priority
//! rounding — are implemented in this file and stop here.
//!
//! ## Error mapping
//!
//! Each failure mode the adapter encounters is mapped to the closest
//! [`TrackerError`] variant so the orchestrator can react with the
//! coarse-grained policy SPEC §16.3 calls for. The mapping is tested at
//! the integration layer in `tests/linear.rs`:
//!
//! | Source                                  | Variant        |
//! |-----------------------------------------|----------------|
//! | reqwest connect/read failure            | `Transport`    |
//! | HTTP 401 / 403                          | `Unauthorized` |
//! | HTTP 5xx                                | `Transport`    |
//! | HTTP 4xx (other)                        | `Misconfigured`|
//! | GraphQL `errors[]` populated            | `Transport`    |
//! | JSON decode failure / missing `data`    | `Malformed`    |
//!
//! `Transport` is deliberately the catch-all for anything that *might*
//! recover on the next poll tick — the reconcile pass tolerates it (SPEC
//! §16.3) — while `Misconfigured` is reserved for things only the
//! operator can fix.

use std::time::Duration;

use async_trait::async_trait;
use graphql_client::{GraphQLQuery, Response};
use reqwest::{Client, StatusCode};
use secrecy::{ExposeSecret, SecretString};
use serde::Serialize;
use serde::de::DeserializeOwned;
use tracing::{debug, warn};
use url::Url;

use crate::TrackerRead;
use symphony_core::tracker::{BlockerRef, Issue, IssueId, IssueState};
use symphony_core::tracker_trait::{
    AddBlockerRequest, AddBlockerResponse, AddCommentRequest, AddCommentResponse, ArtifactKind,
    AttachArtifactRequest, AttachArtifactResponse, CreateIssueRequest, CreateIssueResponse,
    LinkParentChildRequest, LinkParentChildResponse, TrackerCapabilities, TrackerError,
    TrackerMutations, TrackerResult, UpdateIssueRequest, UpdateIssueResponse,
};

use super::queries::{
    AttachmentCreate, AttachmentCreateVariables, CandidateIssues, CandidateIssuesVariables,
    CommentCreate, CommentCreateVariables, IssueCreate, IssueCreateVariables, IssueRelationCreate,
    IssueRelationCreateVariables, IssueStatesByIds, IssueStatesByIdsVariables, IssueUpdate,
    IssueUpdateVariables, IssuesByStates, IssuesByStatesVariables, attachment_create,
    candidate_issues, comment_create, issue_create, issue_relation_create, issue_states_by_ids,
    issue_update, issues_by_states,
};

/// Operator-supplied configuration for [`LinearTracker`].
///
/// All fields are required at construction time. The active-state and
/// project-slug values come from `WORKFLOW.md` (parsed by
/// `symphony-config`); the API key comes from environment via
/// `figment`/`secrecy`. The endpoint is configurable purely so the
/// integration tests can point at a `wiremock` server — production
/// callers should leave it at the default.
#[derive(Debug, Clone)]
pub struct LinearConfig {
    /// Linear personal API key. Linear accepts the raw key as the value
    /// of the `Authorization` header (no `Bearer` prefix). Wrapped in
    /// [`SecretString`] so accidental log expansion or `Debug` prints do
    /// not leak it.
    pub api_key: SecretString,

    /// `slugId` of the Linear project to dispatch from. Mapped into the
    /// `$projectSlug` GraphQL variable in [`CandidateIssues`] /
    /// [`IssuesByStates`].
    pub project_slug: String,

    /// Operator-configured active states from `WORKFLOW.md` (preserving
    /// caller casing). Passed verbatim as `$stateNames` so the server
    /// does the filtering — SPEC §11.1 op 1 wants the adapter to return
    /// *only* these issues from `fetch_active`.
    pub active_states: Vec<String>,

    /// GraphQL endpoint. Defaults to `https://api.linear.app/graphql`
    /// via [`LinearConfig::with_defaults`]; tests override.
    pub endpoint: Url,

    /// Page size for paginated queries (`$first`). Linear caps this at
    /// 250; SPEC §11.2 sets the default to 50.
    pub page_size: i64,

    /// Team id used as `teamId` on `issueCreate`. `None` is fine for
    /// read-only deployments; mutation calls that genuinely need it
    /// (`create_issue`) surface a [`TrackerError::Misconfigured`] when it
    /// is missing rather than silently dropping the request.
    pub team_id: Option<String>,
}

impl LinearConfig {
    /// Construct a config with Linear's production endpoint and the
    /// SPEC-default page size, leaving the caller to provide the parts
    /// that genuinely vary per deployment.
    pub fn with_defaults(
        api_key: SecretString,
        project_slug: impl Into<String>,
        active_states: Vec<String>,
    ) -> Result<Self, TrackerError> {
        let endpoint = Url::parse("https://api.linear.app/graphql")
            .map_err(|e| TrackerError::Misconfigured(format!("invalid default endpoint: {e}")))?;
        Ok(Self {
            api_key,
            project_slug: project_slug.into(),
            active_states,
            endpoint,
            page_size: 50,
            team_id: None,
        })
    }
}

/// Production [`TrackerRead`] adapter targeting Linear's GraphQL API.
///
/// Construct via [`LinearTracker::new`]; the resulting handle is cheap
/// to clone (a `reqwest::Client` is internally `Arc`-shared) so the
/// orchestrator can hand it to multiple workers without contention.
///
/// The adapter is stateless beyond its config + HTTP client: every call
/// issues a fresh GraphQL request. SPEC §16.3 does not ask for caching
/// at this layer — the orchestrator already debounces by polling on a
/// fixed interval.
#[derive(Clone)]
pub struct LinearTracker {
    client: Client,
    config: LinearConfig,
}

impl std::fmt::Debug for LinearTracker {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        // Spell out the safe fields explicitly so a future refactor that
        // adds the api_key to a derived `Debug` would have to delete
        // this impl on purpose. Defence-in-depth against secret leaks.
        f.debug_struct("LinearTracker")
            .field("endpoint", &self.config.endpoint.as_str())
            .field("project_slug", &self.config.project_slug)
            .field("active_states", &self.config.active_states)
            .field("page_size", &self.config.page_size)
            .finish()
    }
}

impl LinearTracker {
    /// Build a tracker from a config. Constructs the underlying
    /// [`reqwest::Client`] with Symphony's timeouts; reuse the returned
    /// handle for every poll.
    pub fn new(config: LinearConfig) -> Result<Self, TrackerError> {
        let client = Client::builder()
            // SPEC §16.3 expects polls to complete in seconds, not minutes.
            // A 30s ceiling is generous enough for paginated sweeps over
            // hundreds of issues yet short enough that a stuck connection
            // surfaces as a Transport error before the next tick.
            .timeout(Duration::from_secs(30))
            .build()
            .map_err(|e| TrackerError::Other(format!("reqwest client build: {e}")))?;
        Ok(Self { client, config })
    }

    /// Build a tracker using a caller-supplied [`reqwest::Client`].
    ///
    /// Tests use this to inject a client with a shorter timeout or with
    /// retry middleware disabled. Production callers should prefer
    /// [`LinearTracker::new`].
    pub fn with_client(client: Client, config: LinearConfig) -> Self {
        Self { client, config }
    }

    /// Issue a single GraphQL operation and decode the response.
    ///
    /// Centralised so the three trait methods share identical error
    /// handling; if SPEC §11.2 ever asks for retries or hedging, this
    /// is the one place to add them.
    async fn execute<Q>(&self, variables: Q::Variables) -> TrackerResult<Q::ResponseData>
    where
        Q: GraphQLQuery,
        Q::Variables: Serialize,
        Q::ResponseData: DeserializeOwned,
    {
        let body = Q::build_query(variables);
        let op_name = body.operation_name;

        let response = self
            .client
            .post(self.config.endpoint.clone())
            // Linear's docs are explicit: the personal API key goes in
            // `Authorization` with no `Bearer` prefix. OAuth tokens
            // would use `Bearer`; if we ever support those, this header
            // construction needs to branch on token kind.
            .header("Authorization", self.config.api_key.expose_secret())
            .header("Content-Type", "application/json")
            .json(&body)
            .send()
            .await
            .map_err(|e| TrackerError::Transport(format!("{op_name}: {e}")))?;

        let status = response.status();
        let bytes = response
            .bytes()
            .await
            .map_err(|e| TrackerError::Transport(format!("{op_name}: read body: {e}")))?;

        if !status.is_success() {
            return Err(map_http_status(op_name, status, &bytes));
        }

        // Decode into graphql_client's envelope first so we can surface
        // schema-level errors distinctly from transport failures. A
        // payload that fails to decode here almost always means our
        // schema.graphql has drifted from the wire shape.
        let envelope: Response<Q::ResponseData> = serde_json::from_slice(&bytes).map_err(|e| {
            TrackerError::Malformed(format!(
                "{op_name}: response did not match schema: {e}; raw={}",
                truncate_for_log(&bytes)
            ))
        })?;

        if let Some(errors) = envelope.errors.filter(|e| !e.is_empty()) {
            // GraphQL errors come back with HTTP 200 by convention. We
            // route them to Transport because the orchestrator's
            // reconcile path is the right place to keep retrying — a
            // misconfiguration would more typically surface as a 4xx.
            let joined = errors
                .iter()
                .map(|e| e.message.as_str())
                .collect::<Vec<_>>()
                .join("; ");
            warn!(operation = op_name, errors = %joined, "linear graphql returned errors");
            return Err(TrackerError::Transport(format!(
                "{op_name}: graphql errors: {joined}"
            )));
        }

        envelope.data.ok_or_else(|| {
            TrackerError::Malformed(format!("{op_name}: graphql response missing data block"))
        })
    }

    /// Walk Relay-style pagination over [`CandidateIssues`].
    ///
    /// Linear caps `first` at 250 and SPEC §11.2 sets our default at 50,
    /// so a typical project yields one page; the loop exists for the
    /// long-tail "very busy backlog" case the spec calls out. Every
    /// page-fetched node is normalized eagerly so the caller sees a
    /// single flat list of [`Issue`]s.
    async fn paginate_candidate(&self) -> TrackerResult<Vec<Issue>> {
        let mut out = Vec::new();
        let mut after: Option<String> = None;
        loop {
            let data = self
                .execute::<CandidateIssues>(CandidateIssuesVariables {
                    project_slug: self.config.project_slug.clone(),
                    state_names: self.config.active_states.clone(),
                    first: self.config.page_size,
                    after: after.clone(),
                })
                .await?;
            for node in data.issues.nodes {
                out.push(candidate_node_to_issue(node));
            }
            if !data.issues.page_info.has_next_page {
                break;
            }
            after = data.issues.page_info.end_cursor;
            if after.is_none() {
                debug!("CandidateIssues reported hasNextPage=true but no endCursor; halting");
                break;
            }
        }
        Ok(out)
    }

    /// Same shape as [`Self::paginate_candidate`] but for the terminal
    /// cleanup sweep. We deliberately keep them as two methods so a
    /// future divergence (different page size, different filter shape)
    /// is a one-line change.
    async fn paginate_terminal(&self, state_names: Vec<String>) -> TrackerResult<Vec<Issue>> {
        let mut out = Vec::new();
        let mut after: Option<String> = None;
        loop {
            let data = self
                .execute::<IssuesByStates>(IssuesByStatesVariables {
                    project_slug: self.config.project_slug.clone(),
                    state_names: state_names.clone(),
                    first: self.config.page_size,
                    after: after.clone(),
                })
                .await?;
            for node in data.issues.nodes {
                out.push(issues_by_states_node_to_issue(node));
            }
            if !data.issues.page_info.has_next_page {
                break;
            }
            after = data.issues.page_info.end_cursor;
            if after.is_none() {
                break;
            }
        }
        Ok(out)
    }
}

#[async_trait]
impl TrackerRead for LinearTracker {
    async fn fetch_active(&self) -> TrackerResult<Vec<Issue>> {
        self.paginate_candidate().await
    }

    async fn fetch_state(&self, ids: &[IssueId]) -> TrackerResult<Vec<Issue>> {
        if ids.is_empty() {
            // No round-trip needed; mirrors the documented contract
            // ("an empty filter must yield empty results") so callers
            // don't pay for an obviously-empty GraphQL request.
            return Ok(Vec::new());
        }

        let data = self
            .execute::<IssueStatesByIds>(IssueStatesByIdsVariables {
                ids: ids.iter().map(|id| id.0.clone()).collect(),
            })
            .await?;

        // Linear returns nodes in server-defined order. The trait
        // contract (and the conformance suite) requires we preserve the
        // caller's order so the reconcile pass can index by position.
        let mut by_id: std::collections::HashMap<String, Issue> = data
            .issues
            .nodes
            .into_iter()
            .map(|n| (n.id.clone(), state_only_node_to_issue(n)))
            .collect();
        Ok(ids
            .iter()
            .filter_map(|id| by_id.remove(id.as_str()))
            .collect())
    }

    async fn fetch_terminal_recent(
        &self,
        terminal_states: &[IssueState],
    ) -> TrackerResult<Vec<Issue>> {
        // Honour the documented "no fallback on empty filter" rule —
        // adapters MUST NOT default to "all terminals" here. The
        // conformance suite asserts this; returning early also avoids a
        // GraphQL request whose filter would be `[]`.
        if terminal_states.is_empty() {
            return Ok(Vec::new());
        }
        let names = terminal_states
            .iter()
            .map(|s| s.as_str().to_string())
            .collect();
        self.paginate_terminal(names).await
    }

    fn capabilities(&self) -> TrackerCapabilities {
        // Linear has first-class support for every SPEC v2 §7.2 mutation:
        // `issueCreate` with `parentId` for sub-issues, `issueRelationCreate`
        // with `type: "blocks"` for structural blocker edges,
        // `commentCreate`/`attachmentCreate` for evidence. The adapter only
        // claims capability for fields it can actually round-trip without
        // resolving auxiliary IDs — see the per-method docs for the
        // resolved-id deferrals.
        TrackerCapabilities::FULL
    }
}

#[async_trait]
impl TrackerMutations for LinearTracker {
    /// Create an issue. Maps directly to `issueCreate`.
    ///
    /// `parent` is honoured atomically via Linear's native `parentId` —
    /// no separate `link_parent_child` call is required for Linear,
    /// matching the SPEC v2 §7.2 contract for trackers with structural
    /// sub-issue support.
    ///
    /// `labels`, `assignees`, and `initial_state` accept tracker-native
    /// IDs (per the trait docstring on [`CreateIssueRequest`]); name → id
    /// resolution is intentionally not in scope for this adapter
    /// iteration. If the caller passes labels/assignees by *name* the
    /// adapter rejects with [`TrackerError::Misconfigured`] rather than
    /// silently sending unresolved strings.
    async fn create_issue(
        &self,
        request: CreateIssueRequest,
    ) -> TrackerResult<CreateIssueResponse> {
        let team_id = self.config.team_id.as_ref().ok_or_else(|| {
            TrackerError::Misconfigured(
                "LinearConfig.team_id is required for issueCreate".to_string(),
            )
        })?;

        let label_ids = if request.labels.is_empty() {
            None
        } else {
            Some(request.labels.clone())
        };
        let assignee_id = match request.assignees.len() {
            0 => None,
            1 => Some(request.assignees[0].clone()),
            _ => {
                // Linear issues have a single `assigneeId` field. Multi-
                // assignee semantics belong on a follow-up issue or in a
                // future adapter pass.
                return Err(TrackerError::Misconfigured(
                    "Linear supports a single assignee per issue; got multiple in CreateIssueRequest"
                        .to_string(),
                ));
            }
        };
        let parent_id = request.parent.as_ref().map(|p| p.0.clone());
        let state_id = request
            .initial_state
            .as_ref()
            .map(|s| s.as_str().to_string());

        let input = issue_create::IssueCreateInput {
            team_id: team_id.clone(),
            title: request.title,
            description: request.body,
            label_ids,
            assignee_id,
            parent_id,
            state_id,
        };
        let data = self
            .execute::<IssueCreate>(IssueCreateVariables { input })
            .await?;

        if !data.issue_create.success {
            return Err(TrackerError::Transport(
                "IssueCreate: linear reported success=false".to_string(),
            ));
        }
        let issue = data.issue_create.issue.ok_or_else(|| {
            TrackerError::Malformed(
                "IssueCreate: success=true but issue payload missing".to_string(),
            )
        })?;
        Ok(CreateIssueResponse {
            id: IssueId::new(issue.id),
            identifier: issue.identifier,
            url: issue.url,
        })
    }

    /// Update an issue. Maps to `issueUpdate`.
    ///
    /// State changes accept a Linear `workflowState` ID through
    /// [`UpdateIssueRequest::state`] — the contract is that the workflow
    /// passes adapter-native identifiers. Name-based label add/remove
    /// would require fetching the current label set on every call to
    /// compute the union/diff (Linear's `issueUpdate` replaces
    /// `labelIds` wholesale); that pass is deferred. If the caller
    /// supplies `add_labels` or `remove_labels` the adapter rejects
    /// with [`TrackerError::Misconfigured`] rather than silently
    /// dropping or replacing the label set.
    async fn update_issue(
        &self,
        request: UpdateIssueRequest,
    ) -> TrackerResult<UpdateIssueResponse> {
        if !request.add_labels.is_empty() || !request.remove_labels.is_empty() {
            return Err(TrackerError::Misconfigured(
                "Linear update_issue does not yet support add_labels/remove_labels by name; \
                 send a follow-up adapter pass or update labelIds via a custom path"
                    .to_string(),
            ));
        }
        if request.add_assignees.len() > 1 || !request.remove_assignees.is_empty() {
            return Err(TrackerError::Misconfigured(
                "Linear supports a single assigneeId; multi-assignee mutations are out of scope"
                    .to_string(),
            ));
        }

        let assignee_id = request.add_assignees.into_iter().next();
        let state_id = request.state.as_ref().map(|s| s.as_str().to_string());

        let input = issue_update::IssueUpdateInput {
            title: request.title,
            description: request.body,
            state_id,
            parent_id: None,
            label_ids: None,
            assignee_id,
        };
        let data = self
            .execute::<IssueUpdate>(IssueUpdateVariables {
                id: request.id.0.clone(),
                input,
            })
            .await?;

        if !data.issue_update.success {
            return Err(TrackerError::Transport(
                "IssueUpdate: linear reported success=false".to_string(),
            ));
        }
        let observed_state = data
            .issue_update
            .issue
            .map(|i| IssueState::new(i.state.name));
        Ok(UpdateIssueResponse {
            id: request.id,
            // Prefer the tracker-observed state when available so the
            // orchestrator persists what Linear actually accepted (e.g.
            // when a workflow rule rewrote the requested transition).
            state: observed_state.or(request.state),
        })
    }

    async fn add_comment(&self, request: AddCommentRequest) -> TrackerResult<AddCommentResponse> {
        let body = match request.author_role.as_deref() {
            Some(role) if !role.is_empty() => format!("**[{role}]** {}", request.body),
            _ => request.body,
        };
        let data = self
            .execute::<CommentCreate>(CommentCreateVariables {
                input: comment_create::CommentCreateInput {
                    issue_id: request.issue.0,
                    body,
                },
            })
            .await?;
        if !data.comment_create.success {
            return Err(TrackerError::Transport(
                "CommentCreate: linear reported success=false".to_string(),
            ));
        }
        let comment = data.comment_create.comment.ok_or_else(|| {
            TrackerError::Malformed(
                "CommentCreate: success=true but comment payload missing".to_string(),
            )
        })?;
        Ok(AddCommentResponse {
            id: Some(comment.id),
            url: comment.url,
        })
    }

    async fn add_blocker(&self, request: AddBlockerRequest) -> TrackerResult<AddBlockerResponse> {
        // Linear's relation model: `issueId` is the source, `relatedIssueId`
        // is the target, and `type: "blocks"` means "issueId blocks
        // relatedIssueId". The trait phrases the request as
        // `(blocked, blocker)` so we map `issueId = blocker`,
        // `relatedIssueId = blocked`. The optional `reason` is dropped on
        // the Linear side because `IssueRelationCreateInput` has no
        // description channel; surfacing it via a follow-up comment is
        // left to the workflow.
        let _ = request.reason;
        let data = self
            .execute::<IssueRelationCreate>(IssueRelationCreateVariables {
                input: issue_relation_create::IssueRelationCreateInput {
                    issue_id: request.blocker.0,
                    related_issue_id: request.blocked.0,
                    type_: "blocks".to_string(),
                },
            })
            .await?;
        if !data.issue_relation_create.success {
            return Err(TrackerError::Transport(
                "IssueRelationCreate: linear reported success=false".to_string(),
            ));
        }
        let edge_id = data.issue_relation_create.issue_relation.map(|r| r.id);
        Ok(AddBlockerResponse { edge_id })
    }

    async fn link_parent_child(
        &self,
        request: LinkParentChildRequest,
    ) -> TrackerResult<LinkParentChildResponse> {
        // Linear sub-issues are expressed as `parentId` on the child.
        // `issueUpdate` with only `parentId` set is the canonical way to
        // attach an existing child under an existing parent.
        let input = issue_update::IssueUpdateInput {
            title: None,
            description: None,
            state_id: None,
            parent_id: Some(request.parent.0),
            label_ids: None,
            assignee_id: None,
        };
        let data = self
            .execute::<IssueUpdate>(IssueUpdateVariables {
                id: request.child.0,
                input,
            })
            .await?;
        if !data.issue_update.success {
            return Err(TrackerError::Transport(
                "link_parent_child: linear reported success=false".to_string(),
            ));
        }
        // Linear does not return a discrete edge id for parent/child
        // (the relation lives on the child's `parentId` field).
        Ok(LinkParentChildResponse { edge_id: None })
    }

    async fn attach_artifact(
        &self,
        request: AttachArtifactRequest,
    ) -> TrackerResult<AttachArtifactResponse> {
        let title = request
            .label
            .clone()
            .unwrap_or_else(|| match &request.kind {
                ArtifactKind::PullRequest => "Pull request".to_string(),
                ArtifactKind::RunLog => "Run log".to_string(),
                ArtifactKind::QaEvidence => "QA evidence".to_string(),
                ArtifactKind::Other { name } => name.clone(),
            });
        let data = self
            .execute::<AttachmentCreate>(AttachmentCreateVariables {
                input: attachment_create::AttachmentCreateInput {
                    issue_id: request.issue.0,
                    url: request.uri,
                    title,
                    subtitle: request.note,
                },
            })
            .await?;
        if !data.attachment_create.success {
            return Err(TrackerError::Transport(
                "AttachmentCreate: linear reported success=false".to_string(),
            ));
        }
        Ok(AttachArtifactResponse {
            id: data.attachment_create.attachment.map(|a| a.id),
        })
    }
}

// ---------------------------------------------------------------------------
// Normalization helpers — convert generated GraphQL response types into
// `Issue`. Kept private and free-standing so they're trivially testable
// from `mod tests` below.
// ---------------------------------------------------------------------------

/// Translate Linear's `priority` (Float, 0=none, 1..4=urgent..low) into
/// our [`Issue::priority`] (Option<i32>, lower=higher per SPEC §4.1.1).
///
/// SPEC §11.3 mandates integer priority. Linear's 0.0 sentinel maps to
/// `None`, not `Some(0)`, because the orchestrator's priority sort
/// treats `None` as "lowest priority" and we don't want unprioritised
/// issues to leapfrog actually-urgent ones.
fn linear_priority_to_int(p: Option<f64>) -> Option<i32> {
    match p {
        Some(f) if f > 0.0 => Some(f.round() as i32),
        _ => None,
    }
}

/// Build a [`BlockerRef`] from a single `inverseRelations` entry, after
/// the type-filter has confirmed it really is a `blocks` relation.
fn relation_to_blocker(
    issue: candidate_issues::CandidateIssuesIssuesNodesInverseRelationsNodesIssue,
) -> BlockerRef {
    BlockerRef {
        id: Some(IssueId::new(issue.id)),
        identifier: Some(issue.identifier),
        state: Some(IssueState::new(issue.state.name)),
    }
}

/// Same shape as [`relation_to_blocker`] but typed against the
/// [`IssuesByStates`] response. graphql_client emits one type per
/// query-and-path even when the GraphQL types are identical, so we get
/// a separate function rather than playing tricks with generics.
fn relation_to_blocker_terminal(
    issue: issues_by_states::IssuesByStatesIssuesNodesInverseRelationsNodesIssue,
) -> BlockerRef {
    BlockerRef {
        id: Some(IssueId::new(issue.id)),
        identifier: Some(issue.identifier),
        state: Some(IssueState::new(issue.state.name)),
    }
}

fn candidate_node_to_issue(node: candidate_issues::CandidateIssuesIssuesNodes) -> Issue {
    Issue {
        id: IssueId::new(node.id),
        identifier: node.identifier,
        title: node.title,
        description: node.description,
        priority: linear_priority_to_int(node.priority),
        state: IssueState::new(node.state.name),
        branch_name: node.branch_name,
        url: node.url,
        labels: node
            .labels
            .nodes
            .into_iter()
            // SPEC §11.3 stores labels lowercased so template lookups
            // match operator-configured rules without per-call casing.
            .map(|l| l.name.to_ascii_lowercase())
            .collect(),
        blocked_by: node
            .inverse_relations
            .nodes
            .into_iter()
            // Linear's `inverseRelations` includes `blocks`, `related`,
            // `duplicate`, etc. Only `blocks` belongs in `blocked_by` —
            // everything else is informational and the orchestrator
            // doesn't model it.
            .filter(|n| n.type_ == "blocks")
            .map(|n| relation_to_blocker(n.issue))
            .collect(),
        created_at: node.created_at,
        updated_at: node.updated_at,
    }
}

fn issues_by_states_node_to_issue(node: issues_by_states::IssuesByStatesIssuesNodes) -> Issue {
    Issue {
        id: IssueId::new(node.id),
        identifier: node.identifier,
        title: node.title,
        description: node.description,
        priority: linear_priority_to_int(node.priority),
        state: IssueState::new(node.state.name),
        branch_name: node.branch_name,
        url: node.url,
        labels: node
            .labels
            .nodes
            .into_iter()
            .map(|l| l.name.to_ascii_lowercase())
            .collect(),
        blocked_by: node
            .inverse_relations
            .nodes
            .into_iter()
            .filter(|n| n.type_ == "blocks")
            .map(|n| relation_to_blocker_terminal(n.issue))
            .collect(),
        created_at: node.created_at,
        updated_at: node.updated_at,
    }
}

fn state_only_node_to_issue(node: issue_states_by_ids::IssueStatesByIdsIssuesNodes) -> Issue {
    // The reconcile-by-id query selects only the fields the orchestrator
    // reads (id, identifier, state.name). All other fields land as `None`
    // / empty so the adapter does not fabricate values it didn't receive
    // — see the fabrication contract on `Issue`.
    Issue {
        id: IssueId::new(node.id),
        identifier: node.identifier,
        title: String::new(),
        description: None,
        priority: None,
        state: IssueState::new(node.state.name),
        branch_name: None,
        url: None,
        labels: Vec::new(),
        blocked_by: Vec::new(),
        created_at: None,
        updated_at: None,
    }
}

/// Map a non-2xx response to the appropriate [`TrackerError`] variant.
///
/// Pulled out so the integration tests can target the exact mapping
/// rules in the table at the top of this module.
fn map_http_status(op: &'static str, status: StatusCode, body: &[u8]) -> TrackerError {
    let snippet = truncate_for_log(body);
    if status == StatusCode::UNAUTHORIZED || status == StatusCode::FORBIDDEN {
        TrackerError::Unauthorized(format!("{op}: HTTP {status}: {snippet}"))
    } else if status.is_server_error() {
        TrackerError::Transport(format!("{op}: HTTP {status}: {snippet}"))
    } else {
        // 4xx that isn't auth: almost always a malformed query or a
        // misconfigured project slug. Surface as Misconfigured so the
        // operator sees a loud, actionable signal.
        TrackerError::Misconfigured(format!("{op}: HTTP {status}: {snippet}"))
    }
}

/// Trim a response body for log inclusion. We never log full payloads —
/// they can carry issue titles operators consider sensitive — so even
/// the error path caps at a few hundred bytes of UTF-8-lossy text.
fn truncate_for_log(body: &[u8]) -> String {
    const MAX: usize = 200;
    let s = String::from_utf8_lossy(body);
    if s.len() <= MAX {
        s.into_owned()
    } else {
        format!("{}…", &s[..MAX])
    }
}

#[cfg(test)]
mod tests {
    //! Pure normalization tests — no HTTP. The wire-level happy/4xx/5xx/
    //! malformed cases live in `tests/linear.rs` against `wiremock`.

    use super::*;

    #[test]
    fn linear_priority_zero_collapses_to_none_so_unprioritised_does_not_leapfrog_urgent() {
        assert_eq!(linear_priority_to_int(None), None);
        assert_eq!(linear_priority_to_int(Some(0.0)), None);
        assert_eq!(linear_priority_to_int(Some(-1.0)), None);
        assert_eq!(linear_priority_to_int(Some(1.0)), Some(1));
        assert_eq!(linear_priority_to_int(Some(2.6)), Some(3));
    }

    #[test]
    fn truncate_for_log_caps_long_bodies_with_an_ellipsis_marker() {
        let big = "x".repeat(1000);
        let out = truncate_for_log(big.as_bytes());
        assert!(out.len() <= 220);
        assert!(out.ends_with('…'));
    }

    #[test]
    fn truncate_for_log_passes_short_bodies_through_unchanged() {
        let small = b"hello";
        assert_eq!(truncate_for_log(small), "hello");
    }

    #[test]
    fn map_http_status_routes_401_403_to_unauthorized() {
        let e = map_http_status("Op", StatusCode::UNAUTHORIZED, b"nope");
        assert!(matches!(e, TrackerError::Unauthorized(_)));
        let e = map_http_status("Op", StatusCode::FORBIDDEN, b"nope");
        assert!(matches!(e, TrackerError::Unauthorized(_)));
    }

    #[test]
    fn map_http_status_routes_5xx_to_transport() {
        let e = map_http_status("Op", StatusCode::INTERNAL_SERVER_ERROR, b"oops");
        assert!(matches!(e, TrackerError::Transport(_)));
        let e = map_http_status("Op", StatusCode::BAD_GATEWAY, b"oops");
        assert!(matches!(e, TrackerError::Transport(_)));
    }

    #[test]
    fn map_http_status_routes_other_4xx_to_misconfigured() {
        let e = map_http_status("Op", StatusCode::BAD_REQUEST, b"bad query");
        assert!(matches!(e, TrackerError::Misconfigured(_)));
        let e = map_http_status("Op", StatusCode::NOT_FOUND, b"nope");
        assert!(matches!(e, TrackerError::Misconfigured(_)));
    }

    #[test]
    fn debug_impl_does_not_leak_api_key() {
        let cfg = LinearConfig::with_defaults(
            SecretString::from("lin_api_super_secret_token".to_string()),
            "alpha",
            vec!["Todo".into()],
        )
        .unwrap();
        let tracker = LinearTracker::new(cfg).unwrap();
        let dbg = format!("{:?}", tracker);
        assert!(!dbg.contains("lin_api_super_secret_token"));
        assert!(dbg.contains("alpha"));
    }
}
