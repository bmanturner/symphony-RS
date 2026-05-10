# Symphony-RS v5 Plan

This is the strategic plan for closing the live agent to kernel loop with an
internal MCP server.

v5 is an addendum to v2, v3, and v4. v2 owns the workflow kernel, v3 owns
dependency orchestration, and v4 owns role doctrine and prompt construction.
v5 makes those surfaces usable by real Claude Code and Codex sessions by
registering Symphony-owned MCP tools, wiring tool results into typed kernel
state, and keeping a text-extraction handoff path as a fallback.

## Product Thesis

The workflow is only live-agent complete when an agent can do more than emit
free-form text. Symphony-RS v5 should make this path reliable:

```text
spawn role-scoped agent -> inject Symphony MCP tools -> validate tool calls ->
persist kernel state -> comment visible workflow transitions -> translate
fallback handoff text -> drive the existing v2/v3/v4 closeout loop
```

## Phases

### Phase 0 — Hermes Removal

Remove Hermes as a supported backend from config, docs, fixtures, and tests.

Success bar: workflows that still reference `backend: hermes` fail loudly and
point at the v5 backend decision.

### Phase 1 — MCP Crate Scaffolding

Add the `symphony-mcp` crate, tool handler abstractions, registry, structured
errors, and per-run unix-socket server lifecycle.

Success bar: the crate can serve a no-op tool over the selected MCP transport
and reject duplicate tool registrations deterministically.

### Phase 2 — Symphony-Agent Integration

Inject Symphony's MCP server into Claude and Codex sessions by allocating a
socket, materializing merged MCP config, appending the backend-specific config
flag, and tying server teardown to the session lifecycle.

Success bar: role-scoped agent runs get the correct tool subset while preserving
operator-supplied third-party MCP servers.

### Phase 3 — Read Tools

Expose side-effect-free tracker and role-catalog reads through MCP:
`get_issue_details`, `list_comments`, `get_related_issues`, and
`list_available_roles`.

Success bar: adapters that support read context return typed shapes, while
unsupported adapters produce structured capability errors rather than partial
or silent data.

### Phase 4 — `propose_decomposition`

Let integration-owner agents submit typed decomposition drafts through MCP.

Success bar: the handler validates child keys, roles, dependency DAGs, and all
existing decomposition policy gates before applying the draft through the same
kernel path as Rust callers.

### Phase 5 — `submit_handoff`

Let specialists, integration owners, and QA agents submit canonical handoffs
directly through MCP.

Success bar: valid handoffs are persisted atomically with terminal run state,
double submits are idempotent, and the Phase 11 text extractor is bypassed for
runs that already submitted via MCP.

### Phase 6 — `record_qa_verdict`

Let QA roles submit typed verdicts through MCP.

Success bar: pass, fail, inconclusive, and waived verdicts enforce the same
role and blocker policies as the existing QA gate.

### Phase 7 — `file_followup`

Let eligible roles file or propose follow-up work through MCP.

Success bar: follow-up creation honors the v2 workflow policy and can either
create tracker issues directly or produce approval proposals.

### Phase 8 — `add_issue_comment`

Add the `agent_comments` audit table and the agent-authored comment tool.

Success bar: comments are idempotent, rate-limited, capability-gated, retried
through the operation queue, bylined by default, and visible in status output.

### Phase 9 — Kernel-Driven Tracker Comments

Post tracker comments for workflow transitions humans need to see.

Success bar: human-review pauses, run failures, QA failure or inconclusive
reasoning, and decomposition-applied summaries become durable queued tracker
comment writes without failing the workflow on transient tracker outages.

### Phase 10 — Authorship Audit

Share one rendering and enqueue path between agent-authored comments and
kernel-driven comments.

Success bar: every comment row records who authored it, why it was written, and
which run produced it, with no byline drift between sources.

### Phase 11 — Live-Agent Handoff Text Extraction

Wire the existing `Handoff::parse` safety net into live agent completion
messages.

Success bar: the final fenced JSON block is parsed, malformed handoffs trigger
structured repair turns until policy exhaustion, and MCP-submitted handoffs are
not double-counted.

### Phase 12 — Cross-Backend Parity

Prove Claude and Codex expose and serialize the same Symphony MCP surface.

Success bar: scripted tool calls produce identical kernel state across both
backends and workflow validation refuses MCP-required roles on backends that
cannot host the server.

### Phase 13 — End-to-End Live-Agent Loop

Exercise the headline broad-parent workflow with MCP tool calls instead of only
mock-agent text fixtures.

Success bar: a broad parent decomposes, specialists hand off, integration
consumes handoffs, QA passes, tracker comments are written, and the parent
closes with the MCP server bound during the live-agent loop.

### Phase 14 — Operator Surfaces

Add CLI commands and status telemetry for inspecting and invoking the effective
MCP tool registry.

Success bar: operators can list role tools, call a tool manually against a live
workflow, and inspect per-run MCP call/comment telemetry in text or JSON.

### Phase 15 — Documentation

Document MCP configuration, comment authorship, handoff extraction, role tool
configuration, and the removal of Hermes.

Success bar: operators can configure role tool exposure and understand which
workflow state is visible through tracker comments without reading the code.

### Phase 16 — Promotion and Cleanup

Update older forward references once the v5 live-agent loop is proven.

Success bar: v2/v4 roadmap claims point at the v5 E2E evidence, temporary
future-development scaffolding is gone, and the product docs no longer hedge on
features that have landed.
