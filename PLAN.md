# Symphony-RS Plan

This is the strategic plan that organises the iteration loop. Concrete
unit-of-work items live in `CHECKLIST.md`; this file holds the *shape*
of the project and any cross-cutting context the next iteration needs.

## Phases

### Phase 1 — Foundations
Workspace skeleton, structured logging, layered config, WORKFLOW.md
parsing. Goal: a `symphony validate path/to/WORKFLOW.md` invocation
parses front matter and prompt body and exits 0 / non-zero on a
malformed file.

### Phase 2 — IssueTracker abstraction + Linear
Trait, in-memory mock, Linear GraphQL adapter, `wiremock` integration
tests, reconciliation queries.

### Phase 3 — Workspace manager
Sanitization, containment-checked path mapping, lifecycle hooks
(`before_run`, `after_create`, `after_release`).

### Phase 4 — AgentRunner abstraction + two backends
Trait + normalized `AgentEvent`. `CodexRunner` over `codex app-server`.
`ClaudeRunner` over `claude -p --output-format stream-json
--permission-mode bypassPermissions`. Stream-shape parity tests.

### Phase 5 — Orchestrator
State machine (`Unclaimed → Claimed{Running|RetryQueued} → Released`),
poll loop with jitter and bounded concurrency, retry queue with
exponential backoff, reconciliation pass each tick.

### Phase 6 — Tandem mode (bonus)
`TandemRunner` wrapping two inner runners with a configurable
strategy: `draft-review`, `split-implement`, `consensus`. Telemetry
for per-agent token counts, agreement rate, cost per turn.

### Phase 7 — CLI binary
`symphony run | validate | status`. Graceful SIGINT shutdown.
`assert_cmd` smoke tests. Quickstart README.

## Pending Refactors

(none)

## Open Questions

(none)
