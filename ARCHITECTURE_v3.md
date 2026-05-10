# Symphony-RS Architecture v3 Addendum

This document describes the target architecture for SPEC v3: tracker-backed
dependency orchestration between sibling children produced by decomposition.

`SPEC_v3.md` is the product contract and `CHECKLIST_v3.md` is the iteration
plan. This file explains how the v3 requirements land on the existing v2
crate surfaces. It is an **addendum to `ARCHITECTURE_v2.md`** and does not
replace any v2 architectural decision.

## 1. Architectural Goal

v2 already has the nouns: `work_item_edges` with a `Blocks` variant,
`ChildProposal.depends_on`, `TrackerMutations::add_blocker`, the open-blocker
gate, and `pending_tracker_syncs`. v3 closes the operational bridge so a
proposal-local DAG becomes durable, scheduler-honored, tracker-mirrored
sequencing.

The v3 kernel must be able to answer, for any decomposed parent:

- which children belong to it;
- which child blocks which other child, and why;
- whether each blocker edge is durable locally, mirrored on the tracker, or
  advisory only;
- which children are presently dispatch-eligible, and which are parked
  waiting on a specific upstream child.

Adapters must keep their narrow role: trackers express structural blocker
edges where they can, and never masquerade label/body text as structural
support.

## 2. Layered View

```text
┌───────────────────────────────────────────────────────────────────────┐
│ Policy Layer                WORKFLOW.md v2 + v3                       │
│   decomposition.dependency_policy { materialize_edges, tracker_sync,  │
│   dispatch_gate, auto_resolve_on_terminal }                           │
├───────────────────────────────────────────────────────────────────────┤
│ Product Orchestration Layer                                           │
│   decomposition_applier (extended) → dependency materializer →        │
│   tracker dependency sync → reconciliation tick                       │
├───────────────────────────────────────────────────────────────────────┤
│ Durable State Layer                                                   │
│   work_item_edges (+ source/tracker_edge_id/sync columns) ·           │
│   pending_tracker_syncs (reused for retry) · events                   │
├───────────────────────────────────────────────────────────────────────┤
│ Execution Scheduler Layer                                             │
│   specialist queue eligibility consults incoming open `blocks` edges  │
├───────────────────────────────────────────────────────────────────────┤
│ Adapter Traits                                                        │
│   TrackerMutations::add_blocker (capability-gated, returns edge id)   │
├───────────────────────────────────────────────────────────────────────┤
│ Concrete Adapters                                                     │
│   Linear: issueRelationCreate(type: "blocks")                         │
│   GitHub Issues: capabilities remain false; advisory text only        │
├───────────────────────────────────────────────────────────────────────┤
│ Observability / Control                                               │
│   `symphony issue graph` shows dependency edges, sync state, source   │
└───────────────────────────────────────────────────────────────────────┘
```

## 3. Key Design Shifts From v2

### 3.1 From proposal-local `depends_on` to durable `blocks` edges

`ChildProposal.depends_on` is validated as a DAG at proposal construction
time but never reaches `work_item_edges`. v3 makes the
decomposition-application step responsible for resolving each
`(child_key, depends_on_key)` pair to `(WorkItemId, WorkItemId)` and writing
a `Blocks` edge with provenance.

### 3.2 From "edge type" to "edge type plus source"

v2 records *what* the edge is (`parent_child` / `blocks` / `relates_to` /
`followup_of`) but not *why* it exists. v3 adds a `source` column so
decomposition-sourced blockers can be auto-resolved on terminal upstream
state without disturbing QA, follow-up, or human blockers.

### 3.3 From local-truth to dual-truth blockers

A blocker edge now has two facets: the local row (authoritative for
dispatch gating) and an optional tracker mirror (`tracker_edge_id`,
`sync_status`). The reconciliation tick keeps the two consistent and
chooses the safer (more blocked) state on disagreement.

### 3.4 From "blocked label" to dispatch-gate predicate

Specialist eligibility no longer relies on tracker `blocked` labels. It
queries `WorkItemEdgeRepository::list_incoming(child, EdgeType::Blocks)`
filtered by `status = 'open'` and parks the child when at least one row
returns.

### 3.5 From single-tracker-call mutation to recoverable workflow

Decomposition application becomes a recoverable, multi-step workflow with
a partial-applied state class. Children may be created on the tracker
before dependency materialization succeeds; partial state is durable and
retryable, and no child whose dependency graph might be incomplete is
dispatched.

## 4. Crate Evolution

### 4.1 `symphony-config`

Add a typed `DependencyPolicy` block under `DecompositionConfig`:

```rust
pub struct DecompositionConfig {
    // existing v2 fields unchanged
    pub enabled: bool,
    pub owner_role: Option<String>,
    pub triggers: DecompositionTriggers,
    pub child_issue_policy: ChildIssuePolicy,
    pub max_depth: u32,
    pub require_acceptance_criteria_per_child: bool,

    // new in v3
    pub dependency_policy: DependencyPolicy,
}

pub struct DependencyPolicy {
    pub materialize_edges: bool,        // default true
    pub tracker_sync: DependencyTrackerSyncPolicy,
    pub dispatch_gate: bool,            // default true
    pub auto_resolve_on_terminal: bool, // default true
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum DependencyTrackerSyncPolicy {
    Required,
    BestEffort, // default
    LocalOnly,
}
```

Validation rules added to the workflow loader:

1. `tracker_sync: Required` requires `TrackerCapabilities.add_blocker = true`
   for the configured tracker; otherwise the workflow fails to load.
2. `dispatch_gate: false` emits a loud warning — the workflow accepts
   dependencies as observability-only.
3. Workflows that require structural parent linking continue to honor
   v2's `child_issue_policy` and the existing `link_parent_child`
   capability check; v3 does not relax that.

The current `decomposition.child_issue_policy`, `integration.require_*`,
`qa.blocker_policy`, and `followups.default_policy` remain authoritative
exactly as they are in v2 (`SPEC_v3.md §9.2`).

### 4.2 `symphony-core`

#### 4.2.1 `decomposition_applier` extension

The existing applier (`crates/symphony-core/src/decomposition_applier.rs`)
returns an `AppliedDecomposition { children: Vec<AppliedChild> }`. v3
extends the post-creation flow without forking the module.

The new orchestration sequence, end-to-end:

```text
ApprovedDecomposition
   │
   ├─ apply_decomposition()  // existing v2 step: create child issues
   │     → AppliedDecomposition { children: [(ChildKey, IssueId)] }
   │
   ├─ persist child work_items rows + parent_child edges
   │     → ChildKey -> WorkItemId map
   │
   ├─ materialize_dependency_edges()      // new in v3
   │     for each (child, dep) in proposal:
   │       create Blocks edge { parent_id = dep_wid, child_id = child_wid,
   │                            status = "open", source = "decomposition",
   │                            reason = "<child> depends on <dep>" }
   │
   ├─ sync_dependency_edges_to_tracker()  // new in v3
   │     when capability + policy allow:
   │       TrackerMutations::add_blocker { blocked, blocker, reason }
   │       persist returned edge_id; on failure enqueue pending_tracker_sync
   │
   └─ DecompositionProposal::mark_applied()
         only after all required steps land per dependency_policy
```

The applier becomes responsible for refusing to mark the proposal "fully
applied" until materialization (and required tracker sync) is durable.
Partial state is recorded as `DecompositionStatus::PartiallyApplied`
(new) — see §5.1.

#### 4.2.2 New module: `dependency_edge.rs`

A small focused module that owns the v3 invariants:

```rust
pub struct DecompositionDependencyEdge {
    pub blocker_child: WorkItemId, // parent_id in the row
    pub blocked_child: WorkItemId, // child_id in the row
    pub source_proposal: DecompositionId,
    pub reason: String,
}

pub fn edges_from_proposal(
    proposal: &DecompositionProposal,
    child_ids: &HashMap<ChildKey, WorkItemId>,
) -> Result<Vec<DecompositionDependencyEdge>, DependencyEdgeError>;
```

This keeps the `(prerequisite -> dependent) :: (parent_id -> child_id)`
direction rule in one place and provides a unit-testable seam separate
from tracker I/O.

#### 4.2.3 Specialist dispatch gate

`specialist_tick` (or its eligibility helper) gains a precondition check:

```rust
fn dependency_clear(
    repo: &dyn WorkItemEdgeRepository,
    child: WorkItemId,
) -> StateResult<bool> {
    let incoming = repo.list_incoming(child, EdgeType::Blocks)?;
    Ok(incoming.iter().all(|e| e.status != "open"))
}
```

The check fires *after* the existing v2 status-class and parent-state
checks but *before* lease acquisition, and parks the child with a
visible reason `blocked by <id>` so the operator surface (§7) can render
it.

This integrates with the existing `blocker_gate` module: that module
continues to gate **integration / draft PR / QA request** for the
parent's subtree (§5.10–§5.12). The new dependency gate runs at the
**specialist queue** layer for a single child. The two are siblings,
not duplicates.

#### 4.2.4 Reconciliation tick

A new responsibility for the existing recovery / reconciliation runner
(`recovery_runner.rs`) — or a sibling `dependency_reconciliation_tick`
when the existing runner's surface is full:

1. Find decomposition-sourced open `Blocks` edges whose `parent_id`
   work item is in a workflow-terminal status class.
2. If `auto_resolve_on_terminal = true` and no contradicting tracker
   relation exists, set the edge `status = 'resolved'`.
3. Re-fetch tracker state where required; on disagreement (tracker says
   blocker is still active), keep the local edge open and emit a
   warning event.
4. Drain `pending_tracker_syncs` rows for `mutation_kind = 'add_blocker'`
   under the configured retry policy.
5. After resolutions, recompute specialist eligibility for any newly
   unblocked child (delegated to the queue tick on the next pass — the
   reconciliation tick only sets state).

### 4.3 `symphony-state`

#### 4.3.1 Schema migration `V3_DEPENDENCY_EDGE_METADATA`

Append a new migration with version `2_026_050_901` (or next free
`YYYYMMDDNN`). It is purely additive — new nullable columns plus an
index — so it is safe to apply over existing v2 deployments.

```sql
-- v3: dependency edge provenance + tracker mirror.
ALTER TABLE work_item_edges
    ADD COLUMN source TEXT;
ALTER TABLE work_item_edges
    ADD COLUMN tracker_edge_id TEXT;
ALTER TABLE work_item_edges
    ADD COLUMN tracker_sync_status TEXT;        -- pending|synced|failed|local_only
ALTER TABLE work_item_edges
    ADD COLUMN tracker_sync_last_error TEXT;
ALTER TABLE work_item_edges
    ADD COLUMN tracker_sync_attempts INTEGER NOT NULL DEFAULT 0;
ALTER TABLE work_item_edges
    ADD COLUMN tracker_sync_last_attempt_at TEXT;

CREATE INDEX idx_work_item_edges_blocked_open
    ON work_item_edges(child_id, edge_type, status)
    WHERE edge_type = 'blocks' AND status = 'open';
```

`source` is a free-text column at the schema layer with the kernel
enforcing the typed allow-list `decomposition | qa | followup | human`.
Keeping it stringly-typed at the storage layer matches the precedent set
by `status` (per `edges.rs` head-comment).

`tracker_sync_status` is also stringly-typed at storage; the typed enum
lives in `symphony-core::dependency_edge`.

#### 4.3.2 Repository surface additions

Extend `WorkItemEdgeRepository` (in `crates/symphony-state/src/edges.rs`):

```rust
pub trait WorkItemEdgeRepository {
    // existing v2 methods unchanged ...

    /// Bulk-insert decomposition dependency edges in one transaction.
    fn create_decomposition_blocker_edges(
        &mut self,
        edges: &[NewDecompositionBlockerEdge<'_>],
    ) -> StateResult<Vec<WorkItemEdgeRecord>>;

    /// Decomposition-sourced open blockers whose blocker child is
    /// terminal. Drives auto-resolution.
    fn list_decomposition_blockers_with_terminal_blocker(
        &self,
    ) -> StateResult<Vec<WorkItemEdgeRecord>>;

    /// Update tracker mirror metadata after `add_blocker` lands or fails.
    fn record_dependency_sync_result(
        &mut self,
        edge: EdgeId,
        result: DependencySyncResult<'_>,
    ) -> StateResult<()>;
}
```

The existing `list_incoming(child, EdgeType::Blocks)` is sufficient for
the dispatch gate; no new query is required there.

#### 4.3.3 `pending_tracker_syncs` reuse

Failed tracker dependency mutations enqueue a row with
`mutation_kind = 'add_blocker'`. The retry runner already exists; v3
adds a deserializer for the payload (an internal `AddBlockerSyncJob`
struct carrying `edge_id`, `blocked`, `blocker`, `reason`) and the
recompute hook described in §4.2.4. No new table is required.

### 4.4 `symphony-tracker`

The existing `TrackerCapabilities.add_blocker` flag and the
`AddBlockerRequest`/`AddBlockerResponse` pair already cover the wire
shape. v3 adds two contracts that adapters must honor:

1. **Linear adapter** must call
   `issueRelationCreate(type: "blocks", issueId: <blocker>, relatedIssueId: <blocked>)`
   for every dependency edge. The conformance suite gains a test that
   asserts both the field names and the direction (a blocker-vs-blocked
   inversion is caught at the test boundary).
2. **GitHub adapter** must continue to report
   `add_blocker = false` and `link_parent_child = false` until a real
   structural API is implemented. Label/body/comment-only behavior
   never flips these flags. v3 adds a *negative* conformance test that
   rejects any GitHub adapter advertising `add_blocker = true`.

No new trait method is needed. The tracker crate's role in v3 is to
keep capability honesty.

### 4.5 `symphony-cli`

Extend two existing surfaces; no new commands are required.

- `symphony issue graph <id>` learns to render the dependency
  sub-graph: per child, the blocker source, local edge status, tracker
  sync status, and the next reconciliation action (§7). A `--json`
  flag emits a stable wire shape for tooling.
- `symphony status` includes a top-level "blocked specialists" tally
  drawn from the same query the dispatch gate runs.

The schedulers in `symphony-cli/src/scheduler.rs` already consume the
core eligibility predicate; the v3 change there is to route the new
"blocked by dependency" outcome into a parked-with-reason status
rather than a generic "not ready".

## 5. Domain State

### 5.1 Decomposition lifecycle

A `PartiallyApplied` status joins the existing
`DecompositionStatus { Proposed, Approved, Applied, Rejected, Cancelled }`
enum to cover the partial-failure state explicitly:

```text
Proposed ─approve─▶ Approved
                     │
                     ├──apply success─▶ Applied
                     │
                     └──apply failure─▶ PartiallyApplied
                                         │
                                         ├──retry success─▶ Applied
                                         └──retry failure─▶ PartiallyApplied
```

`PartiallyApplied` carries the same proposal payload plus a list of
which steps succeeded — child creation, parent_child link,
materialization, tracker sync — so the recovery tick can resume from
the first incomplete step.

### 5.2 Dependency edge lifecycle

```text
                    ┌── tracker_sync = local_only ──▶ synced=local_only
created (open) ─────┤
                    └── add_blocker call
                            │
                            ├── ok    ─▶ synced=synced (tracker_edge_id set)
                            └── error ─▶ synced=failed (queued for retry)

resolved (auto on terminal blocker child)
   │
   └── tracker disagreement ─▶ stays open + warning event
```

### 5.3 Edge direction

Documented once, here, and in `dependency_edge.rs`:

| field      | semantics                  |
|------------|----------------------------|
| `parent_id`| prerequisite (the blocker) |
| `child_id` | dependent (the blocked)    |
| `status=open` | dependency is live       |
| `status=resolved` | dependency cleared    |

This matches the existing `EdgeType::Blocks` doc-comment in
`crates/symphony-state/src/edges.rs` and the `AddBlockerRequest`
field names — both are correct as-is and v3 does not change them.

## 6. Scheduler Architecture

### 6.1 Specialist queue

Pseudocode for the eligibility predicate:

```rust
fn child_eligible_for_specialist(
    child: WorkItemId,
    edges: &dyn WorkItemEdgeRepository,
    work_items: &dyn WorkItemRepository,
    pauses: &dyn BudgetPauseRepository,
    workflow: &WorkflowConfig,
) -> StateResult<Eligibility> {
    // v2 preconditions (unchanged)
    let item = work_items.fetch(child)?;
    if !workflow.is_specialist_routable(&item.status_class) { return parked("not_routable"); }
    if parent_cancelled(&item, work_items)?       { return parked("parent_cancelled"); }
    if pauses.has_active(child)?                  { return parked("budget_paused"); }

    // v3 dispatch gate
    if workflow.decomposition.dependency_policy.dispatch_gate {
        let blockers = edges.list_incoming(child, EdgeType::Blocks)?;
        if let Some(b) = blockers.iter().find(|e| e.status == "open") {
            return parked_with_blocker(b);
        }
    }
    Ok(Eligibility::Ready)
}
```

### 6.2 Sequential chain example (`A → B → C`)

```text
tick 0:  A=ready, B=parked(blocked by A), C=parked(blocked by B)
         dispatch: [A]
tick N:  A=terminal → reconciliation resolves edge(A→B)
         next tick: B=ready, C=parked(blocked by B)
         dispatch: [B]
tick M:  B=terminal → reconciliation resolves edge(B→C)
         next tick: C=ready
         dispatch: [C]
tick P:  parent integration gate fires once A,B,C terminal and no open blockers
```

### 6.3 Parallel-plus-sequential example (`A`, `B`, `C depends_on [A, B]`)

```text
tick 0:  A=ready, B=ready, C=parked(blocked by A,B)
         dispatch: [A, B]   subject to role/concurrency/budget caps
tick N:  A=terminal, B=qa_failed → blocker(A→C) resolved, blocker(B→C) still open
         C=parked(blocked by B)
tick M:  B re-runs, eventually terminal → blocker(B→C) resolved
         C=ready
```

### 6.4 Integration / QA gate interaction

The existing integration-queue gate (`integration.require_no_open_blockers`)
and `blocker_gate` module continue to enforce parent-level closure.
Decomposition-sourced blockers and QA-sourced blockers coexist in the
same edge table; the new `source` column lets the operator surface
distinguish them but the gate logic is provenance-blind: any open
`Blocks` row in the parent's subtree blocks integration.

QA blocker auto-resolution policy is unchanged: a decomposition edge
auto-resolving never triggers a QA blocker auto-resolution and vice
versa, because the auto-resolution rule is scoped to
`source = 'decomposition'`.

## 7. Observability

### 7.1 `symphony issue graph <id>` output

Required minimum (`SPEC_v3.md §11`):

```text
Parent ABC-100: Add billing workflow
Children:
- ABC-101 schema migration        done
- ABC-102 API implementation      blocked by ABC-101
                                  source=decomposition
                                  local=resolved tracker_sync=pending
                                  next=sync retry
- ABC-103 UI wiring               blocked by ABC-102
                                  source=decomposition
                                  local=open tracker_sync=synced
                                  next=waiting
```

### 7.2 Events

Existing `OrchestratorEvent` gains v3 variants (or a sub-payload field
on existing variants — see §9 ADR):

- `DependencyEdgeCreated { parent, blocker, blocked, source, sync }`
- `DependencyEdgeSynced { edge, tracker_edge_id }`
- `DependencyEdgeSyncFailed { edge, error, attempt }`
- `DependencyEdgeResolved { edge, reason: "auto_terminal" | ... }`
- `SpecialistDispatchBlocked { child, blocker, source }`

These flow to the existing event-bus / SSE / TUI fan-out unchanged.

## 8. Failure Modes

### 8.1 Partial child creation

`AppliedDecomposition` already carries per-child `AppliedChild` entries.
v3 adds a `failed: Vec<FailedChild { key, reason }>` field for the
post-decomposition-applier path. The proposal moves to
`PartiallyApplied`; **no** child is dispatched while any same-proposal
sibling is missing, because dependency materialization for the missing
key cannot be safely emitted.

### 8.2 Dependency sync failure

Three policy branches, as specified:

| `tracker_sync`  | local edge | dispatch | tracker call | retry |
|-----------------|-----------:|---------:|--------------|-------|
| `required`      | persisted  | gated by local edge **and** tracker mirror | required, fail-loud | yes, indefinite until success or operator action |
| `best_effort`   | persisted  | gated by local edge only | attempted, surface warning | yes, finite retry then warn |
| `local_only`    | persisted  | gated by local edge only | not attempted | n/a |

The dispatch gate consults a derived "effectively blocked" flag:

```rust
fn effectively_blocked(edge, policy) -> bool {
    edge.status == "open"
        || (policy.tracker_sync == Required && edge.tracker_sync_status != "synced")
}
```

### 8.3 Cycle / missing key

These remain construction-time invariants on `DecompositionProposal`
(unchanged from v2). v3 adds one assertion: the materializer treats
"target `ChildKey` has no `WorkItemId` mapping" as an internal-error
panic-or-typed-error, never a silent drop, because such a state should
be impossible after `mark_applied`.

## 9. ADRs

### 2026-05-09 — Edge metadata as columns, not a parallel table

Context: SPEC v3 needs each `blocks` edge to carry provenance (source)
and tracker mirror state (`tracker_edge_id`, `sync_status`). The
alternatives considered were (a) widening `work_item_edges` with
nullable columns, (b) adding a `dependency_edges` parallel table joined
on edge id, (c) folding the data into `pending_tracker_syncs`.

Decision: widen `work_item_edges`. The propagation query
(`list_open_blockers_for_subtree`) and the dispatch-gate query both
read a single edge row; a join would force every gate read to touch
two tables forever. Provenance is read every time the operator surface
renders, which is also a single-row read. The columns are nullable for
non-decomposition edges, matching the precedent set by `reason`.

Consequence: an additive migration; `pending_tracker_syncs` is reused
only for the retry queue, not for canonical edge state.

### 2026-05-09 — Decomposition application becomes a recoverable workflow

Context: v2's `apply_decomposition` is one transactional step over
tracker child creation. v3 adds at least two more steps
(materialization, optional tracker sync) which can fail independently.

Decision: introduce `DecompositionStatus::PartiallyApplied` and require
the proposal to carry a record of which steps have completed. The
applier never marks `Applied` until the last required step lands.
Recovery resumes from the first incomplete step.

Consequence: tests and the recovery tick must understand the partial
state. Operator surface must surface "decomposition partially applied,
N steps remaining" so a stuck proposal is visible.

### 2026-05-10 — Partially persisted children stay blocked

Context: child tracker issues may be created and committed to local
`work_items` before dependency edge materialization fails. Rolling those
rows back loses durable evidence of tracker-side mutations, but releasing
them as `ready` risks specialist dispatch before sequencing truth exists.

Decision: state persistence commits child `work_items` plus
`parent_child` edges first with the children in `blocked` status. Only
after local dependency `blocks` edges materialize successfully does the
state layer release those children to `ready`. If dependency
materialization fails, the error carries the persisted children and the
children remain parked for recovery.

Consequence: recovery can resume from durable child state without
recreating tracker issues, and scheduler safety does not depend on a
prompt or convention during partial decomposition application.

### 2026-05-09 — Dispatch gate is local-edge authoritative under `best_effort`

Context: under `tracker_sync: best_effort`, the local row may be `open`
while the tracker mirror is `synced`, or vice versa.

Decision: local-edge `status` is authoritative for the dispatch gate
under `best_effort` and `local_only`. Only `tracker_sync: required`
escalates tracker-sync failure to "effectively blocked". This matches
SPEC v3 §12.2 exactly and prevents the kernel from stalling
indefinitely on a tracker outage when the operator opted for
best-effort.

Consequence: workflows that need bulletproof tracker mirroring must
opt in to `required` and accept tracker-outage induced stalling; the
default `best_effort` keeps the kernel self-driving.

### 2026-05-09 — GitHub Issues stay structurally unsupported

Context: SPEC v3 §8.2 forbids label/body text from masquerading as
structural blockers. SPEC also requires `tracker_sync: required` to
fail validation against an unsupported tracker.

Decision: the GitHub adapter keeps `add_blocker = false` and
`link_parent_child = false`. It MAY render advisory `Blocked by #N`
comments but those never feed the capability flag. A workflow can
choose `local_only` to use GitHub safely; a workflow that selects
`required` against GitHub fails at workflow load.

Consequence: Linear is the only currently-supported path for full
structural orchestration. Adding GitHub native support is a future
adapter project, not a v3 deliverable, and would land as a capability
flag flip plus mutation implementation, not as a label hack.

### 2026-05-10 — Direct child creation requires structural parent links

Context: SPEC v3 requires child issues to be created or linked under
their parent according to `decomposition.child_issue_policy`, while also
forbidding adapters from treating comments, labels, or body text as
structural tracker support.

Decision: `decomposition.child_issue_policy: create_directly` requires a
tracker kind whose capability contract includes native parent/child
links. Trackers without that support, including GitHub Issues and the
fixture mock tracker, must use `propose_for_approval` as the explicit
advisory/local-only parent-link mode.

Consequence: GitHub-backed workflows can still use local Symphony
dependency truth, PR lifecycle, and approval queues, but they cannot
claim direct structural child creation until a real native parent/child
API is implemented and advertised by the adapter.

### 2026-05-09 — Edge auto-resolution is provenance-scoped

Context: terminal-blocker auto-resolution must not silently flip QA or
human blockers.

Decision: `auto_resolve_on_terminal` applies *only* to edges with
`source = 'decomposition'`. QA, follow-up, and human blockers retain
their existing manual or workflow-specific resolution paths.

Consequence: the reconciliation query joins on `source` and the
operator surface always shows source for blocked children so a human
reader can tell why a given edge persists past terminal upstream
state.
