# Symphony-RS Architecture v5 Addendum

This document describes the target architecture for SPEC v5: an
internal MCP server that closes the agent ↔ kernel surface, plus the
text-extraction safety net for handoffs and the kernel-driven
tracker-comment writes.

`SPEC_v5.md` is the product contract and `CHECKLIST_v5.md` is the
iteration plan. This file explains how the v5 requirements land on the
existing v2/v3/v4 crate surfaces. It is an **addendum to
`ARCHITECTURE_v2.md`, `ARCHITECTURE_v3.md`, and `ARCHITECTURE_v4.md`**
and does not replace any prior architectural decision.

## 1. Architectural Goal

v2 owns the workflow kernel. v3 adds tracker-backed dependency
orchestration. v4 owns the prompt-side surface (role doctrine,
catalogs, rich issue context, child handoffs in receiving prompts).

What is missing is the channel through which a real LLM session's
output becomes kernel state and through which an agent fetches
additional context or posts comments. v5 introduces that channel as an
**in-process MCP server** Symphony hosts inside the orchestrator
binary, plus a small text-extraction translator that wraps the
existing `Handoff::parse` for end-of-turn reports.

After v5:

- every kernel-mutating action a Rust caller can perform on behalf of
  an agent (decomposition, handoff, QA verdict, follow-up,
  issue-comment) has a typed MCP tool the agent can call directly;
- every read the agent might want beyond the v4 prompt baseline
  (comments, full `blocked_by`, role catalog, related issues) has an
  MCP read tool;
- the kernel itself writes tracker comments on workflow transitions
  that humans need to see (paused-on-review, run-failed, QA reasoning,
  decomposition-applied);
- handoff parsing is wired end-to-end on the live-agent path;
- Hermes is removed.

## 2. Layered View

```text
┌───────────────────────────────────────────────────────────────────────┐
│ Agent Process (Claude Code, Codex)                                    │
│   sees Symphony MCP tools alongside its built-in tools                │
│   model decides when to call propose_decomposition / submit_handoff   │
│   / add_issue_comment / list_comments / etc.                          │
├───────────────────────────────────────────────────────────────────────┤
│ MCP Transport (per-run unix socket)                                   │
│   <workspace>/.symphony/mcp.sock                                      │
│   created by symphony-agent before spawn, removed on Completed/Failed │
├───────────────────────────────────────────────────────────────────────┤
│ symphony-mcp (NEW crate)                                              │
│   in-process MCP server hosted by the orchestrator                    │
│   tool registry, request routing, structured tool errors              │
├───────────────────────────────────────────────────────────────────────┤
│ Tool Handlers (NEW: symphony-mcp::handlers)                           │
│   thin adapters from MCP request payloads to typed kernel calls       │
│   propose_decomposition → DecompositionRunner::accept                 │
│   submit_handoff       → Handoff + HandoffRepository                  │
│   record_qa_verdict    → QaVerdict pipeline                           │
│   file_followup        → followup pipeline                            │
│   add_issue_comment    → TrackerMutations::add_comment                │
│   get_issue_details    → TrackerRead::get_issue                       │
│   list_comments        → TrackerRead::list_comments (NEW)             │
│   list_available_roles → RoleCatalogBuilder::build (v4)               │
│   get_related_issues   → TrackerRead::get_related (NEW)               │
├───────────────────────────────────────────────────────────────────────┤
│ Kernel + State (existing v2/v3/v4)                                    │
│   DecompositionRunner, Handoff, QaVerdict, TrackerMutations,          │
│   HandoffRepository, comment audit (NEW table — §4.3)                 │
└───────────────────────────────────────────────────────────────────────┘
```

In parallel, two non-MCP additions:

```text
┌───────────────────────────────────────────────────────────────────────┐
│ Handoff text-extraction (symphony-agent, NEW)                         │
│   AgentEvent::Message.content → fenced-block scan → Handoff::parse    │
│   on failure: MalformedHandoffPolicy.decide → continue_turn(guidance) │
├───────────────────────────────────────────────────────────────────────┤
│ Kernel-driven tracker comments (symphony-core, NEW)                   │
│   workflow-transition observer → TrackerMutations::add_comment        │
│   paused-on-review, run-failed, QA reasoning, decomposition-applied   │
│   shares the operation queue (migrations.rs:908) with §5.5 writes     │
└───────────────────────────────────────────────────────────────────────┘
```

## 3. Key Design Shifts From v4

### 3.1 From "kernel is the only caller of the kernel" to "MCP is also a caller"

Today every typed kernel call (`DecompositionRunner::accept`,
`HandoffRepository::create_handoff`, `TrackerMutations::add_comment`)
has only Rust callers. v5 introduces a second class of caller — the
MCP tool handler — but the typed surface is unchanged. Handlers
deserialize JSON into the existing types and call existing methods.
This is a deliberate constraint: the kernel does not gain an MCP-shaped
back door; MCP is a thin transport.

### 3.2 From "operator wires MCP" to "Symphony wires MCP"

`SPEC_v4.md` showed `extra_args: [--mcp-config, .symphony/mcp/platform-lead.json]`
as the way operators added MCP servers. v5 keeps that capability for
*third-party* MCP servers (e.g. Linear's published MCP), but Symphony's
own MCP server is **invisible plumbing** injected by the orchestrator
during `spawn_session`. Operators do not see it in their config and
cannot accidentally turn it off.

### 3.3 From "agent → text → handoff" (broken) to "agent → MCP → handoff" (preferred) plus "agent → text → handoff" (safety net)

`Handoff::parse` exists but no production caller wires it to the live
agent stream. v5 lands both paths: MCP `submit_handoff` is preferred
(structured, mid-turn errors); the text-extraction wrapper closes the
v2 gap as a fallback.

### 3.4 From "kernel writes nothing to the tracker on transitions" to "kernel writes targeted human-readable comments"

v5 turns silent-on-SQLite transitions into operator-visible artifacts
without forcing the operator to learn `symphony status`. Reuses the
existing `TrackerMutations::add_comment` capability and the operation
queue at `migrations.rs:908`.

### 3.5 From "Hermes is a third backend" to "Hermes does not exist"

v4 listed Hermes as one of three backends; v5 removes it from code,
config, fixtures, and docs.

## 4. Crate Evolution

### 4.1 New crate: `symphony-mcp`

Top-level Cargo workspace member. Owns:

- the in-process MCP server (built on a maintained Rust MCP server
  library; the choice of crate is an ADR — see §9);
- a tool registry trait + per-tool handler trait;
- per-tool request/response wire types (deriving `Serialize`/`Deserialize`
  with `deny_unknown_fields`);
- the per-run socket lifecycle (bind, serve, shutdown);
- a `ToolError` taxonomy that maps cleanly to MCP `tool_result` errors
  (validation, capability-unsupported, idempotency-replay, rate-limit).

Depends on: `symphony-core` (typed kernel surface), `symphony-state`
(audit-row writes), `symphony-tracker` (read tools call adapters
directly). Does not depend on `symphony-agent` or `symphony-cli` —
those depend on it.

### 4.2 `symphony-agent`

#### 4.2.1 Per-run MCP wiring

`spawn_session` (today in `crates/symphony-agent/src/{claude,codex}/runner.rs`)
gains a pre-spawn step:

```rust
// Pseudocode, real types in symphony-mcp.
let socket_path = workspace.path().join(".symphony/mcp.sock");
let mcp_handle = symphony_mcp::Server::spawn(SpawnParams {
    socket_path: &socket_path,
    role_profile: &profile,           // determines tool subset
    work_item_id,
    run_id,
    kernel_handles: kernel_handles.clone(),
})?;

let mcp_config_path = workspace.path().join(".symphony/mcp-config.json");
write_mcp_config(&mcp_config_path, &socket_path, operator_supplied_config)?;
args.extend(["--mcp-config".into(), mcp_config_path.to_string_lossy().into()]);
```

`mcp_handle` is RAII-tied to the agent session: when the
`AgentEvent::Completed`/`Failed` lands, the handle drops, the server
unbinds the socket, and the per-run audit rows are flushed.

If the operator supplied their own `--mcp-config` via
`AgentBackendProfile.extra_args`, `write_mcp_config` reads it,
verifies no tool-name conflicts with Symphony's registry, and emits a
merged file. Conflicts fail loud at spawn time.

#### 4.2.2 Handoff text-extraction translator

New module `symphony-agent::handoff_translator`:

```rust
pub struct HandoffTranslator { policy: MalformedHandoffPolicy }

pub enum TranslationOutcome {
    Handoff(Handoff),
    NeedsRepair { guidance: String, attempts_remaining: u32 },
    FailRun { error: MalformedHandoff },
    NoHandoffEmitted, // run completed without producing a handoff
}

impl HandoffTranslator {
    pub fn translate(&mut self, final_message: &str) -> TranslationOutcome;
}
```

The orchestrator drives the loop: on `NeedsRepair`, it calls
`session.continue_turn(&guidance)` (`claude/runner.rs:608`) and
re-translates the next final message. On exhaustion, the policy
collapses to `FailRun` which the orchestrator surfaces through the
existing run-failed pathway.

Used only when the agent did not call the MCP `submit_handoff` tool
(the MCP handler short-circuits the translator if it has already
persisted a handoff for the run).

### 4.3 `symphony-state`

#### 4.3.1 New table: `agent_comments`

Audit row associating tracker comments with the run that authored
them, regardless of whether the comment was kernel-driven (§7 of SPEC
v5) or agent-driven (§5.5):

```sql
-- v5: comments authored by Symphony, kernel-side or agent-side.
CREATE TABLE agent_comments (
    id                   INTEGER PRIMARY KEY,
    run_id               INTEGER REFERENCES runs(id) ON DELETE SET NULL,
    work_item_id         INTEGER NOT NULL
                         REFERENCES work_items(id) ON DELETE CASCADE,
    author_kind          TEXT NOT NULL,    -- 'kernel' | 'agent'
    transition_kind      TEXT,             -- when author_kind = 'kernel'
    body                 TEXT NOT NULL,
    idempotency_key      TEXT,             -- when author_kind = 'agent'
    tracker_comment_id   TEXT,             -- adapter return; nullable until landed
    state                TEXT NOT NULL,    -- 'pending' | 'posted' | 'failed'
    created_at           TEXT NOT NULL,
    posted_at            TEXT
);
CREATE INDEX idx_agent_comments_work_item ON agent_comments(work_item_id);
CREATE UNIQUE INDEX idx_agent_comments_idempotency
    ON agent_comments(run_id, idempotency_key)
    WHERE idempotency_key IS NOT NULL;
```

The unique partial index makes idempotency replay-safe: an agent
retrying with the same key gets the original row, not a duplicate.

#### 4.3.2 Reuse the operation queue

The existing `migrations.rs:908` operation queue (`'add_comment',
'{"body":"..."}', 'pending'`) is the dispatcher for actually delivering
the tracker write. `agent_comments` rows transition through `state`
based on operation-queue outcomes; on transient tracker outage,
deferral is automatic.

### 4.4 `symphony-core`

#### 4.4.1 New module: `tracker_comments`

```rust
pub struct TrackerCommentRequest {
    pub work_item: WorkItemId,
    pub body: String,
    pub byline: BylineKind,        // KernelTransition(...) | AgentRole(RoleName, RunId) | NoByline
}

pub trait TrackerCommentSink: Send + Sync {
    fn enqueue(&self, request: TrackerCommentRequest) -> Result<(), TrackerCommentError>;
}
```

Two implementations land in v5:

- `KernelTransitionObserver` — listens to handoff/QA/decomposition
  transitions and enqueues comments per SPEC v5 §7. Subscribes to the
  same event bus the existing runners use.
- `McpAuthorshipSink` — receives §5.5 `add_issue_comment` requests
  from the MCP server and forwards them with the
  `BylineKind::AgentRole` annotation.

Both sinks write `agent_comments` rows and enqueue operation-queue
entries. The byline rendering is a pure function over `BylineKind`.

#### 4.4.2 Tracker read extension

`TrackerRead` (in `tracker_trait.rs`) gains:

```rust
async fn list_comments(&self, issue: &IssueId)
    -> TrackerResult<Vec<IssueComment>>;
async fn get_related(&self, issue: &IssueId)
    -> TrackerResult<RelatedIssues>;
```

with default implementations returning `TrackerError::Unsupported` so
existing adapters compile without change. GitHub and Linear adapters
implement them; mock adapters in `crates/symphony-tracker/src/mock.rs`
gain test fixtures.

### 4.5 `symphony-cli`

`symphony status` learns to render rows from `agent_comments` so an
operator can see "this comment was posted by run #N at $time, body
$preview, current state $state". Two new diagnostic surfaces:

- `symphony mcp tools --role <name>` — print the MCP tool registry
  Symphony would expose for a given role profile.
- `symphony mcp call --role <name> --tool <name> --input <json>` —
  execute a tool against the live workflow's kernel without spinning
  up an agent. Used in the deterministic E2E test in `CHECKLIST_v5.md`
  Phase 13 and useful for human debugging.

### 4.6 `symphony-config`

#### 4.6.1 Hermes removal

Delete:

- `HermesAgentConfig` and its `Default` impl;
- the `agents.hermes` field on `AgentConfig` plus its serde wiring;
- any `AgentBackend::Hermes` variant;
- fixtures referencing `backend: hermes`.

Add a validator that produces a clear error when a workflow YAML
mentions `backend: hermes` so operators upgrading from v4 see what
happened.

#### 4.6.2 Per-role MCP tool subset

`AgentBackendProfile` gains an optional structured field listing which
Symphony MCP tools the role's agent should be able to see:

```yaml
agents:
  platform_lead_sonnet:
    backend: claude
    model: claude-sonnet-4-5
    mcp_tools:
      - propose_decomposition
      - submit_handoff
      - add_issue_comment
      - get_issue_details
      - list_comments
      - list_available_roles
```

Defaults: every role gets the read tools (§6) and `submit_handoff` +
`add_issue_comment`. Roles whose `kind` is `integration_owner` also
get `propose_decomposition`. Roles whose `kind` is `qa_gate` also get
`record_qa_verdict`. Operators can shrink or expand the set per role
within validator-enforced bounds.

## 5. MCP Server Lifecycle

```text
spawn_session(role, work_item, run_id)
  │
  ├─ resolve effective MCP tool set for this role
  ├─ allocate per-run socket path under workspace
  ├─ symphony_mcp::Server::spawn { socket, tools, kernel_handles }
  ├─ write merged --mcp-config to worktree
  │       (Symphony's server + any operator-supplied servers)
  ├─ append --mcp-config <path> to argv (or backend equivalent)
  └─ launch agent process
        │
        ▼
  agent runs, may call tools any number of times mid-turn
        │
        ▼
  AgentEvent::Completed | Failed
        │
        ▼
  drop McpHandle
        ├─ unbind socket, remove socket file
        ├─ flush pending agent_comments rows
        ├─ remove --mcp-config file
        └─ emit run-summary event including MCP-call telemetry
```

The lifecycle is RAII: the `McpHandle` dropping guarantees teardown
even on panic. Crashes between spawn and completion leave a stale
socket file; the next `spawn_session` against the same workspace
unlinks any pre-existing socket file before binding.

## 6. Tool Handler Pattern

Each tool handler is a small adapter:

```rust
// symphony-mcp/src/handlers/propose_decomposition.rs
pub struct ProposeDecompositionHandler<'a> {
    runner: &'a DecompositionRunner,
    state: Arc<dyn DecompositionPersistence>,
    role_catalog: &'a RoleCatalog,
    work_item: WorkItemId,
    parent_depth: u32,
    author_role: RoleName,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct ProposeDecompositionInput {
    summary: String,
    children: Vec<ChildDraftWire>,
}

impl ToolHandler for ProposeDecompositionHandler<'_> {
    fn handle(&self, raw: serde_json::Value) -> Result<ToolResult, ToolError> {
        let input: ProposeDecompositionInput =
            serde_json::from_value(raw).map_err(ToolError::invalid_payload)?;
        let draft = self.lower(input)?;     // resolves role names, validates depends_on graph
        let id = self.allocate_decomposition_id()?;
        let proposal = self.runner.accept(id, draft, self.parent_depth)
            .map_err(ToolError::policy)?;
        self.state.persist(&proposal)?;
        Ok(ToolResult::ok_json(json!({
            "decomposition_id": proposal.id,
            "child_count": proposal.children.len(),
            "status": proposal.status.as_str(),
        })))
    }
}
```

Identical shape for the other write handlers. Read handlers are even
simpler — pure pass-through to `TrackerRead` or `RoleCatalogBuilder`
with caching where it makes sense.

## 7. Failure Modes

### 7.1 Tool payload validation failure

- **Detected at:** handler entry, via `serde_json::from_value` or
  semantic validation (e.g. `depends_on` references unknown child
  key).
- **Behavior:** MCP `tool_result` error with the field path and a
  human-readable explanation.
- **Operator outcome:** none — agent self-corrects in the same turn.
  Telemetry records the validation miss for diagnostics.

### 7.2 Capability gate refusal

- **Detected at:** handler entry, via `TrackerCapabilities` check.
- **Behavior:** structured tool error naming the missing capability.
- **Operator outcome:** none — but the handler should be omitted from
  the tool registry for adapters that do not support it; this branch
  is a defense-in-depth catch.

### 7.3 Per-run rate limit hit

- **Detected at:** `add_issue_comment` handler.
- **Behavior:** structured tool error naming the limit and current
  usage.
- **Operator outcome:** if the limit fires during normal operation,
  raise it via config. If it fires due to a runaway agent loop, the
  agent run typically also exhausts its token budget and the run-failed
  comment lands per §7.

### 7.4 Idempotency replay

- **Detected at:** `add_issue_comment` handler via the unique partial
  index on `(run_id, idempotency_key)`.
- **Behavior:** return the original row's outcome as if the call
  succeeded; do not re-post.
- **Operator outcome:** none — the agent does not need to know the
  call was a replay.

### 7.5 Operator MCP config conflict

- **Detected at:** `spawn_session`, while merging the operator's
  `--mcp-config` with Symphony's.
- **Behavior:** `AgentSpawnError::McpToolNameConflict { tool, operator_server, symphony_owns: true }`.
- **Operator outcome:** workflow refuses to launch with a clear
  message naming the conflicting tool.

### 7.6 Tracker outage during a comment write

- **Detected at:** operation queue dispatcher.
- **Behavior:** row stays `pending`, retries per the existing
  operation-queue policy.
- **Operator outcome:** `symphony status` shows the deferred state.
  No run failure cascades from a transient tracker outage.

## 8. Operator-Visible Surfaces

| Surface | Source |
|---|---|
| `symphony status` lists `agent_comments` rows by run | `agent_comments` table |
| `symphony mcp tools --role <name>` prints tool registry | `symphony-mcp::Server::tools_for(role)` |
| `symphony mcp call --role <name> --tool <name> --input <json>` invokes a tool | `symphony-mcp::Server::call_directly` (test/debug only) |
| Tracker comment with Symphony byline | `BylineKind` rendering in `tracker_comments::byline()` |
| Run telemetry event includes per-tool call counts and failure counts | MCP server emits `McpCallObserved` event into the existing event bus |

## 9. ADRs

### 2026-05-10 — In-process MCP server, not subprocess

Context: Symphony needs to expose an MCP server to each agent run. Two
shapes were considered: (a) subprocess MCP server per run; (b) the
orchestrator binary hosts the server in-process and exposes a per-run
socket.

Decision: in-process.

Consequence: tool handlers can hold direct references to live kernel
runners and the `symphony-state` connection — no IPC marshalling
between the server and the kernel. The agent process is the only thing
on the other side of the unix socket. Per-run isolation is via the
socket path under the workspace; the handle is RAII-tied to the agent
session so a crashed run does not leak a long-lived listener.

The trade-off is that an MCP-library bug becomes an orchestrator bug.
Mitigated by choosing a maintained crate (see next ADR) and by keeping
handlers narrow — almost all logic stays in the kernel where it is
already tested.

### 2026-05-10 — Tool registry shape and library choice

Context: Rust's MCP ecosystem has multiple options. Selecting one is
load-bearing because it shapes the handler trait and the wire types.

Decision: use the official Rust SDK, `rmcp` (`modelcontextprotocol/rust-sdk`).
The MCP transport spec defines stdio and Streamable HTTP as standard
transports while explicitly allowing custom transports. That matters for
Symphony because the target deployment is an in-process server exposed
over a per-run unix socket, not a standalone HTTP daemon. `rmcp` gives us
the maintained protocol model and server/tool surface while our
`symphony-mcp` crate owns the Symphony-specific per-run socket lifecycle
and dynamic role registry.

Consequence: `symphony-mcp` depends on `rmcp` but keeps its first-phase
handler traits and registry library-agnostic. If the unix-socket adapter
needs custom transport glue, that glue stays inside `symphony-mcp`
instead of leaking into agent or kernel crates.

### 2026-05-10 — MCP for mid-turn structured actions, raw-JSON for end-of-turn reports

Context: handoffs were always going to need a parser; v5 is choosing
the transport. Two options: route handoffs through MCP
(`submit_handoff`) only, or keep the existing `Handoff::parse` raw-JSON
path and add MCP as preferred-but-not-required.

Decision: support both, prefer MCP. Handoffs are a natural
end-of-turn report (the agent is "done" when it submits one) and the
raw-JSON path with `MalformedHandoffPolicy::RequestRepairTurn` is
already designed for exactly that shape. Removing it would re-litigate
the v2 handoff design without a load-bearing reason.

Consequence: two parsing surfaces for handoffs. Documented as a rule
("MCP for mid-turn structured actions, raw-JSON for end-of-turn
reports") so future tools land on the right side without ambiguity.

### 2026-05-10 — Symphony injects MCP, operators do not

Context: SPEC v4 §7 showed `extra_args: [--mcp-config, ...]` as
operator-supplied. v5 needs the Symphony tools wired in always, not
opt-in.

Decision: Symphony injects its own `--mcp-config` per run. Operator
config is preserved by merging the two configs at spawn time, with
tool-name conflicts as fail-loud spawn errors.

Consequence: operators do not need to know the Symphony MCP server
exists to use Symphony. They only encounter it when (a) they want to
add their own MCP server too — handled by the merge — or (b) they
want to inspect what Symphony exposes (`symphony mcp tools`).

### 2026-05-10 — Comment authorship is kernel-tracked even when agent-driven

Context: when an agent calls `add_issue_comment` we could just call
`TrackerMutations::add_comment` and forget about it.

Decision: every comment posted by Symphony — kernel-driven (§7) or
agent-driven (§5.5) — gets a row in `agent_comments`. The row records
who, when, why, and the resulting tracker comment id.

Consequence: `symphony status` can show a complete authorship trail.
Idempotency is enforced at the database level. If a tracker reports a
deleted comment, Symphony can detect divergence at the next read.

### 2026-05-10 — Hermes removal, not deprecation

Context: `HermesAgentConfig` exists in `symphony-config`. It has never
been wired into a runnable backend adapter. Two options: deprecate
(keep config, warn on use), or remove.

Decision: remove. The code has no users and its presence forces SPEC
language ("Hermes via supported Hermes non-interactive model/provider
interface") that adds noise without value. v5 considers it vestigial.

Consequence: a one-line config validator catches operators who still
use `backend: hermes` and points them at the v5 removal note.
Migration cost is near-zero because there are no real users.
