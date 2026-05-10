# Upgrade Notes

This note is for repositories or operators moving from the original Symphony-RS workflow shape to the v2 specialist-orchestration product.

v2 is not a compatibility layer around the old poll-loop contract. It promotes the useful pieces of the existing implementation into a gated workflow kernel: configurable specialists, one integration owner, durable state, structured blockers, follow-ups, workspace claims, and QA verdicts.

## Replace Fixtures

Rewrite existing `WORKFLOW.md` fixtures to the v2 schema instead of carrying v1 aliases forward.

The current reference fixtures are:

- `tests/fixtures/sample-workflow/WORKFLOW.md` for the full v2 worked example.
- `tests/fixtures/quickstart-workflow/WORKFLOW.md` for an offline smoke workflow using test-only mock adapters.

The old shape centered on top-level `agent`, tracker active states, and `status`. The v2 shape centers on named `roles`, named `agents`, `routing`, `workspace`, `branching`, `integration`, `pull_requests`, `qa`, `followups`, and `observability`. The top-level `status` block moved under `observability.sse`, and unknown YAML keys are intentionally rejected.

## Fold V1 Docs Into V2 Docs

The v1 planning files are archived in `docs/legacy/` for historical context only. Operator-facing guidance should point at the v2 documents:

- `docs/workflow.md` for the complete workflow schema.
- `docs/roles.md` for integration owner, QA gate, and configurable specialist semantics.
- `docs/workspaces.md` for workspace and branch claims.
- `docs/qa.md` for verdicts, blockers, evidence, waivers, and follow-ups.

Do not maintain a parallel v1 checklist or root-level v1 architecture as product truth. If a legacy detail still matters, restate it in the v2 document that owns the behavior.

## Apply V3 Dependency Orchestration As An Addendum

`SPEC_v3.md` extends the v2 productization path with tracker-backed dependency orchestration between child issues created by decomposition. It does not redefine v2 roles, QA, PR, workspace, blockers, or follow-up behavior.

Use the v3 root documents as the active reference for this addendum:

- `SPEC_v3.md` for the dependency orchestration product contract.
- `ARCHITECTURE_v3.md` for the implementation shape over existing v2 crates.
- `PLAN_v3.md` for the strategic rollout.
- `CHECKLIST_v3.md` for one-commit iteration items.

The practical upgrade point is narrow: `ChildProposal.depends_on` must become durable `blocks` edges, scheduler dispatch must respect those edges, and capable trackers should receive equivalent structural blocker relations.

## Repackage Tandem

The old `tandem` agent behavior is not a backend in v2. It is a composite agent strategy over configured backend profiles:

```yaml
agents:
  lead_agent:
    backend: codex
    command: codex app-server
  qa_agent:
    backend: claude
    command: claude -p --output-format stream-json --permission-mode bypassPermissions
  lead_pair:
    strategy: tandem
    lead: lead_agent
    follower: qa_agent
    mode: draft_review
```

Keep product backend classes limited to `codex`, `claude`, and `hermes`. Tandem orchestration composes those profiles; it does not create a fourth production backend class.

## Keep Mocks Test-Only

`mock` tracker and agent profiles exist to make fixtures, conformance tests, and deterministic end-to-end scenarios reproducible without live credentials or local agent binaries. They are allowed in `tests/fixtures/` and test code, but should not be documented as an operator-facing production adapter.

For real workflows, configure a GitHub or Linear tracker and a Codex, Claude, Hermes, or composite agent profile.

## Retire Dangling Checklist Items

Old checklist items should not be carried forward as v2 work unless they preserve the North Star: broad work decomposes to specialists, integration truth has one owner, QA gates completion, blockers are structured, follow-ups are normal, and workspace/branch truth is verified.

When a v1 item maps to v2 behavior, move the requirement into `CHECKLIST_v2.md` or the owning v2 doc. When it does not, leave it in `docs/legacy/` as historical context. The active checklist should shrink through completed product increments rather than accumulate legacy cleanup noise.

## Migration Checks

Before treating a workflow as upgraded:

- Run `symphony validate WORKFLOW.md`.
- Confirm exactly one role has `kind: integration_owner` authority for decomposition, integration, QA request, and parent closeout.
- Confirm QA uses a role with `kind: qa_gate` when `qa.required: true`.
- Confirm broad work routes to the integration owner and QA work routes to the QA gate.
- Confirm workspace and branch policies verify cwd, branch/ref, and clean-tree rules before mutation-capable runs.
- Confirm follow-up and blocker policies are explicit.

Done means gated: children, integration, blockers, QA, PR policy, and approvals must agree before parent closeout.
