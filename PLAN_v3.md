# Symphony-RS v3 Plan

This is the strategic plan for adding tracker-backed dependency orchestration
to the v2 specialist-agent product.

v3 is an addendum to v2, not a replacement. v2 defines the durable workflow
kernel: configurable specialists, integration ownership, QA gates, blockers,
follow-ups, workspace truth, and closeout rules. v3 narrows one operational
gap: when the integration owner decomposes broad work into child issues, the
child dependency graph must become durable `blocks` edges that gate specialist
dispatch, sync to capable trackers, and reconcile safely.

## Product Thesis

Specialist decomposition is only trustworthy when sequencing is enforced by
state, not suggestion. Symphony-RS v3 should make this path reliable:

```text
decompose children with depends_on -> validate DAG -> create children ->
materialize blocks edges -> dispatch only unblocked specialists ->
auto-resolve decomposition blockers -> integrate -> QA gate -> done
```

## Phases

### Phase 0 — v3 Grounding

Link `SPEC_v3.md` into product documentation and add canonical dependency
fixtures for sequential and mixed parallel/sequential decompositions.

Success bar: operators and maintainers can find v3 from the v2 docs without
confusing v3 dependency orchestration for a new role, QA, PR, workspace, or
follow-up model.

### Phase 1 — Workflow Config Schema

Add `decomposition.dependency_policy` with materialization, tracker sync,
dispatch gate, and auto-resolution controls.

Success bar: workflows can opt into required, best-effort, or local-only
tracker dependency sync, and invalid capability combinations fail loudly.

### Phase 2 — State Model and Repositories

Widen `work_item_edges` with provenance and tracker sync metadata, then add
repository helpers for creating, listing, and updating dependency blockers.

Success bar: the state layer can distinguish decomposition blockers from QA,
follow-up, and human blockers while preserving the existing `blocks` gate.

### Phase 3 — Decomposition Application

Extend decomposition application so child work items and parent-child links are
persisted before dependency `blocks` edges are materialized.

Success bar: `A -> B -> C` and mixed parallel/sequential proposals produce
durable edges in the correct direction and partial failures are recoverable.

### Phase 4 — Tracker Dependency Sync

Mirror dependency blockers through `TrackerMutations::add_blocker` when the
tracker and workflow policy allow it.

Success bar: Linear receives native blocker relations; GitHub remains honest
about lacking structural blocker support unless a real native API lands.

### Phase 5 — Specialist Dispatch Gating

Teach specialist eligibility to consult incoming open `blocks` edges before
leasing a child run.

Success bar: root children can run in parallel, while dependent children stay
parked with visible blocker reasons until prerequisites clear.

### Phase 6 — Reconciliation and Auto-Resolution

Resolve decomposition-sourced blockers when prerequisite children reach
terminal states, retry failed tracker syncs, and choose the safer blocked state
when local and tracker truth disagree.

Success bar: completed prerequisites unblock downstream specialists without
manual bookkeeping, and tracker disagreements remain visible.

### Phase 7 — Integration and QA Gate Interactions

Ensure decomposition blockers coexist with existing integration and QA gates.

Success bar: parent integration and closeout still require terminal children,
no open blockers, QA, and workflow approvals.

### Phase 8 — Observability and Operator Surface

Render dependency graph state in status and issue graph outputs.

Success bar: an operator can answer which child is blocked, by whom, why, from
which source, and what the next reconciliation action is.

### Phase 9 — End-to-End Scenarios

Cover sequential and mixed dependency graphs with deterministic fake runs and
adapter-specific dependency behavior.

Success bar: automated scenarios prove the North Star from decomposition
through QA-gated parent closeout.

### Phase 10 — Documentation

Document edge direction, dependency policy, tracker capability behavior, and
operator interpretation of graph/status output.

Success bar: workflow authors can configure dependency orchestration without
reading the implementation.

## Design Constraints

- Preserve v2 role, QA, integration, PR, workspace, and follow-up semantics.
- Keep the tracker and SQLite state as product truth, not prompt convention.
- Materialize dependency edges before any dependent child can dispatch.
- Treat tracker capability flags as honesty contracts.
- Prefer durable local gating under best-effort and local-only sync.
- Do not let decomposition-sourced auto-resolution resolve QA or human blockers.
