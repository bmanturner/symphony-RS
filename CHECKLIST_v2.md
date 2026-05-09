# Symphony-RS v2 Checklist

One unchecked item per implementation iteration. Each item should land with tests and a commit.

## Phase 0 — v2 Grounding

- [x] Rewrite existing quickstart/sample workflow fixtures to the target schema with roles, routing, polling, hooks, integration, PR, and QA sections.

## Phase 1 — Workflow Config Schema

- [x] Replace the current workflow config shape with `WorkflowConfig`; validate optional `schema_version` where only `1` is accepted and unknown keys are denied.
- [x] Add typed `RoleConfig` with `kind = integration_owner | qa_gate | specialist | reviewer | operator | custom`; `custom` is a role kind, not an adapter extension point.
- [x] Add typed `AgentProfileConfig` decoupled from role names.
- [x] Add `HermesAgentConfig` command/protocol shape and repackage existing tandem support as a composite strategy over configured agents; keep mock variants test-only.
- [x] Add typed `RoutingConfig` and `RoutingRule` with explicit `match_mode = first_match | priority` semantics.
- [x] Add `PollingConfig`, `DecompositionConfig`, `IntegrationConfig`, `PullRequestConfig`, `QaConfig`, `FollowupConfig`, `HooksConfig`, and `ObservabilityConfig`. (Decomposed below.)
  - [x] Extend `PollingConfig` with `jitter_ms` and `startup_reconcile_recent_terminal` per SPEC v2 §5.2.
  - [x] Add typed `DecompositionConfig` (SPEC v2 §5.7) with `triggers` substructure and `child_issue_policy` enum.
  - [x] Add typed `IntegrationConfig` (SPEC v2 §5.10) with `merge_strategy`, `conflict_policy`, and `required_for` enums.
  - [x] Add typed `PullRequestConfig` (SPEC v2 §5.11) with `open_stage`, `mark_ready_stage`, and `initial_state` enums and templated title/body.
  - [x] Add typed `QaConfig` (SPEC v2 §5.12) with `blocker_policy` enum, `waiver_roles`, and `evidence_required` substructure.
  - [x] Add typed `FollowupConfig` (SPEC v2 §5.13) sharing the policy enum with decomposition.
  - [x] Add typed `ObservabilityConfig` (SPEC v2 §5.14) and migrate `StatusConfig` under `observability.sse`, preserving the existing `bind`/`replay_buffer` validation.
  - [x] Reshape `HooksConfig` to v2 list-of-commands form (SPEC v2 §5.17) and update `symphony-workspace` and `symphony-cli` consumers to iterate.
- [x] Add `WorkspacePolicyConfig` and `BranchPolicyConfig` for worktree/shared-branch strategies.
- [x] Add validation errors: missing `kind: integration_owner`, missing `kind: qa_gate` when QA is required, unknown role/agent references, duplicate tracker state mappings, invalid max depth, invalid branch template, PR config without GitHub, and contradictory shared-branch policy.
- [x] Add round-trip tests for a full v2 workflow fixture.
- [x] Add negative tests for unknown nested keys, dangling role references, and invalid strategy combinations.

## Phase 2 — Durable State

- [x] Add `crates/symphony-state` with SQLite-backed `rusqlite` migrations.
- [x] Create migrations for `work_items`, `work_item_edges`, `runs`, `workspace_claims`, `handoffs`, `qa_verdicts`, `events`, pending tracker syncs, and budget pauses.
- [x] Add repository traits for work items and runs.
- [x] Add append-only event repository with monotonically increasing sequence.
- [x] Add transaction helper for state transition + event append.
- [x] Add recovery query for expired leases/runs.
- [x] Add tests proving state survives process restart using a temp SQLite DB.

## Phase 3 — Core Domain Types

- [x] Add `WorkItem`, `WorkItemStatusClass`, and `WorkItemId` to `symphony-core`, including tracker-state-to-status-class mapping and case-insensitive raw state handling.
- [x] Add `RoleKind`, `RoleContext`, and role authority flags.
- [x] Add `Blocker`, `BlockerStatus`, and blocker edge invariants.
- [x] Add `Handoff` and `ReadyFor` structured output type.
- [x] Add `QaVerdict`, `QaEvidence`, and acceptance-criteria trace types.
- [x] Add `FollowupIssueRequest` and `FollowupPolicy` types.
- [x] Add `IntegrationRecord` for canonical branch/worktree consolidation.
- [x] Add serialization tests for all public domain types.

## Phase 4 — Tracker Capabilities

- [x] Split current `IssueTracker` into `TrackerRead` and `TrackerMutations` traits, preserving `fetch_terminal_recent` recovery semantics.
- [x] Add `TrackerCapabilities` so workflows can detect read-only vs mutation-capable adapters.
- [x] Add mutation request/response types: create issue, update issue, add comment, add blocker, link parent/child, attach artifact.
- [x] Implement mutation no-op/advisory wrapper for read-only trackers.
- [x] Extend GitHub adapter with issue create/comment/label-based blocker or relation mapping where feasible.
- [x] Extend Linear adapter with issue create/comment/dependency mapping where feasible.
- [x] Keep product adapter scope limited to git, GitHub, Linear, Codex, Claude, and Hermes.
- [x] Add conformance tests for read adapter behavior and separate mutation conformance tests for capable adapters.

## Phase 5 — Routing and Decomposition

- [x] Implement routing engine that maps normalized work items to roles from workflow config.
- [x] Add tests for first-match and priority-based routing.
- [x] Add decomposition proposal type with child scopes, role assignments, dependencies, and acceptance criteria.
- [x] Implement integration-owner decomposition runner path.
- [x] Add child issue creation through `TrackerMutations`, gated by `decomposition.child_issue_policy`.
- [x] Add parent/child edge persistence and blocker propagation.
- [x] Add guard preventing parent completion while required children are non-terminal.

## Phase 6 — Workspace and Branch Claims

- [x] Replace `WorkspaceManager::ensure` / path-only workspace return with `WorkspaceClaim` containing path, strategy, base ref, branch, owner, cleanup policy, and verification report.
- [x] Add `git_worktree` strategy with branch template expansion using the git adapter.
- [x] Add `existing_worktree` strategy with required branch verification.
- [x] Add shared integration branch strategy for explicit same-branch workflows.
- [x] Add cwd verification immediately before agent launch.
- [x] Add git branch/ref verification immediately before mutation-capable runs.
- [x] Add clean-tree policy enforcement.
- [x] Add tests for path traversal, wrong cwd, wrong branch, dirty tree, and shared branch policy.

## Phase 7 — Structured Agent Handoffs

- [x] Define agent output schema for handoffs, blockers, follow-ups, verdict requests, and `ready_for` queue consequences.
- [x] Update prompt rendering to include role context, workspace claim, parent/child context, blockers, acceptance criteria, and output schema.
- [x] Replace tiny string substitution with the strict built-in `{{path.to.value}}` renderer that fails on unknown variables.
- [x] Add malformed-handoff handling: fail run or request repair turn by policy.
- [x] Persist handoffs and expose them in `symphony status`.
  - [x] Add `HandoffRepository` to `symphony-state` (insert + list-by-work-item + latest), backed by the existing `handoffs` table, with composition through `StateTransaction`.
  - [x] Wire `symphony status` to surface the latest persisted handoff per work item when a state DB is configured.
- [x] Add tests for handoff parsing and repair/failure behavior.

## Phase 8 — Integration Owner Flow

- [x] Add integration queue fed by completed child issues or broad issues requiring consolidation.
- [x] Implement integration-owner run request with child handoffs and branch/workspace claims.
- [x] Add integration record and pull request record persistence.
  - [x] Add `integration_records` table migration and `IntegrationRecordRepository` to `symphony-state`.
  - [x] Add `PullRequestRecord` domain type to `symphony-core` and `pull_request_records` table migration plus `PullRequestRecordRepository` to `symphony-state`.
- [x] Add git integration operation abstraction in `symphony-workspace` using the git CLI: merge/cherry-pick/shared-branch verification and push.
- [x] Add gate requiring all child issues terminal before integration unless explicitly waived.
- [x] Add gate requiring no open blockers before draft PR creation and QA request.
- [x] Add tests for child completion, blocker prevention, draft PR creation, successful integration handoff, merge conflict repair, and conflict/block path.

## Phase 9 — QA Gate Flow

- [x] Add QA queue fed by draft PRs/integration handoffs or direct specialist handoffs for simple issues.
- [x] Implement QA run request with draft PR ref, final branch/workspace, acceptance trace, changed files, CI/check status, and prior handoffs.
- [x] Add QA verdict persistence (widen `qa_verdicts.verdict` CHECK to the five SPEC §4.9 verdicts and persist `role`, `waiver_role`, `reason` alongside the existing evidence/trace columns).
- [x] Add blocker creation from QA verdict when failures are found.
- [x] Add policy: QA blockers block parent completion by default; QA waivers require a configured waiver role and reason.
- [x] Add rework routing after QA failure.
- [x] Add tests for QA pass, QA fail with blockers, inconclusive verdict, and waiver policy.

## Phase 10 — Follow-up Issue Flow

- [x] Add follow-up issue proposal/creation API in core.
- [x] Allow all roles to emit follow-up requests in structured handoff.
- [x] Apply shared workflow policy enum: `create_directly` vs `propose_for_approval`, routed to the follow-up approval queue when approval is required.
- [x] Link follow-ups to source work item.
- [ ] Distinguish blocking follow-ups from non-blocking follow-ups.
- [ ] Add tests for specialist, integration-owner, and QA follow-up creation.

## Phase 11 — Scheduler v2

- [ ] Replace single flat poll loop with logical queues: intake, specialist, integration, QA, follow-up approval, budget pause, recovery; keep cadence under `polling.interval_ms`.
- [ ] Add durable leases for running work.
- [ ] Add lease heartbeat and expiration handling.
- [ ] Add global/role/agent/repository concurrency limits.
- [ ] Add retry policy with max retries, budget awareness, durable budget pauses, and `BudgetExceeded` events.
- [ ] Add cancellation path that records durable run status.
- [ ] Add recovery command that reconciles leases, workspaces, tracker state, and runs.

## Phase 12 — Observability and Operator Surfaces

- [ ] Extend existing `OrchestratorEvent` in place to cover work item, run, blocker, integration, PR, QA, follow-up, and budget events.
- [ ] Persist every event before broadcasting.
- [ ] Update `symphony status` to read durable state, not only tracker snapshot.
- [ ] Add `symphony issue graph <id>` for parent/child/blocker graph.
- [ ] Add `symphony qa verdict <id>` for QA evidence.
- [ ] Add JSON output mode for status commands.
- [ ] Move current `status` config under `observability.sse`, preserving bind and replay buffer, and stream from the durable event tail.
- [ ] Add TUI panels for queues, blockers, QA, integration branch, and runs.

## Phase 13 — End-to-End Scenarios

- [ ] Add deterministic fake test scenario: broad parent decomposes into two child specialist issues, integrates, opens draft PR, passes QA, marks PR ready, closes parent.
- [ ] Add deterministic test scenario: QA files blocker, blocker routes to specialist, integration reruns, QA passes.
- [ ] Add deterministic test scenario: specialist files non-blocking follow-up while current work proceeds.
- [ ] Add deterministic test scenario: dirty/wrong branch blocks agent launch.
- [ ] Add deterministic test scenario: integration owner cannot close parent with unresolved child.
- [ ] Add crash/restart scenario proving durable recovery of queued/running/blocked work.

## Phase 14 — Documentation and Productization

- [ ] Write `docs/workflow.md` with full schema examples.
- [ ] Write `docs/roles.md` explaining integration owner, QA gate, and configurable specialists.
- [ ] Write `docs/workspaces.md` explaining worktree/shared-branch policies.
- [ ] Write `docs/qa.md` explaining verdicts, blockers, evidence, and waivers.
- [ ] Write upgrade note: rewrite current fixtures, fold current v1 docs into target docs, repackage tandem as a composite strategy, keep mock code test-only, and retire dangling old checklist items.
- [ ] Add quickstart fixture and README walkthrough for v2 workflow.
