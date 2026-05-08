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
- [ ] Sanitization rule (`[A-Za-z0-9._-]` → `_`) with property tests
- [ ] Containment check rejecting `..`, absolute, and Unicode-look-alike
      attacks [sanitization]
- [ ] Lifecycle hooks (`before_run`, `after_create`, `after_release`)
      with hook-failure tests [containment]

## Phase 4 — AgentRunner abstraction + two backends

- [ ] Define `AgentRunner` trait + `AgentEvent` enum + `SessionId`
- [ ] Codex JSONL protocol decoder; `CodexRunner` spawning `codex
      app-server` [AgentRunner trait]
- [ ] Claude stream-json decoder; `ClaudeRunner` spawning `claude -p
      --output-format stream-json --permission-mode bypassPermissions
      --session-id <uuid>` [AgentRunner trait]
- [ ] Subprocess tests with a fake binary in `tests/fixtures/`
      [CodexRunner, ClaudeRunner]
- [ ] Stream-shape parity test: identical script → identical normalized
      `AgentEvent` sequence (modulo backend-only events) [fake binary]

## Phase 5 — Orchestrator

- [ ] State machine (`Unclaimed → Claimed{Running|RetryQueued} →
      Released`) as a pure module with property tests
- [ ] Poll loop with `tokio::time::interval`, jitter, bounded concurrency
      [state machine]
- [ ] Retry queue with exponential backoff (cap from config) [state
      machine]
- [ ] Reconciliation pass each tick (drop runs whose issue left active
      states) [poll loop]

## Phase 6 — Tandem mode

- [ ] `TandemRunner` wrapping two inner `AgentRunner`s with strategy enum
- [ ] `draft-review` strategy: lead drafts, follower reviews, orchestrator
      decides accept-or-rerun [TandemRunner]
- [ ] `split-implement` strategy: lead plans, follower executes claimed
      subtasks via tool-use [TandemRunner]
- [ ] `consensus` strategy: both run, pick the output with greater
      test-pass delta [TandemRunner]
- [ ] Telemetry: per-agent tokens, agreement rate, cost-per-turn

## Phase 7 — CLI binary

- [ ] `symphony validate <path>` subcommand wired to `WorkflowLoader`
- [ ] `symphony run` subcommand composing real adapters
- [ ] `symphony status` snapshot output
- [ ] Graceful SIGINT shutdown that drains in-flight turns
- [ ] README "Quickstart" section ending with a first dispatched mock
      issue
