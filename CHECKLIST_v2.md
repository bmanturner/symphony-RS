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
- [x] Distinguish blocking follow-ups from non-blocking follow-ups.
- [x] Add tests for specialist, integration-owner, and QA follow-up creation.

## Phase 11 — Scheduler v2

- [x] Decompose the flat-poll-loop replacement into logical-queue subtasks (this entry).
  - [x] Add `LogicalQueue` enum (`intake`, `specialist`, `integration`, `qa`, `followup_approval`, `budget_pause`, `recovery`) plus `QueueTickOutcome` to `symphony-core` with serialization tests.
  - [x] Add a `QueueTick` trait abstraction in `symphony-core` describing one per-queue tick (input cadence config, output outcome) with a deterministic test harness.
  - [x] Add an intake queue tick that wraps the existing tracker-poll/active-set fetch behind the new abstraction without changing observable behavior, with tests proving parity with the current flat poll.
  - [x] Add a specialist queue tick that claims routable specialist work items and emits dispatch requests, with tests for first-match and priority routing parity.
  - [x] Add an integration queue tick that drains `IntegrationQueueRepository` entries respecting gates, with tests for ready/blocked/waived cases.
  - [x] Add a QA queue tick that drains `QaQueueRepository` entries respecting gates, with tests for ready/blocked cases.
  - [x] Add a follow-up approval queue tick that drains pending approval-routed follow-ups, with tests for approve/reject paths.
  - [x] Add a budget-pause queue tick that surfaces durable budget pauses for operator/policy resume, with tests for resume conditions.
  - [x] Add a recovery queue tick that reconciles expired leases and orphaned workspace claims on each cadence (separate from the operator `recovery` command), with tests for lease expiry reaping.
  - [x] Wire `Scheduler v2` that fans the queue ticks under shared `polling.interval_ms` cadence with `jitter_ms`, replacing the flat `PollLoop` entry point and migrating its existing reconciliation/cancellation semantics to the intake + recovery queues.
- [x] Replace single flat poll loop with logical queues: intake, specialist, integration, QA, follow-up approval, budget pause, recovery; keep cadence under `polling.interval_ms`. (Decomposed below.)
  - [x] Add `SpecialistRunner` that drains `SpecialistDispatchQueue` and invokes the existing `Dispatcher` trait under a max-concurrency `Semaphore`, with parking for capacity-deferred requests, missing-issue counting, parent-cancellation propagation, and reap-completed observability.
  - [x] Add an `IntegrationDispatchRunner` over `IntegrationDispatchQueue` mirroring the specialist runner's bounded-concurrency contract, with tests for ready/blocked candidates.
  - [x] Add a `QaDispatchRunner` over `QaDispatchQueue` mirroring the runner contract, with tests for ready/blocked candidates and waiver-routed verdicts.
  - [x] Add a `FollowupApprovalRunner` and a `BudgetPauseRunner` (operator-decision queues) mirroring the runner contract, with tests for approve/reject and resume paths.
  - [x] Add a `RecoveryRunner` that drains `RecoveryDispatchQueue` with parity tests against the flat-loop reconciliation contract.
  - [x] Wire the per-queue runners and the new ticks into `symphony-cli/src/run.rs` behind `SchedulerV2`, retiring `PollLoop` as the production entry point and migrating SIGINT drain semantics to the runners' reap surface. (Decomposed below.)
    - [x] Add a `symphony-cli` `scheduler` builder module that constructs a `SchedulerV2` from a `WorkflowConfig` plus a `TrackerRead` and returns `(SchedulerV2, Arc<ActiveSetStore>)` populated with the `IntakeQueueTick` only, with tests proving the scheduler advances under the configured cadence and republishes the active set into the store. Not yet integrated into `run.rs`.
    - [x] Extend the `scheduler` builder to register a `RecoveryQueueTick` driven by the shared `ActiveSetStore`, with tests proving expired-lease/orphaned-claim reconciliation parity against the flat-loop contract.
    - [x] Extend the `scheduler` builder to register a `SpecialistQueueTick` paired with a `SpecialistRunner` over a shared `SpecialistDispatchQueue`, reusing the existing `SymphonyDispatcher` under the configured `agent.max_concurrent_agents`, with tests for capacity-deferred parking, parent-cancellation propagation, and reap-completed observability.
    - [x] Extend the `scheduler` builder to register an `IntegrationQueueTick` paired with an `IntegrationDispatchRunner`, with tests for ready/blocked/waived gating end-to-end.
    - [x] Extend the `scheduler` builder to register a `QaQueueTick` paired with a `QaDispatchRunner`, with tests for ready/blocked candidates and waiver-routed verdicts end-to-end.
    - [x] Extend the `scheduler` builder to register `FollowupApprovalQueueTick` + `FollowupApprovalRunner` and `BudgetPauseQueueTick` + `BudgetPauseRunner`, with tests for approve/reject and resume paths end-to-end.
    - [x] Promote the test-only `Empty{Integration,Qa,FollowupApproval,BudgetPause}Source` and `Noop{Integration,Qa,FollowupApproval,BudgetPause}Dispatcher` helpers in `crates/symphony-cli/src/scheduler.rs` to `pub(crate)` production stand-ins (with TODO pointers to the state-crate adapters that will replace them) so `run.rs` can compose `build_scheduler_v2` before state-backed sources land. Keep `Recording*` and `GateAware*` test-only.
    - [x] Switch `symphony-cli/src/run.rs` from `PollLoop` to the `SchedulerV2` builder, migrating SIGINT drain semantics to the runners' reap surface and retiring `PollLoop` as the production entry point. Keep `PollLoop` exported for parity tests until phase close.
- [x] Decompose durable-lease wiring for running work into subtasks (this entry).
- [x] Add durable leases for running work. (Decomposed below.)
  - [x] Add `RunRepository::acquire_lease(run_id, owner, expires_at)` and `release_lease(run_id)` APIs in `symphony-state` that atomically gate on the current lease holder (refuse to acquire when an unexpired non-self lease is held; clear `lease_owner`/`lease_expires_at` on release), with tests for fresh acquisition, idempotent re-acquisition by the same owner, contention rejection, expired-lease takeover, and release semantics.
  - [x] Extend `StateTransaction` with composition helpers so lease acquisition pairs atomically with `update_run_status` + event append in one transaction, with tests proving the lease and status/event rows commit/rollback together.
  - [x] Define a `LeaseOwner` identity convention (process/host + scheduler instance + worker slot) in `symphony-core` and a `LeaseConfig` (default ttl, renewal margin) under `polling`/scheduler config, with serialization tests.
  - [x] Decompose runner lease-wiring into per-runner subtasks (this entry).
  - [ ] Wire lease acquisition into the production runners (`SpecialistRunner`, `IntegrationDispatchRunner`, `QaDispatchRunner`, `FollowupApprovalRunner`, `BudgetPauseRunner`, `RecoveryRunner`): acquire on dispatch start, release on terminal completion (success, failure, or cancellation), with tests proving leases are written, observable in `RunRepository::find_expired_leases` only after expiry, and cleared on terminal status. (Decomposed below.)
    - [x] Add a `RunLeaseStore` trait + `RunLeaseGuard` RAII helper to `symphony-core` so runners depend on a narrow async trait (`acquire`, `release`) rather than the `RunRepository` directly, with a deterministic in-memory fake and tests for acquire-on-dispatch/release-on-terminal/release-on-drop semantics.
    - [x] Add a `RunLeaseStore` adapter in `symphony-state` over `RunRepository::{acquire_lease, release_lease}` (locked behind the existing `Mutex<dyn RunRepository>` composition), with tests against a temp SQLite DB proving leases appear in `find_expired_leases` only after expiry and are cleared on `release`.
    - [x] Wire `SpecialistRunner` to accept an optional `Arc<dyn RunLeaseStore>` + `LeaseOwner` + `LeaseConfig`: acquire on dispatch start, release on terminal completion (Completed/Failed/Canceled), with tests proving leases are written for in-flight dispatch, expire only after `ttl`, and clear on every terminal `ReleaseReason`.
    - [x] Wire `IntegrationDispatchRunner` lease acquisition under the same contract, with tests for write/expiry/cleared-on-terminal across ready/blocked/waived dispatch outcomes.
    - [x] Wire `QaDispatchRunner` lease acquisition under the same contract, with tests for write/expiry/cleared-on-terminal across pass/fail/inconclusive/waiver verdicts.
    - [x] Wire `FollowupApprovalRunner` lease acquisition under the same contract, with tests for write/expiry/cleared-on-terminal across approve/reject paths.
    - [x] Wire `BudgetPauseRunner` lease acquisition under the same contract, with tests for write/expiry/cleared-on-terminal across resume/hold paths.
    - [x] Wire `RecoveryRunner` lease acquisition under the same contract (recovery dispatches are themselves runs), with tests for write/expiry/cleared-on-terminal across reaped/orphaned outcomes.
  - [x] Surface lease acquisition failures (contention, missing run row) as a typed dispatch outcome in each runner so the scheduler can park or reroute without losing the dispatch request, with tests for contention parking and missing-run dropping.
- [x] Add lease heartbeat and expiration handling. (Decomposed below.)
  - [x] Add a `LeaseHeartbeat` helper to `symphony-core::run_lease` that tracks a held lease's `(owner, run_id, last_expires_at, renewal_margin_ms)`, exposes `due(now)` (lex compare with the renewal-margin window expressed via `LeaseClock`) and `pulse(clock, ttl_ms)` (re-acquires through `RunLeaseStore::acquire`, updates `last_expires_at` on `Acquired`, returns the underlying `LeaseAcquireOutcome` on contention/not-found, propagates backend errors), with deterministic tests covering not-due/due transitions, pulse-extends-expiry, contention preserves prior expiry, not-found surfaces outcome, and backend-error propagation.
  - [x] Wire `LeaseHeartbeat` into each production runner so in-flight dispatches periodically refresh their lease before TTL elapses, with tests proving `runs.lease_expires_at` advances during a long-running dispatch and that pulse failures (contention, missing row, backend error) surface as typed runner observations rather than silent drops. (Decomposed below.)
    - [x] Decompose runner heartbeat-wiring into per-runner subtasks plus a shared scaffolding entry (this item).
    - [x] Add a shared `HeartbeatPulseObservation` typed outcome in `symphony-core::run_lease` (variants for `Renewed`, `Contended { holder, expires_at }`, `NotFound`, `BackendError { message }`) plus a thin `pulse_observed(&mut LeaseHeartbeat, &dyn LeaseClock, ttl_ms)` adapter that maps `LeaseHeartbeat::pulse` results onto the typed outcome, with deterministic tests covering each variant.
    - [x] Wire `LeaseHeartbeat` into `SpecialistRunner`: thread an optional `(LeaseConfig, Arc<dyn LeaseClock>)` alongside the existing lease store, hold a `LeaseHeartbeat` for the duration of dispatch, pulse on a runner-driven cadence (`due(clock)` gated), and surface `HeartbeatPulseObservation` as a typed in-flight runner observation rather than dropping the dispatch silently. Tests prove `runs.lease_expires_at` advances during a long-running dispatch, contention/not-found/backend-error pulses each surface as the typed observation, and terminal release still clears the lease exactly once.
    - [x] Wire `LeaseHeartbeat` into `IntegrationDispatchRunner` under the same contract, with tests across ready/blocked/waived dispatch outcomes proving expiry advances mid-flight and pulse failures surface typed observations.
    - [x] Wire `LeaseHeartbeat` into `QaDispatchRunner` under the same contract, with tests across pass/fail/inconclusive/waiver verdicts proving expiry advances mid-flight and pulse failures surface typed observations.
    - [x] Wire `LeaseHeartbeat` into `FollowupApprovalRunner` under the same contract, with tests across approve/reject paths proving expiry advances mid-flight and pulse failures surface typed observations.
    - [x] Wire `LeaseHeartbeat` into `BudgetPauseRunner` under the same contract, with tests across resume/hold paths proving expiry advances mid-flight and pulse failures surface typed observations.
    - [x] Wire `LeaseHeartbeat` into `RecoveryRunner` under the same contract (recovery dispatches are themselves runs), with tests across reaped/orphaned outcomes proving expiry advances mid-flight and pulse failures surface typed observations.
  - [x] Surface expired-lease reap candidates from `RunRepository::find_expired_leases` into the `RecoveryQueueTick` source view so the recovery runner dispatches them, with tests proving expired leases trigger recovery dispatches once, released leases do not, and renewals (refreshed `lease_expires_at`) prune already-claimed candidates.
- [ ] Add global/role/agent/repository concurrency limits. (Decomposed below.)
  - [x] Decompose multi-scope concurrency limits into config-schema, primitive, runner-wiring, and observability subtasks plus a shared scaffolding entry (this item). Scope spans four caps (global, role, agent-profile, repository), a kernel-level permit gate, and integration into every dispatch runner; the decomposition fixes the contract before code lands so each subtask is one focused commit. Resulting subtasks listed below.
  - [x] Extend `WorkflowConfig` with a typed `ConcurrencyConfig` block under `agent` (or top-level `concurrency`) that captures: (a) a `global_max` mirroring today's `agent.max_concurrent_agents`, (b) a `per_agent_profile: BTreeMap<String, u32>` keyed by `WorkflowConfig::agents` profile names, (c) a `per_repository: BTreeMap<String, u32>` keyed by repo slug, plus a deprecation alias keeping `agent.max_concurrent_agents` working. Per-role caps already live on `roles.<name>.max_concurrent` and stay there. Add `deny_unknown_fields` coverage, validation that every key resolves to a known profile/repo, and tests covering defaults, alias precedence, unknown-key rejection, and zero-value rejection.
  - [x] Add a `ConcurrencyGate` primitive in `symphony-core` that holds named scope semaphores keyed by `(ScopeKind, ScopeKey)` (kinds: `Global`, `Role`, `AgentProfile`, `Repository`) and exposes `try_acquire(&[ScopeRequest]) -> Option<ScopePermitSet>` with deterministic all-or-nothing acquire (release any partial acquisitions on failure), a `release_on_drop` permit set, and explicit `available(scope)` introspection. Pure unit tests cover: cap respected per scope, contention on one scope releases prior partials, missing scope keys default to unbounded, repeated acquire/release cycles preserve counts, and zero-cap scopes always reject.
  - [x] Plumb `ConcurrencyGate` into the runner-construction surface (likely via the dispatcher trait or a new `RunnerScopes` argument): each dispatch derives `ScopeRequest`s from the work item's `(role, agent_profile, repository)` triple and acquires before invoking the agent, releasing on terminal completion. Tests prove the request shape (which scopes appear with which keys) without yet changing runner behavior.
  - [x] Wire `ConcurrencyGate` into `SpecialistRunner` so dispatches acquire `{Global, Role, AgentProfile, Repository}` permits before running and release on terminal observation. Surface "scoped cap reached" as a typed in-flight observation (e.g. `ScopeContended { scope_kind, scope_key, in_flight, cap }`) rather than silently dropping or busy-looping. Tests prove caps are respected (N concurrent issues with cap=K hold K and defer the rest), the typed observation fires for the binding scope, and terminal release frees permits exactly once.
  - [x] Wire `ConcurrencyGate` into `IntegrationDispatchRunner` under the same contract, with tests proving role-cap=1 (the default for integration owners) serializes integration even when the global cap allows more, and cross-scope contention (per-repository) defers as a typed observation.
  - [x] Wire `ConcurrencyGate` into `QaDispatchRunner` under the same contract, with tests proving QA respects role cap independently of specialist cap and surfaces typed contention observations.
  - [ ] Wire `ConcurrencyGate` into `FollowupApprovalRunner`, `BudgetPauseRunner`, and `RecoveryRunner` under the same contract — or, per runner, document in code comments and an ADR in `ARCHITECTURE_v2.md` why a given runner is exempt (e.g. recovery dispatches are themselves runs and re-enter the gate via the dispatched runner). Tests cover the chosen behavior for each runner. (Decomposed below.)
    - [x] Decompose ancillary-runner gate wiring into per-runner subtasks plus an ADR-anchor entry (this item). Each of the three runners (followup-approval, budget-pause, recovery) is its own focused commit so the wire-or-exempt decision and its tests/ADR land atomically. Resulting subtasks listed below.
    - [x] Decide and apply gate wiring for `FollowupApprovalRunner`: extend `FollowupApprovalDispatchRequest` with optional `(role, agent_profile, repository)` and wire `ConcurrencyGate` mirroring `QaDispatchRunner` (acquire after local capacity / before lease, contended scope drops permit and parks request, terminal release drops permits exactly once, role-less requests bypass), with `ScopeContendedObservation` surfaced on `FollowupApprovalRunReport` and tests for approval-role cap serializing dispatch independent of QA/specialist caps, per-repository contention parking without consuming local capacity, no-gate passthrough, and role-less requests bypassing the gate. Add an ADR to `ARCHITECTURE_v2.md` recording that follow-up approval is gated under the configured `followups.approval_role`.
    - [x] Decide and apply gate wiring for `BudgetPauseRunner`: either wire `ConcurrencyGate` mirroring the QA contract (with `ScopeContendedObservation` on the run report and the same acquire/release ordering and tests for cap-respected, contention-not-consuming-capacity, no-gate passthrough, role-less bypass) or document the runner's exemption in code comments and a focused ADR in `ARCHITECTURE_v2.md` justifying the bypass on workflow-invariant grounds (e.g. budget gating is policy enforcement, not specialist work). The chosen path includes tests proving the documented behavior.
    - [x] Decide and apply gate wiring for `RecoveryRunner`: per the original guidance, document the runner's exemption in code comments and a focused ADR in `ARCHITECTURE_v2.md` capturing that recovery dispatches are themselves runs and re-enter the gate via the dispatched runner (specialist/integration/QA/followup-approval), with tests proving the recovery-runner pass itself does not consume scope permits and that downstream dispatches still acquire their own permits via the gated runner.
  - [x] Persist `ScopeContended` observations as durable orchestrator events (`ScopeCapReached { run_id, scope_kind, scope_key, in_flight, cap }`) so operators can answer "why is this issue not running" from event history, with serialization tests and a poll-loop integration test proving the event is emitted exactly once per contention episode (not once per tick). Landed as `OrchestratorEvent::ScopeCapReached` (snake_case wire tag, optional `run_id` + `identifier` so follow-up/budget-pause runners can flow), `ScopeKind` gains `Serialize/Deserialize`, and a new `scope_contention_log::ScopeContentionEventLog` performs leading-edge dedup per `(subject, scope_kind, scope_key)` episode. Tests cover wire serialization, dedup over a 25-tick simulated poll loop with two episodes, episode end-and-resume, distinct subjects, distinct scopes, and within-pass duplicate coalescing. Wiring runners to feed the log is split into a follow-up subtask below.
  - [ ] Wire each runner (specialist, integration, QA, follow-up approval, budget pause) to feed its per-pass `scope_contentions` into a shared `ScopeContentionEventLog`, broadcast the resulting `OrchestratorEvent::ScopeCapReached` events on `EventBus`, and persist them via `EventRepository::append_event`. Tests prove the durable log contains exactly one row per episode across multi-pass runner driving, and that events carry the correct `run_id` for run-based runners and `identifier` for follow-up/budget-pause runners. (Decomposed below.)
    - [x] Decompose runner→event-log wiring into a shared sink-broadcaster scaffolding entry plus per-runner subtasks (this item). `symphony-core` does not depend on `symphony-state`, so persistence is exposed as a kernel-side `ScopeContentionEventSink` trait that the composition root implements against `EventRepository::append_event`. Each runner is its own focused commit so the wiring + tests for one runner land atomically. Resulting subtasks listed below.
    - [x] Add a shared `ScopeContentionEventBroadcaster` in `symphony-core` that owns a `Mutex<ScopeContentionEventLog>`, holds a clone of `EventBus`, and accepts an optional `Arc<dyn ScopeContentionEventSink>` (kernel-side trait with one method: persist an `OrchestratorEvent::ScopeCapReached` and return the assigned sequence). Expose `observe_run_pass(&self, &[ScopeContendedObservation])` and `observe_identifier_pass(&self, &[ScopeContendedObservation])` helpers that lift each runner's typed observation type onto `ContentionObservation`, dedup through the shared log, broadcast each emitted event on the bus, and persist via the sink. Pure unit tests cover: leading-edge dedup across multi-pass driving, broadcast carries the correct `run_id` vs `identifier` payload, sink receives exactly one append per episode, and a missing sink is a no-op for persistence (bus still fires).
    - [x] Wire `SpecialistRunner` to feed its `SpecialistRunReport.scope_contentions` (subject = `ContentionSubject::Run(run_id)` when the request reserved a run row, else `ContentionSubject::Identifier(request.identifier)`) into a `ScopeContentionEventBroadcaster` after each `run_pending` pass. Tests prove the durable sink receives exactly one `ScopeCapReached` per episode across N consecutive contended passes, that `run_id` is populated when the request had one, and that bus subscribers see the same event stream. Implementation note: `SpecialistRunner::ScopeContendedObservation` now carries `run_id: Option<RunRef>`, and a new `ScopeContentionEventBroadcaster::observe_mixed_pass` accepts `(ContentionSubject, ScopeFields)` so a single pass can mix Run- and Identifier-keyed observations without breaking the dedup-per-pass contract. The runner invokes the broadcaster on every `run_pending` pass — including idle passes — so episodes terminate when contention clears.
    - [x] Wire `IntegrationDispatchRunner` to feed its scope contentions into the shared broadcaster under the same contract. Tests prove episode dedup across multi-pass driving and that integration dispatches always carry `run_id` (integration always reserves a run row).
    - [x] Wire `QaDispatchRunner` to feed its scope contentions into the shared broadcaster under the same contract. Tests prove episode dedup across multi-pass driving and that QA dispatches always carry `run_id`.
    - [x] Wire `FollowupApprovalRunner` to feed its scope contentions into the shared broadcaster under the same contract. Subject is `ContentionSubject::Identifier(followup_id.to_string())` because follow-up approval does not reserve a run row. Tests prove dedup across 5 contended passes (1 durable event with `identifier="2"` and `run_id=None`), bus subscribers see the same event the sink persists, and an idle pass between episodes terminates the first and re-contention re-emits with the new follow-up id.
    - [x] Wire `BudgetPauseRunner` to feed its scope contentions into the shared broadcaster under the same contract. Subject is `ContentionSubject::Identifier(request.identifier)`. Tests prove episode dedup across multi-pass driving and that emitted events carry `identifier` (not `run_id`).
- [ ] Add retry policy with max retries, budget awareness, durable budget pauses, and `BudgetExceeded` events. (Decomposed below.)
  - [ ] Decompose retry-policy + `BudgetExceeded` wiring into config-schema, event, retry-policy, and budget-pause-bridge subtasks plus a shared scaffolding entry (this item). The durable budget pause queue and runner already exist (`budget_pause_runner`, `budget_pause_tick`), and `budget_kind="max_retries"` is already the established string label flowing through them. What is missing is (a) a typed `budgets` config block carrying SPEC §5.15 fields including `max_retries`, (b) the `OrchestratorEvent::BudgetExceeded` variant + serialization, (c) a kernel-side retry-cap policy decision in `symphony-core::retry` that returns `BudgetExceeded` instead of inserting a `RetryEntry` when `attempt > max_retries`, and (d) a scheduler-side bridge from cap-reached into a `BudgetPauseDispatchQueue` enqueue keyed on `budget_kind="max_retries"` plus durable broadcast of `BudgetExceeded` with per-episode dedup. Each subtask is one focused commit. Resulting subtasks listed below.
  - [ ] Add a typed `BudgetsConfig` block under `WorkflowConfig` mirroring SPEC §5.15: `max_parallel_runs: u32` (default 6), `max_cost_per_issue_usd: f64` (default 10.00), `max_turns_per_run: u32` (default 20), `max_retries: u32` (default 3), and `pause_policy: BudgetPausePolicy` enum (`block_work_item` | `block_run`, default `block_work_item`). `deny_unknown_fields` coverage, validation that all numeric caps are non-zero (zero is rejected with a clear error pointing at the field), and tests covering defaults, per-field override, unknown-key rejection, zero-value rejection per field, and `pause_policy` round-trip.
  - [ ] Add `OrchestratorEvent::BudgetExceeded { run_id: Option<RunRef>, identifier: String, budget_kind: String, observed: f64, cap: f64 }` with snake_case wire tag `budget_exceeded`, a `kind_label()` arm returning `"budget_exceeded"`, exemption from `expects_lease()` (it is a policy event, not a dispatched run), and round-trip serialization tests covering both `run_id`-bearing (failure-driven cap) and identifier-only (continuation cap) payloads. Update `EventBus` matchers/tests that pattern-match on the enum so the new variant compiles.
  - [ ] Add a `RetryPolicyDecision` typed outcome to `symphony-core::retry` with variants `ScheduleRetry(RetryEntry)` and `BudgetExceeded { observed: f64, cap: f64 }`, plus a pure `RetryQueue::schedule_with_policy(&mut self, ScheduleRequest, &RetryConfig, max_retries: u32) -> RetryPolicyDecision` that returns `BudgetExceeded { observed: attempt as f64, cap: max_retries as f64 }` when `attempt > max_retries` and otherwise inserts and returns `ScheduleRetry`. Tests cover: under-cap schedules normally, at-cap (`attempt == max_retries`) schedules normally, over-cap (`attempt == max_retries + 1`) returns `BudgetExceeded` and does not insert an entry, repeated over-cap calls do not mutate the queue, and the pre-existing `schedule()` method remains a thin wrapper that ignores the cap so callers that have not migrated keep their current semantics.
  - [ ] Bridge cap-reached decisions into the existing `BudgetPauseDispatchQueue` from the scheduler: when the retry-driving code receives `RetryPolicyDecision::BudgetExceeded { observed, cap }`, it constructs a `BudgetPauseCandidate { budget_kind: "max_retries".into(), observed, cap, .. }`, enqueues a `BudgetPauseDispatchRequest` (preserving any reserved `run_id`), persists `OrchestratorEvent::BudgetExceeded` via `EventRepository::append_event`, and broadcasts on `EventBus`. Episode dedup is performed by a kernel-side `BudgetExceededEventLog` with leading-edge dedup keyed on `(ContentionSubject, budget_kind)` mirroring `ScopeContentionEventLog`. Tests prove: cap-exceeded schedule emits exactly one `BudgetExceeded` event per episode, the budget-pause queue holds the request keyed on `("max_retries", issue_id)`, repeated over-cap calls in the same episode dedup the event and do not re-enqueue (the existing budget-pause queue is already idempotent on `(budget_kind, issue_id)`), an idle pass terminates the episode, and a re-cap after termination re-emits.
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
