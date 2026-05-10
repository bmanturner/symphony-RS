# Workflow Configuration

`WORKFLOW.md` is the repo-owned contract that tells Symphony-RS v2 how to turn tracker work into agent runs, integration, QA, blockers, follow-ups, and durable evidence.

The file has YAML front matter followed by a Markdown prompt body:

```text
---
schema_version: 1
tracker:
  kind: github
  repository: owner/repo
---

# Agent Instructions

The Markdown body becomes the base prompt for dispatched agents.
```

Unknown YAML keys are rejected. The schema is intentionally strict so typos do not silently weaken workflow gates.

For migration guidance from the original Symphony-RS fixture and planning shape, see `docs/upgrade.md`.

For tracker-backed child dependency orchestration, see the v3 addendum in
`SPEC_v3.md`. v3 extends `decomposition` with dependency policy and durable
`blocks` edges; it does not replace the v2 workflow schema or gate semantics.

For role instruction packs and generated platform-lead assignment
catalogs, see `SPEC_v4.md`. v4 adds prompt-side role doctrine and
catalog rendering without redefining v2 roles or v3 dependency behavior.

## Minimal Valid Shape

A useful workflow normally declares roles, agents, routing, workspace policy, integration, QA, and follow-up policy. For validation only, a minimal Linear workflow can be as small as:

```yaml
schema_version: 1
tracker:
  kind: linear
  project_slug: ENG
```

Production workflows should use the fuller shape below.

## Complete Example

The example between the markers is loaded and validated by the test suite.

<!-- workflow-example:start -->
```markdown
---
schema_version: 1

tracker:
  kind: github
  repository: example/symphony-product
  api_key: $SYMPHONY_TRACKER_API_KEY
  active_states: [Todo, In Progress]
  terminal_states: [Done, Cancelled, Closed, Duplicate]

polling:
  interval_ms: 30000
  jitter_ms: 5000
  startup_reconcile_recent_terminal: true
  lease:
    default_ttl_ms: 60000
    renewal_margin_ms: 15000

roles:
  platform_lead:
    kind: integration_owner
    description: Owns decomposition, canonical integration branch, PR, and parent closeout.
    agent: lead_agent
    max_concurrent: 1
    can_decompose: true
    can_assign: true
    can_request_qa: true
    can_close_parent: true
  qa:
    kind: qa_gate
    description: Verifies acceptance criteria, files blockers, and rejects incomplete work.
    agent: qa_agent
    max_concurrent: 2
    can_file_blockers: true
    can_file_followups: true
    required_for_done: true
  backend_engineer:
    kind: specialist
    description: Implements scoped backend changes.
    agent: codex_fast
  frontend_engineer:
    kind: specialist
    description: Implements scoped TUI and CLI UX changes.
    agent: claude_fast
  reviewer:
    kind: reviewer
    description: Reviews security, design, or performance-sensitive changes.
    agent: qa_agent

agents:
  lead_agent:
    backend: codex
    command: codex app-server
    tools: [git, github, tracker]
    memory: persistent
    max_turns: 12
  qa_agent:
    backend: claude
    command: claude -p --output-format stream-json --permission-mode bypassPermissions
    tools: [git, github, tracker]
    max_turns: 8
  codex_fast:
    backend: codex
    command: codex app-server
    tools: [git, github, tracker]
    max_turns: 6
  claude_fast:
    backend: claude
    command: claude -p --output-format stream-json --permission-mode bypassPermissions
    tools: [git, github, tracker]
    max_turns: 6
  hermes_agent:
    backend: hermes
    command: hermes chat --query-file - --source symphony
    tools: [git, github, tracker]
  lead_pair:
    strategy: tandem
    lead: lead_agent
    follower: qa_agent
    mode: draft_review

routing:
  default_role: backend_engineer
  match_mode: priority
  rules:
    - priority: 100
      when:
        labels_any: [qa]
      assign_role: qa
    - priority: 90
      when:
        labels_any: [epic, broad, umbrella]
      assign_role: platform_lead
    - priority: 70
      when:
        paths_any: [crates/**, tests/**]
      assign_role: backend_engineer
    - priority: 60
      when:
        paths_any: [crates/symphony-cli/src/tui/**]
      assign_role: frontend_engineer

decomposition:
  enabled: true
  owner_role: platform_lead
  triggers:
    labels_any: [epic, broad, umbrella]
    estimated_files_over: 5
    acceptance_items_over: 3
  # GitHub Issues has no structural parent/child links; approval mode
  # keeps parent links advisory/local-only instead of pretending they
  # are tracker-native.
  child_issue_policy: propose_for_approval
  dependency_policy:
    materialize_edges: true
    tracker_sync: best_effort
    dispatch_gate: true
    auto_resolve_on_terminal: true
  max_depth: 2
  require_acceptance_criteria_per_child: true

workspace:
  root: ./.symphony/workspaces
  default_strategy: issue_worktree
  strategies:
    issue_worktree:
      kind: git_worktree
      base: main
      branch_template: symphony/{{identifier}}
      cleanup: retain_until_done
    integration_worktree:
      kind: existing_worktree
      path: ../symphony-product-integration
      require_branch: symphony/integration/{{parent_identifier}}
  require_cwd_in_workspace: true
  forbid_untracked_outside_workspace: true

branching:
  default_base: main
  child_branch_template: symphony/{{identifier}}
  integration_branch_template: symphony/integration/{{identifier}}
  allow_same_branch_for_children: false
  require_clean_tree_before_run: true

integration:
  owner_role: platform_lead
  required_for:
    - decomposed_parent
    - multiple_child_branches
  merge_strategy: sequential_cherry_pick
  conflict_policy: integration_owner_repair_turn
  require_all_children_terminal: true
  require_no_open_blockers: true
  require_integration_summary: true
  next_state_after_integration: qa

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

followups:
  enabled: true
  default_policy: propose_for_approval
  approval_role: platform_lead
  non_blocking_label: follow-up
  blocking_label: blocker
  require_reason: true
  require_acceptance_criteria: true

observability:
  logs:
    format: pretty
  event_bus: true
  sse:
    enabled: true
    bind: 127.0.0.1:6280
    replay_buffer: 1024
  tui:
    enabled: true
  dashboard:
    enabled: false

concurrency:
  global_max: 6
  per_agent_profile:
    qa_agent: 1
  per_repository:
    example/symphony-product: 4

budgets:
  max_parallel_runs: 6
  max_cost_per_issue_usd: 10.0
  max_turns_per_run: 20
  max_retries: 3
  pause_policy: block_work_item

hooks:
  timeout_ms: 300000
  after_create:
    - git config user.email "symphony@example.com"
    - git config user.name "Symphony"
  before_run:
    - git status --short
  after_run:
    - git status --short
  before_remove: []
---

# Symphony Agent Instructions

Work from the structured run context. Specialists own scoped child work, the integration owner owns the canonical integration branch and parent closeout, and QA owns the gate.
```
<!-- workflow-example:end -->

## Top-Level Blocks

`schema_version` accepts only `1`. Omit it only for temporary local experiments; checked-in workflows should write it explicitly.

`tracker` selects the source of work. Supported `kind` values are `github`, `linear`, and test-only `mock`. GitHub requires `repository`; Linear requires `project_slug`; mock requires `fixtures`. `active_states` and `terminal_states` are compared case-insensitively and must not overlap.

`polling` controls scheduler cadence and startup recovery. Keep `startup_reconcile_recent_terminal: true` unless you have an external recovery process, because terminal-recent reconciliation is how stale leases and workspaces are noticed after tracker-side changes.

`roles` is the workflow org chart. Role names are configurable, but two semantic kinds matter to the kernel: `integration_owner` and `qa_gate`. Other supported kinds are `specialist`, `reviewer`, `operator`, and `custom`.

`agents` defines backend profiles independent from role names. Product backends are `codex`, `claude`, and `hermes`; `mock` is for tests and fixtures. Composite `strategy: tandem` profiles wrap two concrete profiles and can run `draft_review`, `split_implement`, or `consensus`.

`routing` maps normalized work items to roles. `first_match` uses file order. `priority` chooses the matching rule with the highest `priority`.

The platform-lead assignment catalog is generated from `WORKFLOW.md`
roles, agents, global `routing.rules`, and role-local assignment
metadata. Do not create parallel per-role `ASSIGNMENT.md` files for
normal operation; they drift from the routing contract and are not the
catalog source of truth.

Prompt issue context is intentionally bounded. Every issue prompt can
see `identifier`, `title`, `description`, `state`, `labels`,
`priority`, `created_at`, `updated_at`, `branch_name`, `url`, and a
compact `blocked_by` summary with count plus identifiers. Comments,
assignee/reporter details, attachments, full blocker chains, and other
tracker-native fields are deferred context; agents should fetch them
on demand through the v5 MCP read tools instead of expecting them in
the durable prompt.

`decomposition` controls when broad issues become child issues. The `owner_role` must be an `integration_owner` when decomposition is enabled. `child_issue_policy` is either `create_directly` or `propose_for_approval`.

`decomposition.dependency_policy` controls how `ChildProposal.depends_on` becomes durable sequencing. Defaults are:

```yaml
dependency_policy:
  materialize_edges: true
  tracker_sync: best_effort
  dispatch_gate: true
  auto_resolve_on_terminal: true
```

Edge direction is always prerequisite to dependent. If a proposal says `api depends_on: [schema]`, Symphony stores an open `blocks` edge with `parent_id = schema_work_item_id`, `child_id = api_work_item_id`, `source = decomposition`, and reason similar to `api depends on schema`. While that edge is open, the specialist queue parks `api`; root children with no incoming open blockers can dispatch in parallel. When `schema` reaches a terminal status class and tracker state does not contradict local state, reconciliation resolves the decomposition edge and `api` becomes eligible on the next tick.

`tracker_sync` accepts `required`, `best_effort`, or `local_only`. `required` fails workflow validation unless the tracker advertises structural blocker support. `best_effort` keeps local Symphony blockers authoritative and records failed tracker syncs as retryable state. `local_only` skips tracker blocker mutation entirely and uses the durable local graph as the sequencing source.

Linear can create native blocker relations, so Linear workflows may use `tracker_sync: required` when native tracker enforcement is desired. Standard GitHub Issues does not have structural parent/child or blocker edges in this implementation; labels, body text, and comments are advisory only. GitHub workflows that need sequencing should use `best_effort` or `local_only`, with Symphony's local `work_item_edges` enforcing dispatch order.

Operators can inspect dependency state with:

```bash
cargo run -p symphony-cli -- issue graph <work_item_id> --state-db symphony.sqlite3
cargo run -p symphony-cli -- issue graph <work_item_id> --state-db symphony.sqlite3 --json
```

For each blocker edge, `issue graph` shows the peer issue, reason, source, local edge status, tracker sync status, and next action. Failed dependency syncs surface as `tracker_sync_status = failed` with `next_action = retry_tracker_sync`; local-only edges show `next_action = wait_for_local_resolution`.

`workspace` declares named workspace strategies. `git_worktree` creates isolated worktrees. `existing_worktree` reuses a known path and verifies its branch. `shared_branch` is only legal when `branching.allow_same_branch_for_children` is true.

`branching` owns branch templates and pre-launch clean-tree policy. Both child and integration templates must include `{{identifier}}`.

`integration` is the single-owner consolidation gate. Use it for decomposed parents and multi-branch work. The default safety posture requires terminal children, no open blockers, and a structured integration summary before QA or PR promotion.

`pull_requests` gives PR ownership to the integration owner. GitHub is the supported provider. The recommended flow is draft after integration verification, then ready after QA passes.

`qa` is the gate. When `required: true`, `owner_role` must be a `qa_gate`. QA blockers should normally use `blocker_policy: blocks_parent`; waivers require a configured waiver role and a reason on the verdict.

`followups` lets agents surface adjacent work without hiding it in prose. `create_directly` writes through tracker mutations; `propose_for_approval` queues the request for the configured approval role.

`observability` controls logs, durable event broadcasting, SSE, TUI, and dashboard surfaces.

`concurrency` caps work globally, by agent profile, and by repository. Per-role caps live on each role's `max_concurrent`.

`budgets` defines hard ceilings for parallel runs, cost, turns, retries, and whether budget exhaustion blocks the whole work item or only the offending run.

`hooks` runs shell snippets around workspace lifecycle events. Hooks execute in the workspace and should be deterministic; non-zero `before_run` hooks abort the attempt.

## Validation Notes

Run:

```bash
cargo run -p symphony-cli -- validate path/to/WORKFLOW.md
```

Common validation failures include missing owner roles, unknown role or agent references, invalid branch templates, PRs enabled with a non-GitHub tracker, overlapping tracker states, contradictory shared-branch settings, and zero concurrency or budget caps.
