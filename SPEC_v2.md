# Symphony-RS Product Specification v2

Status: Draft v2 — product direction for shaping Symphony-RS into a specialist-agent execution system.

## 1. Purpose

Symphony-RS v2 is a headless orchestration product for turning issues into reviewed, integrated, high-confidence changes.

It is not merely a polling daemon. It is an AI operations layer that:

1. Reads or receives work from an issue tracker.
2. Classifies and decomposes broad work into specialist-owned tasks.
3. Dispatches configurable specialist agents into isolated or shared workspaces.
4. Consolidates child outputs through a single integration owner when required.
5. Gates completion behind relentless QA that can file blockers and force another loop.
6. Preserves durable evidence: runs, decisions, workspaces, branches, blockers, PRs, QA verdicts, and follow-up issues.

The core outcome is: **ship correct work, not just run agents.**

## 2. Product Principles

### 2.1 Work is coordinated, not just executed

Specialist agents are valuable because they constrain taste, scope, and verification. Symphony-RS v2 MUST support configurable specialist roles and explicit handoff contracts.

### 2.2 Integration ownership is first-class

Large or multi-specialist work MUST have a single integration owner. The integration owner is responsible for canonical branch/worktree truth, child-output reconciliation, final PR state, and whether the work is ready for QA.

The default integration-owner role name is `platform_lead`, but installations MAY rename it.

### 2.3 QA is a gate, not a courtesy

QA MUST be able to reject work, file blockers, create follow-up issues, and force rework. QA verdicts are durable workflow objects, not only comments.

The default QA role name is `qa`, but installations MAY rename it.

### 2.4 Blockers are productive work

A blocker filed by QA, an integrator, or a specialist is not a failure of the system. It is the system discovering missing work before users do.

### 2.5 Agents may create follow-up work

Every agent SHOULD have a safe pathway to propose or file follow-up issues. Workflows MAY choose whether direct creation is allowed or whether proposals require integration-owner approval.

### 2.6 Branch/workspace policy must be explicit

Every issue has an execution workspace policy. The default SHOULD be isolated per issue or per child issue. Shared branch/worktree execution is allowed only when policy explicitly declares it and the orchestrator can verify actual cwd/branch invariants.

### 2.7 Governance belongs in data, not vibes

Workflow state, dependencies, assignments, approvals, QA verdicts, and run evidence MUST be structured. Prompt instructions are not sufficient as the only source of truth.

## 3. Non-Goals

Symphony-RS is still pre-adoption software, so v2 should make the right product changes directly instead of preserving old internal shapes.

It does not aim to be:

- a generic BPM engine;
- a human project-management suite;
- a replacement for GitHub or Linear as external issue/product systems;
- a mandatory web dashboard before the headless core is correct;
- a one-size-fits-all org chart.

## 4. Core Concepts

### 4.1 Workflow

A workflow is a repo-owned contract, normally `WORKFLOW.md`, with YAML front matter and Markdown instructions.

In v2, `WORKFLOW.md` configures:

- GitHub/Linear tracker adapters;
- git source-control adapter;
- Codex/Claude/Hermes agent adapters;
- role definitions;
- routing rules;
- decomposition policy;
- workspace/branch strategy;
- integration policy;
- QA policy;
- follow-up issue policy;
- agent backends and model/tool profiles;
- observability and persistence settings.

### 4.2 Issue

An issue is the normalized unit of product work.

Required fields:

- `id`: tracker-internal stable ID.
- `identifier`: human-facing key.
- `title`.
- `description`.
- `status`: raw tracker status/state label preserved for display and sync.
- `status_class`: normalized `WorkItemStatusClass` derived from `tracker.state_mapping`.
- `priority`.
- `labels`.
- `url`.

Existing implementation names such as `Issue.state` should be migrated deliberately. Preserve the existing case-insensitive equality semantics when moving to `status`/`status_class` so tracker casing does not change behavior.

Optional structured fields:

- `parent_id`.
- `children`.
- `blocked_by`.
- `blocks`.
- `assignee_role`.
- `assignee_agent`.
- `workspace_policy`.
- `branch_policy`.
- `acceptance_criteria`.
- `qa_requirements`.
- `integration_target`.
- `pull_request`.
- `related_prs`.
- `run_summary`.

Adapters MUST NOT fabricate fields they cannot know. New fields that are not available from the external tracker should be populated by the internal `WorkItem`/state layer, not guessed by tracker adapters. Adapters MAY attach adapter-specific metadata under `metadata`.

### 4.3 Work Item

A work item is a normalized internal representation of issue work. It may correspond 1:1 with a tracker issue, a child issue created by the orchestrator, or a transient decomposition proposal pending approval.

Work item states are workflow-defined, but the orchestrator requires normalized classes:

- `ignore`: ignore.
- `intake`: eligible for triage/decomposition.
- `ready`: eligible for specialist dispatch.
- `running`: actively owned by a run.
- `blocked`: waiting on dependencies or human decision.
- `integration`: ready for consolidation.
- `qa`: ready for QA gate.
- `rework`: failed QA or integration and needs more work.
- `review`: human review or approval.
- `done`: accepted by workflow gates.
- `cancelled`: intentionally abandoned.

### 4.4 Role

A role is a configurable specialist capability profile.

Required role fields:

```yaml
roles:
  platform_lead:
    kind: integration_owner
    description: Owns decomposition, integration branch, and final release handoff.
    agent: lead_agent
    max_concurrent: 1
  qa:
    kind: qa_gate
    description: Verifies acceptance criteria, files blockers, and rejects incomplete work.
    agent: qa_agent
    max_concurrent: 2
  backend_engineer:
    kind: specialist
    description: Implements scoped backend changes.
    agent: codex_fast
```

Role kinds:

- `integration_owner`: may decompose, assign, merge child outputs, request QA, and close parent work.
- `qa_gate`: may verify, reject, file blockers, and create follow-up issues.
- `specialist`: implements scoped work and reports evidence.
- `reviewer`: reviews design/security/performance/content without owning implementation.
- `operator`: performs runtime/deployment/data tasks.
- `custom`: workflow-defined behavior, not a new adapter type.

Only `integration_owner` and `qa_gate` are semantically special in core v2. All other roles are configurable.

### 4.5 Agent

An agent is an executable backend plus runtime policy.

Supported agent backend classes are deliberately limited to:

- `codex`;
- `claude`;
- `hermes`.

Agent config MAY define:

- command/backend;
- model configuration;
- system prompt or profile;
- tools/toolsets;
- environment;
- sandbox/approval policy;
- memory policy;
- timeout/stall policy;
- max turns;
- token/cost budget.

### 4.6 Run

A run is one attempt by one agent or composite agent to perform work.

Runs MUST be durable. A run record SHOULD include:

- run ID;
- work item ID;
- role and agent;
- workspace path;
- branch/head/base refs;
- start/end timestamps;
- exit status;
- cancellation reason;
- token/cost metrics when available;
- emitted events or event-log pointer;
- structured result;
- artifacts/evidence links.

### 4.7 Handoff

A handoff is structured output from a run. It MUST be machine-readable enough for downstream agents.

Minimum handoff fields:

- `summary`;
- `changed_files`;
- `tests_run`;
- `verification_evidence`;
- `known_risks`;
- `blockers_created`;
- `followups_created_or_proposed`;
- `branch_or_workspace`;
- `ready_for` (`integration`, `qa`, `human_review`, `blocked`, `done`).

`ready_for` is advisory from the agent, not authority to bypass gates:

- `integration`: enqueue for integration-owner consolidation.
- `qa`: enqueue for QA only if integration is not required or already complete.
- `human_review`: pause with a durable approval item.
- `blocked`: require blocker records or a structured block reason.
- `done`: valid only for simple non-decomposed work with no integration/QA requirement; otherwise the kernel downgrades it to the next required gate.

### 4.8 Blocker

A blocker is a durable dependency edge plus a reason.

Required fields:

- blocking item ID;
- blocked item ID;
- reason;
- created by run/agent/human;
- severity;
- status.

QA-created blockers MUST prevent parent completion until resolved or explicitly waived by policy.

### 4.9 QA Verdict

A QA verdict is a durable gate result.

Verdicts:

- `passed`;
- `failed_with_blockers`;
- `failed_needs_rework`;
- `inconclusive`;
- `waived`.

A QA verdict MUST include evidence. For UI/UX/TUI work, evidence SHOULD include actual rendered output, screenshots, terminal recordings, harness logs, or an explicit workflow-approved limitation.

### 4.10 Follow-up Issue

A follow-up issue is newly discovered work outside the current acceptance criteria or safely separable from the current branch.

Agents MAY:

- create directly, if policy allows;
- propose for integration-owner approval;
- attach to current issue as non-blocking follow-up;
- attach as blocking if it invalidates acceptance.

## 5. WORKFLOW.md Schema

Top-level keys:

```yaml
schema_version: 1
tracker: {}
polling: {}
persistence: {}
roles: {}
agents: {}
routing: {}
decomposition: {}
workspace: {}
branching: {}
integration: {}
pull_requests: {}
qa: {}
followups: {}
observability: {}
budgets: {}
security: {}
hooks: {}
```

Unknown top-level and nested keys MUST be rejected. This software is new; strictness is cheaper than ambiguity.

### 5.0 `schema_version`

```yaml
schema_version: 1
```

`schema_version` is optional while the project is pre-adoption. If omitted, it defaults to `1`. If present, it MUST be `1`; any other value is a validation error. Future schema changes should bump this field only when the file format needs an explicit migration. Do not create parallel numbered Rust config types for normal product evolution.

### 5.1 `tracker`

```yaml
tracker:
  kind: github | linear
  repository: owner/repo
  project_slug: ENG
  state_mapping:
    ignore: [Archived]
    intake: [Backlog, Todo]
    ready: [Ready]
    running: [In Progress]
    blocked: [Blocked]
    integration: [Integration]
    qa: [QA, In Review]
    rework: [Rework]
    review: [Review]
    done: [Done]
    cancelled: [Cancelled]
```

Tracker adapters MUST support read operations. Workflows that create child issues, blockers, follow-ups, state transitions, comments, or PR links MUST use mutation-capable tracker adapters.

Tracker state mapping rules:

- Raw tracker state names are matched case-insensitively and normalized into `WorkItemStatusClass`.
- A raw state MUST appear in at most one normalized class list.
- Unknown raw states are validation errors unless `tracker.unknown_state_policy` is explicitly set to `ignore`.
- `active` is not a configured class. It is derived as every non-terminal class that can still produce work: `intake`, `ready`, `running`, `blocked`, `integration`, `qa`, `rework`, and `review`.
- Terminal classes are `done` and `cancelled`.
- Startup recovery uses both active classes and recent terminal issues so stale leases/workspaces can be reconciled after tracker-side changes.

### 5.2 `polling`

```yaml
polling:
  interval_ms: 30000
  jitter_ms: 5000
  startup_reconcile_recent_terminal: true
```

The kernel MAY later add webhook intake, but polling remains the baseline operator-controlled cadence. `interval_ms` controls tracker polling and reconciliation cadence. `jitter_ms` prevents thundering-herd behavior when multiple kernels run.

### 5.3 `persistence`

```yaml
persistence:
  kind: sqlite
  path: .symphony/state.db
  event_log: .symphony/events.ndjson
```

SQLite is mandatory for the local kernel. Use `rusqlite` for the first implementation: the kernel is local/single-process, transaction boundaries are explicit, and avoiding async SQL macro/offline setup keeps the execution substrate simpler. If later workloads require async pooled database access, that should be a deliberate ADR.

SQLite stores orchestration truth: runs, leases, workspace claims, handoffs, QA verdicts, blocker edges, pending tracker syncs, budget pauses, and event history. The issue tracker remains the shared product/work truth and must be synced through tracker mutations.

### 5.4 `roles`

Roles are named, configurable, and workflow-owned. `platform_lead` and `qa` are recommended defaults, not required names.

Validation is by role kind, not by role name:

- exactly one role with `kind: integration_owner` is required;
- exactly one role with `kind: qa_gate` is required when `qa.required: true`;
- specialists/reviewers/operators are workflow-defined.

```yaml
roles:
  platform_lead:
    kind: integration_owner
    agent: lead_agent
    max_concurrent: 1
    can_decompose: true
    can_assign: true
    can_request_qa: true
    can_close_parent: true
  qa:
    kind: qa_gate
    agent: qa_agent
    max_concurrent: 2
    can_file_blockers: true
    can_file_followups: true
    required_for_done: true
  backend_engineer:
    kind: specialist
    agent: codex_fast
```

### 5.5 `agents`

```yaml
agents:
  lead_agent:
    backend: codex
    command: codex app-server
    model: configured-by-backend
    tools: [git, github, tracker]
    memory: persistent
    approval_policy: workflow-default
  qa_agent:
    backend: claude
    command: claude -p --output-format stream-json --permission-mode bypassPermissions
    tools: [git, github, tracker]
  codex_fast:
    backend: codex
    command: codex app-server
    tools: [git, github, tracker]
  hermes_agent:
    backend: hermes
    command: hermes chat --query-file - --source symphony
    tools: [git, github, tracker]
```

Product agent backends are limited to `codex`, `claude`, and `hermes`.

`hermes` runs the Hermes Agent CLI in non-interactive mode. The first adapter should stream a rendered prompt through stdin or a temporary prompt file, run with an explicit source tag such as `symphony`, and require a structured handoff envelope in the final response. If Hermes later exposes a stable ACP/server protocol, the adapter may switch to it behind the same `backend: hermes` contract.

Composite/tandem behavior is not a separate product adapter. Existing tandem code should be repackaged as an agent strategy that composes configured `codex`, `claude`, and/or `hermes` agents:

```yaml
agents:
  lead_pair:
    strategy: tandem
    lead: lead_agent
    follower: qa_agent
    mode: draft_review | split_implement | consensus
```

Existing mock tracker/agent code survives only as deterministic test/fake machinery. It should not appear as a production `kind` in operator-facing workflow docs.

### 5.6 `routing`

Routing decides which role receives work.

```yaml
routing:
  default_role: platform_lead
  match_mode: first_match # first_match | priority
  rules:
    - priority: 100
      when:
        labels_any: [qa]
      assign_role: qa
    - priority: 50
      when:
        paths_any: [lib/**, test/**]
      assign_role: backend_engineer
    - priority: 10
      when:
        issue_size: broad
      assign_role: platform_lead
```

Routing rules MUST be deterministic. `match_mode: first_match` evaluates rules in file order and ignores `priority`. `match_mode: priority` picks the highest numeric `priority`; ties are validation errors.

### 5.7 `decomposition`

```yaml
decomposition:
  enabled: true
  owner_role: platform_lead
  triggers:
    labels_any: [epic, broad, umbrella]
    estimated_files_over: 5
    acceptance_items_over: 3
  child_issue_policy: create_directly | propose_for_approval
  max_depth: 2
  require_acceptance_criteria_per_child: true
```

Broad issues SHOULD be decomposed before specialist execution. Decomposition output MUST include child scopes, owners/roles, dependencies, acceptance criteria, and integration strategy.

`child_issue_policy` uses the same policy enum as `followups.default_policy`. `create_directly` writes through `TrackerMutations`; `propose_for_approval` creates a durable approval item for the integration owner/human approval queue.

### 5.8 `workspace`

```yaml
workspace:
  root: .symphony/workspaces
  default_strategy: issue_worktree
  strategies:
    issue_worktree:
      kind: git_worktree
      base: main
      branch_template: symphony/{{identifier}}
      cleanup: retain_until_done
    shared_integration:
      kind: existing_worktree
      path: ../project-integration
      require_branch: symphony/integration/{{parent_identifier}}
  require_cwd_in_workspace: true
  forbid_untracked_outside_workspace: true
```

The runner MUST verify actual cwd before launching any agent. If branch policy is configured, the runner MUST verify actual git branch/ref before mutation-capable runs.

Existing `Workspace`/`WorkspaceManager::ensure` should evolve into `WorkspaceClaim` and `WorkspaceManager::claim`. A claim records path, strategy, base ref, branch/ref, owner work item, verification status, and cleanup policy. The existing hook lifecycle remains, but hooks receive claim metadata rather than just a path.

### 5.9 `branching`

```yaml
branching:
  default_base: main
  child_branch_template: symphony/{{identifier}}
  integration_branch_template: symphony/integration/{{identifier}}
  allow_same_branch_for_children: false
  require_clean_tree_before_run: true
```

For decomposed work, children normally run in separate branches/worktrees and are consolidated by the integration owner. Same-branch child work is allowed only when both conditions are true:

1. `branching.allow_same_branch_for_children: true`;
2. the selected workspace strategy is explicitly a shared strategy such as `shared_integration`.

If those disagree, validation MUST fail. The safer setting wins by rejecting the config, not by guessing.

### 5.10 `integration`

```yaml
integration:
  owner_role: platform_lead
  required_for:
    - decomposed_parent
    - multiple_child_branches
  merge_strategy: sequential_cherry_pick | merge_commits | shared_branch
  conflict_policy: integration_owner_repair_turn | route_to_child_owner | block_for_human
  require_all_children_terminal: true
  require_no_open_blockers: true
  require_integration_summary: true
  next_state_after_integration: qa
```

Integration owner responsibilities:

- confirm every child is done or intentionally waived;
- consolidate changes into the canonical integration branch/worktree;
- resolve conflicts;
- run integration tests;
- open or refresh the draft PR when PR policy requires it;
- produce handoff for QA;
- request QA only when ready.

Merge/cherry-pick failures follow `conflict_policy`. The default is `integration_owner_repair_turn`: the integration owner gets a repair turn in the canonical integration workspace. If that repair fails, the work item becomes blocked with conflict evidence.

### 5.11 `pull_requests`

```yaml
pull_requests:
  enabled: true
  owner_role: platform_lead
  provider: github
  open_stage: after_integration_verification
  initial_state: draft
  mark_ready_stage: after_qa_passes
  title_template: "{{identifier}}: {{title}}"
  body_template: |
    ## Summary
    {{integration.summary}}

    ## QA
    {{qa.status}}

    ## Linked work
    {{links}}
  link_tracker_issues: true
  require_ci_green_before_ready: true
```

PR lifecycle rules:

- Specialists do not open PRs by default.
- The integration owner owns PR creation and final PR state.
- `git` prepares and pushes the canonical branch; GitHub opens, updates, marks ready, and reports checks for the PR.
- The default PR is a draft opened after integration verification passes.
- QA runs against the draft PR branch and records CI/check status as evidence.
- QA blockers keep the PR draft/blocked until resolved.
- After QA passes, the integration owner marks the PR ready for review or hands it off according to workflow policy.
- The tracker is updated with the PR URL/ref, and follow-up/blocker issues should link back to the PR when relevant.

### 5.12 `qa`

```yaml
qa:
  owner_role: qa
  required: true
  exhaustive: true
  allow_static_only: false
  can_file_blockers: true
  can_file_followups: true
  blocker_policy: blocks_parent
  waiver_roles: [platform_lead]
  evidence_required:
    tests: true
    changed_files_review: true
    acceptance_criteria_trace: true
    visual_or_runtime_evidence_when_applicable: true
  rerun_after_blockers_resolved: true
```

QA MUST verify acceptance criteria against the final integration branch/worktree and draft PR when PRs are enabled, not only child branches. QA MAY file blockers that route back to specialists or the integration owner. A `waived` QA verdict is valid only when created by a role listed in `qa.waiver_roles`, and it MUST include a reason.

### 5.13 `followups`

```yaml
followups:
  enabled: true
  default_policy: create_directly | propose_for_approval
  approval_role: platform_lead
  non_blocking_label: follow-up
  blocking_label: blocker
  require_reason: true
  require_acceptance_criteria: true
```

A follow-up MUST include title, reason, scope, acceptance criteria, relationship to current work, and whether it blocks current completion.

### 5.14 `observability`

```yaml
observability:
  logs:
    format: json # json | pretty
  event_bus: true
  sse:
    enabled: true
    bind: 127.0.0.1:6280
    replay_buffer: 1024
  tui:
    enabled: true
  dashboard:
    enabled: false
```

The kernel remains headless: it runs without requiring a TUI or web dashboard. TUI/dashboard surfaces are optional out-of-process operator views over durable state and/or the event stream.

`status` config from the current implementation should move under `observability.sse`, preserving `bind` and `replay_buffer`.

### 5.15 `budgets`

```yaml
budgets:
  max_parallel_runs: 6
  max_cost_per_issue_usd: 10.00
  max_turns_per_run: 20
  max_retries: 3
  pause_policy: block_work_item
```

Budget exhaustion MUST create a durable budget pause/block and emit a `BudgetExceeded` event. It MUST NOT silently fail or spin retries. The state store needs enough information to resume after a human/operator changes budget policy or explicitly approves continuation.

### 5.16 `security`

```yaml
security:
  destructive_actions_require_approval: true
  publish_purchase_deploy_require_human: true
  network_policy: workflow_defined
  secret_redaction: true
```

### 5.17 `hooks`

```yaml
hooks:
  timeout_ms: 300000
  after_create: []
  before_run: []
  after_run: []
  before_remove: []
```

Keep the current hook lifecycle and extend it to receive `WorkspaceClaim` metadata through environment variables and/or a small JSON file. Hook failures are governed by the existing phase semantics: `after_create` and `before_run` failures block launch; `after_run` and `before_remove` are best-effort unless the workflow explicitly makes them blocking.

## 6. Orchestration Flow

### 6.1 Intake

1. Poll tracker or receive webhook.
2. Normalize issue into a work item.
3. Apply routing rules.
4. If broad, route to integration owner for decomposition.
5. If scoped, route to specialist.

### 6.2 Decomposition

1. Integration owner reads issue and repo context.
2. Produces child issue plan.
3. Creates or proposes child issues.
4. Sets dependency graph.
5. Parent remains blocked until children complete and integration/QA gates pass.

### 6.3 Specialist Execution

1. Specialist receives one scoped child issue.
2. Workspace manager prepares isolated workspace/branch.
3. Agent implements and verifies.
4. Agent writes structured handoff.
5. Agent may file blockers/follow-ups per policy.
6. Child moves to integration-ready or blocked/rework.

### 6.4 Integration

1. Integration owner waits for all required children.
2. Consolidates changes into canonical integration branch/worktree.
3. Runs integration verification.
4. Resolves conflicts and regressions.
5. Opens or refreshes the draft PR when PR policy requires it.
6. Produces integration handoff.
7. Requests QA.

### 6.5 QA

1. QA runs against final integration branch/worktree.
2. QA traces every acceptance criterion.
3. QA reviews tests and changed files.
4. QA exercises runtime/visual/user-flow paths when relevant.
5. QA verdict is recorded.
6. If blockers exist, they are filed and parent returns to blocked/rework.
7. If passed, issue can move to review/done according to workflow.

### 6.6 Follow-up Creation

Any role may identify follow-up work. The workflow decides whether the issue is created immediately or proposed.

A follow-up MUST include:

- title;
- reason;
- scope;
- acceptance criteria;
- relationship to current work;
- whether it blocks current completion.

## 7. Required Adapter Capabilities

The supported adapter set is limited to: `git`, `github`, `linear`, `codex`, `claude`, and `hermes`. Tests may use fakes, but fakes are not product adapters.

### 7.1 TrackerRead

- fetch active issues;
- fetch issue by ID;
- fetch related issues/blockers;
- fetch comments/activity if available;
- fetch issue state by IDs;
- fetch terminal/recent issues.

### 7.2 TrackerMutations

Required for autonomous v2 workflows:

- create issue;
- update status;
- assign role/agent or equivalent labels;
- add comment;
- create blocker/dependency;
- link parent/child;
- attach PR/artifact/evidence.

If a tracker lacks mutation support, workflow MUST run in advisory/proposal mode.

### 7.3 Git

The supported source-control adapter is `git`.

- create branch/worktree;
- verify cwd and branch;
- diff/status;
- merge/cherry-pick/rebase if policy allows;
- push branches for PR creation;
- expose branch/ref data to the GitHub adapter for PR creation.

### 7.4 AgentRuntime

- start session;
- stream events;
- abort;
- continue turn if supported;
- report usage;
- report structured final result.

## 8. Safety and Guardrails

### 8.1 Completion rules

A parent issue MUST NOT be marked done until:

- all required children are done or waived;
- no blocking blockers remain;
- integration owner produced integration handoff when required;
- QA passed or was explicitly waived;
- final branch/PR state is recorded;
- required human approval, if configured, is complete.

### 8.2 QA blocker rules

QA-created blockers block parent completion by default. Only configured waiver roles may waive them, and waiver requires a reason.

### 8.3 Workspace safety

Before mutation-capable agent launch, runner MUST verify:

- workspace path is under configured root;
- process cwd equals expected workspace;
- git branch/ref matches policy;
- tree cleanliness matches policy;
- no configured protected paths are writable unless allowed.

### 8.4 Human approval

Publishing, purchasing, externally posting, destructive irreversible actions, and production deployment require explicit human approval unless a workflow marks them as safe and reversible.

## 9. Observability

Symphony-RS v2 MUST expose enough state to answer:

- What is running?
- Who owns each issue?
- What is blocked and why?
- What branch/workspace contains the canonical work?
- What did each agent do?
- What evidence supports completion?
- What did QA reject?
- What follow-ups were filed?
- What cost/time/token budget was used?

Minimum surfaces:

- structured logs;
- durable event log;
- `status` CLI;
- JSON API or equivalent;
- optional live TUI/dashboard.

## 10. Product Posture

Symphony-RS v2 should absorb the operating lessons learned from prior orchestration work while building directly on this codebase.

Preserve these lessons:

- bounded specialist work beats generalist sprawl;
- ambiguity must be decomposed before execution;
- integration ownership is the serialization point;
- worktree isolation is non-negotiable unless explicitly waived;
- blocker graph is core workflow data;
- QA must have authority to reject and file work;
- liveness/cost/observability are product features;
- follow-up issue creation is part of agent literacy.

Do not hardcode:

- a fixed org chart beyond semantic role kinds;
- Foglet-specific workflows;
- prior-system-specific table/state names;
- adapters beyond git, GitHub, Linear, Codex, Claude, and Hermes;
- one branch strategy.
