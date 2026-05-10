# Parallel Plus Sequential Decomposition Fixture

This fixture documents the canonical v3 mixed dependency graph for a broad
parent issue. Two children can run as independent roots, while the third child
must wait for both roots to reach terminal policy state before specialist
dispatch.

<!-- decomposition-scenario:start -->
```yaml
parent:
  key: parent
  title: Ship operator dependency graph display
  integration_owner: platform_lead
children:
  - key: A
    title: Add dependency graph query
    role: backend_engineer
    scope: Load parent, child, and blocker edge state for one broad work item.
    depends_on: []
    acceptance_criteria:
      - Query returns every child belonging to the parent.
      - Query includes local blocker status and tracker sync status.
  - key: B
    title: Add graph JSON formatter
    role: backend_engineer
    scope: Convert dependency graph rows into stable machine-readable output.
    depends_on: []
    acceptance_criteria:
      - Formatter preserves prerequisite-to-dependent edge direction.
      - Formatter includes blocked child reasons without requiring tracker access.
  - key: C
    title: Add graph CLI rendering
    role: cli_engineer
    scope: Render dependency graph status for operators in the issue graph command.
    depends_on: [A, B]
    acceptance_criteria:
      - C is not specialist-dispatch eligible while either A or B has an open blocker edge.
      - C becomes eligible only after both A and B dependency blockers resolve.
expected_blocks_edges:
  - blocker: A
    blocked: C
    reason: C depends on A
  - blocker: B
    blocked: C
    reason: C depends on B
dispatch_order:
  initially_eligible: [A, B]
  blocked_until_terminal:
    C: [A, B]
```
<!-- decomposition-scenario:end -->

In durable state, `A` and `B` both become open `blocks` edges targeting `C`.
Resolving only one edge is not enough: `C` stays parked until all incoming
dependency blockers are terminal, resolved, or explicitly waived by policy.
