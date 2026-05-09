# Symphony-RS Product Specification v4 Addendum

Status: Draft v4 addendum — targeted requirements for role instruction packs and decomposition role catalogs.

## 1. Scope

SPEC v4 does **not** replace `SPEC_v2.md` or `SPEC_v3.md`.

This addendum covers one missing Paperclip-outcome requirement:

> When an integration-owner role decomposes work, it must have the same durable role knowledge that Paperclip currently provides through per-role instructions and SOUL files: who each specialist is, what they own, what they must avoid, and how to write child issues that route cleanly.

This is not a Paperclip adapter. The goal is repo-owned, Linear/GitHub-backed orchestration with explicit role doctrine.

## 2. Current Gap

The current codebase has partial prompt machinery:

- `WORKFLOW.md` front matter defines `roles:` and `agents:`.
- The Markdown body of `WORKFLOW.md` is loaded as one global `prompt_template`.
- `RoleConfig` supports `kind`, `description`, `agent`, concurrency, and authority flags.
- `AgentBackendProfile` supports an optional inline `system_prompt`.
- `symphony-core::prompt::PromptContext` can render the current role, workspace, parent, children, blockers, acceptance criteria, and output schema.

That is not enough for Paperclip-style role behavior.

Missing pieces:

- no per-role instruction file paths;
- no `SOUL.md` / doctrine file support;
- no role catalog injected into the platform lead decomposition prompt;
- no structured role assignment metadata in `WORKFLOW.md` beyond terse descriptions and broad routing rules;
- no distinction between role assignment catalog content and backend system prompt;
- no requirement that the platform lead can see every eligible specialist role and its constraints before creating child issues;
- production `run.rs` still uses legacy prompt rendering that only substitutes issue fields.

## 3. Required Outcome

When a platform lead/integration-owner run is asked to decompose a parent issue, the prompt MUST include:

1. The parent issue and acceptance context.
2. The full role catalog for roles eligible to receive child issues.
3. Each role's durable instruction pack summary.
4. Each role's assignment boundaries: owns, does-not-own, required evidence, known pitfalls.
5. The routing rules that will map proposed child work to roles.
6. The dependency/blocker graph format the platform lead must emit.
7. The output schema for decomposition proposals.

A platform lead MUST NOT have to infer role ownership from terse YAML descriptions like `Implements scoped backend changes`.

## 4. Role Instruction Packs

Add an explicit role-instruction surface to workflow config. Prefer file-backed instructions so doctrine is reviewable and reusable.

Example:

```yaml
roles:
  platform_lead:
    kind: integration_owner
    description: Owns decomposition, integration branch, and final PR/QA handoff.
    agent: lead_agent
    instructions:
      role_prompt: .symphony/roles/platform_lead/AGENTS.md
      soul: .symphony/roles/platform_lead/SOUL.md

  elixir_otp_engineer:
    kind: specialist
    description: Owns OTP supervision, GenServers, Phoenix contexts, Ecto changes, and backend test coverage.
    agent: codex_fast
    routing:
      paths_any: [lib/**, test/**, priv/repo/**]
      labels_any: [backend, elixir, otp, database]
      owns:
        - Phoenix/Ecto domain implementation
        - OTP process boundaries and supervision behavior
        - backend integration tests
      does_not_own:
        - TUI layout/copy polish unless paired with a UI role
        - deployment/runtime operations unless explicitly scoped
    instructions:
      role_prompt: .symphony/roles/elixir_otp_engineer/AGENTS.md
      soul: .symphony/roles/elixir_otp_engineer/SOUL.md
```

The platform lead's assignment catalog MUST be assembled from the existing `WORKFLOW.md` role/routing fields (`roles.*.description`, `roles.*.kind`, `roles.*.agent`, `roles.*.routing`, global `routing.rules`, and agent profile metadata). Do not require a separate `ASSIGNMENT.md` file for normal operation.

### 4.1 `role_prompt`

Long-form operating instructions for the role:

- responsibilities;
- forbidden actions;
- required checks;
- expected handoff shape;
- escalation rules.

### 4.2 `soul`

Stable behavioral doctrine/personality/quality bar for the role:

- how the agent thinks;
- taste preferences;
- recurring traps;
- tone and judgment calls.

This is conceptually equivalent to the Paperclip SOUL files. Symphony must not flatten this into a one-line role description.

### 4.3 Assignment Catalog Source

The platform-lead-facing assignment catalog is generated from `WORKFLOW.md`, not from per-role `ASSIGNMENT.md` files.

Role definitions MUST carry enough structured assignment metadata for the catalog builder to render useful guidance:

- `description`: concise human-readable summary;
- `routing` or equivalent role-local match hints: paths, labels, issue types, domains;
- `owns`: bullets describing work this role should receive;
- `does_not_own`: bullets describing work this role should not receive;
- optional `requires`: prerequisite context or dependencies this role usually needs;
- optional `handoff_expectations`: evidence this role must return.

The catalog builder MAY fall back to `description` and global `routing.rules`, but that fallback should produce a validation warning because terse descriptions are not enough for reliable decomposition.

## 5. Prompt Assembly Order

Prompt assembly MUST be explicit and deterministic.

For specialist runs:

1. Global workflow prompt from `WORKFLOW.md` body.
2. Current role `role_prompt`.
3. Current role `soul`.
4. Agent profile `system_prompt`, if configured.
5. Issue context.
6. Parent/child/dependency/blocker context.
7. Workspace/branch context.
8. Acceptance criteria.
9. Required output schema.

For platform-lead decomposition runs:

1. Global workflow prompt.
2. Platform lead `role_prompt`.
3. Platform lead `soul`.
4. Parent issue/repo context.
5. Decomposition policy.
6. Full eligible-role assignment catalog.
7. Existing dependency graph/blocker rules.
8. Required decomposition output schema.

For QA runs:

1. Global workflow prompt.
2. QA `role_prompt`.
3. QA `soul`.
4. Integrated branch / draft PR context.
5. Acceptance trace.
6. Child handoffs and known blockers.
7. Required QA verdict schema.

## 6. Role Catalog Requirements

The platform lead's role catalog MUST include every role with `kind` in:

- `specialist`;
- `reviewer` when review can be requested as child/follow-up work;
- `operator` when runtime/deployment/data work can be child-scoped.

The catalog SHOULD exclude:

- `integration_owner` roles other than the current role;
- `qa_gate` as a normal child implementer, unless the workflow explicitly allows manual QA task issues;
- roles disabled by workflow policy or concurrency/budget constraints.

Each catalog entry MUST include:

- role name;
- role kind;
- human-readable description;
- structured assignment metadata from `WORKFLOW.md` (`owns`, `does_not_own`, role-local routing hints, prerequisites, handoff expectations), with global routing-rule fallback;
- agent profile name/backend;
- concurrency limit;
- allowed tools/toolsets if relevant;
- required evidence/handoff expectations if role-specific.

## 7. Validation Requirements

Workflow validation MUST fail when:

- a configured instruction file path escapes the repository/workflow root;
- a required instruction file is missing;
- a role references an agent profile that does not exist;
- a platform-lead decomposition workflow is enabled but no eligible child-owning roles are available;
- a role instruction pack contains unsupported keys.

Workflow validation SHOULD warn when:

- a specialist role lacks structured assignment metadata (`owns`, `does_not_own`, role-local routing hints, or equivalent);
- a role only has a terse description and no instruction pack;
- the platform lead's decomposition prompt would exceed configured token limits after catalog assembly.

## 8. Runtime Requirements

The runtime MUST:

1. Load instruction files at workflow load time or dispatch preflight.
2. Cache file contents with invalidation on workflow reload.
3. Redact secrets from instruction file content before logging.
4. Include role instruction provenance in run metadata.
5. Fail loud if a required role instruction file cannot be read.
6. Keep transient issue context separate from durable role doctrine.

## 9. Acceptance Criteria

SPEC v4 is satisfied when:

- A workflow can define file-backed `role_prompt` and `soul` per role.
- A workflow can define structured role assignment metadata directly in `WORKFLOW.md`.
- A platform lead decomposition prompt includes an automatically assembled role catalog with enough information to assign child issues correctly.
- Specialist prompts include their own role instructions and SOUL doctrine.
- QA prompts include QA-specific instructions and verdict contract.
- Tests prove production prompt assembly includes role catalog content during decomposition.
- Tests prove production prompt assembly includes only the current role's full instruction pack during specialist execution.
- Tests prove invalid/missing instruction paths fail validation.
- The legacy issue-field-only prompt renderer is no longer the production path for v2 role-aware dispatch.

## 10. Non-Goals

This addendum does not require:

- importing Paperclip file names exactly, except supporting equivalent `SOUL.md` style doctrine;
- putting full specialist SOUL files into every platform lead prompt;
- inventing new agent backends;
- replacing `WORKFLOW.md` as the global workflow contract;
- treating QA as a normal implementation child by default.
