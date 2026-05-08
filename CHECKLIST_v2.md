# Symphony-RS v2 Checklist

One unchecked item per implementation iteration. Each item should land with tests and a commit.

## Phase 0 — v2 Grounding

- [ ] Add `SPEC_v2.md`, `ARCHITECTURE_v2.md`, `PLAN_v2.md`, `CHECKLIST_v2.md`, and `PROMPT_v2.md` to the repo root.
- [ ] Add README note that v1 files remain current implementation state while v2 files describe the target product direction.
- [ ] Add a `version: 2` fixture workflow under `tests/fixtures/v2-workflow/WORKFLOW.md` with roles, routing, integration, and QA sections.

## Phase 1 — Config v2 Schema

- [ ] Add `WorkflowConfigV2` behind `version: 2` detection in `symphony-config`.
- [ ] Add typed `RoleConfig` with `kind = integration_owner | qa_gate | specialist | reviewer | operator | custom`.
- [ ] Add typed `AgentProfileConfig` decoupled from role names.
- [ ] Add typed `RoutingConfig` and `RoutingRule` with deterministic first-match behavior.
- [ ] Add `DecompositionConfig`, `IntegrationConfig`, `QaConfig`, and `FollowupConfig`.
- [ ] Add `WorkspacePolicyConfig` and `BranchPolicyConfig` for worktree/shared-branch strategies.
- [ ] Add v2 validation errors: missing integration owner role, missing QA role when QA required, unknown role/agent references, invalid max depth, invalid branch template.
- [ ] Add round-trip tests for a full v2 workflow fixture.
- [ ] Add negative tests for unknown nested keys, dangling role references, and invalid strategy combinations.

## Phase 2 — Durable State

- [ ] Add `crates/symphony-state` with SQLite-backed migrations.
- [ ] Create migrations for `work_items`, `work_item_edges`, `runs`, `workspace_claims`, `handoffs`, `qa_verdicts`, and `events`.
- [ ] Add repository traits for work items and runs.
- [ ] Add append-only event repository with monotonically increasing sequence.
- [ ] Add transaction helper for state transition + event append.
- [ ] Add recovery query for expired leases/runs.
- [ ] Add tests proving state survives process restart using a temp SQLite DB.

## Phase 3 — Core Domain Types

- [ ] Add `WorkItem`, `WorkItemStatusClass`, and `WorkItemId` to `symphony-core`.
- [ ] Add `RoleKind`, `RoleContext`, and role authority flags.
- [ ] Add `Blocker`, `BlockerStatus`, and blocker edge invariants.
- [ ] Add `Handoff` and `ReadyFor` structured output type.
- [ ] Add `QaVerdict`, `QaEvidence`, and acceptance-criteria trace types.
- [ ] Add `FollowupIssueRequest` and `FollowupPolicy` types.
- [ ] Add `IntegrationRecord` for canonical branch/worktree consolidation.
- [ ] Add serialization tests for all public domain types.

## Phase 4 — Tracker Capabilities

- [ ] Split current `IssueTracker` into `TrackerRead` and `TrackerMutations` traits while preserving v1 adapter compatibility.
- [ ] Add `TrackerCapabilities` so workflows can detect read-only vs mutation-capable adapters.
- [ ] Add mutation request/response types: create issue, update issue, add comment, add blocker, link parent/child, attach artifact.
- [ ] Implement mutation no-op/advisory wrapper for read-only trackers.
- [ ] Extend GitHub adapter with issue create/comment/label-based blocker or relation mapping where feasible.
- [ ] Add `PaperclipTracker` adapter spike against local Paperclip API or DB, behind a feature flag if needed.
- [ ] Add conformance tests for read adapter behavior and separate mutation conformance tests for capable adapters.

## Phase 5 — Routing and Decomposition

- [ ] Implement routing engine that maps normalized work items to roles from v2 config.
- [ ] Add tests for first-match and priority-based routing.
- [ ] Add decomposition proposal type with child scopes, role assignments, dependencies, and acceptance criteria.
- [ ] Implement integration-owner decomposition runner path.
- [ ] Add child issue creation through `TrackerMutations`, gated by `decomposition.child_issue_policy`.
- [ ] Add parent/child edge persistence and blocker propagation.
- [ ] Add guard preventing parent completion while required children are non-terminal.

## Phase 6 — Workspace and Branch Claims

- [ ] Replace path-only workspace return with `WorkspaceClaim` containing path, strategy, base ref, branch, and verification report.
- [ ] Add `git_worktree` strategy with branch template expansion.
- [ ] Add `existing_worktree` strategy with required branch verification.
- [ ] Add shared integration branch strategy for explicit same-branch workflows.
- [ ] Add cwd verification immediately before agent launch.
- [ ] Add git branch/ref verification immediately before mutation-capable runs.
- [ ] Add clean-tree policy enforcement.
- [ ] Add tests for path traversal, wrong cwd, wrong branch, dirty tree, and shared branch policy.

## Phase 7 — Structured Agent Handoffs

- [ ] Define agent output schema for handoffs, blockers, follow-ups, and verdict requests.
- [ ] Update prompt rendering to include role context, workspace claim, parent/child context, blockers, acceptance criteria, and output schema.
- [ ] Replace tiny string substitution with strict template engine or structured renderer that fails on unknown variables.
- [ ] Add malformed-handoff handling: fail run or request repair turn by policy.
- [ ] Persist handoffs and expose them in `symphony status`.
- [ ] Add tests for handoff parsing and repair/failure behavior.

## Phase 8 — Integration Owner Flow

- [ ] Add integration queue fed by completed child issues or broad issues requiring consolidation.
- [ ] Implement integration-owner run request with child handoffs and branch/workspace claims.
- [ ] Add integration record persistence.
- [ ] Add VCS integration operation abstraction: merge/cherry-pick/shared-branch verification.
- [ ] Add gate requiring all child issues terminal before integration unless explicitly waived.
- [ ] Add gate requiring no open blockers before QA request.
- [ ] Add tests for child completion, blocker prevention, successful integration handoff, and conflict/rework path.

## Phase 9 — QA Gate Flow

- [ ] Add QA queue fed by integration handoffs or direct specialist handoffs for simple issues.
- [ ] Implement QA run request with final branch/workspace, acceptance trace, changed files, and prior handoffs.
- [ ] Add QA verdict persistence.
- [ ] Add blocker creation from QA verdict when failures are found.
- [ ] Add policy: QA blockers block parent completion by default.
- [ ] Add rework routing after QA failure.
- [ ] Add tests for QA pass, QA fail with blockers, inconclusive verdict, and waiver policy.

## Phase 10 — Follow-up Issue Flow

- [ ] Add follow-up issue proposal/creation API in core.
- [ ] Allow all roles to emit follow-up requests in structured handoff.
- [ ] Apply workflow policy: create directly vs propose for approval.
- [ ] Link follow-ups to source work item.
- [ ] Distinguish blocking follow-ups from non-blocking follow-ups.
- [ ] Add tests for specialist, integration-owner, and QA follow-up creation.

## Phase 11 — Scheduler v2

- [ ] Replace single flat poll loop with logical queues: intake, specialist, integration, QA, follow-up approval, recovery.
- [ ] Add durable leases for running work.
- [ ] Add lease heartbeat and expiration handling.
- [ ] Add global/role/agent/repository concurrency limits.
- [ ] Add retry policy with max retries and budget awareness.
- [ ] Add cancellation path that records durable run status.
- [ ] Add recovery command that reconciles leases, workspaces, tracker state, and runs.

## Phase 12 — Observability and Operator Surfaces

- [ ] Extend event model to `OrchestratorEventV2` covering work item, run, blocker, integration, QA, follow-up, and budget events.
- [ ] Persist every event before broadcasting.
- [ ] Update `symphony status` to read durable state, not only tracker snapshot.
- [ ] Add `symphony issue graph <id>` for parent/child/blocker graph.
- [ ] Add `symphony qa verdict <id>` for QA evidence.
- [ ] Add JSON output mode for status commands.
- [ ] Add optional SSE stream from durable event tail.
- [ ] Add TUI panels for queues, blockers, QA, integration branch, and runs.

## Phase 13 — End-to-End Scenarios

- [ ] Add mock scenario: broad parent decomposes into two child specialist issues, integrates, passes QA, closes parent.
- [ ] Add mock scenario: QA files blocker, blocker routes to specialist, integration reruns, QA passes.
- [ ] Add mock scenario: specialist files non-blocking follow-up while current work proceeds.
- [ ] Add mock scenario: dirty/wrong branch blocks agent launch.
- [ ] Add mock scenario: integration owner cannot close parent with unresolved child.
- [ ] Add crash/restart scenario proving durable recovery of queued/running/blocked work.

## Phase 14 — Documentation and Productization

- [ ] Write `docs/workflow-v2.md` with full schema examples.
- [ ] Write `docs/roles.md` explaining integration owner, QA gate, and custom specialists.
- [ ] Write `docs/workspaces.md` explaining worktree/shared-branch policies.
- [ ] Write `docs/qa.md` explaining verdicts, blockers, evidence, and waivers.
- [ ] Write migration note from v1 runner workflow to v2 orchestration workflow.
- [ ] Add quickstart fixture and README walkthrough for v2 mock workflow.
