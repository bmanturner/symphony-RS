# Symphony-RS v2 Checklist

One unchecked item per implementation iteration. Each item should land with tests and a commit. **No sub-items. No nesting. No "(Decomposed below)" rollups.** If a task is too large for one coherent commit, decompose it in your head and do the first piece — do not edit the checklist to record the decomposition.

## Phase 0 — v2 Grounding

- [x] Rewrite existing quickstart/sample workflow fixtures to the target schema with roles, routing, polling, hooks, integration, PR, and QA sections.

## Phase 1 — Workflow Config Schema

- [x] Replace the current workflow config shape with `WorkflowConfig`; validate optional `schema_version` where only `1` is accepted and unknown keys are denied.
- [x] Add typed `RoleConfig` with `kind = integration_owner | qa_gate | specialist | reviewer | operator | custom`; `custom` is a role kind, not an adapter extension point.
- [x] Add typed `AgentProfileConfig` decoupled from role names.
- [x] Add `HermesAgentConfig` command/protocol shape and repackage existing tandem support as a composite strategy over configured agents; keep mock variants test-only.
- [x] Add typed `RoutingConfig` and `RoutingRule` with explicit `match_mode = first_match | priority` semantics.
- [x] Add `PollingConfig`, `DecompositionConfig`, `IntegrationConfig`, `PullRequestConfig`, `QaConfig`, `FollowupConfig`, `HooksConfig`, and `ObservabilityConfig`.
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
- [x] Add tests for handoff parsing and repair/failure behavior.

## Phase 8 — Integration Owner Flow

- [x] Add integration queue fed by completed child issues or broad issues requiring consolidation.
- [x] Implement integration-owner run request with child handoffs and branch/workspace claims.
- [x] Add integration record and pull request record persistence.
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
- [x] Distinguish blocking follow-ups from non-blocking follow-ups.
- [x] Add tests for specialist, integration-owner, and QA follow-up creation.

## Phase 11 — Scheduler v2

- [x] Replace single flat poll loop with logical queues (intake, specialist, integration, QA, follow-up approval, budget pause, recovery) under shared `polling.interval_ms` cadence with `jitter_ms`, retiring `PollLoop` as the production entry point.
- [x] Add per-queue dispatch runners (`SpecialistRunner`, `IntegrationDispatchRunner`, `QaDispatchRunner`, `FollowupApprovalRunner`, `BudgetPauseRunner`, `RecoveryRunner`) with bounded-concurrency semaphores, capacity-deferred parking, and reap-completed observability.
- [x] Add durable run leases with atomic acquire/release in `symphony-state`, `LeaseOwner` identity, `LeaseConfig`, and runner wiring across all dispatch runners.
- [x] Add lease heartbeat with periodic renewal during long-running dispatches and typed `HeartbeatPulseObservation` surfaced on every runner.
- [x] Add `ConcurrencyConfig` and `ConcurrencyGate` primitive with four scopes (Global, Role, AgentProfile, Repository) wired into specialist, integration, QA, follow-up approval, and budget-pause runners; recovery runner is exempt per ADR.
- [x] Persist `ScopeContended` observations as durable `OrchestratorEvent::ScopeCapReached` events with leading-edge per-episode dedup, and feed every dispatch runner into the shared broadcaster.
- [x] Add retry policy with `BudgetsConfig`, `RetryPolicyDecision`, durable budget pauses, and `OrchestratorEvent::BudgetExceeded` events bridged into `BudgetPauseDispatchQueue` with per-episode dedup.
- [x] Add typed `RunStatus` enum in `symphony-core` with `valid_transition` table covering legal cooperative-cancel transitions.
- [x] Add DB CHECK constraint on `runs.status` and a typed `update_run_status_typed` API; reshape the untyped `update_run_status(&str)` to flow through the typed gate via `StateError::InvalidRunTransition`.

## Phase 11.5 — Cancellation

- [x] Add `CancelRequest`, `CancelSubject`, and in-memory `CancellationQueue` primitive in `symphony-core::cancellation`.
- [x] Add pure `cancellation_observer` primitive (`CancellationDecision` + `observe_for_run` + `observe_for_run_or_parent`) that turns a `CancellationQueue` lookup into an abort/proceed decision.
- [x] Wire pre-lease cancellation observation into `SpecialistRunner` and `IntegrationDispatchRunner` with `RunCancellationStatusSink` and run-keyed drain semantics.
- [x] Add durable `cancel_requests` table, `CancelRequestRepository`, and write-through + startup hydration so `CancellationQueue` survives restart.
- [x] Wire pre-lease cancellation observation into the QA, follow-up approval, and budget-pause runners under the same contract used by specialist + integration.
- [x] Add Dispatcher-level cooperation point for between-agent-step cancellation checks in long-running dispatches.
- [x] Add `OrchestratorEvent::RunCancelled` with kernel-side `CancellationEventLog` dedup, and a `CancellationPropagator` that cascades work-item cancellation across `WorkItemEdge::ParentChild` to in-flight child runs.
- [x] Add `symphony cancel <id>` CLI subcommand with `--reason`, `--run` / `--issue` flags, idempotent re-runs, and a one-line summary of cascaded targets.

## Phase 12 — Observability and Operator Surfaces

- [x] Add the specific `OrchestratorEvent` variants required to make Phase 13's deterministic scenarios pass. Drive the variant set from test consumers, not from an enumeration of categories.
- [x] Persist every event before broadcasting.
- [x] Update `symphony status` to read durable state, not only tracker snapshot.
- [x] Add `symphony issue graph <id>` for parent/child/blocker graph.
- [x] Add `symphony qa verdict <id>` for QA evidence.
- [x] Add `symphony recover` CLI command that reconciles durable state with tracker/workspaces (per ARCHITECTURE_v2.md §4.7), surfacing reaped expired leases and orphaned workspace claims.
- [x] Add JSON output mode for status commands.
- [x] Move current `status` config under `observability.sse`, preserving bind and replay buffer, and stream from the durable event tail.
- [x] Add TUI panels for queues, blockers, QA, integration branch, and runs.

## Phase 13 — End-to-End Scenarios

- [x] Add deterministic fake test scenario: broad parent decomposes into two child specialist issues, integrates, opens draft PR, passes QA, marks PR ready, closes parent.
- [x] Add deterministic test scenario: QA files blocker, blocker routes to specialist, integration reruns, QA passes.
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
