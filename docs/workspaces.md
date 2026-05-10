# Workspaces

Workspaces are the execution boundary for agent runs. In v2, Symphony-RS treats a workspace as a verified claim over a path, branch, owner, cleanup policy, and pre-launch evidence. That is deliberately stronger than "a directory where the agent happens to run": workspace truth is part of the product safety model.

The integration owner owns canonical branch truth for broad work. Specialists may work in isolated branches, but consolidation happens through the configured integration flow, and QA verifies the final integrated state.

## Workspace Example

The example between the markers is loaded by the test suite.

<!-- workspaces-example:start -->
```yaml
workspace:
  root: ./.symphony/workspaces
  default_strategy: issue_worktree
  strategies:
    issue_worktree:
      kind: git_worktree
      base: main
      branch_template: symphony/{{identifier}}
      cleanup: retain_until_done
    integration_worktree:
      kind: existing_worktree
      path: ../symphony-product-integration
      require_branch: symphony/integration/{{parent_identifier}}
      cleanup: retain_always
  require_cwd_in_workspace: true
  forbid_untracked_outside_workspace: true

branching:
  default_base: main
  child_branch_template: symphony/{{identifier}}
  integration_branch_template: symphony/integration/{{identifier}}
  allow_same_branch_for_children: false
  require_clean_tree_before_run: true
```
<!-- workspaces-example:end -->

## Workspace Claims

A `WorkspaceClaim` records the workspace path, strategy, base ref, required branch, owner work item, cleanup policy, and verification report. Agents receive the claim as structured run context, and hooks receive claim metadata rather than only a path.

The runner verifies the claim immediately before launching mutation-capable work. The standard pre-launch gate checks cwd containment, required branch/ref, and clean tree policy. Failed checks block the run and become durable evidence for recovery, status, and QA.

## Strategies

`git_worktree` is the default posture for specialist work. It creates a per-work-item git worktree under `workspace.root`, starts from the configured base ref, and renders the branch from a strict template such as `symphony/{{identifier}}`. Use this when child work should be isolated and later consolidated by the integration owner.

`existing_worktree` reuses a path that an operator already provisioned. Symphony verifies that the path is a git work tree and, when `require_branch` is set, that the actual branch matches. This is useful for long-lived integration worktrees where the integration owner maintains canonical branch state.

`shared_branch` allows multiple work items to use one branch and worktree. It is only valid when `branching.allow_same_branch_for_children: true`; otherwise workflow validation fails. Use it sparingly, because it trades isolation for coordination pressure.

## Branch Policy

`branching.default_base` is the fallback base for new worktrees. `child_branch_template` names specialist branches, while `integration_branch_template` names the canonical integration branch. Both templates should include `{{identifier}}` so claims are traceable to work items.

`require_clean_tree_before_run: true` means mutation-capable runs refuse to start on dirty git state. Combined with `workspace.require_cwd_in_workspace: true`, this prevents agents from mutating a sibling checkout, the wrong branch, or an unreviewed local tree.

Same-branch child execution requires two explicit choices: `allow_same_branch_for_children: true` and a shared workspace strategy. If the config says one without the other, validation rejects it rather than guessing.

## Cleanup

`retain_until_done` keeps workspaces available while work is active and lets the kernel reclaim them after terminal state. It is the safest default for debugging and recovery.

`remove_after_run` deletes the workspace after a run completes. Use it only for disposable isolated worktrees where post-run inspection is less important than disk pressure.

`retain_always` leaves cleanup to the operator. Use it for shared or long-lived integration worktrees.

## Hooks

Workspace hooks run around the claim lifecycle. `after_create` and `before_run` failures block launch; `after_run` and `before_remove` are best-effort unless the workflow explicitly makes them blocking. Keep hooks deterministic, because their output becomes part of the run's evidence trail.

## Operator Checks

Use the workflow validator after editing workspace policy:

```bash
cargo run -p symphony-cli -- validate WORKFLOW.md
```

Use the worktree verifier to inspect a specific issue's claim and pre-launch evidence:

```bash
cargo run -p symphony-cli -- worktree verify ISSUE-123
```

Use recovery when a process exits mid-run or a workspace claim may be orphaned:

```bash
cargo run -p symphony-cli -- recover
```

Done means gated: child work, integration, blockers, QA, PR policy, and approvals must all agree before a parent can close. Workspace verification is one of the facts those gates rely on.
