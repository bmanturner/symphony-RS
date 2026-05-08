# Ralph Loop: Symphony-RS v2

You are building **Symphony-RS v2**, a specialist-agent orchestration product.

Read these files first, in this order:

1. `SPEC_v2.md` — product contract.
2. `ARCHITECTURE_v2.md` — target architecture.
3. `PLAN_v2.md` — strategic phase plan.
4. `CHECKLIST_v2.md` — executable iteration queue.
5. Existing implementation files (`SPEC.md`, `ARCHITECTURE.md`, `PLAN.md`, `CHECKLIST.md`) as current-state context to reshape, not preserve.

The goal is to shape Symphony-RS directly into a product that reliably turns issues into integrated, QA-verified changes through configurable specialist agents.

## North Star

A broad issue should be decomposed, routed to specialists, consolidated by a single integration owner, and gated by exhaustive QA that can file blockers and follow-up issues.

If a change makes agents run but weakens integration truth or QA authority, it is the wrong change.

## Non-Negotiable Product Behaviors

1. **Integration ownership is first-class.**
   - Default role name may be `platform_lead`.
   - Semantic role kind is `integration_owner`.
   - This role owns decomposition, dependency truth, canonical integration branch/worktree, QA request, and parent closeout.

2. **QA is a gate.**
   - Default role name may be `qa`.
   - Semantic role kind is `qa_gate`.
   - QA can reject, file blockers, file follow-ups, and force rework.

3. **Specialists are configurable.**
   - Do not hardcode a fixed org chart.
   - Roles beyond integration owner and QA are workflow-defined.

4. **Blockers are structured workflow objects.**
   - Comments are not enough.
   - QA-created blockers block parent completion unless explicitly waived.

5. **Follow-up issue creation is normal.**
   - Agents should feel comfortable filing or proposing follow-ups according to policy.

6. **Workspace/branch truth is verified.**
   - Do not trust convention.
   - Verify cwd, workspace containment, branch/ref, and tree cleanliness before mutation-capable runs.

7. **Done means gated.**
   - Parent done requires children, integration, no blockers, QA, and approvals according to workflow policy.

## Iteration Protocol

### Step 1 — Orient

Run:

```bash
git status --short --branch
git log --oneline -10
```

Read the v2 files listed above. If they are missing, restore or create them before implementing product code.

### Step 2 — Pick one checklist item

Open `CHECKLIST_v2.md`. Pick the first unchecked item whose dependencies are satisfied.

If the next item is too large, add a decomposition item above it and complete that decomposition first.

One checklist item = one commit.

### Step 3 — Implement with tests

Rules:

- Write tests with the implementation.
- Prefer pure domain/state tests before live adapter integration tests.
- Keep public APIs documented.
- Do not paper over workflow invariants in prompts when they belong in state or typed config.
- Do not add a dependency without an ADR in `ARCHITECTURE_v2.md`.
- Change existing behavior when the v2 product direction requires it; keep removals scoped and tested.

### Step 4 — Verify

Run the strongest available checks for the touched area.

Default full gate:

```bash
cargo fmt --all
cargo clippy --workspace --all-targets -- -D warnings
cargo test --workspace
cargo doc --workspace --no-deps
```

If the environment lacks Rust/Cargo, do not claim verification. State the limitation in the commit/body or handoff.

### Step 5 — Update state files

- Tick the completed item in `CHECKLIST_v2.md`.
- Add newly discovered follow-up work to `CHECKLIST_v2.md`.
- Add architectural decisions to `ARCHITECTURE_v2.md` as ADRs.
- Keep `PLAN_v2.md` strategic; do not dump task noise there.

### Step 6 — Commit and stop

Commit format:

```text
feat(v2-core): add durable work item model [N tests]
```

Then stop. Do not start a second checklist item in the same iteration.

## Engineering Tenets

- Kernel owns workflow invariants.
- Adapters own protocol quirks.
- Prompts guide agents; state machines enforce gates.
- Durable event history beats inferred state.
- Integration owner serializes broad work.
- QA protects users from our optimism.
- Follow-up issues are signal, not clutter.
- Workspace isolation is a safety feature and a debugging feature.

## Common Failure Modes To Avoid

### Premature parent completion

Bad: parent closes because children were created.
Good: parent closes only after children complete, integration succeeds, QA passes, and blockers are resolved.

### Split-brain integration

Bad: multiple agents merge independently into competing branches.
Good: one integration owner maintains canonical branch/worktree truth.

### Static-only QA on runtime-sensitive work

Bad: QA reads code and signs off on UI/TUI/runtime behavior.
Good: QA captures runtime evidence or explicitly records why runtime evidence was impossible and accepted by policy.

### Prompt-only governance

Bad: “Tell the agent not to close early.”
Good: make early close impossible in the state transition function.

### Follow-up suppression

Bad: agents hide adjacent problems because they are out of scope.
Good: agents file/propose follow-ups and mark whether they block current acceptance.

## Completion Sentinel

When every v2 checklist item is complete and the full verification gate passes, print:

```text
SYMPHONY_V2_READY_FOR_PRODUCT_DOGFOODING
```
