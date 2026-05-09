# Symphony-RS Product Specification v3 Addendum

Status: Draft v3 addendum — targeted requirements for tracker-backed dependency orchestration.

## 1. Scope

SPEC v3 does **not** replace `SPEC_v2.md`. It narrows one missing operational requirement discovered during repo review:

> When an integration-owner role decomposes broad work into multiple child issues, Symphony-RS must be able to enforce required sequencing between those children through durable dependency data, tracker mutations where supported, and scheduler dispatch gates.

The intent is to replicate the current Paperclip-style outcome using GitHub/Linear issues and Symphony's own state layer. This is **not** a Paperclip adapter and MUST NOT introduce Paperclip-specific tracker code, tables, state names, or workflow semantics.

## 2. Relationship to Existing Specs

Existing requirements remain authoritative in these areas:

- `SPEC_v2.md §4.8` defines blockers as durable dependency edges.
- `SPEC_v2.md §5.7` defines decomposition output as child scopes, owners/roles, dependencies, acceptance criteria, and integration strategy.
- `SPEC_v2.md §6.2` says decomposition creates/proposes child issues, sets a dependency graph, and keeps the parent blocked until children and gates pass.
- `SPEC_v2.md §7.2` requires tracker mutations for issue creation, blocker/dependency creation, parent/child linking, comments, and artifacts.
- `SPEC_v2.md §8.1` requires parent completion to wait for required children and open blockers.

This addendum only makes explicit the missing operational bridge between those requirements: converting decomposition-local `depends_on` edges into actionable scheduler/tracker blocker relationships and using them to serialize child execution.

## 3. Problem Statement

The v2 model already contains most of the necessary nouns:

- `ChildProposal.depends_on` captures a proposal-local dependency DAG.
- `work_item_edges` supports `parent_child` and `blocks` edges.
- `TrackerMutations` exposes `add_blocker` and `link_parent_child`.
- The Linear adapter supports structural `blocks` relations and `parentId` sub-issues.
- Integration and QA queues already respect open-blocker gates.

The current gap is behavioral: child creation can create/link child issues, but dependency edges between the created children are not yet guaranteed to be materialized, persisted, synced to the tracker, and honored by specialist dispatch. Without that bridge, the platform lead can describe sequential work, but Symphony cannot reliably enforce that child B waits for child A.

## 4. Required Outcome

Given a parent issue decomposed into children:

```text
parent
├─ child A: database migration
├─ child B: API implementation       depends_on: A
└─ child C: UI wiring                 depends_on: B
```

Symphony-RS MUST ensure:

1. The platform lead/integration owner can emit all children in one decomposition proposal.
2. The dependency graph is validated as a DAG before tracker mutation.
3. All approved child issues are created or linked under the parent according to `decomposition.child_issue_policy`.
4. Each `depends_on` edge becomes a durable `blocks` edge after child tracker IDs and work item IDs are known.
5. Trackers with structural dependency support receive equivalent tracker-side blocker/dependency mutations.
6. Children blocked by unresolved dependencies are not dispatched to specialists.
7. When the blocking child reaches a configured terminal state, the blocked child becomes eligible on the next reconciliation/tick.
8. Parent integration waits for required children to become terminal and for open blockers to clear, unless explicitly waived by workflow policy.

## 5. Dependency Edge Semantics

### 5.1 Direction

A child dependency is expressed from the waiting child to the prerequisite child:

```yaml
children:
  api:
    depends_on: [schema]
```

This means:

- `schema` blocks `api`.
- `api` is blocked by `schema`.
- Durable `work_item_edges` representation:
  - `edge_type = blocks`
  - `parent_id = schema_work_item_id`
  - `child_id = api_work_item_id`
  - `status = open`

Tracker mutation representation:

```text
AddBlockerRequest {
  blocker: schema_tracker_issue_id,
  blocked: api_tracker_issue_id,
  reason: "api depends on schema"
}
```

### 5.2 Edge Sources

A `blocks` edge MAY be created from:

- platform-lead decomposition `depends_on`;
- QA blocker verdicts;
- specialist/integration-owner handoff blocker requests;
- approved blocking follow-ups.

Edges created from decomposition dependencies MUST be tagged or otherwise attributable to their source proposal/run so reconciliation can distinguish them from QA or human blockers.

### 5.3 Edge Resolution

An edge created from decomposition `depends_on` MUST resolve automatically when the blocker child reaches a workflow-terminal status class, unless the tracker reports an explicit unresolved blocker relation that contradicts local state.

If automatic resolution and tracker state disagree, Symphony MUST choose the safer behavior:

- keep the dependent child blocked;
- emit a reconciliation warning/event;
- surface the issue in `symphony issue graph <id>` / status output.

## 6. Decomposition Application Requirements

When applying an approved `DecompositionProposal`, the applier/orchestrator MUST perform these operations in one recoverable workflow:

1. Create or link every child issue through `TrackerMutations::create_issue`.
2. Persist corresponding child `work_items`.
3. Persist `parent_child` edges from the parent to each child.
4. Update the decomposition proposal with `ChildKey -> WorkItemId` mappings.
5. For each child `depends_on` entry:
   - resolve both `ChildKey`s to `WorkItemId`s;
   - create a local `blocks` edge;
   - if tracker capabilities allow, call `TrackerMutations::add_blocker`;
   - record the tracker edge ID when returned.
6. Emit durable events for created children, parent/child links, local blocker edges, tracker blocker sync results, and partial failures.

The system MUST NOT mark a decomposition as fully applied until dependency edges are either:

- successfully persisted and synced where required; or
- explicitly recorded as local-only/advisory because the configured tracker cannot represent structural blockers.

## 7. Scheduler Dispatch Requirements

Specialist dispatch MUST be dependency-aware.

A child work item is eligible for specialist dispatch only when all of the following are true:

- its normalized status class is routable to specialist work;
- it has no incoming open `blocks` edges;
- all `depends_on` prerequisite children are terminal, waived, or otherwise policy-cleared;
- its parent is not cancelled;
- no unresolved human/budget/workspace pause blocks the child.

The scheduler MUST not rely only on tracker status labels such as `blocked`. It MUST consult durable state edges and tracker reconciliation results.

### 7.1 Sequential Chain Example

Given:

```text
A -> B -> C
```

Expected execution:

1. Only A is dispatched initially.
2. B remains blocked with visible reason: `blocked by A`.
3. C remains blocked with visible reason: `blocked by B`.
4. When A is terminal, B becomes eligible.
5. When B is terminal, C becomes eligible.
6. Parent integration is not eligible until A, B, and C satisfy the integration gates.

### 7.2 Parallel Plus Sequential Example

Given:

```text
A
B
C depends_on: [A, B]
```

Expected execution:

1. A and B may run concurrently, subject to role/budget limits.
2. C is blocked until both A and B are terminal or waived.
3. If A passes and B fails QA/rework, C remains blocked.

## 8. Tracker Capability Policy

### 8.1 Linear

Linear is the preferred tracker for full structural orchestration because it supports:

- child issues through `parentId`;
- blocker/dependency relations through `issueRelationCreate(type: "blocks")`;
- comment/evidence attachment.

For Linear-backed workflows, dependency orchestration MUST create native Linear blocker relations when `TrackerCapabilities.add_blocker` is true.

### 8.2 GitHub Issues

GitHub Issues support remains limited unless the implementation adopts a stable structural sub-issue/dependency API.

When using standard GitHub Issues:

- `link_parent_child` MUST NOT be advertised as structural support unless the adapter implements a stable native relation.
- `add_blocker` MUST NOT be advertised as structural support if the implementation only writes labels or body text.
- Symphony MAY render advisory text/comments such as `Blocked by #123` for human visibility.
- Symphony's own durable state MUST remain the enforcement source for sequencing if the workflow allows GitHub local-only dependency orchestration.

A workflow that requires tracker-native dependency enforcement MUST fail validation when configured with a tracker that lacks `add_blocker` and/or `link_parent_child` capability.

## 9. Workflow Configuration Additions

Avoid adding broad new config sections unless needed. Extend existing sections narrowly.

### 9.1 `decomposition.dependency_policy`

```yaml
decomposition:
  dependency_policy:
    materialize_edges: true
    tracker_sync: required | best_effort | local_only
    dispatch_gate: true
    auto_resolve_on_terminal: true
```

Defaults:

- `materialize_edges: true`
- `tracker_sync: best_effort`
- `dispatch_gate: true`
- `auto_resolve_on_terminal: true`

Validation:

- If `tracker_sync: required`, the tracker MUST have `add_blocker: true`.
- If child issues are created under a parent and `link_parent_child` is required by workflow policy, the tracker MUST support parent/child linking or the workflow MUST explicitly allow advisory/local-only parent links.
- If `dispatch_gate: false`, the workflow MUST warn loudly because dependency edges become observability-only.

### 9.2 Existing Config Remains Authoritative

Do not duplicate or rename existing v2 config unless implementation proves the current names are insufficient. In particular:

- keep `decomposition.child_issue_policy`;
- keep `integration.require_all_children_terminal`;
- keep `integration.require_no_open_blockers`;
- keep `qa.blocker_policy`;
- keep `followups.default_policy`.

## 10. State and Reconciliation Requirements

The durable state layer MUST be able to answer:

- Which children belong to this parent?
- Which child blocks which other child?
- Which blocker edges came from decomposition versus QA/follow-up/human input?
- Which tracker edge ID, if any, mirrors each local edge?
- Which blocked children are now eligible because blockers reached terminal state?
- Which tracker mutations failed and need retry?

A reconciliation tick MUST:

1. Fetch current tracker states for known blocker and blocked issues.
2. Resolve local decomposition blocker edges when blocker children are terminal and policy allows auto-resolution.
3. Retry failed tracker edge syncs when `tracker_sync` is `required` or `best_effort`.
4. Recompute specialist queue eligibility for newly unblocked children.
5. Emit durable events for all state changes.

## 11. Observability Requirements

Operator surfaces MUST make sequential blocking obvious.

Minimum required output for issue graph/status views:

```text
Parent ABC-100: Add billing workflow
Children:
- ABC-101 schema migration        done
- ABC-102 API implementation      blocked by ABC-101 [resolved locally, tracker sync pending]
- ABC-103 UI wiring               blocked by ABC-102 [open]
```

For each blocked child, show:

- blocker issue identifier/title;
- reason;
- source (`decomposition`, `qa`, `followup`, `human`);
- local edge status;
- tracker sync status;
- next action (`waiting`, `eligible next tick`, `sync retry`, `human waiver required`).

## 12. Failure Behavior

### 12.1 Partial Child Creation

If some child issues are created but a later child creation fails:

- persist the successfully created child results;
- mark the decomposition application as partial/blocked;
- do not dispatch any children whose dependency graph may be incomplete;
- emit an operator-visible event with retry guidance.

### 12.2 Dependency Sync Failure

If local edges are persisted but tracker blocker mutation fails:

- with `tracker_sync: required`: keep affected children blocked and surface retry/error state;
- with `tracker_sync: best_effort`: local dispatch gating remains authoritative, but surface tracker sync warning;
- with `tracker_sync: local_only`: do not call tracker blocker mutation; optionally render advisory comments/text.

### 12.3 Cycle or Missing Key

Cycle and missing-key validation remains a decomposition proposal invariant. Such proposals MUST be rejected before any tracker mutation.

## 13. Implementation Targets

The first implementation should touch the existing v2 surfaces rather than create parallel v3 abstractions.

Required work:

1. Extend decomposition application so `depends_on` edges create local `blocks` edges after child work item IDs are known.
2. Add tracker sync for those dependency edges through `TrackerMutations::add_blocker` when capability and policy allow.
3. Add tracker edge ID/sync status persistence if not already represented.
4. Make specialist queue eligibility consult incoming open blocker edges.
5. Add reconciliation for terminal blocker children to resolve decomposition-sourced edges.
6. Add status/graph visibility for blocked children and sync state.
7. Add deterministic tests for sequential and mixed parallel/sequential decompositions.
8. Add Linear mutation tests proving `issueRelationCreate(type: "blocks")` is called with `issue_id = blocker` and `related_issue_id = blocked`.
9. Add GitHub tests proving structural dependency capabilities remain false unless a real native implementation lands.
10. Add workflow validation tests for `dependency_policy.tracker_sync` and tracker capability combinations.

## 14. Acceptance Criteria

SPEC v3 is satisfied when all of the following are true:

- A decomposition proposal with `A`, `B depends_on A`, `C depends_on B` creates all child issues and durable dependency edges.
- Only A is initially eligible for specialist dispatch.
- B becomes eligible only after A reaches terminal status or is explicitly waived.
- C becomes eligible only after B reaches terminal status or is explicitly waived.
- Linear-backed workflows create native Linear blocker relations for A→B and B→C.
- GitHub-backed workflows either fail validation for required structural dependency sync or run with explicit local-only/advisory dependency policy.
- Parent integration remains gated until required children terminal and open blockers clear.
- Operator status/graph output explains why B/C are blocked and what will unblock them.
- Failed dependency-edge sync is durable, visible, and retryable.

## 15. Non-Goals

This addendum does not require:

- a Paperclip adapter;
- Jira/GitLab/custom tracker support;
- a web dashboard before CLI/status output exists;
- replacing v2 role, QA, integration, PR, workspace, or follow-up specs;
- allowing GitHub label/body text to masquerade as structural dependency support;
- dispatching integration-owner work to a generic worker pool.

The platform lead remains the decomposition/integration authority. Specialists execute scoped children only when dependency gates make those children eligible.
