# Symphony-RS Architecture v2

This document describes the target architecture for the v2 product direction: a configurable specialist-agent execution system with decomposition, integration ownership, exhaustive QA, blockers, and follow-up issue creation.

`SPEC_v2.md` is the product contract. This file explains how to build it.

## 1. Architectural Goal

Symphony-RS should evolve from its current issue-polling runner into a durable orchestration kernel with a deliberately small adapter set.

The kernel owns workflow truth:

- normalized work items;
- role routing;
- decomposition state;
- run records;
- workspace/branch claims;
- blocker graph;
- integration gates;
- QA verdicts;
- follow-up issue records;
- event stream.

Adapters own external protocol details:

- GitHub and Linear tracker APIs;
- git source-control operations;
- Codex, Claude, and Hermes agent runtimes;
- local workspaces managed through the git/workspace layer.

## 2. Layered View

```text
┌───────────────────────────────────────────────────────────────────────┐
│ Policy Layer                WORKFLOW.md v2                            │
│   roles, routing, decomposition, workspace, integration, QA, budgets   │
├───────────────────────────────────────────────────────────────────────┤
│ Product Orchestration Layer                                           │
│   intake, routing, decomposition, assignment, integration, QA gates     │
├───────────────────────────────────────────────────────────────────────┤
│ Durable State Layer                                                   │
│   work_items, runs, blockers, handoffs, qa_verdicts, events, artifacts │
├───────────────────────────────────────────────────────────────────────┤
│ Execution Scheduler Layer                                             │
│   queues, leases, retries, cancellation, concurrency, budgets           │
├───────────────────────────────────────────────────────────────────────┤
│ Workspace / Branch Layer                                              │
│   worktrees, branch claims, cwd/ref verification, cleanup               │
├───────────────────────────────────────────────────────────────────────┤
│ Adapter Traits                                                        │
│   TrackerRead/Mutations, AgentRuntime, GitProvider, WorkspaceManager   │
├───────────────────────────────────────────────────────────────────────┤
│ Concrete Adapters                                                     │
│   git, GitHub, Linear, Codex, Claude, Hermes                           │
├───────────────────────────────────────────────────────────────────────┤
│ Observability / Control                                               │
│   logs, status, SSE, TUI/dashboard, approvals, audit exports            │
└───────────────────────────────────────────────────────────────────────┘
```

## 3. Key Design Shifts From Current Implementation

### 3.1 From issue runner to workflow kernel

The current poll loop dispatches active issues. The target kernel creates internal work items, links them, blocks parent completion, and drives staged handoffs.

### 3.2 From single `agent.kind` to role-routed agents

The current config centers on `agent.kind`. The target config has named roles and named agents. A role selects an agent and defines authority. A backend executes sessions.

### 3.3 From comments-as-state to durable workflow objects

Agent comments are useful for humans, but v2 decisions must be structured:

- `Blocker`;
- `Handoff`;
- `QaVerdict`;
- `FollowupIssue`;
- `IntegrationRecord`.

### 3.4 From workspace directory to workspace claim

A workspace is not just a path. It is a claim over:

- path;
- branch/ref;
- base commit;
- owning issue/run;
- cleanliness policy;
- lifecycle state.

### 3.5 From optional QA to gate protocol

QA is a first-class state transition. QA has authority to reject, file blockers, and require rework.

## 4. Core Crate Evolution

### 4.1 `symphony-config`

Add typed v2 workflow config:

- `WorkflowConfigV2`;
- `RoleConfig`;
- `AgentProfileConfig`;
- `RoutingRule`;
- `DecompositionConfig`;
- `WorkspacePolicyConfig`;
- `BranchPolicyConfig`;
- `IntegrationConfig`;
- `QaConfig`;
- `FollowupConfig`;
- `PersistenceConfig`;
- `BudgetConfig`.

Make the target workflow schema the product direction. The loader should parse `version: 2` strictly and existing config should be changed as necessary rather than protected indefinitely.

### 4.2 `symphony-core`

Add domain modules:

```text
crates/symphony-core/src/
  work_item.rs
  role.rs
  routing.rs
  decomposition.rs
  blocker.rs
  handoff.rs
  qa.rs
  integration.rs
  followup.rs
  run_record.rs
  event.rs
  scheduler.rs
  lease.rs
```

Core owns pure state transitions and invariants. It should not call git, GitHub, Linear, Codex, Claude, or Hermes directly.

### 4.3 `symphony-state`

Add a new crate for durable persistence.

Recommended default: SQLite via `sqlx` or `rusqlite`.

The state crate owns:

- migrations;
- repository traits;
- transaction boundaries;
- event append;
- recovery queries;
- idempotency keys.

Rationale: v2 cannot rely on in-memory scheduler state. Runs, blockers, handoffs, and QA verdicts must survive restarts.

### 4.4 `symphony-tracker`

Split read and mutation traits:

```rust
#[async_trait]
pub trait TrackerRead {
    async fn fetch_candidates(&self, query: CandidateQuery) -> Result<Vec<Issue>>;
    async fn fetch_issue(&self, id: &IssueId) -> Result<Option<Issue>>;
    async fn fetch_related(&self, id: &IssueId) -> Result<IssueRelations>;
}

#[async_trait]
pub trait TrackerMutations {
    async fn create_issue(&self, req: CreateIssueRequest) -> Result<Issue>;
    async fn update_issue(&self, req: UpdateIssueRequest) -> Result<Issue>;
    async fn add_comment(&self, req: AddCommentRequest) -> Result<CommentRef>;
    async fn add_blocker(&self, req: AddBlockerRequest) -> Result<BlockerRef>;
    async fn link_parent_child(&self, req: LinkRequest) -> Result<()>;
    async fn attach_artifact(&self, req: ArtifactRequest) -> Result<()>;
}
```

If an adapter only supports read, the workflow can still run in advisory mode but cannot autonomously file blockers/follow-ups.

### 4.5 `symphony-agent`

Keep agent adapters limited to Codex, Claude, and Hermes, but move from generic prompt-only execution to structured run requests:

```rust
pub struct AgentRunRequest {
    pub work_item: WorkItem,
    pub role: RoleContext,
    pub workspace: WorkspaceClaim,
    pub prompt: RenderedPrompt,
    pub tools: ToolManifest,
    pub output_schema: OutputSchema,
}
```

All agents should return a structured handoff envelope. Free-form transcript remains evidence, not the primary API.

### 4.6 `symphony-workspace`

Evolve from `LocalFsWorkspace` into policy-driven workspace management:

- `directory` strategy;
- `git_worktree` strategy;
- `existing_worktree` strategy;
- `shared_integration_branch` strategy.

Add git operations trait:

```rust
#[async_trait]
pub trait GitProvider {
    async fn prepare_branch(&self, req: BranchRequest) -> Result<BranchClaim>;
    async fn verify_claim(&self, claim: &WorkspaceClaim) -> Result<VerificationReport>;
    async fn integrate_child(&self, req: IntegrateChildRequest) -> Result<IntegrationStep>;
    async fn open_pr(&self, req: PullRequestRequest) -> Result<PullRequestRef>;
}
```

### 4.7 `symphony-cli`

Add v2 CLI surfaces:

- `symphony validate WORKFLOW.md` — validate workflow config.
- `symphony run WORKFLOW.md` — start kernel.
- `symphony status` — summarize durable state.
- `symphony worktree verify <issue>` — debug workspace policy.
- `symphony issue graph <issue>` — print parent/child/blockers.
- `symphony qa verdict <issue>` — inspect QA evidence.
- `symphony recover` — reconcile durable state with tracker/workspaces.

## 5. Durable Data Model

Initial SQLite tables:

### 5.1 `work_items`

- `id`;
- `tracker_id`;
- `identifier`;
- `parent_id`;
- `title`;
- `status_class`;
- `tracker_status`;
- `assigned_role`;
- `assigned_agent`;
- `priority`;
- `workspace_policy` JSON;
- `branch_policy` JSON;
- `created_at`;
- `updated_at`.

### 5.2 `work_item_edges`

- `parent_id`;
- `child_id`;
- `edge_type`: `parent_child`, `blocks`, `relates_to`, `followup_of`;
- `reason`;
- `status`.

### 5.3 `runs`

- `id`;
- `work_item_id`;
- `role`;
- `agent`;
- `status`;
- `workspace_claim_id`;
- `started_at`;
- `ended_at`;
- `cost`;
- `tokens`;
- `result_summary`;
- `error`.

### 5.4 `workspace_claims`

- `id`;
- `work_item_id`;
- `path`;
- `strategy`;
- `base_ref`;
- `branch`;
- `head_sha`;
- `claim_status`;
- `verified_at`;
- `verification_report` JSON.

### 5.5 `handoffs`

- `id`;
- `run_id`;
- `work_item_id`;
- `ready_for`;
- `summary`;
- `changed_files` JSON;
- `tests_run` JSON;
- `evidence` JSON;
- `known_risks` JSON;
- `created_at`.

### 5.6 `qa_verdicts`

- `id`;
- `work_item_id`;
- `run_id`;
- `verdict`;
- `evidence` JSON;
- `acceptance_trace` JSON;
- `blockers_created` JSON;
- `created_at`.

### 5.7 `events`

Append-only event log:

- `id`;
- `sequence`;
- `event_type`;
- `work_item_id`;
- `run_id`;
- `payload` JSON;
- `created_at`.

## 6. State Machines

### 6.1 Work item state classes

```text
intake -> ready -> running -> integration -> qa -> review -> done
                  |            |          |      |
                  v            v          v      v
                blocked      rework     blocked rework
```

Transitions are workflow-policy constrained.

### 6.2 Parent issue closeout gate

A parent may enter `done` only if:

- all required child edges are terminal;
- no active blocker edges remain;
- integration record exists when required;
- QA verdict passed or waived;
- required human approval exists when configured.

### 6.3 QA loop

```text
qa_ready -> qa_running -> qa_passed -> review/done
                    \-> qa_failed -> blockers/rework -> integration/ready
```

QA failure must produce either blockers, rework instructions, or an inconclusive verdict with reason.

## 7. Scheduler Architecture

### 7.1 Queues

Use logical queues rather than one flat poll loop:

- intake/decomposition queue;
- specialist execution queue;
- integration queue;
- QA queue;
- follow-up approval queue;
- recovery/reconciliation queue.

### 7.2 Leases

Every running work item has a durable lease:

- lease owner;
- role;
- run ID;
- heartbeat timestamp;
- expiration;
- cancellation token.

On restart, expired leases become recoverable work.

### 7.3 Concurrency

Concurrency limits apply at multiple levels:

- global;
- role;
- agent backend;
- tracker/project;
- repository/workspace;
- integration branch.

Integration-owner roles should default to low concurrency, often `1`, because split-brain integration is worse than slow integration.

## 8. Prompt and Tool Contract

Agents receive:

- role instructions;
- work item details;
- parent/child context;
- blockers;
- workspace claim;
- branch policy;
- acceptance criteria;
- output schema;
- allowed tools;
- follow-up/blocker filing policy.

Agents must return a structured handoff envelope. A malformed handoff fails the run or triggers a repair turn, according to policy.

## 9. Integration Owner Protocol

The integration owner is not a generic implementer by default.

Responsibilities:

1. Decompose broad issues.
2. Assign child work to roles.
3. Maintain dependency graph truth.
4. Wait for children.
5. Consolidate branches/workspaces.
6. Resolve integration conflicts.
7. Run integration verification.
8. Request QA.
9. Close parent only after gates pass.

A workflow MAY allow the integration owner to implement directly, but it SHOULD require a written rationale on broad work.

## 10. QA Protocol

QA receives the final integrated artifact.

QA responsibilities:

1. Verify acceptance criteria one by one.
2. Inspect changed files and tests.
3. Run tests and targeted commands.
4. Exercise real runtime flows when relevant.
5. File blockers for failures.
6. File follow-ups for non-blocking discoveries.
7. Produce durable verdict and evidence.

QA must not sign off solely from implementation summaries unless workflow explicitly allows static-only QA for that issue class.

## 11. Observability Architecture

Emit `OrchestratorEventV2` for:

- work item created/updated;
- role assignment;
- decomposition proposed/accepted;
- child issue created;
- blocker created/resolved;
- run started/event/ended;
- workspace claimed/verified/failed;
- integration started/completed;
- QA verdict;
- follow-up filed/proposed;
- budget warning/exceeded;
- human approval requested/decided.

Events go to:

- durable DB table;
- structured logs;
- optional SSE stream;
- optional TUI/dashboard.

## 12. ADRs

### 2026-05-08 — v2 centers integration ownership and QA gates

Context: prior orchestration experience showed that agents can implement bounded work, but broad work fails without a strong integrator and truthful QA gate. Decomposition alone is not completion.

Decision: v2 elevates `integration_owner` and `qa_gate` to semantic role kinds. The exact org chart remains configurable, but those two behaviors are core.

Consequence: The kernel needs parent/child/blocker state, integration records, and QA verdicts as structured objects.

### 2026-05-08 — Durable state becomes mandatory

Context: the current implementation avoids persistent orchestrator state. The target product needs recovery, audit, run history, blocker/QA evidence, and parent closeout gates.

Decision: Add a persistence layer, with SQLite as the default local implementation.

Consequence: Scheduler code must become transaction-aware. Tests need schema fixtures and recovery scenarios.

### 2026-05-08 — Tracker writes are a capability, not an agent prompt trick

Context: Filing blockers/follow-ups through prompts alone makes workflow mutation unreliable and hard to audit.

Decision: Split tracker adapters into read and mutation capabilities. Workflows requiring autonomous issue creation or blockers must use mutation-capable adapters.

Consequence: Read-only trackers still work in advisory mode, but full v2 behavior requires mutation support.
