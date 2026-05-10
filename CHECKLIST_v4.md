# Symphony-RS v4 Checklist — Role Instruction Packs and Decomposition Role Catalogs

One unchecked item per implementation iteration. Each item should land with tests and a commit. This checklist implements `SPEC_v4.md` only; do not duplicate v2 core workflow requirements or v3 dependency orchestration work.

## Phase 0 — v4 Grounding

- [x] Add `SPEC_v4.md` to product docs/references and link it from role/workflow docs without redefining v2 roles or v3 dependency behavior.
- [x] Add a sample `.symphony/roles/` directory to fixtures with `AGENTS.md` and `SOUL.md` for platform lead, QA, and at least two specialist roles.
- [x] Define the canonical prompt assembly order in docs: global workflow prompt, role instructions, role SOUL, agent profile system prompt, run context, graph/blockers, output schema.
- [x] Explicitly document that the platform-lead assignment catalog is generated from `WORKFLOW.md`, not from per-role `ASSIGNMENT.md` files.

## Phase 1 — Workflow Config Schema

- [x] Add typed `RoleInstructionConfig` under `RoleConfig.instructions` with optional file-backed `role_prompt` and `soul` paths.
- [x] Add structured role assignment metadata directly under `RoleConfig` or a nested role-local routing block: `owns`, `does_not_own`, `requires`, `handoff_expectations`, and role-local match hints such as paths/labels/domains.
- [x] Decide the exact schema shape for role-local routing metadata and make it distinct from global `routing.rules` while allowing the catalog builder to use both.
- [ ] Add strict validation that instruction file paths stay inside the repo/workflow root.
- [ ] Add strict validation that configured instruction files exist and are readable.
- [x] Add strict validation that role instruction config rejects unknown keys.
- [x] Add warning validation when a specialist role lacks structured assignment metadata and only has a terse description.
- [x] Add warning validation when decomposition is enabled but the platform-lead catalog would be built mostly from fallback descriptions/global routing rules.
- [x] Add round-trip tests for role instruction config and role assignment metadata.
- [ ] Add negative tests for path traversal, missing instruction files, unknown instruction keys, and missing eligible child-owning roles.

## Phase 2 — Instruction File Loading

- [ ] Add an instruction-pack loader that reads role `role_prompt` and `soul` files at workflow load time or dispatch preflight.
- [ ] Cache instruction file contents and invalidate on workflow reload.
- [ ] Preserve source provenance: role name, instruction kind, path, hash/mtime, and load timestamp.
- [ ] Redact secrets from instruction content before logging or event emission.
- [ ] Fail loud when a required configured instruction file cannot be loaded.
- [ ] Add tests for successful load, cache invalidation, missing file failure, path escape rejection, and log redaction.

## Phase 3 — Role Catalog Builder

- [ ] Implement a role catalog builder that reads `WorkflowConfig.roles`, `WorkflowConfig.agents`, global `routing.rules`, and role-local assignment metadata.
- [ ] Include in each catalog entry: role name, kind, description, agent profile/backend, concurrency, tools/toolsets, owns, does-not-own, requires, handoff expectations, and routing hints.
- [ ] Include `specialist` roles by default.
- [ ] Include `reviewer` roles only when review can be requested as child/follow-up work.
- [ ] Include `operator` roles only when runtime/deployment/data work can be child-scoped.
- [ ] Exclude `qa_gate` from normal implementation child assignment unless workflow explicitly allows manual QA task issues.
- [ ] Exclude other `integration_owner` roles from normal child assignment unless workflow explicitly allows nested/secondary integration ownership.
- [ ] Add deterministic rendering for the platform-lead catalog prompt section.
- [ ] Add tests proving terse-description fallback emits warnings.

## Phase 4 — Production Prompt Assembly

- [ ] Replace production legacy issue-only `run.rs::render_prompt` path with the richer `symphony-core::prompt::PromptContext` renderer or an equivalent strict v2/v4 renderer.
- [ ] Add explicit prompt section types: global workflow instructions, role prompt, role SOUL, agent system prompt, issue context, parent/child graph, blockers, workspace/branch, acceptance criteria, output schema.
- [ ] Ensure unknown prompt placeholders fail loud in v2/v4 production paths rather than being left silently unresolved.
- [ ] Ensure transient issue context is kept separate from durable role doctrine in prompt construction and run metadata.
- [ ] Add tests proving prompt assembly order is deterministic.
- [ ] Add tests proving agent profile `system_prompt` is included but does not replace role instructions or SOUL.
- [ ] Add tests proving prompt section provenance is included in run metadata.
- [ ] Extend `PromptIssue` with the SPEC v4 §5.1 required fields: `labels`, `priority`, `created_at`, `updated_at`, and a `blocked_by` summary (count + identifiers). Surface them in every prompt that includes an `IssueContext` section.
- [ ] Add tests proving `labels` reaches the platform-lead prompt so the agent can see what triggered decomposition (`DecompositionTriggers.labels_any`).
- [ ] Add tests proving `blocked_by` summary appears in `IssueContext` and is distinct from kernel-tracked `PromptBlocker[]` entries.
- [ ] Document the deferred-context contract: comments, assignee/reporter, attachments, and other tracker fields beyond §5.1 are NOT in the prompt and will be fetched on-demand via the `SPEC_v5.md` MCP read tools.

## Phase 5 — Role-Scoped Runtime Profile Resolution

- [ ] Replace the single global `build_agent_runner(cfg.agent.kind)` production path with role-aware runner resolution: run/work item role → `roles.<role>.agent` → `agents.<profile>` → concrete backend/composite runner.
- [ ] Extend `AgentBackendProfile` with structured `args: Vec<String>` and `extra_args: Vec<String>` fields; keep `command` as executable/shell entrypoint, not a raw concatenated command line.
- [ ] Preserve compatibility for existing `command: codex app-server` fixtures via migration/defaulting or fixture updates, but make the canonical v4 examples use `command: codex` + `args: [app-server]`.
- [ ] Honor `AgentBackendProfile.model` in every concrete backend adapter: Claude via model flag, Codex via app-server conversation/session config. (Hermes is deprecated — see `CHECKLIST_v5.md` Phase 0 for code removal.)
- [ ] Wire per-profile runtime overrides into composite/tandem profiles so lead and follower can use different models, args, tools, and budgets.
- [ ] Add validation for missing role agent, dangling agent profile, unsupported model override for backend, unsupported args ordering, and non-runnable composite profiles.
- [ ] Add tests proving two roles using the same backend can launch with different models and extra args.
- [ ] Add tests proving effective argv/debug metadata is emitted with secrets redacted.
- [ ] Add operator docs showing model/args/extra_args examples for Claude and Codex.

## Phase 6 — Platform Lead Decomposition and Integration Prompts

- [ ] Add a decomposition-specific prompt builder for platform-lead decomposition runs.
- [ ] Include global workflow prompt, platform-lead `role_prompt`, platform-lead `soul`, parent issue/repo context, decomposition policy, role catalog, dependency/blocker rules, and decomposition output schema. (The `decomposition output schema` section reduces to a brief prose pointer once the `propose_decomposition` MCP tool lands in `CHECKLIST_v5.md`; for v4 it remains an inlined schema so the live decomposition path has a working surface either way.)
- [ ] Include the generated role catalog for every eligible child-owning role.
- [ ] Exclude full specialist SOUL files from platform-lead prompts; include role assignment metadata and concise instruction-pack summaries only.
- [ ] Include global routing rules and role-local routing hints so the platform lead can assign child issues deterministically.
- [ ] Add tests proving decomposition prompts include catalog entries for backend/frontend/operator/reviewer roles when eligible.
- [ ] Add tests proving QA is not listed as a normal implementation child role unless workflow explicitly allows manual QA task issues.
- [ ] Add tests proving platform lead has enough prompt context to emit `assigned_role` for each child without guessing from terse descriptions.
- [ ] Add an integration-owner prompt builder (SPEC v4 §5 integration block) for the consolidation/PR phase, distinct from the decomposition phase.
- [ ] Surface latest `Handoff` per child (SPEC v4 §5.2) in the integration-owner prompt: `summary`, `changed_files`, `tests_run`, `verification_evidence`, `known_risks`, `branch_or_workspace`, `ready_for`. Read from `HandoffRepository::latest_handoff` via `IntegrationChild.latest_handoff`.
- [ ] Add tests proving the integration-owner prompt includes each child's latest handoff fields and not just bare `identifier`/`title`/`status`.
- [ ] Add tests proving the integration-owner prompt distinguishes children that produced no handoff (e.g. waived) from children with handoffs.

## Phase 7 — Specialist Prompts

- [ ] Add specialist prompt assembly that includes only the current specialist role's full `role_prompt` and `soul`.
- [ ] Include parent issue context, current child issue context, dependency/blocker context, workspace/branch claim, acceptance criteria, and handoff output schema.
- [ ] Ensure specialist prompts do not include other specialists' full SOUL files.
- [ ] Add tests proving backend specialist receives backend doctrine and not frontend doctrine.
- [ ] Add tests proving blocked specialist runs include visible blocker/dependency reason when dispatched for repair/rework.

## Phase 8 — QA Prompts

- [ ] Add QA prompt assembly that includes global workflow prompt, QA `role_prompt`, QA `soul`, integrated branch/draft PR context, acceptance trace, child handoffs, known blockers, CI/check status, and QA verdict schema.
- [ ] Ensure QA prompt construction treats QA as a gate over integrated output, not a normal implementation child by default.
- [ ] Add tests proving QA verdict schema and evidence requirements are present.
- [ ] Add tests proving QA can see child handoffs and integration summary.
- [ ] Read child handoffs from `HandoffRepository::latest_handoff` (same source as Phase 6 integration-owner prompt) and include the SPEC v4 §5.2 fields. Both Phase 6 and Phase 8 must agree on the rendering format so a child handoff reads identically downstream.

## Phase 9 — Fixtures and Examples

- [ ] Update `tests/fixtures/sample-workflow/WORKFLOW.md` with rich role assignment metadata for platform lead, QA, backend specialist, frontend/TUI specialist, reviewer, and operator examples.
- [ ] Add `.symphony/roles/platform_lead/AGENTS.md` and `SOUL.md` fixture files.
- [ ] Add `.symphony/roles/qa/AGENTS.md` and `SOUL.md` fixture files.
- [ ] Add at least two specialist fixture instruction packs with distinct ownership boundaries.
- [ ] Add fixture tests proving `WorkflowLoader` validates and resolves those files.
- [ ] Add fixture tests proving generated platform-lead catalog text changes when `WORKFLOW.md` role metadata changes.

## Phase 10 — Operator Surface and Debugging

- [ ] Add a command or debug mode to render the effective prompt for a role/run without launching an agent.
- [ ] Add a command or debug mode to render only the platform-lead role catalog.
- [ ] Show role instruction provenance in status/run metadata.
- [ ] Add JSON output for role catalog inspection.
- [ ] Add tests for prompt preview redaction and provenance output.

## Phase 11 — End-to-End Scenarios

- [ ] Add deterministic fake E2E: platform lead receives generated role catalog and decomposes a parent into correctly assigned backend/frontend children.
- [ ] Add deterministic fake E2E: role catalog excludes QA as a child implementer and QA runs only after integration.
- [ ] Add deterministic fake E2E: specialist receives its own instruction pack and emits a structured handoff satisfying role-specific expectations.
- [ ] Add deterministic fake E2E: missing role instruction file fails before agent launch.

## Phase 12 — Documentation

- [ ] Document role instruction packs in `docs/roles.md`.
- [ ] Document generated platform-lead assignment catalog in `docs/workflow.md`.
- [ ] Document recommended `.symphony/roles/<role>/AGENTS.md` and `SOUL.md` conventions.
- [ ] Document why `ASSIGNMENT.md` is not required: assignment guidance is structured in `WORKFLOW.md` and rendered automatically.
- [ ] Document prompt preview/debug commands.

## Phase 13 — Forward Reference

- [ ] Cross-link `CHECKLIST_v5.md` from the Phase 0 grounding and from the integration / QA prompt phases. v4 closes the prompt-side gaps; v5 closes the agent ↔ kernel communication gaps. The product is not end-to-end runnable on a live agent until both are complete.
