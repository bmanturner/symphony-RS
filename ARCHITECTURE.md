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
   we expose an `IssueTracker` trait with two v1 adapters: Linear (via
   `graphql_client` + Linear's GraphQL) and GitHub Issues (via
   `octocrab`, REST for writes and GraphQL for batched polling). A
   shared conformance suite parameterised over the trait keeps both
   implementations honest. A third adapter (Jira, Plane, …) must be
   addable without touching `symphony-core`.

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
│   symphony-tracker   : Linear (GraphQL), GitHub (REST+GraphQL), Mock │
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

### Per-adapter `Issue` conventions

Both v1 trackers return the same `Issue` shape, but each backend models
some fields differently. Adapters do the translation work; the
orchestrator never branches on tracker kind. Optional fields are
**never fabricated** when the source backend lacks the data.

| Field         | Linear                           | GitHub Issues                                                  |
|---------------|----------------------------------|----------------------------------------------------------------|
| `id`          | Linear UUID                      | `<owner>/<repo>#<number>` (composite, stable per repo)         |
| `identifier`  | `ABC-123` (Linear's identifier)  | `#42` (issue number)                                           |
| `state`       | Workflow state name, lowercased  | `open` / `closed`, *or* a configured label name, lowercased    |
| `priority`    | Linear 0–4                       | Mapped from configured priority labels (else `None`)           |
| `branch_name` | Linear's `branchName` field      | Linked-PR head ref if present (else `None`)                    |
| `blocked_by`  | `blockedByIssues` relation       | Parsed from "blocked by #N" / "depends on #N" body refs        |
| `labels`      | Lowercased label names           | Lowercased label names                                         |

Active-state matching is config-driven (`tracker.active_states`). For
Linear those are workflow state names; for GitHub they are the literal
`open` / `closed` enum or label names. The conformance suite asserts
that each adapter's interpretation matches this contract.

## Decisions

ADRs append here over time. Each entry: date, context, decision, and a
one-sentence consequence. Example template:

```
### YYYY-MM-DD — Title
Context  : …
Decision : …
Consequence: …
```

### 2026-05-08 — Ship Linear *and* GitHub Issues at v1
**Context.** A single shipped adapter (Linear) would force every adopter
to bridge ticket-system ↔ code-system. Co-locating tickets with the code
collapses that bridge for the largest possible audience and gives us a
second backend with meaningfully different semantics — open/closed-only
states, labels-as-priority, PR-derived branch names — to keep the trait
honest.

**Decision.** Phase 2 ships two adapters: Linear via `graphql_client`,
and GitHub Issues via `octocrab` (REST for mutations, GraphQL for
batched polling). Both must pass an identical conformance suite
parameterised over `IssueTracker`. The trait surface is unchanged from
the SPEC abstraction; the per-adapter conventions table above is the
contract for how each backend's quirks map to the normalised `Issue`.

**Consequence.** All fields one backend can express natively but the
other infers stay `Option<…>`, and adapters never fabricate them. A
third adapter can be added without touching `symphony-core` by
implementing the trait and the conformance suite.

### 2026-05-08 — Logging env vars: `SYMPHONY_LOG` over `RUST_LOG`
**Context.** The CLI needs an env-filter directive and a format toggle.
`RUST_LOG` is the convention every Rust user knows, but it leaks into
embedded libraries (e.g. `reqwest`) and forces users to carry global
verbosity changes whenever they want Symphony-only debug output. The
`SYMPHONY_*` namespace is the same namespace `figment` uses for layered
config, so co-locating logging knobs there keeps a single mental model.

**Decision.** `SYMPHONY_LOG` is the primary filter directive;
`RUST_LOG` is read as a fallback only. `SYMPHONY_LOG_FORMAT` selects
`pretty` (default, ANSI when stderr is a TTY) or `json` (one object per
line). Unknown values warn and fall back to `pretty`. Malformed filter
directives fall back to `info` rather than panicking — a typo in a
deployed env var should never crash the daemon.

**Consequence.** Operators get a Symphony-scoped knob without losing
the `RUST_LOG` muscle memory. `logging::init` is idempotent (a
re-install is a no-op) so test harnesses that pre-install a subscriber
are not broken.

### 2026-05-08 — Typed `WorkflowConfig` with `deny_unknown_fields` on nested sections
**Context.** SPEC §5.3 says unknown *top-level* keys SHOULD be ignored
for forward compatibility, but says nothing about typos inside known
sections. A silent fallback of `polling.interva_ms: 10` to the 30 s
default is the kind of bug that only surfaces when an operator wonders
why their override "didn't work." We also need defaults to be auditable
without grepping serde attributes.

**Decision.** Every nested config struct (`TrackerConfig`,
`PollingConfig`, `HooksConfig`, `AgentConfig`, `CodexConfig`, …) carries
`#[serde(deny_unknown_fields)]`. Every default value is a free function
named `default_<key>` so a reviewer can audit a SPEC default with
`git grep default_max_turns`. Codex pass-through fields stay typed as
`serde_yaml::Value` because SPEC §5.3.6 explicitly directs implementors
to treat them as opaque. `agent.kind` and `tracker.kind` are typed enums
so the orchestrator can `match` exhaustively on backend selection.

**Consequence.** `WorkflowConfig` cannot derive `Eq` (the Codex
pass-through values are `serde_yaml::Value`); `PartialEq` is enough for
the round-trip tests we need. Adding a new tracker or agent kind is a
two-line change to the relevant enum plus an arm in `validate`.

### 2026-05-08 — Layered config: WORKFLOW.md > env > defaults
**Context.** SPEC §5.4 says "Environment variables do not globally
override YAML values" — the upstream model only honours env *when the
YAML explicitly references `$VAR_NAME`*. We deviate to give operators a
proper layered config (so a daemon-wide `SYMPHONY_TRACKER__API_KEY` does
not need every per-repo `WORKFLOW.md` to reference it), but we keep the
spec's spirit: anything written into the YAML wins.

**Decision.** `LayeredLoader` builds a `figment::Figment` with three
providers in order — `Serialized::defaults(WorkflowConfig::default())`,
`Env::prefixed("SYMPHONY_").split("__")`, then `Yaml::string(front_matter)`.
Later providers override earlier ones, so YAML > env > defaults. The
`__` separator lets nested keys be expressed (`SYMPHONY_POLLING__INTERVAL_MS`).
`WorkflowLoader` stays as the simple "split file" parser; production
callers reach for `LayeredLoader`.

**Consequence.** Typos in env keys (`SYMPHONY_POLLING__INTERVA_MS`) are
hard errors via `deny_unknown_fields`, the same way YAML typos are. Env
values are coerced from strings during typed extraction, so callers can
set `SYMPHONY_AGENT__KIND=claude` without quoting. The `figment::Error`
variant is boxed inside `LayeredLoadError::Merge` because it is large
enough to trip clippy's `result_large_err` lint otherwise. Tests use
`figment::Jail` (gated behind figment's `test` feature, enabled as a
dev-dependency only) so env mutations are process-isolated.

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
- **octocrab** — established Rust GitHub client; one auth surface for
  both REST mutations and GraphQL batched polling.
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
