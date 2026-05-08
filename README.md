# Symphony-RS

Rust port of OpenAI's Symphony — a headless, single-binary daemon that
polls an issue tracker and dispatches AI coding agents at the issues it
finds.

Symphony-RS deviates from the upstream spec in two ways:

- **Agent-agnostic Agent Runner** — pluggable backends for `codex` and
  `claude` (Claude Code), plus an experimental `tandem` mode that runs
  both concurrently with a configurable lead.
- **Trait-abstracted Issue Tracker** — Linear and GitHub Issues are both
  v1 adapters behind a shared `IssueTracker` trait, exercised by a
  conformance suite parameterised over the trait.

The full design contract lives in [`SPEC.md`](./SPEC.md);
[`ARCHITECTURE.md`](./ARCHITECTURE.md) records the layered diagram and
the ADRs that have accumulated as the project evolved.

## Status

Phases 1–7 are landed. Phase 8 (out-of-process live TUI via HTTP SSE)
is in progress — see [`CHECKLIST.md`](./CHECKLIST.md).

## Quickstart

This walkthrough takes a clean checkout to a first dispatched issue
without needing a Linear or GitHub credential, and without `codex` or
`claude` on `PATH`. It uses the in-repo `MockTracker` and
`MockAgentRunner` so every step is reproducible offline.

### 1. Install the binary

From the repo root:

```sh
cargo install --path crates/symphony-cli
```

This places a `symphony` binary in `~/.cargo/bin`. Verify it's on
`PATH`:

```sh
symphony --version
```

### 2. Validate the quickstart workflow

The fixture lives at
[`tests/fixtures/quickstart-workflow/`](./tests/fixtures/quickstart-workflow/)
and pairs a `WORKFLOW.md` (front-matter config + prompt body) with an
`issues.yaml` of canned issues for the `MockTracker`.

```sh
symphony validate tests/fixtures/quickstart-workflow/WORKFLOW.md
```

`validate` parses the front matter, layers `SYMPHONY_*` env overrides,
checks invariants (SPEC §10.1), and prints the resolved config. Exits
0 on success, non-zero on the first error — wire it into CI as a
pre-deploy smoke test.

### 3. Inspect what would be dispatched

`symphony status` is a point-in-time snapshot of the tracker's active
set — i.e. exactly what the next poll tick would see:

```sh
symphony status tests/fixtures/quickstart-workflow/WORKFLOW.md
```

You should see two issues (`QUICK-1`, `QUICK-2`) listed under the
`Todo` and `In Progress` states from `issues.yaml`.

### 4. Run the orchestrator against the fixture

`symphony run` starts the poll loop and dispatches issues at the
configured agent. The quickstart fixture deliberately omits
`workspace.root` so you can point the workspace at a scratch directory
via the `SYMPHONY_WORKSPACE__ROOT` env var (figment env layering, SPEC
§5.4):

```sh
mkdir -p /tmp/symphony-quickstart
SYMPHONY_WORKSPACE__ROOT=/tmp/symphony-quickstart \
  RUST_LOG=info \
  symphony run tests/fixtures/quickstart-workflow/WORKFLOW.md
```

Within one poll tick (200ms in the fixture) the orchestrator:

1. Calls `MockTracker::fetch_active`, which returns the canned
   `QUICK-1` and `QUICK-2` issues from `issues.yaml`.
2. Claims each via the in-memory state machine (bounded by
   `agent.max_concurrent_agents = 2`).
3. Asks `WorkspaceManager` to materialize a per-issue directory under
   `SYMPHONY_WORKSPACE__ROOT`. You should see `QUICK-1/` and `QUICK-2/`
   appear in `/tmp/symphony-quickstart` — that's the smallest
   observable evidence dispatch happened.
4. Spawns the `MockAgentRunner`, which emits a scripted
   `Started → Message → Completed` sequence per turn.

Press `Ctrl-C` to send SIGINT. The orchestrator logs
`SIGINT received; cancelling poll loop`, drains in-flight turns up to
the configured deadline, and exits 0 with `symphony run exited
cleanly`.

### 5. Switch to a real backend

To swap the mock backends for real ones, edit the fixture's front
matter (or copy it elsewhere) and change the `tracker.kind` and
`agent.kind` values:

- `tracker.kind: linear` — set `LINEAR_API_KEY` in the environment.
- `tracker.kind: github` — set `GITHUB_TOKEN` and `tracker.repo`.
- `agent.kind: codex` — requires `codex` on `PATH`.
- `agent.kind: claude` — requires `claude` on `PATH`.
- `agent.kind: tandem` — composes two of the above; see SPEC §6 and
  ARCHITECTURE.md for the strategy knobs.

Every config key is documented inline in
[`crates/symphony-config/src/types.rs`](./crates/symphony-config/src/types.rs).

## Development

```sh
cargo fmt --all
cargo clippy --workspace --all-targets -- -D warnings
cargo test --workspace
cargo doc --workspace --no-deps
```

The build must be green at every commit — see the iteration protocol
in [`CLAUDE.md`](./CLAUDE.md).

## License

Dual-licensed under MIT or Apache-2.0, at your option.
