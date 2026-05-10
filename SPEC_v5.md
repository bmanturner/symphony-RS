# Symphony-RS Product Specification v5 Addendum

Status: Draft v5 addendum — targeted requirements for agent ↔ kernel
integration via an internal MCP server. Companion to `SPEC_v4.md`.

## 1. Scope

SPEC v5 does **not** replace `SPEC_v2.md`, `SPEC_v3.md`, or
`SPEC_v4.md`. It closes the cross-cutting agent ↔ kernel surface that
v2-v4 deferred:

> When a real LLM session participates in a Symphony workflow, it must
> be able to (a) hand structured workflow data back to the kernel, (b)
> fetch additional tracker context on demand, (c) post comments visible
> to humans, and (d) have its end-of-turn reports parsed reliably. The
> kernel half of every workflow is built; the agent-facing wiring is
> not.

Once v5 lands, the headline `PLAN_v2.md` claim — *"broad issue →
decomposition → child execution → integration → draft PR → QA failure
→ blocker → rework → QA pass → PR ready"* — is true with a live agent
(Claude Code or Codex), not only with the mock agent.

## 2. Current Gap

`SPEC_v4.md` §11 named this cohort. Concretely, after v4:

- The kernel can validate, persist, and reconcile `DecompositionDraft`,
  `Handoff`, `QaVerdict`, follow-up proposals, and dependency edges.
- The kernel exposes `TrackerMutations::add_comment`,
  `TrackerMutations::create_issue`, etc. on real GitHub and Linear
  adapters.
- A real agent process (Claude Code, Codex) is launched, observed, and
  emits `AgentEvent::ToolUse { tool, input }` events for whatever tools
  its CLI registered.
- Symphony does **not** register any of its own tools with the model.
  There is no path by which an agent can hand the kernel a
  `DecompositionDraft`, a `Handoff`, a `QaVerdict`, a follow-up, or a
  comment-on-issue request.
- Symphony does **not** write to the tracker on workflow transitions
  (paused-on-human-review, run-failed, QA reasoning). Operators must
  query SQLite to see workflow state.
- The `Handoff::parse` parser exists but no production code extracts a
  payload from the live agent stream and feeds it in.
- `HermesAgentConfig` exists in `crates/symphony-config/src/config.rs`
  but is unused, undocumented as supported, and confuses the v4
  backend story.

## 3. Required Outcome

v5 is satisfied when a real LLM running as the platform-lead role can:

1. Read its prompt (which v4 already populated with role doctrine, peer
   catalog, and rich issue context).
2. Call MCP tools registered by Symphony to (a) read additional tracker
   context (`get_issue_details`, `list_comments`,
   `list_available_roles`), (b) propose a decomposition
   (`propose_decomposition`), and (c) post a status comment on the
   parent issue (`add_issue_comment`).
3. Have those tool calls validated, gated, and persisted by the kernel
   exactly as if a Rust caller had constructed the equivalent typed
   requests.

The same surface MUST work for specialists, the integration owner, and
QA, parameterized by which tools each role's profile exposes.

## 4. The Internal MCP Server

Symphony hosts an MCP server **in-process** in the orchestrator binary.
Per agent run, Symphony:

1. Picks a per-run unix socket path under the run's workspace
   (canonical: `<workspace>/.symphony/mcp.sock`).
2. Binds the in-process server to that socket.
3. Materializes an MCP config file inside the worktree pointing at the
   socket, with the role-appropriate tool subset enabled.
4. Appends `--mcp-config <path>` (Claude) or the equivalent Codex flag
   to the agent's argv during `spawn_session`.
5. Tears down the socket and config file on `Completed` / `Failed`.

This is **invisible plumbing**. The operator's `WORKFLOW.md` MUST NOT
need to mention the Symphony MCP server. The only operator-facing MCP
config is for *third-party* MCP servers an operator chooses to wire
(e.g. a Linear-published MCP server).

When an operator does supply their own `--mcp-config`, Symphony MUST
merge its server into the resulting effective MCP config rather than
silently overwriting the operator's choice. The merge MUST be
deterministic, MUST preserve operator-supplied servers, and MUST
surface a clear error if the operator's config tries to register a
tool name Symphony also registers.

## 5. Tool Surface — Write

Each role's profile selects which write tools it exposes.

### 5.1 `propose_decomposition`

Available to: roles whose `kind` is `integration_owner` and whose
workflow has decomposition enabled.

The tool payload deserializes into the existing
`crates/symphony-core/src/decomposition_runner.rs::DecompositionDraft`
shape (with role names as strings, resolved against the v4 role
catalog). The kernel then runs `DecompositionRunner::accept` exactly as
if a Rust caller had constructed the draft, applying every existing
policy gate (`max_depth`, `owner_role`, acceptance-criteria-per-child,
etc.).

A successful call returns the persisted `DecompositionId` and a
human-readable summary of what was created. A validation failure
returns a structured `tool_result` error naming the specific field that
violated policy, so the agent can self-correct in the same turn.

### 5.2 `submit_handoff`

Available to: every role that produces handoffs (specialists,
integration owner, QA).

Payload deserializes into the existing
`crates/symphony-core/src/handoff.rs::Handoff` shape. On success, the
kernel persists the handoff via `HandoffRepository::create_handoff`,
recomputes downstream queue placement based on `ready_for`, and
acknowledges with the persisted `HandoffId`.

Recommended (not required) over the §8 text-extraction path because
mid-turn structured errors give a cleaner failure surface. The
text-extraction path remains supported for backends that cannot use
MCP for any reason and as a safety net.

### 5.3 `record_qa_verdict`

Available to: roles whose `kind` is `qa_gate`.

Payload deserializes into the existing `QaVerdict` shape. Kernel
applies the verdict and any blocker creation per
`CHECKLIST_v2.md:99-100`.

### 5.4 `file_followup`

Available to: every role allowed to file or propose follow-ups (per
`SPEC_v2.md` §6 follow-up policy). Payload mirrors the
`followups_created_or_proposed` block of the canonical handoff.

### 5.5 `add_issue_comment`

Available to: every role.

```jsonc
{
  "issue_id": "<defaults to the current run's work item if omitted>",
  "body": "markdown body",
  "idempotency_key": "<client-supplied; kernel dedupes retries>"
}
```

The kernel calls `TrackerMutations::add_comment` through the wired
adapter, capability-gated. The tool MUST refuse with a structured
error when the adapter advertises `add_comment: false`.

Each call MUST:

- be rate-limited per run (default cap configurable, e.g.
  `mcp.add_issue_comment.max_per_run = 10`);
- be deduplicated by `(run_id, idempotency_key)` so an agent retry does
  not double-post;
- be associated with the originating run in a kernel audit row so
  `symphony status` can show "this comment was authored by run #N";
- by default prepend a Symphony-rendered byline (e.g.
  `> Posted by Symphony on behalf of role:specialist (run #42)`) so
  humans reading the tracker can tell automated comments apart from
  teammate comments. Operators MAY disable the byline per workflow.

## 6. Tool Surface — Read

Read tools are non-mutating, side-effect-free except for telemetry.

### 6.1 `get_issue_details`

Returns the full normalized `Issue` (`crates/symphony-core/src/tracker.rs::Issue`)
for a given identifier — including labels, priority, `blocked_by`,
timestamps, and any other field the adapter populates. v4's `PromptIssue`
expansion covers the always-on slice; this tool covers everything beyond
that on demand.

### 6.2 `list_comments`

Returns the comment thread for an issue. The kernel does not currently
model `Comment`s on the `Issue` struct (SPEC v4 §5.1 calls this out
deliberately); v5 introduces a minimal typed `IssueComment` shape
(author, body, created_at, id) populated by the adapter on demand.

### 6.3 `list_available_roles`

Returns the v4 `RoleCatalog` projection. Useful for specialists
deciding which `ready_for` target to pick or which role to file a
follow-up against, where pre-injecting the full catalog into every
specialist prompt would waste tokens.

### 6.4 `get_related_issues`

Returns parent / children / blockers for a given identifier in full,
not just the bare summaries v4 puts in the prompt.

## 7. Kernel-Driven Tracker Comments

Distinct from §5.5 (agent-authored). The kernel itself MUST post
comments on workflow transitions where humans watching the tracker
need to see what is happening.

Required transitions for v5:

| Transition | Comment content |
|---|---|
| `Handoff` lands with `ready_for: human_review` | "Symphony paused this run for human review. See handoff $id. Branch: $X." |
| Run failed (e.g. malformed handoff exhausted retries, agent crash, repeated tool-validation failure) | Failure summary + run id + diagnostic. |
| `QaVerdict` is `fail` or `inconclusive` | QA's reasoning + blocker references. |
| Decomposition applied | Summary of children created (covers a gap today: child issues are created, but the parent thread does not get a "Symphony decomposed this into N children" comment). |

Optional (workflow-configurable) for v5:

- specialist handoff `ready_for: integration` (off by default — usually
  invisible internal step);
- run started (off by default).

These comments share the §5.5 byline convention so the human reader
can tell kernel-authored from agent-authored from teammate comments.

Both kernel- and agent-authored comments share an operation queue
(see `crates/symphony-state/src/migrations.rs:908`) so transient
tracker outages defer rather than fail the workflow.

## 8. Live-Agent Handoff Translator

Closes a gap inherited from `CHECKLIST_v2.md:77,82` (which marked
handoff parsing complete in the kernel without wiring the live-agent
extraction path). For agents that do not call `submit_handoff` (§5.2),
the orchestrator MUST extract a JSON payload from the agent's final
message and feed it to `Handoff::parse`.

Extraction rules:

- Scan the final `AgentEvent::Message.content` for the last
  fenced ```` ```json ```` block.
- On `MalformedHandoff::InvalidJson` or `MalformedHandoff::InvalidShape`,
  apply the existing `MalformedHandoffPolicy` (`FailRun` or
  `RequestRepairTurn { max_attempts }`).
- On `RequestRepairTurn`, call `continue_turn(guidance)` with a
  structured guidance message naming the offending field.

The MCP-based path (§5.2) is preferred and recommended; the
text-extraction path is a safety net so a backend that cannot run an
MCP client (today, none in scope; tomorrow, possibly) does not block
the run.

## 9. Validation Requirements

Validation MUST fail when:

- a workflow enables decomposition for a role whose agent profile
  cannot register the `propose_decomposition` MCP tool (i.e. the
  profile's backend cannot host an MCP client);
- a workflow's effective MCP config conflicts with operator-supplied
  servers on a tool name Symphony itself registers;
- a role profile lists a write tool name not in §5;
- `mcp.add_issue_comment.max_per_run` is less than 1 or greater than a
  reasonable safety ceiling (e.g. 100).

Validation SHOULD warn when:

- a workflow has `decomposition.enabled: true` but the platform-lead
  agent profile does not advertise MCP support (would force a fallback
  to text extraction, which is brittle for decomposition);
- a workflow disables the kernel-driven tracker comments (§7) entirely.

## 10. Hermes Removal

The `HermesAgentConfig` block in
`crates/symphony-config/src/config.rs` and any associated runtime
adapter scaffolding MUST be removed. Hermes is not a supported backend
in v5 and was never wired into a runnable adapter. v5 considers it
vestigial.

Removal scope:

- `HermesAgentConfig` struct, its `Default`, and the `agents.hermes`
  field on `AgentConfig`.
- Any fixture or sample workflow that mentions `backend: hermes`.
- Any `Hermes` enum variant on `AgentBackend`.
- Documentation updates (`docs/`, `README.md`) removing Hermes from
  the supported-backend list.

A workflow that still mentions `backend: hermes` after v5 MUST fail
config validation with a clear error pointing operators at the v5
removal.

## 11. Acceptance Criteria

SPEC v5 is satisfied when:

- A live Claude Code (and a live Codex) integration-owner run can call
  `propose_decomposition` and have the kernel persist a
  `DecompositionProposal` whose children are then created on the
  tracker via the existing applier.
- A live specialist run can call `submit_handoff` (or use the §8
  text-extraction path) and have the kernel persist a `Handoff` and
  enqueue the next runner.
- A live agent can call `add_issue_comment` and the comment appears on
  the tracker with the Symphony byline and a kernel audit row.
- Paused-on-`human_review` runs cause a kernel-authored comment on the
  tracker (§7).
- Run-failed events cause a kernel-authored comment on the tracker
  (§7).
- An operator can run the deterministic E2E test in `CHECKLIST_v5.md`
  Phase 13 — the equivalent of `CHECKLIST_v2.md:150` but driving the
  full broad-issue → done loop with a real (mock-LLM-but-MCP-driven)
  agent loop, not a scripted reply.
- `HermesAgentConfig` is gone from the codebase, fixtures, and docs.
- The MCP server is invisible to operator config: an operator who does
  not touch `--mcp-config` still gets the Symphony tools wired in
  automatically.

## 12. Non-Goals

This addendum does not require:

- exposing the Symphony MCP server to processes outside an active
  agent run (no orchestrator-wide socket; no remote MCP);
- standing the MCP server up as a separate process (it is in-process
  in the orchestrator binary by design);
- supporting MCP transports other than the in-process unix socket;
- inventing a new tracker-comment data model — comments use the
  existing `TrackerMutations::add_comment` path;
- supporting Hermes;
- replacing `Handoff::parse` — the text-extraction path in §8 wraps
  the existing parser, it does not reimplement it.
