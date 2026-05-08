# Symphony-RS Architecture

This document is the living architectural reference for Symphony-RS. It is
read at the start of every loop iteration and updated whenever a binding
decision is made. The authoritative *requirements* live in `SPEC.md`; this
document records *how we satisfy them*.

## Goal & Non-Goals

Symphony-RS is a faithful Rust port of OpenAI's Symphony service (see
`SPEC.md`). It is a long-running daemon that polls an issue tracker on a
fixed cadence, creates an isolated workspace per issue, and runs a coding
agent session against that workspace until the issue reaches a terminal or
handoff state.

We track the upstream spec verbatim with two explicit deviations:

1. **Agent-agnostic Agent Runner.** Where SPEC §3.1 names a single
   "Agent Runner" tightly coupled to Codex's `app-server` JSONL protocol,
   we expose an `AgentRunner` trait with three implementations:
   `CodexRunner`, `ClaudeRunner` (Claude Code via `claude -p
   --output-format stream-json`), and `TandemRunner` (two backends in
   tandem with a configurable lead/follower strategy).

2. **Tracker abstracted behind a trait.** Where SPEC §3.1 names Linear,
   we expose an `IssueTracker` trait with a Linear adapter as the only
   shipped implementation. A second adapter (GitHub Issues, Jira, …)
   must be addable without touching `symphony-core`.

**Non-goals** match SPEC §2.2 unchanged: no GUI, no general workflow
engine, no built-in PR-editing logic, no mandatory sandbox policy.

## Layered View

```
┌──────────────────────────────────────────────────────────────────────┐
│ Policy Layer (repo-defined)            : WORKFLOW.md                 │
├──────────────────────────────────────────────────────────────────────┤
│ Composition Layer (symphony-cli)       : install logging, build     │
│                                          adapters, run orchestrator  │
├──────────────────────────────────────────────────────────────────────┤
│ Orchestration Layer (symphony-core)    : poll loop, state machine,   │
│                                          retry queue, reconciliation │
├──────────────────────────────────────────────────────────────────────┤
│ Trait Surface (symphony-core)          : IssueTracker, AgentRunner,  │
│                                          WorkspaceManager, AgentEvent│
├──────────────────────────────────────────────────────────────────────┤
│ Adapter Layer                                                        │
│   symphony-tracker   : Linear (GraphQL), Mock                        │
│   symphony-agent     : Codex, Claude, Tandem                  ◀──── deviation
│   symphony-workspace : LocalFs                                       │
├──────────────────────────────────────────────────────────────────────┤
│ Config Layer (symphony-config)         : WORKFLOW.md parser, figment │
│                                          layered config, hot-reload  │
└──────────────────────────────────────────────────────────────────────┘
```

`symphony-core` depends on no concrete adapter. The `symphony-cli`
composition root is the only place where concrete tracker/agent/workspace
implementations are named.

## Trait Surface (target shape — implemented over Phases 2–4)

```rust
// In symphony-core::tracker
#[async_trait]
pub trait IssueTracker: Send + Sync {
    async fn fetch_active(&self) -> Result<Vec<Issue>>;
    async fn fetch_state(&self, id: &IssueId) -> Result<Option<IssueState>>;
    async fn fetch_terminal_recent(&self) -> Result<Vec<Issue>>;
}

// In symphony-core::workspace
#[async_trait]
pub trait WorkspaceManager: Send + Sync {
    async fn ensure(&self, id: &IssueId) -> Result<WorkspacePath>;
    async fn before_run(&self, ws: &WorkspacePath) -> Result<()>;
    async fn after_release(&self, ws: &WorkspacePath) -> Result<()>;
}

// In symphony-core::agent
#[async_trait]
pub trait AgentRunner: Send + Sync {
    async fn start_session(&self, req: AgentRequest)
        -> Result<BoxStream<'static, AgentEvent>>;
    async fn continue_turn(&self, session: SessionId, input: TurnInput)
        -> Result<()>;
    async fn abort(&self, session: SessionId) -> Result<()>;
}

pub enum AgentEvent {
    Started      { session: SessionId },
    Message      { role: Role, text: String },
    ToolUse      { name: String, input: serde_json::Value },
    TokenUsage   { input: u64, output: u64, cached: u64 },
    RateLimit    { retry_after: Duration },
    Completed    { reason: CompletionReason },
    Failed       { error: String },
}
```

The trait surface is intentionally narrow: the orchestrator only ever
consumes the normalized `AgentEvent` stream — never raw JSON.

## Decisions

ADRs append here over time. Each entry: date, context, decision, and a
one-sentence consequence. Example template:

```
### YYYY-MM-DD — Title
Context  : …
Decision : …
Consequence: …
```

(no decisions yet)

## Dependencies

The pre-approved crate budget lives in the workspace `Cargo.toml`. One-line
justifications follow. Anything added beyond this list requires a new ADR.

- **tokio** — only async runtime we target.
- **async-trait** — dyn-compatible async traits at the orchestrator boundary.
- **futures / tokio-stream** — generic stream combinators for `AgentEvent`.
- **serde / serde_json / serde_yaml** — both Codex and Claude speak JSONL;
  WORKFLOW.md front matter is YAML.
- **gray_matter** — splits front matter from prompt body in one pass.
- **reqwest (rustls)** — Linear HTTP client without a system OpenSSL.
- **graphql_client** — typed Linear queries from `.graphql` files.
- **url** — robust URL parsing for tracker base URLs.
- **clap (derive)** — subcommand surface with auto-generated help/env vars.
- **figment** — layered config (defaults → env → WORKFLOW.md).
- **dotenvy** — local-dev `.env` ergonomics; never required in prod.
- **secrecy** — keeps API tokens out of `Debug`/log output.
- **tracing / tracing-subscriber** — structured logging with env-filter.
- **anyhow / thiserror** — boundary vs. library-internal error types.
- **uuid** — per-run identifiers; Claude `--session-id` requires v4.
- **notify** — `WORKFLOW.md` hot-reload watcher.
- **which** — locate `codex` / `claude` binaries on PATH.
- **tempfile** — throwaway workspaces in tests.
- **tokio-test / wiremock / rstest / insta / assert_cmd / predicates** —
  test-only.
