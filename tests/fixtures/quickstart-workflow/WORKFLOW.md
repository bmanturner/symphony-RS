---
# Quickstart fixture for `symphony run`.
#
# Pairs with `issues.yaml` in this same directory. Drives the in-memory
# `MockTracker` and the no-op `MockAgentRunner`, so the binary can be
# exercised end-to-end without a Linear / GitHub credential or a real
# `codex` / `claude` install on PATH.
#
# Used by:
#   - `crates/symphony-cli/tests/quickstart.rs` smoke test.
#   - The README "Quickstart" walkthrough.
#
# `workspace.root` is intentionally omitted so the smoke test can point
# the orchestrator at a `tempfile::TempDir` via the
# `SYMPHONY_WORKSPACE__ROOT` env var (figment env layering — see SPEC
# §5.4). A documentation-time reader running this manually will get the
# `<system-temp>/symphony_workspaces` fallback per SPEC §5.3.3.
tracker:
  kind: mock
  fixtures: issues.yaml
  active_states:
    - Todo
    - In Progress
  terminal_states:
    - Done
    - Cancelled

polling:
  # Tight tick so the smoke test can observe at least one dispatch
  # within a sub-second sleep window. Production deployments should keep
  # the SPEC §5.3 default (30s).
  interval_ms: 200

agent:
  kind: mock
  max_concurrent_agents: 2
  max_turns: 4
  max_retry_backoff_ms: 1000
---

# Symphony Quickstart Workflow

You are a Symphony quickstart agent dispatched against a canned issue.
The mock backend will reply with a scripted Started → Message →
Completed sequence, so this prompt body is here only to demonstrate the
rendering path end-to-end.

## Issue

- Identifier: {{identifier}}
- Title: {{title}}
- State: {{state}}
