# Ralph Loop: Symphony-RS v2

You are building **Symphony-RS v2**, a specialist-agent orchestration product.

Read these files first, in this order:

1. `SPEC_v2.md` — product contract.
2. `ARCHITECTURE_v2.md` — target architecture.
3. `PLAN_v2.md` — strategic phase plan.
4. `CHECKLIST_v2.md` — executable iteration queue.
5. Existing implementation files (`SPEC.md`, `ARCHITECTURE.md`, `PLAN.md`, `CHECKLIST.md`) as current-state context to reshape, not preserve.

The goal is to shape Symphony-RS directly into a product that captures the useful essence of the Paperclip workflow: specialist decomposition, single-owner integration, rigorous QA, blockers, follow-ups, and durable evidence.

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
   - QA rigor scales to the risk of the change. Not every task needs exhaustive verification; pick the strongest check that fits the change.

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

Do the item.

### Step 3 — Implement with tests

Rules:

- Write tests with the implementation.
- Prefer pure domain/state tests before live adapter integration tests.
- Keep public APIs documented.
- Do not paper over workflow invariants in prompts when they belong in state or typed config. The tracker remains product/work truth; SQLite is orchestration truth and retry/recovery evidence.
- Do not add a dependency without an ADR in `ARCHITECTURE_v2.md`.
- Change existing behavior when the target product direction requires it; keep removals scoped and tested. Repackage useful current code such as tandem rather than leaving it dangling.

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
- Only add new checklist items for work that genuinely blocks the North Star. Otherwise note observations in the commit body and move on. Default to *not* adding items.
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

Bad: agents hide adjacent problems that genuinely block the North Star.
Good: agents surface adjacent problems that matter; they note minor observations in the commit body rather than spawning checklist items for every drive-by thought.

### Decomposition sprawl

Bad: every task spawns a "decompose X" entry plus 5–8 sub-items, and the decomposition entry itself counts as progress. The checklist grows visibly nested while real work stalls.
Good: the checklist stays flat. Tasks that fit in one coherent commit get done. Decomposition happens in your head, not as a committed checklist edit. If you find yourself about to add a sub-bullet, stop — pick the smallest coherent commit you can land for the parent item and do that instead. Finishing is the goal; the checklist should shrink, not grow.

### Skipping the hard half

Bad: a task says "Add X model + durable Y table + Z primitive" and the agent ships only the in-memory primitive while leaving the durable table unbuilt — then proceeds to wire downstream code against the in-memory surface.
Good: when an item names persistence, persist. When an item names a durable table, write the migration. If the full item genuinely cannot land in one commit, do the persistence-bearing piece first — never the easy in-memory half — and only tick the item when its full surface (including durability) is built.

## Completion Sentinel

When every v2 checklist item is complete and the full verification gate passes, print:

```text
SYMPHONY_V2_READY_FOR_PRODUCT_DOGFOODING
```
