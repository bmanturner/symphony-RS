# QA

QA is a product gate in Symphony-RS v2. It is not a courtesy review, a final comment, or a prompt convention. The QA gate records durable verdicts, links evidence to the integrated work, files blockers when failures are found, and decides whether the parent can move toward done.

The default QA role name is `qa`, but the workflow contract is the semantic role kind: `qa_gate`.

## QA Example

The example between the markers is loaded by the test suite.

<!-- qa-example:start -->
```yaml
roles:
  platform_lead:
    kind: integration_owner
    description: Owns decomposition, integration branch, QA request, and parent closeout.
    agent: lead_agent
  qa:
    kind: qa_gate
    description: Verifies acceptance criteria, files blockers, and rejects incomplete work.
    agent: qa_agent
    can_file_blockers: true
    can_file_followups: true
    required_for_done: true

qa:
  owner_role: qa
  required: true
  exhaustive: true
  allow_static_only: false
  can_file_blockers: true
  can_file_followups: true
  blocker_policy: blocks_parent
  waiver_roles: [platform_lead]
  evidence_required:
    tests: true
    changed_files_review: true
    acceptance_criteria_trace: true
    visual_or_runtime_evidence_when_applicable: true
  rerun_after_blockers_resolved: true
```
<!-- qa-example:end -->

## Verdicts

A QA run writes one durable verdict for the work item it verifies:

- `passed`: QA accepted the integrated work. Parent closeout may proceed only if the other gates also pass.
- `failed_with_blockers`: QA found failures and created blocker records. These blockers block parent completion by default.
- `failed_needs_rework`: QA rejected the work without creating structural blockers. The work routes to rework.
- `inconclusive`: QA could not reach a trustworthy answer. The item remains in QA or waits for operator action.
- `waived`: a configured waiver role accepted the risk with a required reason.

Agent handoffs can request a verdict, but the kernel enforces the gate. `ready_for: done` does not bypass QA when `qa.required: true`.

## Evidence

QA rigor should scale with the risk of the change. A docs-only fix may need changed-file review and targeted rendering checks. Runtime-sensitive CLI, TUI, workspace, tracker, or scheduler changes should include runtime evidence, tests, logs, screenshots, or an explicit reason that runtime evidence was impossible and accepted by policy.

The durable QA record should make the closeout decision explainable without reading a transcript. At minimum, evidence should cover acceptance criteria, changed files, tests or checks run, unresolved risks, and any runtime or visual evidence required by the workflow.

## Blockers

QA-created blockers are structured workflow objects. They are not just comments.

When `qa.blocker_policy: blocks_parent`, an open QA blocker prevents parent completion until it is resolved or waived. The blocker should identify the blocked work item, the reason, severity, authoring run or role, and current status. After blocker rework completes, `qa.rerun_after_blockers_resolved: true` sends the integrated work back through QA instead of treating the resolved blocker as a pass.

Use `failed_with_blockers` when QA can name concrete missing work. Use `failed_needs_rework` when the result is unacceptable but the failure does not need a durable dependency edge.

## Waivers

Waivers are deliberate risk acceptances. They require two things:

- `waiver_role` must be listed in `qa.waiver_roles`.
- `reason` must be non-empty and durable.

If `qa.waiver_roles` is empty, the workflow refuses QA waivers. A waiver should be rare and operator-visible; it is not a shortcut around an inconvenient blocker. The closeout gate may continue after a valid waiver only if no other required gate is failing.

## Follow-Ups

QA can file or propose follow-up issues when the discovered work is real but should not block the current parent. Blocking follow-ups participate in the blocker graph. Non-blocking follow-ups are linked to the source work item and labeled according to `followups.non_blocking_label`.

Minor observations that do not affect the North Star should stay in the QA summary or commit body rather than expanding the checklist.

## Operator Commands

Inspect durable QA evidence for a work item:

```bash
cargo run -p symphony-cli -- qa verdict ISSUE-123
```

Inspect parent, child, and blocker relationships:

```bash
cargo run -p symphony-cli -- issue graph ISSUE-123
```

Done means gated: children, integration, blockers, QA, PR policy, and approvals must agree before the parent can close.
