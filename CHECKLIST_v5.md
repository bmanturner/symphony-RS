# Symphony-RS v5 Checklist — Agent ↔ Kernel Integration via Internal MCP Server

One unchecked item per implementation iteration. Each item should land
with tests and a commit. This checklist implements `SPEC_v5.md` only;
do not duplicate v2 core workflow requirements, v3 dependency
orchestration, or v4 prompt-assembly work.

When this milestone closes, the headline `PLAN_v2.md` claim — *"broad
issue → decomposition → child execution → integration → draft PR → QA
failure → blocker → rework → QA pass → PR ready"* — must run
end-to-end against a live agent (Claude Code, Codex), not only the
mock agent.

## Phase 0 — Hermes Removal

- [x] Delete `HermesAgentConfig` and its `Default` impl from `crates/symphony-config/src/config.rs`.
- [x] Remove the `hermes:` field from `AgentConfig` and from the default fixture.
- [x] Delete any `AgentBackend::Hermes` enum variant and its match arms.
- [x] Remove all `backend: hermes` references from `tests/fixtures/`.
- [x] Add a config validator that fails loud with a clear message when a workflow YAML still references `backend: hermes`, pointing at SPEC v5 §10.
- [x] Update `docs/` and `README.md` to remove Hermes from the supported-backend list.
- [x] Add a regression test proving `backend: hermes` is rejected at config load.

## Phase 1 — MCP Crate Scaffolding

- [x] Evaluate Rust MCP server crates (transport on unix sockets, dynamic per-session tool registration, structured tool-result errors). Pick one. Record the choice in an ADR addendum to `ARCHITECTURE_v5.md`.
- [x] Create the `crates/symphony-mcp` crate as a workspace member with `serde`, `serde_json`, `async-trait`, `thiserror`, the chosen MCP crate, and `symphony-core` as dependencies.
- [x] Define the `ToolHandler`, `ToolResult`, and `ToolError` traits/types in `symphony-mcp::handler`.
- [x] Define the `Server` type with `spawn(SpawnParams)` (returns `McpHandle` RAII guard) and `tools_for(role)` (returns the tool registry for a role profile).
- [x] Implement per-run unix-socket bind/serve/shutdown semantics with stale-socket cleanup on bind retry.
- [x] Add a `ToolRegistry` that maps tool names to handlers, supports per-session enable/disable, and rejects duplicate registrations.
- [x] Add unit tests for socket lifecycle, registry duplicate rejection, and a no-op `Echo` tool round-trip.

## Phase 2 — Symphony-Agent Integration

- [x] Add MCP wiring to `crates/symphony-agent/src/claude/runner.rs::spawn_session`: allocate socket, spawn `symphony-mcp::Server`, materialize `--mcp-config`, append to argv, attach RAII handle to the session.
- [x] Mirror the wiring for `crates/symphony-agent/src/codex/runner.rs`.
- [x] Add the `agents.<profile>.mcp_tools` field to `AgentBackendProfile` (SPEC v5 §4.6.2) with sensible per-`RoleKind` defaults.
- [x] Implement multi-config merging when the operator supplies their own `--mcp-config` via `extra_args`. Preserve operator servers; fail spawn on tool-name conflicts with Symphony's registry.
- [x] Add config-merge unit tests: no operator config, operator config without conflicts, operator config with conflicts (must fail).
- [x] Add an integration test using the mock agent to prove the Symphony MCP socket is bound during a session and torn down on completion.

## Phase 3 — Read Tools

- [x] Extend `TrackerRead` (in `crates/symphony-core/src/tracker_trait.rs`) with `list_comments` and `get_related` default-implemented methods returning `TrackerError::Unsupported`.
- [x] Implement `list_comments` on the Linear and GitHub adapters.
- [x] Implement `get_related` on the Linear and GitHub adapters.
- [x] Add the `IssueComment` typed shape (author, body, created_at, id) and the `RelatedIssues` typed shape (parent, children, blockers) to `symphony-core::tracker`.
- [x] Implement `get_issue_details` MCP handler — pure pass-through to `TrackerRead::get_issue`.
- [x] Implement `list_comments` MCP handler.
- [x] Implement `get_related_issues` MCP handler.
- [x] Implement `list_available_roles` MCP handler — calls `RoleCatalogBuilder::build` (v4) for the live workflow and returns the typed `RoleCatalog`.
- [x] Add tests proving each read handler returns the expected shape against fixture trackers.
- [x] Add a tests proving capability-gated read handlers refuse with `ToolError::CapabilityUnsupported` when the adapter does not advertise the capability.

## Phase 4 — `propose_decomposition` Write Tool

- [x] Add the `DecompositionDraftWire` and `ChildDraftWire` typed shapes in `symphony-mcp` with `#[serde(deny_unknown_fields)]`.
- [x] Implement `ProposeDecompositionHandler` (SPEC v5 §5.1, ARCHITECTURE v5 §6 example).
- [x] Resolve `assigned_role` strings against the live v4 `RoleCatalog`; reject unknown roles with field-pathed `ToolError::InvalidPayload`.
- [x] Validate `depends_on` graph at handler entry: every reference points to a child key in the same payload; no cycles; no duplicate keys.
- [x] On `DecompositionRunner::accept` success, persist via `symphony-state::decomposition_apply` and return the `DecompositionId`, child count, and resulting status.
- [x] On policy-gate refusal (depth, owner role, missing acceptance criteria), return `ToolError::PolicyViolation` with the gate name.
- [x] Add deterministic tests covering: success, unknown role, cyclic depends_on, missing acceptance criteria, max-depth violation.
- [x] Add a cross-backend parity test that verifies Claude and Codex both surface `propose_decomposition` correctly (both backends register the tool; both can serialize a successful tool call).

## Phase 5 — `submit_handoff` Write Tool

- [x] Implement `SubmitHandoffHandler`. Payload deserializes into the existing `crates/symphony-core/src/handoff.rs::Handoff` shape.
- [x] Run `Handoff::validate` on the deserialized value before persisting.
- [x] On success, persist via `HandoffRepository::create_handoff` inside a `transaction.rs` envelope so the run's terminal event and the handoff land atomically.
- [x] Compute the kernel `ReadyForConsequence` and enqueue the next runner accordingly.
- [x] On invalid shape, return `ToolError::InvalidPayload` with the failing field.
- [x] When the agent has called `submit_handoff`, the orchestrator MUST short-circuit the Phase 11 text-extraction path so the handoff is not double-counted.
- [x] Add tests covering: success, invalid shape, double-submit (second call returns idempotency-style success referring to the first row).

## Phase 6 — `record_qa_verdict` Write Tool

- [x] Implement `RecordQaVerdictHandler`. Payload deserializes into the existing `QaVerdict` shape.
- [x] On `fail` / `inconclusive`, file the corresponding blocker per `CHECKLIST_v2.md:99-100`.
- [x] Refuse with `ToolError::CapabilityRefused` when the calling role is not a `qa_gate`.
- [x] Add tests covering: pass, fail, inconclusive, waived (waiver role check), wrong role.

## Phase 7 — `file_followup` Write Tool

- [x] Implement `FileFollowupHandler` mirroring the `followups_created_or_proposed` block of the canonical handoff.
- [x] Honor the workflow's follow-up policy (`SPEC_v2.md` §6).
- [x] Add tests covering: direct creation when policy allows, proposal-for-approval when policy requires.

## Phase 8 — `add_issue_comment` Write Tool and `agent_comments` Table

- [ ] Add the `agent_comments` table migration per ARCHITECTURE v5 §4.3.1, including the unique partial index on `(run_id, idempotency_key)`.
- [ ] Implement `AddIssueCommentHandler`.
- [ ] Wire to the operation queue at `crates/symphony-state/src/migrations.rs:908` so the actual tracker write is deferred and retried.
- [ ] Implement per-run rate limiting with a default cap (recommended: 10) overridable via `mcp.add_issue_comment.max_per_run` workflow config.
- [ ] Implement Symphony byline rendering for `BylineKind::AgentRole`. Per SPEC v5 §5.5 default ON; expose a per-workflow off switch.
- [ ] Implement idempotency via the unique partial index: replay returns the original row's outcome.
- [ ] Add tests covering: success, capability gate (`add_comment: false` adapter), rate-limit hit, idempotency replay, byline rendering on/off.
- [ ] Surface `agent_comments` rows in `symphony status`.

## Phase 9 — Kernel-Driven Tracker Comments

- [ ] Add `crates/symphony-core/src/tracker_comments.rs` with `TrackerCommentRequest`, `BylineKind`, `TrackerCommentSink`, and the `KernelTransitionObserver` implementation per ARCHITECTURE v5 §4.4.1.
- [ ] Subscribe the observer to the existing event bus.
- [ ] Implement the four required transitions per SPEC v5 §7: `human_review` pause, run-failed, QA `fail`/`inconclusive` reasoning, decomposition-applied summary.
- [ ] Wire the optional transitions (specialist `ready_for: integration`, run-started) behind workflow config flags defaulting OFF.
- [ ] Reuse the operation queue so transient tracker outage defers rather than fails the workflow.
- [ ] Add deterministic tests for each required transition: comment row created, byline correct, eventually posted.
- [ ] Add a test proving that a tracker outage during a transition write defers the comment without failing the run.

## Phase 10 — `McpAuthorshipSink` and Authorship Audit

- [ ] Implement `McpAuthorshipSink` (the §5.5 path's sink). Receives requests from `AddIssueCommentHandler` and forwards to the same enqueue path as the kernel observer with `BylineKind::AgentRole(role, run_id)`.
- [ ] Confirm both kernel-driven and agent-driven comments share one rendering path (no byline drift across sources).
- [ ] Add a test that posts one of each kind in the same run and asserts both appear in `agent_comments` with correct `author_kind`.

## Phase 11 — Live-Agent Handoff Text-Extraction Translator

- [ ] Add `crates/symphony-agent/src/handoff_translator.rs` per ARCHITECTURE v5 §4.2.2.
- [ ] Scan the final `AgentEvent::Message.content` for the last fenced `json` block.
- [ ] Feed the extracted block to `Handoff::parse`.
- [ ] On `MalformedHandoff`, drive `MalformedHandoffPolicy::decide`. On `RequestRepairTurn`, call `session.continue_turn(&guidance)` with a structured field-pathed message and re-translate.
- [ ] Short-circuit when the MCP `submit_handoff` tool already persisted a handoff for this run (Phase 5 contract).
- [ ] Reopen `CHECKLIST_v2.md:77,82`: re-run those validation tests against the live-agent path now that the production wiring exists.
- [ ] Add tests: well-formed fenced block parses, malformed JSON triggers `RequestRepairTurn`, malformed shape triggers `RequestRepairTurn`, exhausted retries collapse to `FailRun`, agent that submitted via MCP does not get extracted twice.

## Phase 12 — Cross-Backend Parity

- [ ] Add a parameterized integration test that exercises every write tool through both Claude and Codex backends with a mocked underlying LLM that scripts MCP tool calls.
- [ ] Add a parity test for read tools: same mock LLM script must produce identical kernel state regardless of backend.
- [ ] Add an explicit MCP-capability check in workflow validation: a role configured as `integration_owner` whose backend cannot host an MCP client refuses to load.

## Phase 13 — End-to-End Live-Agent Loop

- [ ] Add a deterministic E2E test mirroring `CHECKLIST_v2.md:150` ("broad parent decomposes into two child specialist issues, integrates, opens draft PR, passes QA, marks PR ready, closes parent") but driving the loop with the **MCP server bound and tool calls scripted** rather than scripted handoff text.
- [ ] The test MUST fail in a useful way if any MCP tool is missing from the registry, if any tracker comment is not written, or if the cross-agent handoff data is not surfaced in the receiving prompt.
- [ ] Add the same E2E test variant where the agent uses the Phase 11 text-extraction handoff path instead of `submit_handoff`, proving both paths produce identical kernel state.

## Phase 14 — Operator Surfaces

- [ ] Implement `symphony mcp tools --role <name>` printing the effective tool registry for a role.
- [ ] Implement `symphony mcp call --role <name> --tool <name> --input <json>` for direct tool invocation against a live workflow without an agent. Used by Phase 13 and for human debugging.
- [ ] Extend `symphony status` to render per-run MCP-call telemetry (counts, failures) and the `agent_comments` rows.
- [ ] Add JSON output mode for the new commands matching existing v2/v3 status JSON conventions.

## Phase 15 — Documentation

- [ ] Document the Symphony MCP server in `docs/mcp.md`: tool surface, role/tool defaults, multi-config merging, byline conventions.
- [ ] Document agent-authored vs kernel-driven comments in `docs/comments.md`.
- [ ] Document the handoff text-extraction safety net and when each path is used.
- [ ] Update `docs/roles.md` with the v5 `agents.<profile>.mcp_tools` configuration knob.
- [ ] Update `README.md` to reflect that Hermes is no longer a supported backend and that v5 closes the live-agent loop.

## Phase 16 — Promotion and Cleanup

- [ ] Update `PLAN_v2.md:97` (or its successor) so the headline end-to-end claim is qualified by "as exercised by `CHECKLIST_v5.md` Phase 13".
- [ ] Remove any `FUTURE_DEVELOPMENT.md` references in inline comments or commit messages that were temporary scaffolding.
- [ ] Verify `SPEC_v4.md` §11 forward-references resolve to landed v5 features and remove the "must both ship before" hedge once Phase 13 passes.
