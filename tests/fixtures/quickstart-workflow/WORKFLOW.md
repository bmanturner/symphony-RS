---
# Quickstart fixture for Symphony-RS v2.
#
# Pairs with `issues.yaml` in this same directory. Drives the
# in-memory `MockTracker` and the no-op `MockAgentRunner`, so the
# binary can be exercised end-to-end without a Linear/GitHub
# credential or a real `codex`/`claude` install on PATH.
#
# Used by:
#   - `crates/symphony-cli/tests/quickstart.rs` smoke test (gated
#     until the v2 loader/runner lands).
#   - The README "Quickstart" walkthrough.
#
# `mock` is a deliberate test-only adapter `kind` per SPEC_v2 §4.5.
# It is allowed inside `tests/fixtures/` and not as an
# operator-facing production adapter.
#
# `workspace.root` is intentionally omitted so the smoke test can
# point the orchestrator at a `tempfile::TempDir` via the
# `SYMPHONY_WORKSPACE__ROOT` env var.

schema_version: 1

tracker:
  kind: mock
  fixtures: issues.yaml
  unknown_state_policy: error
  state_mapping:
    intake: [Todo]
    running: [In Progress]
    done: [Done]
    cancelled: [Cancelled]

polling:
  # Tight tick so the smoke test can observe at least one dispatch
  # within a sub-second sleep window. Production deployments should
  # keep the SPEC_v2 §5.2 default (30s).
  interval_ms: 200
  jitter_ms: 0
  startup_reconcile_recent_terminal: false

persistence:
  kind: sqlite
  path: .symphony/quickstart-state.db
  event_log: .symphony/quickstart-events.ndjson

roles:
  platform_lead:
    kind: integration_owner
    agent: mock_agent
    max_concurrent: 1
    can_decompose: true
    can_request_qa: true
    can_close_parent: true
  qa:
    kind: qa_gate
    agent: mock_agent
    max_concurrent: 1
    can_file_blockers: true
    required_for_done: true
  worker:
    kind: specialist
    agent: mock_agent

agents:
  mock_agent:
    backend: mock
    max_concurrent_agents: 2
    max_turns: 4
    max_retry_backoff_ms: 1000

routing:
  default_role: worker
  match_mode: first_match
  rules:
    - when:
        labels_any: [qa]
      assign_role: qa
    - when:
        labels_any: [epic, broad]
      assign_role: platform_lead

decomposition:
  enabled: false
  owner_role: platform_lead
  child_issue_policy: propose_for_approval
  max_depth: 1
  require_acceptance_criteria_per_child: false

workspace:
  default_strategy: issue_worktree
  strategies:
    issue_worktree:
      kind: git_worktree
      base: main
      branch_template: symphony/{{identifier}}
      cleanup: retain_until_done
  require_cwd_in_workspace: false
  forbid_untracked_outside_workspace: false

branching:
  default_base: main
  child_branch_template: symphony/{{identifier}}
  integration_branch_template: symphony/integration/{{identifier}}
  allow_same_branch_for_children: false
  require_clean_tree_before_run: false

integration:
  owner_role: platform_lead
  required_for: []
  merge_strategy: sequential_cherry_pick
  conflict_policy: integration_owner_repair_turn
  require_all_children_terminal: true
  require_no_open_blockers: true
  require_integration_summary: false
  next_state_after_integration: qa

pull_requests:
  enabled: false
  owner_role: platform_lead
  provider: github
  open_stage: after_integration_verification
  initial_state: draft
  mark_ready_stage: after_qa_passes
  title_template: "{{identifier}}: {{title}}"
  link_tracker_issues: false
  require_ci_green_before_ready: false

qa:
  owner_role: qa
  required: false
  exhaustive: false
  allow_static_only: true
  can_file_blockers: true
  can_file_followups: true
  blocker_policy: blocks_parent
  waiver_roles: [platform_lead]
  evidence_required:
    tests: false
    changed_files_review: true
    acceptance_criteria_trace: false
    visual_or_runtime_evidence_when_applicable: false
  rerun_after_blockers_resolved: true

followups:
  enabled: true
  default_policy: propose_for_approval
  approval_role: platform_lead
  non_blocking_label: follow-up
  blocking_label: blocker
  require_reason: true
  require_acceptance_criteria: false

observability:
  logs:
    format: pretty
  event_bus: true
  sse:
    enabled: true
    # Bind to a kernel-assigned ephemeral port so the smoke test can
    # run in parallel with another `symphony` invocation without
    # colliding on the production default (6280).
    bind: "127.0.0.1:0"
    replay_buffer: 64
  tui:
    enabled: false
  dashboard:
    enabled: false

budgets:
  max_parallel_runs: 2
  max_cost_per_issue_usd: 0.10
  max_turns_per_run: 4
  max_retries: 1
  pause_policy: block_work_item

security:
  destructive_actions_require_approval: true
  publish_purchase_deploy_require_human: true
  network_policy: workflow_defined
  secret_redaction: true

hooks:
  timeout_ms: 5000
  after_create: []
  before_run: []
  after_run: []
  before_remove: []
---

# Symphony Quickstart Workflow

You are a Symphony-RS v2 quickstart agent dispatched against a canned
issue. The mock backend will reply with a scripted Started → Message
→ Completed sequence, so this prompt body is here only to demonstrate
the rendering path end-to-end.

## Issue

- Identifier: {{identifier}}
- Title: {{title}}
- State: {{status}}
