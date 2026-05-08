# Symphony-RS v2 Plan

This is the strategic plan for shaping Symphony-RS into a specialist-agent orchestration product.

The v2 direction intentionally borrows lessons from Paperclip without cloning Paperclip. The target product is configurable: platform-lead and QA behaviors are first-class because they preserve the crux of the workflow, while all other specialist roles are workflow-defined.

## Product Thesis

AI agents are useful when work is bounded, ownership is explicit, integration is serialized, and QA has real authority. Symphony-RS v2 should make that the default path:

```text
intake -> decompose -> specialist execution -> integration owner -> exhaustive QA -> done/rework/follow-up
```

## Phases

### Phase 0 — v2 Design Grounding

Create v2 state files and fixtures. Keep existing v1 docs as implementation history. v2 documents are the target product direction.

### Phase 1 — Config v2 Schema

Parse `version: 2` workflows with roles, agents, routing, decomposition, workspace, branching, integration, QA, follow-up, observability, and budgets.

Success bar: a full v2 fixture validates and negative fixtures fail loudly.

### Phase 2 — Durable State

Introduce persistent state for work items, runs, edges, handoffs, QA verdicts, workspace claims, and events.

Success bar: a mock run survives process restart with enough state to recover safely.

### Phase 3 — Domain Model

Codify the product vocabulary in `symphony-core`: work items, roles, blockers, handoffs, QA verdicts, follow-ups, integration records.

Success bar: pure unit tests cover state invariants before scheduler code uses them.

### Phase 4 — Tracker Capabilities

Split tracker reads from mutations. Add capability detection. Mutation-capable adapters can create issues, comments, blockers, parent/child links, and artifacts.

Success bar: workflows can declare whether direct blocker/follow-up creation is allowed or advisory-only.

### Phase 5 — Routing and Decomposition

Implement role routing and integration-owner decomposition. Broad issues become child issues with owners, acceptance criteria, and dependencies.

Success bar: parent completion is impossible while child work remains incomplete.

### Phase 6 — Workspace and Branch Claims

Promote workspaces from paths to verified claims over path + branch + base ref + cleanliness policy.

Success bar: the runner refuses to launch in the wrong cwd, wrong branch, dirty tree, or outside the workspace root.

### Phase 7 — Structured Agent Handoffs

Agents stop returning only free-form text. Every run produces a structured handoff with changed files, tests, evidence, risks, blockers, follow-ups, and readiness.

Success bar: malformed output is caught and either repaired or failed.

### Phase 8 — Integration Owner Flow

Implement the platform-lead/integration-owner protocol: wait for children, consolidate branches, run integration verification, produce handoff, request QA.

Success bar: broad parent work cannot skip integration.

### Phase 9 — QA Gate Flow

Implement QA as a durable gate. QA verifies final integrated work, records verdict, files blockers/follow-ups, and forces rework when necessary.

Success bar: QA-created blockers block parent completion by default.

### Phase 10 — Follow-up Issue Flow

Make follow-up literacy a product feature. Any role can file or propose follow-up work according to policy.

Success bar: follow-ups are linked, scoped, acceptance-tested, and clearly blocking or non-blocking.

### Phase 11 — Scheduler v2

Replace the flat v1 poll loop with durable logical queues, leases, recovery, retries, and multi-level concurrency.

Success bar: crash/restart recovery leaves no phantom running work and no duplicate active leases.

### Phase 12 — Observability

Expose durable status: queues, blockers, integration branches, QA verdicts, runs, costs, and event history.

Success bar: an operator can answer “what is blocked, who owns it, what branch is canonical, and why is this not done?” without reading raw logs.

### Phase 13 — End-to-End Scenarios

Use mock trackers/agents to test the whole product loop before relying on real providers.

Success bar: broad issue -> decomposition -> child execution -> integration -> QA failure -> blocker -> rework -> QA pass is covered by an automated test.

### Phase 14 — Documentation and Productization

Document workflow schema, role semantics, workspace policy, QA protocol, and migration from v1.

## Design Constraints

- Do not hardcode a full org chart.
- Do codify `integration_owner` and `qa_gate` role kinds.
- Keep adapters replaceable.
- Make tracker mutation capability explicit.
- Make state durable before complex autonomous loops depend on it.
- Prefer strict schemas over prompt-only conventions.
- Prefer local deterministic mock E2E tests before real tracker tests.

## Pending Refactors

- Replace v1 prompt string substitution with a strict template/renderer.
- Split current `IssueTracker` into read and mutation traits.
- Move v1 in-memory scheduler state behind a durable state abstraction.
- Promote `LocalFsWorkspace` return values into `WorkspaceClaim`.
- Reconcile retry queue comments/checklist with actual production wiring.

## Open Questions

1. Persistence crate choice: `sqlx` vs `rusqlite`.
2. Whether v2 should keep v1 `symphony run` behavior as `run-v1` during migration.
3. Whether tracker mutations should be direct adapter methods or exposed as agent tools plus audited orchestrator wrappers.
4. Whether integration branch operations should be implemented in core or delegated to a VCS adapter crate.
5. How much of the live dashboard should ship before the durable CLI status is excellent.
