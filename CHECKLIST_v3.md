# Symphony-RS v3 Checklist — Tracker Dependency Orchestration

One unchecked item per implementation iteration. Each item should land with tests and a commit. This checklist implements `SPEC_v3.md` only; do not duplicate completed v2 work.

## Phase 0 — v3 Grounding

- [x] Add `SPEC_v3.md` to product docs/references and link it from the v2 productization docs without redefining v2 role, QA, PR, workspace, or follow-up requirements.
- [x] Add a fixture decomposition scenario documenting `A`, `B depends_on A`, `C depends_on B` as the canonical sequential child DAG.
- [x] Add a fixture decomposition scenario documenting mixed parallel/sequential work: `A`, `B`, `C depends_on [A, B]`.

## Phase 1 — Workflow Config Schema

- [x] Extend `DecompositionConfig` with `dependency_policy` containing `materialize_edges`, `tracker_sync`, `dispatch_gate`, and `auto_resolve_on_terminal`.
- [x] Add typed `DependencyTrackerSyncPolicy` enum: `required | best_effort | local_only`.
- [x] Add defaults matching SPEC v3: materialize edges on, tracker sync best-effort, dispatch gate on, auto-resolve on terminal on.
- [x] Add workflow validation: `tracker_sync: required` requires `TrackerCapabilities.add_blocker` for the configured tracker.
- [x] Add workflow validation/warning: `dispatch_gate: false` emits a loud warning because dependencies become observability-only.
- [x] Add workflow validation for parent/child link policy: structural parent links require tracker support unless local/advisory mode is explicitly allowed.
- [x] Update `tests/fixtures/sample-workflow/WORKFLOW.md` with `decomposition.dependency_policy` and at least one role-routed dependency example.
- [x] Add round-trip config tests and negative tests for invalid dependency policy values/capability combinations.

## Phase 2 — State Model and Repositories

- [x] Add source/provenance metadata for `work_item_edges` so decomposition-sourced `blocks` edges can be distinguished from QA, follow-up, and human blockers.
- [x] Add tracker sync fields for dependency edges if missing: tracker edge id, sync status, last error, retry count, last attempted timestamp.
- [x] Add repository helpers to create decomposition dependency edges in one transaction after child `WorkItemId`s are known.
- [x] Add repository helper to list incoming open blocker edges for specialist dispatch eligibility.
- [x] Add repository helper to list decomposition-sourced open blockers whose blocker child is terminal and eligible for auto-resolution.
- [x] Add repository tests for edge direction: prerequisite child is `parent_id`/blocker, waiting child is `child_id`/blocked.
- [x] Add repository tests proving open decomposition blockers suppress blocked-child eligibility and resolved blockers do not.

## Phase 3 — Decomposition Application

- [x] Extend decomposition application orchestration to persist `parent_child` edges and child `work_items` before materializing dependency `blocks` edges.
- [x] For each `ChildProposal.depends_on`, resolve both `ChildKey`s to created child `WorkItemId`s and create a local open `blocks` edge.
- [x] Preserve partial child creation state when child creation succeeds but dependency materialization fails.
- [x] Prevent a decomposition proposal from being marked fully applied until dependency edges are persisted and, when required, tracker sync state is recorded.
- [x] Add tests for successful `A -> B -> C` child creation plus local `blocks` edge materialization.
- [ ] Add tests for mixed parallel/sequential graph materialization.
- [ ] Add tests for partial failure: child issue created, dependency edge creation fails, decomposition remains partial/blocked and no unsafe child dispatch occurs.

## Phase 4 — Tracker Dependency Sync

- [ ] Add orchestration path that calls `TrackerMutations::add_blocker` for each decomposition dependency edge when capability and policy allow.
- [ ] Persist returned tracker edge IDs where adapters provide them.
- [ ] Implement `tracker_sync: required` behavior: keep affected children blocked when tracker blocker mutation fails.
- [ ] Implement `tracker_sync: best_effort` behavior: local gating remains authoritative while failed tracker sync is visible/retryable.
- [ ] Implement `tracker_sync: local_only` behavior: skip tracker blocker mutation and optionally render advisory comments/text.
- [ ] Add Linear mutation tests proving `issueRelationCreate(type: "blocks")` is called with `issue_id = blocker` and `related_issue_id = blocked`.
- [ ] Add GitHub tests proving `add_blocker` and `link_parent_child` remain false unless a real structural implementation lands.
- [ ] Add conformance tests proving label/body/comment-only GitHub behavior never masquerades as structural blocker support.

## Phase 5 — Specialist Dispatch Gating

- [ ] Update specialist queue eligibility to consult incoming open `blocks` edges before dispatch.
- [ ] Ensure root children with no blockers can dispatch in parallel subject to role/concurrency/budget/workspace limits.
- [ ] Ensure blocked children remain parked with visible blocker reason instead of disappearing from queues.
- [ ] Add tests proving only A dispatches initially for `A -> B -> C`.
- [ ] Add tests proving A and B can dispatch concurrently for `A`, `B`, `C depends_on [A, B]`.
- [ ] Add tests proving blocked children become eligible only after all prerequisite blockers are terminal/resolved/waived.

## Phase 6 — Reconciliation and Auto-Resolution

- [ ] Add reconciliation tick logic that fetches current tracker states for blocker/blocked children.
- [ ] Auto-resolve decomposition-sourced local blocker edges when the blocker child reaches terminal state and policy allows.
- [ ] Keep the dependent child blocked when local terminal state and tracker blocker state disagree; emit a reconciliation warning/event.
- [ ] Retry failed tracker edge syncs for `required` and `best_effort` policies.
- [ ] Recompute specialist queue eligibility for newly unblocked children on the next tick.
- [ ] Add tests for terminal blocker child unblocking dependent child.
- [ ] Add tests for tracker/local disagreement choosing the safer blocked state.
- [ ] Add tests for failed sync retry counters and eventual success.

## Phase 7 — Integration and QA Gate Interactions

- [ ] Ensure integration queue still requires required children terminal and no open blockers before platform-lead integration.
- [ ] Ensure QA gate remains a post-integration verdict over the integrated branch/draft PR, not a normal child issue by default.
- [ ] Ensure QA-created blockers and decomposition dependency blockers coexist without collapsing provenance.
- [ ] Add tests proving parent integration is blocked while any required sequential child remains blocked/non-terminal.
- [ ] Add tests proving QA blocker failures do not auto-resolve merely because a decomposition dependency resolved.

## Phase 8 — Observability and Operator Surface

- [ ] Extend `symphony issue graph <id>` / status output to show parent/child/dependency blocker graph.
- [ ] Show for each blocked child: blocker identifier/title, reason, source, local edge status, tracker sync status, and next action.
- [ ] Surface failed dependency-edge sync as durable, visible, retryable state.
- [ ] Add JSON output for dependency graph status.
- [ ] Add tests/snapshots for the SPEC v3 sample graph display.

## Phase 9 — End-to-End Scenarios

- [ ] Add deterministic fake E2E: parent decomposes into `A -> B -> C`; only A runs first; B then C run after terminal prerequisites; platform lead integrates; QA passes; parent closes.
- [ ] Add deterministic fake E2E: parallel roots A/B run together, C waits for both, B fails QA/rework, C remains blocked until B passes.
- [ ] Add Linear-backed adapter test path proving native blocker relations are created and reconciled.
- [ ] Add GitHub local-only/advisory scenario proving local Symphony state enforces sequencing when tracker-native blockers are unavailable.

## Phase 10 — Documentation

- [ ] Document dependency edge direction and examples in `docs/workflow.md`.
- [ ] Document Linear vs GitHub dependency capability behavior.
- [ ] Document `decomposition.dependency_policy` defaults and failure modes.
- [ ] Document operator interpretation of dependency graph/status output.
