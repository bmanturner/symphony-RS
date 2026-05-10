# Sequential Decomposition Fixture

This fixture documents the canonical v3 child dependency chain for a broad
parent issue. It is intentionally tracker-agnostic: the child keys are
proposal-local labels that will later resolve to tracker issue IDs and durable
`WorkItemId`s during decomposition application.

<!-- decomposition-scenario:start -->
```yaml
parent:
  key: parent
  title: Ship durable dependency orchestration
  integration_owner: platform_lead
children:
  - key: A
    title: Add database migration
    role: backend_engineer
    scope: Create the durable schema needed to store dependency blocker edges.
    depends_on: []
    acceptance_criteria:
      - Migration records decomposition-sourced blocker provenance.
      - Migration preserves existing blocker gate behavior.
  - key: B
    title: Add API implementation
    role: backend_engineer
    scope: Expose dependency edge creation after child work item IDs are known.
    depends_on: [A]
    acceptance_criteria:
      - B is not specialist-dispatch eligible while A has an open blocker edge.
      - A blocks B using a durable blocks edge.
  - key: C
    title: Add UI wiring
    role: frontend_engineer
    scope: Render dependency status for operators.
    depends_on: [B]
    acceptance_criteria:
      - C is not specialist-dispatch eligible while B has an open blocker edge.
      - B blocks C using a durable blocks edge.
expected_blocks_edges:
  - blocker: A
    blocked: B
    reason: B depends on A
  - blocker: B
    blocked: C
    reason: C depends on B
dispatch_order:
  initially_eligible: [A]
  blocked_until_terminal:
    B: [A]
    C: [B]
```
<!-- decomposition-scenario:end -->

In durable state, each dependency becomes a `blocks` edge where the
prerequisite child is stored as `parent_id` / blocker and the waiting child is
stored as `child_id` / blocked.
