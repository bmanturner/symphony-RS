# Symphony-RS

Symphony-RS v2 is a headless specialist-agent orchestration product. It
turns tracker issues into routed agent runs, single-owner integration,
QA-gated closeout, blockers, follow-ups, and durable evidence.

The product center is deliberately small:

- **Workflow kernel** — `WORKFLOW.md` defines roles, agents, routing,
  decomposition, workspace/branch policy, integration, QA, follow-ups,
  observability, budgets, and persistence.
- **First-class gate roles** — `integration_owner` owns decomposition,
  canonical integration branch/worktree truth, PR handoff, and parent
  closeout; `qa_gate` can reject, file blockers, create follow-ups, and
  force rework.
- **Limited adapters** — production backends are GitHub/Linear trackers,
  git workspaces, and Codex/Claude/Hermes agents. Test-only mock
  adapters keep fixtures and deterministic scenarios reproducible.

## Quickstart

This walkthrough takes a clean checkout through the v2 workflow shape
without needing a Linear/GitHub credential or a local Codex/Claude/Hermes
binary. It uses the in-repo test fixture at
[`tests/fixtures/quickstart-workflow/`](./tests/fixtures/quickstart-workflow/).

The fixture is intentionally offline: `tracker.kind: mock` reads canned
issues from `issues.yaml`, and `agents.mock_agent.backend: mock` returns
scripted handoffs. Mock adapters are test-only; copy the workflow before
using it as a production starting point.

### 1. Install the binary

From the repo root:

```sh
cargo install --path crates/symphony-cli
```

This places a `symphony` binary in `~/.cargo/bin`. Verify it is on
`PATH`:

```sh
symphony --version
```

### 2. Inspect the v2 workflow fixture

Open
[`tests/fixtures/quickstart-workflow/WORKFLOW.md`](./tests/fixtures/quickstart-workflow/WORKFLOW.md).
The front matter includes the core v2 sections:

- `roles` with `platform_lead` as `kind: integration_owner`, `qa` as
  `kind: qa_gate`, and `worker` as a configurable specialist.
- `routing`, `decomposition`, `workspace`, `branching`, `integration`,
  `pull_requests`, `qa`, and `followups`.
- `observability` and budget settings that make the offline run easy to
  inspect.

The Markdown body below the front matter is the base prompt template.

### 3. Validate the quickstart workflow

```sh
symphony validate tests/fixtures/quickstart-workflow/WORKFLOW.md
```

`validate` parses the front matter and checks the typed v2 invariants:
owner roles exist, role and agent references resolve, tracker states do
not overlap, workspace/branch policy is coherent, and QA/follow-up gates
are internally consistent. It exits 0 on success and non-zero on the
first error.

### 4. Inspect queued work

`symphony status` can preview the tracker-backed candidates for this
offline workflow:

```sh
symphony status tests/fixtures/quickstart-workflow/WORKFLOW.md
```

You should see the canned `QUICK-1` and `QUICK-2` issues from
[`issues.yaml`](./tests/fixtures/quickstart-workflow/issues.yaml).

### 5. Run the orchestrator against the fixture

`symphony run` starts the v2 scheduler. The quickstart fixture omits
`workspace.root` so a smoke run can point workspaces at a scratch
directory via `SYMPHONY_WORKSPACE__ROOT`:

```sh
mkdir -p /tmp/symphony-quickstart
SYMPHONY_WORKSPACE__ROOT=/tmp/symphony-quickstart \
  RUST_LOG=info \
  symphony run tests/fixtures/quickstart-workflow/WORKFLOW.md \
  --state-db /tmp/symphony-quickstart/state.db
```

Within one scheduler tick (200 ms in the fixture), the orchestrator:

1. Reads active candidates from `issues.yaml`.
2. Maps tracker states to normalized work-item status classes.
3. Routes work through the configured roles.
4. Verifies workspace/branch policy before mutation-capable runs.
5. Persists run/workspace/handoff/event evidence to the SQLite database
   passed with `--state-db`.
6. Executes the scripted mock agent.

Press `Ctrl-C` to send SIGINT. The orchestrator logs
the shutdown path, cooperatively drains in-flight work, and exits cleanly.

### 6. Switch to real adapters

To adapt the fixture for real work, copy it into your repository and
replace the test-only adapters:

- `tracker.kind: linear` — set `LINEAR_API_KEY` in the environment.
- `tracker.kind: github` — set `GITHUB_TOKEN` and `tracker.repository`.
- agent `backend: codex` — requires `codex` on `PATH`.
- agent `backend: claude` — requires `claude` on `PATH`.
- agent `backend: hermes` — requires `hermes` on `PATH`.
- composite `strategy: tandem` — wraps two configured agent profiles for
  review or split-implementation workflows.

Keep `platform_lead`/`qa` if those names fit your team, or rename them
while preserving the semantic role kinds: `integration_owner` and
`qa_gate`.

The full schema is documented in
[`docs/workflow.md`](./docs/workflow.md), with role semantics in
[`docs/roles.md`](./docs/roles.md), workspace policy in
[`docs/workspaces.md`](./docs/workspaces.md), and QA behavior in
[`docs/qa.md`](./docs/qa.md).

## Development

```sh
cargo fmt --all
cargo clippy --workspace --all-targets -- -D warnings
cargo test --workspace
cargo doc --workspace --no-deps
```

The full verification gate for product changes is:

```sh
cargo fmt --all
cargo clippy --workspace --all-targets -- -D warnings
cargo test --workspace
cargo doc --workspace --no-deps
```

## License

Dual-licensed under MIT or Apache-2.0, at your option.
