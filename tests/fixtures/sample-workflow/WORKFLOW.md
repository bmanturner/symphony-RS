---
# Sample WORKFLOW.md fixture for Symphony-RS integration tests and the
# `symphony validate` smoke test. Mirrors the SPEC §5.3 defaults closely
# while exercising every top-level section so a reviewer can read this
# file as a worked example of a complete, valid workflow.
#
# Anything marked "$VAR" is a placeholder. Layered loading expands these
# from the environment; tests that care about credential resolution set
# the relevant `SYMPHONY_*` variables explicitly and never rely on a
# developer's shell.
tracker:
  kind: linear
  api_key: $SYMPHONY_TRACKER_API_KEY
  project_slug: ENG
  active_states:
    - Todo
    - In Progress
  terminal_states:
    - Done
    - Cancelled
    - Duplicate

polling:
  interval_ms: 30000

workspace:
  root: ./.symphony/workspaces

hooks:
  after_create: |
    git config user.email "symphony@example.com"
    git config user.name "Symphony"
  before_run: |
    git checkout -b "symphony/$SYMPHONY_ISSUE_ID"
  after_run: |
    git status --short
  timeout_ms: 60000

agent:
  kind: codex
  max_concurrent_agents: 4
  max_turns: 20
  max_retry_backoff_ms: 300000
  max_concurrent_agents_by_state:
    in_progress: 2

codex:
  command: codex app-server
  turn_timeout_ms: 3600000
  read_timeout_ms: 5000
  stall_timeout_ms: 300000
---

# Symphony Sample Workflow

You are an autonomous coding agent dispatched by Symphony. Your job is to
read the issue described below, propose a minimal patch, run the project
test suite, and open a pull request once the suite is green.

## Working agreement

- Stay strictly in scope. One issue, one PR.
- Never disable or skip a failing test to make CI pass.
- If the issue is ambiguous, leave a comment on the issue describing the
  ambiguity and stop. Do not guess.

## Issue

The orchestrator will inject the issue body here at dispatch time.
