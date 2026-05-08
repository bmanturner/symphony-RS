---
# Sample WORKFLOW.md fixture for Symphony-RS v2.
#
# This file is the worked example shipped with the repo. It exercises
# every top-level section of the v2 schema (SPEC_v2 §5) so a reviewer
# can read it as a reference for a complete, production-shaped
# workflow. The matching typed `WorkflowConfig` lands in Phase 1;
# until then the v1 loader cannot parse this file and the
# fixture-shaped integration tests are gated `#[ignore]` with a
# pointer back to this fixture.
#
# Anything marked "$VAR" is a placeholder. Layered loading expands
# these from the environment; tests that care about credential
# resolution set the relevant `SYMPHONY_*` variables explicitly and
# never rely on a developer's shell.

schema_version: 1

tracker:
  kind: linear
  project_slug: ENG
  api_key: $SYMPHONY_TRACKER_API_KEY
  unknown_state_policy: error
  state_mapping:
    ignore: [Archived, Duplicate]
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

polling:
  interval_ms: 30000
  jitter_ms: 5000
  startup_reconcile_recent_terminal: true

persistence:
  kind: sqlite
  path: .symphony/state.db
  event_log: .symphony/events.ndjson

roles:
  platform_lead:
    kind: integration_owner
    description: Owns decomposition, integration branch, and final PR/QA handoff.
    agent: lead_agent
    max_concurrent: 1
    can_decompose: true
    can_assign: true
    can_request_qa: true
    can_close_parent: true
  qa:
    kind: qa_gate
    description: Verifies acceptance criteria, files blockers, rejects incomplete work.
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
    description: Implements scoped UI/TUI changes.
    agent: claude_fast
  reviewer:
    kind: reviewer
    description: Optional design/security review for sensitive changes.
    agent: qa_agent

agents:
  lead_agent:
    backend: codex
    command: codex app-server
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
  claude_fast:
    backend: claude
    command: claude -p --output-format stream-json --permission-mode bypassPermissions
    tools: [git, github, tracker]
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
  default_role: platform_lead
  match_mode: priority
  rules:
    - priority: 100
      when:
        labels_any: [qa]
      assign_role: qa
    - priority: 80
      when:
        labels_any: [epic, broad, umbrella]
      assign_role: platform_lead
    - priority: 60
      when:
        paths_any: [crates/**, tests/**]
      assign_role: backend_engineer
    - priority: 50
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
  child_issue_policy: create_directly
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
    shared_integration:
      kind: existing_worktree
      path: ../rust-symphony-integration
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

budgets:
  max_parallel_runs: 6
  max_cost_per_issue_usd: 10.00
  max_turns_per_run: 20
  max_retries: 3
  pause_policy: block_work_item

security:
  destructive_actions_require_approval: true
  publish_purchase_deploy_require_human: true
  network_policy: workflow_defined
  secret_redaction: true

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

# Symphony Sample Workflow

You are an autonomous coding agent dispatched by Symphony-RS v2.
Your role, scoped issue, workspace claim, parent/child context,
acceptance criteria, blockers, and structured output schema are
provided in the run context.

## Working agreement

- Stay strictly in scope. Specialists implement one child issue; the
  integration owner consolidates across children on the canonical
  integration branch.
- Never disable or skip a failing test to make CI pass. File a
  blocker or follow-up instead.
- If acceptance is ambiguous, file a blocker with a structured
  reason and stop. Do not guess.
- Verify cwd and branch before any mutation-capable action. If the
  workspace claim disagrees with reality, abort and emit a blocker.

## Output

Emit a structured handoff containing `summary`, `changed_files`,
`tests_run`, `verification_evidence`, `known_risks`,
`blockers_created`, `followups_created_or_proposed`,
`branch_or_workspace`, and `ready_for`. `ready_for` is advisory; the
kernel decides the next gate.
