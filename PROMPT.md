# Ralph Loop: Symphony-RS

You are building **Symphony-RS**, a Rust port of OpenAI's Symphony service.
The full specification is pinned locally at `SPEC.md` in the repo root —
treat that file as the source of truth (not the upstream URL, which may
drift). We deviate from it in two ways:

1. The Agent Runner is **agent-agnostic** — pluggable backends for both
   `codex` and `claude` (Claude Code), plus an experimental **Tandem** mode
   that runs both concurrently with a configurable lead.
2. The Issue Tracker is **abstracted behind a trait** — Linear and GitHub
   Issues are both v1 adapters, exercised by a shared conformance test
   suite parameterised over the trait. The trait must be designed so a
   third adapter (Jira, Plane, etc.) can be added without touching the
   orchestrator.

This file is read fresh at the start of every loop iteration. It is the
source of truth for direction. `PLAN.md`, `ARCHITECTURE.md`, and
`CHECKLIST.md` are the persistent shared state you read and update.

────────────────────────────────────────────────────────────────────────────
## ITERATION PROTOCOL — do every step, in order

### Step 1 — Orient (read-only, ~3 min budget)
- Confirm `SPEC.md` exists at the repo root. If missing, **stop the
  iteration**, print "SPEC.md missing — restore it before continuing", and
  do not commit. The loop is unsafe to run without it.
- Read `PLAN.md`, `ARCHITECTURE.md`, `CHECKLIST.md` in full.
- Run `git log --oneline -20` and `git status` to see prior iterations.
- Run `cargo check --workspace 2>&1 | tail -50` to see the current build state.
- Run `cargo test --workspace --no-run 2>&1 | tail -30` for test compile state.

If any of those files are missing → jump to **BOOTSTRAP** below.

### Step 2 — Pick exactly one task
- Open `CHECKLIST.md`. Find the **first unchecked item that has all its
  dependencies satisfied** (dependencies are listed in square brackets).
- If no such task exists but unchecked items remain → unblock by adding a
  small "decompose" task at the top of the checklist and pick that.
- If nothing is unchecked AND `cargo test --workspace` passes AND
  `cargo clippy --workspace --all-targets -- -D warnings` passes → emit the
  completion sentinel (see **COMPLETION** below).

### Step 3 — Implement (the meat of the iteration)
- Stay in scope. One checklist item = one commit. ~200 lines of impl + tests is
  a good upper bound. If the task is bigger than that, decompose it first.
- Write tests **in the same commit** as the code they cover. No "tests
  next iteration."
- **Comment exhaustively.** Every public item gets a doc comment
  (`///`) explaining *why it exists and when to use it*, not just what it does.
  Module-level `//!` comments explain the role of the module in the system.
  Inline `//` comments wherever a future reader would otherwise need to ask
  "why this and not the obvious thing?"
- Prefer **established crates** over hand-rolling. The crate budget below is
  pre-approved; introducing a new dependency requires adding a one-line
  justification to `ARCHITECTURE.md` § Dependencies.
- Never disable a failing test. Either fix the code or document a
  `#[ignore = "reason: ..."]` with a follow-up task in `CHECKLIST.md`.
- Never delete prior work to "clean up." If a refactor invalidates a
  module, propose it in `PLAN.md § Pending Refactors` first.

### Step 4 — Verify (gate before commit)
Run, in order, and do not commit if any fail:
```
cargo fmt --all
cargo clippy --workspace --all-targets -- -D warnings
cargo test --workspace
cargo doc --workspace --no-deps  # ensures doc comments compile
```
Capture the test count. Add it to your commit message.

### Step 5 — Update state files
- Tick the completed item in `CHECKLIST.md`.
- If you discovered new tasks, add them under the appropriate section.
- If you made an architectural decision, record it as a one-paragraph ADR
  in `ARCHITECTURE.md § Decisions`.

### Step 6 — Commit and exit
- `git add -A && git commit -m "<conventional-commit-style message>"`.
  Format: `feat(scope): summary [N tests]` or `test(scope): ...` or
  `refactor(scope): ...`. Body explains the *why*.
- Then **stop the iteration**. Do not start another task. The loop will
  restart you with a fresh context.

────────────────────────────────────────────────────────────────────────────
## BOOTSTRAP (iteration 0 only — when PLAN.md is missing)

Do this **and only this** for the first iteration. Then commit and exit.

1. Initialize a Cargo workspace at the repo root with these member crates
   (empty stubs, each with a one-line `lib.rs` doc comment):
   - `crates/symphony-core` — orchestrator, state machine, traits
   - `crates/symphony-config` — `WORKFLOW.md` parsing, typed config
   - `crates/symphony-tracker` — `IssueTracker` trait + Linear and GitHub adapters
   - `crates/symphony-agent` — `AgentRunner` trait + Codex + Claude + Tandem
   - `crates/symphony-workspace` — per-issue workspace lifecycle
   - `crates/symphony-cli` — `symphony` binary

   Edition: `2024`. MSRV: pinned in `rust-toolchain.toml` to a current stable.

2. Add the **pre-approved crate budget** to the workspace `Cargo.toml`
   under `[workspace.dependencies]` (versions resolved with `cargo add`):
   - Async / runtime: `tokio` (full), `async-trait`, `futures`, `tokio-stream`
   - Serde: `serde`, `serde_json`, `serde_yaml`, `gray_matter`
   - HTTP / GraphQL: `reqwest` (rustls), `graphql_client`, `octocrab`, `url`
   - CLI / config: `clap` (derive), `dotenvy`, `secrecy`, `figment`
   - Logging: `tracing`, `tracing-subscriber` (env-filter, json)
   - Errors / IDs: `anyhow`, `thiserror`, `uuid` (v4)
   - Filesystem / process: `notify` (WORKFLOW.md hot-reload), `which`,
     `tempfile`
   - Test: `tokio-test`, `wiremock`, `rstest`, `insta`, `assert_cmd`,
     `predicates`

   No additions outside this list without recording a justification ADR.

3. Read `SPEC.md` from the repo root and write `ARCHITECTURE.md` with
   these top sections:
   - **Goal & non-goals** (lifted from SPEC §2 with our deviations called out)
   - **Layered diagram** (eight Symphony layers + our two deviations)
   - **Trait surface**: `AgentRunner`, `IssueTracker`, `WorkspaceManager`,
     plus the normalized event enum `AgentEvent`
   - **Decisions** (empty section, ADRs append here over time)
   - **Dependencies** (each crate above gets a one-line "why")

4. Write `PLAN.md` with these sections:
   - **Phases** (1–6 below)
   - **Pending Refactors** (empty)
   - **Open Questions** (empty)

5. Write `CHECKLIST.md` decomposing each phase into 4–10 items. Phases:

   **Phase 1 — Foundations**
   - Workspace skeleton compiles
   - `tracing` + structured JSON logging wired through CLI
   - `figment`-based config layering: defaults → env → `WORKFLOW.md`
   - `gray_matter` parser for `WORKFLOW.md` front matter + body
   - Round-trip tests for the workflow loader (valid, missing, malformed)

   **Phase 2 — IssueTracker abstraction + Linear + GitHub Issues**
   - `IssueTracker` trait (async, returns normalized `Issue` per SPEC §4.1.1).
     `branch_name`, `priority`, and `blocked_by` are `Option`s that adapters
     **never fabricate** when the source backend lacks the data.
   - In-memory `MockTracker` for tests
   - Tracker **conformance suite** parameterised over `dyn IssueTracker`:
     asserts active-state filtering, lowercase state normalisation, no
     fabricated optional fields. Runs against `MockTracker` immediately;
     re-runs against each real adapter as it lands.
   - **Linear adapter** using `graphql_client` against Linear's GraphQL API,
     with `wiremock` integration tests covering happy path, 4xx, 5xx,
     malformed responses
   - **GitHub Issues adapter** using `octocrab` (REST for mutations, GraphQL
     for batched polling). Maps issue number → `identifier`, derives
     `branch_name` from linked-PR head ref when present, parses
     `blocked_by` from "blocked by #N" / "depends on #N" body refs.
     `wiremock` integration tests covering the same scenarios as Linear.
   - Reconciliation queries (state refresh, terminal cleanup) implemented
     for both adapters and exercised through the conformance suite

   **Phase 3 — Workspace manager**
   - Sanitization rule (SPEC §4.2: `[A-Za-z0-9._-]` → `_`)
   - Per-issue directory creation under root, with containment checks
   - `before_run` / `after_create` / `after_release` lifecycle hooks
   - Tests covering path traversal attempts and hook failures

   **Phase 4 — AgentRunner abstraction + two backends**
   - `AgentRunner` trait: `start_session`, `continue_turn`, `abort`, plus
     a `Stream<Item = AgentEvent>` of normalized events
   - Normalized `AgentEvent` enum: `Started`, `Message`, `ToolUse`,
     `TokenUsage`, `RateLimit`, `Completed { reason }`, `Failed { error }`
   - **CodexRunner**: spawns `codex app-server`, parses JSON-line protocol,
     maps `thread_id`/`turn_id` → session, extracts token telemetry
   - **ClaudeRunner**: spawns `claude -p --output-format stream-json
     --permission-mode bypassPermissions`, synthesizes session id from
     `--session-id <uuid>`, extracts `usage` from the final `result` event
   - Subprocess tests with a fake binary in `tests/fixtures/`
   - Stream-shape parity tests: same script, both backends, identical
     normalized event sequence (modulo backend-specific events)

   **Phase 5 — Orchestrator**
   - State machine per SPEC §4.1.8: `Unclaimed → Claimed{Running|RetryQueued}
     → Released`
   - Polling loop with `tokio::time::interval`, jitter, bounded concurrency
   - Retry queue with exponential backoff (cap from config)
   - Reconciliation pass on every tick (drop runs whose issue left active
     states)
   - Single-authority guarantee: only the orchestrator mutates `claimed`
   - Property tests with `proptest` over the state machine (add `proptest`
     to the crate budget when you reach this — log it as an ADR)

   **Phase 6 — Tandem mode (bonus)**
   - `TandemRunner` impl of `AgentRunner` that wraps two inner runners
   - Strategies, configurable via `WORKFLOW.md`:
     - `draft-review`: lead drafts the turn, follower reviews and the
       orchestrator either accepts the draft or runs a second turn with
       the merged feedback
     - `split-implement`: lead plans, follower executes specific subtasks
       it claims via tool use
     - `consensus`: both run, orchestrator diffs outputs and picks the one
       with greater test-pass delta
   - `lead = "claude" | "codex"` config knob; `follower` is the other
   - Telemetry: per-agent token counts, agreement rate, cost per turn
   - End-to-end test against mock backends covering each strategy

   **Phase 7 — CLI binary**
   - `symphony run` (default), `symphony validate`, `symphony status`
   - Graceful shutdown on SIGINT (drain in-flight, persist nothing)
   - `assert_cmd` smoke tests

────────────────────────────────────────────────────────────────────────────
## ARCHITECTURE TENETS (don't violate without an ADR)

- **The orchestrator never speaks Codex or Claude protocol.** Only
  `symphony-agent` translates. The orchestrator only sees `AgentEvent`.
- **The orchestrator never speaks Linear.** Only `symphony-tracker` does.
  The orchestrator only sees `Issue`.
- **No `unwrap()` outside tests.** Use `anyhow::Result` at boundaries,
  `thiserror`-derived errors inside library crates.
- **All async fns that touch I/O take a cancellation handle** (use
  `tokio_util::sync::CancellationToken` — add to budget when needed).
- **Every public type and function has a `///` doc comment** that explains
  intent, not signature. CI gate: `RUSTDOCFLAGS="-D warnings" cargo doc`.
- **Tests live next to code** (`#[cfg(test)] mod tests`), with integration
  tests in `crates/<name>/tests/`.
- **No global state.** Pass dependencies; the orchestrator owns the
  composition root.

────────────────────────────────────────────────────────────────────────────
## GUARDRAILS

- One checklist item per iteration. If you finish early, **stop**. The
  next loop will pick up.
- Never silently change the crate budget. Adding a crate is an ADR.
- Never delete a test. If a test is wrong, fix it and explain in the
  commit body.
- Never push to a remote. The loop only commits locally.
- If `cargo test` fails after your change → fix it before committing,
  even if the broken test was unrelated. The build must be green at every
  commit.
- If you spent more than ~30 minutes of wall-clock equivalent (very long
  reasoning + many tool calls) without producing a commit, **stop and
  write a note to `PLAN.md § Open Questions`** describing what's blocking
  you. The next iteration can route around it.

────────────────────────────────────────────────────────────────────────────
## COMPLETION

Emit the literal text `<promise>COMPLETE</promise>` and stop only when
all of the following are true. Verify each by running the command and
quoting the result in your final message.

1. `CHECKLIST.md` has zero unchecked `- [ ]` items.
2. `cargo test --workspace` passes (quote test count).
3. `cargo clippy --workspace --all-targets -- -D warnings` passes clean.
4. `cargo doc --workspace --no-deps` produces no warnings.
5. `symphony validate` runs against the fixture `WORKFLOW.md` in
   `tests/fixtures/sample-workflow/` and exits 0.
6. The README has a "Quickstart" section that walks a new user from
   `cargo install --path crates/symphony-cli` to a first dispatched
   issue against a mock tracker.

If any one of those is not true, do not emit the sentinel. Pick the
next failing condition, add a checklist item for it, and continue the
loop.
