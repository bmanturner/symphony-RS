# Roles

Roles are the workflow-owned capability profiles that decide who may do what. A role name is operator language; a role kind is kernel language.

For example, a workflow can call its integration owner `platform_lead`, `release_captain`, or `maintainer`. Symphony-RS cares that the role has `kind: integration_owner`. The same rule applies to QA: the default role name is usually `qa`, but the gate is identified by `kind: qa_gate`.

`SPEC_v4.md` extends this model with file-backed role instruction packs
and generated platform-lead assignment catalogs. It does not redefine
the v2 role kinds or the v3 dependency sequencing rules.

## Role Example

The example between the markers is loaded by the test suite.

<!-- roles-example:start -->
```yaml
roles:
  platform_lead:
    kind: integration_owner
    description: Owns decomposition, canonical integration branch, QA request, and parent closeout.
    agent: lead_agent
    max_concurrent: 1
    can_decompose: true
    can_assign: true
    can_request_qa: true
    can_close_parent: true
  qa:
    kind: qa_gate
    description: Verifies acceptance criteria, files blockers, and rejects incomplete work.
    agent: qa_agent
    max_concurrent: 2
    can_file_blockers: true
    can_file_followups: true
    required_for_done: true
  backend_engineer:
    kind: specialist
    description: Implements scoped backend changes and reports evidence.
    agent: codex_fast
  security_reviewer:
    kind: reviewer
    description: Reviews security-sensitive changes without owning implementation.
    agent: qa_agent
  release_operator:
    kind: operator
    description: Performs release, deployment, or data operations.
    agent: lead_agent
```
<!-- roles-example:end -->

## Semantic Kinds

`integration_owner` is the single owner for broad work. It owns decomposition, dependency truth, the canonical integration branch or worktree, QA requests, and parent closeout. Parent completion still requires the configured gates: terminal children, integration success, no unresolved blocking blockers, QA, PR policy, and approvals.

`qa_gate` is the verification authority. It can pass, reject, mark work inconclusive, file blockers, file follow-ups, and force rework. When `qa.required: true`, the configured `qa.owner_role` must resolve to a role with this kind.

`specialist` implements scoped work and emits structured handoffs with changed files, tests, evidence, risks, blockers, follow-ups, and `ready_for` guidance. Specialists do not close broad parent work by themselves.

`reviewer` reviews a dimension such as security, design, performance, docs, or content. Reviewers are useful when routing calls for judgment but not direct implementation ownership.

`operator` performs runtime, deployment, release, data, or environment work where the main risk is operational correctness.

`custom` is a workflow-defined role kind. It is not a new agent backend or a way to bypass gates. Use it when the workflow needs a named responsibility that does not fit the other categories.

## Authority

The kernel treats only `integration_owner` and `qa_gate` as special role kinds. Their defaults are intentionally narrow:

- `integration_owner` defaults to `can_decompose`, `can_assign`, `can_request_qa`, `can_close_parent`, and `can_file_followups`.
- `qa_gate` defaults to `can_file_blockers`, `can_file_followups`, and `required_for_done`.
- `specialist`, `reviewer`, `operator`, and `custom` default to `can_file_followups` only.

Every authority flag can be explicitly overridden in `WORKFLOW.md`, but disabling a gate authority should be rare and visible in review. If a workflow needs to grant extra authority to a non-special role, prefer the smallest specific flag over changing the role kind.

Follow-up creation is deliberately broad. Any role may surface follow-up work by default, while `followups.default_policy` decides whether the request is created directly or routed for approval.

## Routing

Routing maps work items to role names. The role name selects the configured agent profile, concurrency cap, description, and semantic kind. The router does not hardcode an org chart; it only sees the roles declared by the workflow.

Use route rules for ownership, not for gate bypass. A broad issue should route to an `integration_owner`; QA-labeled or QA-queued work should route to a `qa_gate`; scoped implementation should route to specialists or other workflow-defined roles.

## Instruction Packs

v4 role doctrine lives in repo-owned files, conventionally
`.symphony/roles/<role>/AGENTS.md` for operating instructions and
`.symphony/roles/<role>/SOUL.md` for stable quality bar / judgment
doctrine. Prompt assembly order is deterministic:

1. Global workflow prompt from `WORKFLOW.md`.
2. Current role `AGENTS.md` instructions.
3. Current role `SOUL.md` doctrine.
4. Agent profile system prompt.
5. Run context: issue, parent/child graph, dependency blockers, workspace/branch, acceptance criteria, and output schema.

The platform lead's assignment catalog is generated from `WORKFLOW.md`
role metadata, agent profiles, routing rules, and role-local assignment
hints. Normal workflows do not need per-role `ASSIGNMENT.md` files.

## Done

A role's handoff is evidence, not final authority. `ready_for: done` is valid only when the kernel's gates allow it. For broad or decomposed work, done requires the integration owner and QA gate to satisfy their structured checks, with blockers resolved or explicitly waived according to policy.
