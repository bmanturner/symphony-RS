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

### Observation surface (Phase 8)

```rust
// In symphony-core::events
//
// Public observation contract. Emitted by the orchestrator on a
// `tokio::sync::broadcast` bus; the SSE endpoint is one consumer.
// Treat as a stable wire format — additions are non-breaking, removals
// require a major version bump.
pub enum OrchestratorEvent {
    StateChanged   { issue: IssueId, from: Phase, to: Phase },
    Dispatched     { issue: IssueId, session: SessionId },
    AgentEvent     { issue: IssueId, session: SessionId, event: AgentEvent },
    RetryScheduled { issue: IssueId, attempt: u32, due_at: SystemTime, reason: String },
    Reconciled     { dropped: Vec<IssueId> },
    Released       { issue: IssueId, outcome: ReleaseOutcome },
}
```

The observation surface lives **outside** the daemon process:

```
                      orchestrator.subscribe()
   symphony-core ────────────────────────────────► broadcast bus
                                                       │
                                                       ▼
                                       axum  GET /events  (text/event-stream)
                                                       │
                                                       ▼
                                       symphony watch  (separate process)
                                          ratatui + crossterm TUI
```

`symphony status` (Phase 7) is the snapshot view — scriptable,
point-in-time, exits. `symphony watch` (Phase 8) is the live view —
streams the same events that any future consumer (Grafana, custom
exporter, web dashboard) would consume.

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

### 2026-05-08 — `octocrab` for the GitHub Issues adapter
**Context.** SPEC §3.1 names a single tracker; our deviation ships
GitHub Issues alongside Linear. GitHub's API has two surfaces we need
— REST for mutations and GraphQL for batched polling — and we do not
want two HTTP clients with two auth flows. Hand-rolling a GitHub client
to stay outside the crate budget would force us to duplicate retry,
pagination, rate-limit, and auth logic that octocrab already covers.

**Decision.** Add `octocrab` (v0.50) to the workspace crate budget. The
default `default-client` feature is kept so octocrab manages its own
reqwest-with-rustls stack; we do not share the top-level `reqwest`
client with it. Only `symphony-tracker` depends on octocrab —
`symphony-core` and the orchestrator stay adapter-free, consistent with
the layered architecture.

**Consequence.** The GitHub adapter gets one typed surface for REST and
GraphQL, with built-in pagination and rate-limit handling. octocrab
internally pulls in hyper 0.14 + reqwest 0.11 alongside our top-level
reqwest 0.12, which inflates the dep graph but is contained to the
tracker crate. A future migration to a single shared HTTP client is
recorded here as a known follow-up rather than a blocker.

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

### 2026-05-08 — `proptest` for invariant coverage
**Context.** The workspace identifier sanitiser is a pure function with
a small set of structural properties (allowlist closure, idempotence,
char-count preservation, no path-separator survives). Enumerated tests
catch the cases we name; the corner cases — combining marks, mixed-
script identifiers, runs of disallowed bytes — are exactly what random
property coverage finds. Phase 5's orchestrator state machine has the
same shape: pure transitions whose invariants are easier to express as
properties than as exhaustive tables. The bootstrap CLAUDE.md flagged
proptest as a "add when you reach Phase 5" dep; Phase 3's sanitiser
needs it first.

**Decision.** Add `proptest` (v1) to the workspace crate budget,
test-only. Used today by `symphony-workspace` for the four sanitiser
invariants; will be used by `symphony-core` in Phase 5 for the state
machine.

**Consequence.** One additional dev-dependency tree. Property failures
include a minimal shrunk counter-example, which is the affordance we
want when an adapter contributor adds an exotic identifier scheme. No
production code path depends on proptest.

### 2026-05-08 — Collapse the SPEC §7.1 five-state model to two stored variants
**Context.** SPEC §7.1 enumerates `Unclaimed`, `Claimed`, `Running`,
`RetryQueued`, and `Released`. A naive port stores all five as enum
variants, but two of them (`Unclaimed`, `Released`) are equivalent for
scheduling purposes — both mean "no claim exists" — and `Claimed` is
just the union of `Running ∪ RetryQueued`. Storing all five would force
the orchestrator to disambiguate "absent (never claimed)" from "absent
(released)", a distinction that has no scheduling effect.

**Decision.** `state_machine::ClaimState` carries two variants —
`Running` and `RetryQueued { attempt }` — and the storage type is
`HashMap<IssueId, ClaimState>`. Absence from the map represents both
`Unclaimed` and `Released`; [`ReleaseReason`] is captured at the moment
of release for logs and the Phase-8 event bus rather than persisted in
the ledger. The five SPEC states are recoverable as a documented
projection (see the module-level rustdoc), so faithfulness to §7.1 is
preserved on the observable boundary.

**Consequence.** Smaller surface area, cheaper invariants ("running ≤
claimed", "claimed = running + retry_queued"), and `proptest` covers a
state space of two storage variants instead of five. The trade-off:
attempts to release an already-released issue are reported as
`TransitionError::NotClaimed` rather than a distinct "already
released" — which the orchestrator treats as a no-op via
`release_if_present` on reconciliation passes.

### 2026-05-08 — Status Surface as out-of-process TUI over HTTP SSE
**Context.** SPEC §3.1's Status Surface is optional but valuable when
running multiple issues concurrently — Tandem mode is genuinely
confusing without a live view. SPEC §2.2 forbids "rich web UI." The
choice space narrows to: nothing, snapshot CLI, live in-process TUI,
or live out-of-process TUI. Daemons live in systemd units and
container `CMD`s; binding the daemon's lifetime to a terminal would
defeat that.

**Decision.** Phase 8 (after Phase 7's daemon ships) adds an
out-of-process TUI as a separate `symphony watch` subcommand.
`symphony-core` gains an `OrchestratorEvent` broadcast bus.
`symphony run` exposes those events at `GET /events` via `axum` as
`text/event-stream` of NDJSON. The TUI is `ratatui` + `crossterm`,
hand-rolling the SSE client to keep the dep budget tight. Existing
`symphony status` (Phase 7) stays as the snapshot view; `symphony
watch` is the live view. Default bind `127.0.0.1:6280`; disable-able
via `status.enabled = false` in `WORKFLOW.md`.

**Consequence.** The wire format (`OrchestratorEvent` JSON) becomes
the stable contract for any future observer — web dashboards, Grafana
exporters, custom CI integrations — without further changes to the
daemon. Daemons stay headless and systemd-friendly. Three new
production deps enter the budget: `ratatui`, `crossterm`, `axum`. The
broadcast bus is a pure addition to the orchestrator (replay buffer
default 256, configurable) — no behavioural change.

### 2026-05-08 — `tokio-util` for cooperative cancellation
**Context.** The architecture tenets require every async fn that touches
I/O to take a cancellation handle. The poll loop in Phase 5 fans out
dispatch tasks that each own an agent session; SIGINT shutdown (Phase 7)
must drain in-flight work without aborting it mid-turn. `tokio` itself
ships abort-style cancellation (drop the `JoinHandle`) but no shared,
cloneable cooperative signal — `tokio_util::sync::CancellationToken` is
that signal: clone-on-demand, parent/child relationships, and
`cancelled().await` integrates naturally with `tokio::select!`.

**Decision.** Add `tokio-util` (v0.7) to the workspace crate budget,
default-features off, with the `rt` feature enabled (the only feature
needed for `CancellationToken`). The orchestrator's `PollLoop::run`
takes a `CancellationToken`, propagates clones to per-issue dispatch
tasks, and resolves on cancellation by draining the join set rather
than aborting it.

**Consequence.** One additional production dependency, but it's a
first-party tokio crate so the version-skew risk is minimal. No
behavioural change to existing code; the token is plumbed only through
new code paths added during Phase 5.

### 2026-05-08 — Bounded SIGINT drain via `PollLoopConfig::drain_deadline`
**Context.** Pure cooperative cancellation has a failure mode: a wedged
agent backend (subprocess that ignores its cancel token, retry loop
stuck on a network call, `start_session` future blocked on a slow
spawn) keeps `JoinSet::join_next` returning `Some(_)` forever, so
`PollLoop::run` never returns and SIGINT never produces a clean exit.
The architecture tenet "drain in-flight on SIGINT" is correct for the
happy path but underspecified for the bad one.

**Decision.** Add a `drain_deadline: Duration` field to
`PollLoopConfig`. `PollLoop::run` wraps the cooperative drain in a
`tokio::time::timeout(drain_deadline, ...)`. If the timeout fires (or
`drain_deadline` is `Duration::ZERO`), the loop calls
`JoinSet::abort_all()`, drains the aborted handles, and releases the
remaining ledger entries with `ReleaseReason::Canceled`. The CLI
hardcodes 30 s; surfacing it as a `WORKFLOW.md` knob is a follow-up
task that lands when a deployment actually needs to retune it.

**Consequence.** Daemon exit is bounded at `drain_deadline + tracker
tick remainder` even with a misbehaving backend. Aborted runs are
released as `Canceled` (not a new dedicated variant) on the theory
that "SIGINT cancelled the run" describes both cooperative and forced
shutdowns; if observability needs to distinguish them later, that's a
new release reason — not a wire-format break, since the existing
variants stay valid.

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
- **proptest** — test-only; invariant coverage for pure helpers
  (workspace path sanitisation today, orchestrator state machine in
  Phase 5). See ADR `2026-05-08 — proptest for invariant coverage`.
- **ratatui** — Phase 8 only; established Rust TUI library used by
  `symphony watch`. See ADR `2026-05-08 — Status Surface as
  out-of-process TUI over HTTP SSE`.
- **crossterm** — Phase 8 only; cross-platform terminal backend for
  ratatui.
- **axum** — Phase 8 only; HTTP server in `symphony-cli` exposing
  `GET /events` as `text/event-stream`.
- **tokio-util** — `CancellationToken` for cooperative cancellation of
  the orchestrator's poll loop and any in-flight dispatch tasks. See
  ADR `2026-05-08 — tokio-util for cooperative cancellation`.
