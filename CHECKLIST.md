# Symphony-RS Checklist

One unchecked item per iteration. Dependencies are noted in `[brackets]`
and must be checked off before the dependent item is eligible. Tasks live
under their phase header and are added or refined as the project evolves.

## Phase 0 — Bootstrap

- [x] Workspace skeleton, six member crates, crate budget, three state files

## Phase 1 — Foundations

- [x] `cargo check --workspace` builds clean (resolve any latent crate
      version issues from bootstrap; lock `Cargo.lock`)
- [x] Wire `tracing-subscriber` in `symphony-cli` with env-filter and an
      optional JSON formatter selectable by `SYMPHONY_LOG_FORMAT` [needs
      cargo check green]
- [x] Define a typed `WorkflowConfig` struct in `symphony-config` covering
      the SPEC §5.1 keys (poll interval, concurrency, retry caps, agent
      kind, tracker kind)
- [x] Implement `WorkflowLoader::from_path()` using `gray_matter` to split
      front matter from body; return `{config, prompt_template}`
      [WorkflowConfig]
- [x] Layer config sources with `figment`: defaults → env (`SYMPHONY_*`) →
      WORKFLOW.md; round-trip tests for valid / missing / malformed cases
      [WorkflowLoader]
- [x] Add `tests/fixtures/sample-workflow/WORKFLOW.md` for downstream
      tests and the `symphony validate` smoke test

## Phase 2 — IssueTracker abstraction + Linear + GitHub Issues

- [x] Define `Issue`, `IssueId`, `IssueState` per SPEC §4.1.1 in
      `symphony-core::tracker`. Doc comments must spell out that
      `branch_name`, `priority`, and `blocked_by` are `Option`s and that
      adapters **never fabricate** these when the source backend lacks
      the data.
- [x] Define the `IssueTracker` async trait with `fetch_active`,
      `fetch_state`, `fetch_terminal_recent` [Issue model]
- [x] Implement `MockTracker` in `symphony-tracker::mock` with a
      programmable script for tests [IssueTracker trait]
- [x] Tracker **conformance suite** parameterised over `dyn IssueTracker`
      using `rstest`: asserts active-state filtering against
      `tracker.active_states`, lowercase normalisation of state names,
      stable ordering, and that no adapter fabricates `branch_name` /
      `priority` / `blocked_by`. Runs against `MockTracker` immediately;
      gets re-run against each real adapter as it lands.
      [IssueTracker trait, MockTracker]
- [x] Add `octocrab` to the workspace crate budget via `cargo add -p
      symphony-tracker octocrab` (or workspace-level if other crates need
      it later). Record the rationale as a one-paragraph ADR.
- [x] Generate Linear GraphQL bindings via `graphql_client` from a
      checked-in `.graphql` query set [IssueTracker trait]
- [x] Implement `LinearTracker` translating GraphQL → `Issue`. `wiremock`
      integration tests covering happy path, 4xx, 5xx, malformed
      responses; passes the conformance suite. [Linear bindings,
      conformance suite]
- [x] **GitHubTracker (a)** — `GitHubConfig` + `GitHubTracker` skeleton
      built on `octocrab` with a configurable `base_uri` (so wiremock can
      stand in for `api.github.com`). Implement `fetch_active` paginating
      `GET /repos/{owner}/{repo}/issues?state=open` with state derived from
      a configurable `status:` label prefix (fallback to native open).
      Issue → `Issue` mapping leaves `branch_name = None` and
      `blocked_by = []` for now — those land in (c) and (d). Pure unit
      tests on the normalization helpers + a happy-path wiremock test.
      [octocrab in budget]
- [x] **GitHubTracker (b)** — `fetch_state(&[IssueId])` (per-number GET
      preserving caller order) and `fetch_terminal_recent(&[IssueState])`
      (paginated `state=closed` + label-derived state filtering).
      Wiremock tests covering 4xx (auth + other), 5xx, malformed JSON.
      [GitHubTracker (a)]
- [x] **GitHubTracker (c)** — parse `blocked_by` from issue body
      "blocked by #N" / "depends on #N" refs (no server-side resolution
      yet; `BlockerRef.id` stays `None`, only `identifier` is filled).
      Add `github_canonical_scenario()` mirroring the Linear fixture but
      using GitHub-style identifiers; full conformance suite passes
      against the wiremock-backed adapter. [GitHubTracker (b)]
- [x] **GitHubTracker (d)** — derive `branch_name` from linked-PR head
      ref by querying `GET /repos/{owner}/{repo}/issues/{n}/timeline` for
      cross-referenced PR events; take the most-recently-opened PR's
      `head.ref`. Extend `github_canonical_scenario()` to cover this and
      re-run the conformance suite. [GitHubTracker (c)]
- [x] Reconciliation queries: state refresh by id, terminal cleanup —
      implemented for both adapters and exercised via the conformance
      suite. [LinearTracker, GitHubTracker]

## Phase 3 — Workspace manager

- [x] `WorkspaceManager` trait + `LocalFsWorkspace` implementation
- [x] Sanitization rule (`[A-Za-z0-9._-]` → `_`) with property tests
- [x] Containment check rejecting `..`, absolute, and Unicode-look-alike
      attacks [sanitization]
- [x] Lifecycle hooks (`after_create`, `before_run`, `after_run`,
      `before_remove`) with hook-failure tests [containment]

## Phase 4 — AgentRunner abstraction + two backends

- [x] Define `AgentRunner` trait + `AgentEvent` enum + `SessionId`
- [x] Codex JSONL protocol decoder; `CodexRunner` spawning `codex
      app-server` [AgentRunner trait]
- [x] Claude stream-json decoder; `ClaudeRunner` spawning `claude -p
      --output-format stream-json --permission-mode bypassPermissions
      --session-id <uuid>` [AgentRunner trait]
- [x] Subprocess tests with a fake binary in `tests/fixtures/`
      [CodexRunner, ClaudeRunner]
- [x] Stream-shape parity test: identical script → identical normalized
      `AgentEvent` sequence (modulo backend-only events) [fake binary]

## Phase 5 — Orchestrator

- [x] State machine (`Unclaimed → Claimed{Running|RetryQueued} →
      Released`) as a pure module with property tests
- [x] Poll loop with `tokio::time::interval`, jitter, bounded concurrency
      [state machine]
- [x] Retry queue with exponential backoff (cap from config) [state
      machine]
- [x] Reconciliation pass each tick (drop runs whose issue left active
      states) [poll loop]

## Phase 6 — Tandem mode

- [x] `TandemRunner` wrapping two inner `AgentRunner`s with strategy enum
- [x] `draft-review` strategy: lead drafts, follower reviews, orchestrator
      decides accept-or-rerun [TandemRunner]
- [x] `split-implement` strategy: lead plans, follower executes claimed
      subtasks via tool-use [TandemRunner]
- [x] `consensus` strategy: both run, pick the output with greater
      test-pass delta [TandemRunner]
- [x] Telemetry: per-agent tokens, agreement rate, cost-per-turn

## Phase 7 — CLI binary

- [x] `symphony validate <path>` subcommand wired to `WorkflowLoader`
- [x] `symphony run` subcommand composing real adapters
- [x] `symphony status` snapshot output (point-in-time; distinct from
      Phase 8's `symphony watch` live TUI)
- [x] Graceful SIGINT shutdown that drains in-flight turns
- [x] **Decompose Quickstart** — split the original "README
      Quickstart ending in a dispatched mock issue" item into four
      sub-items: mock tracker kind, mock agent kind, quickstart
      fixture + smoke test, README prose. Picked because the original
      single line bundled config, runtime, fixtures, and docs into a
      task far larger than the ~200-LOC iteration budget; each new
      sub-item is independently testable and commitable.
- [ ] Add a `mock` variant to `TrackerKind` in `symphony-config` with a
      `tracker.fixtures` YAML path; `build_tracker` in `symphony-cli`
      loads the fixtures into a `MockTracker`. Round-trip tests for the
      config parsing + a unit test for the CLI factory path.
      [Decompose Quickstart]
- [ ] Add a `mock` variant to `AgentKind` driving a no-op
      `MockAgentRunner` (returns a scripted `Started → Message →
      Completed` sequence per turn). Lets `symphony run` dispatch
      against a fixture without `codex` or `claude` installed. Unit
      tests on the runner and CLI factory wiring.
      [Decompose Quickstart]
- [ ] Add `tests/fixtures/quickstart-workflow/WORKFLOW.md` and
      `issues.yaml` exercising both new mock kinds; `assert_cmd` smoke
      test that `symphony run` against them dispatches at least one
      issue and exits cleanly on SIGINT.
      [mock tracker kind, mock agent kind]
- [ ] README "Quickstart" section walking from `cargo install --path
      crates/symphony-cli` through `symphony validate`, `symphony
      status`, and `symphony run` against the quickstart fixture, with
      a copy-pasteable command block. [quickstart fixture]

## Phase 8 — Status Surface (out-of-process live TUI via HTTP SSE)

SPEC §3.1 layer 7. Implemented out-of-process so the daemon stays
headless and systemd-friendly. Sub-tasks are intentionally small —
keep one-task-per-iteration discipline.

- [ ] Define `OrchestratorEvent` enum in `symphony-core::events`
      covering `StateChanged`, `Dispatched`, `AgentEvent` re-emission,
      `RetryScheduled`, `Reconciled`, `Released`. `Serialize` with
      `#[serde(tag = "type")]` so SSE consumers can switch on type.
      Doc comment declares the wire format stable: additions
      non-breaking, removals require a major bump.
- [ ] Add `tokio::sync::broadcast` event bus to the orchestrator with a
      configurable replay buffer (default 256). Public `subscribe()`
      returns `BroadcastStream<OrchestratorEvent>`. Pure addition — no
      behavioural change. Tests assert that events are emitted on every
      state transition. [OrchestratorEvent, Phase 5 state machine]
- [ ] Add `axum`, `ratatui`, `crossterm` to the workspace crate budget
      via `cargo add --workspace`. One-paragraph ADR documenting why
      these three over hand-rolled alternatives.
- [ ] Add `status` section to `WorkflowConfig`: `enabled` (bool, default
      true), `bind` (default `"127.0.0.1:6280"`), `replay_buffer`
      (default 256). `deny_unknown_fields`, defaults via
      `default_status_*` free functions per the existing convention.
- [ ] Build the SSE handler in `symphony-cli`: an `axum` router with
      `GET /events` that subscribes to the orchestrator's broadcast bus
      and serialises each `OrchestratorEvent` as one SSE `data:` frame.
      Bound concurrent subscribers; drop slow consumers with a
      `Lagged` event. [event bus, axum in budget, status config]
- [ ] Wire the SSE server into `symphony run` lifecycle (start with the
      orchestrator, drain on SIGINT). `wiremock`-style integration test
      that connects a fake client and asserts a scripted event sequence
      arrives in order. [SSE handler]
- [ ] Add `symphony watch [--url <URL>]` subcommand: hand-rolled SSE
      client over `reqwest` (no extra crate), parsing `data: <json>`
      lines. Reconnects with capped exponential backoff; renders a
      "disconnected" banner during retries. [SSE server]
- [ ] TUI scaffold with `ratatui` + `crossterm`: alternate screen,
      raw mode, terminal-resize-safe, hotkey `q` to quit. Renders a
      placeholder layout. [symphony watch, ratatui in budget]
- [ ] TUI panel — **active issues** table: identifier, state, elapsed,
      agent backend. Updates from `StateChanged` and `Dispatched`.
      [TUI scaffold]
- [ ] TUI panel — **cost summary**: tokens (input/output/cached),
      cumulative dollars from `AgentEvent::TokenUsage`. [active issues
      panel]
- [ ] TUI panel — **recent events log**: ring buffer of the last N
      events, colour-coded by variant. Hotkey `f` filters by issue
      identifier substring. [active issues panel]
- [ ] TUI panel — **tandem activity**: visible only when at least one
      running session uses `TandemRunner`. Shows lead/follower roles,
      strategy, current phase (drafting / reviewing / executing),
      agreement rate. [recent events panel, Phase 6 TandemRunner]
- [ ] Hotkey `r` toggles relative ↔ absolute time formatting across
      all panels. [tandem panel]
- [ ] Snapshot tests: `insta`-snapshotted SSE stream against a scripted
      orchestrator run, and ratatui `TestBackend` frame snapshots
      against a scripted event sequence covering every panel.
      [all panels]
- [ ] README "Quickstart" gains a paragraph showing `symphony run` in
      one terminal and `symphony watch` in another, plus a screenshot
      placeholder. [snapshot tests]
